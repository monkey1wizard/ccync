//! Install/update/uninstall operations for CCYNC.
//!
//! Orchestrates the full flow: config → mode → render → runtime-surface projections.
//! This is the Rust-native implementation replacing the frozen PowerShell oracle.
//!
//! Entry switch (BUG-B): once this module is wired in main.rs, the Rust binary
//! is the single entry for install/update/uninstall. The frozen scripts remain
//! as oracle only and are NOT invoked by the entry.
//!
//! Oracle parity: run_update_with_evidence() produces Claude+Copilot surfaces
//! that match the frozen Install-CcyncPlugins oracle in an isolated home.

use crate::config::{CcyncConfig, LegacyState};
use ccync_foundation::provider::{
    CommittedOwnership, IntentPhase, OwnershipFingerprint, PendingIntent, ProviderLifecycleJournal,
    ProviderLifecycleLock,
};
use ccync_foundation::target::ProjectionTargetId;
use mcp::run_mcp_remove_managed;
use projection::agy::AgyProjection;
use projection::claude_skill::ClaudeSkillProjection;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

const COMPONENT_DIRS: &[&str] = &["skills", "commands", "agents", "hooks"];

pub(crate) fn provider_state_base() -> Option<PathBuf> {
    ccync_foundation::paths::EffectiveLayout::load_or_legacy()
        .ok()
        .map(|layout| layout.state_base())
}

fn effective_layout() -> Result<ccync_foundation::paths::EffectiveLayout, InstallError> {
    ccync_foundation::paths::EffectiveLayout::load_or_legacy()
        .map_err(|error| InstallError::Io(std::io::Error::other(error.to_string())))
}

fn provider_journal_path() -> Option<PathBuf> {
    provider_state_base().map(|root| root.join("projection.txn.json"))
}

fn provider_lock_path() -> Option<PathBuf> {
    provider_state_base().map(|root| root.join("lock.json"))
}

/// Upper bounds for one recorded intent. An oversized intent is refused before
/// the journal is read or written, so recovery never parses unbounded state.
const MAX_INTENT_FINGERPRINTS: usize = 64;
const MAX_INTENT_SCOPED_PATHS: usize = 256;

fn invalid_lifecycle_state(message: impl Into<String>) -> InstallError {
    InstallError::Io(std::io::Error::other(message.into()))
}

/// Refuse an intent that recovery could not interpret, before any journal
/// write or native call. Every preimage the intent names must already be saved.
fn validate_intent(intent: &PendingIntent) -> Result<(), InstallError> {
    if intent.ownership_fingerprints.is_empty() {
        return Err(invalid_lifecycle_state("native intent names no selector"));
    }
    if intent.ownership_fingerprints.len() > MAX_INTENT_FINGERPRINTS
        || intent.scoped_paths.len() > MAX_INTENT_SCOPED_PATHS
    {
        return Err(invalid_lifecycle_state(
            "native intent exceeds its recorded bounds",
        ));
    }
    let build = provider_state_base().ok_or(InstallError::NoHome)?;
    for preimage in &intent.preimage_refs {
        // A saved preimage is a file, or a tree when a replaced skill directory was saved.
        if !build.join(preimage.saved.as_str()).exists() {
            return Err(invalid_lifecycle_state(format!(
                "preimage {} is not saved before its intent",
                preimage.saved.as_str()
            )));
        }
    }
    Ok(())
}

fn write_lifecycle_journal(
    path: &Path,
    journal: &ProviderLifecycleJournal,
) -> Result<(), InstallError> {
    let bytes = journal
        .to_json_string()
        .map_err(|e| invalid_lifecycle_state(e.to_string()))?;
    ccync_foundation::platform::atomic_write_bytes(path, bytes.as_bytes())
        .map_err(|e| InstallError::Io(std::io::Error::other(e.to_string())))
}

/// Persist `intent` before `mutate` runs. When the write fails, `mutate` never
/// runs, so no native call can happen without a recorded intent.
fn persist_then_mutate<T>(
    intent: &PendingIntent,
    mutate: impl FnOnce() -> Result<T, InstallError>,
) -> Result<T, InstallError> {
    record_native_intent(intent.clone())?;
    mutate()
}

/// Atomically persist a native operation intent before changing a runtime.
/// The caller must hold the lifecycle guard for the entire operation.
pub fn record_native_intent(intent: PendingIntent) -> Result<(), InstallError> {
    validate_intent(&intent)?;
    let path = provider_journal_path().ok_or(InstallError::NoHome)?;
    let mut journal = match fs::read_to_string(&path) {
        Ok(text) => ProviderLifecycleJournal::from_json_str(&text)
            .map_err(|e| InstallError::Io(std::io::Error::other(e.to_string())))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ProviderLifecycleJournal::default()
        }
        Err(error) => return Err(InstallError::Io(error)),
    };
    journal.intents.retain(|existing| {
        existing.target != intent.target
            || existing.ownership_fingerprints != intent.ownership_fingerprints
    });
    journal.intents.push(intent);
    write_lifecycle_journal(&path, &journal)
}

/// Atomically update native ownership after a fresh observation verifies the
/// installed selector, generation, and required capabilities.
pub fn commit_native_ownership(record: CommittedOwnership) -> Result<(), InstallError> {
    record
        .validate()
        .map_err(|e| invalid_lifecycle_state(e.to_string()))?;
    // A row without the full proof grants no ownership, so writing one only
    // hides a registration that recovery would then have to reconcile.
    if record.client_scope.is_none()
        || record.source_digest.is_none()
        || record.view_digest.is_none()
    {
        return Err(invalid_lifecycle_state(
            "committed ownership needs client scope, source digest, and view digest",
        ));
    }
    let path = provider_state_base()
        .map(|root| root.join("lock.json"))
        .ok_or(InstallError::NoHome)?;
    let mut lock = match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<Value>(&text)
            .map_err(|e| InstallError::Io(std::io::Error::other(e.to_string())))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(InstallError::Io(error)),
    };
    let mut section = ProviderLifecycleLock::from_lock(&lock)
        .map_err(|e| InstallError::Io(std::io::Error::other(e.to_string())))?;
    section.committed.retain(|existing| {
        existing.target != record.target || existing.selector != record.selector
    });
    section.committed.push(record);
    section
        .write_to_lock(&mut lock)
        .map_err(|e| InstallError::Io(std::io::Error::other(e.to_string())))?;
    write_json_atomic(&path, &lock).map_err(|e| InstallError::Io(std::io::Error::other(e)))
}

/// Drop one committed ownership row after its registration has been removed
/// with fresh proof. Other rows, including those for other selectors, stay.
pub fn drop_committed_ownership(record: &CommittedOwnership) -> Result<(), InstallError> {
    let path = provider_state_base()
        .map(|root| root.join("lock.json"))
        .ok_or(InstallError::NoHome)?;
    let mut lock = match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<Value>(&text)
            .map_err(|e| InstallError::Io(std::io::Error::other(e.to_string())))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(InstallError::Io(error)),
    };
    let mut section = ProviderLifecycleLock::from_lock(&lock)
        .map_err(|e| InstallError::Io(std::io::Error::other(e.to_string())))?;
    let before = section.committed.len();
    section.committed.retain(|existing| existing != record);
    if section.committed.len() == before {
        return Ok(());
    }
    section
        .write_to_lock(&mut lock)
        .map_err(|e| InstallError::Io(std::io::Error::other(e.to_string())))?;
    write_json_atomic(&path, &lock).map_err(|e| InstallError::Io(std::io::Error::other(e)))
}

/// Clear an intent only after native state and ownership have been verified.
pub fn clear_native_intent(intent: &PendingIntent) -> Result<(), InstallError> {
    let Some(path) = provider_journal_path() else {
        return Err(InstallError::NoHome);
    };
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(InstallError::Io(error)),
    };
    let mut journal = ProviderLifecycleJournal::from_json_str(&text)
        .map_err(|e| InstallError::Io(std::io::Error::other(e.to_string())))?;
    journal.intents.retain(|existing| existing != intent);
    write_lifecycle_journal(&path, &journal)
}

/// The newest journaled intent that names `selector` for `target`, if any.
/// Callers read it before recording a replacement intent, so the earlier
/// record can still prove a registration that was installed by an interrupted
/// operation.
pub fn pending_native_intent(
    target: ProjectionTargetId,
    selector: &ccync_foundation::provider::NativeSelector,
) -> Result<Option<PendingIntent>, InstallError> {
    let path = provider_journal_path().ok_or(InstallError::NoHome)?;
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(InstallError::Io(error)),
    };
    let journal = ProviderLifecycleJournal::from_json_str(&text)
        .map_err(|e| InstallError::Io(std::io::Error::other(e.to_string())))?;
    Ok(journal.intents.into_iter().rev().find(|intent| {
        intent.target == target
            && intent
                .ownership_fingerprints
                .iter()
                .any(|fingerprint| fingerprint.selector == *selector)
    }))
}

/// Read the committed native ownership records. A missing lockfile holds none.
fn read_committed_records() -> Result<Vec<CommittedOwnership>, String> {
    let path = provider_state_base()
        .map(|root| root.join("lock.json"))
        .ok_or_else(|| "home directory is unknown".to_string())?;
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    let lockfile = serde_json::from_str::<Value>(&text).map_err(|e| e.to_string())?;
    ProviderLifecycleLock::from_lock(&lockfile)
        .map(|section| section.committed)
        .map_err(|e| e.to_string())
}

/// Whether fresh observation still shows `record` as installed: the selector
/// is present and loadable, and its view directory hashes to the committed
/// view digest. Any failure, including an unavailable CLI, returns `false`.
fn committed_record_is_live(
    runner: &dyn crate::plugin_native::NativeCommandRunner,
    executable: crate::plugin_native::NativeExecutable,
    record: &CommittedOwnership,
) -> bool {
    let Ok(observed) = runner.observe(executable, &record.selector) else {
        return false;
    };
    if !observed.available
        || !observed.present
        || observed.broken
        || observed.selector.as_ref() != Some(&record.selector)
    {
        return false;
    }
    let Some(view_path) = crate::plugin_native::committed_view_path(record) else {
        return false;
    };
    matches!(
        (
            crate::plugin_native::observe_view_digest(&view_path),
            record.view_digest.as_ref(),
        ),
        (
            crate::plugin_native::NativePathEvidence::Digest(observed_digest),
            Some(committed_digest),
        ) if observed_digest == *committed_digest
    )
}

/// Reconcile recorded native intents with committed ownership and fresh
/// observation. Runs before sync stages surfaces and before uninstall acts.
///
/// An intent is cleared when a live committed record proves its exact desired
/// generation and view. An older intent is superseded when a live committed
/// record proves a later intent for the same target and selector. Every other
/// intent is kept, with its preimages, because attribution is ambiguous. The
/// journal is written first. Preimages are removed only after the journal no
/// longer names them.
fn reconcile_native_intents(
    _lock: &crate::tool_manager::lock::HeldLifecycleLock,
    runner: &dyn crate::plugin_native::NativeCommandRunner,
    warnings: &mut Vec<String>,
) {
    let Some(journal_path) = provider_journal_path() else {
        return;
    };
    let text = match fs::read_to_string(&journal_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            warnings.push(format!("native intent journal unreadable: {error}"));
            return;
        }
    };
    let mut journal = match ProviderLifecycleJournal::from_json_str(&text) {
        Ok(journal) if !journal.intents.is_empty() => journal,
        Ok(_) => return,
        Err(error) => {
            warnings.push(format!("native intent journal unreadable: {error}"));
            return;
        }
    };
    let committed = match read_committed_records() {
        Ok(committed) => committed,
        Err(error) => {
            warnings.push(format!(
                "native ownership unreadable; pending intents kept: {error}"
            ));
            return;
        }
    };

    let selector_key = |intent: &PendingIntent| -> Option<String> {
        match intent.ownership_fingerprints.as_slice() {
            [fingerprint] => Some(fingerprint.selector.as_str().to_string()),
            _ => None,
        }
    };
    let mut dropped = vec![false; journal.intents.len()];
    let mut live: BTreeMap<(String, String), CommittedOwnership> = BTreeMap::new();
    for record in &committed {
        let Some(executable) = crate::plugin_native::NativeExecutable::for_target(record.target)
        else {
            continue;
        };
        if committed_record_is_live(runner, executable, record) {
            live.insert(
                (
                    record.target.as_str().to_string(),
                    record.selector.as_str().to_string(),
                ),
                record.clone(),
            );
        }
    }
    for (index, intent) in journal.intents.iter().enumerate() {
        let Some(selector) = selector_key(intent) else {
            continue;
        };
        let key = (intent.target.as_str().to_string(), selector);
        let Some(record) = live.get(&key) else {
            continue;
        };
        if record.generation == intent.desired_generation
            && record.view_digest == intent.view_digest
        {
            dropped[index] = true;
            continue;
        }
        let later_committed = journal.intents[index + 1..].iter().any(|later| {
            selector_key(later).as_deref() == Some(key.1.as_str())
                && later.target == intent.target
                && later.desired_generation == record.generation
        });
        if later_committed {
            dropped[index] = true;
        }
    }
    if !dropped.contains(&true) {
        return;
    }

    let retired: Vec<PendingIntent> = journal
        .intents
        .iter()
        .zip(&dropped)
        .filter(|(_, drop)| **drop)
        .map(|(intent, _)| intent.clone())
        .collect();
    journal.intents = journal
        .intents
        .into_iter()
        .zip(&dropped)
        .filter(|(_, drop)| !**drop)
        .map(|(intent, _)| intent)
        .collect();
    if let Err(error) = write_lifecycle_journal(&journal_path, &journal) {
        warnings.push(format!(
            "native intent reconciliation kept intents: {error}"
        ));
        return;
    }

    let Some(build) = provider_state_base() else {
        return;
    };
    let still_named: BTreeSet<String> = journal
        .intents
        .iter()
        .flat_map(|intent| intent.preimage_refs.iter())
        .map(|preimage| preimage.saved.as_str().to_string())
        .collect();
    for preimage in retired
        .iter()
        .flat_map(|intent| intent.preimage_refs.iter())
    {
        if still_named.contains(preimage.saved.as_str()) {
            continue;
        }
        if let Err(error) = remove_path_if_present(&build.join(preimage.saved.as_str())) {
            warnings.push(format!(
                "reconciled preimage {} kept: {error}",
                preimage.saved.as_str()
            ));
        }
    }
}

/// Reconcile native intents while a lifecycle guard is already held.
/// Migration must not proceed while an intent remains unresolved.
pub(crate) fn recover_native_intents(
    guard: &crate::tool_manager::lock::LifecycleGuard,
) -> Result<(), String> {
    let proof = guard.held_lock_proof();
    let mut warnings = Vec::new();
    reconcile_native_intents(
        &proof,
        &crate::plugin_native::ProcessNativeRunner,
        &mut warnings,
    );
    if !warnings.is_empty() {
        return Err(warnings.join("; "));
    }
    let journal_path = provider_journal_path().ok_or("provider state layout is unavailable")?;
    let journal = match fs::read_to_string(&journal_path) {
        Ok(raw) => ProviderLifecycleJournal::from_json_str(&raw)
            .map_err(|error| format!("native intent journal unreadable: {error}"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("native intent journal unreadable: {error}")),
    };
    if !journal.intents.is_empty() {
        return Err("native intent journal still has pending intents".into());
    }
    Ok(())
}

/// Remove committed native providers during uninstall. Exact ownership is
/// re-observed before mutation, and failed work keeps its retry evidence.
fn remove_committed_providers_for_uninstall(
    _lock: &crate::tool_manager::lock::HeldLifecycleLock,
    warnings: &mut Vec<String>,
    runner: &dyn crate::plugin_native::NativeCommandRunner,
) -> BTreeSet<String> {
    let mut retained = BTreeSet::new();
    let Some(path) = provider_state_base().map(|root| root.join("lock.json")) else {
        return retained;
    };
    let Ok(text) = fs::read_to_string(&path) else {
        return retained;
    };
    let Ok(lockfile) = serde_json::from_str::<Value>(&text) else {
        return retained;
    };
    let Ok(section) = ProviderLifecycleLock::from_lock(&lockfile) else {
        return retained;
    };
    if section.committed.is_empty() {
        return retained;
    }
    for ownership in section.committed {
        let Some(executable) = crate::plugin_native::NativeExecutable::for_target(ownership.target)
        else {
            retained.insert(ownership.selector.as_str().to_string());
            continue;
        };
        // Each adapter gates its own removals, so an unverified adapter keeps
        // its records without writing an intent or calling its CLI.
        if !runner.mutation_verified(executable) {
            retained.insert(ownership.selector.as_str().to_string());
            warnings.push(format!(
                "native provider ownership evidence is unverified ({}); committed registrations retained without CLI calls",
                runner.mutation_block_reason(executable)
            ));
            continue;
        }
        let intent = PendingIntent {
            target: ownership.target,
            ownership_fingerprints: vec![OwnershipFingerprint {
                selector: ownership.selector.clone(),
                package_digest: ownership.generation.package_digest.clone(),
            }],
            scoped_paths: Vec::new(),
            preimage_refs: Vec::new(),
            previous_generation: Some(ownership.generation.clone()),
            desired_generation: ownership.generation.clone(),
            phase: IntentPhase::Pruning,
            client_scope: ownership.client_scope.clone(),
            source_digest: ownership.source_digest.clone(),
            view_digest: ownership.view_digest.clone(),
            observed_preimage: ownership.observed_preimage.clone(),
            enablement_intent: ownership.enablement_intent,
            group_substep: ownership.group_substep.clone(),
        };
        let result = persist_then_mutate(&intent, || {
            crate::plugin_native::remove_committed_registration(runner, executable, &ownership)
                .map_err(|error| InstallError::Io(std::io::Error::other(error.to_string())))
        });
        match result {
            Ok(()) => {
                let Ok(mut current) = fs::read_to_string(&path).and_then(|raw| {
                    serde_json::from_str::<Value>(&raw).map_err(std::io::Error::other)
                }) else {
                    retained.insert(ownership.selector.as_str().to_string());
                    continue;
                };
                let Ok(mut lifecycle) = ProviderLifecycleLock::from_lock(&current) else {
                    retained.insert(ownership.selector.as_str().to_string());
                    continue;
                };
                lifecycle.committed.retain(|entry| {
                    entry.target != ownership.target || entry.selector != ownership.selector
                });
                if lifecycle.write_to_lock(&mut current).is_err()
                    || write_json_atomic(&path, &current).is_err()
                {
                    retained.insert(ownership.selector.as_str().to_string());
                    continue;
                }
                if clear_native_intent(&intent).is_err() {
                    retained.insert(ownership.selector.as_str().to_string());
                }
            }
            Err(error) => {
                retained.insert(ownership.selector.as_str().to_string());
                warnings.push(format!(
                    "native provider {} removal retained for retry: {error}",
                    ownership.selector
                ));
            }
        }
    }
    retained
}

/// Describes the payload shape found in a cached source directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayloadShape {
    /// A conventional CC plugin root with at least one component directory.
    PluginRoot,
    /// A repository whose root is one Agent Skill.
    BareSkill(String),
    /// No supported payload shape was found.
    Unrecognized,
}

/// Classify the contents of a cached source directory.
pub fn payload_shape(cache_dir: &Path) -> PayloadShape {
    if COMPONENT_DIRS
        .iter()
        .any(|component| cache_dir.join(component).exists())
    {
        return PayloadShape::PluginRoot;
    }

    let skill_path = cache_dir.join("SKILL.md");
    let contents = match fs::read_to_string(skill_path) {
        Ok(contents) => contents,
        Err(_) => return PayloadShape::Unrecognized,
    };
    let mut lines = contents.lines();
    if lines.next().map(str::trim) != Some("---") {
        return PayloadShape::Unrecognized;
    }

    let mut skill_name = None;
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            return skill_name
                .map(PayloadShape::BareSkill)
                .unwrap_or(PayloadShape::Unrecognized);
        }
        if skill_name.is_some() {
            continue;
        }
        let Some(raw_name) = trimmed.strip_prefix("name:") else {
            continue;
        };
        let name = raw_name.trim();
        let name = if name.starts_with('"') || name.starts_with('\'') {
            let quote = name.as_bytes()[0] as char;
            if name.ends_with(quote) && name.len() >= 2 {
                &name[1..name.len() - 1]
            } else {
                return PayloadShape::Unrecognized;
            }
        } else if name.ends_with('"') || name.ends_with('\'') {
            return PayloadShape::Unrecognized;
        } else {
            name
        };
        if is_safe_skill_name(name) {
            skill_name = Some(name.to_owned());
        } else {
            return PayloadShape::Unrecognized;
        }
    }

    PayloadShape::Unrecognized
}

/// True when `name` is safe to use as the single directory name a bare skill is
/// materialized under.
///
/// Fail-closed and delegated: containment is decided by `is_contained_relative`,
/// the same primitive the archive extractor uses, so a value carrying a root or
/// a platform path prefix is rejected here for the same reason it is rejected
/// there. A denylist of separator characters is not enough — on Windows a
/// drive-prefixed value such as `C:evil` carries no separator yet still replaces
/// the whole buffer when joined onto the canonical root, which would write the
/// payload outside the tree ccync owns.
///
/// On top of containment the value must be exactly one `Normal` component whose
/// text round-trips unchanged, so `.`, `a/b`, and trailing-separator forms are
/// all rejected even though each of them is "contained".
fn is_safe_skill_name(name: &str) -> bool {
    if name.is_empty() || name.contains('\\') {
        return false;
    }
    let path = Path::new(name);
    if !is_contained_relative(path) {
        return false;
    }
    // An all-dot name (`...` and longer) survives as a `Normal` component even
    // though `.` and `..` do not, and it is not a name any skill should claim —
    // rejecting the whole family keeps the rule stated in the plan intact.
    if name.chars().all(|character| character == '.') {
        return false;
    }
    let mut components = path.components();
    let Some(std::path::Component::Normal(only)) = components.next() else {
        return false;
    };
    components.next().is_none() && only.to_str() == Some(name)
}

/// Count the components represented by a cached source directory.
pub fn component_count(cache_dir: &Path) -> usize {
    match payload_shape(cache_dir) {
        PayloadShape::PluginRoot => COMPONENT_DIRS
            .iter()
            .map(|component| {
                fs::read_dir(cache_dir.join(component))
                    .map(|entries| entries.count())
                    .unwrap_or(0)
            })
            .sum(),
        PayloadShape::BareSkill(_) => 1,
        PayloadShape::Unrecognized => 0,
    }
}

/// Whether a cached source directory contributes anything a render would
/// pick up. Corrupt or unreadable MCP manifests are returned as errors.
///
/// `render_canonical_root` reads exactly two things out of each plugin
/// directory: the component payload (`payload_shape`/`component_count`) and
/// an author `.mcp.json`'s server map. Those two channels are the complete
/// set this predicate checks; a future third channel added to the render
/// needs a matching addition here.
///
/// A nonzero `component_count` contributes. Otherwise an MCP manifest with a
/// non-empty server map contributes, while an absent or empty manifest does
/// not.
pub fn inspect_render_contribution(
    plugin_id: &str,
    cache_dir: &Path,
) -> Result<bool, InstallError> {
    let mcp = read_plugin_mcp_servers(plugin_id, cache_dir)?;
    if component_count(cache_dir) > 0 {
        return Ok(true);
    }
    Ok(mcp.is_some_and(|read| !read.servers.is_empty()))
}

/// Doctor compatibility wrapper. On inspection failure, contribution is not
/// proven absent, so preserve the conservative `true` result.
pub fn contributes_to_render(plugin_id: &str, cache_dir: &Path) -> bool {
    inspect_render_contribution(plugin_id, cache_dir).unwrap_or(true)
}

/// Errors that can occur during install/update/uninstall.
#[derive(Debug)]
pub enum InstallError {
    /// Filesystem I/O error.
    Io(std::io::Error),
    /// JSON parsing or serialization failed.
    Json(serde_json::Error),
    /// Home directory could not be determined.
    NoHome,
    /// Uninstall completed partially; some removals failed.
    PartialUninstall { warnings: Vec<String> },
    /// The selected targets in `config.json` disagree with the snapshot that
    /// the last `ccync init` recorded. Carries the full refusal text.
    SelectionChangedOutsideInit(String),
    /// An enabled plugin has no source or cache directory on disk. Refused
    /// before any mutation, because a missing directory must never be read
    /// as removal intent.
    ManagedSourceMissing(String),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallError::Io(e) => write!(f, "I/O error: {e}"),
            InstallError::Json(e) => write!(f, "JSON error: {e}"),
            InstallError::NoHome => write!(f, "could not determine home directory"),
            InstallError::SelectionChangedOutsideInit(message) => write!(f, "{message}"),
            InstallError::ManagedSourceMissing(message) => write!(f, "{message}"),
            InstallError::PartialUninstall { warnings } => {
                write!(
                    f,
                    "uninstall completed with partial failures: {}",
                    warnings.join("; ")
                )
            }
        }
    }
}

impl std::error::Error for InstallError {}

impl From<std::io::Error> for InstallError {
    fn from(e: std::io::Error) -> Self {
        InstallError::Io(e)
    }
}

impl From<serde_json::Error> for InstallError {
    fn from(e: serde_json::Error) -> Self {
        InstallError::Json(e)
    }
}

/// Which of the two projection phases a [`ProjectionTargetResult`] belongs to.
///
/// The engine composes exactly two projection phases per run: the lifecycle
/// apply against the canonical root (Claude/Copilot/AGY selection-gated
/// projections above), and the post-MCP machine projection (the
/// `run_machine_update` pass in the projection crate). There is no third
/// phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProjectionPhase {
    /// The canonical-root lifecycle apply phase.
    Canonical,
    /// The post-MCP machine projection phase.
    Machine,
}

/// The engine-derived outcome for a single projection target's phase.
///
/// This is the sole status vocabulary the engine assigns — projection-crate
/// evidence never carries a status of its own, only path/warning/failure
/// facts. `Skipped`/`Failed` carry a `reason` so a rendered report can explain
/// *why* without a second lookup; `Failed` additionally carries the failing
/// path when the failure was path-scoped (mirrors
/// `projection::ProjectionExpectedFailure`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionTargetStatus {
    /// The target's phase has not executed yet.
    NotRun,
    /// The target's phase executed and produced at least one path change.
    Applied,
    /// The target's phase executed but produced no path changes.
    Unchanged,
    /// The target's phase was deliberately not executed.
    Skipped { reason: String },
    /// The target's phase executed and hit an expected, non-panicking failure.
    Failed {
        path: Option<PathBuf>,
        reason: String,
    },
}

/// The engine's own per-target result: target identity, the engine-derived
/// status, and the underlying typed evidence a rendered report needs.
///
/// `paths`/`warnings` reuse `projection::ProjectionPathResult` / `String`
/// directly rather than duplicating those shapes — this crate already
/// depends on `projection` (see `AgyProjection`/`ClaudeSkillProjection`
/// above), and result types are required to carry typed per-path changes and
/// target warnings, which the projection crate's own evidence types already
/// express exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionTargetResult {
    /// Which target this result belongs to (target identity).
    pub target: ProjectionTargetId,
    /// The engine-derived status for this target's phase (explicit status).
    pub status: ProjectionTargetStatus,
    /// The paths this target's phase touched, and how (typed per-path changes).
    pub paths: Vec<projection::ProjectionPathResult>,
    /// Non-fatal warnings scoped to this target (target warnings).
    pub warnings: Vec<String>,
}

/// The engine-owned, run-level projection report: one run's worth of common
/// evidence, per-target results, and the install mode, plus a single
/// run-level warning carrier.
///
/// `targets` is a `BTreeMap` keyed by [`ProjectionTargetId`], mirroring
/// `projection::ProjectionEvidence::by_target` — the same container choice
/// keeps target lookup and iteration order consistent between the
/// projection-crate evidence this report is built from and the report
/// itself, and a run never produces two results for the same target so the
/// map's key-uniqueness is not a lossy constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionRunReport {
    /// Evidence not owned by any single target (common evidence).
    pub common: Vec<projection::ProjectionPathResult>,
    /// Per-target results, keyed by target identity (target results).
    pub targets: std::collections::BTreeMap<ProjectionTargetId, ProjectionTargetResult>,
    /// Mode in use. Always the literal `"normal"` (D-07: single mode).
    pub mode: String,
    /// Run-level warnings not scoped to any single target (one run-level
    /// warning carrier).
    pub run_warnings: Vec<String>,
}

/// Per-target bookkeeping [`ProjectionRunAccumulator`] merges across calls to
/// `record_canonical`/`record_machine`, prior to status derivation.
///
/// `skipped`/`failure` are last-write-wins across phases rather than
/// first-write-wins: in practice at most one phase ever sets either field for
/// a given target (a target that already failed or was skipped in one phase
/// is not expected to also produce a conflicting skip/failure in the other),
/// so the direction of the overwrite does not change any currently reachable
/// outcome — it only decides an unreached tie.
#[derive(Debug, Clone, Default)]
struct ProjectionTargetAccumState {
    paths: Vec<projection::ProjectionPathResult>,
    warnings: Vec<String>,
    skipped: Option<String>,
    failure: Option<projection::ProjectionExpectedFailure>,
}

/// The engine-owned accumulator that merges [`projection::ProjectionEvidence`]
/// from both projection phases and is the sole deriver of
/// [`ProjectionTargetStatus`] for every selected target.
///
/// # Precedence order (applied per target, in this exact order)
///
/// 1. **Failure wins.** If either phase recorded an expected owned-path
///    failure for a target, the target is `Failed { path, reason }` — even if
///    the target also completed both phases. This includes AGY junction
///    failures, which arrive as ordinary target failures like any other.
/// 2. **Skip wins next.** If no failure was recorded and either phase
///    recorded a skip reason, the target is `Skipped { reason }` — even if
///    the target also completed both phases.
/// 3. **Completion gates Applied/Unchanged — the required phase(s) depend on
///    which phase(s) structurally own the target.** A target only becomes
///    eligible for `Applied`/`Unchanged` once every phase that can
///    structurally complete it has done so (see
///    [`Self::required_phases_completed`] for the per-target classification):
///    - `Claude`/`Codex`/`Copilot` are covered by both phases and must
///      appear in *both* phases' completed-targets sets.
///    - `AgyCli`/`AgyIde` are completed only by the `Canonical` phase (the
///      AGY junction apply inside `run_update_with_evidence`);
///      `Canonical`-phase completion alone is sufficient for these two.
///    - `Opencode`/`AgyGui` are completed only by the `Machine`
///      phase — the `Canonical` phase's completed set only ever contains
///      `Claude`/`Copilot`/`Codex`/`AgyCli`/`AgyIde` (see
///      `run_update_with_evidence`'s own doc comment), so requiring the
///      `Canonical` conjunct for these two would leave them permanently
///      `NotRun` even on a fully clean run; `Machine`-phase completion alone
///      is sufficient for exactly these two. Note the machine projection
///      pass DOES attribute AGY GUI evidence to a specific target id: its
///      agy-gui skill links, prune, and mcp-config writes record
///      `ProjectionTargetId::AgyGui` as owner rather than folding into
///      `common`.
///
///    `completed_targets` is tracked as two independent sets — one per phase
///    — never merged into one: a dual-phase target that completed only one
///    phase must stay `NotRun` regardless of what the other phase later
///    reports. Missing a required phase's completion keeps the target
///    `NotRun`; absence is never treated as completion.
/// 4. **Net per-path effect decides Applied vs. Unchanged.** Once a target
///    clears step 3, its recorded `paths` are grouped by `path`; a target is
///    `Applied` when at least one *distinct* path's own change history ends
///    in real, surviving evidence, and `Unchanged` when none does. A path's
///    own trailing `Removed` counts as real evidence UNLESS it is directly
///    preceded by a `Written` for that same path — that exact
///    `[Written, Removed]` shape is what a rolled-back AGY junction produces
///    (see `apply_agy_evidence`'s rollback-compensation fix) and represents a
///    write that was undone within the same run, i.e. no lasting effect. A
///    standalone `Removed` with no preceding `Written` for that path (a
///    genuine deletion of something that already existed) still counts, as
///    does any path whose last recorded change is `Written` or `Linked`.
///    Raw non-emptiness of `paths` is deliberately NOT the test — that would
///    count a rolled-back `[Written, Removed]` pair as change evidence too.
///
/// Every target the accumulator was constructed with (the run's selected
/// targets) is guaranteed a result in the final report even if it produced
/// zero evidence in either phase — such a target derives `NotRun` from step 3
/// finding it in neither required phase's completed set, rather than being
/// silently absent from the report.
#[derive(Debug, Clone)]
pub struct ProjectionRunAccumulator {
    selected: std::collections::BTreeSet<ProjectionTargetId>,
    canonical_completed: std::collections::BTreeSet<ProjectionTargetId>,
    machine_completed: std::collections::BTreeSet<ProjectionTargetId>,
    targets: std::collections::BTreeMap<ProjectionTargetId, ProjectionTargetAccumState>,
    common: Vec<projection::ProjectionPathResult>,
    run_warnings: Vec<String>,
    mode: String,
}

impl ProjectionRunAccumulator {
    /// Start a new accumulator for a run, seeded with the run's selected
    /// targets (so a target with zero evidence still derives `NotRun` in
    /// `finish()` rather than being absent from the report) and the run's
    /// mode (carried through to `ProjectionRunReport::mode` unchanged).
    pub fn new(
        mode: impl Into<String>,
        selected: impl IntoIterator<Item = ProjectionTargetId>,
    ) -> Self {
        Self {
            selected: selected.into_iter().collect(),
            canonical_completed: std::collections::BTreeSet::new(),
            machine_completed: std::collections::BTreeSet::new(),
            targets: std::collections::BTreeMap::new(),
            common: Vec::new(),
            run_warnings: Vec::new(),
            mode: mode.into(),
        }
    }

    /// Merge in evidence produced by the given phase. Common evidence and
    /// run-level warnings are appended (never overwritten); the phase's
    /// `completed_targets` are folded into that phase's own completed set
    /// only (never the other phase's); per-target paths/warnings are
    /// appended, and a per-target `skipped`/`failure` value is recorded when
    /// present (see [`ProjectionTargetAccumState`] for the merge rule).
    pub fn record(&mut self, phase: ProjectionPhase, evidence: &projection::ProjectionEvidence) {
        self.common.extend(evidence.common.iter().cloned());
        self.run_warnings
            .extend(evidence.run_warnings.iter().cloned());

        let completed = match phase {
            ProjectionPhase::Canonical => &mut self.canonical_completed,
            ProjectionPhase::Machine => &mut self.machine_completed,
        };
        completed.extend(evidence.completed_targets.iter().copied());

        for (target, target_evidence) in &evidence.by_target {
            let entry = self.targets.entry(*target).or_default();
            entry.paths.extend(target_evidence.paths.iter().cloned());
            entry
                .warnings
                .extend(target_evidence.warnings.iter().cloned());
            if target_evidence.skipped.is_some() {
                entry.skipped = target_evidence.skipped.clone();
            }
            if target_evidence.failure.is_some() {
                entry.failure = target_evidence.failure.clone();
            }
        }
    }

    /// Merge in `Canonical`-phase evidence. Equivalent to
    /// `record(ProjectionPhase::Canonical, evidence)`.
    pub fn record_canonical(&mut self, evidence: &projection::ProjectionEvidence) {
        self.record(ProjectionPhase::Canonical, evidence);
    }

    /// Merge in `Machine`-phase evidence. Equivalent to
    /// `record(ProjectionPhase::Machine, evidence)`.
    pub fn record_machine(&mut self, evidence: &projection::ProjectionEvidence) {
        self.record(ProjectionPhase::Machine, evidence);
    }

    /// Whether a target's accumulated per-path history contains at least one
    /// distinct path with real, surviving change evidence — the net-effect
    /// test documented on step 4 of this type's own precedence order. Groups
    /// `state.paths` by `path` (preserving each path's own recording order,
    /// which is chronological since `record` only ever appends), then checks
    /// each distinct path's own trailing evidence: a last change of
    /// `Written`/`Linked` counts; a last change of `Removed` counts UNLESS it
    /// directly follows a `Written` for that same path (the rolled-back-AGY
    /// shape), in which case that path contributes no evidence.
    fn has_net_change(state: &ProjectionTargetAccumState) -> bool {
        let mut history: std::collections::BTreeMap<
            &PathBuf,
            Vec<&projection::ProjectionPathChange>,
        > = std::collections::BTreeMap::new();
        for p in &state.paths {
            history.entry(&p.path).or_default().push(&p.change);
        }
        history.values().any(|changes| match changes.last() {
            Some(projection::ProjectionPathChange::Removed) => {
                let preceded_by_written = changes.len() >= 2
                    && *changes[changes.len() - 2] == projection::ProjectionPathChange::Written;
                !preceded_by_written
            }
            Some(_) => true,
            None => false,
        })
    }

    /// Step 3 of this type's precedence order: whether every phase that can
    /// structurally complete `target` has actually listed it in that phase's
    /// completed-targets set. The match is deliberately exhaustive so a new
    /// `ProjectionTargetId` variant forces an explicit classification here
    /// rather than silently inheriting the dual-phase rule.
    ///
    /// - Dual-phase targets (`Claude`/`Codex`/`Copilot`): the canonical
    ///   render/lifecycle chain and the machine skills/commands/agents pass
    ///   both cover them, so both phases must complete.
    /// - Canonical-only targets (`AgyCli`/`AgyIde`): completed exclusively
    ///   by the `Canonical` phase's AGY junction apply.
    /// - Machine-only targets (`Opencode`/`AgyGui`): completed
    ///   exclusively by the `Machine` phase (`run_machine_update` completes
    ///   every parseable selected target; the `Canonical` phase's completed
    ///   set never contains these two), so `Machine`-phase completion
    ///   alone is sufficient — requiring the canonical conjunct would leave
    ///   them permanently `NotRun` even on a fully clean run.
    fn required_phases_completed(&self, target: ProjectionTargetId) -> bool {
        match target {
            ProjectionTargetId::Claude
            | ProjectionTargetId::Codex
            | ProjectionTargetId::Copilot => {
                self.canonical_completed.contains(&target)
                    && self.machine_completed.contains(&target)
            }
            ProjectionTargetId::AgyCli | ProjectionTargetId::AgyIde => {
                self.canonical_completed.contains(&target)
            }
            ProjectionTargetId::Opencode | ProjectionTargetId::AgyGui => {
                self.machine_completed.contains(&target)
            }
        }
    }

    /// Non-consuming counterpart to [`Self::finish`]: derive the run's
    /// [`ProjectionRunReport`] as it stands right now, without ending the
    /// accumulator's usable lifetime.
    ///
    /// A caller with multiple return points before the run's natural end
    /// (an early abort that skips the Machine phase entirely, a host-apply
    /// failure that must still let the Machine phase run) needs to inspect
    /// — or return — the accumulator's derived status at more than one
    /// point, while still owning the accumulator afterward for later calls
    /// (`record_machine`, a later `snapshot()`, or the eventual `finish()`).
    /// `finish()` cannot serve that since it consumes `self`. Both methods
    /// share the exact same precedence-order derivation via
    /// [`Self::derive_report`] — they differ only in whether they consume
    /// `self`, never in how a target's status is derived.
    pub fn snapshot(&self) -> ProjectionRunReport {
        self.derive_report()
    }

    /// Terminally derive the run's [`ProjectionRunReport`], walking the
    /// precedence order documented on this type for every target that was
    /// selected up front, appeared in either phase's completed set, or
    /// carries recorded per-target state.
    pub fn finish(self) -> ProjectionRunReport {
        self.derive_report()
    }

    /// Shared precedence-order derivation used by both [`Self::snapshot`]
    /// (borrows) and [`Self::finish`] (consumes) — see this type's own doc
    /// comment for the exact precedence order walked here. Kept as the one
    /// place this logic is written so `snapshot()` and `finish()` can never
    /// drift apart.
    fn derive_report(&self) -> ProjectionRunReport {
        let mut all_targets = self.selected.clone();
        all_targets.extend(self.canonical_completed.iter().copied());
        all_targets.extend(self.machine_completed.iter().copied());
        all_targets.extend(self.targets.keys().copied());

        let mut targets = std::collections::BTreeMap::new();
        for target in all_targets {
            let state = self.targets.get(&target);

            let status = if let Some(failure) = state.and_then(|s| s.failure.clone()) {
                ProjectionTargetStatus::Failed {
                    path: failure.path,
                    reason: failure.reason,
                }
            } else if let Some(reason) = state.and_then(|s| s.skipped.clone()) {
                ProjectionTargetStatus::Skipped { reason }
            } else if self.required_phases_completed(target) {
                let has_change = state.map(Self::has_net_change).unwrap_or(false);
                if has_change {
                    ProjectionTargetStatus::Applied
                } else {
                    ProjectionTargetStatus::Unchanged
                }
            } else {
                ProjectionTargetStatus::NotRun
            };

            let (paths, warnings) = match state {
                Some(s) => (s.paths.clone(), s.warnings.clone()),
                None => (Vec::new(), Vec::new()),
            };

            targets.insert(
                target,
                ProjectionTargetResult {
                    target,
                    status,
                    paths,
                    warnings,
                },
            );
        }

        ProjectionRunReport {
            common: self.common.clone(),
            targets,
            mode: self.mode.clone(),
            run_warnings: self.run_warnings.clone(),
        }
    }
}

/// Which underlying error type a [`ProjectionPhaseFailure`] wraps.
///
/// The `Canonical` phase's own sub-steps raise two different error enums
/// depending on which sub-step fails — this engine's own [`InstallError`]
/// (canonical-root render, lifecycle artifacts) or
/// `projection::agy::AgyError` (AGY junction apply) — while the `Machine`
/// phase raises `projection::ProjectionError`. None of the three enums can
/// represent the other two's variants without lossy coercion (e.g. flattening
/// to `String`), so wrapping each verbatim behind one enum is the most honest
/// "either phase's underlying error" representation available today, and it
/// keeps each source error's own `Display`/`source()` intact for whichever
/// later caller inspects `ProjectionPhaseFailure::error`.
#[derive(Debug)]
pub enum ProjectionPhaseError {
    /// The engine's own install-level failure (`Canonical` phase).
    Install(InstallError),
    /// The AGY sub-step's own failure (`Canonical` phase).
    Agy(projection::agy::AgyError),
    /// The projection crate's machine-phase failure (`Machine` phase).
    Projection(projection::ProjectionError),
}

impl std::fmt::Display for ProjectionPhaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProjectionPhaseError::Install(e) => write!(f, "{e}"),
            ProjectionPhaseError::Agy(e) => write!(f, "{e}"),
            ProjectionPhaseError::Projection(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ProjectionPhaseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ProjectionPhaseError::Install(e) => Some(e),
            ProjectionPhaseError::Agy(e) => Some(e),
            ProjectionPhaseError::Projection(e) => Some(e),
        }
    }
}

/// A single projection phase's unexpected/global fault: the underlying error
/// (see [`ProjectionPhaseError`]) alongside the [`projection::ProjectionEvidence`]
/// that had already accumulated before it — mirrors `projection::ProjectionFailure`'s
/// error+partial-evidence shape at the engine's phase level.
///
#[derive(Debug)]
pub struct ProjectionPhaseFailure {
    /// The fault that ended the phase.
    pub error: ProjectionPhaseError,
    /// Evidence accumulated by the phase before the fault occurred.
    pub partial: projection::ProjectionEvidence,
}

/// The canonical projection evidence and any run-global fault encountered
/// while composing it.
#[derive(Debug)]
pub struct CanonicalRunOutcome {
    pub evidence: projection::ProjectionEvidence,
    pub fault: Option<ProjectionPhaseFailure>,
    /// Route facts for each route the provider rollout planned. Empty for
    /// ordinary sync, which keeps the combined surface.
    pub routes: Vec<crate::managed_view::RouteEvidence>,
    /// Retirement of the combined surface, per cutover target. A target with
    /// no entry was not part of this run's cutover.
    pub route_retirements: BTreeMap<ProjectionTargetId, RouteRetirement>,
    /// Routes the route plan blocked, each with the reason it was blocked.
    pub route_conflicts: Vec<RouteConflict>,
}

/// Whether one target's combined `ccync` surface was retired in this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteRetirement {
    /// The combined surface for this target was removed.
    Done,
    /// The combined surface stays, for the stated reason.
    Kept { reason: String },
}

/// One route the route plan blocked. It is never published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteConflict {
    pub target: ProjectionTargetId,
    /// Component the route was planned for, before any alias.
    pub component: String,
    /// Reason the route plan gave for blocking the route.
    pub reason: String,
}

/// Run `ccync sync`/`refresh` — re-render canonical root and refresh all
/// runtime surfaces, returning the complete Canonical-phase
/// [`projection::ProjectionEvidence`] this run composed.
///
/// Claude and Copilot read surfaces are inside the canonical root. AGY
/// junctions are re-applied best-effort.
///
/// Covers `Claude`/`Copilot`/`Codex`/`AgyCli`/`AgyIde` — exactly the targets
/// this function's own body genuinely applies (its Step 4 AGY block plus
/// `apply_lifecycle_artifact_chain`'s own composition). `AgyGui` is a
/// separate, decomposed skill+mcp_config projection this function does not
/// touch.
///
/// Target-scoped failures and best-effort cleanup warnings land in the returned
/// evidence's `run_warnings`, in the exact order they occurred. A run-global
/// canonical-root render failure is returned as a typed fault with the partial
/// evidence composed before that failure.
///
/// This function's only `Err` path (`InstallError::NoHome` / `InstallError::Io`
/// from resolving/creating the canonical root, below) occurs before either
/// evidence-producing call runs, so on `Err` there is genuinely no partial
/// evidence to report — matching the "nothing attempted yet" convention
/// `apply_agy_evidence`'s own constructor-fault arm already uses.
/// `ccync_foundation::paths::canonical_plugin_root` and the `user_home` lookup
/// `AgyProjection::new` depends on both resolve the identical
/// `HOME`/`USERPROFILE` environment value, so no real-world input can make
/// the canonical-root resolution below succeed while a later step's own
/// home-dependent setup fails — later steps retain their own best-effort
/// warnings, while the canonical render fault remains typed in the outcome.
pub fn run_update_with_evidence(config: &CcyncConfig) -> Result<CanonicalRunOutcome, InstallError> {
    run_update_with_handoff(config).map(|(outcome, _scope)| outcome)
}

/// Same as [`run_update_with_evidence`], with native CLI calls routed through
/// `runner`.
pub fn run_update_with_evidence_using(
    config: &CcyncConfig,
    runner: &dyn crate::plugin_native::NativeCommandRunner,
) -> Result<CanonicalRunOutcome, InstallError> {
    run_update_with_handoff_using(config, runner).map(|(outcome, _scope)| outcome)
}

/// Same run as [`run_update_with_evidence`], also returning the direct
/// delegation scope this run prepared. A caller that then runs the machine
/// writers passes the scope to `projection::run_machine_update_with_scope`, so
/// those writers leave every path that direct publication owns.
pub fn run_update_with_handoff(
    config: &CcyncConfig,
) -> Result<(CanonicalRunOutcome, projection::MachineProjectionScope), InstallError> {
    let _lifecycle_guard = crate::tool_manager::lock::LifecycleLock::acquire_if_needed()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    run_update_with_handoff_using(config, &crate::plugin_native::ProcessNativeRunner)
}

/// Reads `_adoption.targetSnapshot` from the personal catalog. Returns `None`
/// when the catalog, namespace, or snapshot is absent or empty.
fn read_init_target_snapshot(catalog_path: &Path) -> Result<Option<Vec<String>>, InstallError> {
    let text = match fs::read_to_string(catalog_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(InstallError::Io(error)),
    };
    let catalog: Value = serde_json::from_str(&text).map_err(InstallError::Json)?;
    Ok(catalog
        .get("_adoption")
        .and_then(|adoption| adoption.get("targetSnapshot"))
        .and_then(Value::as_array)
        .map(|targets| {
            targets
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|targets| !targets.is_empty()))
}

/// Same as [`run_update_with_handoff`], with every native CLI call routed
/// through `runner`. Tests pass an injected runner so no host CLI is reached.
pub fn run_update_with_handoff_using(
    config: &CcyncConfig,
    runner: &dyn crate::plugin_native::NativeCommandRunner,
) -> Result<(CanonicalRunOutcome, projection::MachineProjectionScope), InstallError> {
    let _lifecycle_guard = crate::tool_manager::lock::LifecycleLock::acquire_if_needed()
        .map_err(|error| InstallError::Io(std::io::Error::other(error.to_string())))?;
    verify_init_selection_snapshot(config)?;
    verify_managed_sources_present()?;
    validate_standalone_source_collections()?;
    let inventory =
        read_mcp_declarations().map_err(|error| invalid_lifecycle_state(error.to_string()))?;
    let held_lock =
        crate::tool_manager::lock::LifecycleLock::current_proof().expect("lifecycle lock held");
    let mut staged = stage_provider_surfaces(&held_lock, runner);
    let canonical_root =
        ccync_foundation::paths::canonical_plugin_root().ok_or(InstallError::NoHome)?;
    fs::create_dir_all(&canonical_root)?;
    let plugin_dirs = managed_plugin_dirs();
    let legacy_referents = crate::paths::user_home()
        .map(|home| crate::delivery_apply::capture_legacy_referents(&home, &canonical_root))
        .unwrap_or_default();
    let rendered = render_canonical_root_with_sources(&canonical_root, &plugin_dirs);
    let mut source_warnings = Vec::new();
    let mut source_error = None;
    let mut sources = match rendered {
        Ok((warnings, sources)) => {
            source_warnings = warnings;
            sources
        }
        Err(error) => {
            source_error = Some(error);
            BTreeMap::new()
        }
    };
    // An old rendered directory without fresh source proof is never
    // republished. A bare file child is not a skill directory and never was
    // published as standalone content, so it carries no proof to demand.
    for entry in fs::read_dir(canonical_root.join("skills"))
        .into_iter()
        .flatten()
        .flatten()
    {
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        sources.entry(name).or_insert_with(|| {
            StandaloneSkillSource::blocked("fresh winning source proof unavailable")
        });
    }
    let standalone =
        if let (Some(home), Some(lockfile)) = (crate::paths::user_home(), provider_lock_path()) {
            let targets = staged
                .selected
                .iter()
                .filter_map(|target| target.parse().ok())
                .collect::<Vec<_>>();
            let evidence = crate::delivery_apply::StandaloneEvidence {
                selected: targets,
                // Removal intent is provable only when this run rendered every source.
                desired_origins: if source_error.is_none() {
                    desired_standalone_origins()
                } else {
                    None
                },
                native_failed: staged.failed_targets.clone(),
                legacy_referents,
            };
            Some(crate::delivery_apply::publish_standalone_sources(
                &held_lock,
                &sources,
                staged.route_plan.as_ref(),
                &evidence,
                &home,
                &canonical_root,
                &lockfile,
            ))
        } else {
            None
        };
    if let Some(standalone) = &standalone {
        staged
            .failed_targets
            .extend(standalone.failed.iter().copied());
    }

    // Decide retirement before the legacy chain runs, because that chain
    // recreates the combined Claude link and entries unless told not to.
    let mut retirement = plan_combined_retirement(&mut staged);
    let mut provider_evidence = std::mem::take(&mut staged.evidence);
    provider_evidence.run_warnings.append(&mut retirement.notes);
    // Route evidence reads the lockfile that publication just rewrote, so
    // ownership reflects what this run wrote.
    let mut routes: Vec<crate::managed_view::RouteEvidence> = Vec::new();
    let mut route_conflicts: Vec<RouteConflict> = Vec::new();
    if let Some(route_plan) = &staged.route_plan {
        route_conflicts = crate::delivery_apply::route_conflicts(route_plan);
    }
    if let (Some(route_plan), Some(home), Some(lockfile)) = (
        &staged.route_plan,
        crate::paths::user_home(),
        provider_lock_path(),
    ) {
        routes = crate::delivery_apply::route_evidence(route_plan, &home, &lockfile);
        for route in &mut routes {
            route.registration = retirement.registrations.get(&route.target).cloned();
        }
    }
    // Direct provider routes are independent of the combined staging listing.
    // The combined Claude link stays only when it is not being retired.

    // Step 2: Resolve the canonical root path. ccync projects managed bundles +
    // aggregated MCP under this root; it does NOT bake ccync-core content (see D1/D8).

    let mut composed_evidence = projection::ProjectionEvidence::default();
    let mut fault = None;
    merge_projection_evidence(&mut composed_evidence, provider_evidence);

    /// Refuses a run whose `config.json` selection disagrees with the target
    /// snapshot the last `ccync init` recorded in the personal catalog. Runs
    /// before any mutation.
    ///
    /// A missing or empty snapshot is a legacy machine and is accepted. A config
    /// with no selection key has nothing to compare and is accepted too.
    fn verify_init_selection_snapshot(config: &CcyncConfig) -> Result<(), InstallError> {
        let catalog_path = effective_layout()?.personal_catalog_path();
        let Some(recorded) = read_init_target_snapshot(&catalog_path)? else {
            return Ok(());
        };
        let resolution = config.resolve_projection_targets();
        if resolution.state == LegacyState::Absent {
            return Ok(());
        }
        let selected: BTreeSet<String> = resolution
            .targets
            .iter()
            .map(|target| target.as_str().to_string())
            .collect();
        let recorded: BTreeSet<String> = recorded.into_iter().collect();
        if selected == recorded {
            return Ok(());
        }
        let list = |set: &BTreeSet<String>| set.iter().cloned().collect::<Vec<_>>().join(", ");
        Err(InstallError::SelectionChangedOutsideInit(format!(
        "ccync: the selected targets changed outside `ccync init` — config.json selects [{}] but the last `ccync init` recorded [{}]; run `ccync init <claude|codex|copilot|opencode|agy>` to reselect, then `ccync sync`",
        list(&selected),
        list(&recorded)
    )))
    }

    /// Refuses a run when an enabled plugin has no source directory (bundled) or
    /// no cache directory (personal) on disk. The projection would otherwise drop
    /// that plugin from the render set and remove its surfaces, so a missing
    /// directory is treated as an error, not as removal intent. Runs before any
    /// mutation.
    fn verify_managed_sources_present() -> Result<(), InstallError> {
        let mut missing = Vec::new();
        if let Some(plugins_root) = crate::paths::ccync_plugins_root() {
            for (id, resolved_source) in load_enabled_bundles() {
                let dir = plugins_root.join(&resolved_source);
                if !dir.is_dir() {
                    missing.push(format!("{id} (source {})", dir.display()));
                }
            }
        }
        let cache_root = effective_layout()?.source_cache_path();
        for (id, _) in load_enabled_personal_plugins() {
            if cache_candidates(&cache_root, &id).is_empty() {
                missing.push(format!("{id} (cache under {})", cache_root.display()));
            }
        }
        if missing.is_empty() {
            return Ok(());
        }
        Err(InstallError::ManagedSourceMissing(format!(
        "ccync: enabled plugin directories are missing, so nothing was changed: {}; restore them, then run `ccync sync` again",
        missing.join(", ")
    )))
    }

    for warning in source_warnings {
        eprintln!("ccync: {warning}");
    }
    if let Some(error) = source_error {
        fault = Some(ProjectionPhaseFailure {
            error: ProjectionPhaseError::Install(error),
            partial: composed_evidence.clone(),
        });
    }
    if let Some(standalone) = &standalone {
        merge_projection_evidence(&mut composed_evidence, standalone.evidence.clone());
        routes.extend(standalone.routes.clone());
        // Every processed or pending path survives combined-copy pruning.
        for root in &mut retirement.written_roots {
            for decision in &standalone.scope.decisions {
                if let Ok(relative) = decision.destination.strip_prefix(&root.path) {
                    if let Some(name) = relative
                        .components()
                        .next()
                        .and_then(|part| part.as_os_str().to_str())
                    {
                        root.keep.insert(name.to_string());
                    }
                }
            }
        }
    }

    let standalone_failed = standalone.as_ref().map(|run| &run.failed);
    let can_keep_fallback =
        |target| !standalone_failed.is_some_and(|failed| failed.contains(&target));
    let existing_claude = crate::paths::user_home()
        .is_some_and(|home| home.join(".claude/skills/ccync").symlink_metadata().is_ok());
    let legacy_claude_link =
        !retirement.claude && (can_keep_fallback(ProjectionTargetId::Claude) || existing_claude);
    let legacy_codex_entry = !retirement.codex && can_keep_fallback(ProjectionTargetId::Codex);
    let legacy_copilot_plugin = !plugin_root_retired(&retirement, ProjectionTargetId::Copilot)
        && can_keep_fallback(ProjectionTargetId::Copilot);

    // Step 3: Claude skill surface projection — `~/.claude/skills/ccync` → canonical root.
    // This is selection-gated: the sole apply call lives in
    // `apply_lifecycle_artifact_chain` below, run only when "claude" is a
    // selected primary surface. No unconditional call belongs here — an
    // unselected Claude runtime must never see a skill link created.

    // Step 4: AGY plugin-junction projection — best-effort and never fatal.
    // The CLI / IDE junctions are gated by their own runtime keys. The GUI root
    // is handled after this block: it is written only as the fallback for a
    // failed `agy-gui` direct publication.
    //
    // Delegates to `apply_agy_evidence`, then translates its evidence
    // back into this exact legacy push/warning shape: `agy.apply(cli, ide)`
    // was one atomic call — CLI failing (or IDE failing after CLI succeeded
    // and was rolled back) is a single failure with a single warning, never a
    // partial "agy" push. `apply_agy_evidence`'s own `Err` arm requires a
    // constructor/setup fault (unresolvable home) with no per-target failure
    // to report — reformatted to the same "AGY init (best-effort)" wording
    // the legacy constructor-failure branch used; its other error variant is
    // not reachable through this call site's real inputs (see
    // `apply_agy_evidence`'s own doc comment) but is handled defensively.
    let agy_selection = load_runtime_selection();
    // A face whose direct publication succeeded has no combined writer here.
    let want_agy_cli = agy_selection
        .selected_runtimes
        .iter()
        .any(|r| r == "agy-cli")
        && !plugin_root_retired(&retirement, ProjectionTargetId::AgyCli)
        && can_keep_fallback(ProjectionTargetId::AgyCli);
    let want_agy_ide = agy_selection
        .selected_runtimes
        .iter()
        .any(|r| r == "agy-ide")
        && !plugin_root_retired(&retirement, ProjectionTargetId::AgyIde)
        && can_keep_fallback(ProjectionTargetId::AgyIde);
    if want_agy_cli || want_agy_ide {
        // `run_update_with_evidence` itself carries no `dry_run` parameter —
        // every real-world caller (`lifecycle.rs`, `plugin.rs`) invokes it
        // unconditionally for a genuine update, matching the pre-existing
        // caller contract `AgyProjection::apply`'s own dry-run docs already
        // relied on. `false` here preserves that exact prior behavior.
        match apply_agy_evidence(&canonical_root, want_agy_cli, want_agy_ide, false) {
            Ok(evidence) => {
                let agy_fully_completed = (!want_agy_cli
                    || evidence
                        .completed_targets
                        .contains(&ProjectionTargetId::AgyCli))
                    && (!want_agy_ide
                        || evidence
                            .completed_targets
                            .contains(&ProjectionTargetId::AgyIde));
                if !agy_fully_completed {
                    let reason = [ProjectionTargetId::AgyCli, ProjectionTargetId::AgyIde]
                        .iter()
                        .find_map(|target| {
                            evidence
                                .by_target
                                .get(target)
                                .and_then(|te| te.failure.as_ref())
                        })
                        .map(|failure| failure.reason.clone())
                        .unwrap_or_default();
                    composed_evidence
                        .run_warnings
                        .push(format!("AGY projection (best-effort): {reason}"));
                }
                merge_projection_evidence(&mut composed_evidence, evidence);
            }
            Err(failure) => {
                // Destructure so `failure.partial` is still available after
                // matching on `failure.error` below (the legacy `match
                // failure.error { .. }` form would otherwise move it whole).
                let ProjectionPhaseFailure { error, partial } = *failure;
                let warning = match error {
                    ProjectionPhaseError::Agy(e) => format!("AGY init (best-effort): {e}"),
                    other => format!("AGY projection (best-effort): {other}"),
                };
                composed_evidence.run_warnings.push(warning);
                // Defensive: not reachable through real inputs today (see
                // this function's own doc comment), and `partial` is always
                // empty on this arm in practice, but merging it keeps the
                // composed evidence honest if that ever changes.
                merge_projection_evidence(&mut composed_evidence, partial);
            }
        }
    }

    // AGY GUI root: a fallback writer, like the CLI and IDE roots above. It runs
    // only when `agy-gui` is selected and its direct publication did not retire
    // the combined root. Otherwise the root is retired in `retire_plugin_roots`.
    let want_agy_gui = agy_selection
        .selected_runtimes
        .iter()
        .any(|r| r == "agy-gui")
        && !plugin_root_retired(&retirement, ProjectionTargetId::AgyGui)
        && can_keep_fallback(ProjectionTargetId::AgyGui);
    if want_agy_gui {
        match AgyProjection::new(canonical_root.to_path_buf()) {
            Ok(agy) => {
                let fallback_owner;
                let ownership = match retirement.ownership.as_ref() {
                    Some(owner) => owner,
                    None => {
                        let lockfile = provider_lock_path().unwrap_or_default();
                        fallback_owner = projection::AgyRootOwnership::from_lockfile(&lockfile);
                        &fallback_owner
                    }
                };
                let evidence = agy.publish_gui_root(ownership, false);
                merge_projection_evidence(&mut composed_evidence, evidence);
            }
            Err(e) => composed_evidence
                .run_warnings
                .push(format!("AGY GUI projection (best-effort): {e}")),
        }
    }

    // Delegates Claude/Copilot/Codex/AGY-runtime-state to
    // `apply_lifecycle_artifact_chain`, which mirrors the retired legacy
    // chain's exact single-`?`-propagation short-circuit. The chain's own
    // failure reason is already formatted exactly as the retired legacy
    // chain's own `?`-propagated `InstallError` would have displayed, so the
    // "runtime lifecycle state: " prefix applies directly with no re-wrapping.
    let LifecycleArtifactOutcome {
        failure_reason,
        evidence: lifecycle_evidence,
        ..
    } = apply_lifecycle_artifact_chain_with(
        &canonical_root,
        legacy_claude_link,
        legacy_codex_entry,
        legacy_copilot_plugin,
    );
    if let Some(reason) = failure_reason {
        composed_evidence
            .run_warnings
            .push(format!("runtime lifecycle state: {reason}"));
    }
    let selected = primary_targets(&load_runtime_selection());
    let plugin_kept = retire_plugin_roots(&retirement, &selected, &mut composed_evidence);
    // A Claude failure short-circuits the lifecycle chain before Copilot is
    // reached, and a failure of Copilot's own chain step means it never
    // completed. Either way a retired Copilot stays out of `completed_targets`.
    let copilot_chain_blocked = [ProjectionTargetId::Claude, ProjectionTargetId::Copilot]
        .iter()
        .any(|target| {
            lifecycle_evidence
                .by_target
                .get(target)
                .is_some_and(|te| te.failure.is_some())
        });
    // A retired target completed through direct publication, without its
    // combined writer, so the run reports it as completed.
    for target in [
        ProjectionTargetId::Copilot,
        ProjectionTargetId::AgyCli,
        ProjectionTargetId::AgyIde,
        ProjectionTargetId::AgyGui,
    ] {
        if target == ProjectionTargetId::Copilot && copilot_chain_blocked {
            continue;
        }
        if plugin_root_retired(&retirement, target) {
            composed_evidence.completed_targets.insert(target);
        }
    }
    let route_retirements = finish_combined_retirement(
        &retirement,
        &canonical_root,
        &lifecycle_evidence,
        &plugin_kept,
        &mut composed_evidence,
    );
    merge_projection_evidence(&mut composed_evidence, lifecycle_evidence);

    // Generate merged managed.mcp.json from core + enabled bundles (best-effort).
    if let Err(e) = publish_managed_mcp(&inventory) {
        composed_evidence
            .run_warnings
            .push(format!("managed MCP generation (best-effort): {e}"));
    }

    // Best-effort migration: the new dist/targets state is written above; now remove
    // the pre-rename dist/providers and dist/runtimes trees so updated installs
    // don't keep an orphan. Failure is a warning, never fatal (mirrors the
    // best-effort projection contract).
    if let Ok(stale) = stale_dist_providers_root() {
        if let Err(e) = remove_path_if_present(&stale) {
            composed_evidence
                .run_warnings
                .push(format!("stale dist/providers cleanup (best-effort): {e}"));
        }
    }
    if let Ok(stale) = stale_dist_runtimes_root() {
        if let Err(e) = remove_path_if_present(&stale) {
            composed_evidence
                .run_warnings
                .push(format!("stale dist/runtimes cleanup (best-effort): {e}"));
        }
    }
    if let Ok(stale) = stale_ledger_path() {
        if let Err(e) = remove_path_if_present(&stale) {
            composed_evidence
                .run_warnings
                .push(format!("stale ledger.json cleanup (best-effort): {e}"));
        }
    }

    let scope = standalone.map(|run| run.scope).unwrap_or_else(|| {
        crate::delivery_apply::delegation_scope(
            staged.route_plan.as_ref(),
            crate::paths::user_home().as_deref(),
        )
    });
    Ok((
        CanonicalRunOutcome {
            evidence: composed_evidence,
            fault,
            routes,
            route_retirements,
            route_conflicts,
        },
        scope,
    ))
}

/// Kept so existing callers that pass a rollout value still compile. The
/// value is ignored: every sync stages provider generations and direct
/// surfaces through [`run_update_with_evidence`].
pub struct ProviderRollout {
    /// Ignored. Retained only for source compatibility.
    pub enabled: bool,
}

/// Same as [`run_update_with_evidence`]. The rollout argument is ignored.
pub fn run_update_with_provider_rollout(
    config: &CcyncConfig,
    rollout: &ProviderRollout,
) -> Result<CanonicalRunOutcome, InstallError> {
    run_update_with_provider_rollout_using(
        config,
        rollout,
        &crate::plugin_native::ProcessNativeRunner,
    )
}

/// Same as [`run_update_with_provider_rollout`], with native CLI calls routed
/// through `runner`. Tests pass an injected runner so no host CLI is reached.
pub fn run_update_with_provider_rollout_using(
    config: &CcyncConfig,
    _rollout: &ProviderRollout,
    runner: &dyn crate::plugin_native::NativeCommandRunner,
) -> Result<CanonicalRunOutcome, InstallError> {
    run_update_with_evidence_using(config, runner)
}

/// Provider publication evidence produced by one sync.
struct StagedProviderSurfaces {
    evidence: projection::ProjectionEvidence,
    selected: Vec<String>,
    /// Targets the journal recorded for this cutover.
    cutover_targets: Vec<ProjectionTargetId>,
    /// True only when direct publication ran after the cutover was journaled.
    publication_ran: bool,
    /// Targets whose direct publication reported a failure.
    failed_targets: std::collections::BTreeSet<ProjectionTargetId>,
    /// Route plan from direct publication, used to group skill roots.
    route_plan: Option<crate::delivery::DeliveryPlan>,
    /// Ownership recorded before publication, which publication may rewrite.
    prior_ownership: Option<projection::AgyRootOwnership>,
}

/// Combined-surface retirement decided before the legacy lifecycle chain runs.
struct CombinedRetirement {
    /// Targets the journal recorded for this cutover.
    cutover_targets: Vec<ProjectionTargetId>,
    /// True when direct publication ran after the cutover was journaled.
    publication_ran: bool,
    /// Retire the Claude legacy link and marketplace entry in this run.
    claude: bool,
    /// Retire the Codex marketplace entry in this run.
    codex: bool,
    /// Targets that keep their combined surface: failed, user-registered, or unreadable.
    blocked: BTreeSet<ProjectionTargetId>,
    /// Combined `ccync` copies, each with the targets that depend on it.
    copies: Vec<CombinedCopy>,
    /// Skill roots this rollout wrote, with the names the route plan keeps.
    written_roots: Vec<WrittenSkillRoot>,
    /// Lockfile ownership recorded before publication. Authorizes removal.
    ownership: Option<projection::AgyRootOwnership>,
    /// Warnings for targets that keep their combined surface.
    notes: Vec<String>,
    /// Registration state ccync read for targets it does not write, so route
    /// evidence can name it.
    registrations: BTreeMap<ProjectionTargetId, crate::managed_view::RouteRegistration>,
    /// Reason each blocked target keeps its combined surface, for route labels.
    kept_reasons: BTreeMap<ProjectionTargetId, String>,
}

/// One combined `ccync` copy under a physical skill root.
#[derive(Debug)]
struct CombinedCopy {
    /// `<root>/ccync`, the combined copy in that root.
    path: PathBuf,
    /// Targets whose retirement waits on this copy.
    targets: Vec<ProjectionTargetId>,
    /// Targets that must be ready before this copy is removed. A shared root
    /// whose direct copies were all written waits on no target, except the
    /// Claude link, which follows the Claude combined entry.
    wait_on: Vec<ProjectionTargetId>,
}

/// A physical skill root this rollout wrote.
#[derive(Debug)]
struct WrittenSkillRoot {
    /// Physical root path, for example `~/.copilot/skills`.
    path: PathBuf,
    /// Skill names the route plan keeps in this root.
    keep: BTreeSet<String>,
}

/// Name of the combined ccync copy inside each skill root.
const COMBINED_COPY_NAME: &str = "ccync";

/// Manual removal hint for the Codex plugin. Codex registration is not read.
const CODEX_MANUAL_REMOVAL_HINT: &str =
    "codex plugin remove ccync@ccync-marketplace ; codex plugin marketplace remove ccync-marketplace";

/// Whether the user registered the combined Claude plugin.
enum ClaudeRegistration {
    /// No user-level registration exists.
    Absent,
    /// The user registered `ccync@ccync`. ccync must not write that file.
    RegisteredByUser { uninstall_command: String },
    /// The registration file exists but cannot be read.
    Unreadable { reason: String },
}

fn claude_registration() -> ClaudeRegistration {
    let Some(path) = ccync_foundation::paths::claude_installed_plugins_path() else {
        return ClaudeRegistration::Absent;
    };
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ClaudeRegistration::Absent;
        }
        Err(error) => {
            return ClaudeRegistration::Unreadable {
                reason: error.to_string(),
            };
        }
    };
    let parsed: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            return ClaudeRegistration::Unreadable {
                reason: error.to_string(),
            };
        }
    };
    let template = claude_uninstall_template();
    let commands = parsed
        .get("plugins")
        .and_then(Value::as_object)
        .map(|plugins| {
            plugins
                .iter()
                .filter(|(key, _)| key.ends_with("@ccync"))
                .map(|(key, entries)| claude_uninstall_command(&template, key, entries))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if commands.is_empty() {
        ClaudeRegistration::Absent
    } else {
        ClaudeRegistration::RegisteredByUser {
            uninstall_command: commands.join(" ; "),
        }
    }
}

/// The Claude uninstall command template from the state file, with a
/// fallback when the state file names none.
fn claude_uninstall_template() -> String {
    target_state_path("claude")
        .ok()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|state| {
            state
                .pointer("/lifecycle/uninstallCommandTemplate")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "claude plugin uninstall <plugin> --scope <scope>".to_string())
}

/// The uninstall command for one registered `<plugin>@ccync` key.
///
/// The plugin placeholder becomes the registered key. The scope placeholder
/// becomes the scope the registration records. When no entry records a
/// scope, the scope option is dropped, so the command never keeps a
/// placeholder.
fn claude_uninstall_command(template: &str, key: &str, entries: &Value) -> String {
    let scope = entries
        .as_array()
        .into_iter()
        .flatten()
        .chain(entries.is_object().then_some(entries))
        .find_map(|entry| entry.get("scope").and_then(Value::as_str))
        .filter(|scope| !scope.trim().is_empty());
    let command = template.replace("<plugin>", key);
    match scope {
        Some(scope) => command.replace("<scope>", scope),
        None => command
            .replace(" --scope <scope>", "")
            .replace("--scope <scope>", "")
            .replace(" <scope>", "")
            .trim()
            .to_string(),
    }
}

/// Decide which combined surfaces this rollout may retire. A target whose
/// direct publication failed, or that the user still registers, keeps its
/// combined surface.
fn plan_combined_retirement(staged: &mut StagedProviderSurfaces) -> CombinedRetirement {
    let mut plan = CombinedRetirement {
        cutover_targets: staged.cutover_targets.clone(),
        publication_ran: staged.publication_ran,
        claude: false,
        codex: false,
        blocked: BTreeSet::new(),
        copies: Vec::new(),
        written_roots: Vec::new(),
        ownership: staged.prior_ownership.take(),
        notes: Vec::new(),
        registrations: BTreeMap::new(),
        kept_reasons: BTreeMap::new(),
    };
    if !staged.publication_ran {
        return plan;
    }
    let undiscoverable = staged
        .route_plan
        .as_ref()
        .map(crate::delivery_apply::undiscoverable_targets)
        .unwrap_or_default();
    for target in &staged.cutover_targets {
        if staged.failed_targets.contains(target) {
            plan.blocked.insert(*target);
            plan.kept_reasons
                .insert(*target, "direct publication failed".to_string());
            plan.notes.push(format!(
                "{target}: direct publication failed, so its combined ccync surface stays on private roots. Fix the reported path, then rerun `ccync sync` to retry retirement."
            ));
            continue;
        }
        // A component the target cannot discover without a user step means
        // direct publication did not replace the combined surface.
        if let Some(reason) = undiscoverable.get(target) {
            plan.blocked.insert(*target);
            plan.kept_reasons.insert(
                *target,
                format!("not every component is discoverable without a user step: {reason}"),
            );
            plan.notes.push(format!(
                "{target}: {reason}, so not every component is discoverable without a user step and its combined ccync surface stays. Resolve the conflict, then rerun `ccync sync` to retry retirement."
            ));
            continue;
        }
        match target {
            ProjectionTargetId::Claude => match claude_registration() {
                ClaudeRegistration::Absent => plan.claude = true,
                ClaudeRegistration::RegisteredByUser { uninstall_command } => {
                    plan.blocked.insert(*target);
                    plan.kept_reasons
                        .insert(*target, "registered by the user".to_string());
                    plan.registrations.insert(
                        *target,
                        crate::managed_view::RouteRegistration::RegisteredByUser {
                            uninstall_command: uninstall_command.clone(),
                        },
                    );
                    plan.notes.push(format!(
                        "claude: ccync@ccync is registered by the user, so its combined entry and link stay and the registration file is unchanged. To remove the registration, run `{uninstall_command}`, then rerun `ccync sync`."
                    ));
                }
                ClaudeRegistration::Unreadable { reason } => {
                    plan.blocked.insert(*target);
                    plan.kept_reasons
                        .insert(*target, "registration file could not be read".to_string());
                    plan.notes.push(format!(
                        "claude: the registration file could not be read ({reason}), so the combined ccync surface stays. Fix the file, then rerun `ccync sync`."
                    ));
                }
            },
            ProjectionTargetId::Codex => {
                // The manual removal hint is printed when the combined copy is
                // actually removed, in `finish_combined_retirement`.
                plan.codex = true;
            }
            _ => {}
        }
    }
    plan_skill_copies(staged, &mut plan);
    plan
}

/// Private skill root for each target. Mirrors the skill rows of
/// `delivery::native_root`, which the delivery module does not export.
fn native_skill_root(target: ProjectionTargetId) -> &'static str {
    match target {
        ProjectionTargetId::Claude => "~/.claude/skills",
        ProjectionTargetId::Codex => "~/.agents/skills",
        ProjectionTargetId::Copilot => "~/.copilot/skills",
        ProjectionTargetId::Opencode => "~/.config/opencode/skills",
        ProjectionTargetId::AgyCli => "~/.gemini/antigravity-cli/skills",
        ProjectionTargetId::AgyIde | ProjectionTargetId::AgyGui => "~/.gemini/config/skills",
    }
}

/// Physical path of a `~/`-relative skill root, computed the same way the
/// direct publication computes its root.
fn physical_skill_root(home: &Path, root: &str) -> PathBuf {
    home.join(
        root.trim_start_matches("~/")
            .replace('/', std::path::MAIN_SEPARATOR_STR),
    )
}

/// The combined copy that stays as a target's fallback under `home`. It is the
/// `ccync` copy in the target's native skill root, the same path that
/// retirement keeps for a target whose direct publication did not replace it.
pub(crate) fn fallback_copy_path(home: &Path, target: ProjectionTargetId) -> PathBuf {
    physical_skill_root(home, native_skill_root(target)).join(COMBINED_COPY_NAME)
}

/// Group skill roots by physical path and plan one combined-copy removal and
/// one prune per root. The roots are the route plan's skill roots plus each
/// target's private root, so a target served by a shared root still has its
/// private combined copy planned.
///
/// A shared discovery root whose planned direct copies were all written has
/// its combined copy retired regardless of a failure on another target: that
/// target keeps its fallback on its own private roots. The Claude link is the
/// exception, because it is part of the Claude combined entry and follows
/// Claude's own readiness. Every other combined copy is removed only when
/// every target it serves is ready to retire.
fn plan_skill_copies(staged: &StagedProviderSurfaces, plan: &mut CombinedRetirement) {
    #[derive(Default)]
    struct RootUse {
        targets: BTreeSet<ProjectionTargetId>,
        keep: BTreeSet<String>,
        /// At least one route writes a direct copy into this root.
        routed: bool,
        /// A route into this root reported that its copy was not written.
        unpublished: bool,
    }
    let Some(home) = crate::paths::user_home() else {
        plan.notes.push(
            "combined ccync copies: the home directory is unavailable, so they stay in place."
                .to_string(),
        );
        return;
    };
    let mut roots: std::collections::BTreeMap<String, RootUse> = std::collections::BTreeMap::new();
    if let Some(routes) = &staged.route_plan {
        for route in &routes.routes {
            if route.identity.kind != ccync_foundation::provider::ComponentKind::Skill
                && !route.command_to_skill
            {
                continue;
            }
            let (crate::delivery::ComponentRouteChoice::Direct { root }
            | crate::delivery::ComponentRouteChoice::ServedByRoot { root }) = &route.choice
            else {
                continue;
            };
            let usage = roots.entry(root.clone()).or_default();
            usage.targets.insert(route.target);
            usage.routed = true;
            usage.unpublished |= route.publication_failure.is_some();
            usage.keep.insert(
                route
                    .alias
                    .clone()
                    .unwrap_or_else(|| route.identity.component_id.clone()),
            );
        }
    }
    for target in &plan.cutover_targets {
        roots
            .entry(native_skill_root(*target).to_string())
            .or_default()
            .targets
            .insert(*target);
    }
    let claude_link_root = native_skill_root(ProjectionTargetId::Claude);
    for (root, usage) in roots {
        let shared = crate::delivery::SHARED_ROOT_CONSUMERS
            .iter()
            .any(|row| row.root == root);
        let written = shared && usage.routed && !usage.unpublished;
        let any_failed = usage
            .targets
            .iter()
            .any(|target| staged.failed_targets.contains(target));
        let wait_on = if written {
            usage
                .targets
                .iter()
                .copied()
                .filter(|target| root == claude_link_root && *target == ProjectionTargetId::Claude)
                .collect()
        } else {
            usage.targets.iter().copied().collect()
        };
        let path = physical_skill_root(&home, &root);
        plan.copies.push(CombinedCopy {
            path: path.join(COMBINED_COPY_NAME),
            targets: usage.targets.iter().copied().collect(),
            wait_on,
        });
        // A private root of a failed target keeps every copy as its fallback.
        if written || !any_failed {
            plan.written_roots.push(WrittenSkillRoot {
                path,
                keep: usage.keep,
            });
        }
    }
}

/// Whether `path` is a link whose target lies inside `canonical_root`. This is
/// the same proof `ClaudeSkillProjection::remove` uses for the Claude link.
fn links_into_canonical_root(path: &Path, canonical_root: &Path) -> bool {
    match (fs::canonicalize(path), fs::canonicalize(canonical_root)) {
        (Ok(target), Ok(root)) => target.starts_with(root),
        _ => false,
    }
}

/// Remove one ccync-owned entry: a link, a directory, or a file. The lockfile
/// row is forgotten after the entry is gone, so a row never outlives its entry.
fn remove_owned_entry(
    path: &Path,
    ownership: Option<&projection::AgyRootOwnership>,
) -> Result<(), String> {
    if ccync_foundation::platform::is_symlink_or_junction(path) {
        ccync_foundation::platform::remove_dir_link(path).map_err(|e| e.to_string())?;
    } else if path.is_dir() {
        fs::remove_dir_all(path).map_err(|e| e.to_string())?;
    } else {
        fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    match ownership {
        Some(owner) => owner.forget_now(path, false).map_err(|e| e.to_string()),
        None => Ok(()),
    }
}

/// Remove one combined copy. A link goes only when it points into the
/// canonical root (Claude uses its own `ClaudeSkillProjection::remove`). A real
/// directory goes only when the lockfile records it as ccync-owned. Anything
/// else at that path is a user's entry and stays.
fn remove_combined_copy(
    path: &Path,
    canonical_root: &Path,
    ownership: Option<&projection::AgyRootOwnership>,
) -> Result<(), String> {
    if let Ok(claude) = ClaudeSkillProjection::new(canonical_root.to_path_buf()) {
        if claude.skill_path() == path {
            return claude.remove().map_err(|e| e.to_string());
        }
    }
    let owned = ownership.is_some_and(|owner| owner.is_owned(path));
    if ccync_foundation::platform::is_symlink_or_junction(path) {
        if links_into_canonical_root(path, canonical_root) || owned {
            return remove_owned_entry(path, ownership);
        }
        return Ok(());
    }
    if path.is_dir() && owned {
        return remove_owned_entry(path, ownership);
    }
    Ok(())
}

/// Prune skill entries this rollout's registry owns but the route plan no
/// longer names. Only entries the lockfile records are removed, and the
/// combined copy is left to `remove_combined_copy`.
fn prune_unplanned_copies(
    root: &WrittenSkillRoot,
    ownership: &projection::AgyRootOwnership,
    composed: &mut projection::ProjectionEvidence,
) {
    let Ok(entries) = fs::read_dir(&root.path) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if name == COMBINED_COPY_NAME || root.keep.contains(&name) || !ownership.is_owned(&path) {
            continue;
        }
        if let Err(reason) = remove_owned_entry(&path, Some(ownership)) {
            composed
                .run_warnings
                .push(format!("pruned copy {}: {reason}", path.display()));
        }
    }
}

/// Targets whose combined plugin root direct publication replaces. Each root
/// is a whole-plugin copy or junction that ccync wrote before direct routes.
const PLUGIN_ROOT_TARGETS: [ProjectionTargetId; 4] = [
    ProjectionTargetId::Copilot,
    ProjectionTargetId::AgyCli,
    ProjectionTargetId::AgyIde,
    ProjectionTargetId::AgyGui,
];

/// Whether direct publication replaced a plugin-root target's combined root
/// this run. A target whose publication failed, or that the journal did not
/// record, keeps its root under its writer as the fallback.
fn plugin_root_retired(plan: &CombinedRetirement, target: ProjectionTargetId) -> bool {
    plan.publication_ran
        && plan.cutover_targets.contains(&target)
        && !plan.blocked.contains(&target)
}

/// The combined plugin root a plugin-root target wrote before direct routes.
fn combined_plugin_root(target: ProjectionTargetId) -> Option<PathBuf> {
    let home = crate::paths::user_home()?;
    match target {
        ProjectionTargetId::Copilot => copilot_projection_root().ok(),
        ProjectionTargetId::AgyCli => Some(
            home.join(".gemini")
                .join("antigravity-cli")
                .join("plugins")
                .join("ccync"),
        ),
        ProjectionTargetId::AgyIde => Some(
            home.join(".gemini")
                .join("antigravity-ide")
                .join("plugins")
                .join("ccync"),
        ),
        ProjectionTargetId::AgyGui => Some(
            home.join(".gemini")
                .join("config")
                .join("plugins")
                .join("ccync"),
        ),
        _ => None,
    }
}

/// The Copilot plugin root that ccync's Copilot state file records, if any.
fn recorded_copilot_projection_root() -> Option<PathBuf> {
    let text = fs::read_to_string(target_state_path("copilot").ok()?).ok()?;
    let state: Value = serde_json::from_str(&text).ok()?;
    state
        .get("projectionRoot")
        .and_then(Value::as_str)
        .map(PathBuf::from)
}

/// Clear the projection-root record after the Copilot root it named is gone,
/// so a later directory at that path is not mistaken for ccync's own.
fn clear_copilot_projection_root_record() -> Result<(), String> {
    let state_path = target_state_path("copilot").map_err(|e| e.to_string())?;
    let text = fs::read_to_string(&state_path).map_err(|e| e.to_string())?;
    let mut state: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    state["projectionRoot"] = Value::Null;
    write_json_file(&state_path, &state).map_err(|e| e.to_string())
}

/// Remove the combined plugin roots that direct publication replaced, and the
/// roots of plugin-root targets no longer selected. Only ccync-owned roots are
/// removed: an AGY root must be recorded in the lockfile, and the Copilot root
/// must equal the projection root its state file records. An unowned entry
/// stays and is reported.
///
/// A selected target whose publication failed keeps its root under its writer
/// as the fallback. A selected target whose publication succeeded loses its
/// root here, including the AGY GUI face. Returns the reason each kept root
/// stays, keyed by target, for the route labels.
fn retire_plugin_roots(
    plan: &CombinedRetirement,
    selected: &BTreeSet<ProjectionTargetId>,
    composed: &mut projection::ProjectionEvidence,
) -> BTreeMap<ProjectionTargetId, String> {
    let mut kept = BTreeMap::new();
    let fallback_owner;
    let owner = match plan.ownership.as_ref() {
        Some(owner) => Some(owner),
        None => {
            fallback_owner = provider_lock_path()
                .map(|lockfile| projection::AgyRootOwnership::from_lockfile(&lockfile));
            fallback_owner.as_ref()
        }
    };
    for target in PLUGIN_ROOT_TARGETS {
        if selected.contains(&target) && !plugin_root_retired(plan, target) {
            continue;
        }
        let Some(path) = combined_plugin_root(target) else {
            continue;
        };
        if path.symlink_metadata().is_err() {
            continue;
        }
        let is_copilot = target == ProjectionTargetId::Copilot;
        let owned = if is_copilot {
            recorded_copilot_projection_root().is_some_and(|root| root == path)
        } else {
            owner.is_some_and(|owner| owner.is_owned(&path))
        };
        if !owned {
            composed.run_warnings.push(format!(
                "{target}: the combined plugin root {} is not ccync-owned, so it stays.",
                path.display()
            ));
            kept.insert(
                target,
                "its combined plugin root is not ccync-owned".to_string(),
            );
            continue;
        }
        let ownership = if is_copilot { None } else { owner };
        match remove_owned_entry(&path, ownership) {
            Ok(()) if is_copilot => {
                if let Err(reason) = clear_copilot_projection_root_record() {
                    composed
                        .run_warnings
                        .push(format!("copilot state: {reason}"));
                }
            }
            Ok(()) => {}
            Err(reason) => {
                composed
                    .run_warnings
                    .push(format!("combined plugin root {}: {reason}", path.display()));
                kept.insert(
                    target,
                    "its combined plugin root could not be removed".to_string(),
                );
            }
        }
    }
    kept
}

/// Finish retirement after the legacy chain. Each combined copy is removed
/// once every target it serves is ready. The cutover fields are cleared only
/// when every journaled target is retired. Otherwise the output states the
/// coexistence window and the rerun.
fn finish_combined_retirement(
    plan: &CombinedRetirement,
    canonical_root: &Path,
    lifecycle: &projection::ProjectionEvidence,
    plugin_kept: &BTreeMap<ProjectionTargetId, String>,
    composed: &mut projection::ProjectionEvidence,
) -> BTreeMap<ProjectionTargetId, RouteRetirement> {
    let mut labels = BTreeMap::new();
    if !plan.publication_ran {
        return labels;
    }
    let chain_ok = |target: ProjectionTargetId| {
        lifecycle.completed_targets.contains(&target)
            && lifecycle
                .by_target
                .get(&target)
                .is_none_or(|evidence| evidence.failure.is_none())
    };
    // Claude and Codex also retire through the legacy chain, so they wait for it.
    let chain_ready = |target: &ProjectionTargetId| match target {
        ProjectionTargetId::Claude => plan.claude && chain_ok(*target),
        ProjectionTargetId::Codex => plan.codex && chain_ok(*target),
        _ => true,
    };
    let ready = |target: &ProjectionTargetId| !plan.blocked.contains(target) && chain_ready(target);
    let ownership = plan.ownership.as_ref();

    let mut unretired: BTreeSet<ProjectionTargetId> = BTreeSet::new();
    // Copy-level reasons for targets whose combined copy was kept.
    let mut copy_kept: BTreeMap<ProjectionTargetId, &str> = BTreeMap::new();
    for copy in &plan.copies {
        if !copy.wait_on.iter().all(ready) {
            unretired.extend(copy.targets.iter().copied());
            for target in &copy.targets {
                copy_kept
                    .entry(*target)
                    .or_insert("its combined copy is shared with a target that keeps it");
            }
            continue;
        }
        let existed = copy.path.symlink_metadata().is_ok();
        match remove_combined_copy(&copy.path, canonical_root, ownership) {
            Err(reason) => {
                composed.run_warnings.push(format!(
                    "combined ccync copy {}: {reason}",
                    copy.path.display()
                ));
                unretired.extend(copy.targets.iter().copied());
                for target in &copy.targets {
                    copy_kept
                        .entry(*target)
                        .or_insert("its combined copy could not be removed");
                }
            }
            Ok(()) => {
                // Print the Codex manual removal hint only on the sync that
                // retires the Codex combined copy, not on every sync.
                let codex_retired = copy.targets.contains(&ProjectionTargetId::Codex)
                    && existed
                    && copy.path.symlink_metadata().is_err();
                if codex_retired {
                    composed.run_warnings.push(format!(
                        "codex: the combined ccync surface was retired. Registration is not checked. If the combined plugin was installed, remove it with `{CODEX_MANUAL_REMOVAL_HINT}`."
                    ));
                }
            }
        }
    }
    if let Some(owner) = ownership {
        for root in &plan.written_roots {
            prune_unplanned_copies(root, owner, composed);
        }
    }

    // Label each cutover target before the pending check, so a partial run
    // reports the targets it retired next to the ones it kept.
    for target in &plan.cutover_targets {
        let label = if !ready(target) {
            let reason = plan
                .kept_reasons
                .get(target)
                .map_or("legacy retirement did not finish", String::as_str);
            RouteRetirement::Kept {
                reason: reason.to_string(),
            }
        } else if let Some(reason) = copy_kept.get(target) {
            RouteRetirement::Kept {
                reason: (*reason).to_string(),
            }
        } else if let Some(reason) = plugin_kept.get(target) {
            RouteRetirement::Kept {
                reason: reason.clone(),
            }
        } else {
            RouteRetirement::Done
        };
        labels.insert(*target, label);
    }

    let pending: Vec<&str> = plan
        .cutover_targets
        .iter()
        .filter(|target| {
            unretired.contains(target) || !ready(target) || plugin_kept.contains_key(target)
        })
        .map(|target| target.as_str())
        .collect();
    if pending.is_empty() {
        if let Err(error) = clear_provider_cutover() {
            composed
                .run_warnings
                .push(format!("provider cutover journal: {error}"));
        }
        return labels;
    }
    composed.run_warnings.push(format!(
        "old and new copies may coexist until the next sync for {}. Rerun `ccync sync` before restarting agents.",
        pending.join(", ")
    ));
    labels
}

/// Clear the cutover fields after retirement finished for every target.
fn clear_provider_cutover() -> Result<(), InstallError> {
    let path = provider_journal_path().ok_or(InstallError::NoHome)?;
    let mut journal = match fs::read_to_string(&path) {
        Ok(text) => ProviderLifecycleJournal::from_json_str(&text)
            .map_err(|error| InstallError::Io(std::io::Error::other(error.to_string())))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(InstallError::Io(error)),
    };
    journal.cutover_targets = None;
    journal.combined_entries_and_link_to_retire = None;
    journal.redundant_managed_copy_paths = None;
    let bytes = journal
        .to_json_string()
        .map_err(|error| InstallError::Io(std::io::Error::other(error.to_string())))?;
    ccync_foundation::platform::atomic_write_bytes(&path, bytes.as_bytes())
        .map_err(|error| InstallError::Io(std::io::Error::other(error.to_string())))
}

fn record_provider_cutover(targets: &[ProjectionTargetId]) -> Result<(), InstallError> {
    use ccync_foundation::provider::{PathRoot, RelativePath, ScopedPath};
    let path = provider_journal_path().ok_or(InstallError::NoHome)?;
    let mut journal = match fs::read_to_string(&path) {
        Ok(text) => ProviderLifecycleJournal::from_json_str(&text)
            .map_err(|error| InstallError::Io(std::io::Error::other(error.to_string())))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ProviderLifecycleJournal::default()
        }
        Err(error) => return Err(InstallError::Io(error)),
    };
    let scoped = |path: &str| -> Result<ScopedPath, InstallError> {
        Ok(ScopedPath {
            root: PathRoot::UserHome,
            path: RelativePath::new(path)
                .map_err(|error| InstallError::Io(std::io::Error::other(error.to_string())))?,
        })
    };
    journal.cutover_targets = Some(targets.to_vec());
    journal.combined_entries_and_link_to_retire = Some(vec![
        scoped(".ccync/plugins")?,
        scoped(".claude/skills/ccync")?,
    ]);
    journal.redundant_managed_copy_paths = Some(vec![
        scoped(".claude/skills/ccync")?,
        scoped(".agents/skills/ccync")?,
        scoped(".copilot/skills/ccync")?,
        scoped(".config/opencode/skills/ccync")?,
        scoped(".gemini/antigravity-cli/skills/ccync")?,
        scoped(".gemini/config/skills/ccync")?,
    ]);
    let bytes = journal
        .to_json_string()
        .map_err(|error| InstallError::Io(std::io::Error::other(error.to_string())))?;
    ccync_foundation::platform::atomic_write_bytes(&path, bytes.as_bytes())
        .map_err(|error| InstallError::Io(std::io::Error::other(error.to_string())))
}

impl StagedProviderSurfaces {
    fn is_selected(&self, target: ProjectionTargetId) -> bool {
        self.selected.iter().any(|name| name == target.as_str())
    }
}

/// Stage provider generations and supported direct surfaces while the
/// caller-held lifecycle proof is alive.
fn stage_provider_surfaces(
    lock: &crate::tool_manager::lock::HeldLifecycleLock,
    runner: &dyn crate::plugin_native::NativeCommandRunner,
) -> StagedProviderSurfaces {
    let mut staged = StagedProviderSurfaces {
        evidence: projection::ProjectionEvidence::default(),
        selected: load_runtime_selection().selected_runtimes,
        cutover_targets: Vec::new(),
        publication_ran: false,
        failed_targets: std::collections::BTreeSet::new(),
        route_plan: None,
        prior_ownership: None,
    };
    reconcile_native_intents(lock, runner, &mut staged.evidence.run_warnings);
    let inventory = match crate::plugin_render::load_provider_inventory() {
        Ok(inventory) => inventory,
        Err(error) => {
            staged
                .evidence
                .run_warnings
                .push(format!("provider inventory: {error}"));
            return staged;
        }
    };
    let rendered = match crate::plugin_render::render_provider_generations(&inventory) {
        Ok(rendered) => rendered,
        Err(error) => {
            staged
                .evidence
                .run_warnings
                .push(format!("provider staging: {error}"));
            return staged;
        }
    };
    let targets = ccync_foundation::target::CANONICAL_PROJECTION_TARGETS
        .iter()
        .copied()
        .filter(|target| staged.is_selected(*target))
        .collect::<Vec<_>>();
    let cutover_recorded = match record_provider_cutover(&targets) {
        Ok(()) => true,
        Err(error) => {
            staged
                .evidence
                .run_warnings
                .push(format!("provider cutover journal: {error}"));
            false
        }
    };
    staged.cutover_targets = targets.clone();
    if cutover_recorded {
        if let (Some(home), Some(lockfile)) = (crate::paths::user_home(), provider_lock_path()) {
            // Publication rewrites the lockfile, so snapshot ownership first.
            staged.prior_ownership = Some(projection::AgyRootOwnership::from_lockfile(&lockfile));
            // The exact direct rows are read before publication. Publication
            // upserts them, so a row for a copy the plan no longer names
            // survives it and is pruned below.
            let prior_records = projection::direct_records(&lockfile);
            let (direct, route_plan, failed) = crate::delivery_apply::publish_with_native(
                &rendered, &targets, &home, &lockfile, runner,
            );
            // A failed route is still planned, so its copy is kept.
            let mut planned = planned_direct_copies(&route_plan, &home);
            if let Ok(records) = projection::standalone_publication_records(&lockfile) {
                planned.extend(records.iter().map(|record| path_key(&record.path)));
            }
            if let Ok(journal) = read_standalone_journal() {
                let destinations = journal
                    .standalone_intents
                    .iter()
                    .map(|intent| &intent.destination)
                    .chain(
                        journal
                            .standalone_withdrawal_intents
                            .iter()
                            .map(|intent| &intent.destination),
                    )
                    .chain(
                        journal
                            .intents
                            .iter()
                            .flat_map(|intent| intent.scoped_paths.iter()),
                    );
                planned.extend(destinations.filter_map(|destination| {
                    destination
                        .root
                        .resolve()
                        .map(|root| path_key(&destination.path.join_to(&root)))
                }));
            }
            prune_unplanned_direct_records(
                &lockfile,
                &prior_records,
                |record| {
                    let key = path_key(&record.path);
                    is_planned_direct_copy(&record.path, &planned)
                        || planned
                            .iter()
                            .any(|reserved| Path::new(reserved).starts_with(Path::new(&key)))
                },
                &mut staged.evidence.run_warnings,
            );
            merge_projection_evidence(&mut staged.evidence, direct);
            staged.failed_targets = failed;
            staged.route_plan = Some(route_plan);
            staged.publication_ran = true;
        } else {
            staged
                .evidence
                .run_warnings
                .push("provider surfaces: home or lockfile path is unavailable".into());
        }
    }
    for provider in &rendered {
        if provider
            .components
            .iter()
            .any(|component| component.kind == ccync_foundation::provider::ComponentKind::Hook)
        {
            for target in &targets {
                staged.evidence.run_warnings.push(format!(
                    "{target} provider {} hooks: unsupported",
                    provider.plugin_id
                ));
            }
        }
    }
    staged
}

#[derive(Debug, Clone)]
struct RuntimeSelection {
    selected_runtimes: Vec<String>,
}

/// Absent-state default selected runtimes, in the legacy preference order:
/// `claude, codex, copilot, opencode, agy-cli, agy-ide, agy-gui`.
///
/// This is deliberately NOT sourced from
/// `ccync_foundation::target::CANONICAL_PROJECTION_TARGETS` — that table's
/// order does not match the legacy order this crate has always used.
/// `RuntimeSelection::selected_runtimes` falls back to this order, so the
/// exact legacy sequence must be preserved here rather than re-derived from
/// the canonical target order.
const DEFAULT_RUNTIME_ORDER: [ProjectionTargetId; 7] = [
    ProjectionTargetId::Claude,
    ProjectionTargetId::Codex,
    ProjectionTargetId::Copilot,
    ProjectionTargetId::Opencode,
    ProjectionTargetId::AgyCli,
    ProjectionTargetId::AgyIde,
    ProjectionTargetId::AgyGui,
];

fn default_selected_runtimes() -> Vec<String> {
    DEFAULT_RUNTIME_ORDER
        .iter()
        .map(|target| target.as_str().to_string())
        .collect()
}

fn load_runtime_selection() -> RuntimeSelection {
    resolve_runtime_selection(CcyncConfig::load())
}

/// The stale-key warning shown when both `selectedProjectionTargets` and
/// `selectedRuntimes` are present in `config.json`. Returns `None` for every
/// other [`LegacyState`] — split out as a pure function so the gating logic
/// is unit-testable without capturing the process's actual stderr.
fn both_state_warning(state: LegacyState) -> Option<&'static str> {
    if state == LegacyState::Both {
        Some(
            "ccync: both `selectedProjectionTargets` and `selectedRuntimes` are set in ~/.ccync/config.json — using `selectedProjectionTargets`",
        )
    } else {
        None
    }
}

/// Validate and resolve a `CcyncConfig`'s selected-target keys into a
/// `RuntimeSelection`. Pure function (no I/O) so it is directly
/// unit-testable without a filesystem/env seam.
///
/// Delegates set resolution to `CcyncConfig::resolve_projection_targets`,
/// the shared foundation resolver, so the canonical `selectedProjectionTargets`
/// key and the legacy `selectedRuntimes` key are reconciled the same way as
/// every other projection consumer (mirrors
/// `projection::selected_projection_targets_from_config`). Entries that were
/// present but did not parse as a known target come back via
/// `resolution.rejected` and produce the same aggregated stderr warning this
/// function has always printed. If a key was present but every entry was
/// blank or invalid, the resolved set — and `selected_runtimes` — stays
/// empty; a genuinely absent config still resolves to the full default
/// runtime set in `DEFAULT_RUNTIME_ORDER`.
///
/// When both the canonical and legacy keys are present on disk
/// (`LegacyState::Both`), the canonical key's values win (same rule the
/// foundation resolver itself documents) and [`both_state_warning`] prints a
/// dedicated warning so a user relying on a stale `selectedRuntimes` key
/// notices it is being ignored — this state was previously unobservable here
/// (only `selectedRuntimes` was ever read), so there was no prior wording to
/// match.
fn resolve_runtime_selection(config: CcyncConfig) -> RuntimeSelection {
    let default_selected = default_selected_runtimes();
    let resolution = config.resolve_projection_targets();

    if let Some(warning) = both_state_warning(resolution.state) {
        eprintln!("{warning}");
    }

    if !resolution.rejected.is_empty() {
        eprintln!(
            "ccync: ignoring invalid runtime key(s) in ~/.ccync/config.json `selectedRuntimes`: {} — valid: {}",
            resolution.rejected.join(", "),
            default_selected.join(", ")
        );
    }

    let selected_runtimes = if resolution.targets.is_empty() {
        // A genuinely absent config (`LegacyState::Absent`) selecting the
        // full default set is expected, ordinary behavior, not a fallback
        // worth flagging. Only warn — and leave the selection empty — when a
        // key was actually present but resolved to nothing usable (all-blank
        // / all-invalid entries).
        if resolution.state == LegacyState::Absent {
            default_selected.clone()
        } else {
            let source_key = if resolution.state == LegacyState::Legacy {
                "selectedRuntimes"
            } else {
                "selectedProjectionTargets"
            };
            eprintln!("ccync: no valid runtime keys in `{source_key}` — nothing selected");
            Vec::new()
        }
    } else {
        resolution
            .targets
            .iter()
            .map(|target| target.as_str().to_string())
            .collect()
    };

    RuntimeSelection { selected_runtimes }
}

/// Resolves each `selection.selected_runtimes` entry through
/// `ProjectionTargetId::FromStr`, keeping AGY's three faces
/// (`AgyCli`/`AgyIde`/`AgyGui`) distinct instead of collapsing them into one
/// bucket string — typed IDs are already granular, so no collapsing step is
/// needed. Callers gate on specific `ProjectionTargetId` membership
/// (`Claude`, `Copilot`, `Codex`, `AgyCli`/`AgyIde`/`AgyGui`); `Opencode`
/// is never checked for membership at either call site, so it is naturally
/// excluded from what callers look for.
///
/// A `selected_runtimes` entry that doesn't parse to a known
/// `ProjectionTargetId` is skipped rather than treated as an error —
/// `RuntimeSelection` is only ever built from already-validated runtime
/// names, so this is defensive, not a real path.
fn primary_targets(selection: &RuntimeSelection) -> std::collections::BTreeSet<ProjectionTargetId> {
    selection
        .selected_runtimes
        .iter()
        .filter_map(|runtime| ProjectionTargetId::from_str(runtime).ok())
        .collect()
}

/// `true` when any of the three AGY faces (`AgyCli`/`AgyIde`/`AgyGui`) is
/// present in `targets` — fires whenever *any* of `agy-cli`/`agy-ide`/`agy-gui`
/// is selected. Typed targets never collapse into one bucket string, so this
/// checks all three explicitly.
fn any_agy_face_selected(targets: &std::collections::BTreeSet<ProjectionTargetId>) -> bool {
    targets.contains(&ProjectionTargetId::AgyCli)
        || targets.contains(&ProjectionTargetId::AgyIde)
        || targets.contains(&ProjectionTargetId::AgyGui)
}

fn ensure_parent_dir(path: &Path) -> Result<(), InstallError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn ccync_dist_targets_root() -> Result<PathBuf, InstallError> {
    Ok(effective_layout()?.dist_path().join("targets"))
}

/// Pre-rename dist root (`~/.ccync/dist/providers`). Retained only as a best-effort
/// cleanup target so an update/uninstall migrates installs created before the
/// `providers`→`runtimes` path rename.
fn stale_dist_providers_root() -> Result<PathBuf, InstallError> {
    Ok(effective_layout()?.dist_path().join("providers"))
}

/// Pre-rename dist root (`~/.ccync/dist/runtimes`). Retained only as a best-effort
/// cleanup target so an update/uninstall migrates installs created before the
/// `runtimes`→`targets` path rename, same precedent as
/// [`stale_dist_providers_root`].
fn stale_dist_runtimes_root() -> Result<PathBuf, InstallError> {
    Ok(effective_layout()?.dist_path().join("runtimes"))
}

/// Pre-removal ledger path (`~/.ccync/ledger.json`). Retained only as a
/// best-effort cleanup target so an update migrates installs that still
/// carry the retired ledger file, same precedent as
/// [`stale_dist_runtimes_root`].
fn stale_ledger_path() -> Result<PathBuf, InstallError> {
    ccync_foundation::paths::user_home()
        .map(|path| path.join(".ccync").join("ledger.json"))
        .ok_or(InstallError::NoHome)
}

fn target_state_path(target: &str) -> Result<PathBuf, InstallError> {
    Ok(ccync_dist_targets_root()?.join(target).join("managed.json"))
}

fn write_json_file(path: &Path, value: &Value) -> Result<(), InstallError> {
    ensure_parent_dir(path)?;
    fs::write(path, serde_json::to_string_pretty(value)?)?;
    Ok(())
}

fn plugins_lockfile_path() -> Option<PathBuf> {
    provider_lock_path()
}

/// Returns `(plugin_id, resolved_source)` pairs for enabled bundled-local plugins
/// from the lockfile. Returns empty vec when the lockfile is absent or unreadable.
fn load_enabled_bundles() -> Vec<(String, String)> {
    let path = match plugins_lockfile_path() {
        Some(p) => p,
        None => return vec![],
    };
    let contents = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let lock: Value = match serde_json::from_str(&contents) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    lock.get("resolvedPlugins")
        .and_then(Value::as_array)
        .map(|plugins| {
            plugins
                .iter()
                .filter(|p| {
                    p.get("installStrategy").and_then(Value::as_str) == Some("bundled-local")
                })
                .filter_map(|p| {
                    let id = p.get("pluginId").and_then(Value::as_str)?.to_owned();
                    let src = p.get("resolvedSource").and_then(Value::as_str)?.to_owned();
                    Some((id, src))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Returns `(plugin_id, resolved_source)` pairs for enabled personal plugins
/// from the lockfile `_personalPlugins` namespace.
/// Returns empty vec when the lockfile is absent, unreadable, or has no personal plugins.
/// Does not touch the `resolvedPlugins` public namespace — the two namespaces are disjoint.
pub(crate) fn load_enabled_personal_plugins() -> Vec<(String, String)> {
    let path = match plugins_lockfile_path() {
        Some(p) => p,
        None => return vec![],
    };
    let contents = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let lock: Value = match serde_json::from_str(&contents) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    lock.get("_personalPlugins")
        .and_then(Value::as_array)
        .map(|plugins| {
            plugins
                .iter()
                .filter_map(|p| {
                    let id = p.get("pluginId").and_then(Value::as_str)?.to_owned();
                    let src = p.get("source").and_then(Value::as_str)?.to_owned();
                    Some((id, src))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// True when `source` is shaped like a remote URL (always fetched via `git
/// clone`, never routed to the local directory-snapshot fetcher).
pub fn is_remote_url(source: &str) -> bool {
    source.starts_with("http://")
        || source.starts_with("https://")
        || source.starts_with("git@")
        || source.starts_with("git://")
}

/// Split a remote git source from an optional repository subpath.
pub fn split_git_subpath(source: &str) -> (&str, Option<&str>) {
    if !is_remote_url(source) {
        return (source, None);
    }

    let authority_end = if let Some(scheme_end) = source.find("://") {
        source[scheme_end + 3..]
            .find('/')
            .map(|offset| scheme_end + 3 + offset)
            .unwrap_or(source.len())
    } else {
        source.find(':').unwrap_or(source.len())
    };

    let Some(separator_offset) = source[authority_end..].find("//") else {
        return (source, None);
    };
    let separator = authority_end + separator_offset;
    (&source[..separator], Some(&source[separator + 2..]))
}

/// Validate a repository subpath without accessing the filesystem.
pub fn check_subpath_lexical(subpath: &str) -> Result<(), String> {
    if subpath.is_empty() {
        return Err(format!("invalid subpath '{subpath}': it must not be empty"));
    }
    if subpath.contains('\\') {
        return Err(format!(
            "invalid subpath '{subpath}': backslashes are not allowed"
        ));
    }
    let bytes = subpath.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err(format!(
            "invalid subpath '{subpath}': drive-prefixed paths are not allowed"
        ));
    }
    if subpath
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(format!(
            "invalid subpath '{subpath}': empty, '.' and '..' segments are not allowed"
        ));
    }
    if !is_contained_relative(Path::new(subpath)) {
        return Err(format!(
            "invalid subpath '{subpath}': path must remain within the repository"
        ));
    }
    Ok(())
}

/// Read and parse a personal catalog JSON file. Returns `None` if unreadable or invalid JSON.
pub fn read_personal_catalog(path: &Path) -> Option<serde_json::Value> {
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Write JSON atomically (temp sibling + rename).
pub fn write_json_atomic(path: &Path, value: &serde_json::Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(
        &tmp,
        serde_json::to_string_pretty(value).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("write tmp: {e}"))?;
    fs::rename(&tmp, path).map_err(|e| format!("rename: {e}"))
}

/// Fetch result for a single personal plugin clone.
#[derive(Debug, PartialEq)]
pub enum PersonalFetchResult {
    /// Clone succeeded; `path` is the cache dir, `sha` is the pinned HEAD commit.
    Cloned { path: PathBuf, sha: String },
    /// A `<plugin_id>@*` dir already exists in cache — no-op (idempotent).
    AlreadyPresent(PathBuf),
    /// `~/.ccync/cache/` root could not be resolved (no home dir).
    NoCacheRoot,
    /// `git clone` or sha-read invocation failed; message contains stderr.
    CloneFailed(String),
}

/// Validate that `plugin_id` is safe to use as a path component and catalog identifier.
///
/// Rejects empty values, values containing path separators (`/`, `\`), traversal (`..`),
/// colons (`:`), absolute paths, drive prefixes, and non-normal path components.
pub fn is_valid_plugin_id(plugin_id: &str) -> bool {
    if plugin_id.is_empty() {
        return false;
    }
    if plugin_id.contains('/')
        || plugin_id.contains('\\')
        || plugin_id.contains("..")
        || plugin_id.contains(':')
        || plugin_id.contains('\0')
    {
        return false;
    }
    if plugin_id.chars().all(|c| c == '.') {
        return false;
    }
    let path = Path::new(plugin_id);
    if path.is_absolute() || !is_contained_relative(path) {
        return false;
    }
    let mut components = path.components();
    let Some(std::path::Component::Normal(only)) = components.next() else {
        return false;
    };
    components.next().is_none() && only.to_str() == Some(plugin_id)
}

/// Clone a personal plugin from `source_url` into
/// `~/.ccync/cache/<plugin_id>@<sha>/` preserving full CC-plugin structure.
///
/// Idempotent: if any `<plugin_id>@*` directory already exists in `cache_root`,
/// returns `AlreadyPresent` without re-cloning.
///
/// Flow:
/// 1. Scan `cache_root` for an existing `<plugin_id>@*` dir → AlreadyPresent.
/// 2. `git clone --depth 1 <source_url> <tmp>` inside cache root.
/// 3. `git rev-parse HEAD` in the tmp clone to get the pinned sha.
/// 4. Rename `<tmp>` → `<plugin_id>@<sha>`.
pub fn fetch_personal_plugin(
    cache_root: &Path,
    plugin_id: &str,
    source_url: &str,
) -> PersonalFetchResult {
    fetch_personal_plugin_at(cache_root, plugin_id, source_url, None)
}

/// Clone a personal plugin from `source_url` into
/// `~/.ccync/cache/<plugin_id>@<sha>/` preserving full CC-plugin structure.
///
/// Idempotent: if any `<plugin_id>@*` directory already exists in `cache_root`,
/// returns `AlreadyPresent` without re-cloning.
///
/// When `target_revision` is `Some(sha)`, fetches that specific commit.
pub fn fetch_personal_plugin_at(
    cache_root: &Path,
    plugin_id: &str,
    source_url: &str,
    target_revision: Option<&str>,
) -> PersonalFetchResult {
    let (repo_url, subpath) = split_git_subpath(source_url);
    materialize_plugin(cache_root, plugin_id, repo_url, subpath, target_revision)
}

fn promote_subpath(
    staging: &Path,
    subpath: &str,
    target: &Path,
    plugin_id: &str,
) -> PersonalFetchResult {
    let fail = |detail: String| {
        let _ = fs::remove_dir_all(staging);
        PersonalFetchResult::CloneFailed(format!(
            "plugin '{plugin_id}', subpath '{subpath}': {detail}"
        ))
    };
    let canonical_staging = match fs::canonicalize(staging) {
        Ok(path) => path,
        Err(error) => return fail(format!("canonical staging checkout failed: {error}")),
    };
    let selected = staging.join(subpath);
    match fs::symlink_metadata(&selected) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return fail(
                "selected subpath is a symlink; symlink selectors are not allowed".to_string(),
            );
        }
        Ok(_) => {}
        Err(error) => return fail(format!("subpath cannot be inspected: {error}")),
    }
    let canonical_subtree = match fs::canonicalize(&selected) {
        Ok(path) => path,
        Err(error) => return fail(format!("subpath cannot be canonicalized: {error}")),
    };
    if !canonical_subtree.starts_with(&canonical_staging) {
        return fail("canonical subpath is outside the staging checkout".to_string());
    }
    if !canonical_subtree.is_dir() {
        return fail("subpath is not a directory".to_string());
    }

    fn check_tree(path: &Path, root: &Path) -> Result<(), String> {
        let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
        if metadata.file_type().is_symlink() {
            let canonical = fs::canonicalize(path)
                .map_err(|error| format!("link {:?} cannot be canonicalized: {error}", path))?;
            if !canonical.starts_with(root) {
                return Err(format!(
                    "link {:?} resolves outside the selected subtree",
                    path
                ));
            }
            return Ok(());
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(path).map_err(|error| error.to_string())? {
                check_tree(&entry.map_err(|error| error.to_string())?.path(), root)?;
            }
        }
        Ok(())
    }

    if let Err(detail) = check_tree(&canonical_subtree, &canonical_subtree) {
        return fail(format!("link validation failed: {detail}"));
    }
    if let Err(error) = fs::rename(&selected, target) {
        return fail(format!("rename selected subpath to target failed: {error}"));
    }
    if let Err(error) = fs::remove_dir_all(staging) {
        return PersonalFetchResult::CloneFailed(format!(
            "plugin '{plugin_id}', subpath '{subpath}': remove staging failed: {error}"
        ));
    }
    PersonalFetchResult::Cloned {
        path: target.to_path_buf(),
        sha: target
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.rsplit_once('@'))
            .map(|(_, sha)| sha.to_string())
            .unwrap_or_default(),
    }
}

/// Materialize a repository or a checked-out repository subpath into the cache.
pub fn materialize_plugin(
    cache_root: &Path,
    plugin_id: &str,
    repo_url: &str,
    subpath: Option<&str>,
    target_revision: Option<&str>,
) -> PersonalFetchResult {
    if let Some(subpath) = subpath {
        if let Err(message) = check_subpath_lexical(subpath) {
            return PersonalFetchResult::CloneFailed(format!("plugin '{plugin_id}': {message}"));
        }
    }
    if !is_valid_plugin_id(plugin_id) {
        return PersonalFetchResult::CloneFailed(format!("invalid plugin id: '{plugin_id}'"));
    }
    if repo_url.starts_with('-') {
        return PersonalFetchResult::CloneFailed(
            "invalid repo_url: cannot start with '-'".to_string(),
        );
    }
    match target_revision {
        None => {
            let prefix = format!("{}@", plugin_id);
            if let Ok(entries) = fs::read_dir(cache_root) {
                for entry in entries.flatten() {
                    if entry.file_name().to_string_lossy().starts_with(&prefix)
                        && !entry
                            .file_name()
                            .to_string_lossy()
                            .ends_with("@__fetching__")
                        && entry.path().is_dir()
                    {
                        return PersonalFetchResult::AlreadyPresent(entry.path());
                    }
                }
            }
            if let Err(e) = fs::create_dir_all(cache_root) {
                return PersonalFetchResult::CloneFailed(format!("create cache root: {e}"));
            }
            let tmp = if subpath.is_some() {
                cache_root.join(format!(".{plugin_id}.staging"))
            } else {
                cache_root.join(format!("{plugin_id}@__fetching__"))
            };
            let _ = fs::remove_dir_all(&tmp);
            let clone_out = std::process::Command::new("git")
                .args([
                    "clone",
                    "--depth",
                    "1",
                    repo_url,
                    tmp.to_str().unwrap_or(""),
                ])
                .output();
            match clone_out {
                Err(e) => return PersonalFetchResult::CloneFailed(format!("spawn git: {e}")),
                Ok(out) if !out.status.success() => {
                    let _ = fs::remove_dir_all(&tmp);
                    return PersonalFetchResult::CloneFailed(
                        String::from_utf8_lossy(&out.stderr).to_string(),
                    );
                }
                Ok(_) => {}
            }
            let sha_out = std::process::Command::new("git")
                .args(["-C", tmp.to_str().unwrap_or(""), "rev-parse", "HEAD"])
                .output();
            let sha = match sha_out {
                Ok(out) if out.status.success() => {
                    String::from_utf8_lossy(&out.stdout).trim().to_string()
                }
                _ => {
                    let _ = fs::remove_dir_all(&tmp);
                    return PersonalFetchResult::CloneFailed("could not read HEAD sha".to_string());
                }
            };
            let target = cache_root.join(format!("{}@{}", plugin_id, sha));
            if target.exists() {
                let _ = fs::remove_dir_all(&tmp);
                return PersonalFetchResult::AlreadyPresent(target);
            }
            if let Some(subpath) = subpath {
                return promote_subpath(&tmp, subpath, &target, plugin_id);
            }
            if let Err(e) = fs::rename(&tmp, &target) {
                let _ = fs::remove_dir_all(&tmp);
                return PersonalFetchResult::CloneFailed(format!("rename to target: {e}"));
            }
            PersonalFetchResult::Cloned { path: target, sha }
        }
        Some(sha) => {
            if sha.is_empty() || sha.starts_with('-') {
                return PersonalFetchResult::CloneFailed(
                    "invalid target_revision: cannot be empty or start with '-'".to_string(),
                );
            }
            let target = cache_root.join(format!("{}@{}", plugin_id, sha));
            if target.is_dir() {
                return PersonalFetchResult::AlreadyPresent(target);
            }
            if let Err(e) = fs::create_dir_all(cache_root) {
                return PersonalFetchResult::CloneFailed(format!("create cache root: {e}"));
            }
            let tmp = if subpath.is_some() {
                cache_root.join(format!(".{plugin_id}.staging"))
            } else {
                cache_root.join(format!("{plugin_id}@__fetching__"))
            };
            let _ = fs::remove_dir_all(&tmp);
            if let Err(e) = fs::create_dir_all(&tmp) {
                return PersonalFetchResult::CloneFailed(format!("create temp dir: {e}"));
            }

            let init_res = std::process::Command::new("git")
                .current_dir(&tmp)
                .arg("init")
                .output();
            match init_res {
                Err(e) => {
                    let _ = fs::remove_dir_all(&tmp);
                    return PersonalFetchResult::CloneFailed(format!("spawn git: {e}"));
                }
                Ok(out) if !out.status.success() => {
                    let _ = fs::remove_dir_all(&tmp);
                    return PersonalFetchResult::CloneFailed(
                        String::from_utf8_lossy(&out.stderr).to_string(),
                    );
                }
                Ok(_) => {}
            }

            let remote_res = std::process::Command::new("git")
                .current_dir(&tmp)
                .args(["remote", "add", "origin", repo_url])
                .output();
            match remote_res {
                Err(e) => {
                    let _ = fs::remove_dir_all(&tmp);
                    return PersonalFetchResult::CloneFailed(format!("spawn git: {e}"));
                }
                Ok(out) if !out.status.success() => {
                    let _ = fs::remove_dir_all(&tmp);
                    return PersonalFetchResult::CloneFailed(
                        String::from_utf8_lossy(&out.stderr).to_string(),
                    );
                }
                Ok(_) => {}
            }

            let fetch_res = std::process::Command::new("git")
                .current_dir(&tmp)
                .args(["fetch", "--depth", "1", "origin", sha])
                .output();

            let fetch_succeeded = match fetch_res {
                Ok(ref out) => out.status.success(),
                Err(e) => {
                    let _ = fs::remove_dir_all(&tmp);
                    return PersonalFetchResult::CloneFailed(format!("spawn git: {e}"));
                }
            };

            if fetch_succeeded {
                let checkout_res = std::process::Command::new("git")
                    .current_dir(&tmp)
                    .args(["checkout", "--detach", "FETCH_HEAD"])
                    .output();
                match checkout_res {
                    Err(e) => {
                        let _ = fs::remove_dir_all(&tmp);
                        return PersonalFetchResult::CloneFailed(format!("spawn git: {e}"));
                    }
                    Ok(out) if !out.status.success() => {
                        let _ = fs::remove_dir_all(&tmp);
                        return PersonalFetchResult::CloneFailed(
                            String::from_utf8_lossy(&out.stderr).to_string(),
                        );
                    }
                    Ok(_) => {}
                }
            } else {
                let _ = fs::remove_dir_all(&tmp);
                let clone_res = std::process::Command::new("git")
                    .args(["clone", repo_url, tmp.to_str().unwrap_or("")])
                    .output();
                match clone_res {
                    Err(e) => return PersonalFetchResult::CloneFailed(format!("spawn git: {e}")),
                    Ok(out) if !out.status.success() => {
                        let _ = fs::remove_dir_all(&tmp);
                        return PersonalFetchResult::CloneFailed(
                            String::from_utf8_lossy(&out.stderr).to_string(),
                        );
                    }
                    Ok(_) => {}
                }
                let checkout_res = std::process::Command::new("git")
                    .current_dir(&tmp)
                    .args(["checkout", "--detach", sha])
                    .output();
                match checkout_res {
                    Err(e) => {
                        let _ = fs::remove_dir_all(&tmp);
                        return PersonalFetchResult::CloneFailed(format!("spawn git: {e}"));
                    }
                    Ok(out) if !out.status.success() => {
                        let _ = fs::remove_dir_all(&tmp);
                        return PersonalFetchResult::CloneFailed(
                            String::from_utf8_lossy(&out.stderr).to_string(),
                        );
                    }
                    Ok(_) => {}
                }
            }

            let sha_out = std::process::Command::new("git")
                .current_dir(&tmp)
                .args(["rev-parse", "HEAD"])
                .output();
            let actual_sha = match sha_out {
                Ok(out) if out.status.success() => {
                    String::from_utf8_lossy(&out.stdout).trim().to_string()
                }
                Ok(out) => {
                    let _ = fs::remove_dir_all(&tmp);
                    return PersonalFetchResult::CloneFailed(
                        String::from_utf8_lossy(&out.stderr).to_string(),
                    );
                }
                Err(e) => {
                    let _ = fs::remove_dir_all(&tmp);
                    return PersonalFetchResult::CloneFailed(format!("spawn git: {e}"));
                }
            };

            if actual_sha != sha {
                let _ = fs::remove_dir_all(&tmp);
                return PersonalFetchResult::CloneFailed(format!(
                    "HEAD sha mismatch: expected {sha}, got {actual_sha}"
                ));
            }

            if target.exists() {
                let _ = fs::remove_dir_all(&tmp);
                return PersonalFetchResult::AlreadyPresent(target);
            }
            if let Some(subpath) = subpath {
                return promote_subpath(&tmp, subpath, &target, plugin_id);
            }
            if let Err(e) = fs::rename(&tmp, &target) {
                let _ = fs::remove_dir_all(&tmp);
                return PersonalFetchResult::CloneFailed(format!("rename to target: {e}"));
            }
            PersonalFetchResult::Cloned {
                path: target,
                sha: sha.to_string(),
            }
        }
    }
}

/// Result of adding a single plugin entry to the personal catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddResult {
    Added {
        path: PathBuf,
        sha: String,
        fetched: bool,
    },
    AlreadyPresent,
    Failed(String),
    WriteFailed(String),
}

/// Add a plugin entry to the personal catalog, fetching if not already cached.
pub fn add_plugin_entry(
    cache_root: &Path,
    catalog_path: &Path,
    plugin_id: &str,
    source: &str,
    pin: Option<&str>,
) -> AddResult {
    if !is_valid_plugin_id(plugin_id) {
        return AddResult::Failed(format!("invalid plugin id: '{plugin_id}'"));
    }

    let mut catalog = read_personal_catalog(catalog_path)
        .unwrap_or_else(|| json!({ "schemaVersion": 1, "plugins": [] }));

    let already_in_catalog = catalog
        .get("plugins")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .any(|e| e.get("pluginId").and_then(Value::as_str) == Some(plugin_id))
        })
        .unwrap_or(false);

    if already_in_catalog {
        return AddResult::AlreadyPresent;
    }

    let fetch_result = fetch_personal_plugin_at(cache_root, plugin_id, source, pin);
    let (path, sha, fetched) = match fetch_result {
        PersonalFetchResult::Cloned { path, sha } => (path, sha, true),
        PersonalFetchResult::AlreadyPresent(path) => {
            let sha = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.split('@').nth(1))
                .unwrap_or("unknown")
                .to_string();
            (path, sha, false)
        }
        PersonalFetchResult::CloneFailed(m) => return AddResult::Failed(m),
        PersonalFetchResult::NoCacheRoot => {
            return AddResult::Failed("could not determine cache root".to_string());
        }
    };

    match inspect_render_contribution(plugin_id, &path) {
        Ok(true) => {}
        Ok(false) => {
            return AddResult::Failed(
                "source has no renderable contribution (no supported components or MCP servers)"
                    .to_string(),
            );
        }
        Err(error) => {
            return AddResult::Failed(format!("could not inspect source contribution: {error}"));
        }
    }

    let entry = json!({
        "pluginId": plugin_id,
        "displayName": plugin_id,
        "sourceType": "curated-upstream",
        "installStrategy": "personal-git-clone",
        "source": source,
        "pin": sha,
    });

    if let Some(arr) = catalog.get_mut("plugins").and_then(Value::as_array_mut) {
        arr.push(entry);
    } else {
        catalog["plugins"] = json!([entry]);
    }

    if let Err(e) = write_json_atomic(catalog_path, &catalog) {
        return AddResult::WriteFailed(e);
    }

    AddResult::Added { path, sha, fetched }
}

/// Fetch all personal plugins listed in `_personalPlugins` lockfile namespace.
/// Skips entries that are already cached (idempotent).
/// Returns a vec of `(plugin_id, PersonalFetchResult)`.
pub fn fetch_all_personal_plugins() -> Vec<(String, PersonalFetchResult)> {
    let Ok(layout) = effective_layout() else {
        return vec![];
    };
    let cache_root = layout.source_cache_path();
    load_enabled_personal_plugins()
        .into_iter()
        .map(|(id, src)| {
            let result = fetch_personal_plugin(&cache_root, &id, &src);
            (id, result)
        })
        .collect()
}

/// Fetch a plugin from a local archive (`.zip` or `.tar.gz`/`.tgz`) into
/// `~/.ccync/cache/<plugin_id>@<archive-bytes-sha256-first-12>/`.
///
/// Cache key uses the first 12 hex chars of the archive's SHA-256 so the same
/// file fetched twice hits `AlreadyPresent`. A single-root-dir wrapper produced
/// by GitHub downloads is automatically stripped so the plugin content sits at
/// the cache dir root (mirrors what `git clone` produces).
pub fn fetch_archive_plugin(
    cache_root: &Path,
    plugin_id: &str,
    archive_path: &Path,
) -> PersonalFetchResult {
    let prefix = format!("{}@", plugin_id);
    if let Ok(entries) = fs::read_dir(cache_root) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) && entry.path().is_dir() {
                return PersonalFetchResult::AlreadyPresent(entry.path());
            }
        }
    }

    let bytes = match fs::read(archive_path) {
        Ok(b) => b,
        Err(e) => return PersonalFetchResult::CloneFailed(format!("read archive: {e}")),
    };
    let hash = archive_sha_prefix(&bytes);

    let target = cache_root.join(format!("{}@{}", plugin_id, hash));
    if target.exists() {
        return PersonalFetchResult::AlreadyPresent(target);
    }

    if let Err(e) = fs::create_dir_all(cache_root) {
        return PersonalFetchResult::CloneFailed(format!("create cache root: {e}"));
    }

    let tmp = cache_root.join(format!("{}@__extracting__", plugin_id));
    let _ = fs::remove_dir_all(&tmp);

    let ext = archive_extension(archive_path);
    if let Err(e) = extract_archive_to(&bytes, ext, &tmp) {
        let _ = fs::remove_dir_all(&tmp);
        return PersonalFetchResult::CloneFailed(e);
    }

    if let Err(e) = promote_single_root_dir(&tmp) {
        let _ = fs::remove_dir_all(&tmp);
        return PersonalFetchResult::CloneFailed(e);
    }

    if let Err(e) = fs::rename(&tmp, &target) {
        let _ = fs::remove_dir_all(&tmp);
        return PersonalFetchResult::CloneFailed(format!("rename to target: {e}"));
    }

    PersonalFetchResult::Cloned {
        path: target,
        sha: hash,
    }
}

/// Fetch a plain local directory as an immutable content snapshot.
///
/// Walks `dir_path` in normalized relative-path order, accepting only regular
/// files and directories and failing closed on any symlink / junction / reparse
/// entry (so nothing outside the selected tree is ever copied). The snapshot
/// hash is SHA-256 over the sorted, `/`-normalized relative paths + file bytes,
/// so an identical tree produces the same prefix on Windows and Unix. Accepted
/// files are copied into a `<id>@__copying__` staging dir then atomically
/// renamed to `<id>@<sha-first12>`; any error cleans up the staging dir and
/// leaves no catalog/cache residue. An identical snapshot already cached returns
/// `AlreadyPresent`. Unlike archives, no single-root wrapper promotion is done —
/// the caller's directory is the plugin root verbatim.
pub fn fetch_dir_plugin(
    cache_root: &Path,
    plugin_id: &str,
    dir_path: &Path,
) -> PersonalFetchResult {
    // Collect the accepted file set first — this fails closed on any link/reparse
    // entry before any staging dir is created (no temp residue on rejection).
    let entries = match collect_dir_snapshot(dir_path) {
        Ok(e) => e,
        Err(e) => return PersonalFetchResult::CloneFailed(e),
    };

    let hash = dir_snapshot_sha_prefix(&entries);
    let target = cache_root.join(format!("{plugin_id}@{hash}"));
    if target.exists() {
        return PersonalFetchResult::AlreadyPresent(target);
    }

    if let Err(e) = fs::create_dir_all(cache_root) {
        return PersonalFetchResult::CloneFailed(format!("create cache root: {e}"));
    }

    let tmp = cache_root.join(format!("{plugin_id}@__copying__"));
    let _ = fs::remove_dir_all(&tmp);
    if let Err(e) = fs::create_dir_all(&tmp) {
        return PersonalFetchResult::CloneFailed(format!("create staging dir: {e}"));
    }

    for (rel, bytes) in &entries {
        let dest = tmp.join(rel);
        if let Some(parent) = dest.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                let _ = fs::remove_dir_all(&tmp);
                return PersonalFetchResult::CloneFailed(format!(
                    "create dir {}: {e}",
                    parent.display()
                ));
            }
        }
        if let Err(e) = fs::write(&dest, bytes) {
            let _ = fs::remove_dir_all(&tmp);
            return PersonalFetchResult::CloneFailed(format!("write {}: {e}", dest.display()));
        }
    }

    if let Err(e) = fs::rename(&tmp, &target) {
        let _ = fs::remove_dir_all(&tmp);
        return PersonalFetchResult::CloneFailed(format!("rename to target: {e}"));
    }

    PersonalFetchResult::Cloned {
        path: target,
        sha: hash,
    }
}

/// Walk a directory tree, returning accepted `(normalized-relative-path, bytes)`
/// pairs sorted by path. Fails closed on any symlink / junction / reparse entry
/// or non-regular file so nothing outside the selected tree is copied.
pub(crate) fn collect_dir_snapshot(root: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut out = Vec::new();
    walk_dir_snapshot(root, root, &mut out)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn walk_dir_snapshot(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("read dir {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("dir entry: {e}"))?;
        let path = entry.path();
        // symlink_metadata does not follow links — required to detect them.
        let meta =
            fs::symlink_metadata(&path).map_err(|e| format!("stat {}: {e}", path.display()))?;
        if is_link_like(&meta) {
            return Err(format!(
                "refusing to snapshot link/reparse entry: {} (symlinks, junctions, and reparse points are not copied)",
                path.display()
            ));
        }
        let ft = meta.file_type();
        if ft.is_dir() {
            walk_dir_snapshot(root, &path, out)?;
        } else if ft.is_file() {
            let rel_path = path
                .strip_prefix(root)
                .map_err(|e| format!("relativize {}: {e}", path.display()))?;
            // Fail closed on a non-UTF-8 name: a lossy conversion can map two
            // distinct names to the same rel, silently dropping a file from
            // the snapshot (the second write overwrites the first).
            let Some(rel) = rel_path.to_str() else {
                return Err(format!(
                    "refusing to snapshot non-UTF-8 path: {}",
                    path.display()
                ));
            };
            // On Windows `\` is the path separator and is normalized to `/`
            // for cross-platform hash stability. On Unix `\` is a legal
            // FILENAME character; rewriting it would turn the file `a\b` into
            // a path traversal into directory `a/`, so reject it instead.
            #[cfg(windows)]
            let rel = rel.replace('\\', "/");
            #[cfg(not(windows))]
            let rel = {
                if rel.contains('\\') {
                    return Err(format!(
                        "refusing to snapshot path with '\\' in a file name: {}",
                        path.display()
                    ));
                }
                rel.to_string()
            };
            let bytes = fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
            out.push((rel, bytes));
        } else {
            return Err(format!(
                "refusing to snapshot non-regular entry: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

/// True for a symlink, Windows junction, or any other reparse-point entry.
pub(crate) fn is_link_like(meta: &fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

/// SHA-256 (first 12 hex) over the sorted, `/`-normalized relative paths + file
/// bytes. Deterministic across Windows/Unix because paths are normalized.
///
/// Each field is length-prefixed (u64 LE) rather than delimiter-framed: file
/// bytes can legally contain any delimiter (including NUL), so delimiter
/// framing made entry boundaries ambiguous — two different trees could share
/// a hash. Length prefixes make the encoding injective.
fn dir_snapshot_sha_prefix(entries: &[(String, Vec<u8>)]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for (rel, bytes) in entries {
        hasher.update((rel.len() as u64).to_le_bytes());
        hasher.update(rel.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex[..12].to_string()
}

fn archive_sha_prefix(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex[..12].to_string()
}

fn archive_extension(path: &Path) -> &str {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        ".tar.gz"
    } else if name.ends_with(".zip") {
        ".zip"
    } else {
        ""
    }
}

fn extract_archive_to(bytes: &[u8], ext: &str, dst: &Path) -> Result<(), String> {
    match ext {
        ".zip" => extract_zip_to(bytes, dst),
        ".tar.gz" => extract_tar_gz_to(bytes, dst),
        other => Err(format!("unsupported archive format: {other}")),
    }
}

fn extract_zip_to(bytes: &[u8], dst: &Path) -> Result<(), String> {
    use std::io::Cursor;
    let cursor = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor).map_err(|e| format!("open zip: {e}"))?;
    fs::create_dir_all(dst).map_err(|e| format!("create dst: {e}"))?;
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| format!("zip entry {i}: {e}"))?;
        let out_path = match file.enclosed_name() {
            Some(p) => dst.join(p),
            None => continue,
        };
        if file.is_dir() {
            fs::create_dir_all(&out_path).map_err(|e| format!("create dir: {e}"))?;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("create parent: {e}"))?;
            }
            let mut buf = Vec::with_capacity(file.size() as usize);
            std::io::copy(&mut file, &mut buf).map_err(|e| format!("read entry: {e}"))?;
            fs::write(&out_path, &buf).map_err(|e| format!("write entry: {e}"))?;
        }
    }
    Ok(())
}

fn extract_tar_gz_to(bytes: &[u8], dst: &Path) -> Result<(), String> {
    use flate2::read::GzDecoder;
    use std::io::Cursor;
    use tar::Archive;

    let cursor = Cursor::new(bytes);
    let gz = GzDecoder::new(cursor);
    let mut archive = Archive::new(gz);
    fs::create_dir_all(dst).map_err(|e| format!("create dst: {e}"))?;
    for entry in archive.entries().map_err(|e| format!("tar entries: {e}"))? {
        let mut entry = entry.map_err(|e| format!("tar entry: {e}"))?;
        let entry_path = entry
            .path()
            .map_err(|e| format!("entry path: {e}"))?
            .into_owned();
        // Tar-slip guard: reject absolute paths and any `..` traversal so a
        // crafted archive cannot escape `dst`. The tar reader's `path()` is the
        // raw stored name and is NOT containment-checked (unlike the zip path's
        // `enclosed_name()`), so we must validate before joining. Fail closed —
        // one unsafe entry aborts the whole extraction.
        if !is_contained_relative(&entry_path) {
            return Err(format!(
                "unsafe tar entry path (escapes extraction dir): {}",
                entry_path.display()
            ));
        }
        let out_path = dst.join(&entry_path);
        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(&out_path).map_err(|e| format!("create dir: {e}"))?;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("create parent: {e}"))?;
            }
            entry
                .unpack(&out_path)
                .map_err(|e| format!("unpack entry: {e}"))?;
        }
    }
    Ok(())
}

/// True only when `path` is a safe relative path that stays inside its base:
/// not absolute, no root/prefix (drive) component, and no `..` parent-dir
/// component. Used to defend archive extraction against path-traversal
/// (zip-slip / tar-slip) before joining an untrusted entry path onto the
/// extraction dir.
fn is_contained_relative(path: &Path) -> bool {
    use std::path::Component;
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            // RootDir / Prefix (absolute or drive-letter) and ParentDir (`..`)
            // can all escape the extraction dir.
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => return false,
        }
    }
    true
}

/// If `dir` contains exactly one entry that is a directory AND that directory
/// contains CC-plugin structure (skills/, commands/, agents/, hooks/, .mcp.json,
/// plugin.json, .claude-plugin/), promote its contents up to `dir` and remove
/// the now-empty inner dir. This strips the single-root wrapper that GitHub
/// archives typically produce (`repo-main/`) without misidentifying a flat
/// archive whose top-level dir IS a CC-plugin component.
fn promote_single_root_dir(dir: &Path) -> Result<(), String> {
    let entries: Vec<_> = fs::read_dir(dir)
        .map_err(|e| format!("read dir: {e}"))?
        .flatten()
        .collect();
    if entries.len() == 1 && entries[0].path().is_dir() {
        let inner = entries[0].path();
        if !dir_contains_plugin_structure(&inner) {
            return Ok(());
        }
        let inner_entries: Vec<_> = fs::read_dir(&inner)
            .map_err(|e| format!("read inner dir: {e}"))?
            .flatten()
            .collect();
        for entry in inner_entries {
            let dst = dir.join(entry.file_name());
            fs::rename(entry.path(), &dst)
                .map_err(|e| format!("promote {:?}: {e}", entry.file_name()))?;
        }
        fs::remove_dir(&inner).map_err(|e| format!("remove inner dir: {e}"))?;
    }
    Ok(())
}

fn dir_contains_plugin_structure(dir: &Path) -> bool {
    if COMPONENT_DIRS
        .iter()
        .any(|component| dir.join(component).exists())
    {
        return true;
    }

    [".mcp.json", "plugin.json", ".claude-plugin"]
        .iter()
        .any(|marker| dir.join(marker).exists())
        || (dir.join("SKILL.md").is_file()
            && !dir
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| COMPONENT_DIRS.contains(&name)))
}

/// Read the adopted MCP server definitions from the lockfile `_mcpServers`
/// namespace (the ccync truth snapshot, written by `adopt_mcp_definitions`).
/// Returns `(name, definition)` pairs; empty when the lockfile or namespace is
/// absent/unreadable (fail-safe).
fn load_adopted_mcp_servers() -> Vec<(String, Value)> {
    let path = match plugins_lockfile_path() {
        Some(p) => p,
        None => return vec![],
    };
    let contents = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let lock: Value = match serde_json::from_str(&contents) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    lock.get("_mcpServers")
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

/// Merge ccync-core .mcp.json (from canonical root) with any enabled bundle .mcp.json files
/// and write the result to `~/.ccync/build/mcp/managed.json`.
///
/// Fail-safe: missing core or bundle .mcp.json files are silently skipped.
/// Bundle files are read from `~/.ccync/plugins/<resolvedSource>/.mcp.json` and are
/// only present once bundles have been deployed.
/// Read an author-facing plugin/bundle `.mcp.json` server map.
///
/// Standard MCP plugin files use the `mcpServers` key (the plugin ecosystem
/// convention); ccync's own generated canonical/managed files use the internal
/// `servers` key. This reader prefers `mcpServers` and falls back to legacy
/// `servers`, so both are accepted at any author boundary while generated files
/// keep the internal `servers` shape. Provenance (which source a server came
/// from) stays with the caller's merge site; this reader only extracts the map.
fn read_author_mcp_servers(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    value
        .get("mcpServers")
        .and_then(Value::as_object)
        .or_else(|| value.get("servers").and_then(Value::as_object))
}

/// Read an optional `.mcp.json` for merging, distinguishing the two empties:
/// an **absent** file is a silent skip (`Ok(None)` — absence legitimately means
/// "this source contributes nothing"), while a file that **exists but cannot be
/// read or parsed fails closed** with an error naming the path. Swallowing a
/// read/parse error here would yield an empty desired set indistinguishable
/// from "all servers removed", and the managed-manifest write downstream would
/// then delete the user's live MCP servers while reporting clean success.
///
/// The error names the file **path only, never its content** — a malformed
/// `.mcp.json` may hold resolved credentials (the batch-5 FINDING-A-1 class),
/// and `serde_json::Error`'s Display carries only line/column positions.
fn read_optional_mcp_file(path: &Path) -> Result<Option<Value>, InstallError> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            // Exists but unreadable (permissions, sharing violation, …):
            // fail closed rather than treating it as absent.
            return Err(InstallError::Io(std::io::Error::new(
                e.kind(),
                format!("read MCP manifest {}: {e}", path.display()),
            )));
        }
    };
    match serde_json::from_str::<Value>(&content) {
        Ok(v) => Ok(Some(v)),
        Err(e) => Err(InstallError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("parse MCP manifest {}: {e}", path.display()),
        ))),
    }
}

/// Reads the user-authored MCP input at `path` (`~/.ccync/mcp.json`).
///
/// An absent file returns `Ok(None)`. A present file must hold a JSON object
/// whose top-level `mcpServers` value is an object. Each server entry must set
/// exactly one string transport field: `command` (stdio) or `url` (remote). An
/// optional `type` must be `stdio`, `http`, or `sse`, and it must agree with
/// that field.
///
/// Errors name the path only. They never echo server names or values, because
/// the file may hold resolved credentials.
pub fn read_user_mcp_input(
    path: &Path,
) -> Result<Option<serde_json::Map<String, Value>>, InstallError> {
    let Some(root) = read_optional_mcp_file(path)? else {
        return Ok(None);
    };
    let servers = root
        .get("mcpServers")
        .and_then(Value::as_object)
        .ok_or_else(|| user_mcp_input_error(path, "top-level mcpServers must be an object"))?;
    for entry in servers.values() {
        validate_user_mcp_server(entry).map_err(|reason| user_mcp_input_error(path, reason))?;
    }
    Ok(Some(servers.clone()))
}

/// A direct input and its validated routing intent.
#[derive(Debug)]
pub struct DirectMcpInput {
    path: PathBuf,
    servers: serde_json::Map<String, Value>,
    routes: ccync_foundation::mcp::DeclarationRoutes,
}

impl DirectMcpInput {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn servers(&self) -> &serde_json::Map<String, Value> {
        &self.servers
    }

    pub fn routes(&self) -> &ccync_foundation::mcp::DeclarationRoutes {
        &self.routes
    }
}

/// Read the single active direct-input candidate without modifying either file.
pub fn read_direct_mcp_input(
    layout: &ccync_foundation::paths::EffectiveLayout,
) -> Result<Option<DirectMcpInput>, InstallError> {
    let (personal, legacy) = layout.mcp_input_candidates();
    read_direct_mcp_candidates(&personal, &legacy)
}

fn read_direct_mcp_candidates(
    personal: &Path,
    legacy: &Path,
) -> Result<Option<DirectMcpInput>, InstallError> {
    use ccync_foundation::mcp::{read_declaration_routes, DeclarationRoot};

    let personal_root = read_optional_mcp_file(personal)?;
    let legacy_root = read_optional_mcp_file(legacy)?;
    let (path, root, kind, key) = match (personal_root, legacy_root) {
        (None, None) => return Ok(None),
        (Some(_), Some(_)) => {
            return Err(user_mcp_input_error(
                personal,
                &format!("both direct inputs exist; retire {}", legacy.display()),
            ));
        }
        (Some(root), None) => (personal, root, DeclarationRoot::Personal, "ccyncMcpServers"),
        (None, Some(root)) => (legacy, root, DeclarationRoot::Legacy, "mcpServers"),
    };
    let routes = read_declaration_routes(&root, kind, "user")
        .map_err(|error| user_mcp_input_error(path, &error.to_string()))?;
    let servers = root[key].as_object().expect("validated server root");
    for entry in servers.values() {
        validate_user_mcp_server(entry).map_err(|reason| user_mcp_input_error(path, reason))?;
    }
    Ok(Some(DirectMcpInput {
        path: path.to_path_buf(),
        servers: servers.clone(),
        routes,
    }))
}

#[cfg(test)]
mod direct_mcp_candidate_tests {
    use super::read_direct_mcp_candidates;
    use std::fs;

    #[test]
    fn selects_one_candidate_and_rejects_conflicts_and_invalid_routes() {
        let dir = tempfile::tempdir().unwrap();
        let personal = dir.path().join("personal.json");
        let legacy = dir.path().join("legacy.json");
        let project = dir.path().join("project");
        fs::create_dir(&project).unwrap();
        fs::write(
            project.join("mcp.json"),
            r#"{"mcpServers":{"project":{"command":"node"}}}"#,
        )
        .unwrap();
        assert!(read_direct_mcp_candidates(&personal, &legacy)
            .unwrap()
            .is_none());

        fs::write(&legacy, r#"{"mcpServers":{"legacy":{"command":"node"}}}"#).unwrap();
        assert!(read_direct_mcp_candidates(&personal, &legacy)
            .unwrap()
            .unwrap()
            .servers()
            .contains_key("legacy"));
        fs::write(
            &personal,
            r#"{"ccyncMcpServers":{"personal":{"command":"node"}}}"#,
        )
        .unwrap();
        assert!(read_direct_mcp_candidates(&personal, &legacy).is_err());
        fs::remove_file(&legacy).unwrap();
        assert!(read_direct_mcp_candidates(&personal, &legacy)
            .unwrap()
            .unwrap()
            .servers()
            .contains_key("personal"));

        fs::write(
            &personal,
            r#"{"ccyncMcpServers":{"personal":{"command":"node","hosts":["claude"]}},"hostRoutes":[{"source":"user","server":"personal","hosts":[]}]}"#,
        )
        .unwrap();
        let selected = read_direct_mcp_candidates(&personal, &legacy)
            .unwrap()
            .unwrap();
        assert_eq!(selected.path(), personal);
        let identity = ccync_foundation::mcp::DeclarationIdentity {
            source: "user".into(),
            server: "personal".into(),
        };
        assert_eq!(
            selected.routes().declarations[&identity].hosts,
            Some(vec![])
        );

        fs::write(
            &personal,
            r#"{"ccyncMcpServers":{"personal":{"command":"node","hosts":["invalid"]}}}"#,
        )
        .unwrap();
        assert!(read_direct_mcp_candidates(&personal, &legacy).is_err());
        fs::write(&personal, r#"{"mcpServers":{}}"#).unwrap();
        assert!(read_direct_mcp_candidates(&personal, &legacy)
            .unwrap_err()
            .to_string()
            .contains(&personal.display().to_string()));
        fs::remove_file(&personal).unwrap();
        fs::write(&legacy, r#"{"ccyncMcpServers":{}}"#).unwrap();
        assert!(read_direct_mcp_candidates(&personal, &legacy)
            .unwrap_err()
            .to_string()
            .contains(&legacy.display().to_string()));
        fs::remove_file(&legacy).unwrap();
        fs::write(&personal, "{").unwrap();
        assert!(read_direct_mcp_candidates(&personal, &legacy)
            .unwrap_err()
            .to_string()
            .contains(&personal.display().to_string()));
        fs::remove_file(&personal).unwrap();
        fs::create_dir(&personal).unwrap();
        assert!(read_direct_mcp_candidates(&personal, &legacy).is_err());
    }
}

fn user_mcp_input_error(path: &Path, reason: &str) -> InstallError {
    InstallError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("user MCP input {}: {reason}", path.display()),
    ))
}

/// Checks one user-authored server entry. The error is a fixed reason with no
/// content from the entry.
fn validate_user_mcp_server(entry: &Value) -> Result<(), &'static str> {
    let fields = entry.as_object().ok_or("server entry must be an object")?;
    let is_stdio = match (fields.get("command"), fields.get("url")) {
        (Some(command), None) if command.is_string() => true,
        (None, Some(url)) if url.is_string() => false,
        _ => return Err("server entry must set exactly one string command or url"),
    };
    let Some(declared) = fields.get("type") else {
        return Ok(());
    };
    match declared.as_str() {
        Some("stdio") if is_stdio => Ok(()),
        Some("http" | "sse") if !is_stdio => Ok(()),
        Some("stdio" | "http" | "sse") => {
            Err("server type does not match its command or url field")
        }
        _ => Err("server type must be stdio, http, or sse"),
    }
}

/// The result of reading an author-facing plugin MCP configuration.
#[derive(Debug, Clone)]
pub(crate) struct PluginMcpRead {
    pub(crate) unresolved_servers: serde_json::Map<String, Value>,
    pub(crate) servers: serde_json::Map<String, Value>,
    pub(crate) warnings: Vec<String>,
    pub(crate) data_dirs: Vec<PathBuf>,
}

/// Expands plugin root/data placeholders in a single pass without rescanning replacement output.
fn expand_plugin_placeholders(input: &str, plugin_root: &str, plugin_data: &str) -> String {
    const PLACEHOLDERS: [(&str, bool); 4] = [
        ("${PLUGIN_ROOT}", true),
        ("${CLAUDE_PLUGIN_ROOT}", true),
        ("${PLUGIN_DATA}", false),
        ("${CLAUDE_PLUGIN_DATA}", false),
    ];
    let mut result = String::with_capacity(input.len());
    let mut remainder = input;

    while !remainder.is_empty() {
        let Some((offset, (placeholder, is_root))) = PLACEHOLDERS
            .iter()
            .filter_map(|(placeholder, is_root)| {
                remainder
                    .find(placeholder)
                    .map(|offset| (offset, (*placeholder, *is_root)))
            })
            .min_by_key(|(offset, _)| *offset)
        else {
            result.push_str(remainder);
            break;
        };

        result.push_str(&remainder[..offset]);
        result.push_str(if is_root { plugin_root } else { plugin_data });
        remainder = &remainder[offset + placeholder.len()..];
    }

    result
}

/// Read author MCP servers from a plugin directory.
///
/// Looks for `.mcp.json` first, then falls back to `mcp.json`. If both files
/// exist, `.mcp.json` takes precedence and a warning is emitted noting that
/// `mcp.json` is ignored. If neither exists, returns `Ok(None)`. If an existing
/// file cannot be read or parsed, fails closed naming the path.
pub(crate) fn read_plugin_mcp_servers(
    plugin_id: &str,
    plugin_dir: &Path,
) -> Result<Option<PluginMcpRead>, InstallError> {
    let dot_mcp_path = plugin_dir.join(".mcp.json");
    let bare_mcp_path = plugin_dir.join("mcp.json");

    let dot_val = read_optional_mcp_file(&dot_mcp_path)?;
    let bare_val = read_optional_mcp_file(&bare_mcp_path)?;

    let (chosen_val, mut warnings) = match (dot_val, bare_val) {
        (Some(dot), Some(_bare)) => {
            let warn = format!(
                "plugin '{plugin_id}': both .mcp.json and mcp.json present; keeping .mcp.json, ignoring mcp.json"
            );
            (Some(dot), vec![warn])
        }
        (Some(dot), None) => (Some(dot), Vec::new()),
        (None, Some(bare)) => (Some(bare), Vec::new()),
        (None, None) => (None, Vec::new()),
    };

    let Some(val) = chosen_val else {
        return Ok(None);
    };

    let mut servers = read_author_mcp_servers(&val).cloned().unwrap_or_default();

    const SUPPORTED_FIELDS: [&str; 7] = ["type", "command", "args", "env", "cwd", "url", "headers"];
    for (server_name, server_val) in &servers {
        let Value::Object(server_obj) = server_val else {
            continue;
        };
        let mut unsupported_fields: Vec<_> = server_obj
            .keys()
            .filter(|key| !SUPPORTED_FIELDS.contains(&key.as_str()))
            .cloned()
            .collect();
        unsupported_fields.sort();
        if !unsupported_fields.is_empty() {
            warnings.push(format!(
                "plugin '{plugin_id}': server '{server_name}' has unsupported fields: {}; ignored",
                unsupported_fields.join(", ")
            ));
        }
    }

    // Normalize incoming "type": "streamable-http" to internal "http" before merge.
    // "sse" and "stdio" remain untouched; no serializer edits.
    for server_val in servers.values_mut() {
        if let Value::Object(server_obj) = server_val {
            if server_obj.get("type").and_then(Value::as_str) == Some("streamable-http") {
                server_obj.insert("type".to_string(), Value::String("http".to_string()));
            }
        }
    }

    let unresolved_servers = servers.clone();

    // Resolve `./` commands, expand plugin placeholders, and inject PLUGIN_ROOT/PLUGIN_DATA for plugin stdio servers.
    let plugin_data_opt = ccync_foundation::paths::ccync_data_root().map(|d| d.join(plugin_id));
    let plugin_root_str = plugin_dir.display().to_string();
    let plugin_data_str = plugin_data_opt.as_ref().map(|d| d.display().to_string());

    let mut data_dirs = Vec::new();
    let mut dropped_servers = Vec::new();

    for (srv_name, server_val) in servers.iter_mut() {
        let Value::Object(server_obj) = server_val else {
            continue;
        };

        let is_stdio = match server_obj.get_mut("command") {
            Some(Value::String(cmd)) => {
                if let Some(rest) = cmd.strip_prefix("./") {
                    *cmd = plugin_dir.join(rest).display().to_string();
                }
                true
            }
            _ => false,
        };

        if !is_stdio {
            continue;
        }

        // Author env must not override injected plugin roots (Agent Plugins §9.2).
        let reserved_key = match server_obj.get("env") {
            Some(Value::Object(env_map)) if env_map.contains_key("CLAUDE_PLUGIN_ROOT") => {
                Some("CLAUDE_PLUGIN_ROOT")
            }
            Some(Value::Object(env_map)) if env_map.contains_key("CLAUDE_PLUGIN_DATA") => {
                Some("CLAUDE_PLUGIN_DATA")
            }
            Some(Value::Object(env_map))
                if env_map.contains_key("PLUGIN_ROOT") || env_map.contains_key("PLUGIN_DATA") =>
            {
                Some("PLUGIN_ROOT or PLUGIN_DATA")
            }
            _ => None,
        };

        if let Some(reserved_key) = reserved_key {
            warnings.push(format!(
                "server '{srv_name}' from plugin {plugin_id}: env must not set {reserved_key} (Agent Plugins §9.2); skipped"
            ));
            dropped_servers.push(srv_name.clone());
            continue;
        }

        // Placeholder expansion & env injection: skip when ccync_data_root() is None.
        if let (Some(ref data_str), Some(ref data_dir)) = (&plugin_data_str, &plugin_data_opt) {
            if let Some(Value::String(command)) = server_obj.get_mut("command") {
                *command = expand_plugin_placeholders(command, &plugin_root_str, data_str);
            }

            if let Some(Value::Array(args)) = server_obj.get_mut("args") {
                for arg in args {
                    if let Value::String(s) = arg {
                        *s = expand_plugin_placeholders(s, &plugin_root_str, data_str);
                    }
                }
            }

            if let Some(Value::Object(env_map)) = server_obj.get_mut("env") {
                for val in env_map.values_mut() {
                    if let Value::String(s) = val {
                        *s = expand_plugin_placeholders(s, &plugin_root_str, data_str);
                    }
                }
            }

            if let Some(Value::String(cwd)) = server_obj.get_mut("cwd") {
                *cwd = expand_plugin_placeholders(cwd, &plugin_root_str, data_str);
            }

            if !matches!(server_obj.get("env"), Some(Value::Object(_))) {
                server_obj.insert("env".to_string(), Value::Object(serde_json::Map::new()));
            }
            if let Some(Value::Object(env_map)) = server_obj.get_mut("env") {
                env_map.insert(
                    "PLUGIN_ROOT".to_string(),
                    Value::String(plugin_root_str.clone()),
                );
                env_map.insert("PLUGIN_DATA".to_string(), Value::String(data_str.clone()));
            }

            if !data_dirs.contains(data_dir) {
                data_dirs.push(data_dir.clone());
            }
        }
    }

    for name in dropped_servers {
        servers.remove(&name);
    }

    static LEFTOVER_PLACEHOLDER_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let leftover_placeholder_re = LEFTOVER_PLACEHOLDER_RE.get_or_init(|| {
        regex::Regex::new(r"\$\{(CLAUDE_[A-Z0-9_]*|PLUGIN_ROOT|PLUGIN_DATA)\}").unwrap()
    });

    for (server_name, server_val) in &servers {
        let Value::Object(server_obj) = server_val else {
            continue;
        };
        let mut placeholders = Vec::new();
        let mut scan_string = |field: String, value: &str| {
            for capture in leftover_placeholder_re.captures_iter(value) {
                placeholders.push(format!("{field}={}", &capture[0]));
            }
        };

        if let Some(Value::String(value)) = server_obj.get("command") {
            scan_string("command".to_string(), value);
        }
        if let Some(Value::Array(values)) = server_obj.get("args") {
            for (index, value) in values.iter().enumerate() {
                if let Value::String(value) = value {
                    scan_string(format!("args[{index}]"), value);
                }
            }
        }
        if let Some(Value::Object(values)) = server_obj.get("env") {
            let mut names: Vec<&String> = values.keys().collect();
            names.sort();
            for name in names {
                if let Some(Value::String(value)) = values.get(name) {
                    scan_string(format!("env.{name}"), value);
                }
            }
        }
        if let Some(Value::String(value)) = server_obj.get("cwd") {
            scan_string("cwd".to_string(), value);
        }
        if let Some(Value::String(value)) = server_obj.get("url") {
            scan_string("url".to_string(), value);
        }
        if let Some(Value::Object(values)) = server_obj.get("headers") {
            let mut names: Vec<&String> = values.keys().collect();
            names.sort();
            for name in names {
                if let Some(Value::String(value)) = values.get(name) {
                    scan_string(format!("headers.{name}"), value);
                }
            }
        }

        if !placeholders.is_empty() {
            warnings.push(format!(
                "plugin '{plugin_id}': server '{server_name}' has unexpanded placeholders: {}; server kept",
                placeholders.join("; ")
            ));
        }
    }

    Ok(Some(PluginMcpRead {
        unresolved_servers,
        servers,
        warnings,
        data_dirs,
    }))
}

#[derive(Debug, Clone)]
pub struct McpMergeSource {
    label: String,
    servers: serde_json::Map<String, Value>,
    provenance: std::collections::HashMap<String, ccync_foundation::mcp::McpServerProvenance>,
}

impl McpMergeSource {
    pub fn new(
        label: String,
        servers: serde_json::Map<String, Value>,
        provenance: std::collections::HashMap<String, ccync_foundation::mcp::McpServerProvenance>,
    ) -> Self {
        assert!(
            servers.keys().all(|name| provenance.contains_key(name)),
            "each MCP declaration requires provenance"
        );
        Self {
            label,
            servers,
            provenance,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpMergeLoser {
    server: String,
    losing_source: String,
    winning_source: String,
}

impl McpMergeLoser {
    pub fn server(&self) -> &str {
        &self.server
    }
    pub fn losing_source(&self) -> &str {
        &self.losing_source
    }
    pub fn winning_source(&self) -> &str {
        &self.winning_source
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpMergeOutcome {
    servers: serde_json::Map<String, Value>,
    provenance: std::collections::HashMap<String, ccync_foundation::mcp::McpServerProvenance>,
    losers: Vec<McpMergeLoser>,
    warnings: Vec<String>,
    data_dirs: Vec<PathBuf>,
}

impl McpMergeOutcome {
    pub fn servers(&self) -> &serde_json::Map<String, Value> {
        &self.servers
    }
    pub fn provenance(
        &self,
    ) -> &std::collections::HashMap<String, ccync_foundation::mcp::McpServerProvenance> {
        &self.provenance
    }
    pub fn losers(&self) -> &[McpMergeLoser] {
        &self.losers
    }
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

/// Merge sources in precedence order, preserving the supplied winner provenance.
pub fn merge_mcp_declarations(sources: Vec<McpMergeSource>) -> McpMergeOutcome {
    let mut outcome = McpMergeOutcome {
        servers: serde_json::Map::new(),
        provenance: std::collections::HashMap::new(),
        losers: Vec::new(),
        warnings: Vec::new(),
        data_dirs: Vec::new(),
    };
    let mut owner = std::collections::HashMap::<String, String>::new();
    for McpMergeSource {
        label,
        servers,
        mut provenance,
    } in sources
    {
        let mut entries: Vec<_> = servers.into_iter().collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, value) in entries {
            if let Some(retained) = owner.get(&name) {
                if provenance
                    .get(&name)
                    .is_some_and(|p| p.native_package_entry())
                {
                    if let Some(p) = outcome.provenance.get_mut(&name) {
                        *p = ccync_foundation::mcp::McpServerProvenance::new(
                            p.identity().clone(),
                            p.kind(),
                            p.package_version().map(str::to_owned),
                            p.unresolved_digest().to_owned(),
                            p.hosts().map(<[String]>::to_vec),
                            p.enabled(),
                            p.source_preserved(),
                            true,
                        );
                    }
                }
                outcome.warnings.push(format!(
                    "MCP server '{name}': keeping definition from {retained}, ignoring duplicate from {label}"
                ));
                outcome.losers.push(McpMergeLoser {
                    server: name,
                    losing_source: label.clone(),
                    winning_source: retained.clone(),
                });
            } else {
                if let Some(item) = provenance.remove(&name) {
                    outcome.provenance.insert(name.clone(), item);
                }
                owner.insert(name.clone(), label.clone());
                outcome.servers.insert(name, value);
            }
        }
    }
    outcome
}

#[derive(Debug, thiserror::Error)]
pub enum McpDeclarationsError {
    #[error("MCP declarations blocked: {0}")]
    Blocked(String),
    #[error("invalid MCP input: {0}")]
    InvalidInput(String),
}

/// Read declarations and their routing intent without creating or changing files.
pub fn read_mcp_declarations() -> Result<McpMergeOutcome, McpDeclarationsError> {
    use ccync_foundation::mcp::{
        read_declaration_routes, DeclarationIdentity, DeclarationRoot, DeclarationRoutes,
        McpServerProvenance, McpSourceKind,
    };

    let layout = effective_layout().map_err(|e| McpDeclarationsError::Blocked(e.to_string()))?;
    let direct = read_direct_mcp_input(&layout).map_err(|e| match &e {
        InstallError::Io(error) if error.kind() != std::io::ErrorKind::InvalidData => {
            McpDeclarationsError::Blocked(e.to_string())
        }
        _ => McpDeclarationsError::InvalidInput(e.to_string()),
    })?;
    let routes = direct
        .as_ref()
        .map(|input| input.routes.clone())
        .unwrap_or(DeclarationRoutes {
            declarations: Default::default(),
            overrides: Default::default(),
        });
    let lock_path = plugins_lockfile_path()
        .ok_or_else(|| McpDeclarationsError::Blocked("plugin lockfile path".into()))?;
    let lock = match fs::read_to_string(&lock_path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "{}".to_string(),
        Err(e) => {
            return Err(McpDeclarationsError::Blocked(format!(
                "{}: {e}",
                lock_path.display()
            )))
        }
    };
    let lock: Value = serde_json::from_str(&lock)
        .map_err(|e| McpDeclarationsError::Blocked(format!("{}: {e}", lock_path.display())))?;
    if !lock.is_object() {
        return Err(McpDeclarationsError::Blocked(format!(
            "{}: expected object",
            lock_path.display()
        )));
    }
    for (key, is_array) in [
        ("resolvedPlugins", true),
        ("_personalPlugins", true),
        ("_mcpServers", false),
    ] {
        if let Some(value) = lock.get(key) {
            if (is_array && !value.is_array()) || (!is_array && !value.is_object()) {
                return Err(McpDeclarationsError::Blocked(format!(
                    "{}: invalid {key}",
                    lock_path.display()
                )));
            }
        }
    }

    let mut pending = Vec::new();
    if let Some(input) = direct {
        pending.push((
            "user".to_string(),
            McpSourceKind::Direct,
            None,
            false,
            input.servers.clone(),
            input.servers,
        ));
    }
    let mut packages = Vec::new();
    let mut warnings = Vec::new();
    let mut data_dirs = Vec::new();
    for (id, source) in load_enabled_bundles() {
        let root = crate::paths::ccync_plugins_root().ok_or_else(|| {
            McpDeclarationsError::Blocked(format!("package {id}: plugins root unavailable"))
        })?;
        packages.push((id, root.join(source), McpSourceKind::Core));
    }
    let pins = personal_pins();
    for (id, _) in load_enabled_personal_plugins() {
        let candidates = cache_candidates(&layout.source_cache_path(), &id);
        let (chosen, ambiguous) =
            pick_live_dirs(&candidates, pins.get(&id).map(String::as_str), &id);
        if ambiguous || chosen.len() != 1 {
            return Err(McpDeclarationsError::Blocked(format!(
                "package {id}: no unique cache directory"
            )));
        }
        packages.push((id, chosen[0].clone(), McpSourceKind::Package));
    }
    for (id, dir, kind) in packages {
        if !dir.is_dir() {
            return Err(McpDeclarationsError::Blocked(format!(
                "package {id}: directory {} missing",
                dir.display()
            )));
        }
        let read = read_plugin_mcp_servers(&id, &dir)
            .map_err(|e| McpDeclarationsError::Blocked(format!("package {id}: {e}")))?;
        let Some(read) = read else { continue };
        for (name, value) in &read.unresolved_servers {
            crate::catalog::validate_mcp_output(name, value, true)
                .map_err(McpDeclarationsError::InvalidInput)?;
        }
        data_dirs.extend(read.data_dirs.iter().cloned());
        read_declaration_routes(
            &json!({"ccyncMcpServers": read.unresolved_servers}),
            DeclarationRoot::Personal,
            &id,
        )
        .map_err(|e| McpDeclarationsError::InvalidInput(format!("{id}: {e}")))?;
        warnings.extend(read.warnings.clone());
        let mut version = None;
        let mut native_entry = false;
        for manifest in [
            "plugin.json",
            ".claude-plugin/plugin.json",
            ".codex-plugin/plugin.json",
        ] {
            let path = dir.join(manifest);
            if let Some(value) = read_optional_mcp_file(&path)
                .map_err(|e| McpDeclarationsError::Blocked(format!("package {id}: {e}")))?
            {
                if version.is_none() {
                    version = value
                        .get("version")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
                native_entry |= value.get("mcpServers").is_some()
                    || value.pointer("/extensions/com.openai/mcpServers").is_some();
            }
        }
        let servers = read
            .unresolved_servers
            .into_iter()
            .filter(|(name, _)| read.servers.contains_key(name))
            .collect();
        pending.push((id, kind, version, native_entry, servers, read.servers));
    }
    let mut adopted: serde_json::Map<String, Value> =
        load_adopted_mcp_servers().into_iter().collect();
    let mut preserved = std::collections::HashSet::new();
    let catalog = layout.personal_catalog_path();
    if catalog.exists() {
        let records = crate::adopt::read_adoption_records(&catalog)
            .map_err(|e| McpDeclarationsError::Blocked(e.to_string()))?;
        for record in records
            .iter()
            .filter(|r| r.kind == "mcp-server" && r.digest.is_some())
        {
            adopted.remove(&record.name);
            match crate::adopt::verify_consented_mcp(record) {
                Ok(crate::adopt::ConsentedMcp::Portable(value)) => {
                    adopted.insert(record.name.clone(), value);
                }
                Ok(crate::adopt::ConsentedMcp::SourcePreserved(value)) => {
                    let modeled: ccync_foundation::mcp::McpServer = serde_json::from_value(value)
                        .map_err(|_| {
                        McpDeclarationsError::Blocked(format!("{}: unsafe", record.name))
                    })?;
                    let value = serde_json::to_value(modeled)
                        .map_err(|e| McpDeclarationsError::Blocked(e.to_string()))?;
                    preserved.insert(record.name.clone());
                    adopted.insert(record.name.clone(), value);
                }
                Err(reason) => {
                    return Err(McpDeclarationsError::Blocked(format!(
                        "{}: {:?}",
                        record.name, reason
                    )))
                }
            }
        }
    }
    pending.push((
        "adopted".into(),
        McpSourceKind::Adopted,
        None,
        false,
        adopted.clone(),
        adopted,
    ));

    let mut sources = Vec::new();
    for (label, kind, version, native_entry, servers, resolved_servers) in pending {
        for (name, value) in &servers {
            if kind != McpSourceKind::Adopted || !preserved.contains(name) {
                crate::catalog::validate_mcp_output(name, value, true)
                    .map_err(McpDeclarationsError::InvalidInput)?;
            }
        }
        let document = json!({"ccyncMcpServers": servers});
        let inline = read_declaration_routes(&document, DeclarationRoot::Personal, &label)
            .map_err(|e| McpDeclarationsError::InvalidInput(format!("{label}: {e}")))?;
        let servers = document["ccyncMcpServers"]
            .as_object()
            .expect("constructed object")
            .clone();
        let mut provenance = std::collections::HashMap::new();
        for (name, value) in &servers {
            let identity = DeclarationIdentity {
                source: label.clone(),
                server: name.clone(),
            };
            let route = &inline.declarations[&identity];
            let explicit = routes.overrides.get(&identity);
            let is_local = value.get("command").is_some();
            let hosts = explicit
                .cloned()
                .or_else(|| {
                    if kind == McpSourceKind::Direct || !is_local {
                        route.hosts.clone()
                    } else {
                        None
                    }
                })
                .or_else(|| {
                    (!is_local).then(|| {
                        ["claude", "codex", "copilot", "opencode", "agy"]
                            .into_iter()
                            .map(str::to_owned)
                            .collect()
                    })
                });
            let enabled = if kind == McpSourceKind::Direct {
                routes
                    .declarations
                    .get(&identity)
                    .map_or(true, |r| r.enabled)
            } else {
                route.enabled
            };
            let digest = crate::adopt::mcp_source_fingerprint(value)
                .map_err(|e| McpDeclarationsError::Blocked(format!("{label}: {e}")))?;
            provenance.insert(
                name.clone(),
                McpServerProvenance::new(
                    identity,
                    kind,
                    version.clone(),
                    digest,
                    hosts,
                    enabled,
                    kind == McpSourceKind::Adopted && preserved.contains(name),
                    native_entry,
                ),
            );
        }
        sources.push(McpMergeSource::new(label, resolved_servers, provenance));
    }
    let mut outcome = merge_mcp_declarations(sources);
    outcome.warnings.extend(warnings);
    outcome.data_dirs = data_dirs;
    Ok(outcome)
}

fn merge_mcp_sources_with_warnings(
    sources: Vec<(String, serde_json::Map<String, Value>)>,
) -> (serde_json::Map<String, Value>, Vec<String>) {
    let outcome = merge_mcp_declarations(
        sources
            .into_iter()
            .map(|(label, servers)| McpMergeSource {
                label,
                servers,
                provenance: Default::default(),
            })
            .collect(),
    );
    (outcome.servers, outcome.warnings)
}

#[cfg(test)]
mod merge_declaration_tests {
    use super::*;
    use ccync_foundation::mcp::{DeclarationIdentity, McpServerProvenance, McpSourceKind};

    #[test]
    fn merge_mcp_declarations_carries_loser_native_package_entry_to_winner() {
        for native in [false, true] {
            let source = |label: &str, kind, native| {
                let provenance = McpServerProvenance::new(
                    DeclarationIdentity {
                        source: label.into(),
                        server: "x".into(),
                    },
                    kind,
                    Some("1".into()),
                    "digest".into(),
                    Some(vec!["claude".into()]),
                    true,
                    false,
                    native,
                );
                McpMergeSource::new(
                    label.into(),
                    serde_json::json!({"x": {}}).as_object().unwrap().clone(),
                    [("x".into(), provenance)].into_iter().collect(),
                )
            };
            let outcome = merge_mcp_declarations(vec![
                source("direct", McpSourceKind::Direct, false),
                source("package", McpSourceKind::Package, native),
            ]);
            let winner = &outcome.provenance()["x"];
            assert_eq!(winner.identity().source, "direct");
            assert_eq!(winner.kind(), McpSourceKind::Direct);
            assert_eq!(winner.native_package_entry(), native);
            assert_eq!(outcome.losers().len(), 1);
        }
    }

    #[test]
    fn mcp_declarations_native_entry_only_from_package_manifest() {
        let home = crate::test_support::TestHome::new();
        let base = home.path().join(".ccync");
        let package = base.join("cache/personal@abc");
        fs::create_dir_all(package.join(".claude-plugin")).unwrap();
        fs::create_dir_all(base.join("build")).unwrap();
        fs::write(
            package.join(".mcp.json"),
            r#"{"mcpServers":{"example":{"command":"node"}}}"#,
        )
        .unwrap();
        fs::write(
            base.join("build/lock.json"),
            r#"{"_personalPlugins":[{"pluginId":"personal","source":"example","pin":"abc"}]}"#,
        )
        .unwrap();
        let first = read_mcp_declarations().unwrap();
        assert!(!first.provenance()["example"].native_package_entry());
        fs::write(
            package.join(".claude-plugin/plugin.json"),
            r#"{"mcpServers":{"example":{"command":"node"}}}"#,
        )
        .unwrap();
        let second = read_mcp_declarations().unwrap();
        assert!(second.provenance()["example"].native_package_entry());
    }

    #[test]
    fn inventory_rejects_machine_config_secret_before_publish() {
        let _home = crate::test_support::TestHome::new();
        let layout = effective_layout().unwrap();
        fs::create_dir_all(layout.machine_config_path().parent().unwrap()).unwrap();
        fs::write(
            layout.machine_config_path(),
            r#"{"secrets":{"TOKEN":"private-marker-123"}}"#,
        )
        .unwrap();
        fs::create_dir_all(layout.mcp_input_path().parent().unwrap()).unwrap();
        fs::write(
            layout.mcp_input_path(),
            r#"{"ccyncMcpServers":{"demo":{"command":"private-marker-123"}}}"#,
        )
        .unwrap();
        assert!(matches!(
            read_mcp_declarations(),
            Err(McpDeclarationsError::InvalidInput(_))
        ));
        assert!(!layout.generated_mcp_path().exists());
    }

    #[test]
    fn inventory_reads_resolved_personal_package_after_install() {
        let home = crate::test_support::TestHome::new();
        let layout = effective_layout().unwrap();
        let source = home.path().join("package-source");
        fs::create_dir_all(source.join(".claude-plugin")).unwrap();
        fs::write(
            source.join(".claude-plugin/plugin.json"),
            r#"{"name":"personal-fixture","version":"1.2.3"}"#,
        )
        .unwrap();
        fs::write(
            source.join(".mcp.json"),
            r#"{"mcpServers":{"shared":{"command":"package"}}}"#,
        )
        .unwrap();
        let cache = layout.source_cache_path();
        let (dir, sha) = match fetch_dir_plugin(&cache, "personal-fixture", &source) {
            PersonalFetchResult::Cloned { path, sha } => (path, sha),
            other => panic!("fetch failed: {other:?}"),
        };
        assert!(matches!(
            add_plugin_entry(
                &cache,
                &layout.personal_catalog_path(),
                "personal-fixture",
                source.to_str().unwrap(),
                Some(&sha)
            ),
            AddResult::Added { .. }
        ));
        fs::create_dir_all(layout.config_path().parent().unwrap()).unwrap();
        fs::write(
            layout.config_path(),
            r#"{"selectedProjectionTargets":["claude"]}"#,
        )
        .unwrap();
        crate::catalog::run_resolve_catalog(&crate::catalog::ResolveCatalogOptions {
            catalog_path: None,
            config_path: layout.config_path(),
            lockfile_path: layout.build_lock_path(),
            dry_run: false,
            personal_catalog_path_override: Some(layout.personal_catalog_path()),
        })
        .unwrap();
        let config = CcyncConfig::load_from_path(&layout.config_path());
        run_update_with_handoff_using(&config, &crate::delivery_apply::UnavailableNativeRunner)
            .unwrap();
        let direct = layout.mcp_input_path();
        fs::create_dir_all(direct.parent().unwrap()).unwrap();
        fs::write(
            direct,
            r#"{"ccyncMcpServers":{"shared":{"command":"direct"}}}"#,
        )
        .unwrap();
        let outcome = read_mcp_declarations().unwrap();
        assert_eq!(outcome.servers()["shared"]["command"], "direct");
        assert!(
            outcome
                .losers()
                .iter()
                .any(|loser| loser.server() == "shared"
                    && loser.losing_source() == "personal-fixture")
        );
        fs::remove_dir_all(dir).unwrap();
        assert!(matches!(
            read_mcp_declarations(),
            Err(McpDeclarationsError::Blocked(_))
        ));
    }

    #[test]
    fn inventory_reads_sources_in_order_without_creating_data() {
        let home = crate::test_support::TestHome::new();
        let base = home.path().join(".ccync");
        let direct = home.path().join(".agents/mcp.json");
        let core = base.join("plugins/core");
        let package = base.join("cache").join("personal@abc");
        for dir in [
            direct.parent().unwrap(),
            core.as_path(),
            package.as_path(),
            base.join("build").as_path(),
        ] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(&direct, r#"{"ccyncMcpServers":{"shared":{"command":"node","hosts":["claude"],"env":{"TOKEN":"${INVENTORY_SECRET}"}},"remote":{"url":"https://example.com"},"inline":{"command":"node","hosts":["copilot"]},"disabled":{"url":"https://example.com","enabled":false}},"hostRoutes":[{"source":"user","server":"shared","hosts":["codex"]},{"source":"personal","server":"package-explicit","hosts":["agy"]}]}"#).unwrap();
        fs::write(
            core.join(".mcp.json"),
            r#"{"mcpServers":{"shared":{"command":"core"},"local":{"command":"node"}}}"#,
        )
        .unwrap();
        fs::write(core.join("plugin.json"), r#"{"version":"1.2.3"}"#).unwrap();
        fs::write(
            package.join(".mcp.json"),
            r#"{"mcpServers":{"shared":{"command":"package"},"package-local":{"command":"./server","args":["${PLUGIN_DATA}"],"hosts":["claude"]},"package-remote":{"url":"https://example.com","hosts":["codex"]},"package-explicit":{"command":"node"},"dropped":{"command":"node","env":{"PLUGIN_ROOT":"reserved"}}}}"#,
        )
        .unwrap();
        fs::write(package.join("plugin.json"), r#"{"version":"2.7.1"}"#).unwrap();
        fs::write(base.join("build/lock.json"), r#"{"resolvedPlugins":[{"pluginId":"core","installStrategy":"bundled-local","resolvedSource":"core"}],"_personalPlugins":[{"pluginId":"personal","source":"example","pin":"abc"}],"_mcpServers":{"shared":{"command":"adopted"}}}"#).unwrap();
        let before = collect_dir_snapshot(home.path()).unwrap();
        let first = read_mcp_declarations().unwrap();
        assert_eq!(first, read_mcp_declarations().unwrap());
        assert_eq!(before, collect_dir_snapshot(home.path()).unwrap());
        assert_eq!(first.servers()["shared"]["command"], "node");
        assert_eq!(first.losers().len(), 3);
        assert_eq!(first.provenance()["shared"].hosts().unwrap(), ["codex"]);
        assert_eq!(first.provenance()["local"].hosts(), None);
        assert_eq!(first.provenance()["local"].package_version(), Some("1.2.3"));
        assert_eq!(first.provenance()["remote"].hosts().unwrap().len(), 5);
        assert_eq!(first.provenance()["inline"].hosts().unwrap(), ["copilot"]);
        assert!(!first.provenance()["disabled"].enabled());
        assert_eq!(first.provenance()["package-local"].hosts(), None);
        assert_eq!(
            first.servers()["package-local"]["command"],
            package.join("server").display().to_string()
        );
        assert_eq!(
            first.servers()["package-local"]["env"]["PLUGIN_ROOT"],
            package.display().to_string()
        );
        assert_eq!(
            first.servers()["package-local"]["args"][0],
            base.join("data").join("personal").display().to_string()
        );
        assert!(!first.servers().contains_key("dropped"));
        assert_eq!(
            first.provenance()["package-remote"].hosts().unwrap(),
            ["codex"]
        );
        assert_eq!(
            first.provenance()["package-explicit"].hosts().unwrap(),
            ["agy"]
        );
        assert_eq!(
            first.provenance()["package-local"].package_version(),
            Some("2.7.1")
        );
        assert_eq!(
            first.servers()["shared"]["env"]["TOKEN"],
            "${INVENTORY_SECRET}"
        );
        assert!(!format!("{:?}", first.provenance()).contains("${INVENTORY_SECRET}"));
        assert_eq!(
            first
                .losers()
                .iter()
                .map(|loser| loser.losing_source())
                .collect::<Vec<_>>(),
            ["core", "personal", "adopted"]
        );
        assert!(!base.join("data").exists());
        fs::write(package.join(".mcp.json"), r#"{"mcpServers":{"invalid":{"command":"node","env":{"PLUGIN_ROOT":"reserved"},"hosts":["unknown"]}}}"#).unwrap();
        let before = collect_dir_snapshot(home.path()).unwrap();
        assert!(matches!(
            read_mcp_declarations(),
            Err(McpDeclarationsError::InvalidInput(_))
        ));
        assert_eq!(before, collect_dir_snapshot(home.path()).unwrap());
        fs::remove_dir_all(&package).unwrap();
        let before = collect_dir_snapshot(home.path()).unwrap();
        let error = read_mcp_declarations().unwrap_err().to_string();
        assert!(error.contains("personal"));
        assert_eq!(before, collect_dir_snapshot(home.path()).unwrap());
        fs::create_dir_all(&package).unwrap();
        fs::remove_dir_all(&core).unwrap();
        let before = collect_dir_snapshot(home.path()).unwrap();
        let error = read_mcp_declarations().unwrap_err().to_string();
        assert!(error.contains("core"));
        assert_eq!(before, collect_dir_snapshot(home.path()).unwrap());
        assert!(!core.exists());
        assert!(!base.join("data").exists());
    }

    #[test]
    fn merge_preserves_winner_and_ordered_losers() {
        let sources: Vec<_> = [
            McpSourceKind::Direct,
            McpSourceKind::Core,
            McpSourceKind::Package,
            McpSourceKind::Adopted,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, kind)| {
            let label = format!("source-{index}");
            let provenance = McpServerProvenance::new(
                DeclarationIdentity {
                    source: label.clone(),
                    server: "shared".into(),
                },
                kind,
                Some(format!("{index}.0")),
                "digest".into(),
                None,
                true,
                false,
                false,
            );
            McpMergeSource::new(
                label,
                serde_json::json!({"shared": index})
                    .as_object()
                    .unwrap()
                    .clone(),
                [("shared".into(), provenance)].into_iter().collect(),
            )
        })
        .collect();
        let first = merge_mcp_declarations(sources.clone());
        let second = merge_mcp_declarations(sources);
        assert_eq!(first, second);
        assert_eq!(first.servers()["shared"], 0);
        assert_eq!(first.provenance()["shared"].package_version(), Some("0.0"));
        assert_eq!(
            first
                .losers()
                .iter()
                .map(|item| (item.server(), item.losing_source(), item.winning_source()))
                .collect::<Vec<_>>(),
            vec![
                ("shared", "source-1", "source-0"),
                ("shared", "source-2", "source-0"),
                ("shared", "source-3", "source-0")
            ]
        );
        assert_eq!(first.warnings().len(), 3);
    }
}

#[cfg(test)]
fn generate_managed_mcp(
    _canonical_root: &Path,
    _user_servers: serde_json::Map<String, Value>,
) -> Result<(), InstallError> {
    let inventory =
        read_mcp_declarations().map_err(|error| invalid_lifecycle_state(error.to_string()))?;
    publish_managed_mcp(&inventory)
}

fn publish_managed_mcp(inventory: &McpMergeOutcome) -> Result<(), InstallError> {
    for warning in &inventory.warnings {
        eprintln!("ccync: {warning}");
    }
    for dir in &inventory.data_dirs {
        fs::create_dir_all(dir)?;
    }
    let mut managed = json!({ "servers": inventory.servers });
    if !inventory.provenance.is_empty() {
        let provenance: std::collections::BTreeMap<_, _> = inventory.provenance.iter().collect();
        managed["provenance"] = serde_json::to_value(provenance)?;
    }
    let managed_path = effective_layout()?.generated_mcp_path();
    let bytes = serde_json::to_vec_pretty(&managed)?;
    ensure_parent_dir(&managed_path)?;
    ccync_foundation::platform::atomic_write_bytes(&managed_path, &bytes)
        .map_err(|error| InstallError::Io(std::io::Error::other(error.to_string())))?;
    Ok(())
}

fn claude_marketplace_manifest_path() -> Result<PathBuf, InstallError> {
    crate::paths::ccync_plugins_root()
        .map(|root| root.join(".claude-plugin").join("marketplace.json"))
        .ok_or(InstallError::NoHome)
}

fn codex_marketplace_manifest_path() -> Result<PathBuf, InstallError> {
    // Not a candidate for `shared_agents_skills_path()`: this ".agents" is
    // relative to `ccync_plugins_root()`, not home, and the final segment is
    // "plugins/marketplace.json", not "skills" — substituting a home-based
    // helper here would silently resolve to a different path.
    crate::paths::ccync_plugins_root()
        .map(|root| {
            root.join(".agents")
                .join("plugins")
                .join("marketplace.json")
        })
        .ok_or(InstallError::NoHome)
}

fn copilot_projection_root() -> Result<PathBuf, InstallError> {
    crate::paths::user_home()
        .map(|home| {
            home.join(".copilot")
                .join("installed-plugins")
                .join("ccync-copilot")
                .join("ccync")
        })
        .ok_or(InstallError::NoHome)
}

fn sync_copilot_projection(
    canonical_root: &Path,
) -> Result<(PathBuf, bool, Option<String>), InstallError> {
    let projection_root = copilot_projection_root()?;
    ensure_parent_dir(&projection_root)?;

    if ccync_foundation::platform::is_symlink_or_junction(&projection_root) {
        let _ = ccync_foundation::platform::remove_dir_link(&projection_root);
    } else if projection_root.exists() {
        fs::remove_dir_all(&projection_root)?;
    }

    // When the canonical root also holds the private provider generation
    // store, a link would expose it as part of Copilot's plugin root, so
    // materialize a filtered copy instead. Otherwise keep the plain link.
    if !canonical_root.join("providers").exists()
        && ccync_foundation::platform::create_dir_link(canonical_root, &projection_root).is_ok()
    {
        return Ok((projection_root, false, None));
    }

    copy_copilot_projection(canonical_root, &projection_root)?;
    let manifest_path = projection_root.join("copilot-manifest.json");
    let version_bumped_to = bump_copilot_manifest_version(&manifest_path)?;
    Ok((projection_root, true, version_bumped_to))
}

fn copy_copilot_projection(src: &Path, dst: &Path) -> Result<(), InstallError> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        if entry.file_name() == "providers" {
            continue;
        }
        let source_path = entry.path();
        let target_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_copilot_projection(&source_path, &target_path)?;
        } else {
            fs::copy(&source_path, &target_path)?;
        }
    }
    Ok(())
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), InstallError> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&source_path, &target_path)?;
        } else {
            fs::copy(&source_path, &target_path)?;
        }
    }
    Ok(())
}

fn bump_copilot_manifest_version(path: &Path) -> Result<Option<String>, InstallError> {
    if !path.is_file() {
        return Ok(None);
    }
    let mut manifest: Value = serde_json::from_str(&fs::read_to_string(path)?)?;
    let current = manifest
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("1.0.0");
    let bumped = format!(
        "{current}.host{}",
        chrono::Utc::now().format("%Y%m%d%H%M%S")
    );
    manifest["version"] = Value::String(bumped.clone());
    fs::write(path, serde_json::to_string_pretty(&manifest)?)?;
    Ok(Some(bumped))
}

/// Resolve the on-disk cache directories of the enabled managed set — bundled
/// plugins (under `~/.ccync/plugins/<resolvedSource>`) plus personal plugins
/// (`~/.ccync/cache/<id>@<sha>`). Only existing dirs are returned.
fn managed_plugin_dirs() -> Vec<(String, PathBuf)> {
    let mut dirs = Vec::new();
    if let Some(plugins_root) = crate::paths::ccync_plugins_root() {
        for (id, resolved_source) in load_enabled_bundles() {
            let d = plugins_root.join(&resolved_source);
            if d.is_dir() {
                dirs.push((id, d));
            }
        }
    }
    if let Ok(cache_root) = effective_layout().map(|layout| layout.source_cache_path()) {
        let pins = personal_pins();
        for (id, _) in load_enabled_personal_plugins() {
            let candidates = cache_candidates(&cache_root, &id);
            let (chosen, ambiguous) =
                pick_live_dirs(&candidates, pins.get(&id).map(String::as_str), &id);
            if ambiguous {
                eprintln!(
                    "ccync: plugin '{id}' has {} cache dirs but no resolved pin — \
                     skipped from render (ambiguous; run `ccync upgrade {id}`).",
                    candidates.len()
                );
            }
            for d in chosen {
                dirs.push((id.clone(), d));
            }
        }
    }
    dirs
}

/// All `<id>@<sha>` cache dirs for a plugin id (the `@` delimiter prevents an id
/// being a prefix of a longer id, e.g. `foo` vs `foobar`).
pub(crate) fn cache_candidates(cache_root: &Path, id: &str) -> Vec<PathBuf> {
    let prefix = format!("{id}@");
    let Ok(entries) = fs::read_dir(cache_root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .map(|n| n.to_string_lossy().starts_with(&prefix))
                    .unwrap_or(false)
        })
        .collect()
}

/// Choose which cache dir(s) to render for a personal plugin (pure, no IO).
///
/// With a resolved `pin`, render exactly `<id>@<pin>` (the live dir) and ignore
/// stale-sha siblings. Without a pin: a single candidate is unambiguous and
/// rendered; multiple candidates are **ambiguous** → render none and signal it
/// (fail-closed — never guess which sha is live). Returns `(chosen, ambiguous)`.
pub(crate) fn pick_live_dirs(
    candidates: &[PathBuf],
    pin: Option<&str>,
    id: &str,
) -> (Vec<PathBuf>, bool) {
    match pin {
        Some(pin) => {
            let want = format!("{id}@{pin}");
            let chosen = candidates
                .iter()
                .filter(|p| p.file_name().and_then(|n| n.to_str()) == Some(want.as_str()))
                .cloned()
                .collect();
            (chosen, false)
        }
        None => match candidates.len() {
            1 => (candidates.to_vec(), false),
            n if n > 1 => (Vec::new(), true),
            _ => (Vec::new(), false),
        },
    }
}

fn stage_canonical_render(canonical_root: &Path) -> Result<PathBuf, InstallError> {
    let parent = canonical_root.parent().ok_or_else(|| {
        InstallError::Io(std::io::Error::other(format!(
            "canonical root has no parent: {}",
            canonical_root.display()
        )))
    })?;
    let stage = parent.join(format!(
        ".ccync-render-stage-{}",
        uuid::Uuid::new_v4().simple()
    ));

    let result = (|| {
        fs::create_dir(&stage)?;
        if canonical_root.exists() {
            for entry in fs::read_dir(canonical_root)? {
                let entry = entry?;
                let source = entry.path();
                if ccync_foundation::platform::is_symlink_or_junction(&source) {
                    return Err(InstallError::Io(std::io::Error::other(format!(
                        "canonical root contains a symlink or junction: {}",
                        source.display()
                    ))));
                }
                // Provider generations are a separate immutable lifecycle
                // store nested under render/. Move them at swap time instead
                // of copying their potentially large payloads with the root.
                if entry.file_name() == "providers" {
                    continue;
                }
                let target = stage.join(entry.file_name());
                if entry.file_type()?.is_dir() {
                    copy_dir_all(&source, &target)?;
                } else {
                    fs::copy(&source, &target)?;
                }
            }
        }
        Ok(stage.clone())
    })();

    if result.is_err() {
        let _ = fs::remove_dir_all(&stage);
    }
    result
}

/// Map of personal plugin id → resolved live sha from the lockfile
/// (`pin` / `pinnedSha` / `sha`). Empty when the lockfile is absent/unparseable.
fn personal_pins() -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Some(path) = plugins_lockfile_path() else {
        return map;
    };
    let Ok(contents) = fs::read_to_string(&path) else {
        return map;
    };
    let Ok(lock) = serde_json::from_str::<Value>(&contents) else {
        return map;
    };
    if let Some(arr) = lock.get("_personalPlugins").and_then(Value::as_array) {
        for e in arr {
            let id = e.get("pluginId").and_then(Value::as_str);
            let sha = ccync_foundation::pins::entry_pin(e);
            if let (Some(id), Some(sha)) = (id, sha) {
                map.insert(id.to_string(), sha);
            }
        }
    }
    map
}

/// Materialize the ccync managed set into the canonical root: copy each managed
/// plugin's `skills/`, `commands/`, `agents/`, `hooks/` subtrees and merge its
/// `.mcp.json` servers into `<canonical_root>/.mcp.json`. Skills use
/// first-directory-wins on a child-name collision; the other components retain
/// bulk-copy behavior. MCP servers also use first-plugin-wins on a name
/// collision (caller controls precedence via `plugin_dirs` order).
///
/// Returns one deterministic collision warning per duplicate skill child or
/// server name, naming the retained and skipped plugin (labels only, never
/// server definitions). The caller prints them; returning them keeps the
/// warning contract directly testable.
///
/// This materializes the canonical-root content, so the Claude skill symlink
/// (and the other read surfaces) point at a real, non-empty tree instead of
/// an empty directory.
#[derive(Debug, Clone)]
pub(crate) struct StandaloneSkillSource {
    pub path: Option<PathBuf>,
    pub source_id: String,
    pub revision: Result<String, String>,
}

impl StandaloneSkillSource {
    fn blocked(reason: &str) -> Self {
        Self {
            path: None,
            source_id: "unresolved-origin".into(),
            revision: Err(reason.into()),
        }
    }
}

/// Content proof is computed from the actual winning subtree, never merged render.
fn source_content_revision(source: &Path, loose_skill: bool) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    // Projection owns the link boundary and cycle policy. Check before reading.
    let complete = projection::standalone_source_fingerprint(source).map_err(|e| e.to_string())?;
    fn visit(
        boundary: &Path,
        path: &Path,
        relative: &Path,
        hash: &mut Sha256,
        stack: &mut BTreeSet<PathBuf>,
        loose_skill: bool,
    ) -> Result<(), String> {
        let resolved = fs::canonicalize(path).map_err(|error| error.to_string())?;
        if !resolved.starts_with(boundary) || !stack.insert(resolved.clone()) {
            return Err("source changed its link boundary while proving content".into());
        }
        let label = relative.to_string_lossy().replace('\\', "/");
        hash.update((label.len() as u64).to_le_bytes());
        hash.update(label.as_bytes());
        if path.is_dir() {
            hash.update(b"D");
            let mut entries = fs::read_dir(path)
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let entry_path = entry.path();
                let entry_relative = relative.join(entry.file_name());
                if loose_skill
                    && !loose_skill_path_included(
                        &entry_relative,
                        entry.file_type().map_err(|e| e.to_string())?.is_dir(),
                    )
                {
                    continue;
                }
                visit(
                    boundary,
                    &entry_path,
                    &entry_relative,
                    hash,
                    stack,
                    loose_skill,
                )?;
            }
        } else {
            let bytes = fs::read(path).map_err(|e| e.to_string())?;
            hash.update(b"F");
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
        stack.remove(&resolved);
        Ok(())
    }
    let mut hash = Sha256::new();
    if !loose_skill {
        hash.update(complete.as_bytes());
    }
    let boundary = fs::canonicalize(source).map_err(|error| error.to_string())?;
    visit(
        &boundary,
        source,
        Path::new(""),
        &mut hash,
        &mut BTreeSet::new(),
        loose_skill,
    )?;
    if projection::standalone_source_fingerprint(source).map_err(|e| e.to_string())? != complete {
        return Err("source changed while proving content".into());
    }
    Ok(format!("sha256:{:x}", hash.finalize()))
}

fn plugin_skill_source(id: &str, plugin: &Path, source: &Path) -> StandaloneSkillSource {
    let pin = plugins_lockfile_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|lock| {
            ["resolvedPlugins", "_personalPlugins"]
                .into_iter()
                .flat_map(|key| {
                    lock.get(key)
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                })
                .find(|row| row.get("pluginId").and_then(Value::as_str) == Some(id))
                .and_then(ccync_foundation::pins::entry_pin)
        });
    StandaloneSkillSource {
        path: Some(source.to_path_buf()),
        source_id: format!("plugin:{id}:{}", legacy_source_path_key(plugin)),
        revision: source_content_revision(source, false).and_then(|digest| {
            pin.map(|pin| json!({"kind": "plugin-pin", "pin": pin, "digest": digest}).to_string())
                .ok_or_else(|| "winning plugin has no recorded pin".into())
        }),
    }
}

/// Standalone updates preserve every other journal collection and cutover field.
pub(crate) fn record_standalone_intent(
    intent: ccync_foundation::provider::StandalonePublicationIntent,
) -> Result<(), InstallError> {
    intent
        .validate()
        .map_err(|e| invalid_lifecycle_state(e.to_string()))?;
    let path = provider_journal_path().ok_or(InstallError::NoHome)?;
    let mut journal = read_standalone_journal()?;
    if journal.standalone_intents.iter().any(|prior| {
        prior.operation_id != intent.operation_id && prior.destination == intent.destination
    }) {
        return Err(invalid_lifecycle_state(
            "destination already has a pending standalone intent",
        ));
    }
    if journal
        .standalone_withdrawal_intents
        .iter()
        .any(|prior| prior.destination == intent.destination)
    {
        return Err(invalid_lifecycle_state(
            "destination already has a pending standalone withdrawal",
        ));
    }
    journal
        .standalone_intents
        .retain(|prior| prior.operation_id != intent.operation_id);
    journal.standalone_intents.push(intent);
    write_lifecycle_journal(&path, &journal)
}

/// Withdrawal updates preserve every other journal collection and cutover field.
pub(crate) fn record_withdrawal_intent(
    intent: ccync_foundation::provider::StandaloneWithdrawalIntent,
) -> Result<(), InstallError> {
    intent
        .validate()
        .map_err(|e| invalid_lifecycle_state(e.to_string()))?;
    let path = provider_journal_path().ok_or(InstallError::NoHome)?;
    let mut journal = read_standalone_journal()?;
    if journal
        .standalone_intents
        .iter()
        .any(|prior| prior.destination == intent.destination)
        || journal.standalone_withdrawal_intents.iter().any(|prior| {
            prior.operation_id != intent.operation_id && prior.destination == intent.destination
        })
    {
        return Err(invalid_lifecycle_state(
            "destination already has a pending standalone operation",
        ));
    }
    journal
        .standalone_withdrawal_intents
        .retain(|prior| prior.operation_id != intent.operation_id);
    journal.standalone_withdrawal_intents.push(intent);
    write_lifecycle_journal(&path, &journal)
}

/// Clear exactly the matching withdrawal and leave all other pending state.
pub(crate) fn clear_withdrawal_intent(
    intent: &ccync_foundation::provider::StandaloneWithdrawalIntent,
) -> Result<(), InstallError> {
    let path = provider_journal_path().ok_or(InstallError::NoHome)?;
    let mut journal = read_standalone_journal()?;
    journal
        .standalone_withdrawal_intents
        .retain(|prior| prior != intent);
    write_lifecycle_journal(&path, &journal)
}

/// Source identities that are still desired, or `None` when the lockfile cannot
/// prove the complete desired set. Absence from a proven set is the only
/// evidence that a source was removed on purpose.
pub(crate) fn desired_standalone_origins() -> Option<Vec<String>> {
    let path = plugins_lockfile_path()?;
    let lock: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    desired_standalone_origins_from_lock(&lock)
}

fn desired_standalone_origins_from_lock(lock: &Value) -> Option<Vec<String>> {
    let mut origins = Vec::new();
    for key in ["resolvedPlugins", "_personalPlugins"] {
        let rows = match lock.get(key) {
            None => continue,
            Some(Value::Array(rows)) => rows,
            Some(_) => return None,
        };
        for row in rows {
            let id = row.get("pluginId").and_then(Value::as_str)?;
            origins.push(format!("plugin:{id}:"));
        }
    }
    let loose_rows = match lock.get("_looseSkills") {
        None => &[][..],
        Some(Value::Array(rows)) => rows.as_slice(),
        Some(_) => return None,
    };
    for row in loose_rows {
        let skill = serde_json::from_value::<crate::truth::LooseSkill>(row.clone()).ok()?;
        if let Some(origin) = skill.origin {
            origins.push(format!("loose-origin:{}", legacy_source_path_key(&origin)));
        }
    }
    Some(origins)
}

/// Reject a skill subtree whose links escape or cycle before anything copies it.
fn check_source_links(source: &Path) -> Result<(), InstallError> {
    projection::standalone_source_fingerprint(source)
        .map(|_| ())
        .map_err(|error| InstallError::Io(std::io::Error::other(error.to_string())))
}

pub(crate) fn read_standalone_journal() -> Result<ProviderLifecycleJournal, InstallError> {
    let path = provider_journal_path().ok_or(InstallError::NoHome)?;
    match fs::read_to_string(path) {
        Ok(raw) => ProviderLifecycleJournal::from_json_str(&raw)
            .map_err(|e| invalid_lifecycle_state(e.to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(ProviderLifecycleJournal::default())
        }
        Err(error) => Err(InstallError::Io(error)),
    }
}

pub(crate) fn clear_standalone_intent(
    intent: &ccync_foundation::provider::StandalonePublicationIntent,
) -> Result<(), InstallError> {
    let path = provider_journal_path().ok_or(InstallError::NoHome)?;
    let mut journal = read_standalone_journal()?;
    journal.standalone_intents.retain(|prior| prior != intent);
    write_lifecycle_journal(&path, &journal)
}

#[cfg(test)]
fn render_canonical_root(
    canonical_root: &Path,
    plugin_dirs: &[(String, PathBuf)],
) -> Result<Vec<String>, InstallError> {
    render_canonical_root_with_sources(canonical_root, plugin_dirs).map(|(warnings, _)| warnings)
}

fn render_canonical_root_with_sources(
    canonical_root: &Path,
    plugin_dirs: &[(String, PathBuf)],
) -> Result<(Vec<String>, BTreeMap<String, StandaloneSkillSource>), InstallError> {
    validate_standalone_source_collections()?;
    // Validate every plugin `.mcp.json` before staging: a corrupt file must
    // abort the render with the canonical root untouched. Absent files skip
    // silently; existing-but-corrupt files fail closed naming the path.
    //
    // Plugin identity is kept as a merge-source label so a cross-plugin
    // server-name collision produces a warning naming both plugins instead of
    // being silently collapsed before generate_managed_mcp can see it.
    let mut mcp_sources: Vec<(String, serde_json::Map<String, Value>)> = Vec::new();
    let mut helper_warnings = Vec::new();
    for (plugin_id, plugin) in plugin_dirs {
        if let Some(read) = read_plugin_mcp_servers(plugin_id, plugin)? {
            for dir in &read.data_dirs {
                fs::create_dir_all(dir)?;
            }
            helper_warnings.extend(read.warnings);
            let label = plugin_id;
            mcp_sources.push((format!("plugin {label}"), read.servers));
        }
    }
    let (merged_servers, mut warnings) = merge_mcp_sources_with_warnings(mcp_sources);
    warnings.extend(helper_warnings);

    let staging = stage_canonical_render(canonical_root)?;
    let result = (|| {
        // Clear render-owned entries in the staged copy before re-rendering.
        for component in COMPONENT_DIRS {
            let dir = staging.join(component);
            if dir.is_dir() {
                fs::remove_dir_all(&dir)?;
            }
        }
        let mcp_path = staging.join(".mcp.json");
        if mcp_path.is_file() {
            fs::remove_file(&mcp_path)?;
        }

        let mut skill_owners = std::collections::HashMap::new();
        let mut sources = BTreeMap::new();
        for (plugin_id, plugin) in plugin_dirs {
            let plugin_label = plugin_id.clone();
            match payload_shape(plugin) {
                PayloadShape::PluginRoot => {
                    let skills_src = plugin.join("skills");
                    if skills_src.is_dir() {
                        let skills_dest = staging.join("skills");
                        for entry in fs::read_dir(&skills_src)? {
                            let entry = entry?;
                            let name = entry.file_name().to_string_lossy().into_owned();
                            let dest = skills_dest.join(&name);
                            if dest.exists() {
                                if let Some(retained) = skill_owners.get(&name) {
                                    warnings.push(format!(
                                        "skill '{name}': keeping directory from plugin {retained}, \
                                         ignoring duplicate from plugin {plugin_label}"
                                    ));
                                }
                                continue;
                            }
                            // Links are proven before any byte is copied, so an
                            // escaping or cyclic subtree aborts with the live root intact.
                            if entry.path().is_dir() {
                                check_source_links(&entry.path())?;
                            }
                            // Every child is carried across, not just directories.
                            if entry.path().is_dir() {
                                copy_dir_all(&entry.path(), &dest)?;
                            } else {
                                fs::create_dir_all(&skills_dest)?;
                                fs::copy(entry.path(), &dest)?;
                            }
                            if entry.path().is_dir() {
                                sources.insert(
                                    name.clone(),
                                    plugin_skill_source(plugin_id, plugin, &entry.path()),
                                );
                            }
                            skill_owners.insert(name, plugin_label.clone());
                        }
                    }

                    for component in COMPONENT_DIRS.iter().filter(|c| **c != "skills") {
                        let src = plugin.join(component);
                        if src.is_dir() {
                            copy_dir_all(&src, &staging.join(component))?;
                        }
                    }
                }
                PayloadShape::BareSkill(name) => {
                    let dest = staging.join("skills").join(&name);
                    if dest.exists() {
                        if let Some(retained) = skill_owners.get(&name) {
                            warnings.push(format!(
                                "skill '{name}': keeping directory from plugin {retained}, \
                                 ignoring duplicate from plugin {plugin_label}"
                            ));
                        }
                        continue;
                    }

                    check_source_links(plugin)?;
                    fs::create_dir_all(&dest)?;
                    for entry in fs::read_dir(plugin)? {
                        let entry = entry?;
                        if entry.file_name() == ".git" {
                            continue;
                        }
                        let target = dest.join(entry.file_name());
                        if entry.file_type()?.is_dir() {
                            copy_dir_all(&entry.path(), &target)?;
                        } else {
                            fs::copy(entry.path(), target)?;
                        }
                    }
                    sources.insert(name.clone(), plugin_skill_source(plugin_id, plugin, plugin));
                    skill_owners.insert(name, plugin_label.clone());
                }
                PayloadShape::Unrecognized => {}
            }
        }
        let (tool_warnings, tool_sources) =
            crate::tool_manager::render::materialize_tool_snapshots_with_sources(&staging)
                .map_err(|error| InstallError::Io(std::io::Error::other(error.to_string())))?;
        warnings.extend(tool_warnings);
        for source in tool_sources {
            sources.insert(
                source.name,
                StandaloneSkillSource {
                    path: Some(source.path),
                    source_id: source.source_id,
                    revision: Ok(source.revision),
                },
            );
        }
        let (loose_skills, loose_warnings) = load_loose_skills();
        warnings.extend(loose_warnings);
        warnings.extend(materialize_loose_skills_with_sources(
            &staging,
            &loose_skills,
            &mut sources,
        )?);
        if !merged_servers.is_empty() {
            write_json_file(
                &staging.join(".mcp.json"),
                &json!({ "servers": Value::Object(merged_servers) }),
            )?;
        }
        write_json_file(
            &staging.join(".claude-plugin").join("plugin.json"),
            &json!({
                "name": "ccync",
                "description": "ccync — cross-agent plugin / MCP / skills manager"
            }),
        )?;

        write_root_plugin_manifest(&staging)?;
        let providers = canonical_root.join("providers");
        if providers.is_dir() && !staging.join("providers").exists() {
            fs::rename(&providers, staging.join("providers"))?;
        }
        ccync_foundation::platform::atomic_swap(&staging, canonical_root)
            .map_err(|error| InstallError::Io(std::io::Error::other(error.to_string())))?;
        Ok((warnings, sources))
    })();

    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

/// A present source collection must be an array before render can prove that
/// sources were intentionally removed. Missing collections remain valid empty
/// state for older lockfiles.
fn validate_standalone_source_collections() -> Result<(), InstallError> {
    let Some(path) = plugins_lockfile_path() else {
        return Ok(());
    };
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(InstallError::Io(error)),
    };
    let lock: Value = serde_json::from_str(&text)?;
    for key in ["_looseSkills", "resolvedPlugins", "_personalPlugins"] {
        if lock.get(key).is_some_and(|value| !value.is_array()) {
            return Err(invalid_lifecycle_state(format!(
                "lockfile source collection {key} must be an array"
            )));
        }
    }
    Ok(())
}

/// Write the canonical root's root-level `plugin.json`, conforming to the
/// Agent Plugins 1.0.0 specification (`$schema`, `name`, and `description`).
/// The explicit `skills` list was dropped per Antigravity Probe A: Antigravity's
/// plugin loader and validator discover skills by scanning the `skills/`
/// directory directly, and the 1.0.0 schema does not permit an unknown `skills`
/// key.
fn write_root_plugin_manifest(canonical_root: &Path) -> Result<(), InstallError> {
    write_json_file(
        &canonical_root.join("plugin.json"),
        &json!({
            "$schema": "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json",
            "name": "ccync",
            "description": "ccync — cross-agent plugin / MCP / skills manager"
        }),
    )
}

/// Loose-skill rows recorded in the lockfile `_looseSkills` namespace (written
/// by `adopt::adopt_loose_skills` during `ccync init`). Returns an empty list
/// when the lockfile is absent or unreadable — fail-safe, matching every other
/// lockfile reader in this module. A row that cannot be decoded yields a
/// diagnostic instead of disappearing, so the rest of the namespace still loads.
fn load_loose_skills() -> (Vec<crate::truth::LooseSkill>, Vec<String>) {
    let mut skills = Vec::new();
    let mut diagnostics = Vec::new();
    let Some(path) = plugins_lockfile_path() else {
        return (skills, diagnostics);
    };
    let Ok(contents) = fs::read_to_string(&path) else {
        return (skills, diagnostics);
    };
    let Ok(lock) = serde_json::from_str::<Value>(&contents) else {
        return (skills, diagnostics);
    };
    let Some(rows) = lock.get("_looseSkills").and_then(Value::as_array) else {
        return (skills, diagnostics);
    };
    for (index, row) in rows.iter().enumerate() {
        match serde_json::from_value::<crate::truth::LooseSkill>(row.clone()) {
            Ok(skill) => skills.push(skill),
            Err(error) => diagnostics.push(format!(
                "loose skill row {index} is malformed ({error}); rescan the import source — skipped"
            )),
        }
    }
    (skills, diagnostics)
}

/// Materialize every `_looseSkills` entry into `<canonical_root>/skills/<name>`.
///
/// Loose skills are not owned by any managed plugin — their real content lives
/// directly under a master agent's own skill root (`claude_skills_path()` or
/// `shared_agents_skills_path()`). This runs immediately after
/// [`render_canonical_root`], which already pruned and recreated `skills/` in
/// staging for this pass, so a loose skill whose lockfile entry was removed by
/// an un-adopt is not re-copied and re-runs stay idempotent.
///
/// Malformed lockfile rows are reported as warnings and skipped. Valid rows are
/// still materialized. See [`materialize_loose_skills_from`] for the group rules.
#[cfg(test)]
fn materialize_loose_skills(canonical_root: &Path) -> Result<Vec<String>, InstallError> {
    let (skills, mut warnings) = load_loose_skills();
    warnings.extend(materialize_loose_skills_from(canonical_root, &skills)?);
    Ok(warnings)
}

/// Materialize loose skills, grouped by name before any destination is touched.
///
/// Identity is the exact physical origin. Within one name:
///
/// - One distinct identity with a verified directory writes exactly one copy.
///   Repeated rows for the same identity dedupe, whatever their source label.
/// - Two or more distinct identities form a conflict group. Every member is
///   blocked, nothing is written, and one warning names every origin. The
///   result does not depend on input order.
/// - A row without a recorded origin, or whose directory is gone, is a
///   rescan-required outcome and writes nothing.
/// - A destination that already holds a plugin-owned skill is kept. A warning
///   reports the collision.
///
/// Unrelated names are unaffected. Copy failures abort the render so a partially
/// materialized skill cannot be swapped into place.
pub fn materialize_loose_skills_from(
    canonical_root: &Path,
    skills: &[crate::truth::LooseSkill],
) -> Result<Vec<String>, InstallError> {
    materialize_loose_skills_with_sources(canonical_root, skills, &mut BTreeMap::new())
}

fn materialize_loose_skills_with_sources(
    canonical_root: &Path,
    skills: &[crate::truth::LooseSkill],
    sources: &mut BTreeMap<String, StandaloneSkillSource>,
) -> Result<Vec<String>, InstallError> {
    let mut warnings = Vec::new();
    let skills_root = canonical_root.join("skills");

    let mut groups: BTreeMap<&str, Vec<&crate::truth::LooseSkill>> = BTreeMap::new();
    for skill in skills {
        groups.entry(skill.name.as_str()).or_default().push(skill);
    }

    for (name, members) in groups {
        let identities: BTreeSet<Option<&Path>> = members
            .iter()
            .map(|skill| skill.origin.as_deref())
            .collect();
        if identities.len() > 1 {
            let reason = loose_conflict_warning(name, &members);
            sources
                .entry(name.to_string())
                .or_insert_with(|| StandaloneSkillSource::blocked(&reason));
            warnings.push(reason);
            continue;
        }
        let verified = identities
            .into_iter()
            .next()
            .flatten()
            .filter(|path| path.is_dir());
        let Some(src) = verified else {
            sources.entry(name.to_string()).or_insert_with(|| {
                StandaloneSkillSource::blocked("recorded loose origin unavailable")
            });
            warnings.push(format!(
                "skill '{name}' has no available recorded origin; rescan the import source — skipped"
            ));
            continue;
        };

        let dest = skills_root.join(name);
        check_source_links(src)?;
        if dest.exists() {
            // Identical content is this same identity written earlier in the pass,
            // so there is nothing to report. Different content is a plugin-owned
            // skill, which wins and is reported.
            if !tree_matches(src, &dest)? {
                warnings.push(format!(
                    "skill '{name}' from {} was not written: a plugin-owned skill already occupies {}; plugin content wins",
                    src.display(),
                    dest.display()
                ));
            }
            continue;
        }

        copy_loose_skill_dir(src, &dest)?;
        sources.insert(
            name.to_string(),
            StandaloneSkillSource {
                path: Some(src.to_path_buf()),
                source_id: format!("loose-origin:{}", legacy_source_path_key(src)),
                revision: source_content_revision(src, true)
                    .map(|digest| json!({"kind": "loose-origin", "digest": digest}).to_string()),
            },
        );
    }

    Ok(warnings)
}

/// Describe every distinct member of a conflict group, sorted so the report is
/// identical for any input order. Each line names the adoption source and the
/// recorded origin, or says that no origin was recorded.
fn loose_conflict_warning(name: &str, members: &[&crate::truth::LooseSkill]) -> String {
    let mut described: Vec<String> = members
        .iter()
        .map(|skill| {
            let source = skill.source.as_deref().unwrap_or("unknown source");
            let origin = skill.origin.as_deref().map_or_else(
                || "no recorded origin".to_string(),
                |path| path.display().to_string(),
            );
            format!("{source} {origin}")
        })
        .collect();
    described.sort();
    described.dedup();
    format!(
        "skill '{name}' has conflicting recorded origins; none was materialized — {}",
        described.join("; ")
    )
}

/// True when `dest` already holds exactly the files and directories of `src`,
/// compared byte for byte. A missing or differently shaped `dest` does not match.
fn loose_skill_path_included(relative: &Path, is_dir: bool) -> bool {
    if relative
        .components()
        .any(|component| component.as_os_str() == ".git")
    {
        return false;
    }
    if is_dir {
        return true;
    }
    let Some(name) = relative.file_name().and_then(OsStr::to_str) else {
        return true;
    };
    ![".bak", ".orig", "~"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

fn copy_loose_skill_dir(src: &Path, dst: &Path) -> Result<(), InstallError> {
    fn copy(src: &Path, dst: &Path, relative: &Path) -> Result<(), InstallError> {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let source_path = entry.path();
            let entry_relative = relative.join(entry.file_name());
            let is_dir = entry.file_type()?.is_dir();
            if !loose_skill_path_included(&entry_relative, is_dir) {
                continue;
            }
            let target_path = dst.join(entry.file_name());
            if is_dir {
                copy(&source_path, &target_path, &entry_relative)?;
            } else {
                fs::copy(&source_path, &target_path)?;
            }
        }
        Ok(())
    }
    copy(src, dst, Path::new(""))
}

fn tree_matches(src: &Path, dest: &Path) -> std::io::Result<bool> {
    if !dest.exists() || src.is_dir() != dest.is_dir() {
        return Ok(false);
    }
    if !src.is_dir() {
        return Ok(match fs::read(dest) {
            Ok(bytes) => fs::read(src)? == bytes,
            Err(_) => false,
        });
    }
    let filtered_entries = |path: &Path| -> std::io::Result<Vec<fs::DirEntry>> {
        fs::read_dir(path)?
            .filter_map(|entry| match entry {
                Ok(entry) => {
                    let file_name = entry.file_name();
                    let relative = Path::new(file_name.as_os_str());
                    let is_dir = entry.file_type().map(|kind| kind.is_dir());
                    match is_dir {
                        Ok(is_dir) if loose_skill_path_included(relative, is_dir) => {
                            Some(Ok(entry))
                        }
                        Ok(_) => None,
                        Err(error) => Some(Err(error)),
                    }
                }
                Err(error) => Some(Err(error)),
            })
            .collect()
    };
    let src_entries = filtered_entries(src)?;
    if src_entries.len() != filtered_entries(dest)?.len() {
        return Ok(false);
    }
    for entry in src_entries {
        if !tree_matches(&entry.path(), &dest.join(entry.file_name()))? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Perform the real Claude lifecycle work — skill projection apply,
/// marketplace manifest write, runtime-state JSON write — mirroring the
/// `claude` branch of the retired legacy lifecycle-artifact chain byte-for-byte
/// (same constructors, same JSON shape). Returns the paths written as
/// `Written`-change evidence on success; on failure returns the path the
/// failing step was writing to (`None` when the failure happened before any
/// path was known, e.g. resolving `canonical_root`-derived paths) alongside
/// the underlying error, so the caller can build an owned-path
/// `ProjectionExpectedFailure` for `ProjectionTargetId::Claude`.
pub fn apply_claude_lifecycle(
    canonical_root: &Path,
    enabled_bundles: &[(String, String)],
) -> Result<Vec<projection::ProjectionPathResult>, (Option<PathBuf>, InstallError)> {
    apply_claude_lifecycle_with(canonical_root, enabled_bundles, true)
}

/// Apply the Claude lifecycle. When `legacy_skill_link` is false, verified
/// native provider packages have replaced the combined skill surface for
/// every consumer, so the owned `~/.claude/skills/ccync` link is removed
/// instead of created. `remove` only deletes the link, never a real directory.
fn apply_claude_lifecycle_with(
    canonical_root: &Path,
    enabled_bundles: &[(String, String)],
    legacy_skill_link: bool,
) -> Result<Vec<projection::ProjectionPathResult>, (Option<PathBuf>, InstallError)> {
    let claude_projection = ClaudeSkillProjection::new(canonical_root.to_path_buf())
        .map_err(|e| (None, InstallError::Io(std::io::Error::other(e.to_string()))))?;
    if legacy_skill_link {
        claude_projection
            .apply()
            .map_err(|e| (None, InstallError::Io(std::io::Error::other(e.to_string()))))?;
    }

    let marketplace_path = claude_marketplace_manifest_path().map_err(|e| (None, e))?;
    let mut claude_plugins = Vec::new();
    if legacy_skill_link {
        claude_plugins.push(json!({
            "name": "ccync",
            "source": canonical_root,
            "description": "ccync — cross-agent plugin / MCP / skills manager"
        }));
    }
    for (bundle_id, bundle_src) in enabled_bundles {
        claude_plugins.push(json!({
            "name": bundle_id,
            "source": format!("./{bundle_src}"),
            "description": format!("ccync domain bundle: {bundle_id}")
        }));
    }
    write_json_file(
        &marketplace_path,
        &json!({
            "name": "ccync",
            "owner": { "name": "ccync" },
            "description": "ccync — cross-agent plugin / MCP / skills manager",
            "plugins": claude_plugins
        }),
    )
    .map_err(|e| (Some(marketplace_path.clone()), e))?;

    let state_path = target_state_path("claude").map_err(|e| (None, e))?;
    write_json_file(
        &state_path,
        &json!({
            "schemaVersion": 1,
            "target": "claude",
            "canonicalRoot": canonical_root,
            "packageOutputRoot": canonical_root,
            "projectionRoot": claude_projection.skill_path(),
            "installTarget": claude_projection.skill_path(),
            "manifestPath": canonical_root.join(".claude-plugin").join("plugin.json"),
            "generatedAt": chrono::Utc::now().to_rfc3339(),
            "status": "linked-projection",
            "readSurface": "linked-projection",
            "cli": {
                "available": false,
                "validateSupported": false,
                "localArtifactInstallSupported": false,
                "installScopeSupported": false,
                "installHelpSummary": null
            },
            "validation": {
                "command": "claude plugin validate <plugin-root> --strict",
                "strictPassed": false
            },
            "lifecycle": {
                "mode": "session-load-only",
                "stagedPluginRoot": claude_projection.skill_path(),
                "marketplaceName": "ccync",
                "sessionLoadCommand": "claude --plugin-dir <plugin-root>",
                "installCommandTemplate": "claude plugin install <plugin> --scope <scope>",
                "updateCommandTemplate": "claude plugin update <plugin> --scope <scope>",
                "uninstallCommandTemplate": "claude plugin uninstall <plugin> --scope <scope>"
            }
        }),
    )
    .map_err(|e| (Some(state_path.clone()), e))?;

    // The link goes last. An earlier failure leaves the combined surface intact.
    if !legacy_skill_link {
        claude_projection
            .remove()
            .map_err(|e| (None, InstallError::Io(std::io::Error::other(e.to_string()))))?;
    }

    Ok(vec![
        projection::ProjectionPathResult {
            path: marketplace_path,
            change: projection::ProjectionPathChange::Written,
        },
        projection::ProjectionPathResult {
            path: state_path,
            change: projection::ProjectionPathChange::Written,
        },
    ])
}

/// Perform the real Copilot lifecycle work — projection sync, runtime-state
/// JSON write — mirroring the `copilot` branch of the retired legacy
/// lifecycle-artifact chain byte-for-byte. Same success/failure evidence
/// shape as [`apply_claude_lifecycle`], scoped to `ProjectionTargetId::Copilot`.
pub fn apply_copilot_lifecycle(
    canonical_root: &Path,
) -> Result<Vec<projection::ProjectionPathResult>, (Option<PathBuf>, InstallError)> {
    let (projection_root, refreshed_copy_to_host, version_bumped_to) =
        sync_copilot_projection(canonical_root).map_err(|e| (None, e))?;
    let status = if refreshed_copy_to_host {
        "refreshed-copy2-host"
    } else {
        "linked-projection"
    };
    let state_path = target_state_path("copilot").map_err(|e| (None, e))?;
    write_json_file(
        &state_path,
        &json!({
            "schemaVersion": 1,
            "target": "copilot",
            "canonicalRoot": canonical_root,
            "packageOutputRoot": canonical_root,
            "projectionRoot": projection_root,
            "installTarget": projection_root,
            "manifestPath": canonical_root.join("copilot-manifest.json"),
            "generatedAt": chrono::Utc::now().to_rfc3339(),
            "status": status,
            "readSurface": status,
            "cli": {
                "available": false,
                "validateSupported": false,
                "localArtifactInstallSupported": false,
                "marketplaceInstallSupported": false,
                "installScopeSupported": false,
                "installHelpSummary": null
            },
            "validation": {
                "command": null,
                "strictPassed": false
            },
            "lifecycle": {
                "mode": "artifact-only",
                "stagedPluginRoot": projection_root,
                "refreshedCopyToHost": refreshed_copy_to_host,
                "versionBumpedTo": version_bumped_to,
                "sessionLoadCommand": "GitHub Copilot reads the projected plugin from the copilot staged-plugin directory",
                "installCommandTemplate": "gh copilot plugin install <plugin-root>",
                "updateCommandTemplate": "gh copilot plugin update <plugin-id>",
                "uninstallCommandTemplate": "gh copilot plugin uninstall <plugin-id>"
            }
        }),
    )
    .map_err(|e| (Some(state_path.clone()), e))?;

    Ok(vec![projection::ProjectionPathResult {
        path: state_path,
        change: projection::ProjectionPathChange::Written,
    }])
}

/// Perform the real Codex lifecycle work — marketplace manifest write,
/// runtime-state JSON write — mirroring the `codex` branch of the retired
/// legacy lifecycle-artifact chain byte-for-byte (same JSON shape, same
/// paths). Same success/failure evidence shape as [`apply_claude_lifecycle`],
/// scoped to `ProjectionTargetId::Codex`.
pub fn apply_codex_lifecycle(
    canonical_root: &Path,
    enabled_bundles: &[(String, String)],
) -> Result<Vec<projection::ProjectionPathResult>, (Option<PathBuf>, InstallError)> {
    apply_codex_lifecycle_with(canonical_root, enabled_bundles, true)
}

/// Apply the Codex lifecycle. When `combined_entry` is false, the combined
/// `ccync` marketplace entry is omitted because direct Codex surfaces replaced it.
fn apply_codex_lifecycle_with(
    canonical_root: &Path,
    enabled_bundles: &[(String, String)],
    combined_entry: bool,
) -> Result<Vec<projection::ProjectionPathResult>, (Option<PathBuf>, InstallError)> {
    let marketplace_path = codex_marketplace_manifest_path().map_err(|e| (None, e))?;
    let mut codex_plugins = Vec::new();
    if combined_entry {
        codex_plugins.push(json!({
            "name": "ccync",
            "source": { "source": "local", "path": canonical_root },
            "policy": { "installation": "AVAILABLE", "authentication": "ON_INSTALL" },
            "category": "Engineering"
        }));
    }
    for (bundle_id, bundle_src) in enabled_bundles {
        codex_plugins.push(json!({
            "name": bundle_id,
            "source": { "source": "local", "path": format!("./{bundle_src}") },
            "policy": { "installation": "AVAILABLE", "authentication": "ON_INSTALL" },
            "category": "Engineering"
        }));
    }
    write_json_file(
        &marketplace_path,
        &json!({
            "name": "ccync-marketplace",
            "interface": { "displayName": "ccync Plugin Marketplace" },
            "plugins": codex_plugins
        }),
    )
    .map_err(|e| (Some(marketplace_path.clone()), e))?;

    let state_path = target_state_path("codex").map_err(|e| (None, e))?;
    write_json_file(
        &state_path,
        &json!({
            "schemaVersion": 1,
            "target": "codex",
            "canonicalRoot": canonical_root,
            "packageOutputRoot": canonical_root,
            "projectionRoot": null,
            "installTarget": "ccync@ccync-marketplace",
            "manifestPath": canonical_root.join(".codex-plugin").join("plugin.json"),
            "generatedAt": chrono::Utc::now().to_rfc3339(),
            "status": "unprojected-artifact",
            "readSurface": "unprojected-artifact",
            "cli": {
                "available": false,
                "validateSupported": false,
                "localArtifactInstallSupported": false,
                "marketplaceInstallSupported": false,
                "installScopeSupported": false,
                "installHelpSummary": null
            },
            "validation": {
                "command": null,
                "strictPassed": false
            },
            "lifecycle": {
                "mode": "artifact-only",
                "stagedPluginRoot": canonical_root,
                "marketplaceRoot": crate::paths::ccync_plugins_root(),
                "marketplaceManifestPath": marketplace_path,
                "marketplaceName": "ccync-marketplace",
                "installedSelector": "ccync@ccync-marketplace",
                "sessionLoadCommand": "codex plugin marketplace add <plugins-root> ; codex plugin add ccync@ccync-marketplace",
                "installCommandTemplate": "codex plugin marketplace add <plugins-root>",
                "updateCommandTemplate": "codex plugin remove ccync@ccync-marketplace ; codex plugin add ccync@ccync-marketplace",
                "uninstallCommandTemplate": CODEX_MANUAL_REMOVAL_HINT,
                "pluginRemovedBeforeAdd": false
            }
        }),
    )
    .map_err(|e| (Some(state_path.clone()), e))?;

    Ok(vec![
        projection::ProjectionPathResult {
            path: marketplace_path,
            change: projection::ProjectionPathChange::Written,
        },
        projection::ProjectionPathResult {
            path: state_path,
            change: projection::ProjectionPathChange::Written,
        },
    ])
}

/// Perform the real AGY runtime-state write — a single JSON descriptor at
/// `target_state_path("agy")` — mirroring the retired legacy lifecycle-artifact
/// chain's own AGY branch byte-for-byte (same JSON shape, same path).
///
/// This is deliberately distinct from [`apply_agy_evidence`]: that function
/// owns the AGY CLI/IDE *plugin-junction* projection (`AgyProjection::apply`),
/// gated individually on `agy-cli`/`agy-ide`. This write is gated by
/// `any_agy_face_selected`, which is `true` whenever *any* of
/// `agy-cli`/`agy-ide`/`agy-gui` is selected — independent of which specific
/// face(s) the junction step actually applied, and reachable even when the
/// junction step ran nothing at all (e.g. only `agy-gui` selected). It has no
/// single owning `ProjectionTargetId` of its own for that reason.
pub fn apply_agy_runtime_state(
    canonical_root: &Path,
) -> Result<Vec<projection::ProjectionPathResult>, (Option<PathBuf>, InstallError)> {
    let state_path = target_state_path("agy").map_err(|e| (None, e))?;
    write_json_file(
        &state_path,
        &json!({
            "schemaVersion": 1,
            "target": "agy",
            "canonicalRoot": canonical_root,
            "packageOutputRoot": canonical_root,
            "projectionRoot": crate::paths::ccync_active_provider_path("agy"),
            "installTarget": crate::paths::user_home().map(|home| home.join(".gemini").join("antigravity-cli").join("plugins").join("ccync")),
            "shortcutTarget": crate::paths::ccync_active_provider_path("agy"),
            "generatedAt": chrono::Utc::now().to_rfc3339(),
            "status": "linked-projection",
            "readSurface": "linked-projection",
            "cli": {
                "available": false,
                "validateSupported": false,
                "localArtifactInstallSupported": false,
                "marketplaceInstallSupported": false,
                "installScopeSupported": false,
                "installHelpSummary": null
            },
            "validation": {
                "command": null,
                "strictPassed": false
            },
            "lifecycle": {
                "mode": "managed-shortcut",
                "status": "implemented"
            }
        }),
    )
    .map_err(|e| (Some(state_path.clone()), e))?;

    Ok(vec![projection::ProjectionPathResult {
        path: state_path,
        change: projection::ProjectionPathChange::Written,
    }])
}

/// Apply the Claude and Copilot surface-lifecycle projections, reporting
/// outcomes as [`projection::ProjectionEvidence`] instead of the legacy
/// early-return `Result<(), InstallError>` the retired legacy
/// lifecycle-artifact chain used.
///
/// It duplicates the *behavior* of that retired chain's `claude`/`copilot`
/// branches (via [`apply_claude_lifecycle`] / [`apply_copilot_lifecycle`],
/// which perform the identical real work: same constructors, same JSON
/// payloads, same paths). [`apply_lifecycle_artifact_chain`] calls this
/// function directly as its own first step, reusing the exact
/// Claude-fails-skips-Copilot short-circuit documented below rather than
/// reimplementing it.
///
/// Claude is attempted first, then Copilot — the same order the retired
/// legacy chain checked `primary.contains(&"claude"...)`
/// before `primary.contains(&"copilot"...)`. If Claude's own work fails, the
/// failure is recorded as an owned-path value on `ProjectionTargetId::Claude`
/// and Copilot is never attempted (short-circuit), mirroring the
/// CLI-fail-skips-IDE precedent in `AgyProjection::apply`. If
/// Claude succeeds (or was not selected) and Copilot then fails, Copilot's
/// own failure is recorded independently on `ProjectionTargetId::Copilot`;
/// Claude's already-recorded evidence is left untouched — there is no
/// rollback concept here, unlike AGY's junction pair: Claude's marketplace
/// manifest and runtime-state files staying written when Copilot later fails
/// is the correct, intended outcome, not a fault to undo. When neither
/// target is selected, this returns `Ok(ProjectionEvidence::default())`.
///
/// Every reachable outcome returns `Ok` — a target's own owned failure is
/// recorded as an evidence value, never propagated as the outer `Err`. The
/// `Err` arm exists only for a genuinely global fault with no single owning
/// target; the two lifecycle branches this function mirrors decompose
/// entirely into per-target owned work with no such shared precondition, so
/// in practice this function never constructs it (same as
/// `AgyProjection::apply`, which also never truly needs its
/// outer error given its own signature).
///
/// The `Err` payload is boxed (`Box<ProjectionPhaseFailure>`) purely for
/// `clippy::result_large_err` at this call site: `ProjectionPhaseFailure`
/// wraps `ProjectionPhaseError`, which in turn wraps the largest of
/// `InstallError` / `AgyError` / `projection::ProjectionError`, so the
/// unboxed `Err` variant is well past clippy's size threshold. This function
/// is the type's first real use as a `Result` error (it was purely additive
/// and uncalled before), so the size only became observable here.
/// `ProjectionPhaseFailure` itself is left exactly as landed — reshaping an
/// already-approved, already-committed shared type is out of scope for this
/// change; boxing at the one call site that actually returns it is the
/// narrower fix.
pub fn apply_claude_copilot_evidence(
    canonical_root: &Path,
) -> Result<projection::ProjectionEvidence, Box<ProjectionPhaseFailure>> {
    apply_claude_copilot_evidence_with(canonical_root, true, true)
}

/// `legacy_copilot_plugin` is false when direct publication replaced Copilot's
/// combined plugin root, so the Copilot writer stays off.
fn apply_claude_copilot_evidence_with(
    canonical_root: &Path,
    legacy_claude_link: bool,
    legacy_copilot_plugin: bool,
) -> Result<projection::ProjectionEvidence, Box<ProjectionPhaseFailure>> {
    let selection = load_runtime_selection();
    let primary = primary_targets(&selection);
    let enabled_bundles = load_enabled_bundles();

    let mut evidence = projection::ProjectionEvidence::default();

    if primary.contains(&ProjectionTargetId::Claude) {
        match apply_claude_lifecycle_with(canonical_root, &enabled_bundles, legacy_claude_link) {
            Ok(paths) => {
                evidence
                    .by_target
                    .entry(ProjectionTargetId::Claude)
                    .or_default()
                    .paths
                    .extend(paths);
                evidence
                    .completed_targets
                    .insert(ProjectionTargetId::Claude);
            }
            Err((path, err)) => {
                let failure = projection::ProjectionExpectedFailure {
                    path,
                    reason: err.to_string(),
                };
                evidence.by_target.insert(
                    ProjectionTargetId::Claude,
                    projection::ProjectionTargetEvidence::new(
                        Vec::new(),
                        Vec::new(),
                        None,
                        Some(failure),
                    )
                    .expect("skipped/failure exclusivity: only failure is set"),
                );
                // Short-circuit: Claude failing means Copilot is never
                // attempted, mirroring the AGY CLI-fail-skips-IDE precedent.
                return Ok(evidence);
            }
        }
    }

    if primary.contains(&ProjectionTargetId::Copilot) && legacy_copilot_plugin {
        match apply_copilot_lifecycle(canonical_root) {
            Ok(paths) => {
                evidence
                    .by_target
                    .entry(ProjectionTargetId::Copilot)
                    .or_default()
                    .paths
                    .extend(paths);
                evidence
                    .completed_targets
                    .insert(ProjectionTargetId::Copilot);
            }
            Err((path, err)) => {
                let failure = projection::ProjectionExpectedFailure {
                    path,
                    reason: err.to_string(),
                };
                evidence.by_target.insert(
                    ProjectionTargetId::Copilot,
                    projection::ProjectionTargetEvidence::new(
                        Vec::new(),
                        Vec::new(),
                        None,
                        Some(failure),
                    )
                    .expect("skipped/failure exclusivity: only failure is set"),
                );
            }
        }
    }

    Ok(evidence)
}

/// Apply the AGY (Antigravity) CLI/IDE plugin-junction projections, consuming
/// `AgyProjection::apply` and reporting outcomes as
/// [`projection::ProjectionEvidence`].
///
/// `run_update_with_evidence`'s own Step 4 AGY block is this function's real
/// production caller: it constructs the same `want_agy_cli`/`want_agy_ide`
/// selection this function expects, delegates to this function for the
/// actual apply, then translates the returned evidence back into its own
/// legacy best-effort push/warning shape (a single warning covering the
/// whole AGY step, matching `agy.apply(cli, ide, ..)`'s own one-atomic-call
/// contract) rather than a partial per-target push.
///
/// When neither `want_cli` nor `want_ide` is selected, this returns
/// `Ok(ProjectionEvidence::default())` without even constructing
/// `AgyProjection` — matching the caller's own `if want_agy_cli ||
/// want_agy_ide { .. }` gate, and without resolving home/constructing the
/// ownership handle either (nothing owns anything if nothing runs).
///
/// This function **publishes only**. It never removes a root. A deselected or
/// retired face's combined root is removed by `retire_plugin_roots`, and only
/// after the lockfile proves ccync owns it.
///
/// `AgyProjection::new` failing, or resolving the user's home directory to
/// build the ownership handle's lockfile path failing, are both genuine
/// constructor/setup faults: nothing per-target has been attempted yet, so
/// there is no owning target and no partial evidence to report — these are
/// the only places in this function where `ProjectionPhaseFailure`'s `Err`
/// arm is actually reachable, wrapping the underlying `AgyError` via
/// `ProjectionPhaseError::Agy` with an empty `partial`, matching the
/// "nothing attempted yet" convention `run_machine_update` established for
/// its own outer-fault case.
///
/// Once constructed, `apply`'s own `Err(ProjectionFailure)` arm
/// is mapped through the same convention (`ProjectionPhaseError::Projection`,
/// carrying that call's own partial evidence) — but per `apply`'s
/// own contract, expected owned-path junction failures (CLI or IDE) are
/// already represented as values inside its `Ok(ProjectionEvidence)`, not as
/// its `Err`; this function's `Err` arm for that call is a defensive mapping
/// for a genuinely unexpected fault, not a path this function relies on for
/// the CLI/IDE failure cases.
///
/// Rollback evidence: when the later IDE face fails, `apply` itself rolls
/// back a CLI root it actually published and appends the compensating
/// `Removed` path event onto `AgyCli`'s existing `Linked`/`Written` event —
/// but only when that rollback actually removed the root from disk. This
/// function no longer duplicates that decision from evidence shape alone
/// (`AgyCli` completed + `AgyIde` failed used to be treated as proof of a
/// successful rollback, which it wasn't — a failed rollback still matched
/// that shape and evidence would then claim removal that never happened);
/// `apply`'s own evidence is disk-truth already by the time it reaches here.
pub fn apply_agy_evidence(
    canonical_root: &Path,
    want_cli: bool,
    want_ide: bool,
    dry_run: bool,
) -> Result<projection::ProjectionEvidence, Box<ProjectionPhaseFailure>> {
    if !want_cli && !want_ide {
        return Ok(projection::ProjectionEvidence::default());
    }

    let agy = AgyProjection::new(canonical_root.to_path_buf()).map_err(|e| {
        Box::new(ProjectionPhaseFailure {
            error: ProjectionPhaseError::Agy(e),
            partial: projection::ProjectionEvidence::default(),
        })
    })?;

    let _home = ccync_foundation::paths::user_home().ok_or_else(|| {
        Box::new(ProjectionPhaseFailure {
            error: ProjectionPhaseError::Agy(projection::agy::AgyError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Could not determine home directory",
            ))),
            partial: projection::ProjectionEvidence::default(),
        })
    })?;
    let lockfile_path = provider_lock_path().ok_or_else(|| {
        Box::new(ProjectionPhaseFailure {
            error: ProjectionPhaseError::Agy(projection::agy::AgyError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Could not determine provider state location",
            ))),
            partial: projection::ProjectionEvidence::default(),
        })
    })?;
    let ownership = projection::AgyRootOwnership::from_lockfile(&lockfile_path);

    let evidence = agy
        .apply(want_cli, want_ide, &ownership, dry_run)
        .map_err(|inner| {
            Box::new(ProjectionPhaseFailure {
                error: ProjectionPhaseError::Projection(inner.error),
                partial: inner.partial,
            })
        })?;

    Ok(evidence)
}

/// Merge `from` into `into`: `common`/`run_warnings` append in call order,
/// `completed_targets` unions, and `by_target` replaces key-for-key.
///
/// The one exception is a target failure. When `into` already holds a failure
/// for a target and the later entry has none, the failure stays, the later
/// entry's paths and warnings are kept, and the merged entry has no skip
/// state. A later failure replaces an earlier one as usual.
///
/// [`apply_claude_copilot_evidence`] and [`apply_agy_evidence`] own disjoint
/// target sets (`{Claude, Copilot}` vs `{AgyCli, AgyIde}` — confirmed against
/// `ProjectionTargetId`'s full variant list: `Claude`, `Codex`, `Copilot`,
/// `Opencode`, `AgyCli`, `AgyIde`, `AgyGui`, and neither sub-call
/// ever touches `Codex`/`Opencode`/`AgyGui`), so no `by_target` key collides
/// between the two callers of this helper today.
fn merge_projection_evidence(
    into: &mut projection::ProjectionEvidence,
    from: projection::ProjectionEvidence,
) {
    into.common.extend(from.common);
    for (target, later) in from.by_target {
        let merged = match into.by_target.remove(&target) {
            Some(earlier) if earlier.failure.is_some() && later.failure.is_none() => {
                projection::ProjectionTargetEvidence {
                    paths: later.paths,
                    warnings: later.warnings,
                    skipped: None,
                    failure: earlier.failure,
                }
            }
            _ => later,
        };
        into.by_target.insert(target, merged);
    }
    into.completed_targets.extend(from.completed_targets);
    into.run_warnings.extend(from.run_warnings);
}

/// Compose the Canonical phase's two evidence-producing sub-calls —
/// [`apply_claude_copilot_evidence`] and [`apply_agy_evidence`] — into one
/// [`projection::ProjectionEvidence`] for [`ProjectionPhase::Canonical`].
///
/// Calling order: Claude/Copilot first, then AGY. `run_update_with_evidence`'s
/// own sequencing runs its AGY block *before* the Claude/Copilot/Codex/
/// AGY-runtime-state chain's own Claude/Copilot work (see
/// `apply_lifecycle_artifact_chain`) — but neither
/// sub-call's `Err` arm is reachable through normal runtime input today (both
/// require forcing a setup fault, e.g. `AgyProjection::new` failing to
/// resolve home), so this function's calling order has no observable effect
/// on real behavior either way; it only affects which sub-call's evidence is
/// already present in `partial` when a forced/hypothetical fault hits the
/// other one. Claude/Copilot-first is chosen here so that a forced AGY setup
/// fault demonstrates the "global fault partial equals snapshot" contract
/// against non-trivial prior evidence, which is the harder and more useful
/// case to prove correct.
///
/// The two sub-calls own disjoint target sets (see
/// [`merge_projection_evidence`]), so successful composition is a plain
/// union: `completed_targets` ends up exactly the union of what each
/// sub-call reported, nothing added, dropped, or duplicated.
///
/// If either sub-call returns its own `Err` — an unexpected/global fault, not
/// an owned-path target failure (those are values inside `Ok`) — this
/// function does not swallow it. It merges the already-accumulated evidence
/// (from whichever sub-call ran first and succeeded) with the failing
/// sub-call's own `partial`, and propagates that combined snapshot as the
/// returned failure's `partial`. The propagated `partial` is therefore never
/// just the failing sub-call's own partial alone when prior work already
/// completed.
pub fn run_update_phase(
    canonical_root: &Path,
    want_agy_cli: bool,
    want_agy_ide: bool,
    dry_run: bool,
) -> Result<projection::ProjectionEvidence, Box<ProjectionPhaseFailure>> {
    let mut evidence = projection::ProjectionEvidence::default();

    match apply_claude_copilot_evidence(canonical_root) {
        Ok(cc_evidence) => merge_projection_evidence(&mut evidence, cc_evidence),
        Err(mut failure) => {
            merge_projection_evidence(&mut evidence, std::mem::take(&mut failure.partial));
            failure.partial = evidence;
            return Err(failure);
        }
    }

    match apply_agy_evidence(canonical_root, want_agy_cli, want_agy_ide, dry_run) {
        Ok(agy_evidence) => merge_projection_evidence(&mut evidence, agy_evidence),
        Err(mut failure) => {
            merge_projection_evidence(&mut evidence, std::mem::take(&mut failure.partial));
            failure.partial = evidence;
            return Err(failure);
        }
    }

    Ok(evidence)
}

/// Outcome of [`apply_lifecycle_artifact_chain`]: which runtime names fully
/// completed, the chain-stopping failure reason (if any), and the composed
/// evidence for future consumers.
struct LifecycleArtifactOutcome {
    /// Runtime names that fully completed, in call order: `"claude"`,
    /// `"copilot"`, `"codex"`, then `"agy"` for the runtime-state write
    /// specifically (which may duplicate an `"agy"` the caller already
    /// pushed from the separate AGY plugin-junction step — the caller is
    /// responsible for de-duplicating on push, matching the retired legacy
    /// chain's own per-branch `if !runtimes.contains(...)` guards).
    ///
    /// Retained as this chain's tested completion report after its former
    /// production consumer — the ledger entry's `targets` list — was removed
    /// with the ledger write path. It is not redundant with
    /// `evidence.completed_targets`: the AGY runtime-state write records its
    /// success only here.
    #[allow(dead_code)]
    completed: Vec<&'static str>,
    /// The reason the chain stopped early, already formatted exactly as the
    /// failing step's own `InstallError`/expected-failure `Display` would
    /// read — matching what the retired legacy chain's single `?`-propagated
    /// `InstallError` produced when the caller formatted it directly. `None`
    /// when every selected step completed (or nothing was selected).
    failure_reason: Option<String>,
    /// Structured evidence for future consumers (D-04), exercised by this
    /// module's own tests
    /// (`apply_lifecycle_artifact_chain_composes_full_evidence_for_all_targets`)
    /// and, by `run_update_with_evidence`, a real production caller: it
    /// merges this field into the `ProjectionEvidence` that function returns.
    evidence: projection::ProjectionEvidence,
}

/// Compose Claude/Copilot/Codex lifecycle-artifact evidence plus the AGY
/// runtime-state write into one outcome, mirroring the retired legacy
/// lifecycle-artifact chain's exact sequential short-circuit: a failure at
/// any step means every later step in this chain — Claude -> Copilot ->
/// Codex -> AGY runtime-state — is never attempted, matching that chain's
/// single `?`-propagation through one function. AGY's plugin-junction
/// projection ([`apply_agy_evidence`]) is a separate, independently
/// best-effort step that legacy runs earlier and unconditionally (its own
/// failure never aborts this chain) — callers must call it separately, not
/// through this function.
///
/// Delegates the Claude/Copilot pair to [`apply_claude_copilot_evidence`]
/// (an existing evidence-producing composition that already implements the
/// identical Claude-fails-skips-Copilot short-circuit this chain needs for
/// its first two steps), then extends the same short-circuit discipline to
/// Codex and the AGY runtime-state write using [`apply_codex_lifecycle`] /
/// [`apply_agy_runtime_state`].
fn apply_lifecycle_artifact_chain_with(
    canonical_root: &Path,
    legacy_claude_link: bool,
    legacy_codex_entry: bool,
    legacy_copilot_plugin: bool,
) -> LifecycleArtifactOutcome {
    let selection = load_runtime_selection();
    let primary = primary_targets(&selection);
    let enabled_bundles = load_enabled_bundles();

    let mut completed: Vec<&'static str> = Vec::new();
    let mut evidence = projection::ProjectionEvidence::default();

    match apply_claude_copilot_evidence_with(
        canonical_root,
        legacy_claude_link,
        legacy_copilot_plugin,
    ) {
        Ok(cc_evidence) => merge_projection_evidence(&mut evidence, cc_evidence),
        Err(failure) => {
            // Never reachable through normal runtime input — see
            // `apply_claude_copilot_evidence`'s own doc comment — but handled
            // defensively so this chain never panics or silently drops a
            // global fault.
            merge_projection_evidence(&mut evidence, failure.partial);
            return LifecycleArtifactOutcome {
                completed,
                failure_reason: Some(failure.error.to_string()),
                evidence,
            };
        }
    }

    if let Some(reason) = evidence
        .by_target
        .get(&ProjectionTargetId::Claude)
        .and_then(|te| te.failure.as_ref())
    {
        return LifecycleArtifactOutcome {
            completed,
            failure_reason: Some(reason.reason.clone()),
            evidence,
        };
    }
    if evidence
        .completed_targets
        .contains(&ProjectionTargetId::Claude)
    {
        completed.push("claude");
    }

    if let Some(reason) = evidence
        .by_target
        .get(&ProjectionTargetId::Copilot)
        .and_then(|te| te.failure.as_ref())
    {
        return LifecycleArtifactOutcome {
            completed,
            failure_reason: Some(reason.reason.clone()),
            evidence,
        };
    }
    if evidence
        .completed_targets
        .contains(&ProjectionTargetId::Copilot)
    {
        completed.push("copilot");
    }

    if primary.contains(&ProjectionTargetId::Codex) {
        match apply_codex_lifecycle_with(canonical_root, &enabled_bundles, legacy_codex_entry) {
            Ok(paths) => {
                evidence
                    .by_target
                    .entry(ProjectionTargetId::Codex)
                    .or_default()
                    .paths
                    .extend(paths);
                evidence.completed_targets.insert(ProjectionTargetId::Codex);
                completed.push("codex");
            }
            Err((path, err)) => {
                let reason = err.to_string();
                evidence.by_target.insert(
                    ProjectionTargetId::Codex,
                    projection::ProjectionTargetEvidence::new(
                        Vec::new(),
                        Vec::new(),
                        None,
                        Some(projection::ProjectionExpectedFailure {
                            path,
                            reason: reason.clone(),
                        }),
                    )
                    .expect("skipped/failure exclusivity: only failure is set"),
                );
                return LifecycleArtifactOutcome {
                    completed,
                    failure_reason: Some(reason),
                    evidence,
                };
            }
        }
    }

    if any_agy_face_selected(&primary) {
        match apply_agy_runtime_state(canonical_root) {
            Ok(paths) => {
                evidence.common.extend(paths);
                completed.push("agy");
            }
            Err((_, err)) => {
                let reason = err.to_string();
                evidence.run_warnings.push(reason.clone());
                return LifecycleArtifactOutcome {
                    completed,
                    failure_reason: Some(reason),
                    evidence,
                };
            }
        }
    }

    LifecycleArtifactOutcome {
        completed,
        failure_reason: None,
        evidence,
    }
}

/// Combine projection's live-touch plan with the engine-owned Claude/AGY/Copilot
/// live-surface paths that `run_update_with_evidence` / `apply_lifecycle_artifact_chain`
/// derive, into one grouped, sorted, de-duplicated runtime/path list.
///
/// Every engine-owned path below is produced by calling the exact same
/// constructor/accessor the real apply site calls, never re-derived:
/// - Claude: `ClaudeSkillProjection::new(..).skill_path()`, the same struct
///   `apply_claude_lifecycle` constructs before calling `apply()`.
/// - Copilot: `copilot_projection_root()`, the same function
///   `sync_copilot_projection` calls to learn where it will link/copy.
/// - AGY: `AgyProjection::new(..).cli_target` / `.ide_target`, the same struct
///   `apply_agy_evidence` constructs before calling `apply(cli, ide)`, gated
///   individually on `agy-cli` / `agy-ide` exactly as that call site gates them.
///
/// `opts.repo_root` is the same canonical root `run_update_with_evidence` resolves
/// (`resolve_runtime_roots`: `repo_root` and `source_root` are both
/// `canonical_plugin_root()`), and `opts.selected_projection_targets()` is the
/// single selection both the projection half and the engine half gate on, so the two
/// halves cannot diverge from independently re-resolving the selection.
///
/// Codex is intentionally out of scope: unlike Claude/AGY/Copilot it never
/// writes into a live external agent home — its only artifacts are
/// ccync-internal (the marketplace manifest under `~/.ccync/plugins/` and its
/// target-state JSON under `~/.ccync/dist/targets/`).
///
/// Read-only: no directory creation, no link creation, no file write — the
/// constructors above only join path segments.
pub fn planned_engine_touches(
    opts: &projection::MachineUpdateOptions,
) -> Result<Vec<(String, PathBuf)>, InstallError> {
    let mut touches = projection::planned_machine_touches(opts);

    let selection = RuntimeSelection {
        selected_runtimes: opts.selected_projection_targets().to_vec(),
    };
    let primary = primary_targets(&selection);

    if primary.contains(&ProjectionTargetId::Claude) {
        let claude = ClaudeSkillProjection::new(opts.repo_root.clone())
            .map_err(|e| InstallError::Io(std::io::Error::other(e.to_string())))?;
        // The combined link stays in the disclosure because a blocked retirement
        // keeps it. Direct skill paths depend on the rendered payload, so they
        // are not listed here.
        touches.push(("claude".to_string(), claude.skill_path().to_path_buf()));
    }

    if primary.contains(&ProjectionTargetId::Copilot) {
        touches.push(("copilot".to_string(), copilot_projection_root()?));
    }

    let want_agy_cli = primary.contains(&ProjectionTargetId::AgyCli);
    let want_agy_ide = primary.contains(&ProjectionTargetId::AgyIde);
    let want_agy_gui = primary.contains(&ProjectionTargetId::AgyGui);
    if want_agy_cli || want_agy_ide || want_agy_gui {
        let agy = AgyProjection::new(opts.repo_root.clone())
            .map_err(|e| InstallError::Io(std::io::Error::other(e.to_string())))?;
        // D-13: the three Antigravity surfaces share one "agy" lifecycle label.
        let agy_target = "agy".to_string();
        if want_agy_cli {
            touches.push((agy_target.clone(), agy.cli_target));
        }
        if want_agy_ide {
            touches.push((agy_target.clone(), agy.ide_target));
        }
        if want_agy_gui {
            touches.push((agy_target, agy.gui_target));
        }
    }

    touches.sort();
    touches.dedup();
    Ok(touches)
}

/// Prerequisite state read before any destructive step in `run_uninstall`.
///
/// Captures live paths that become inaccessible after `build/` is deleted, so
/// later steps (MCP de-projection, projection-registry sweep) can use cached
/// paths rather than reading a deleted tree.
pub(crate) struct UninstallPrereqs {
    /// `~/.ccync/build/mcp/managed.json` — present when the file exists.
    pub managed_json: Option<PathBuf>,
    /// `~/.ccync/build/lock.json` — present when the file exists.
    pub lock_json: Option<PathBuf>,
    /// `~/.ccync/config.json` — present when the file exists. Preserved on uninstall.
    pub config_json: Option<PathBuf>,
}

/// Read prerequisite state from `ccync_home` before any deletion.
///
/// Emits named warnings for each missing required input so the caller can
/// propagate them to the audit summary. Never modifies the filesystem.
pub(crate) fn load_uninstall_prereqs(
    layout: &ccync_foundation::paths::EffectiveLayout,
) -> (UninstallPrereqs, Vec<String>) {
    let mut warnings = Vec::new();

    let managed_json = layout.generated_mcp_path();
    let lock_json = layout.build_lock_path();
    let config_json = layout.config_path();

    if !managed_json.exists() {
        warnings.push(format!(
            "build/mcp/managed.json not found at {} — MCP de-projection will be skipped (managed names unknown)",
            managed_json.display()
        ));
    }
    if !lock_json.exists() {
        warnings.push(format!(
            "build/lock.json not found at {} — projection registry cleanup will be skipped",
            lock_json.display()
        ));
    }

    (
        UninstallPrereqs {
            managed_json: managed_json.exists().then_some(managed_json),
            lock_json: lock_json.exists().then_some(lock_json),
            config_json: config_json.exists().then_some(config_json),
        },
        warnings,
    )
}

/// Files under `build/mcp/` that a retry of `ccync uninstall` needs.
///
/// `projected-state.json` is the committed per-host ownership record — the only
/// proof that authorizes removing an entry from a live host. `managed.json` must
/// survive with it because `load_uninstall_prereqs` keys off that file's
/// existence: without it, the next run never calls `run_mcp_remove_managed`, so
/// the record alone would be useless.
const MCP_RETRY_STATE_FILES: [&str; 2] = ["managed.json", "projected-state.json"];

/// Suffix of a generation's views directory. Must match the suffix that
/// `plugin_render::native_package_views_dir` appends.
const VIEWS_DIR_SUFFIX: &str = ".views";

/// Resolve one committed or pending reference to the `g-` generation that owns
/// its files.
///
/// A `g-` id is its own generation. A `v-` view id is owned by the single
/// `<generation>.views/<target>/<view-id>` directory that holds it. Any other
/// shape, a view with no directory, or a view found under two generations is
/// unknown, and the caller must stop pruning that provider.
fn resolve_retained_generation(
    providers: &Path,
    source: &str,
    id: &str,
) -> Result<(String, String), String> {
    if source.is_empty() || source == "." || source == ".." || source.contains(['/', '\\', ':']) {
        return Err(format!(
            "provider reference has an invalid source key: {source}"
        ));
    }
    if id.starts_with("g-") {
        let generation = providers.join(source).join(id);
        match fs::symlink_metadata(&generation) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => {
                return Err(format!(
                    "generation {id} of provider source {source} is not a directory"
                ));
            }
            Err(error) => return Err(error.to_string()),
        }
        return Ok((source.to_string(), id.to_string()));
    }
    if !id.starts_with("v-") {
        return Err(format!(
            "reference {id} of provider source {source} has an unknown generation shape"
        ));
    }
    let mut owners = BTreeSet::new();
    let source_dir = providers.join(source);
    for entry in fs::read_dir(&source_dir).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let generation_dir = entry.path();
        if !entry
            .file_type()
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            continue;
        }
        if !entry.file_name().to_string_lossy().starts_with("g-") {
            continue;
        }
        let views = crate::plugin_render::native_package_views_dir(&generation_dir);
        let targets = match fs::read_dir(&views) {
            Ok(targets) => targets,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        for target in targets {
            let target = target.map_err(|error| error.to_string())?;
            let view = target.path().join(id);
            match fs::symlink_metadata(&view) {
                Ok(metadata) if metadata.file_type().is_dir() => {
                    owners.insert(entry.file_name().to_string_lossy().into_owned());
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
        }
    }
    match owners.len() {
        1 => Ok((source.to_string(), owners.pop_first().unwrap_or_default())),
        0 => Err(format!(
            "view {id} of provider source {source} has no published directory"
        )),
        _ => Err(format!(
            "view {id} of provider source {source} is under more than one generation"
        )),
    }
}

/// Collect provider-generation references from typed ownership records and
/// nested standalone recovery evidence. Path references are recognized only
/// when they name an object below `render/providers/`.
fn collect_provider_generation_references(
    value: &Value,
    refs: &mut BTreeSet<(String, String)>,
    unknown: &mut Vec<String>,
) {
    match value {
        Value::Object(object) => {
            if let (Some(source), Some(generation)) = (
                object.get("source").and_then(Value::as_str),
                object.get("generation").and_then(Value::as_str),
            ) {
                refs.insert((source.to_string(), generation.to_string()));
            }
            for (key, nested) in object {
                if key.to_ascii_lowercase().contains("generation")
                    && nested.as_str().is_some()
                    && object.get("source").and_then(Value::as_str).is_none()
                {
                    unknown.push(format!("unqualified {key} reference"));
                }
                collect_provider_generation_references(nested, refs, unknown);
            }
        }
        Value::Array(values) => {
            for nested in values {
                collect_provider_generation_references(nested, refs, unknown);
            }
        }
        Value::String(text) => {
            let normalized = text.replace('\\', "/");
            let Some((_, tail)) = normalized.split_once("render/providers/") else {
                return;
            };
            let mut parts = tail.split('/');
            let (Some(source), Some(generation)) = (parts.next(), parts.next()) else {
                unknown.push(format!("incomplete provider path reference: {text}"));
                return;
            };
            if source.is_empty()
                || generation.is_empty()
                || source == "."
                || source == ".."
                || source.contains(['/', '\\', ':'])
            {
                unknown.push(format!("incomplete provider path reference: {text}"));
                return;
            }
            if source == "native-marketplaces" {
                return;
            }
            refs.insert((
                source.to_string(),
                generation
                    .strip_suffix(VIEWS_DIR_SUFFIX)
                    .unwrap_or(generation)
                    .to_string(),
            ));
        }
        _ => {}
    }
}

/// Delete everything under `build/` except the ownership proof a retry needs,
/// and return the number of `build/` children removed.
///
/// Two independent retentions, each enabled only when its own teardown left
/// something behind: `keep_mcp_state` preserves `build/mcp/`'s retry files
/// after a failed MCP de-projection, and `keep_agy_ownership` preserves
/// `build/lock.json` while an owned `agy-*` root still stands. `keep_direct_rows`
/// keeps the whole `build/lock.json` while a recorded direct copy could not be
/// removed, so the exact rows survive for the retry. Each keeps the blast radius
/// to "one entry survives" rather than skipping the whole of `build/`.
pub fn prune_build_except_retained(
    build_dir: &Path,
    keep_mcp_state: bool,
    keep_agy_ownership: bool,
    keep_provider_state: bool,
    warnings: &mut Vec<String>,
) -> usize {
    prune_build_except_retained_rows(
        build_dir,
        keep_mcp_state,
        keep_agy_ownership,
        keep_provider_state,
        false,
        warnings,
    )
}

/// [`prune_build_except_retained`] with the direct-row retention an uninstall
/// with a failed direct removal needs.
fn prune_build_except_retained_rows(
    build_dir: &Path,
    keep_mcp_state: bool,
    keep_agy_ownership: bool,
    keep_provider_state: bool,
    keep_direct_rows: bool,
    warnings: &mut Vec<String>,
) -> usize {
    let mut removed = 0usize;
    let Ok(entries) = fs::read_dir(build_dir) else {
        return removed;
    };
    let standalone_state = fs::read_to_string(build_dir.join("projection.txn.json"))
        .ok()
        .map(|raw| ProviderLifecycleJournal::from_json_str(&raw));
    let pending_standalone = match &standalone_state {
        Some(Ok(journal)) => {
            !journal.standalone_intents.is_empty()
                || !journal.standalone_withdrawal_intents.is_empty()
        }
        Some(Err(_)) => true,
        None => false,
    };
    // A post-clear interruption can leave a backup without its intent. Keep
    // unknown private evidence until a specific operation can prove cleanup.
    // An operation directory that cleanup emptied is not evidence.
    let private_evidence = ["standalone-publications", "standalone-withdrawals"]
        .iter()
        .any(|name| {
            fs::read_dir(build_dir.join(name)).is_ok_and(|operations| {
                operations.flatten().any(|operation| {
                    fs::read_dir(operation.path())
                        .map(|mut inner| inner.next().is_some())
                        .unwrap_or(true)
                })
            })
        });
    if pending_standalone || private_evidence {
        warnings.push(
            "standalone publication evidence retained; recovery or cleanup verification required"
                .into(),
        );
    }
    for entry in entries.flatten() {
        let path = entry.path();
        if (pending_standalone || private_evidence)
            && matches!(
                entry.file_name().to_str(),
                Some(
                    "lock.json"
                        | "projection.txn.json"
                        | "standalone-publications"
                        | "standalone-withdrawals"
                        | "render"
                )
            )
        {
            continue;
        }
        if entry.file_name().to_str() == Some("lifecycle.lock") {
            continue;
        }
        if keep_direct_rows && entry.file_name().to_str() == Some("lock.json") {
            continue;
        }
        // A retained journal intent names saved preimages under `preimages/`,
        // so that tree stays whole while provider state is retained.
        if keep_provider_state
            && matches!(
                entry.file_name().to_str(),
                Some("lock.json") | Some("projection.txn.json") | Some("preimages")
            )
        {
            continue;
        }
        if keep_provider_state && entry.file_name().to_str() == Some("render") {
            let providers = path.join("providers");
            if let Ok(entries) = fs::read_dir(&path) {
                for render_entry in entries.flatten() {
                    if render_entry.file_name().to_str() != Some("providers") {
                        if let Err(error) = remove_path_if_present(&render_entry.path()) {
                            warnings.push(format!(
                                "ccync home cleanup ({}): {error}",
                                render_entry.path().display()
                            ));
                        }
                    }
                }
            }
            if providers.exists() {
                let referenced = (|| -> Result<BTreeSet<(String, String)>, String> {
                    let _layout = ccync_foundation::paths::EffectiveLayout::load_or_legacy()
                        .map_err(|error| format!("effective storage layout unreadable: {error}"))?;
                    let text = fs::read_to_string(build_dir.join("lock.json"))
                        .map_err(|e| e.to_string())?;
                    let value: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
                    let lifecycle =
                        ProviderLifecycleLock::from_lock(&value).map_err(|e| e.to_string())?;
                    let mut refs = BTreeSet::new();
                    let mut unknown = Vec::new();
                    for record in lifecycle.committed {
                        refs.insert((
                            record.generation.source.to_string(),
                            record.generation.generation.to_string(),
                        ));
                    }
                    collect_provider_generation_references(&value, &mut refs, &mut unknown);
                    match fs::read_to_string(build_dir.join("projection.txn.json")) {
                        Ok(text) => {
                            let journal = ProviderLifecycleJournal::from_json_str(&text)
                                .map_err(|e| e.to_string())?;
                            let journal_value: Value =
                                serde_json::from_str(&text).map_err(|e| e.to_string())?;
                            collect_provider_generation_references(
                                &journal_value,
                                &mut refs,
                                &mut unknown,
                            );
                            for intent in journal.intents {
                                refs.insert((
                                    intent.desired_generation.source.to_string(),
                                    intent.desired_generation.generation.to_string(),
                                ));
                                if let Some(previous) = intent.previous_generation {
                                    refs.insert((
                                        previous.source.to_string(),
                                        previous.generation.to_string(),
                                    ));
                                }
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.to_string()),
                    }
                    if !unknown.is_empty() {
                        return Err(format!(
                            "unknown provider references: {}",
                            unknown.join(", ")
                        ));
                    }
                    let mut resolved = BTreeSet::new();
                    for (source, id) in refs {
                        if source == "native-marketplaces" {
                            continue;
                        }
                        resolved.insert(resolve_retained_generation(&providers, &source, &id)?);
                    }
                    Ok(resolved)
                })();
                let referenced = match referenced {
                    Ok(referenced) => Some(referenced),
                    Err(error) => {
                        warnings.push(format!("provider generation references unreadable or unresolved; keeping all provider generations: {error}"));
                        None
                    }
                };
                if let Some(referenced) = &referenced {
                    let sources = fs::read_dir(&providers)
                        .and_then(|entries| entries.collect::<Result<Vec<_>, _>>());
                    match sources {
                        Ok(sources) => {
                            for source in sources {
                                let key = source.file_name().to_string_lossy().into_owned();
                                if key == "native-marketplaces" {
                                    continue;
                                }
                                let generations = match fs::read_dir(source.path()).and_then(
                                    |entries| entries.collect::<Result<Vec<_>, _>>(),
                                ) {
                                    Ok(generations) => generations,
                                    Err(error) => {
                                        warnings.push(format!(
                                            "provider source {} unreadable; keeping its generations: {error}",
                                            source.path().display()
                                        ));
                                        continue;
                                    }
                                };
                                for generation in generations {
                                    if !generation.file_type().is_ok_and(|kind| kind.is_dir()) {
                                        continue;
                                    }
                                    // `<g>.views` holds the views of generation `<g>`, so it
                                    // is retained or pruned with that generation.
                                    let name = generation.file_name().to_string_lossy().into_owned();
                                    let (id, is_views) = name
                                        .strip_suffix(VIEWS_DIR_SUFFIX)
                                        .map(|id| (id, true))
                                        .unwrap_or((&name, false));
                                    if !id.starts_with("g-") {
                                        continue;
                                    }
                                    let owned = if is_views {
                                        referenced.contains(&(key.clone(), id.to_string()))
                                    } else {
                                        referenced.contains(&(key.clone(), name.clone()))
                                    };
                                    if !owned {
                                        if let Err(error) =
                                            remove_path_if_present(&generation.path())
                                        {
                                            warnings.push(format!(
                                                "ccync home cleanup ({}): {error}",
                                                generation.path().display()
                                            ));
                                        }
                                    }
                                }
                                if fs::read_dir(source.path())
                                    .is_ok_and(|mut entries| entries.next().is_none())
                                {
                                    let _ = fs::remove_dir(source.path());
                                }
                            }
                        }
                        Err(error) => warnings.push(format!(
                            "provider source listing unreadable; keeping all provider generations: {error}"
                        )),
                    }
                }
                if fs::read_dir(&providers).is_ok_and(|mut entries| entries.next().is_none()) {
                    let _ = fs::remove_dir(&providers);
                }
            }
            continue;
        }
        if keep_agy_ownership && entry.file_name().to_str() == Some("lock.json") {
            // The ownership rows live here; keeping them is what makes a later
            // `ccync uninstall` able to prove it owns the surviving roots and
            // finish the job. Keep the projection namespace whole — its
            // sibling categories are the same proof for the other managed
            // surfaces — and drop everything outside it. `_mcpServers` is what
            // makes that boundary matter: it carries the adopted MCP
            // definitions verbatim (`env`, `headers`), so retaining the whole
            // file would leave the user's credentials on disk after they
            // uninstalled ccync.
            if let Err(e) = projection::strip_lockfile_to_agy_rows(&path) {
                warnings.push(format!("ccync home cleanup ({}): {e}", path.display()));
            }
            continue;
        }
        if keep_mcp_state && entry.file_name().to_str() == Some("mcp") {
            // Inside build/mcp/, keep only the retry state; the rest is derived.
            if let Ok(mcp_entries) = fs::read_dir(&path) {
                for mcp_entry in mcp_entries.flatten() {
                    let keep = mcp_entry
                        .file_name()
                        .to_str()
                        .is_some_and(|n| MCP_RETRY_STATE_FILES.contains(&n));
                    if keep {
                        continue;
                    }
                    let mcp_path = mcp_entry.path();
                    if let Err(e) = remove_path_if_present(&mcp_path) {
                        warnings.push(format!("ccync home cleanup ({}): {e}", mcp_path.display()));
                    }
                }
            }
            continue;
        }
        match remove_path_if_present(&path) {
            Ok(()) => removed += 1,
            Err(e) => warnings.push(format!("ccync home cleanup ({}): {e}", path.display())),
        }
    }
    removed
}

/// Describe which preserved INPUT files are actually present.
fn preserved_files_note(
    config_present: bool,
    plugins_present: bool,
    agents_present: bool,
    mcp_present: bool,
) -> String {
    let mut present = Vec::new();
    if config_present {
        present.push("config.json");
    }
    if plugins_present {
        present.push("plugins.json");
    }
    if agents_present {
        present.push("AGENTS.md");
    }
    if mcp_present {
        present.push("mcp.json");
    }

    if present.is_empty() {
        "(none present)".to_string()
    } else {
        present.join(" + ")
    }
}

/// Remove the managed instruction block from every user-scope target file.
///
/// The path table intentionally contains one entry for each target face, while
/// Antigravity faces share one file. Resolve and dedupe before writing so a
/// shared file is processed once. `write_block` validates the complete marker
/// pair before publishing, so damaged files remain untouched and are reported.
fn remove_managed_instruction_blocks(warnings: &mut Vec<String>) {
    let paths = projection::managed_block::dedupe_resolved(
        ccync_foundation::target::CANONICAL_PROJECTION_TARGETS
            .iter()
            .filter_map(|target| {
                ccync_foundation::paths::user_scope_instruction_write_path(*target)
            })
            .chain(ccync_foundation::paths::legacy_claude_user_instruction_path())
            .collect(),
    );

    for path in paths {
        if let Err(error) = projection::managed_block::write_block(&path, None) {
            let message = format!(
                "managed instruction block removal ({}): {error}",
                path.display()
            );
            eprintln!("ccync uninstall warning: {message}");
            warnings.push(message);
        }
    }
}

/// Keys of the direct copies the plan names: `root/name` for every skill,
/// agent, and command route that publishes. A command-derived skill names a
/// directory, and a native command or agent names a file. A failed route is
/// still planned, so its copy is not pruned.
fn planned_direct_copies(plan: &crate::delivery::DeliveryPlan, home: &Path) -> BTreeSet<String> {
    use crate::delivery::ComponentRouteChoice;
    plan.routes
        .iter()
        .filter_map(|route| match &route.choice {
            ComponentRouteChoice::Direct { root } | ComponentRouteChoice::ServedByRoot { root } => {
                let name = route
                    .alias
                    .clone()
                    .unwrap_or_else(|| route.identity.component_id.clone());
                Some(path_key(&physical_skill_root(home, root).join(name)))
            }
            _ => None,
        })
        .collect()
}

/// Whether a recorded direct copy is named by a planned route. A skill record
/// names its directory, so the copy itself is compared. An agent or native
/// command file is planned when its stem, the part before the first dot, names
/// a planned `root/name`. For example, `helper.agent.md` is `helper`.
fn is_planned_direct_copy(copy: &Path, planned: &BTreeSet<String>) -> bool {
    if planned.contains(&path_key(copy)) {
        return true;
    }
    let (Some(parent), Some(file)) = (copy.parent(), copy.file_name()) else {
        return false;
    };
    // A command-derived skill records its `SKILL.md`, while the plan names the
    // directory that holds it. The registry stores paths lowercased on Windows,
    // so the file name is compared without case.
    if file.eq_ignore_ascii_case("SKILL.md") && planned.contains(&path_key(parent)) {
        return true;
    }
    let stem = file.to_string_lossy();
    let name = stem.split('.').next().unwrap_or_default();
    planned.contains(&path_key(&parent.join(name)))
}

/// Remove prior direct copies that the current publication no longer names.
/// This is how a deselected target's copies leave disk, including a root no
/// other route writes.
///
/// A planned copy is kept, because publication rewrites it. A copy named
/// `ccync` is kept too: combined-copy retirement owns that name and decides on
/// its own whether a failed target keeps it. Only recorded rows are removed, so
/// a foreign file at a planned or unplanned path survives.
///
/// A row is forgotten only after its copy is gone. A failed removal keeps the
/// row, so the next run can retry it.
fn prune_unplanned_direct_records(
    lockfile: &Path,
    records: &[projection::DirectProjectionRecord],
    is_planned: impl Fn(&projection::DirectProjectionRecord) -> bool,
    warnings: &mut Vec<String>,
) {
    for record in records {
        let copy = &record.path;
        if copy
            .file_name()
            .is_some_and(|name| name == COMBINED_COPY_NAME)
            || is_planned(record)
        {
            continue;
        }
        if let Err(error) = remove_path_if_present(copy) {
            warnings.push(format!(
                "deselected direct copy removal ({}): {error}",
                copy.display()
            ));
            continue;
        }
        remove_emptied_skill_dir(copy);
        if let Err(error) = projection::forget_direct_record(lockfile, record.kind, copy, false) {
            warnings.push(format!(
                "deselected direct copy record ({}): {error}",
                copy.display()
            ));
        }
    }
}

/// Comparison key for a path, normalized as the projection lockfile
/// normalizes its rows, so a stored row and a route destination compare equal.
fn path_key(path: &Path) -> String {
    projection::normalize_managed_artifact_path(path)
}

fn legacy_source_path_key(path: &Path) -> String {
    let rendered = path.to_string_lossy().replace('/', "\\");
    if cfg!(windows) {
        rendered.to_ascii_lowercase()
    } else {
        rendered
    }
}

/// Remove private committed state while preserving its authority evidence and locks.
fn prune_committed_state(
    layout: &ccync_foundation::paths::EffectiveLayout,
    retain_mcp_state: bool,
    retain_agy_ownership: bool,
    retain_provider_state: bool,
    retain_direct_rows: bool,
    warnings: &mut Vec<String>,
) -> usize {
    let mut removed = 0;
    let state_root = layout.machine_state_root();
    let keep = ["layout.json", "migration.json", "lifecycle.lock"];
    let Ok(entries) = fs::read_dir(&state_root) else {
        return removed;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_str().is_some_and(|name| keep.contains(&name)) {
            continue;
        }
        if name == "build" {
            removed += prune_build_except_retained_rows(
                &entry.path(),
                retain_mcp_state,
                retain_agy_ownership,
                retain_provider_state,
                retain_direct_rows,
                warnings,
            );
            if fs::read_dir(entry.path()).is_ok_and(|mut children| children.next().is_none()) {
                if fs::remove_dir(entry.path()).is_ok() {
                    removed += 1;
                }
            }
            continue;
        }
        if let Err(error) = remove_path_if_present(&entry.path()) {
            warnings.push(format!(
                "ccync state cleanup ({}): {error}",
                entry.path().display()
            ));
        } else {
            removed += 1;
        }
    }
    removed
}

/// Remove legacy private data while preserving lifecycle lock files and, after
/// a committed migration, the legacy migration sentinel.
fn prune_legacy_home(
    ccync_home: &Path,
    keep_migration_sentinel: bool,
    warnings: &mut Vec<String>,
) -> usize {
    let mut removed = 0;
    let Ok(entries) = fs::read_dir(ccync_home) else {
        return removed;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if keep_migration_sentinel && name == "migration.json" {
            continue;
        }
        if name == "build" {
            if let Ok(children) = fs::read_dir(entry.path()) {
                for child in children.flatten() {
                    if child.file_name() == "lifecycle.lock" {
                        continue;
                    }
                    if let Err(error) = remove_path_if_present(&child.path()) {
                        warnings.push(format!(
                            "ccync home cleanup ({}): {error}",
                            child.path().display()
                        ));
                    }
                }
            }
            continue;
        }
        match remove_path_if_present(&entry.path()) {
            Ok(()) => removed += 1,
            Err(error) => warnings.push(format!(
                "ccync home cleanup ({}): {error}",
                entry.path().display()
            )),
        }
    }
    removed
}

/// Run `ccync uninstall` — remove canonical root and runtime surfaces.
pub fn run_uninstall() -> Result<(), InstallError> {
    run_uninstall_using(&crate::plugin_native::ProcessNativeRunner)
}

/// Same as [`run_uninstall`], with native CLI calls routed through `runner`.
/// Tests pass an injected runner so no host CLI is reached.
pub fn run_uninstall_using(
    runner: &dyn crate::plugin_native::NativeCommandRunner,
) -> Result<(), InstallError> {
    let layout = effective_layout()?;
    let direct_mcp_input = read_direct_mcp_input(&layout)?;
    let canonical_root = crate::paths::canonical_plugin_root().ok_or(InstallError::NoHome)?;
    let home = crate::paths::user_home().ok_or(InstallError::NoHome)?;
    let lifecycle_guard = crate::tool_manager::lock::LifecycleLock::acquire_if_needed()
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    use ccync_foundation::paths::EffectiveLayoutAuthority;
    match layout.authority() {
        EffectiveLayoutAuthority::MutationBlocked => {
            return Err(invalid_lifecycle_state(
                "uninstall blocked: effective storage layout is MutationBlocked",
            ));
        }
        EffectiveLayoutAuthority::Committed => {
            let journal_exists = layout
                .migration_journal_path()
                .try_exists()
                .map_err(|error| {
                    invalid_lifecycle_state(format!(
                        "uninstall blocked: migration journal cannot be checked: {error}"
                    ))
                })?;
            if journal_exists {
                let drift =
                    crate::migrate::detect_legacy_input_drift(&layout).map_err(|error| {
                        invalid_lifecycle_state(format!("uninstall blocked: {error}"))
                    })?;
                if !drift.is_empty() {
                    return Err(invalid_lifecycle_state(format!(
                        "uninstall blocked: legacy storage drift detected: {}",
                        drift.join("; ")
                    )));
                }
            }
        }
        _ => {}
    }

    let pending = read_standalone_journal()?;
    if !pending.standalone_intents.is_empty() || !pending.standalone_withdrawal_intents.is_empty() {
        return Err(invalid_lifecycle_state("recovery blocked: pending standalone publications retain their destinations and backups; sync must reconcile before uninstall"));
    }

    let mut warnings = Vec::new();

    // Preflight: read prerequisite state before any deletion.
    // Missing prereq files are informational — they don't trigger PartialUninstall
    // (the user is told what was skipped; actual removal failures go into warnings).
    let ccync_home = home.join(".ccync");
    let (prereqs, prereq_warnings) = load_uninstall_prereqs(&layout);
    for w in &prereq_warnings {
        eprintln!("ccync uninstall info: {w}");
    }

    // De-project managed MCP entries from live agent configs BEFORE deleting build/.
    //
    // `retain_mcp_state` makes run_mcp_remove_managed's "committed ownership
    // retained for retry" promise true. The committed per-host ownership record
    // is the *only* thing that can ever authorize removing ccync's entries from a
    // live host; the cleanup below otherwise deletes every ~/.ccync child except
    // config.json/plugins.json, taking build/mcp/ with it. That would leave a
    // failed host's entries stranded permanently, with no record that could
    // authorize a later removal — and the retry the message promises would be
    // impossible, because the next run finds no managed.json and never calls
    // run_mcp_remove_managed at all.
    let mut retain_mcp_state = false;
    if let Some(managed_json) = &prereqs.managed_json {
        match run_mcp_remove_managed(managed_json) {
            Ok(report) => {
                // Report warnings are benign informational notices under the
                // committed-ownership model (e.g. "no committed ownership found —
                // nothing removed"); a real host-write failure returns Err below.
                // Print them, but do not escalate the uninstall to a partial
                // failure over a benign no-op.
                for w in report.warnings {
                    eprintln!("ccync uninstall warning: {w}");
                }
            }
            Err(e) => {
                let msg = format!("MCP de-projection failed: {e}");
                eprintln!("ccync uninstall warning: {msg}");
                warnings.push(msg);
                retain_mcp_state = true;
            }
        }
    }

    let lock_path = prereqs
        .lock_json
        .clone()
        .unwrap_or_else(|| layout.build_lock_path());
    let lock_proof = match &lifecycle_guard {
        Some(guard) => guard.held_lock_proof(),
        None => {
            crate::tool_manager::lock::LifecycleLock::current_proof().expect("lifecycle lock held")
        }
    };
    reconcile_native_intents(&lock_proof, runner, &mut warnings);
    let retain_provider_state =
        remove_committed_providers_for_uninstall(&lock_proof, &mut warnings, runner);

    // De-register marketplace / plugin-state records BEFORE deleting build/.
    // Claude owns structured, readable files (installed_plugins.json /
    // known_marketplaces.json); Codex has no ccync-readable registry, only a CLI.
    remove_claude_installed_plugins_residue(&home, &retain_provider_state, &mut warnings);
    remove_claude_known_marketplaces_residue(&home, &mut warnings);
    for note in deregister_codex_marketplace() {
        eprintln!("ccync uninstall info: {note}");
    }
    // Remove granular ccync-owned projection surfaces recorded in build/lock.json,
    // guarded by positive managed-set membership. Read the prior
    // lockfile BEFORE build/ is deleted; an empty/missing prior authorizes nothing,
    // so user-owned siblings on shared surfaces are never touched.
    let authority = projection::ManagedDeprojection::from_lockfile(&lock_path);
    let retain_direct_rows = deproject_managed_surfaces(
        &lock_path,
        &authority,
        &managed_surface_roots(&home),
        &mut warnings,
    );

    // Remove AGY surfaces before removing canonical root (they link to it).
    // Reuses the same prior-lockfile `lock_path` already resolved above for
    // `ManagedDeprojection` — a fresh `AgyRootOwnership` over it authorizes
    // exactly the agy-* roots this uninstall run's own prior state recorded,
    // never a same-named directory it never owned.
    //
    // Not gated on `canonical_root.exists()`. That guard was safe while every
    // agy root was a link: with the canonical root gone, a surviving link was
    // inert. A directory copy is self-contained — Antigravity keeps loading
    // ccync's whole bundle out of it — so teardown must run regardless of
    // whether the canonical root is still there.
    //
    // `retain_agy_ownership` is the same invariant `retain_mcp_state` keeps
    // below: `agyRootProjectionPaths` in `build/lock.json` is the *only* proof
    // that authorizes removing a published copy (`remove_agy_root` requires
    // `is_owned`, and a missing lockfile owns nothing). Deleting the lockfile
    // while a copy survives would strand that copy permanently — re-running
    // `ccync uninstall` could never converge.
    let mut retain_agy_ownership = false;
    match AgyProjection::new(canonical_root.clone()) {
        Ok(agy) => {
            let ownership = projection::AgyRootOwnership::from_lockfile(&lock_path);
            // Take the teardown's own warnings rather than dropping them: a
            // preserved (unprovable-ownership) root survives the uninstall as
            // a live, self-contained plugin copy, and the user needs to be
            // told which path to remove by hand.
            let mut agy_warnings = Vec::new();
            if let Err(e) = agy.remove_reporting(&ownership, false, &mut agy_warnings) {
                warnings.push(format!("AGY removal (best-effort): {e}"));
            }
            for warning in agy_warnings {
                let msg = format!("AGY root left in place: {warning}");
                eprintln!("ccync uninstall warning: {msg}");
                warnings.push(msg);
            }
            // Survival is read from disk, not inferred from the error: a
            // best-effort teardown can fail on one root and succeed on the
            // others, and a root preserved as user-owned is not a survivor
            // ccync must keep proof for — but `is_owned` already excluded it,
            // so anything still standing that ccync owns is exactly what this
            // retention is for.
            for target in [&agy.cli_target, &agy.ide_target, &agy.gui_target] {
                if ownership.is_owned(target)
                    && ccync_foundation::platform::classify_link(target)
                        != ccync_foundation::platform::LinkShape::Absent
                {
                    retain_agy_ownership = true;
                }
            }
        }
        Err(e) => warnings.push(format!("AGY uninstall init (best-effort): {e}")),
    }

    // Remove the managed instruction block after all other projection surfaces
    // have been de-projected. The source file under ~/.ccync is retained below;
    // only its projected blocks are removed from user-scope target files.
    remove_managed_instruction_blocks(&mut warnings);

    // Remove canonical root.
    if canonical_root.exists() {
        if let Err(e) = fs::remove_dir_all(&canonical_root) {
            warnings.push(format!("canonical root removal: {e}"));
        }
    }

    collect_removal_error(
        &mut warnings,
        "copilot projection removal",
        copilot_projection_root().and_then(|path| remove_path_if_present(&path)),
    );
    collect_removal_error(
        &mut warnings,
        "Claude skills removal",
        remove_path_if_present(&home.join(".claude").join("skills").join("ccync")),
    );
    collect_removal_error(
        &mut warnings,
        "Claude plugins removal",
        remove_path_if_present(&home.join(".claude").join("plugins").join("ccync")),
    );
    collect_removal_error(
        &mut warnings,
        "target dist removal",
        remove_path_if_present(&layout.dist_path().join("targets")),
    );
    collect_removal_error(
        &mut warnings,
        "stale provider dist removal",
        remove_path_if_present(&layout.dist_path().join("providers")),
    );
    collect_removal_error(
        &mut warnings,
        "stale runtime dist removal",
        remove_path_if_present(&layout.dist_path().join("runtimes")),
    );
    collect_removal_error(
        &mut warnings,
        "claude-plugin marker removal",
        remove_path_if_present(
            &layout
                .portable_root()
                .join("plugins")
                .join(".claude-plugin"),
        ),
    );
    collect_removal_error(
        &mut warnings,
        ".agents marker removal",
        remove_path_if_present(&layout.portable_root().join("plugins").join(".agents")),
    );

    // Delete every ~/.ccync child except the four persistent INPUT files
    // (config.json, plugins.json, AGENTS.md, mcp.json) — removes cache/, build/, any legacy
    // ledger.json, and any other derived state. The final preserve-set is
    // exactly the user inputs. No uninstall ledger is written: the former
    // write_ledger_entry("uninstall", ...) is intentionally gone (ledger is a
    // derived ~/.ccync child and must not survive a full uninstall).
    //
    // Exception: when MCP de-projection failed, build/ is pruned down to the MCP
    // ownership record instead of being deleted, so `ccync uninstall` can be
    // re-run and actually converge.
    let mut removed_children: usize = 0;
    if layout.authority() == EffectiveLayoutAuthority::Committed {
        removed_children += prune_legacy_home(&ccync_home, true, &mut warnings);
        removed_children += prune_committed_state(
            &layout,
            retain_mcp_state,
            retain_agy_ownership,
            !retain_provider_state.is_empty(),
            retain_direct_rows,
            &mut warnings,
        );
        if let Err(error) = remove_path_if_present(&layout.machine_cache_root()) {
            warnings.push(format!(
                "ccync cache cleanup ({}): {error}",
                layout.machine_cache_root().display()
            ));
        }
    } else if let Ok(entries) = fs::read_dir(&ccync_home) {
        for entry in entries.flatten() {
            if matches!(
                entry.file_name().to_str(),
                Some("config.json") | Some("plugins.json") | Some("AGENTS.md") | Some("mcp.json")
            ) {
                continue;
            }
            let path = entry.path();
            if entry.file_name().to_str() == Some("build") {
                removed_children += prune_build_except_retained_rows(
                    &path,
                    retain_mcp_state,
                    retain_agy_ownership,
                    !retain_provider_state.is_empty(),
                    retain_direct_rows,
                    &mut warnings,
                );
                continue;
            }
            match remove_path_if_present(&path) {
                Ok(()) => removed_children += 1,
                Err(e) => warnings.push(format!("ccync home cleanup ({}): {e}", path.display())),
            }
        }
    }
    if retain_mcp_state {
        eprintln!(
            "ccync uninstall: retained {} so the MCP entries above can be removed by re-running `ccync uninstall` — without it ccync could never prove it owned them",
            layout.state_base().join("mcp").display()
        );
    }
    if retain_agy_ownership {
        eprintln!(
            "ccync uninstall: retained {} so the surviving agy plugin root(s) can be removed by re-running `ccync uninstall` — without it ccync could never prove it owned them",
            layout.build_lock_path().display()
        );
    }
    if retain_direct_rows {
        eprintln!(
            "ccync uninstall: retained {} so the recorded direct copies that could not be removed can be removed by re-running `ccync uninstall`",
            layout.build_lock_path().display()
        );
    }

    // stderr audit summary: removed derived state, preserved inputs, warnings.
    // Report each of the four preserved INPUT files truthfully — none of
    // deletion above touches them, so their on-disk existence right now is
    // exactly what was preserved.
    let plugins_json_present = layout.plugins_path().exists();
    let agents_md_present = layout.agents_md_path().exists();
    let mcp_json_present = direct_mcp_input.is_some();
    let mut config_note = preserved_files_note(
        prereqs.config_json.is_some(),
        plugins_json_present,
        agents_md_present,
        mcp_json_present,
    );
    if let Some(input) = &direct_mcp_input {
        config_note = config_note.replace("mcp.json", &input.path().display().to_string());
    }
    eprintln!(
        "ccync uninstall: removed {removed_children} derived ~/.ccync item(s); preserved {config_note}"
    );
    for w in &warnings {
        eprintln!("ccync uninstall warning: {w}");
    }

    if warnings.is_empty() {
        Ok(())
    } else {
        Err(InstallError::PartialUninstall { warnings })
    }
}

/// Remove ccync-owned plugin records from Claude's `installed_plugins.json`.
///
/// ccync registers a Claude marketplace named `ccync`, so its installed plugins
/// are keyed `<plugin>@ccync` in the v2 `{ "plugins": { "<key>": [..] } }` map.
/// Only those keys are removed; user plugins from other marketplaces survive.
/// Parse failure preserves the file byte-for-byte and emits a named warning
/// (genuine failure → `PartialUninstall`). An absent file or a file with no
/// ccync records is a silent no-op (the file is never rewritten needlessly).
fn remove_claude_installed_plugins_residue(
    home: &Path,
    retained_selectors: &BTreeSet<String>,
    warnings: &mut Vec<String>,
) {
    let path = home
        .join(".claude")
        .join("plugins")
        .join("installed_plugins.json");
    if !path.exists() {
        return;
    }
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            warnings.push(format!(
                "Claude installed_plugins.json read failed (preserved): {e}"
            ));
            return;
        }
    };
    let mut value: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            warnings.push(format!(
                "Claude installed_plugins.json parse failed — preserved byte-for-byte, ccync records not removed: {e}"
            ));
            return;
        }
    };
    let mut removed = false;
    if let Some(plugins) = value.get_mut("plugins").and_then(Value::as_object_mut) {
        let ccync_keys: Vec<String> = plugins
            .keys()
            .filter(|key| key.ends_with("@ccync") && !retained_selectors.contains(*key))
            .cloned()
            .collect();
        for key in ccync_keys {
            plugins.remove(&key);
            removed = true;
        }
    }
    if removed {
        if let Err(e) = write_json_file(&path, &value) {
            warnings.push(format!("Claude installed_plugins.json write failed: {e}"));
        }
    }
}

/// Remove the ccync marketplace record from Claude's `known_marketplaces.json`.
///
/// The real Claude Code file is flat: each top-level key is a marketplace name
/// mapping directly to its entry object — there is no `"marketplaces"` wrapper.
/// This mirrors `adopt::parse_known_marketplaces`, which reads the same file;
/// the two must agree on the shape or de-registration silently no-ops.
///
/// ccync never writes this file itself, but it does render a marketplace
/// manifest at `~/.ccync/plugins/.claude-plugin/marketplace.json` (see
/// `apply_claude_lifecycle`) so a user can run `claude plugin marketplace add`.
/// When they have, Claude creates a flat top-level `ccync` key here, and this
/// is the code that must remove it again.
///
/// Removes only the `ccync` key, preserving every other marketplace verbatim.
/// Parse failure preserves the file byte-for-byte + warns; a non-object
/// top-level shape is a named skip (stderr info, not a hard failure) so we
/// never silently claim a clean de-registration we did not perform.
fn remove_claude_known_marketplaces_residue(home: &Path, warnings: &mut Vec<String>) {
    let path = home
        .join(".claude")
        .join("plugins")
        .join("known_marketplaces.json");
    if !path.exists() {
        return;
    }
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            warnings.push(format!(
                "Claude known_marketplaces.json read failed (preserved): {e}"
            ));
            return;
        }
    };
    let mut value: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            warnings.push(format!(
                "Claude known_marketplaces.json parse failed — preserved byte-for-byte, ccync marketplace not removed: {e}"
            ));
            return;
        }
    };
    let removed = match value.as_object_mut() {
        Some(marketplaces) => {
            let removed = marketplaces.remove("ccync").is_some();
            if !removed {
                eprintln!(
                    "ccync uninstall info: Claude known_marketplaces.json holds no `ccync` marketplace — nothing to de-register (file preserved)"
                );
            }
            removed
        }
        None => {
            eprintln!(
                "ccync uninstall info: Claude known_marketplaces.json is not a JSON object — ccync marketplace de-registration skipped (file preserved)"
            );
            false
        }
    };
    if removed {
        if let Err(e) = write_json_file(&path, &value) {
            warnings.push(format!("Claude known_marketplaces.json write failed: {e}"));
        }
    }
}

/// Best-effort de-registration of the ccync Codex marketplace.
///
/// Codex exposes no ccync-readable registry file — its plugin state is reachable
/// only through the `codex` CLI. When `codex` is on PATH we best-effort run the
/// documented removal commands; otherwise we record a single named skip note.
///
/// Returns informational notes (never `PartialUninstall` warnings): de-registering
/// an external CLI's own state is best-effort, so a missing CLI or a failed CLI
/// call must not fail the whole uninstall — but it is always reported (no silent
/// degradation, never a claim that de-registration completed when it did not).
fn deregister_codex_marketplace() -> Vec<String> {
    deregister_codex_marketplace_impl(binary_in_path("codex"))
}

fn deregister_codex_marketplace_impl(codex_available: bool) -> Vec<String> {
    if !codex_available {
        return vec![
            "codex not found on PATH — Codex marketplace de-registration skipped (run `codex plugin remove ccync@ccync-marketplace ; codex plugin marketplace remove ccync-marketplace` manually if Codex was used)".to_string(),
        ];
    }
    let mut notes = Vec::new();
    let steps: [&[&str]; 2] = [
        &["plugin", "remove", "ccync@ccync-marketplace"],
        &["plugin", "marketplace", "remove", "ccync-marketplace"],
    ];
    for args in steps {
        match std::process::Command::new("codex").args(args).output() {
            Ok(out) if out.status.success() => {}
            Ok(out) => notes.push(format!(
                "codex {} exited non-zero (best-effort): {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => notes.push(format!(
                "codex {} failed to spawn (best-effort): {e}",
                args.join(" ")
            )),
        }
    }
    notes
}

/// The live agent surface directories ccync projects skills / commands / agents
/// into, where ccync-owned files can sit alongside user-owned ones. Used as the
/// scan set for registry-guarded de-projection. Safety does not depend on this
/// list being exhaustive — the [`ManagedDeprojection`] guard only removes paths
/// recorded ccync-managed — but completeness here minimizes residue.
fn managed_surface_roots(home: &Path) -> Vec<PathBuf> {
    let mut roots = vec![
        home.join(".claude").join("skills"),
        home.join(".claude").join("commands"),
        home.join(".claude").join("agents"),
        // No existing same-value helper for ".codex/skills" (only the shared
        // ".agents/skills" surface has one) — left as a literal rather than
        // inventing a new helper.
        home.join(".codex").join("skills"),
        home.join(".codex").join("agents"),
        home.join(".gemini").join("skills"),
        home.join(".gemini").join("commands"),
        // Direct AGY skill and agent roots, which the direct route plan writes.
        home.join(".gemini").join("antigravity-cli").join("skills"),
        home.join(".gemini").join("antigravity-ide").join("skills"),
        home.join(".gemini").join("config").join("skills"),
        home.join(".gemini").join("config").join("agents"),
        home.join(".copilot").join("skills"),
        home.join(".copilot").join("agents"),
        home.join(".config").join("opencode").join("skills"),
        home.join(".config").join("opencode").join("commands"),
        home.join(".config").join("opencode").join("agents"),
        home.join(".gemini").join("antigravity").join("skills"),
    ];
    if let Some(shared_agents_skills) = crate::paths::shared_agents_skills_path() {
        roots.push(shared_agents_skills);
    }
    roots
}

/// Remove ccync-owned projected entries, authorized strictly by prior-lockfile
/// membership.
///
/// The exact direct rows come first. Each recorded copy is removed, and its row
/// is forgotten only after that removal succeeds. Recorded source links follow,
/// with the same rule. Then the legacy surface roots are scanned one level deep,
/// and each authorized child is removed.
///
/// A recorded source link is removed only while it is still a link. A path a
/// user replaced with a real directory is left in place and reported, and its
/// row is still forgotten.
///
/// Returns `true` when a direct copy, direct row, or source link could not be
/// removed. The caller then keeps `build/lock.json`, so a later uninstall can
/// retry from the same rows.
///
/// An empty/missing prior lockfile authorizes nothing, so this is a no-op on a
/// partial-state uninstall rather than a risk to same-named user-owned siblings.
fn deproject_managed_surfaces(
    lock_path: &Path,
    authority: &projection::ManagedDeprojection,
    surface_roots: &[PathBuf],
    warnings: &mut Vec<String>,
) -> bool {
    if authority.is_empty() {
        return false;
    }
    let mut direct_failed = false;
    for record in projection::direct_records(lock_path) {
        if let Err(e) = remove_path_if_present(&record.path) {
            warnings.push(format!(
                "managed surface removal ({}): {e}",
                record.path.display()
            ));
            direct_failed = true;
            continue;
        }
        remove_emptied_skill_dir(&record.path);
        if let Err(e) =
            projection::forget_direct_record(lock_path, record.kind, &record.path, false)
        {
            warnings.push(format!(
                "managed surface record ({}): {e}",
                record.path.display()
            ));
            direct_failed = true;
        }
    }
    for link in projection::source_link_records(lock_path) {
        let left_in_place = link.symlink_metadata().is_ok()
            && !ccync_foundation::platform::is_symlink_or_junction(&link);
        if let Err(e) = projection::remove_source_link(lock_path, &link, false) {
            warnings.push(format!("source link removal ({}): {e}", link.display()));
            direct_failed = true;
            continue;
        }
        if left_in_place {
            eprintln!(
                "ccync uninstall info: source link left in place, not a link: {}",
                link.display()
            );
        }
    }
    for root in surface_roots {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if authority.is_authorized_for_removal(&path) {
                if let Err(e) = remove_path_if_present(&path) {
                    warnings.push(format!("managed surface removal ({}): {e}", path.display()));
                }
            }
        }
    }
    direct_failed
}

/// Whether `name` resolves to an executable on PATH (Windows also tries the
/// `PATHEXT`-style `.exe`/`.cmd`/`.bat` suffixes).
fn binary_in_path(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        if dir.join(name).is_file() {
            return true;
        }
        #[cfg(windows)]
        {
            ["exe", "cmd", "bat"]
                .iter()
                .any(|ext| dir.join(format!("{name}.{ext}")).is_file())
        }
        #[cfg(not(windows))]
        {
            false
        }
    })
}

fn collect_removal_error(
    warnings: &mut Vec<String>,
    label: &str,
    result: Result<(), InstallError>,
) {
    if let Err(err) = result {
        warnings.push(format!("{label}: {err}"));
    }
}

/// Remove the directory a command-derived skill's `SKILL.md` leaves behind.
///
/// Only an empty directory is removed, so a foreign sibling keeps its
/// directory. The directory is never recorded as owned. Leaving it would make
/// the next planning pass read it as a foreign skill.
fn remove_emptied_skill_dir(copy: &Path) {
    if !copy
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("SKILL.md"))
    {
        return;
    }
    if let Some(parent) = copy.parent() {
        let _ = fs::remove_dir(parent);
    }
}

fn remove_path_if_present(path: &Path) -> Result<(), InstallError> {
    if ccync_foundation::platform::is_symlink_or_junction(path) {
        ccync_foundation::platform::remove_dir_link(path)?;
        return Ok(());
    }
    if path.is_file() {
        fs::remove_file(path)?;
    } else if path.is_dir() {
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccync_foundation::provider::ClientScope;

    #[test]
    fn private_engine_paths_use_committed_roots_and_keep_live_roots_legacy() {
        use ccync_foundation::{
            paths::{EffectiveLayout, EffectiveLayoutAuthority, StorageRoots},
            provider::{LayoutRootIdentity, StorageLayoutCommit},
        };

        let home = crate::test_support::TestHome::new();

        let roots = StorageRoots::resolve().unwrap();
        for root in [&roots.portable, &roots.config, &roots.state, &roots.cache] {
            fs::create_dir_all(root).unwrap();
        }
        let commit = StorageLayoutCommit {
            layout_version: 1,
            roots: vec![
                ("portable", &roots.portable),
                ("config", &roots.config),
                ("state", &roots.state),
                ("cache", &roots.cache),
            ]
            .into_iter()
            .map(|(name, path)| LayoutRootIdentity {
                name: name.into(),
                path: path.clone(),
            })
            .collect(),
        };
        let commit = serde_json::to_vec(&commit).unwrap();
        fs::create_dir_all(home.path().join(".ccync")).unwrap();
        fs::write(home.path().join(".ccync/migration.json"), &commit).unwrap();
        fs::write(roots.state.join("layout.json"), commit).unwrap();

        let layout = EffectiveLayout::load_or_legacy().unwrap();
        assert_eq!(layout.authority(), EffectiveLayoutAuthority::Committed);
        assert_eq!(
            layout.personal_catalog_path(),
            roots.config.join("plugins.json")
        );
        assert_eq!(
            layout.mcp_input_path(),
            home.path().join(".agents/mcp.json")
        );
        assert_eq!(layout.source_cache_path(), roots.state.join("sources"));
        assert_eq!(layout.dist_path(), roots.state.join("dist"));
        assert_eq!(
            layout.generated_mcp_path(),
            roots.state.join("build/mcp/managed.json")
        );
        assert_eq!(
            layout.routes_path(),
            layout.state_base().join("routes.json")
        );
        let legacy = home.path().join(".ccync");
        assert_eq!(
            crate::paths::ccync_plugins_root(),
            Some(legacy.join("plugins"))
        );
        assert_eq!(
            ccync_foundation::paths::provider_render_root(),
            Some(legacy.join("build/render/providers"))
        );
        assert_eq!(crate::paths::ccync_data_root(), Some(legacy.join("data")));
        assert_eq!(
            ccync_foundation::paths::ccync_active_provider_path("claude"),
            Some(legacy.join("build/active/claude"))
        );
        assert_eq!(
            ccync_foundation::paths::install_receipt_path(),
            Some(legacy.join("install-receipts/ccync.json"))
        );

        let mcp_path = layout.mcp_input_path();
        fs::create_dir_all(mcp_path.parent().unwrap()).unwrap();
        fs::write(
            &mcp_path,
            r#"{"ccyncMcpServers":{"fixture":{"command":"fixture"}}}"#,
        )
        .unwrap();
        assert!(read_direct_mcp_input(&effective_layout().unwrap())
            .unwrap()
            .unwrap()
            .servers()
            .contains_key("fixture"));

        let catalog_path = layout.personal_catalog_path();
        fs::write(
            &catalog_path,
            r#"{"_adoption":{"targetSnapshot":["claude"]}}"#,
        )
        .unwrap();
        assert_eq!(
            read_init_target_snapshot(&effective_layout().unwrap().personal_catalog_path())
                .unwrap(),
            Some(vec!["claude".to_string()])
        );

        let lock_path = layout.build_lock_path();
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        fs::write(
            &lock_path,
            r#"{"_personalPlugins":[{"pluginId":"fixture","source":"https://example.invalid/fixture"}]}"#,
        )
        .unwrap();
        fs::create_dir_all(layout.source_cache_path().join("fixture@abc")).unwrap();
        let dirs = managed_plugin_dirs();
        assert!(dirs.iter().any(|(id, path)| {
            id == "fixture" && path == &layout.source_cache_path().join("fixture@abc")
        }));

        let dist_targets = ccync_dist_targets_root().unwrap();
        assert_eq!(dist_targets, roots.state.join("dist/targets"));
        fs::create_dir_all(&dist_targets).unwrap();
        fs::write(dist_targets.join("fixture.json"), "{}").unwrap();

        let canonical = home.path().join("canonical");
        generate_managed_mcp(
            &canonical,
            serde_json::from_value(json!({
                "input-derived": {"command": "fixture"}
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(roots.state.join("build/mcp/managed.json").is_file());

        let route_path = crate::managed_view::route_record_path().unwrap();
        crate::managed_view::write_route_record(&route_path, &[]).unwrap();
        assert_eq!(route_path, layout.state_base().join("routes.json"));
        assert!(route_path.is_file());

        fs::create_dir_all(layout.source_cache_path().join("fixture@abc")).unwrap();
        fs::write(
            layout.source_cache_path().join("fixture@abc/plugin.json"),
            "{}",
        )
        .unwrap();
        fs::create_dir_all(layout.dist_path().join("targets/claude")).unwrap();
        fs::write(layout.dist_path().join("targets/claude/managed.json"), "{}").unwrap();
        assert!(layout
            .source_cache_path()
            .join("fixture@abc/plugin.json")
            .is_file());
        assert!(layout
            .dist_path()
            .join("targets/claude/managed.json")
            .is_file());
        assert!(!home.path().join(".ccync/cache").exists());
        assert!(!home.path().join(".ccync/dist").exists());
        assert!(!legacy.join("plugins.json").exists());
        assert!(!legacy.join("mcp.json").exists());
        assert!(!legacy.join("dist/targets/fixture.json").exists());
        assert!(!legacy.join("build/mcp/managed.json").exists());
        assert!(!legacy.join("routes.json").exists());
    }

    #[test]
    fn malformed_desired_source_collections_are_unproven() {
        for key in ["resolvedPlugins", "_personalPlugins", "_looseSkills"] {
            for malformed in [json!({"unexpected": true}), Value::Null] {
                let lock = json!({key: malformed});
                assert_eq!(desired_standalone_origins_from_lock(&lock), None, "{key}");
            }
        }
        assert_eq!(
            desired_standalone_origins_from_lock(&json!({})),
            Some(Vec::new())
        );
        assert_eq!(
            desired_standalone_origins_from_lock(&json!({
                "resolvedPlugins": [],
                "_personalPlugins": [],
                "_looseSkills": []
            })),
            Some(Vec::new())
        );
    }

    #[test]
    fn p7_t05_failed_intent_write_runs_no_native_mutation() {
        use ccync_foundation::provider::{
            EnablementIntent, GenerationId, IntentPhase, NativeSelector, OwnershipFingerprint,
            PackageDigest, PathRoot, PendingIntent, ProviderGeneration, ProviderSourceKey,
            RelativePath, ScopedPath,
        };

        let _env = crate::test_support::env_guard();
        let temp = tempfile::TempDir::new().unwrap();
        std::env::set_var("USERPROFILE", temp.path());
        std::env::set_var("HOME", temp.path());
        std::env::set_var("CCYNC_HOME", temp.path().join(".ccync"));
        // A directory at the journal path makes the journal write fail.
        let journal = provider_journal_path().unwrap();
        fs::create_dir_all(&journal).unwrap();
        let digest = PackageDigest::new("sha256:bb").unwrap();
        let intent = PendingIntent {
            target: ProjectionTargetId::Claude,
            ownership_fingerprints: vec![OwnershipFingerprint {
                selector: NativeSelector::new("demo@ccync").unwrap(),
                package_digest: digest.clone(),
            }],
            scoped_paths: vec![ScopedPath {
                root: PathRoot::UserHome,
                path: RelativePath::new(".claude/skills/demo").unwrap(),
            }],
            preimage_refs: Vec::new(),
            previous_generation: None,
            desired_generation: ProviderGeneration {
                source: ProviderSourceKey::new("demo").unwrap(),
                generation: GenerationId::new("g-0001").unwrap(),
                package_digest: digest,
            },
            phase: IntentPhase::Staged,
            client_scope: None,
            source_digest: None,
            view_digest: None,
            observed_preimage: None,
            enablement_intent: Some(EnablementIntent::Enabled),
            group_substep: None,
        };
        let mut native_calls = 0;

        let result = persist_then_mutate(&intent, || {
            native_calls += 1;
            Ok(())
        });

        assert!(result.is_err(), "a failed intent write must surface");
        assert_eq!(native_calls, 0, "no native call may run without its intent");
    }

    #[test]
    fn provider_lifecycle_prune_keeps_pending_and_committed_generation_only() {
        use ccync_foundation::provider::{
            CapabilityInventory, CommittedOwnership, GenerationId, NativeSelector, PackageDigest,
            ProviderGeneration, ProviderSourceKey,
        };

        let temp = tempfile::TempDir::new().unwrap();
        let build = temp.path().join("build");
        let provider = build.join("render/providers/demo");
        let generation_path = provider.join("g-0001");
        let obsolete_path = provider.join("g-obsolete");
        fs::create_dir_all(&generation_path).unwrap();
        fs::create_dir_all(&obsolete_path).unwrap();
        let generation = ProviderGeneration {
            source: ProviderSourceKey::new("demo").unwrap(),
            generation: GenerationId::new("g-0001").unwrap(),
            package_digest: PackageDigest::new("sha256:abc").unwrap(),
        };
        let selector = NativeSelector::new("demo@ccync").unwrap();
        let committed = CommittedOwnership {
            target: ProjectionTargetId::Claude,
            selector: selector.clone(),
            plugin_name: "demo".to_string(),
            upstream_revision: Some("rev1".to_string()),
            generation: generation.clone(),
            capabilities: CapabilityInventory::new(),
            client_scope: Some(ClientScope::new("demo-client").unwrap()),
            source_digest: Some(PackageDigest::new("sha256:src").unwrap()),
            view_digest: Some(PackageDigest::new("sha256:view").unwrap()),
            observed_preimage: None,
            enablement_intent: None,
            group_substep: None,
        };
        let mut lock = json!({});
        ProviderLifecycleLock {
            schema_version: 1,
            committed: vec![committed],
        }
        .write_to_lock(&mut lock)
        .unwrap();
        fs::create_dir_all(&build).unwrap();
        fs::write(build.join("lock.json"), serde_json::to_vec(&lock).unwrap()).unwrap();
        let intent = PendingIntent {
            target: ProjectionTargetId::Claude,
            ownership_fingerprints: vec![OwnershipFingerprint {
                selector,
                package_digest: generation.package_digest.clone(),
            }],
            scoped_paths: Vec::new(),
            preimage_refs: Vec::new(),
            previous_generation: None,
            desired_generation: generation,
            phase: IntentPhase::Pruning,
            client_scope: None,
            source_digest: None,
            view_digest: None,
            observed_preimage: None,
            enablement_intent: None,
            group_substep: None,
        };
        fs::write(
            build.join("projection.txn.json"),
            ProviderLifecycleJournal {
                schema_version: 1,
                intents: vec![intent],
                ..Default::default()
            }
            .to_json_string()
            .unwrap(),
        )
        .unwrap();

        prune_build_except_retained(&build, false, false, true, &mut Vec::new());

        assert!(build.join("lock.json").exists());
        assert!(build.join("projection.txn.json").exists());
        assert!(generation_path.exists());
        assert!(!obsolete_path.exists());
    }

    #[test]
    fn provider_lifecycle_prune_keeps_all_generations_when_references_unreadable() {
        let temp = tempfile::TempDir::new().unwrap();
        let build = temp.path().join("build");
        let generation_path = build.join("render/providers/demo/g-0001");
        fs::create_dir_all(&generation_path).unwrap();
        fs::write(build.join("lock.json"), "{ not valid json").unwrap();

        let mut warnings = Vec::new();
        prune_build_except_retained(&build, false, false, true, &mut warnings);

        assert!(
            generation_path.exists(),
            "unreadable references must not delete provider generations"
        );
        assert!(warnings
            .iter()
            .any(|warning| warning.contains("keeping all provider generations")));
    }

    /// Lay out `render/providers/demo` with two generations, each owning a
    /// `<g>.views/codex/<view>` directory, and write `lock.json` committing the
    /// given reference. Returns the build directory.
    fn retention_fixture(temp: &tempfile::TempDir, committed_id: &str) -> PathBuf {
        use ccync_foundation::provider::{
            CapabilityInventory, CommittedOwnership, GenerationId, NativeSelector, PackageDigest,
            ProviderGeneration, ProviderSourceKey,
        };

        let build = temp.path().join("build");
        let providers = build.join("render/providers/demo");
        for dir in [
            "g-0001/codex-only",
            "g-0001.views/codex/v-abc",
            "g-obsolete",
            "g-obsolete.views/codex/v-old",
        ] {
            fs::create_dir_all(providers.join(dir)).unwrap();
        }
        let mut lock = json!({});
        ProviderLifecycleLock {
            schema_version: 1,
            committed: vec![CommittedOwnership {
                target: ProjectionTargetId::Codex,
                selector: NativeSelector::new("demo@ccync").unwrap(),
                plugin_name: "demo".to_string(),
                upstream_revision: Some("rev1".to_string()),
                generation: ProviderGeneration {
                    source: ProviderSourceKey::new("demo").unwrap(),
                    generation: GenerationId::new(committed_id).unwrap(),
                    package_digest: PackageDigest::new("sha256:abc").unwrap(),
                },
                capabilities: CapabilityInventory::new(),
                client_scope: Some(ClientScope::new("demo-client").unwrap()),
                source_digest: Some(PackageDigest::new("sha256:src").unwrap()),
                view_digest: Some(PackageDigest::new("sha256:view").unwrap()),
                observed_preimage: None,
                enablement_intent: None,
                group_substep: None,
            }],
        }
        .write_to_lock(&mut lock)
        .unwrap();
        fs::write(build.join("lock.json"), serde_json::to_vec(&lock).unwrap()).unwrap();
        build
    }

    #[test]
    fn delivery_retention_committed_view_keeps_its_generation_and_views() {
        let temp = tempfile::TempDir::new().unwrap();
        let build = retention_fixture(&temp, "v-abc");
        let providers = build.join("render/providers/demo");

        let mut warnings = Vec::new();
        prune_build_except_retained(&build, false, false, true, &mut warnings);

        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(providers.join("g-0001/codex-only").is_dir());
        assert!(providers.join("g-0001.views/codex/v-abc").is_dir());
        assert!(!providers.join("g-obsolete").exists());
        assert!(!providers.join("g-obsolete.views").exists());
    }

    #[test]
    fn delivery_retention_unknown_view_reference_prunes_nothing() {
        let temp = tempfile::TempDir::new().unwrap();
        let build = retention_fixture(&temp, "v-missing");
        let providers = build.join("render/providers/demo");

        let mut warnings = Vec::new();
        prune_build_except_retained(&build, false, false, true, &mut warnings);

        assert!(providers.join("g-obsolete").is_dir());
        assert!(providers.join("g-obsolete.views/codex/v-old").is_dir());
        assert!(providers.join("g-0001.views/codex/v-abc").is_dir());
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("keeping all provider generations")),
            "{warnings:?}"
        );
    }

    #[test]
    fn provider_lifecycle_unverified_uninstall_retains_records_without_cli() {
        use crate::tool_manager::lock::LifecycleLock;
        use ccync_foundation::provider::{
            CapabilityInventory, CommittedOwnership, GenerationId, NativeSelector, PackageDigest,
            ProviderGeneration, ProviderSourceKey,
        };

        let _env = crate::test_support::env_guard();
        let temp = tempfile::TempDir::new().unwrap();
        std::env::set_var("USERPROFILE", temp.path());
        std::env::set_var("HOME", temp.path());
        let build = temp.path().join(".ccync/build");
        fs::create_dir_all(&build).unwrap();
        let generation = ProviderGeneration {
            source: ProviderSourceKey::new("demo").unwrap(),
            generation: GenerationId::new("g-0001").unwrap(),
            package_digest: PackageDigest::new("sha256:abc").unwrap(),
        };
        let selector = NativeSelector::new("demo@ccync").unwrap();
        // Copilot has no native adapter, so its record is retained without any
        // client CLI call. A Claude record would reach the real `claude` binary.
        let mut lock_value = json!({});
        ProviderLifecycleLock {
            schema_version: 1,
            committed: vec![CommittedOwnership {
                target: ProjectionTargetId::Copilot,
                selector: selector.clone(),
                plugin_name: "demo".to_string(),
                upstream_revision: Some("rev1".to_string()),
                generation,
                capabilities: CapabilityInventory::new(),
                client_scope: Some(ClientScope::new("demo-client").unwrap()),
                source_digest: Some(PackageDigest::new("sha256:src").unwrap()),
                view_digest: Some(PackageDigest::new("sha256:view").unwrap()),
                observed_preimage: None,
                enablement_intent: None,
                group_substep: None,
            }],
        }
        .write_to_lock(&mut lock_value)
        .unwrap();
        fs::write(
            build.join("lock.json"),
            serde_json::to_vec(&lock_value).unwrap(),
        )
        .unwrap();

        let guard = LifecycleLock::acquire().unwrap();
        let mut warnings = Vec::new();
        let retained = remove_committed_providers_for_uninstall(
            &guard.held_lock_proof(),
            &mut warnings,
            &crate::plugin_native::ProcessNativeRunner,
        );

        assert!(retained.contains(selector.as_str()));
        assert!(!ProviderLifecycleLock::from_lock(
            &serde_json::from_slice(&fs::read(build.join("lock.json")).unwrap()).unwrap()
        )
        .unwrap()
        .committed
        .is_empty());
    }

    #[test]
    fn provider_lifecycle_uninstall_removes_claude_paths_while_retaining_provider_state() {
        use ccync_foundation::provider::{
            CapabilityInventory, CommittedOwnership, GenerationId, NativeSelector, PackageDigest,
            ProviderGeneration, ProviderSourceKey,
        };

        let _env = crate::test_support::env_guard();
        let temp = tempfile::TempDir::new().unwrap();
        std::env::set_var("USERPROFILE", temp.path());
        std::env::set_var("HOME", temp.path());

        let build = temp.path().join(".ccync/build");
        fs::create_dir_all(build.join("render")).unwrap();
        let generation = ProviderGeneration {
            source: ProviderSourceKey::new("demo").unwrap(),
            generation: GenerationId::new("g-0001").unwrap(),
            package_digest: PackageDigest::new("sha256:abc").unwrap(),
        };
        // Copilot keeps this test off the real `claude` binary. Paths are still removed.
        let mut lock_value = json!({});
        ProviderLifecycleLock {
            schema_version: 1,
            committed: vec![CommittedOwnership {
                target: ProjectionTargetId::Copilot,
                selector: NativeSelector::new("demo@ccync").unwrap(),
                plugin_name: "demo".to_string(),
                upstream_revision: Some("rev1".to_string()),
                generation,
                capabilities: CapabilityInventory::new(),
                client_scope: Some(ClientScope::new("demo-client").unwrap()),
                source_digest: Some(PackageDigest::new("sha256:src").unwrap()),
                view_digest: Some(PackageDigest::new("sha256:view").unwrap()),
                observed_preimage: None,
                enablement_intent: None,
                group_substep: None,
            }],
        }
        .write_to_lock(&mut lock_value)
        .unwrap();
        fs::write(
            build.join("lock.json"),
            serde_json::to_vec(&lock_value).unwrap(),
        )
        .unwrap();

        let claude_skills = temp.path().join(".claude/skills/ccync");
        let claude_plugins = temp.path().join(".claude/plugins/ccync");
        fs::create_dir_all(&claude_skills).unwrap();
        fs::create_dir_all(&claude_plugins).unwrap();
        fs::write(claude_skills.join("skill.md"), "owned skill").unwrap();
        fs::write(claude_plugins.join("plugin.json"), "owned plugin").unwrap();

        let _ = run_uninstall();

        assert!(
            !claude_skills.exists(),
            "Claude skills path must be removed"
        );
        assert!(
            !claude_plugins.exists(),
            "Claude plugins path must be removed"
        );
        assert!(
            build.join("lock.json").exists(),
            "unverified committed provider state must remain available for retry"
        );
    }

    #[test]
    fn install_rs_has_exactly_one_component_dirs_array_literal() {
        let source = include_str!("install.rs").replace("\r\n", "\n");
        let production_source = source
            .split_once("#[cfg(test)]\nmod tests {")
            .map(|(before, _)| before.to_string())
            .expect("install.rs must contain the #[cfg(test)] mod tests boundary");
        let production_lines: Vec<&str> = production_source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect();
        let hits: Vec<&&str> = production_lines
            .iter()
            .filter(|line| line.contains("[\"skills\", \"commands\", \"agents\", \"hooks\"]"))
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "install.rs production code must contain exactly one [\"skills\", \"commands\", \"agents\", \"hooks\"] array literal (in COMPONENT_DIRS const), found {}: {:?}",
            hits.len(),
            hits
        );
    }
    /// Serialize tests that mutate process-global env vars (`USERPROFILE`/`HOME`).
    /// Shared crate-wide lock — env mutation is process-wide and `cargo test`
    /// runs multithreaded, so two such tests racing (even across modules)
    /// would resolve `ccync_home()`-derived paths to each other's temp dirs.
    /// See `crate::test_support::env_guard` for the poison-tolerance rationale.
    use crate::test_support::env_guard;

    /// Regression for the finalize holistic review's HR-03. `lock.json`'s
    /// `agyRootProjectionPaths` rows are the only proof that authorizes
    /// removing a published agy root; deleting them while a copy survives
    /// strands that copy permanently, because `remove_agy_root` requires
    /// `is_owned` and a missing lockfile owns nothing. Under the old junction
    /// shape a survivor was inert once the canonical root was gone — a
    /// directory copy is self-contained and keeps being loaded.
    #[test]
    fn prune_build_retains_lock_json_only_when_an_agy_root_survives() {
        use tempfile::TempDir;

        fn seed(build: &Path) {
            fs::create_dir_all(build.join("mcp")).unwrap();
            fs::write(build.join("mcp").join("managed.json"), "{}").unwrap();
            let lock = build.join("lock.json");
            fs::create_dir_all(build).unwrap();
            projection::AgyRootOwnership::from_lockfile(&lock)
                .record_now(&build.join("agy-root"), false)
                .unwrap();
            // The credential-bearing namespace a real adopted install carries.
            let raw = fs::read_to_string(&lock).unwrap();
            let mut root: serde_json::Map<String, serde_json::Value> =
                match serde_json::from_str(&raw).unwrap() {
                    serde_json::Value::Object(root) => root,
                    other => panic!("lockfile must be an object, got {other:?}"),
                };
            root.insert(
                "_mcpServers".to_string(),
                serde_json::json!({ "github": { "env": { "GITHUB_TOKEN": "ghp_secret" } } }),
            );
            fs::write(
                &lock,
                serde_json::to_string_pretty(&serde_json::Value::Object(root)).unwrap(),
            )
            .unwrap();
            fs::create_dir_all(build.join("render")).unwrap();
        }

        let retained = TempDir::new().unwrap();
        let build = retained.path().join("build");
        seed(&build);
        let mut warnings = Vec::new();
        prune_build_except_retained(&build, false, true, false, &mut warnings);
        assert!(
            build.join("lock.json").is_file(),
            "a surviving agy root must keep its ownership proof"
        );
        let retained_lock = fs::read_to_string(build.join("lock.json")).unwrap();
        assert!(
            retained_lock.contains("agyRootProjectionPaths"),
            "the retained file must still be usable as ownership proof: {retained_lock}"
        );
        assert!(
            !retained_lock.contains("ghp_secret") && !retained_lock.contains("_mcpServers"),
            "retaining the proof must not retain adopted MCP credentials: {retained_lock}"
        );
        assert!(
            !build.join("render").exists(),
            "everything else under build/ is still derived state and goes"
        );
        assert!(
            !build.join("mcp").join("managed.json").exists(),
            "the agy retention must not silently retain MCP state as well"
        );

        let dropped = TempDir::new().unwrap();
        let build = dropped.path().join("build");
        seed(&build);
        let mut warnings = Vec::new();
        prune_build_except_retained(&build, false, false, false, &mut warnings);
        assert!(
            !build.join("lock.json").exists(),
            "with nothing surviving there is nothing to prove ownership of; \
             the lockfile is derived state like the rest"
        );
    }

    #[test]
    fn install_error_display_is_non_empty() {
        assert!(!InstallError::NoHome.to_string().is_empty());
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "not found");
        assert!(InstallError::Io(io_err).to_string().contains("not found"));
        assert!(InstallError::PartialUninstall {
            warnings: vec!["x".to_string()]
        }
        .to_string()
        .contains("partial failures"));
    }

    /// Independent oracle for the legacy default-all order (`claude, codex,
    /// copilot, opencode, agy-cli, agy-ide, agy-gui`), hardcoded
    /// rather than imported from `ccync_foundation::runtime::VALID_RUNTIMES`
    /// — this crate keeps zero consumers of that symbol, production or test.
    /// A passing comparison against `default_selected_runtimes()` /
    /// `resolve_runtime_selection`'s fallback proves the two independently
    /// agree on the exact legacy order, not merely that both read the same
    /// shared symbol.
    const EXPECTED_DEFAULT_RUNTIME_ORDER: [&str; 7] = [
        "claude", "codex", "copilot", "opencode", "agy-cli", "agy-ide", "agy-gui",
    ];

    #[test]
    fn install_default_selected_runtimes_match_projection_fallback_source() {
        // Single source: the install-side absent-state default must be the
        // same set projection falls back to, so a config.json without
        // selectedRuntimes/selectedProjectionTargets defaults to all agents.
        let expected: Vec<String> = EXPECTED_DEFAULT_RUNTIME_ORDER
            .iter()
            .map(|r| r.to_string())
            .collect();
        assert_eq!(default_selected_runtimes(), expected);
        assert!(default_selected_runtimes().iter().any(|r| r == "opencode"));
    }

    // ── resolve_runtime_selection() validation ───────────────────────────────
    //
    // `resolve_runtime_selection` is exercised directly with in-memory
    // `CcyncConfig` values rather than through `load_runtime_selection()`'s
    // `CcyncConfig::load()` → `CcyncConfig::config_path()` path: that path
    // now resolves home via `ccync_foundation::paths::user_home()`
    // (env-based), so the fake-home/env-var seam used elsewhere in this
    // module does reach it. Testing the pure resolver function directly
    // here is still preferred — it is faster and avoids a filesystem round
    // trip for pure-logic validation — not because the seam is unreachable.

    fn config_with(selected: &[&str]) -> CcyncConfig {
        CcyncConfig {
            selected_runtimes: selected.iter().map(|s| s.to_string()).collect(),
            ..CcyncConfig::default()
        }
    }

    #[test]
    fn resolve_runtime_selection_all_blank_keys_does_not_panic_and_falls_back_to_full_default_set()
    {
        // `selectedRuntimes` was present but every entry is blank or
        // unparseable, so nothing usable survives resolution. That must
        // leave the selection empty (a mutating projection run then refuses
        // to proceed) rather than silently falling back to all agents.
        let config = config_with(&["", "   ", "not-a-real-runtime"]);

        let selection = resolve_runtime_selection(config);

        assert!(
            selection.selected_runtimes.is_empty(),
            "all-invalid selectedRuntimes must resolve to an empty selection, not fall back"
        );
    }

    #[test]
    fn resolve_runtime_selection_filters_invalid_keys_and_keeps_valid_ones() {
        let config = config_with(&["claude", "bogus-key", "codex", "gemini", "antigravity"]);

        let selection = resolve_runtime_selection(config);

        assert_eq!(
            selection.selected_runtimes,
            vec!["claude".to_string(), "codex".to_string()],
            "invalid keys must be filtered out while valid keys are kept, order preserved"
        );
    }

    #[test]
    fn resolve_runtime_selection_all_valid_config_is_unchanged() {
        let config = config_with(&["claude", "codex"]);

        let selection = resolve_runtime_selection(config);

        assert_eq!(
            selection.selected_runtimes,
            vec!["claude".to_string(), "codex".to_string()]
        );
    }

    #[test]
    fn both_state_warning_fires_only_for_both_legacy_state() {
        assert!(both_state_warning(LegacyState::Both).is_some());
        assert!(both_state_warning(LegacyState::Canonical).is_none());
        assert!(both_state_warning(LegacyState::Legacy).is_none());
        assert!(both_state_warning(LegacyState::Absent).is_none());
    }

    #[test]
    fn resolve_runtime_selection_both_keys_present_canonical_values_win() {
        // Canonical and legacy hold genuinely different values, so a passing
        // assertion proves canonical truly wins rather than the two fixtures
        // happening to agree (same fixture-design rationale as the
        // foundation resolver's own `resolve_projection_targets_both_present_canonical_wins`).
        let config = CcyncConfig {
            selected_projection_targets: vec!["claude".to_string(), "codex".to_string()],
            selected_runtimes: vec!["agy-gui".to_string()],
            ..CcyncConfig::default()
        };

        let selection = resolve_runtime_selection(config);

        assert_eq!(
            selection.selected_runtimes,
            vec!["claude".to_string(), "codex".to_string()],
            "canonical selectedProjectionTargets must win over legacy selectedRuntimes"
        );
    }

    #[test]
    fn resolve_runtime_selection_empty_config_uses_full_default_set_and_no_warnings() {
        // Absent selectedRuntimes (pre-existing default-empty-vec behavior,
        // unchanged by this fix): falls back to the full default set, same
        // path as before, no invalid-key warning branch is hit.
        let config = CcyncConfig::default();

        let selection = resolve_runtime_selection(config);

        let expected: Vec<String> = EXPECTED_DEFAULT_RUNTIME_ORDER
            .iter()
            .map(|r| r.to_string())
            .collect();
        assert_eq!(selection.selected_runtimes, expected);
    }

    #[test]
    fn projection_target_status_variants_are_exact() {
        // Exhaustive match compiles only if the variant set/shape is exactly
        // NotRun, Applied, Unchanged, Skipped { reason }, Failed { path, reason }.
        fn describe(status: &ProjectionTargetStatus) -> &'static str {
            match status {
                ProjectionTargetStatus::NotRun => "not_run",
                ProjectionTargetStatus::Applied => "applied",
                ProjectionTargetStatus::Unchanged => "unchanged",
                ProjectionTargetStatus::Skipped { reason: _ } => "skipped",
                ProjectionTargetStatus::Failed { path: _, reason: _ } => "failed",
            }
        }

        assert_eq!(describe(&ProjectionTargetStatus::NotRun), "not_run");
        assert_eq!(describe(&ProjectionTargetStatus::Applied), "applied");
        assert_eq!(describe(&ProjectionTargetStatus::Unchanged), "unchanged");
        assert_eq!(
            describe(&ProjectionTargetStatus::Skipped {
                reason: "not selected".to_string()
            }),
            "skipped"
        );
        assert_eq!(
            describe(&ProjectionTargetStatus::Failed {
                path: Some(PathBuf::from("/tmp/x")),
                reason: "io error".to_string()
            }),
            "failed"
        );
        assert_eq!(
            describe(&ProjectionTargetStatus::Failed {
                path: None,
                reason: "no path".to_string()
            }),
            "failed"
        );
    }

    #[test]
    fn projection_run_report_fields_are_accessible_and_mode_is_normal() {
        let target_result = ProjectionTargetResult {
            target: ProjectionTargetId::Claude,
            status: ProjectionTargetStatus::Applied,
            paths: vec![projection::ProjectionPathResult {
                path: PathBuf::from("/tmp/ccync/claude"),
                change: projection::ProjectionPathChange::Linked,
            }],
            warnings: vec![],
        };
        let mut targets = std::collections::BTreeMap::new();
        targets.insert(ProjectionTargetId::Claude, target_result);

        let report = ProjectionRunReport {
            common: vec![],
            targets,
            mode: "normal".to_string(),
            run_warnings: vec![],
        };

        assert_eq!(report.mode, "normal");
        assert_eq!(report.targets.len(), 1);
        assert!(report.common.is_empty());
        assert!(report.run_warnings.is_empty());
        assert_eq!(
            report.targets[&ProjectionTargetId::Claude].status,
            ProjectionTargetStatus::Applied
        );
    }

    #[test]
    fn projection_phase_has_exactly_canonical_and_machine_variants() {
        fn describe(phase: ProjectionPhase) -> &'static str {
            match phase {
                ProjectionPhase::Canonical => "canonical",
                ProjectionPhase::Machine => "machine",
            }
        }
        assert_eq!(describe(ProjectionPhase::Canonical), "canonical");
        assert_eq!(describe(ProjectionPhase::Machine), "machine");
    }

    #[test]
    fn projection_phase_error_variants_are_exact() {
        // Exhaustive match compiles only if the variant set is exactly
        // Install(InstallError), Agy(AgyError), Projection(ProjectionError).
        fn describe(error: &ProjectionPhaseError) -> &'static str {
            match error {
                ProjectionPhaseError::Install(_) => "install",
                ProjectionPhaseError::Agy(_) => "agy",
                ProjectionPhaseError::Projection(_) => "projection",
            }
        }

        let install_err = ProjectionPhaseError::Install(InstallError::NoHome);
        assert_eq!(describe(&install_err), "install");
        assert_eq!(
            install_err.to_string(),
            "could not determine home directory"
        );
        assert!(std::error::Error::source(&install_err).is_some());

        let agy_err = ProjectionPhaseError::Agy(projection::agy::AgyError::LinkCreation(
            "junction denied".to_string(),
        ));
        assert_eq!(describe(&agy_err), "agy");
        assert!(agy_err.to_string().contains("junction denied"));
        assert!(std::error::Error::source(&agy_err).is_some());

        let projection_err = ProjectionPhaseError::Projection(
            projection::ProjectionError::Message("machine phase blew up".to_string()),
        );
        assert_eq!(describe(&projection_err), "projection");
        assert_eq!(projection_err.to_string(), "machine phase blew up");
        assert!(std::error::Error::source(&projection_err).is_some());
    }

    #[test]
    fn projection_phase_failure_fields_are_accessible() {
        // Compile-time lock on ProjectionPhaseFailure { error, partial }: this
        // only compiles if both fields exist with these exact types.
        let failure = ProjectionPhaseFailure {
            error: ProjectionPhaseError::Projection(projection::ProjectionError::Message(
                "machine phase aborted".to_string(),
            )),
            partial: projection::ProjectionEvidence::default(),
        };
        assert_eq!(failure.error.to_string(), "machine phase aborted");
        assert!(failure.partial.common.is_empty());
        assert!(failure.partial.by_target.is_empty());
    }

    #[test]
    fn uninstall_succeeds_when_canonical_root_absent() {
        // If canonical root never existed, uninstall should not error.
        // We test by pointing to a non-existent path.
        // Full isolated-home test is integration/manual.
        // Here we just verify no panic and no unexpected I/O error.
        //
        // Note: run_uninstall() resolves home via ccync_foundation::paths::user_home()
        // (env-based, reachable via a fake-home env-var seam), but we still skip the
        // real filesystem call here and only test the error surface directly.
        let err = InstallError::NoHome;
        assert!(err.to_string().contains("home"));
    }

    // ── load_enabled_bundles() cross-machine / β1 no-op tests ───────────────────

    /// Helper: create a fake lock.json in a temp home and run the loader.
    /// Returns the parsed bundle vec. Accepts `home_path` as USERPROFILE / HOME.
    fn with_fake_lock<F>(home_path: &std::path::Path, lock_contents: &str, f: F)
    where
        F: FnOnce(Vec<(String, String)>),
    {
        // Hold the env lock across the whole env-dependent body so no other
        // env-mutating test runs concurrently.
        let _env = env_guard();

        let build_dir = home_path.join(".ccync").join("build");
        std::fs::create_dir_all(&build_dir).unwrap();
        std::fs::write(build_dir.join("lock.json"), lock_contents).unwrap();

        // Point home at temp dir so plugins_lockfile_path() resolves under it.
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", home_path);
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", home_path);
        }

        let result = load_enabled_bundles();
        f(result);
    }

    fn read_user_mcp_input_text(
        content: &str,
    ) -> Result<Option<serde_json::Map<String, Value>>, String> {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("mcp.json");
        std::fs::write(&path, content).unwrap();
        read_user_mcp_input(&path).map_err(|e| e.to_string())
    }

    #[test]
    fn user_mcp_input_absent_file_returns_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("mcp.json");
        assert!(read_user_mcp_input(&path).unwrap().is_none());
        assert!(!path.exists(), "reading must not create the input file");
    }

    #[test]
    fn user_mcp_input_valid_matrix_is_accepted() {
        let cases = [
            (r#"{"mcpServers":{}}"#, 0),
            (r#"{"mcpServers":{"a":{"command":"npx"}}}"#, 1),
            (
                r#"{"mcpServers":{"a":{"command":"npx","type":"stdio"}}}"#,
                1,
            ),
            (r#"{"mcpServers":{"a":{"url":"https://x.test/mcp"}}}"#, 1),
            (
                r#"{"mcpServers":{"a":{"url":"https://x.test/mcp","type":"http"}}}"#,
                1,
            ),
            (
                r#"{"mcpServers":{"a":{"url":"https://x.test/sse","type":"sse"}}}"#,
                1,
            ),
            (
                r#"{"mcpServers":{"a":{"command":"c"},"b":{"url":"https://x.test"}}}"#,
                2,
            ),
        ];
        for (content, expected_len) in cases {
            let servers = read_user_mcp_input_text(content)
                .unwrap_or_else(|e| panic!("expected accept for {content}: {e}"))
                .expect("present file must yield a map");
            assert_eq!(servers.len(), expected_len, "for {content}");
        }
    }

    #[test]
    fn user_mcp_input_returns_cloned_server_definitions() {
        let servers = read_user_mcp_input_text(
            r#"{"mcpServers":{"memory":{"command":"npx","args":["-y","srv"]}}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(servers["memory"]["command"], "npx");
        assert_eq!(servers["memory"]["args"][1], "srv");
    }

    #[test]
    fn user_mcp_input_invalid_matrix_is_rejected() {
        let cases = [
            // Malformed JSON and wrong top-level shape.
            r#"{"mcpServers":"#,
            r#"[]"#,
            r#"{}"#,
            r#"{"servers":{"a":{"command":"npx"}}}"#,
            r#"{"mcpServers":[{"command":"npx"}]}"#,
            // Server entry is not an object.
            r#"{"mcpServers":{"a":"npx"}}"#,
            // Neither or both transport fields.
            r#"{"mcpServers":{"a":{"type":"stdio"}}}"#,
            r#"{"mcpServers":{"a":{"args":["x"]}}}"#,
            r#"{"mcpServers":{"a":{"command":"npx","url":"https://x.test"}}}"#,
            // Transport fields must be strings.
            r#"{"mcpServers":{"a":{"command":["npx"]}}}"#,
            r#"{"mcpServers":{"a":{"url":null}}}"#,
            // Unknown type, non-string type, and mismatch.
            r#"{"mcpServers":{"a":{"command":"npx","type":"websocket"}}}"#,
            r#"{"mcpServers":{"a":{"command":"npx","type":1}}}"#,
            r#"{"mcpServers":{"a":{"command":"npx","type":"http"}}}"#,
            r#"{"mcpServers":{"a":{"command":"npx","type":"sse"}}}"#,
            r#"{"mcpServers":{"a":{"url":"https://x.test","type":"stdio"}}}"#,
        ];
        for content in cases {
            assert!(
                read_user_mcp_input_text(content).is_err(),
                "expected reject for {content}"
            );
        }
    }

    #[test]
    fn user_mcp_input_unreadable_path_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("mcp.json");
        std::fs::create_dir(&path).unwrap();
        assert!(read_user_mcp_input(&path).is_err());
    }

    #[test]
    fn user_mcp_input_errors_name_path_and_never_echo_content() {
        let secret = "SECRET-TOKEN-7f3a";
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("mcp.json");
        let contents = [
            format!(
                r#"{{"mcpServers":{{"a":{{"command":"npx","url":"https://x.test?k={secret}"}}}}}}"#
            ),
            format!(r#"{{"mcpServers":{{"{secret}":{{"command":"npx","type":"bad-{secret}"}}}}}}"#),
            format!(r#"{{"mcpServers":{{"a":{{"env":"{secret}"#),
        ];
        for content in contents {
            std::fs::write(&path, &content).unwrap();
            let message = read_user_mcp_input(&path).unwrap_err().to_string();
            assert!(message.contains(&path.display().to_string()), "{message}");
            assert!(!message.contains(secret), "leaked content: {message}");
        }
    }

    #[test]
    fn load_adopted_mcp_servers_reads_mcpservers_namespace() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let build_dir = tmp.path().join(".ccync").join("build");
        std::fs::create_dir_all(&build_dir).unwrap();
        std::fs::write(
            build_dir.join("lock.json"),
            r#"{"_mcpServers":{"memory":{"command":"npx","args":["-y","srv"]}},"resolvedPlugins":[]}"#,
        )
        .unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let servers = load_adopted_mcp_servers();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].0, "memory");
        assert_eq!(servers[0].1["command"], "npx");
    }

    #[test]
    fn load_adopted_mcp_servers_absent_namespace_empty() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let build_dir = tmp.path().join(".ccync").join("build");
        std::fs::create_dir_all(&build_dir).unwrap();
        std::fs::write(build_dir.join("lock.json"), r#"{"resolvedPlugins":[]}"#).unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        assert!(load_adopted_mcp_servers().is_empty());
    }

    #[test]
    fn managed_mcp_empty_replaces_prior_one_server_file() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();

        // Canonical root with an empty server map (last server removed).
        let canonical = tmp.path().join("canonical");
        std::fs::create_dir_all(&canonical).unwrap();
        std::fs::write(canonical.join(".mcp.json"), r#"{"servers":{}}"#).unwrap();

        // Lock with no plugins → no bundle/adopted servers.
        let build_dir = tmp.path().join(".ccync").join("build");
        std::fs::create_dir_all(&build_dir).unwrap();
        std::fs::write(build_dir.join("lock.json"), r#"{"resolvedPlugins":[]}"#).unwrap();

        // Pre-existing managed.json with one server (a stale prior projection).
        let mcp_dir = build_dir.join("mcp");
        std::fs::create_dir_all(&mcp_dir).unwrap();
        std::fs::write(
            mcp_dir.join("managed.json"),
            r#"{"servers":{"old":{"command":"x"}}}"#,
        )
        .unwrap();

        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        generate_managed_mcp(&canonical, serde_json::Map::new()).unwrap();

        let written = std::fs::read_to_string(mcp_dir.join("managed.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert!(v.get("servers").is_some(), "internal servers key retained");
        assert!(
            v["servers"].as_object().unwrap().is_empty(),
            "prior one-server file replaced by an empty server map"
        );
    }

    #[test]
    fn generate_managed_mcp_corrupt_core_fails_closed_preserves_managed() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();

        let canonical = tmp.path().join("canonical");
        std::fs::create_dir_all(&canonical).unwrap();
        let plugin = tmp.path().join(".ccync/plugins/core");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join(".mcp.json"),
            r#"{"servers":{"github":{"command":"SECRET_MARKER_TOKEN"},}"#,
        )
        .unwrap();

        let build_dir = tmp.path().join(".ccync").join("build");
        std::fs::create_dir_all(&build_dir).unwrap();
        std::fs::write(build_dir.join("lock.json"), r#"{"resolvedPlugins":[{"pluginId":"core","installStrategy":"bundled-local","resolvedSource":"core"}]}"#).unwrap();

        // Prior managed.json holding the user's projected server.
        let mcp_dir = build_dir.join("mcp");
        std::fs::create_dir_all(&mcp_dir).unwrap();
        let prior = r#"{"servers":{"github":{"command":"x"}}}"#;
        std::fs::write(mcp_dir.join("managed.json"), prior).unwrap();

        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let err = generate_managed_mcp(&canonical, serde_json::Map::new())
            .expect_err("corrupt .mcp.json must fail closed, not project an empty set");
        let msg = format!("{err}");
        assert!(
            msg.contains(".mcp.json"),
            "error must name the offending file path: {msg}"
        );
        assert!(
            !msg.contains("SECRET_MARKER_TOKEN"),
            "error must never echo file content: {msg}"
        );

        // The prior managed manifest is left byte-for-byte untouched.
        let after = std::fs::read_to_string(mcp_dir.join("managed.json")).unwrap();
        assert_eq!(after, prior, "managed.json must be untouched on failure");
    }

    #[test]
    fn generate_managed_mcp_absent_core_mcp_still_succeeds() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();

        // Canonical root exists but has no .mcp.json at all — absence is the
        // legitimate "no servers desired" signal and must stay a silent skip.
        let canonical = tmp.path().join("canonical");
        std::fs::create_dir_all(&canonical).unwrap();

        let build_dir = tmp.path().join(".ccync").join("build");
        std::fs::create_dir_all(&build_dir).unwrap();
        std::fs::write(build_dir.join("lock.json"), r#"{"resolvedPlugins":[]}"#).unwrap();

        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        generate_managed_mcp(&canonical, serde_json::Map::new())
            .expect("absent .mcp.json must not be an error");
        let written = std::fs::read_to_string(build_dir.join("mcp").join("managed.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert!(v["servers"].as_object().unwrap().is_empty());
    }

    #[test]
    fn render_canonical_root_corrupt_plugin_mcp_fails_closed_untouched() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let plugin = tmp.path().join("badplug");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join(".mcp.json"),
            r#"{"mcpServers":{"m":{"command":"SECRET_MARKER_TOKEN"},}"#,
        )
        .unwrap();

        // Pre-existing canonical render from a prior successful pass.
        let canonical = tmp.path().join("canonical");
        std::fs::create_dir_all(&canonical).unwrap();
        let prior = r#"{"servers":{"kept":{"command":"y"}}}"#;
        std::fs::write(canonical.join(".mcp.json"), prior).unwrap();

        let err = render_canonical_root(&canonical, &[("plugin".into(), plugin)])
            .expect_err("corrupt plugin .mcp.json must abort the render");
        let msg = format!("{err}");
        assert!(
            msg.contains(".mcp.json"),
            "error must name the offending file path: {msg}"
        );
        assert!(
            !msg.contains("SECRET_MARKER_TOKEN"),
            "error must never echo file content: {msg}"
        );

        // Validate-before-prune: the prior canonical .mcp.json survives, so the
        // downstream managed-manifest generation still sees the last good set.
        let after = std::fs::read_to_string(canonical.join(".mcp.json")).unwrap();
        assert_eq!(
            after, prior,
            "canonical .mcp.json must be untouched on failure"
        );
    }

    #[test]
    fn declared_mcp_prewrite_invalid_input_rejects_before_canonical_root_and_leaves_absent_root() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let ccync_dir = tmp.path().join(".ccync");
        std::fs::create_dir_all(&ccync_dir).unwrap();
        let mcp_path = ccync_dir.join("mcp.json");
        // Invalid input: transport conflict (both command and url)
        std::fs::write(
            &mcp_path,
            r#"{"mcpServers":{"conflict":{"command":"npx","url":"https://x.test"}}}"#,
        )
        .unwrap();

        let canonical_root = tmp.path().join(".ccync").join("build").join("render");
        let managed_path = tmp
            .path()
            .join(".ccync")
            .join("build")
            .join("mcp")
            .join("managed.json");

        assert!(
            !canonical_root.exists(),
            "canonical root must be absent initially"
        );
        assert!(
            !managed_path.exists(),
            "managed.json must be absent initially"
        );

        let err = run_update_with_evidence_using(
            &CcyncConfig::default(),
            &crate::delivery_apply::UnavailableNativeRunner,
        )
        .expect_err("invalid mcp.json must reject before canonical render");

        match err {
            InstallError::Io(e) => {
                let msg = e.to_string();
                assert!(msg.contains("mcp.json"), "error must name the file: {msg}");
            }
            other => panic!("expected InstallError::Io, got: {other:?}"),
        }

        assert!(
            !canonical_root.exists(),
            "absent canonical root must remain absent on rejection"
        );
        assert!(
            !managed_path.exists(),
            "managed.json must remain absent on rejection"
        );
    }

    #[test]
    fn declared_mcp_prewrite_invalid_input_preserves_existing_canonical_and_managed_bytes() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let ccync_dir = tmp.path().join(".ccync");
        let canonical_root = ccync_dir.join("build").join("render");
        let mcp_dir = ccync_dir.join("build").join("mcp");
        std::fs::create_dir_all(&canonical_root).unwrap();
        std::fs::create_dir_all(&mcp_dir).unwrap();

        let prior_canonical_content = r#"{"servers":{"core_srv":{"command":"keep"}}}"#;
        let canonical_file = canonical_root.join(".mcp.json");
        std::fs::write(&canonical_file, prior_canonical_content).unwrap();

        let prior_managed_content = r#"{"servers":{"prior_managed":{"command":"untouched"}}}"#;
        let managed_file = mcp_dir.join("managed.json");
        std::fs::write(&managed_file, prior_managed_content).unwrap();

        let prior_canonical_bytes = std::fs::read(&canonical_file).unwrap();
        let prior_managed_bytes = std::fs::read(&managed_file).unwrap();

        // Write invalid mcp.json (malformed JSON with syntax error)
        let mcp_path = ccync_dir.join("mcp.json");
        std::fs::write(&mcp_path, r#"{"mcpServers":{"broken": {"command": "x",}}}"#).unwrap();

        let err = run_update_with_evidence_using(
            &CcyncConfig::default(),
            &crate::delivery_apply::UnavailableNativeRunner,
        )
        .expect_err("corrupt mcp.json must fail before writes");

        match err {
            InstallError::Io(e) => {
                let msg = e.to_string();
                assert!(msg.contains("mcp.json"), "error must name the file: {msg}");
            }
            other => panic!("expected InstallError::Io, got: {other:?}"),
        }

        let after_canonical_bytes = std::fs::read(&canonical_file).unwrap();
        let after_managed_bytes = std::fs::read(&managed_file).unwrap();
        assert_eq!(
            after_canonical_bytes, prior_canonical_bytes,
            "canonical .mcp.json bytes must be identical before and after rejection"
        );
        assert_eq!(
            after_managed_bytes, prior_managed_bytes,
            "managed.json bytes must be identical before and after rejection"
        );
    }

    #[test]
    fn declared_mcp_prewrite_valid_merge_user_first_wins_over_core_and_adopted() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());
        std::fs::create_dir_all(tmp.path().join(".agents")).unwrap();

        let ccync_dir = tmp.path().join(".ccync");
        let build_dir = ccync_dir.join("build");
        let canonical_root = build_dir.join("render");
        std::fs::create_dir_all(&canonical_root).unwrap();

        let plugin = ccync_dir.join("plugins/core");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join(".mcp.json"),
            r#"{"servers":{"shared_srv":{"command":"core-cmd"},"core_only":{"command":"core-only-cmd"}}}"#,
        )
        .unwrap();

        // Adopted master (_mcpServers in lock.json) defines "shared_srv" and "adopted_only"
        std::fs::write(
            build_dir.join("lock.json"),
            r#"{"_mcpServers":{"shared_srv":{"command":"adopted-cmd"},"adopted_only":{"command":"adopted-only-cmd"}},"resolvedPlugins":[{"pluginId":"core","installStrategy":"bundled-local","resolvedSource":"core"}]}"#,
        )
        .unwrap();

        // User declarations define "shared_srv" and "user_only"
        let mut user_servers = serde_json::Map::new();
        user_servers.insert(
            "shared_srv".to_string(),
            serde_json::json!({"command": "user-cmd", "type": "stdio"}),
        );
        user_servers.insert(
            "user_only".to_string(),
            serde_json::json!({"url": "https://user.test/sse", "type": "sse"}),
        );

        std::fs::write(
            effective_layout().unwrap().mcp_input_path(),
            serde_json::to_vec(&json!({"ccyncMcpServers": user_servers})).unwrap(),
        )
        .unwrap();
        generate_managed_mcp(&canonical_root, serde_json::Map::new()).unwrap();

        let managed_path = build_dir.join("mcp").join("managed.json");
        let managed_str = std::fs::read_to_string(&managed_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&managed_str).unwrap();
        let servers = v["servers"].as_object().unwrap();

        // User must win the clash on shared_srv
        assert_eq!(servers["shared_srv"]["command"], "user-cmd");
        // Core only and adopted only must be present
        assert_eq!(servers["core_only"]["command"], "core-only-cmd");
        assert_eq!(servers["adopted_only"]["command"], "adopted-only-cmd");
        assert_eq!(servers["user_only"]["url"], "https://user.test/sse");
    }

    #[test]
    fn declared_mcp_prewrite_collision_warnings_name_retained_and_ignored_sources() {
        let mut user_map = serde_json::Map::new();
        user_map.insert(
            "clash".to_string(),
            serde_json::json!({"command": "user-bin"}),
        );

        let mut core_map = serde_json::Map::new();
        core_map.insert(
            "clash".to_string(),
            serde_json::json!({"command": "core-bin"}),
        );

        let mut adopted_map = serde_json::Map::new();
        adopted_map.insert(
            "clash".to_string(),
            serde_json::json!({"command": "adopted-bin"}),
        );

        let sources = vec![
            ("user declarations".to_string(), user_map),
            ("core (.mcp.json)".to_string(), core_map),
            ("adopted master".to_string(), adopted_map),
        ];

        let (merged, warnings) = merge_mcp_sources_with_warnings(sources);
        assert_eq!(merged["clash"]["command"], "user-bin");
        assert_eq!(warnings.len(), 2);
        assert_eq!(
            warnings[0],
            "MCP server 'clash': keeping definition from user declarations, ignoring duplicate from core (.mcp.json)"
        );
        assert_eq!(
            warnings[1],
            "MCP server 'clash': keeping definition from user declarations, ignoring duplicate from adopted master"
        );
    }

    #[test]
    fn declared_mcp_prewrite_placeholders_preserved_in_managed_output() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());
        std::fs::create_dir_all(tmp.path().join(".agents")).unwrap();

        let canonical_root = tmp.path().join(".ccync").join("build").join("render");
        std::fs::create_dir_all(&canonical_root).unwrap();

        let mut user_servers = serde_json::Map::new();
        user_servers.insert(
            "templated".to_string(),
            serde_json::json!({
                "command": "srv",
                "args": ["--port", "${PORT:-8080}"],
                "env": {
                    "TOKEN": "${SECRET_KEY}",
                    "HOST": "${HOST:-localhost}"
                }
            }),
        );

        std::fs::write(
            effective_layout().unwrap().mcp_input_path(),
            serde_json::to_vec(&json!({"ccyncMcpServers": user_servers})).unwrap(),
        )
        .unwrap();
        generate_managed_mcp(&canonical_root, serde_json::Map::new()).unwrap();

        let managed_path = tmp
            .path()
            .join(".ccync")
            .join("build")
            .join("mcp")
            .join("managed.json");
        let managed_str = std::fs::read_to_string(&managed_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&managed_str).unwrap();
        let srv = &v["servers"]["templated"];

        assert_eq!(srv["args"][1], "${PORT:-8080}");
        assert_eq!(srv["env"]["TOKEN"], "${SECRET_KEY}");
        assert_eq!(srv["env"]["HOST"], "${HOST:-localhost}");
    }

    #[test]
    fn declared_mcp_prewrite_prune_replaces_prior_managed_manifest_with_empty_servers_map() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let build_dir = tmp.path().join(".ccync").join("build");
        let canonical_root = build_dir.join("render");
        let mcp_dir = build_dir.join("mcp");
        std::fs::create_dir_all(&canonical_root).unwrap();
        std::fs::create_dir_all(&mcp_dir).unwrap();

        // Prior managed.json has a server that was removed
        std::fs::write(
            mcp_dir.join("managed.json"),
            r#"{"servers":{"removed_srv":{"command":"old"}}}"#,
        )
        .unwrap();

        // Empty user input and empty canonical .mcp.json
        std::fs::write(canonical_root.join(".mcp.json"), r#"{"servers":{}}"#).unwrap();
        std::fs::write(build_dir.join("lock.json"), r#"{"resolvedPlugins":[]}"#).unwrap();

        generate_managed_mcp(&canonical_root, serde_json::Map::new()).unwrap();

        let managed_str = std::fs::read_to_string(mcp_dir.join("managed.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&managed_str).unwrap();
        assert!(v.get("servers").is_some(), "servers key must exist");
        assert!(
            v["servers"].as_object().unwrap().is_empty(),
            "all servers must be pruned and replaced with empty map"
        );
    }

    #[test]
    fn declared_mcp_prewrite_idempotency_produces_identical_bytes() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());
        std::fs::create_dir_all(tmp.path().join(".agents")).unwrap();

        let canonical_root = tmp.path().join(".ccync").join("build").join("render");
        std::fs::create_dir_all(&canonical_root).unwrap();

        let mut user_servers = serde_json::Map::new();
        user_servers.insert(
            "srv".to_string(),
            serde_json::json!({"command": "node", "args": ["index.js"]}),
        );

        std::fs::write(
            effective_layout().unwrap().mcp_input_path(),
            serde_json::to_vec(&json!({"ccyncMcpServers": user_servers})).unwrap(),
        )
        .unwrap();
        generate_managed_mcp(&canonical_root, serde_json::Map::new()).unwrap();
        let managed_path = tmp
            .path()
            .join(".ccync")
            .join("build")
            .join("mcp")
            .join("managed.json");
        let first_pass_bytes = std::fs::read(&managed_path).unwrap();

        generate_managed_mcp(&canonical_root, serde_json::Map::new()).unwrap();
        let second_pass_bytes = std::fs::read(&managed_path).unwrap();

        assert_eq!(
            first_pass_bytes, second_pass_bytes,
            "repeated generation must produce byte-identical output"
        );
    }

    #[test]
    fn declared_mcp_prewrite_run_update_with_evidence_end_to_end_merges_user_declaration() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let ccync_dir = tmp.path().join(".ccync");
        std::fs::create_dir_all(&ccync_dir).unwrap();
        let mcp_path = ccync_dir.join("mcp.json");
        std::fs::write(
            &mcp_path,
            r#"{"mcpServers":{"my_user_srv":{"command":"my-cmd","args":["arg1"]}}}"#,
        )
        .unwrap();

        let outcome = run_update_with_evidence_using(
            &CcyncConfig::default(),
            &crate::delivery_apply::UnavailableNativeRunner,
        )
        .expect("run_update_with_evidence should succeed with valid mcp.json");
        assert!(outcome.fault.is_none());

        let managed_path = ccync_dir.join("build").join("mcp").join("managed.json");
        let managed_str = std::fs::read_to_string(&managed_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&managed_str).unwrap();
        assert_eq!(
            v["servers"]["my_user_srv"]["command"], "my-cmd",
            "user declared server must reach managed.json"
        );
        assert_eq!(
            v["servers"]["my_user_srv"]["args"][0], "arg1",
            "user declared server args must reach managed.json"
        );
    }

    #[test]
    fn load_enabled_bundles_absent_lock_returns_empty() {
        // β1 not landed (no lock file) → (b) degrades to no-op.
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        let bundles = load_enabled_bundles();
        assert!(
            bundles.is_empty(),
            "absent lock must produce empty bundle list (β1 no-op)"
        );
    }

    #[test]
    fn load_enabled_bundles_bundled_local_entries_parsed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let lock = r#"{
            "generatedAt": "2026-06-19T00:00:00Z",
            "resolvedPlugins": [
                {
                    "pluginId": "ccync-game-assets",
                    "installStrategy": "bundled-local",
                    "resolvedSource": "ccync-domain/ccync-game-assets"
                },
                {
                    "pluginId": "ccync-godot",
                    "installStrategy": "bundled-local",
                    "resolvedSource": "ccync-domain/ccync-godot"
                }
            ]
        }"#;
        with_fake_lock(tmp.path(), lock, |bundles| {
            assert_eq!(bundles.len(), 2);
            assert_eq!(bundles[0].0, "ccync-game-assets");
            assert_eq!(bundles[0].1, "ccync-domain/ccync-game-assets");
            assert_eq!(bundles[1].0, "ccync-godot");
            assert_eq!(bundles[1].1, "ccync-domain/ccync-godot");
        });
    }

    #[test]
    fn load_enabled_bundles_non_bundled_entries_filtered() {
        let tmp = tempfile::TempDir::new().unwrap();
        let lock = r#"{
            "resolvedPlugins": [
                {
                    "pluginId": "some-git-plugin",
                    "installStrategy": "git-clone",
                    "resolvedSource": "https://github.com/example/plugin.git"
                },
                {
                    "pluginId": "ccync-web-testing",
                    "installStrategy": "bundled-local",
                    "resolvedSource": "ccync-domain/ccync-web-testing"
                }
            ]
        }"#;
        with_fake_lock(tmp.path(), lock, |bundles| {
            assert_eq!(bundles.len(), 1, "git-clone entry must be filtered out");
            assert_eq!(bundles[0].0, "ccync-web-testing");
        });
    }

    #[test]
    fn load_enabled_bundles_empty_resolved_plugins_returns_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let lock = r#"{ "resolvedPlugins": [] }"#;
        with_fake_lock(tmp.path(), lock, |bundles| {
            assert!(
                bundles.is_empty(),
                "empty resolvedPlugins must produce empty bundle list"
            );
        });
    }

    #[test]
    fn load_enabled_bundles_malformed_json_returns_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        with_fake_lock(tmp.path(), "not valid json {{{", |bundles| {
            assert!(
                bundles.is_empty(),
                "malformed JSON must degrade to empty (fail-safe)"
            );
        });
    }

    #[test]
    fn load_enabled_bundles_missing_plugin_id_entry_skipped() {
        let tmp = tempfile::TempDir::new().unwrap();
        let lock = r#"{
            "resolvedPlugins": [
                {
                    "installStrategy": "bundled-local",
                    "resolvedSource": "ccync-domain/ccync-pdf"
                },
                {
                    "pluginId": "ccync-pdf",
                    "installStrategy": "bundled-local",
                    "resolvedSource": "ccync-domain/ccync-pdf"
                }
            ]
        }"#;
        with_fake_lock(tmp.path(), lock, |bundles| {
            assert_eq!(bundles.len(), 1, "entry missing pluginId must be skipped");
            assert_eq!(bundles[0].0, "ccync-pdf");
        });
    }

    // ── apply_lifecycle_artifact_chain() marketplace mapping tests ───────────
    //
    // The "ccync" self-entry must point at the absolute canonical root passed
    // in by the caller (not the historical relative "./ccync"); bundle entries
    // must keep the relative "./{bundle_src}" form untouched. These tests
    // exercised `write_surface_lifecycle_artifacts` directly before it was
    // retired in favor of `apply_lifecycle_artifact_chain` — same real
    // JSON-writing work, reached through the evidence-producing chain instead.

    #[test]
    fn apply_lifecycle_artifact_chain_ccync_entry_uses_absolute_canonical_root() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        // No config.json present → resolve_runtime_selection defaults to the
        // full default runtime set, so both the claude and codex primary-surface
        // branches run in this single call.
        let canonical_root = tmp.path().join("fake-canonical-root");
        std::fs::create_dir_all(&canonical_root).unwrap();

        let outcome = apply_lifecycle_artifact_chain_with(&canonical_root, true, true, true);
        assert!(
            outcome.failure_reason.is_none(),
            "apply_lifecycle_artifact_chain should succeed against a fake home: {:?}",
            outcome.failure_reason
        );

        // ── claude form: "source" field on the marketplace.json ccync entry ──
        let claude_manifest_path = claude_marketplace_manifest_path().unwrap();
        let claude_manifest: Value =
            serde_json::from_str(&std::fs::read_to_string(&claude_manifest_path).unwrap()).unwrap();
        let claude_plugins = claude_manifest["plugins"].as_array().unwrap();
        let claude_ccync_entry = claude_plugins
            .iter()
            .find(|p| p["name"] == "ccync")
            .expect("claude marketplace must contain a ccync self-entry");
        let claude_source = claude_ccync_entry["source"].as_str().unwrap();
        assert_eq!(
            std::path::Path::new(claude_source).canonicalize().unwrap(),
            canonical_root.canonicalize().unwrap(),
            "claude ccync entry `source` must canonicalize to the passed-in canonical_root"
        );
        assert_ne!(
            claude_source, "./ccync",
            "claude ccync entry must no longer use the relative \"./ccync\" literal"
        );

        // ── codex form: nested "source":{"path":...} field ──
        let codex_manifest_path = codex_marketplace_manifest_path().unwrap();
        let codex_manifest: Value =
            serde_json::from_str(&std::fs::read_to_string(&codex_manifest_path).unwrap()).unwrap();
        let codex_plugins = codex_manifest["plugins"].as_array().unwrap();
        let codex_ccync_entry = codex_plugins
            .iter()
            .find(|p| p["name"] == "ccync")
            .expect("codex marketplace must contain a ccync self-entry");
        let codex_path = codex_ccync_entry["source"]["path"].as_str().unwrap();
        assert_eq!(
            std::path::Path::new(codex_path).canonicalize().unwrap(),
            canonical_root.canonicalize().unwrap(),
            "codex ccync entry nested `source.path` must canonicalize to the passed-in canonical_root"
        );
        assert_ne!(
            codex_path, "./ccync",
            "codex ccync entry must no longer use the relative \"./ccync\" literal"
        );
    }

    // ── report only successful selected runtime outcomes ──────────────────────

    #[test]
    fn runtime_report_outcomes_excludes_unselected_copilot() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        // Select only claude — copilot must never appear in the report even
        // though `run_update_with_evidence` used to unconditionally seed it.
        let cfg_path = ccync_foundation::paths::machine_config_path().unwrap();
        std::fs::create_dir_all(cfg_path.parent().unwrap()).unwrap();
        std::fs::write(
            &cfg_path,
            r#"{"selectedRuntimes":["claude"],"primaryRuntime":"claude"}"#,
        )
        .unwrap();

        let canonical_root = tmp.path().join("fake-canonical-root");
        std::fs::create_dir_all(&canonical_root).unwrap();

        let outcome = apply_lifecycle_artifact_chain_with(&canonical_root, true, true, true);

        assert!(
            outcome.completed.contains(&"claude"),
            "the selected+applied runtime must still be reported: {:?}",
            outcome.completed
        );
        assert!(
            !outcome.completed.contains(&"copilot"),
            "an unselected runtime must never appear in the report, seeded or not: {:?}",
            outcome.completed
        );
    }

    #[test]
    fn runtime_report_outcomes_excludes_selected_runtime_whose_apply_failed() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        // Select only codex, then inject a real failure at codex's apply
        // point: block its marketplace-manifest parent directory with a
        // *file* at that exact path, so `fs::create_dir_all` (inside
        // `write_json_file`) fails with a real I/O error.
        let cfg_path = ccync_foundation::paths::machine_config_path().unwrap();
        std::fs::create_dir_all(cfg_path.parent().unwrap()).unwrap();
        std::fs::write(
            &cfg_path,
            r#"{"selectedRuntimes":["codex"],"primaryRuntime":"codex"}"#,
        )
        .unwrap();

        let blocked_dir = codex_marketplace_manifest_path().unwrap();
        let blocked_parent = blocked_dir.parent().unwrap();
        std::fs::create_dir_all(blocked_parent.parent().unwrap()).unwrap();
        std::fs::write(blocked_parent, b"not a directory").unwrap();

        let canonical_root = tmp.path().join("fake-canonical-root");
        std::fs::create_dir_all(&canonical_root).unwrap();

        let outcome = apply_lifecycle_artifact_chain_with(&canonical_root, true, true, true);

        assert!(
            outcome.failure_reason.is_some(),
            "the blocked codex apply must fail, not silently succeed"
        );
        assert!(
            !outcome.completed.contains(&"codex"),
            "a selected runtime whose apply failed must never be reported: {:?}",
            outcome.completed
        );
    }

    /// D-04 coverage: `apply_lifecycle_artifact_chain` doesn't just push
    /// runtime names for legacy reconstruction — it composes real,
    /// per-target `ProjectionEvidence` covering Claude/Copilot/Codex and the
    /// AGY runtime-state write's `common` attribution, for a future
    /// consumer (e.g. an accumulator) to read.
    #[test]
    fn apply_lifecycle_artifact_chain_composes_full_evidence_for_all_targets() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let cfg_path = ccync_foundation::paths::machine_config_path().unwrap();
        select_runtimes_config(
            &cfg_path,
            r#"["claude","copilot","codex","agy-cli"]"#,
            "claude",
        );

        let canonical_root = tmp.path().join("fake-canonical-root");
        std::fs::create_dir_all(canonical_root.join("skills")).unwrap();

        let outcome = apply_lifecycle_artifact_chain_with(&canonical_root, true, true, true);

        assert!(
            outcome.failure_reason.is_none(),
            "{:?}",
            outcome.failure_reason
        );
        for target in [
            ProjectionTargetId::Claude,
            ProjectionTargetId::Copilot,
            ProjectionTargetId::Codex,
        ] {
            assert!(
                outcome.evidence.completed_targets.contains(&target),
                "{target:?} must be in the composed evidence's completed_targets"
            );
            assert!(
                !outcome
                    .evidence
                    .by_target
                    .get(&target)
                    .map(|te| te.paths.is_empty())
                    .unwrap_or(true),
                "{target:?} must carry real path evidence, not just a completed marker"
            );
        }
        assert!(
            !outcome.evidence.common.is_empty(),
            "the AGY runtime-state write has no single owning target — it must \
             attribute to `common`: {:?}",
            outcome.evidence.common
        );
        assert!(
            !outcome
                .evidence
                .by_target
                .contains_key(&ProjectionTargetId::AgyCli),
            "the AGY plugin-junction step is not part of this chain — this test \
             never called apply_agy_evidence, so AgyCli must not appear here"
        );
    }

    #[test]
    fn apply_lifecycle_artifact_chain_bundle_entries_stay_relative() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        // Enable one bundled-local plugin via the lockfile so the
        // `enabled_bundles` loop actually runs and appends a bundle entry to
        // both marketplace manifests alongside the "ccync" self-entry.
        let build_dir = tmp.path().join(".ccync").join("build");
        std::fs::create_dir_all(&build_dir).unwrap();
        std::fs::write(
            build_dir.join("lock.json"),
            r#"{
                "resolvedPlugins": [
                    {
                        "pluginId": "ccync-game-assets",
                        "installStrategy": "bundled-local",
                        "resolvedSource": "ccync-domain/ccync-game-assets"
                    }
                ]
            }"#,
        )
        .unwrap();

        let canonical_root = tmp.path().join("fake-canonical-root");
        std::fs::create_dir_all(&canonical_root).unwrap();

        let outcome = apply_lifecycle_artifact_chain_with(&canonical_root, true, true, true);
        assert!(
            outcome.failure_reason.is_none(),
            "apply_lifecycle_artifact_chain should succeed against a fake home: {:?}",
            outcome.failure_reason
        );

        // ── claude bundle entry stays "./{bundle_src}" byte-for-byte ──
        let claude_manifest_path = claude_marketplace_manifest_path().unwrap();
        let claude_manifest: Value =
            serde_json::from_str(&std::fs::read_to_string(&claude_manifest_path).unwrap()).unwrap();
        let claude_plugins = claude_manifest["plugins"].as_array().unwrap();
        let claude_bundle_entry = claude_plugins
            .iter()
            .find(|p| p["name"] == "ccync-game-assets")
            .expect("claude marketplace must contain the enabled bundle entry");
        assert_eq!(
            claude_bundle_entry["source"].as_str().unwrap(),
            "./ccync-domain/ccync-game-assets",
            "claude bundle entry source must stay the relative \"./{{bundle_src}}\" form, byte-for-byte"
        );

        // ── codex bundle entry stays "./{bundle_src}" byte-for-byte ──
        let codex_manifest_path = codex_marketplace_manifest_path().unwrap();
        let codex_manifest: Value =
            serde_json::from_str(&std::fs::read_to_string(&codex_manifest_path).unwrap()).unwrap();
        let codex_plugins = codex_manifest["plugins"].as_array().unwrap();
        let codex_bundle_entry = codex_plugins
            .iter()
            .find(|p| p["name"] == "ccync-game-assets")
            .expect("codex marketplace must contain the enabled bundle entry");
        assert_eq!(
            codex_bundle_entry["source"]["path"].as_str().unwrap(),
            "./ccync-domain/ccync-game-assets",
            "codex bundle entry nested source.path must stay the relative \"./{{bundle_src}}\" form, byte-for-byte"
        );

        // Sanity: the "ccync" self-entry in the same manifest is still the
        // absolute canonical_root, not "./ccync" — confirms both branches
        // (self-entry vs bundle entry) are exercised together in one manifest.
        let claude_ccync_entry = claude_plugins
            .iter()
            .find(|p| p["name"] == "ccync")
            .expect("claude marketplace must also contain the ccync self-entry");
        assert_ne!(claude_ccync_entry["source"].as_str().unwrap(), "./ccync");
    }

    // ── apply_claude_copilot_evidence() ────────────────────────────────────
    //
    // Evidence-reporting sibling of the retired legacy lifecycle-artifact
    // chain, scoped to just the claude/copilot branches. Mirrors the
    // fixture-setup style of the `apply_lifecycle_artifact_chain_*` tests
    // above: a fake home via `USERPROFILE`/`HOME`, and a `config.json`
    // written directly to `machine_config_path()` to control
    // `selectedRuntimes`.

    fn select_runtimes_config(cfg_path: &std::path::Path, runtimes: &str, primary: &str) {
        std::fs::create_dir_all(cfg_path.parent().unwrap()).unwrap();
        std::fs::write(
            cfg_path,
            format!(r#"{{"selectedRuntimes":{runtimes},"primaryRuntime":"{primary}"}}"#),
        )
        .unwrap();
    }

    #[test]
    fn apply_claude_copilot_evidence_neither_selected_is_default_evidence() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let cfg_path = ccync_foundation::paths::machine_config_path().unwrap();
        select_runtimes_config(&cfg_path, r#"["codex"]"#, "codex");

        let canonical_root = tmp.path().join("fake-canonical-root");
        std::fs::create_dir_all(&canonical_root).unwrap();

        let evidence = apply_claude_copilot_evidence(&canonical_root).unwrap();

        assert_eq!(evidence, projection::ProjectionEvidence::default());
    }

    #[test]
    fn apply_claude_copilot_evidence_both_succeed_records_both_targets_written_and_completed() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let cfg_path = ccync_foundation::paths::machine_config_path().unwrap();
        select_runtimes_config(&cfg_path, r#"["claude","copilot"]"#, "claude");

        let canonical_root = tmp.path().join("fake-canonical-root");
        std::fs::create_dir_all(&canonical_root).unwrap();

        let evidence = apply_claude_copilot_evidence(&canonical_root).unwrap();

        assert!(evidence
            .completed_targets
            .contains(&ProjectionTargetId::Claude));
        assert!(evidence
            .completed_targets
            .contains(&ProjectionTargetId::Copilot));

        let claude_evidence = evidence.by_target.get(&ProjectionTargetId::Claude).unwrap();
        assert_eq!(
            claude_evidence.paths.len(),
            2,
            "marketplace manifest + runtime-state JSON"
        );
        assert!(claude_evidence
            .paths
            .iter()
            .all(|p| p.change == projection::ProjectionPathChange::Written));
        assert!(claude_evidence.failure.is_none());

        let copilot_evidence = evidence
            .by_target
            .get(&ProjectionTargetId::Copilot)
            .unwrap();
        assert_eq!(copilot_evidence.paths.len(), 1, "runtime-state JSON");
        assert_eq!(
            copilot_evidence.paths[0].change,
            projection::ProjectionPathChange::Written
        );
        assert!(copilot_evidence.failure.is_none());

        // Confirms the real work actually happened, not just evidence bookkeeping.
        assert!(claude_marketplace_manifest_path().unwrap().is_file());
        assert!(target_state_path("claude").unwrap().is_file());
        assert!(target_state_path("copilot").unwrap().is_file());
    }

    #[test]
    fn apply_claude_copilot_evidence_claude_failure_short_circuits_before_copilot_is_attempted() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let cfg_path = ccync_foundation::paths::machine_config_path().unwrap();
        select_runtimes_config(&cfg_path, r#"["claude","copilot"]"#, "claude");

        // Force Claude's marketplace-manifest write to fail: block its parent
        // directory with a *file* at that exact path, same technique the
        // existing codex-apply-failure test uses.
        let marketplace_path = claude_marketplace_manifest_path().unwrap();
        let blocked_parent = marketplace_path.parent().unwrap();
        std::fs::create_dir_all(blocked_parent.parent().unwrap()).unwrap();
        std::fs::write(blocked_parent, b"not a directory").unwrap();

        let canonical_root = tmp.path().join("fake-canonical-root");
        std::fs::create_dir_all(&canonical_root).unwrap();

        let evidence = apply_claude_copilot_evidence(&canonical_root).unwrap();

        assert!(
            !evidence
                .completed_targets
                .contains(&ProjectionTargetId::Claude),
            "Claude target must stay NotRun (absent from completed_targets) when its own step fails"
        );
        assert!(
            !evidence
                .completed_targets
                .contains(&ProjectionTargetId::Copilot),
            "Copilot target must stay NotRun — Claude failure short-circuits before Copilot is attempted"
        );
        assert!(
            !evidence
                .by_target
                .contains_key(&ProjectionTargetId::Copilot),
            "Copilot must never be attempted when Claude fails first"
        );

        let claude_evidence = evidence.by_target.get(&ProjectionTargetId::Claude).unwrap();
        assert!(claude_evidence.paths.is_empty());
        assert!(claude_evidence.skipped.is_none());
        assert!(claude_evidence.failure.is_some());

        assert!(
            !target_state_path("copilot").unwrap().is_file(),
            "copilot runtime-state JSON must never be written when claude fails first"
        );
    }

    #[test]
    fn apply_claude_copilot_evidence_copilot_failure_leaves_claude_completed() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let cfg_path = ccync_foundation::paths::machine_config_path().unwrap();
        select_runtimes_config(&cfg_path, r#"["claude","copilot"]"#, "claude");

        // Force Copilot's runtime-state JSON write to fail: block its parent
        // directory with a *file* at that exact path.
        let copilot_state_path = target_state_path("copilot").unwrap();
        let blocked_parent = copilot_state_path.parent().unwrap();
        std::fs::create_dir_all(blocked_parent.parent().unwrap()).unwrap();
        std::fs::write(blocked_parent, b"not a directory").unwrap();

        let canonical_root = tmp.path().join("fake-canonical-root");
        std::fs::create_dir_all(&canonical_root).unwrap();

        let evidence = apply_claude_copilot_evidence(&canonical_root).unwrap();

        // Claude's own step genuinely completed before Copilot's later
        // failure — it stays recorded as Written + completed, matching the
        // "no rollback here" contract.
        assert!(evidence
            .completed_targets
            .contains(&ProjectionTargetId::Claude));
        assert!(!evidence
            .completed_targets
            .contains(&ProjectionTargetId::Copilot));

        let claude_evidence = evidence.by_target.get(&ProjectionTargetId::Claude).unwrap();
        assert_eq!(claude_evidence.paths.len(), 2);
        assert!(claude_evidence.failure.is_none());

        let copilot_evidence = evidence
            .by_target
            .get(&ProjectionTargetId::Copilot)
            .unwrap();
        assert!(copilot_evidence.paths.is_empty());
        assert!(copilot_evidence.failure.is_some());

        assert!(claude_marketplace_manifest_path().unwrap().is_file());
        assert!(target_state_path("claude").unwrap().is_file());
    }

    /// Demonstrates the probe's "later global work runs" property by
    /// construction: because `apply_claude_copilot_evidence` returns `Ok`
    /// even when a target's own step failed (never propagating a target
    /// failure as an outer `Err`), a hypothetical caller's subsequent code is
    /// provably reachable — it is never short-circuited by an `Err`. A tiny
    /// inline marker stands in for that "later global work".
    #[test]
    fn apply_claude_copilot_evidence_ok_return_lets_later_global_work_run_even_after_a_target_fails(
    ) {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let cfg_path = ccync_foundation::paths::machine_config_path().unwrap();
        select_runtimes_config(&cfg_path, r#"["claude","copilot"]"#, "claude");

        let marketplace_path = claude_marketplace_manifest_path().unwrap();
        let blocked_parent = marketplace_path.parent().unwrap();
        std::fs::create_dir_all(blocked_parent.parent().unwrap()).unwrap();
        std::fs::write(blocked_parent, b"not a directory").unwrap();

        let canonical_root = tmp.path().join("fake-canonical-root");
        std::fs::create_dir_all(&canonical_root).unwrap();

        let result = apply_claude_copilot_evidence(&canonical_root);

        // The function returned `Ok` despite Claude's owned failure ...
        let evidence = result.expect(
            "an owned target failure must never surface as the outer Err — \
             a later caller must still be able to proceed",
        );
        assert!(evidence
            .by_target
            .get(&ProjectionTargetId::Claude)
            .and_then(|e| e.failure.as_ref())
            .is_some());

        // ... so this "later global work" marker is provably reachable: it
        // only runs because the match above did not early-return on `Err`.
        fn later_global_work_marker() -> bool {
            true
        }
        let later_global_work_ran = later_global_work_marker();
        assert!(
            later_global_work_ran,
            "later global work must run after a value-level target failure, not be skipped"
        );
    }

    // ── apply_agy_evidence() ────────────────────────────────────────────────
    //
    // Evidence-reporting sibling of the AGY block inside `run_update_with_evidence`.
    // Mirrors the fixture-setup style of `apply_claude_copilot_evidence_*`
    // above (fake home via `USERPROFILE`/`HOME`) and the fault-injection
    // technique of `AgyProjection::apply`'s own tests (block a
    // junction's parent directory with a plain file).

    #[test]
    fn apply_agy_evidence_neither_selected_is_default_evidence() {
        let _env = env_guard();

        #[cfg(windows)]
        let var_name = "USERPROFILE";
        #[cfg(not(windows))]
        let var_name = "HOME";

        // Deliberately leave home unresolvable: if `apply_agy_evidence`
        // attempted `AgyProjection::new` despite neither face being
        // selected, this would surface as an `Err`, not silently pass.
        let prior = std::env::var_os(var_name);
        unsafe {
            std::env::remove_var(var_name);
        }

        let canonical_root = std::path::PathBuf::from("unused-canonical-root");
        let result = apply_agy_evidence(&canonical_root, false, false, false);

        unsafe {
            match &prior {
                Some(v) => std::env::set_var(var_name, v),
                None => std::env::remove_var(var_name),
            }
        }

        assert_eq!(
            result.unwrap(),
            projection::ProjectionEvidence::default(),
            "neither face selected must return default evidence without even constructing AgyProjection"
        );
    }

    #[test]
    fn apply_agy_evidence_both_succeed_records_both_targets_written_and_completed() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let canonical_root = tmp.path().join("fake-canonical-root");
        std::fs::create_dir_all(canonical_root.join("skills")).unwrap();

        let evidence = apply_agy_evidence(&canonical_root, true, true, false).unwrap();

        assert!(evidence
            .completed_targets
            .contains(&ProjectionTargetId::AgyCli));
        assert!(evidence
            .completed_targets
            .contains(&ProjectionTargetId::AgyIde));

        // Shape depends on platform symlink privilege: `Linked` where
        // permitted, `Written` where a directory copy landed instead. Either
        // is a genuine publish, never a preserve/skip (fresh temp home, no
        // pre-existing content at either target).
        let cli_evidence = evidence.by_target.get(&ProjectionTargetId::AgyCli).unwrap();
        assert_eq!(
            cli_evidence.paths.len(),
            1,
            "no rollback occurred, so no compensating Removed event must be appended"
        );
        assert!(matches!(
            cli_evidence.paths[0].change,
            projection::ProjectionPathChange::Linked | projection::ProjectionPathChange::Written
        ));
        assert!(cli_evidence.failure.is_none());

        let ide_evidence = evidence.by_target.get(&ProjectionTargetId::AgyIde).unwrap();
        assert_eq!(ide_evidence.paths.len(), 1);
        assert!(matches!(
            ide_evidence.paths[0].change,
            projection::ProjectionPathChange::Linked | projection::ProjectionPathChange::Written
        ));
        assert!(ide_evidence.failure.is_none());
    }

    #[test]
    fn apply_agy_evidence_cli_failure_short_circuits_before_ide_is_attempted() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let canonical_root = tmp.path().join("fake-canonical-root");
        std::fs::create_dir_all(canonical_root.join("skills")).unwrap();

        // Force CLI junction creation to fail: place a regular file where the
        // CLI plugins directory's parent must be a directory.
        std::fs::create_dir_all(tmp.path().join(".gemini")).unwrap();
        std::fs::write(
            tmp.path().join(".gemini").join("antigravity-cli"),
            b"not a directory",
        )
        .unwrap();

        let evidence = apply_agy_evidence(&canonical_root, true, true, false).unwrap();

        assert!(
            !evidence
                .completed_targets
                .contains(&ProjectionTargetId::AgyCli),
            "AgyCli target must stay NotRun (absent from completed_targets) when its own step fails"
        );
        assert!(
            !evidence
                .completed_targets
                .contains(&ProjectionTargetId::AgyIde),
            "AgyIde target must stay NotRun — CLI failure short-circuits before IDE is attempted"
        );
        assert!(
            !evidence.by_target.contains_key(&ProjectionTargetId::AgyIde),
            "IDE must never be attempted when CLI fails first"
        );

        let cli_evidence = evidence.by_target.get(&ProjectionTargetId::AgyCli).unwrap();
        assert!(cli_evidence.paths.is_empty());
        assert!(cli_evidence.skipped.is_none());
        assert!(cli_evidence.failure.is_some());
    }

    #[test]
    fn apply_agy_evidence_ide_failure_after_cli_success_applies_rollback_compensating_removed_event(
    ) {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let canonical_root = tmp.path().join("fake-canonical-root");
        std::fs::create_dir_all(canonical_root.join("skills")).unwrap();

        // Force IDE junction creation to fail (same technique as
        // `AgyProjection::apply`'s own rollback test).
        std::fs::create_dir_all(tmp.path().join(".gemini")).unwrap();
        std::fs::write(
            tmp.path().join(".gemini").join("antigravity-ide"),
            b"not a directory",
        )
        .unwrap();

        let evidence = apply_agy_evidence(&canonical_root, true, true, false).unwrap();

        // The junction itself was physically rolled back, exactly like
        // `AgyProjection::apply`'s own rollback behavior.
        let cli_target = tmp
            .path()
            .join(".gemini")
            .join("antigravity-cli")
            .join("plugins")
            .join("ccync");
        assert!(
            !cli_target.exists(),
            "CLI junction must be rolled back after IDE junction failure"
        );

        // AgyCli's own step genuinely completed, so it stays in
        // completed_targets — but its path evidence now carries a
        // compensating Removed event after the original publish (Linked or
        // Written, per platform symlink privilege), honestly reflecting the
        // later reversal.
        assert!(evidence
            .completed_targets
            .contains(&ProjectionTargetId::AgyCli));
        assert!(!evidence
            .completed_targets
            .contains(&ProjectionTargetId::AgyIde));

        let cli_evidence = evidence.by_target.get(&ProjectionTargetId::AgyCli).unwrap();
        assert_eq!(
            cli_evidence.paths.len(),
            2,
            "must carry both the original publish event and the compensating Removed event"
        );
        assert!(matches!(
            cli_evidence.paths[0].change,
            projection::ProjectionPathChange::Linked | projection::ProjectionPathChange::Written
        ));
        assert_eq!(
            cli_evidence.paths[1].change,
            projection::ProjectionPathChange::Removed
        );
        assert_eq!(
            cli_evidence.paths[0].path, cli_evidence.paths[1].path,
            "the compensating Removed event must target the same path as the original publish event"
        );
        assert!(
            cli_evidence.failure.is_none(),
            "AgyCli's own step succeeded — only AgyIde carries a failure"
        );

        let ide_evidence = evidence.by_target.get(&ProjectionTargetId::AgyIde).unwrap();
        assert!(ide_evidence.paths.is_empty());
        assert!(ide_evidence.failure.is_some());
    }

    #[test]
    fn apply_agy_evidence_constructor_failure_returns_boxed_agy_error_with_empty_partial() {
        let _env = env_guard();

        #[cfg(windows)]
        let var_name = "USERPROFILE";
        #[cfg(not(windows))]
        let var_name = "HOME";

        let prior = std::env::var_os(var_name);
        unsafe {
            std::env::remove_var(var_name);
        }

        let canonical_root = std::path::PathBuf::from("unused-canonical-root");
        let result = apply_agy_evidence(&canonical_root, true, true, false);

        unsafe {
            match &prior {
                Some(v) => std::env::set_var(var_name, v),
                None => std::env::remove_var(var_name),
            }
        }

        let failure =
            result.expect_err("AgyProjection::new must fail when home cannot be resolved");
        match failure.error {
            ProjectionPhaseError::Agy(_) => {}
            other => panic!("expected ProjectionPhaseError::Agy, got {other:?}"),
        }
        assert_eq!(
            failure.partial,
            projection::ProjectionEvidence::default(),
            "nothing was attempted yet, so partial evidence must be empty"
        );
    }

    #[test]
    fn apply_agy_evidence_ide_only_failure_does_not_spuriously_trigger_cli_rollback_compensation() {
        // Adversarial check: with want_cli=false, want_ide=true, and IDE
        // alone failing, AgyCli was never attempted at all — it must be
        // genuinely absent from `completed_targets` (not merely "usually"
        // absent), so the rollback-detection condition
        // (`completed_targets.contains(AgyCli) && AgyIde has a failure`)
        // must stay false and no spurious compensating `Removed` event (or
        // any AgyCli entry at all) gets appended.
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let canonical_root = tmp.path().join("fake-canonical-root");
        std::fs::create_dir_all(canonical_root.join("skills")).unwrap();

        // Force IDE junction creation to fail; CLI is never selected so it
        // must never be attempted regardless.
        std::fs::create_dir_all(tmp.path().join(".gemini")).unwrap();
        std::fs::write(
            tmp.path().join(".gemini").join("antigravity-ide"),
            b"not a directory",
        )
        .unwrap();

        let evidence = apply_agy_evidence(&canonical_root, false, true, false).unwrap();

        assert!(
            !evidence
                .completed_targets
                .contains(&ProjectionTargetId::AgyCli),
            "AgyCli must never appear in completed_targets when it was never selected"
        );
        assert!(
            !evidence.by_target.contains_key(&ProjectionTargetId::AgyCli),
            "AgyCli must have no evidence entry at all — no spurious compensating Removed \
             event may be fabricated for a target that was never attempted"
        );

        let ide_evidence = evidence.by_target.get(&ProjectionTargetId::AgyIde).unwrap();
        assert!(ide_evidence.paths.is_empty());
        assert!(ide_evidence.failure.is_some());
    }

    // ── run_update_phase() ───────────────────────────────────────────────────
    //
    // Composition of `apply_claude_copilot_evidence` + `apply_agy_evidence`
    // into one `ProjectionEvidence` for the Canonical phase. Mirrors the
    // fixture-setup style of both sub-calls' own tests above.

    #[test]
    fn run_update_phase_both_succeed_composes_all_four_targets() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let cfg_path = ccync_foundation::paths::machine_config_path().unwrap();
        select_runtimes_config(&cfg_path, r#"["claude","copilot"]"#, "claude");

        let canonical_root = tmp.path().join("fake-canonical-root");
        std::fs::create_dir_all(canonical_root.join("skills")).unwrap();

        let evidence = run_update_phase(&canonical_root, true, true, false).unwrap();

        for target in [
            ProjectionTargetId::Claude,
            ProjectionTargetId::Copilot,
            ProjectionTargetId::AgyCli,
            ProjectionTargetId::AgyIde,
        ] {
            assert!(
                evidence.completed_targets.contains(&target),
                "{target:?} must be in completed_targets after a full success"
            );
        }
        assert_eq!(
            evidence.completed_targets.len(),
            4,
            "exactly the union of both sub-calls' targets, nothing extra"
        );
    }

    #[test]
    fn run_update_phase_claude_failure_merges_correctly_with_agy_success() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let cfg_path = ccync_foundation::paths::machine_config_path().unwrap();
        select_runtimes_config(&cfg_path, r#"["claude","copilot"]"#, "claude");

        // Force Claude's marketplace-manifest write to fail (same technique as
        // `apply_claude_copilot_evidence_claude_failure_short_circuits_...`).
        let marketplace_path = claude_marketplace_manifest_path().unwrap();
        let blocked_parent = marketplace_path.parent().unwrap();
        std::fs::create_dir_all(blocked_parent.parent().unwrap()).unwrap();
        std::fs::write(blocked_parent, b"not a directory").unwrap();

        let canonical_root = tmp.path().join("fake-canonical-root");
        std::fs::create_dir_all(canonical_root.join("skills")).unwrap();

        let evidence = run_update_phase(&canonical_root, true, true, false).unwrap();

        // Claude's value-level failure and Copilot's never-attempted status
        // survive the merge untouched, alongside AGY's independent success.
        assert!(!evidence
            .completed_targets
            .contains(&ProjectionTargetId::Claude));
        assert!(!evidence
            .completed_targets
            .contains(&ProjectionTargetId::Copilot));
        assert!(evidence
            .completed_targets
            .contains(&ProjectionTargetId::AgyCli));
        assert!(evidence
            .completed_targets
            .contains(&ProjectionTargetId::AgyIde));
        let claude_failure = evidence
            .by_target
            .get(&ProjectionTargetId::Claude)
            .and_then(|e| e.failure.as_ref());
        assert!(claude_failure.is_some());
    }

    #[test]
    fn run_update_phase_neither_agy_face_selected_claude_copilot_succeed() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let cfg_path = ccync_foundation::paths::machine_config_path().unwrap();
        select_runtimes_config(&cfg_path, r#"["claude","copilot"]"#, "claude");

        let canonical_root = tmp.path().join("fake-canonical-root");
        std::fs::create_dir_all(canonical_root.join("skills")).unwrap();

        let evidence = run_update_phase(&canonical_root, false, false, false).unwrap();

        assert!(evidence
            .completed_targets
            .contains(&ProjectionTargetId::Claude));
        assert!(evidence
            .completed_targets
            .contains(&ProjectionTargetId::Copilot));
        assert!(
            !evidence
                .completed_targets
                .contains(&ProjectionTargetId::AgyCli),
            "AGY was never selected, so AgyCli must never appear in completed_targets"
        );
        assert!(!evidence
            .completed_targets
            .contains(&ProjectionTargetId::AgyIde));
        assert_eq!(
            evidence.completed_targets.len(),
            2,
            "exact partial target set when AGY is unselected"
        );
    }

    /// Gap-closing coverage: `agy_fully_completed`'s two-clause AND was only
    /// ever exercised at `(want_cli, want_ide) = (true, true)` and `(false,
    /// false)` by the tests above. This is the `(true, false)` corner: CLI
    /// alone is selected and succeeds, IDE is never selected (never attempted,
    /// so `AgyIde` never enters `completed_targets`).
    #[test]
    fn run_update_phase_agy_cli_only_selected_and_succeeds() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let cfg_path = ccync_foundation::paths::machine_config_path().unwrap();
        select_runtimes_config(&cfg_path, r#"[]"#, "claude");

        let canonical_root = tmp.path().join("fake-canonical-root");
        std::fs::create_dir_all(canonical_root.join("skills")).unwrap();

        let evidence = run_update_phase(&canonical_root, true, false, false).unwrap();

        assert!(evidence
            .completed_targets
            .contains(&ProjectionTargetId::AgyCli));
        assert!(
            !evidence
                .completed_targets
                .contains(&ProjectionTargetId::AgyIde),
            "IDE was never selected, so AgyIde must never appear in completed_targets"
        );
    }

    /// The `(false, true)` mirror of the test above: IDE alone is selected
    /// and succeeds, CLI is never selected. `agy_fully_completed`'s CLI
    /// clause (`!want_agy_cli || ..`) must short-circuit to `true` on
    /// `want_agy_cli == false` without ever checking `completed_targets` for
    /// `AgyCli` (which never ran).
    #[test]
    fn run_update_phase_agy_ide_only_selected_and_succeeds() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let cfg_path = ccync_foundation::paths::machine_config_path().unwrap();
        select_runtimes_config(&cfg_path, r#"[]"#, "claude");

        let canonical_root = tmp.path().join("fake-canonical-root");
        std::fs::create_dir_all(canonical_root.join("skills")).unwrap();

        let evidence = run_update_phase(&canonical_root, false, true, false).unwrap();

        assert!(
            !evidence
                .completed_targets
                .contains(&ProjectionTargetId::AgyCli),
            "CLI was never selected, so AgyCli must never appear in completed_targets"
        );
        assert!(evidence
            .completed_targets
            .contains(&ProjectionTargetId::AgyIde));
    }

    /// The probe's core claim: a genuine sub-call `Err` occurring after the
    /// other sub-call already ran must propagate a `partial` that equals
    /// exactly what had already accumulated — not empty, not just the
    /// failing sub-call's own (empty, per
    /// `apply_agy_evidence_constructor_failure_returns_boxed_agy_error_with_empty_partial`)
    /// partial alone.
    ///
    /// `AgyProjection::new`'s only failure mode is unresolvable home — the
    /// same call `ClaudeSkillProjection::new`/`claude_marketplace_manifest_path`
    /// use — so within one `run_update_phase` invocation there is no
    /// environment that makes AGY's constructor fail while leaving
    /// Claude/Copilot's own path resolution untouched: both sub-calls read
    /// the identical process-wide home resolution. With home unresolvable
    /// throughout, the first sub-call (`apply_claude_copilot_evidence`, which
    /// runs first per `run_update_phase`'s calling order) still returns `Ok`
    /// — its own `Claude` step fails as an owned-path value (matching
    /// `apply_claude_copilot_evidence_claude_failure_short_circuits_...`'s
    /// contract), which is non-trivial, non-default evidence in its own
    /// right — while the second sub-call, `apply_agy_evidence`, genuinely
    /// fails via `AgyProjection::new`. This is the realistic version of
    /// "the other sub-call already ran and produced real evidence before the
    /// global fault" reachable under a single fixed environment; it proves
    /// the same "snapshot, not swallowed, not just the failing side's own
    /// partial" property the probe asks for.
    #[test]
    fn run_update_phase_global_fault_after_prior_sub_call_propagates_exact_partial_snapshot() {
        let _env = env_guard();

        #[cfg(windows)]
        let var_name = "USERPROFILE";
        #[cfg(not(windows))]
        let var_name = "HOME";

        let prior = std::env::var_os(var_name);
        unsafe {
            std::env::remove_var(var_name);
        }

        let canonical_root = std::path::PathBuf::from("unused-canonical-root");

        // Independently reconstruct exactly what the first sub-call produces
        // under this same unresolvable-home environment, using the exact
        // same sub-call `run_update_phase` itself calls first. Deterministic
        // and side-effect-free: `apply_claude_lifecycle` fails before
        // writing anything once `ClaudeSkillProjection::new`/
        // `claude_marketplace_manifest_path` cannot resolve home.
        let expected_first_call_evidence = apply_claude_copilot_evidence(&canonical_root).unwrap();
        assert!(
            !expected_first_call_evidence.by_target.is_empty(),
            "the first sub-call's own evidence must be non-trivial for this test to prove \
             the partial is a real snapshot, not just an empty default"
        );

        let result = run_update_phase(&canonical_root, true, true, false);

        unsafe {
            match &prior {
                Some(v) => std::env::set_var(var_name, v),
                None => std::env::remove_var(var_name),
            }
        }

        let failure = result.expect_err(
            "AgyProjection::new failing after the first sub-call already ran must propagate Err",
        );
        match failure.error {
            ProjectionPhaseError::Agy(_) => {}
            other => panic!("expected ProjectionPhaseError::Agy, got {other:?}"),
        }
        assert_eq!(
            failure.partial, expected_first_call_evidence,
            "partial must equal exactly the already-run first sub-call's evidence — \
             not empty, not just the (empty) AGY sub-call's own partial alone"
        );
    }

    // ── run_update_with_evidence delegates to the evidence-producing
    // machinery ────────────────────────────────────────────────────────────
    //
    // `run_update_with_evidence` no longer builds Claude/Copilot/Codex/AGY-state
    // artifacts or the AGY plugin-junction inline — it delegates to
    // `apply_agy_evidence` and `apply_lifecycle_artifact_chain` and composes
    // the returned `ProjectionEvidence` from their evidence directly.

    #[test]
    fn run_update_with_evidence_all_targets_succeed_composes_every_covered_target() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let selected = ["claude", "copilot", "codex", "agy-cli", "agy-ide"];
        write_selected_runtimes_config(&selected, "claude");

        let outcome = run_update_with_evidence_using(
            &CcyncConfig::default(),
            &crate::delivery_apply::UnavailableNativeRunner,
        )
        .expect("run_update_with_evidence should succeed against a fake home");
        assert!(outcome.fault.is_none());
        let evidence = outcome.evidence;

        assert!(
            evidence.run_warnings.is_empty(),
            "an all-succeed run must carry no warnings: {:?}",
            evidence.run_warnings
        );

        // The composed evidence covers exactly the targets this function's
        // own body genuinely applies: Claude, Copilot, Codex (via
        // `apply_lifecycle_artifact_chain`) and AgyCli/AgyIde (via the
        // separate `apply_agy_evidence` step) — not `AgyGui`, which is a
        // different, decomposed projection this function never touches.
        for target in [
            ProjectionTargetId::Claude,
            ProjectionTargetId::Copilot,
            ProjectionTargetId::Codex,
            ProjectionTargetId::AgyCli,
            ProjectionTargetId::AgyIde,
        ] {
            assert!(
                evidence.completed_targets.contains(&target),
                "{target:?} must be in completed_targets"
            );
            // Copilot and the AGY faces complete through direct publication and
            // retire their combined plugin roots, so they carry no combined path.
            if matches!(
                target,
                ProjectionTargetId::Copilot
                    | ProjectionTargetId::AgyCli
                    | ProjectionTargetId::AgyIde
            ) {
                continue;
            }
            assert!(
                evidence
                    .by_target
                    .get(&target)
                    .is_some_and(|te| !te.paths.is_empty()),
                "{target:?} must carry real path evidence"
            );
        }
        assert!(
            !evidence
                .completed_targets
                .contains(&ProjectionTargetId::AgyGui),
            "AgyGui is not covered by this composition"
        );
    }

    #[test]
    fn run_update_with_evidence_render_failure_returns_typed_fault_and_no_render_warning() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let canonical_root = ccync_foundation::paths::canonical_plugin_root().unwrap();
        std::fs::create_dir_all(&canonical_root).unwrap();

        let target_dir = tmp.path().join("target_dir");
        std::fs::create_dir_all(&target_dir).unwrap();
        let link_path = canonical_root.join("symlink_entry");
        if ccync_foundation::platform::create_dir_link(&target_dir, &link_path).is_err() {
            return;
        }

        let outcome = run_update_with_evidence_using(
            &CcyncConfig::default(),
            &crate::delivery_apply::UnavailableNativeRunner,
        )
        .expect("run_update_with_evidence must return Ok(outcome) on render failure");

        assert!(
            !outcome
                .evidence
                .run_warnings
                .iter()
                .any(|w| w.contains("canonical root content render")),
            "render failure must not be pushed to run_warnings: {:?}",
            outcome.evidence.run_warnings
        );

        let fault = outcome.fault.expect("fault must be Some when render fails");
        match fault.error {
            ProjectionPhaseError::Install(InstallError::Io(err)) => {
                let msg = err.to_string();
                assert!(
                    msg.contains("canonical root contains a symlink or junction"),
                    "unexpected error message: {msg}"
                );
            }
            other => {
                panic!("expected ProjectionPhaseError::Install(InstallError::Io), got {other:?}")
            }
        }
        assert_eq!(
            fault.partial,
            projection::ProjectionEvidence::default(),
            "partial evidence before Step 2.5 must match initial default evidence"
        );
    }

    #[test]
    fn run_update_with_evidence_claude_failure_short_circuits_copilot_but_records_failure_evidence()
    {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let selected = ["claude", "copilot", "codex"];
        write_selected_runtimes_config(&selected, "claude");

        // Force Claude's marketplace-manifest write to fail (same technique
        // `run_update_claude_failure_short_circuits_copilot_and_codex_with_single_catch_all_warning`
        // below uses).
        let marketplace_path = claude_marketplace_manifest_path().unwrap();
        let blocked_parent = marketplace_path.parent().unwrap();
        std::fs::create_dir_all(blocked_parent.parent().unwrap()).unwrap();
        std::fs::write(blocked_parent, b"not a directory").unwrap();

        let evidence = run_update_with_evidence_using(
            &CcyncConfig::default(),
            &crate::delivery_apply::UnavailableNativeRunner,
        )
        .expect("run_update_with_evidence must still return Ok — lifecycle faults are best-effort")
        .evidence;

        assert!(evidence.completed_targets.is_empty());
        // The chain catch-all, then the coexistence notice that retirement adds
        // while the failed chain leaves the combined copies in place.
        assert_eq!(evidence.run_warnings.len(), 2);

        // The exact canonical partial payload: Claude's own failure is
        // recorded as a value on its target (not silently dropped), and
        // Copilot/Codex never appear at all — the short-circuit means their
        // branches were never attempted, not merely "attempted and empty".
        let claude_evidence = evidence
            .by_target
            .get(&ProjectionTargetId::Claude)
            .expect("Claude's failure must be recorded in the composed evidence");
        assert!(claude_evidence.failure.is_some());
        assert!(!evidence
            .completed_targets
            .contains(&ProjectionTargetId::Claude));
        assert!(!evidence
            .by_target
            .contains_key(&ProjectionTargetId::Copilot));
        assert!(!evidence.by_target.contains_key(&ProjectionTargetId::Codex));
    }

    #[test]
    fn run_update_claude_failure_short_circuits_copilot_and_codex_with_single_catch_all_warning() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let selected = ["claude", "copilot", "codex"];
        write_selected_runtimes_config(&selected, "claude");

        // Force Claude's marketplace-manifest write to fail: block its parent
        // directory with a *file* at that exact path (same technique the
        // apply_claude_copilot_evidence failure tests use).
        let marketplace_path = claude_marketplace_manifest_path().unwrap();
        let blocked_parent = marketplace_path.parent().unwrap();
        std::fs::create_dir_all(blocked_parent.parent().unwrap()).unwrap();
        std::fs::write(blocked_parent, b"not a directory").unwrap();

        let evidence = run_update_with_evidence_using(
            &CcyncConfig::default(),
            &crate::delivery_apply::UnavailableNativeRunner,
        )
        .expect("run_update_with_evidence must still return Ok — lifecycle faults are best-effort")
        .evidence;

        assert_eq!(
            evidence.run_warnings.len(),
            2,
            "one catch-all warning, matching the retired legacy chain's \
             single `?`-propagated `InstallError` caught once by the outer \
             `if let Err(e) = ... {{ warnings.push(...) }}`, plus the \
             coexistence notice: {:?}",
            evidence.run_warnings
        );
        assert!(
            evidence.run_warnings[0].starts_with("runtime lifecycle state: "),
            "must reuse the exact legacy catch-all prefix, not the per-target \
             \"Claude projection (best-effort): ...\" format: {:?}",
            evidence.run_warnings[0]
        );
        assert!(
            evidence.run_warnings[1].starts_with("old and new copies may coexist"),
            "the second warning must be the coexistence notice: {:?}",
            evidence.run_warnings[1]
        );
        assert!(
            evidence.completed_targets.is_empty(),
            "Claude failing first must short-circuit Copilot and Codex too, \
             matching the retired legacy chain's single `?`-chain across all \
             three: {:?}",
            evidence.completed_targets
        );
        assert!(
            !target_state_path("copilot").unwrap().is_file(),
            "copilot runtime-state JSON must never be written when claude fails first"
        );
        assert!(
            !target_state_path("codex").unwrap().is_file(),
            "codex runtime-state JSON must never be written when claude fails first"
        );
    }

    #[test]
    fn run_update_agy_cli_retired_root_never_reaches_junction_step() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let selected = ["claude", "agy-cli"];
        write_selected_runtimes_config(&selected, "claude");

        // A regular file where the CLI plugins directory's parent must be a
        // directory. Direct publication succeeds in this fixture, so the
        // combined CLI junction step never runs and this file is left alone.
        std::fs::create_dir_all(tmp.path().join(".gemini")).unwrap();
        std::fs::write(
            tmp.path().join(".gemini").join("antigravity-cli"),
            b"not a directory",
        )
        .unwrap();

        let evidence = run_update_with_evidence_using(
            &CcyncConfig::default(),
            &crate::delivery_apply::UnavailableNativeRunner,
        )
        .expect("run_update_with_evidence must still return Ok")
        .evidence;

        assert!(
            evidence.run_warnings.is_empty(),
            "a retired AGY CLI root must add no AGY-specific warning: {:?}",
            evidence.run_warnings
        );
        assert!(
            tmp.path().join(".gemini").join("antigravity-cli").is_file(),
            "the blocking file must stay untouched: no combined CLI root is written"
        );
        assert!(
            evidence
                .completed_targets
                .contains(&ProjectionTargetId::Claude),
            "Claude's own success is independent of AGY's failure: {:?}",
            evidence.completed_targets
        );
        // Surprising but genuine legacy behavior, preserved byte-for-byte:
        // the AGY runtime-state write inside the lifecycle-artifact chain is
        // gated only by `any_agy_face_selected` (true whenever any AGY face
        // is selected) — it is unconditional on the plugin-junction step
        // above, and writes an unrelated file. That write has no blocker in
        // this fixture, so it succeeds even though the combined junction step
        // never ran. The retired legacy chain had the exact same behavior (its
        // own AGY branch is equally unconditional on Step 4's outcome) — this
        // test proves the refactor reproduces it, not that it is desirable.
        assert!(
            target_state_path("agy").unwrap().is_file(),
            "the independent AGY runtime-state write must still succeed \
             even though the combined plugin-junction step did not run, \
             matching the retired legacy chain's own unconditional gate"
        );
    }

    #[test]
    fn run_update_agy_gui_only_selection_writes_runtime_state_without_junction_apply() {
        // agy-gui alone selects no CLI/IDE junction face at all — the
        // plugin-junction step (Step 4, `want_agy_cli || want_agy_ide`) must
        // be skipped entirely, while the separate AGY runtime-state write
        // inside the lifecycle-artifact chain still fires: it is gated only
        // on `any_agy_face_selected` seeing any of agy-cli/agy-ide/agy-gui,
        // independent of which specific face triggered it. Manually verified
        // against the pre-refactor source to hold identically in both
        // versions; this test pins it.
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let selected = ["claude", "agy-gui"];
        write_selected_runtimes_config(&selected, "claude");

        let evidence = run_update_with_evidence_using(
            &CcyncConfig::default(),
            &crate::delivery_apply::UnavailableNativeRunner,
        )
        .expect("run_update_with_evidence should succeed against a fake home")
        .evidence;

        assert!(
            evidence.run_warnings.is_empty(),
            "no junction was ever attempted (agy-gui selects neither CLI nor \
             IDE), and the runtime-state write has no blocker in this \
             fixture, so this run must carry no warnings: {:?}",
            evidence.run_warnings
        );
        assert!(
            evidence
                .completed_targets
                .contains(&ProjectionTargetId::Claude),
            "claude's own success is independent of AGY selection: {:?}",
            evidence.completed_targets
        );
        assert!(
            target_state_path("agy").unwrap().is_file(),
            "the AGY runtime-state write must still succeed for an \
             agy-gui-only selection even though the plugin-junction step \
             never ran (want_agy_cli and want_agy_ide are both false)"
        );
        let cli_target = tmp
            .path()
            .join(".gemini")
            .join("antigravity-cli")
            .join("plugins")
            .join("ccync");
        let ide_target = tmp
            .path()
            .join(".gemini")
            .join("antigravity-ide")
            .join("plugins")
            .join("ccync");
        assert!(
            !cli_target.exists() && !ide_target.exists(),
            "the plugin-junction step must never run for an agy-gui-only \
             selection — no CLI/IDE junction should be created"
        );
    }

    #[test]
    fn run_update_retired_agy_cli_and_lifecycle_chain_failure_keeps_legacy_warning_order() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let selected = ["claude", "copilot", "codex", "agy-cli"];
        write_selected_runtimes_config(&selected, "claude");

        // Force Claude's marketplace-manifest write to fail (the lifecycle-
        // artifact chain). The AGY CLI combined junction is retired by direct
        // publication, so it no longer runs and adds no warning.
        let marketplace_path = claude_marketplace_manifest_path().unwrap();
        let blocked_parent = marketplace_path.parent().unwrap();
        std::fs::create_dir_all(blocked_parent.parent().unwrap()).unwrap();
        std::fs::write(blocked_parent, b"not a directory").unwrap();

        let evidence = run_update_with_evidence_using(
            &CcyncConfig::default(),
            &crate::delivery_apply::UnavailableNativeRunner,
        )
        .expect("run_update_with_evidence must still return Ok — the fault is best-effort")
        .evidence;

        assert_eq!(
            evidence.run_warnings.len(),
            2,
            "the lifecycle-chain catch-all and the coexistence notice, no more: {:?}",
            evidence.run_warnings
        );
        assert!(
            evidence.run_warnings[0].starts_with("runtime lifecycle state: "),
            "the lifecycle-chain catch-all must come first: {:?}",
            evidence.run_warnings
        );
        assert!(
            evidence.run_warnings[1].starts_with("old and new copies may coexist"),
            "the coexistence notice must come second: {:?}",
            evidence.run_warnings
        );
        // Claude's failure short-circuits the chain, so neither Claude, Copilot
        // nor Codex completes. AGY CLI completes through its direct publication.
        for target in [
            ProjectionTargetId::Claude,
            ProjectionTargetId::Copilot,
            ProjectionTargetId::Codex,
        ] {
            assert!(
                !evidence.completed_targets.contains(&target),
                "{target:?} must not complete when the lifecycle chain fails at Claude: {:?}",
                evidence.completed_targets
            );
        }
    }

    /// Copilot's direct publication fails (a regular file sits at its agents
    /// root), so the fallback combined writer runs. Its blocked runtime-state
    /// write fails the chain, which must then short-circuit Codex and the AGY
    /// runtime state, matching the retired legacy chain's single `?`-chain.
    #[test]
    fn run_update_copilot_direct_failure_runs_fallback_and_short_circuits_chain() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let selected = ["claude", "copilot", "codex", "agy-cli"];
        write_selected_runtimes_config(&selected, "claude");

        // One bundled plugin with a skill and an agent, so direct publication
        // has Copilot agent output to write.
        let plugin = tmp.path().join(".ccync").join("plugins").join("example");
        std::fs::create_dir_all(plugin.join(".claude-plugin")).unwrap();
        std::fs::write(
            plugin.join(".claude-plugin").join("plugin.json"),
            r#"{"name":"example","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(plugin.join("skills").join("example")).unwrap();
        std::fs::write(
            plugin.join("skills").join("example").join("SKILL.md"),
            "---\nname: example\ndescription: Example skill.\n---\nBody.\n",
        )
        .unwrap();
        std::fs::create_dir_all(plugin.join("agents")).unwrap();
        std::fs::write(
            plugin.join("agents").join("helper.md"),
            "---\nname: helper\ndescription: Helper agent.\n---\nBody.\n",
        )
        .unwrap();
        let build = tmp.path().join(".ccync").join("build");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(
            build.join("lock.json"),
            r#"{"resolvedPlugins":[{"pluginId":"example","resolvedSource":"example","installStrategy":"bundled-local","sha":"0123456789abcdef0123456789abcdef01234567"}]}"#,
        )
        .unwrap();

        // A regular file at the Copilot agents root makes the direct agent
        // write fail, so Copilot is not retired and its combined writer runs.
        let agents_root = tmp.path().join(".copilot").join("agents");
        std::fs::create_dir_all(agents_root.parent().unwrap()).unwrap();
        std::fs::write(&agents_root, b"foreign\n").unwrap();

        // Block the fallback writer's runtime-state path with a file, using the
        // same technique as the chain-failure tests above.
        let copilot_state_path = target_state_path("copilot").unwrap();
        let blocked_parent = copilot_state_path.parent().unwrap();
        std::fs::create_dir_all(blocked_parent.parent().unwrap()).unwrap();
        std::fs::write(blocked_parent, b"not a directory").unwrap();

        let evidence = run_update_with_evidence_using(
            &CcyncConfig::default(),
            &crate::delivery_apply::UnavailableNativeRunner,
        )
        .expect("run_update_with_evidence must still return Ok — lifecycle faults are best-effort")
        .evidence;

        assert!(
            evidence
                .completed_targets
                .contains(&ProjectionTargetId::Claude),
            "Claude completed before Copilot's failure: {:?}",
            evidence.completed_targets
        );
        assert!(
            !evidence
                .completed_targets
                .contains(&ProjectionTargetId::Copilot),
            "Copilot's direct publication and fallback writer both failed: {:?}",
            evidence.completed_targets
        );
        assert!(
            !evidence
                .completed_targets
                .contains(&ProjectionTargetId::Codex),
            "Copilot failing must short-circuit Codex: {:?}",
            evidence.completed_targets
        );
        assert!(
            !target_state_path("codex").unwrap().is_file(),
            "codex runtime-state JSON must never be written when copilot fails first"
        );
        assert!(
            !target_state_path("agy").unwrap().is_file(),
            "AGY runtime-state JSON must never be written when copilot fails first"
        );
        assert!(
            evidence
                .run_warnings
                .iter()
                .any(|w| w.starts_with("runtime lifecycle state: ")),
            "the chain failure must surface under the legacy catch-all prefix: {:?}",
            evidence.run_warnings
        );
        assert!(
            blocked_parent.is_file(),
            "the blocked Copilot state path must stay a file, so the fallback writer cannot replace it"
        );
    }

    /// Independent verification (tester-added, not implementer-authored),
    /// rewritten for the retired layout. Copilot's combined writer is gated off
    /// once direct publication retires its plugin root, so a blocked Copilot
    /// state path is never written and cannot fail the chain. The chain only
    /// reaches a Copilot failure when direct publication failed and the
    /// fallback writer runs, which this fixture does not force.
    #[test]
    fn run_update_retired_copilot_state_block_never_reaches_lifecycle_chain() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let selected = ["claude", "copilot", "codex", "agy-cli"];
        write_selected_runtimes_config(&selected, "claude");

        // Force Copilot's runtime-state JSON write to fail: block its parent
        // directory with a *file* at that exact path (same technique
        // `apply_claude_copilot_evidence_copilot_failure_leaves_claude_completed` uses).
        let copilot_state_path = target_state_path("copilot").unwrap();
        let blocked_parent = copilot_state_path.parent().unwrap();
        std::fs::create_dir_all(blocked_parent.parent().unwrap()).unwrap();
        std::fs::write(blocked_parent, b"not a directory").unwrap();

        let evidence = run_update_with_evidence_using(
            &CcyncConfig::default(),
            &crate::delivery_apply::UnavailableNativeRunner,
        )
        .expect("run_update_with_evidence must still return Ok — lifecycle faults are best-effort")
        .evidence;

        assert!(
            evidence
                .completed_targets
                .contains(&ProjectionTargetId::Claude),
            "Claude completed: {:?}",
            evidence.completed_targets
        );
        // Copilot completes through its retired plugin root, not through the
        // combined writer, so the blocked state path does not affect it.
        assert!(
            evidence
                .completed_targets
                .contains(&ProjectionTargetId::Copilot),
            "a retired Copilot root completes through direct publication: {:?}",
            evidence.completed_targets
        );
        assert!(
            evidence
                .completed_targets
                .contains(&ProjectionTargetId::Codex),
            "Codex is not short-circuited when no Copilot writer runs: {:?}",
            evidence.completed_targets
        );
        assert!(
            blocked_parent.is_file(),
            "the blocked Copilot state path must stay untouched because its writer is retired"
        );
        assert!(
            evidence.run_warnings.is_empty(),
            "a retired Copilot writer adds no warning: {:?}",
            evidence.run_warnings
        );
    }

    /// Independent verification (tester-added, not implementer-authored):
    /// closes short-circuit point 3 — a Codex failure inside
    /// `apply_lifecycle_artifact_chain` must abort the chain's own AGY
    /// runtime-state write (the fourth and final step), matching the
    /// retired legacy chain's single `?`-propagation. AGY's separate
    /// plugin-junction step (Step 4, outside this chain) is deliberately
    /// left unselected here so the only source of an "agy" push in this
    /// fixture is the chain's own runtime-state write.
    #[test]
    fn run_update_codex_failure_short_circuits_agy_runtime_state_write() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        // "agy-cli"/"agy-ide" are deliberately NOT selected, so Step 4 (the
        // separate AGY plugin-junction) never runs and cannot itself push
        // "agy" — isolating the chain's own runtime-state write as the only
        // possible source of an "agy" push in this fixture.
        let selected = ["claude", "copilot", "codex"];
        write_selected_runtimes_config(&selected, "claude");

        // Force Codex's marketplace-manifest write to fail: block its parent
        // directory with a *file* at that exact path (same technique
        // `runtime_report_outcomes_excludes_selected_runtime_whose_apply_failed` uses).
        let codex_manifest_path = codex_marketplace_manifest_path().unwrap();
        let blocked_parent = codex_manifest_path.parent().unwrap();
        std::fs::create_dir_all(blocked_parent.parent().unwrap()).unwrap();
        std::fs::write(blocked_parent, b"not a directory").unwrap();

        let evidence = run_update_with_evidence_using(
            &CcyncConfig::default(),
            &crate::delivery_apply::UnavailableNativeRunner,
        )
        .expect("run_update_with_evidence must still return Ok — lifecycle faults are best-effort")
        .evidence;

        assert!(
            evidence
                .completed_targets
                .contains(&ProjectionTargetId::Claude),
            "Claude completed before Codex's later failure: {:?}",
            evidence.completed_targets
        );
        assert!(
            evidence
                .completed_targets
                .contains(&ProjectionTargetId::Copilot),
            "Copilot completed before Codex's later failure: {:?}",
            evidence.completed_targets
        );
        assert!(
            !evidence
                .completed_targets
                .contains(&ProjectionTargetId::Codex),
            "Codex itself failed: {:?}",
            evidence.completed_targets
        );
        assert!(
            !target_state_path("agy").unwrap().is_file(),
            "Codex failing must short-circuit the chain's own AGY \
             runtime-state write (the fourth step), matching the retired \
             legacy chain's single `?`-chain — agy runtime-state JSON must \
             never be written when codex fails first"
        );
        assert_eq!(
            evidence.run_warnings.len(),
            2,
            "one catch-all warning from the chain plus the coexistence notice; \
             no separate AGY warning since Step 4 was never selected: {:?}",
            evidence.run_warnings
        );
        assert!(
            evidence.run_warnings[0].starts_with("runtime lifecycle state: "),
            "must reuse the legacy catch-all prefix: {:?}",
            evidence.run_warnings[0]
        );
        assert!(
            evidence.run_warnings[1].starts_with("old and new copies may coexist"),
            "the second warning must be the coexistence notice: {:?}",
            evidence.run_warnings[1]
        );
    }

    // ── claude_selection_gate: run_update_with_evidence's sole Claude apply site ──
    //
    // Regression guard for the removed early unconditional
    // `ClaudeSkillProjection` apply call in `run_update_with_evidence`: the gated
    // path inside `apply_lifecycle_artifact_chain` (via `apply_claude_lifecycle`)
    // must be the only site that creates the Claude skill link, and it must
    // fire exactly once when Claude is selected.

    #[test]
    fn claude_selection_gate_unselected_creates_no_skill_link() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let cfg_path = ccync_foundation::paths::machine_config_path().unwrap();
        std::fs::create_dir_all(cfg_path.parent().unwrap()).unwrap();
        std::fs::write(
            &cfg_path,
            r#"{"selectedRuntimes":["codex"],"primaryRuntime":"codex"}"#,
        )
        .unwrap();

        let evidence = run_update_with_evidence_using(
            &CcyncConfig::default(),
            &crate::delivery_apply::UnavailableNativeRunner,
        )
        .expect("run_update_with_evidence should succeed against a fake home")
        .evidence;

        let skill_surface = tmp.path().join(".claude").join("skills").join("ccync");
        assert!(
            !skill_surface.exists(),
            "Claude skill link must not be created when Claude is not selected: {skill_surface:?}"
        );
        assert!(
            !evidence
                .completed_targets
                .contains(&ProjectionTargetId::Claude),
            "unselected Claude must never be reported: {:?}",
            evidence.completed_targets
        );
    }

    #[test]
    fn claude_selection_gate_selected_applies_exactly_once() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let cfg_path = ccync_foundation::paths::machine_config_path().unwrap();
        std::fs::create_dir_all(cfg_path.parent().unwrap()).unwrap();
        std::fs::write(
            &cfg_path,
            r#"{"selectedRuntimes":["claude"],"primaryRuntime":"claude"}"#,
        )
        .unwrap();

        let evidence = run_update_with_evidence_using(
            &CcyncConfig::default(),
            &crate::delivery_apply::UnavailableNativeRunner,
        )
        .expect("run_update_with_evidence should succeed against a fake home")
        .evidence;

        // Direct publication succeeds on a fresh home, so the combined Claude
        // link is retired in the same run instead of being created.
        let skill_surface = tmp.path().join(".claude").join("skills").join("ccync");
        assert!(
            !skill_surface.exists(),
            "the combined Claude skill link must be retired once direct publication succeeds"
        );
        // Guards against the previous double-apply bug where both the early
        // unconditional call and the gated path each attempted to apply and
        // report the Claude surface. That early unconditional call site no
        // longer exists at all (single call site today, inside
        // `apply_lifecycle_artifact_chain`), so `completed_targets` — a set,
        // inherently de-duplicated — reporting Claude at all is the
        // remaining observable proof the gated path ran.
        assert!(
            evidence
                .completed_targets
                .contains(&ProjectionTargetId::Claude),
            "Claude must be reported as completed: {:?}",
            evidence.completed_targets
        );
    }

    /// Regression: `apply_claude_lifecycle` serializes
    /// `projectionRoot`/`installTarget`/`stagedPluginRoot` via
    /// `ClaudeSkillProjection::skill_path()` (`&Path`) instead of reading the
    /// `skill_surface` (`PathBuf`) field directly. Nothing exercised the
    /// *written JSON* against the live filesystem path before this test —
    /// `planned_engine_touches_full_selection_matches_observed_run` only
    /// covers the disclosure list, not `managed.json`'s lifecycle payload.
    /// Guards against a future accessor/type change silently diverging the
    /// disclosed lifecycle metadata from the real link target.
    #[test]
    fn claude_managed_json_lifecycle_paths_match_skill_surface() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let cfg_path = ccync_foundation::paths::machine_config_path().unwrap();
        std::fs::create_dir_all(cfg_path.parent().unwrap()).unwrap();
        std::fs::write(
            &cfg_path,
            r#"{"selectedRuntimes":["claude"],"primaryRuntime":"claude"}"#,
        )
        .unwrap();

        // A user-registered `ccync@ccync` blocks retirement, so the combined
        // Claude link stays and its lifecycle paths can be checked against it.
        let registration = tmp.path().join(".claude/plugins/installed_plugins.json");
        std::fs::create_dir_all(registration.parent().unwrap()).unwrap();
        std::fs::write(
            &registration,
            r#"{"version":2,"plugins":{"ccync@ccync":[{"version":"1.0.0"}]}}"#,
        )
        .unwrap();

        run_update_with_evidence_using(
            &CcyncConfig::default(),
            &crate::delivery_apply::UnavailableNativeRunner,
        )
        .expect("run_update_with_evidence should succeed against a fake home");

        let skill_surface = tmp.path().join(".claude").join("skills").join("ccync");
        assert!(
            skill_surface.exists(),
            "Claude skill link must stay while the user registration blocks retirement"
        );
        let expected = skill_surface.to_string_lossy().to_string();

        let managed_json_path = target_state_path("claude").unwrap();
        let managed_json: Value = serde_json::from_str(
            &std::fs::read_to_string(&managed_json_path)
                .unwrap_or_else(|e| panic!("failed to read {managed_json_path:?}: {e}")),
        )
        .expect("managed.json must be valid JSON");

        for pointer in [
            "/projectionRoot",
            "/installTarget",
            "/lifecycle/stagedPluginRoot",
        ] {
            let actual = managed_json
                .pointer(pointer)
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("{pointer} missing or not a string: {managed_json}"));
            assert_eq!(
                actual, expected,
                "{pointer} must equal the real skill surface link path"
            );
        }
    }

    // ── planned_engine_touches: projection::planned_machine_touches merged
    // with engine-owned Claude/AGY/Copilot lifecycle paths ────────────────────

    fn set_fake_home(home: &std::path::Path) {
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", home);
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", home);
        }
    }

    fn write_selected_runtimes_config(selected: &[&str], primary: &str) {
        let cfg_path = ccync_foundation::paths::machine_config_path().unwrap();
        std::fs::create_dir_all(cfg_path.parent().unwrap()).unwrap();
        let body = json!({
            "selectedRuntimes": selected,
            "primaryRuntime": primary,
        });
        std::fs::write(&cfg_path, serde_json::to_string(&body).unwrap()).unwrap();
    }

    fn opts_for_runtimes(selected: &[&str]) -> projection::MachineUpdateOptions {
        let config = CcyncConfig {
            selected_runtimes: selected.iter().map(|s| s.to_string()).collect(),
            ..CcyncConfig::default()
        };
        let state_root = ccync_foundation::paths::EffectiveLayout::load_or_legacy()
            .expect("effective test layout")
            .state_base();
        projection::machine_options_from_config(&config, false, state_root)
            .expect("machine_options_from_config should resolve against the fake home")
    }

    fn assert_sorted_deduped(touches: &[(String, PathBuf)]) {
        let mut sorted = touches.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            touches, sorted,
            "planned_engine_touches must return a sorted, de-duplicated list"
        );
    }

    /// Broad selection exercising every Claude/AGY/Copilot branch together —
    /// the AGY selection also drives `projection::planned_machine_touches`'
    /// own `.gemini/ccync` source link, proving the two halves actually
    /// merge rather than one shadowing the other. Every live path asserted
    /// here is read back from an isolated, real `run_update_with_evidence`
    /// — not a hand-typed expected list.
    #[test]
    fn planned_engine_touches_full_selection_matches_observed_run() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let selected = ["claude", "copilot", "agy-cli", "agy-ide"];
        write_selected_runtimes_config(&selected, "claude");

        let evidence = run_update_with_evidence_using(
            &CcyncConfig::default(),
            &crate::delivery_apply::UnavailableNativeRunner,
        )
        .expect("run_update_with_evidence should succeed against a fake home")
        .evidence;
        assert!(
            evidence.run_warnings.is_empty(),
            "isolated full-selection run should have no warnings: {:?}",
            evidence.run_warnings
        );

        let opts = opts_for_runtimes(&selected);
        let planned = planned_engine_touches(&opts).expect("planned_engine_touches should succeed");
        assert_sorted_deduped(&planned);

        // Engine-owned surfaces (this task).
        let claude_surface = tmp.path().join(".claude").join("skills").join("ccync");
        let copilot_surface = tmp
            .path()
            .join(".copilot")
            .join("installed-plugins")
            .join("ccync-copilot")
            .join("ccync");
        let agy_cli_target = tmp
            .path()
            .join(".gemini")
            .join("antigravity-cli")
            .join("plugins")
            .join("ccync");
        let agy_ide_target = tmp
            .path()
            .join(".gemini")
            .join("antigravity-ide")
            .join("plugins")
            .join("ccync");
        // Claude's combined link, and the Copilot and AGY combined plugin roots,
        // are retired by the ordinary sync once direct publication succeeds. None
        // of them exists afterwards, yet each is still disclosed as a may-touch path.
        assert!(
            !claude_surface.exists(),
            "the combined Claude link must be retired after direct publication: {claude_surface:?}"
        );
        for observed in [&copilot_surface, &agy_cli_target, &agy_ide_target] {
            assert!(
                !observed.exists(),
                "combined plugin root must be retired after direct publication: {observed:?}"
            );
            assert!(
                planned.iter().any(|(_, path)| path == observed),
                "planned_engine_touches missing observed live path {observed:?}\nplanned={planned:?}"
            );
        }

        // Projection-owned surface (from `projection::planned_machine_touches`),
        // included to prove the merge is real rather than one half shadowing the
        // other. `run_update_with_evidence` does not currently wire
        // `update_skills`/`update_personalization` into its own pipeline (that
        // projection entry point is not yet called from ccync-engine), so this
        // path is not asserted to exist on disk — only that
        // `planned_engine_touches` actually surfaces the projection half's
        // output for the selected AGY runtimes, not just the engine-owned
        // half.
        let agy_source_link = tmp.path().join(".gemini").join("ccync");
        assert!(
            planned.iter().any(|(_, path)| path == &agy_source_link),
            "planned_engine_touches must include projection::planned_machine_touches' output: {planned:?}"
        );
    }

    /// Narrow selection that selects none of Claude/AGY/Copilot — proves the
    /// engine-owned homes are excluded (not just quietly present because a
    /// broader fixture happens to include them too).
    #[test]
    fn planned_engine_touches_excludes_unselected_engine_homes() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let selected = ["codex"];
        write_selected_runtimes_config(&selected, "codex");

        run_update_with_evidence_using(
            &CcyncConfig::default(),
            &crate::delivery_apply::UnavailableNativeRunner,
        )
        .expect("run_update_with_evidence should succeed against a fake home");

        let claude_surface = tmp.path().join(".claude").join("skills").join("ccync");
        let copilot_surface = tmp
            .path()
            .join(".copilot")
            .join("installed-plugins")
            .join("ccync-copilot")
            .join("ccync");
        let agy_cli_target = tmp
            .path()
            .join(".gemini")
            .join("antigravity-cli")
            .join("plugins")
            .join("ccync");
        let agy_ide_target = tmp
            .path()
            .join(".gemini")
            .join("antigravity-ide")
            .join("plugins")
            .join("ccync");
        for unselected in [
            &claude_surface,
            &copilot_surface,
            &agy_cli_target,
            &agy_ide_target,
        ] {
            assert!(
                !unselected.exists(),
                "unselected engine home must not be created by run_update_with_evidence: {unselected:?}"
            );
        }

        let opts = opts_for_runtimes(&selected);
        let planned = planned_engine_touches(&opts).expect("planned_engine_touches should succeed");
        assert_sorted_deduped(&planned);
        assert!(
            !planned.iter().any(|(runtime, _)| runtime == "claude"
                || runtime == "copilot"
                || runtime == "agy"),
            "codex-only selection must exclude every Claude/AGY/Copilot pair: {planned:?}"
        );
    }

    /// Claude selected alone (no Copilot, no AGY) — proves the three gates
    /// are independent rather than a single bucket that fires together.
    #[test]
    fn planned_engine_touches_claude_only_excludes_copilot_and_agy() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let selected = ["claude"];
        write_selected_runtimes_config(&selected, "claude");

        run_update_with_evidence_using(
            &CcyncConfig::default(),
            &crate::delivery_apply::UnavailableNativeRunner,
        )
        .expect("run_update_with_evidence should succeed against a fake home");

        let opts = opts_for_runtimes(&selected);
        let planned = planned_engine_touches(&opts).expect("planned_engine_touches should succeed");
        assert_sorted_deduped(&planned);

        // The combined Claude link is retired by the ordinary sync, while the
        // disclosure still names it: a blocked retirement keeps that link.
        let claude_surface = tmp.path().join(".claude").join("skills").join("ccync");
        assert!(!claude_surface.exists());
        assert!(planned
            .iter()
            .any(|(runtime, path)| runtime == "claude" && path == &claude_surface));
        assert!(
            !planned
                .iter()
                .any(|(runtime, _)| runtime == "copilot" || runtime == "agy"),
            "claude-only selection must not report copilot/agy pairs: {planned:?}"
        );
    }

    /// AGY selected with only the CLI face — proves `agy-cli` / `agy-ide` are
    /// gated individually: a fixture that always selects both AGY faces
    /// together can hide a missing per-face gate.
    #[test]
    fn planned_engine_touches_agy_cli_only_excludes_ide_target() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let selected = ["agy-cli"];
        write_selected_runtimes_config(&selected, "agy-cli");

        run_update_with_evidence_using(
            &CcyncConfig::default(),
            &crate::delivery_apply::UnavailableNativeRunner,
        )
        .expect("run_update_with_evidence should succeed against a fake home");

        let agy_cli_target = tmp
            .path()
            .join(".gemini")
            .join("antigravity-cli")
            .join("plugins")
            .join("ccync");
        let agy_ide_target = tmp
            .path()
            .join(".gemini")
            .join("antigravity-ide")
            .join("plugins")
            .join("ccync");
        assert!(
            !agy_cli_target.exists(),
            "agy-cli combined plugin root must be retired after direct publication"
        );
        assert!(
            !agy_ide_target.exists(),
            "agy-ide junction must not be created when agy-ide is not selected"
        );

        let opts = opts_for_runtimes(&selected);
        let planned = planned_engine_touches(&opts).expect("planned_engine_touches should succeed");
        assert_sorted_deduped(&planned);

        assert!(planned
            .iter()
            .any(|(runtime, path)| runtime == "agy" && path == &agy_cli_target));
        assert!(
            !planned.iter().any(|(_, path)| path == &agy_ide_target),
            "agy-cli-only selection must not report the agy-ide target: {planned:?}"
        );
    }

    /// `planned_engine_touches` reads `opts.selected_projection_targets()`
    /// rather than a value captured before the switch: mutating an existing
    /// `MachineUpdateOptions` in place via `set_selected_projection_targets`
    /// must be visible to the very next `planned_engine_touches` call on that
    /// same value. This is the sharpest regression check for the accessor
    /// call-site rewrite — a stale-copy bug
    /// (e.g. capturing the legacy backing field before the mutation) would
    /// still pass the other fixtures above but fail here.
    #[test]
    fn planned_engine_touches_reflects_setter_mutation_on_same_opts() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        write_selected_runtimes_config(&["claude"], "claude");
        run_update_with_evidence_using(
            &CcyncConfig::default(),
            &crate::delivery_apply::UnavailableNativeRunner,
        )
        .expect("run_update_with_evidence should succeed against a fake home");

        let mut opts = opts_for_runtimes(&["claude"]);
        let claude_surface = tmp.path().join(".claude").join("skills").join("ccync");
        let before = planned_engine_touches(&opts).expect("planned_engine_touches should succeed");
        assert!(
            before
                .iter()
                .any(|(runtime, path)| runtime == "claude" && path == &claude_surface),
            "claude selection must report the claude surface before mutation: {before:?}"
        );

        opts.set_selected_projection_targets(vec!["agy-cli".to_string()]);
        assert_eq!(opts.selected_projection_targets(), ["agy-cli".to_string()]);

        let agy_cli_target = tmp
            .path()
            .join(".gemini")
            .join("antigravity-cli")
            .join("plugins")
            .join("ccync");

        let after = planned_engine_touches(&opts).expect("planned_engine_touches should succeed");
        assert!(
            !after.iter().any(|(runtime, _)| runtime == "claude"),
            "claude surface must disappear once selection no longer includes claude: {after:?}"
        );
        assert!(
            after
                .iter()
                .any(|(runtime, path)| runtime == "agy" && path == &agy_cli_target),
            "agy-cli must appear once the setter selects it: {after:?}"
        );
    }

    #[test]
    fn run_update_sweeps_stale_ledger_json() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let ledger_path = tmp.path().join(".ccync").join("ledger.json");
        std::fs::create_dir_all(ledger_path.parent().unwrap()).unwrap();
        std::fs::write(&ledger_path, "{}").unwrap();

        write_selected_runtimes_config(&["claude"], "claude");
        run_update_with_evidence_using(
            &CcyncConfig::default(),
            &crate::delivery_apply::UnavailableNativeRunner,
        )
        .expect("run_update_with_evidence should succeed against a fake home");

        assert!(
            !ledger_path.exists(),
            "stale ledger.json must be swept during run_update"
        );
    }

    /// Parity proof for the probe requirement: "Disclosed targets equal
    /// applied targets for every selection fixture." `planned_engine_touches`
    /// (disclosure, takes `MachineUpdateOptions`) and the real apply path
    /// reached through `run_update_with_evidence`, which reads its
    /// own `RuntimeSelection` from on-disk config, keyed by `canonical_root`
    /// rather than `MachineUpdateOptions`) are bridged here by writing the
    /// same selection to disk for both call shapes, then computing the
    /// expected Claude/Copilot/AGY-CLI/AGY-IDE target set exactly once via
    /// `primary_targets` and checking it three ways: (1) what
    /// `planned_engine_touches` discloses, (2) what `run_update_with_evidence` actually
    /// materializes on disk, (3) the typed selection itself. All three must
    /// agree for every fixture, including the neither-AGY-face and
    /// empty-selection edge cases.
    #[test]
    fn planned_engine_touches_disclosure_matches_real_apply_for_selection_fixtures() {
        let fixtures: &[(&[&str], &str)] = &[
            (&["claude", "copilot", "agy-cli", "agy-ide"], "claude"),
            (&["claude"], "claude"),
            (&["agy-cli"], "agy-cli"),
            (&["agy-ide"], "agy-ide"),
            (&["claude", "copilot"], "claude"),
            (&[], "claude"),
            // Gap fixtures added during independent test review: copilot
            // selected alone (its branch had previously only ever been
            // exercised alongside claude in this disk-materialization test),
            // and both AGY faces together without claude/copilot present
            // (previously only co-exercised inside the full four-target
            // fixture above).
            (&["copilot"], "copilot"),
            (&["agy-cli", "agy-ide"], "agy-cli"),
        ];

        for (selected, primary_runtime) in fixtures {
            let _env = env_guard();
            let tmp = tempfile::TempDir::new().unwrap();
            set_fake_home(tmp.path());

            write_selected_runtimes_config(selected, primary_runtime);
            run_update_with_evidence_using(
                &CcyncConfig::default(),
                &crate::delivery_apply::UnavailableNativeRunner,
            )
            .expect("run_update_with_evidence should succeed against a fake home");

            // Expected set, computed exactly once from the typed selection —
            // the same function both the disclosure and (indirectly, via
            // `apply_lifecycle_artifact_chain`/`apply_agy_evidence`) the real
            // apply path resolve gating through.
            let selection = load_runtime_selection();
            let primary = primary_targets(&selection);
            // Claude's combined link is retired once its direct publication
            // succeeds, and so are the Copilot and AGY combined plugin roots.
            // No combined surface materializes after a successful sync.
            let expected: Vec<ProjectionTargetId> = Vec::new();

            let claude_surface = tmp.path().join(".claude").join("skills").join("ccync");
            let copilot_surface = tmp
                .path()
                .join(".copilot")
                .join("installed-plugins")
                .join("ccync-copilot")
                .join("ccync");
            let agy_cli_target = tmp
                .path()
                .join(".gemini")
                .join("antigravity-cli")
                .join("plugins")
                .join("ccync");
            let agy_ide_target = tmp
                .path()
                .join(".gemini")
                .join("antigravity-ide")
                .join("plugins")
                .join("ccync");

            // (2) What the real apply path (`run_update_with_evidence`) actually
            // materialized on disk.
            let mut applied: Vec<ProjectionTargetId> = Vec::new();
            if claude_surface.exists() {
                applied.push(ProjectionTargetId::Claude);
            }
            if copilot_surface.exists() {
                applied.push(ProjectionTargetId::Copilot);
            }
            if agy_cli_target.exists() {
                applied.push(ProjectionTargetId::AgyCli);
            }
            if agy_ide_target.exists() {
                applied.push(ProjectionTargetId::AgyIde);
            }
            applied.sort();

            assert_eq!(
                expected, applied,
                "typed selection {selected:?} disagrees with what run_update_with_evidence materialized on disk"
            );

            // (1) What `planned_engine_touches` discloses for the same
            // selection, expressed through `MachineUpdateOptions` rather
            // than the on-disk `RuntimeSelection` the apply path reads.
            let opts = opts_for_runtimes(selected);
            let planned =
                planned_engine_touches(&opts).expect("planned_engine_touches should succeed");
            assert_sorted_deduped(&planned);

            let mut disclosed: Vec<ProjectionTargetId> = Vec::new();
            if planned
                .iter()
                .any(|(runtime, path)| runtime == "claude" && path == &claude_surface)
            {
                disclosed.push(ProjectionTargetId::Claude);
            }
            if planned
                .iter()
                .any(|(runtime, path)| runtime == "copilot" && path == &copilot_surface)
            {
                disclosed.push(ProjectionTargetId::Copilot);
            }
            if planned
                .iter()
                .any(|(runtime, path)| runtime == "agy" && path == &agy_cli_target)
            {
                disclosed.push(ProjectionTargetId::AgyCli);
            }
            if planned
                .iter()
                .any(|(runtime, path)| runtime == "agy" && path == &agy_ide_target)
            {
                disclosed.push(ProjectionTargetId::AgyIde);
            }
            disclosed.sort();

            // Claude is the one exception to parity. A selected Claude target
            // discloses its combined link as a may-touch path, because a blocked
            // retirement keeps that link. A successful sync removes it, so Claude
            // is checked against selection and left out of the applied comparison.
            let claude_disclosed = disclosed.contains(&ProjectionTargetId::Claude);
            assert_eq!(
                primary.contains(&ProjectionTargetId::Claude),
                claude_disclosed,
                "claude disclosure must follow claude selection for {selected:?}"
            );
            disclosed.retain(|id| *id != ProjectionTargetId::Claude);

            // Copilot and the AGY faces follow the same exception. A selected
            // plugin root is disclosed as a may-touch path, and a successful
            // sync retires it, so it is checked against selection and left out
            // of the applied comparison.
            let mut selected_plugin_roots: Vec<ProjectionTargetId> = [
                ProjectionTargetId::Copilot,
                ProjectionTargetId::AgyCli,
                ProjectionTargetId::AgyIde,
            ]
            .into_iter()
            .filter(|id| primary.contains(id))
            .collect();
            selected_plugin_roots.sort();
            assert_eq!(
                selected_plugin_roots, disclosed,
                "planned_engine_touches disclosure disagrees with the typed selection for {selected:?}"
            );
            disclosed.clear();

            assert_eq!(
                expected, disclosed,
                "planned_engine_touches disclosure disagrees with the typed selection for {selected:?}"
            );
            assert_eq!(
                applied, disclosed,
                "disclosed targets must equal applied targets for selection {selected:?}"
            );
        }
    }

    // ── load_enabled_personal_plugins() tests ───────────────────────────────────

    fn with_fake_personal_lock<F>(home_path: &std::path::Path, lock_contents: &str, f: F)
    where
        F: FnOnce(Vec<(String, String)>),
    {
        let _env = env_guard();
        let build_dir = home_path.join(".ccync").join("build");
        std::fs::create_dir_all(&build_dir).unwrap();
        std::fs::write(build_dir.join("lock.json"), lock_contents).unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", home_path);
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", home_path);
        }
        let result = load_enabled_personal_plugins();
        f(result);
    }

    #[test]
    fn load_enabled_personal_plugins_absent_namespace_returns_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        // lockfile has only resolvedPlugins — no _personalPlugins key
        let lock = r#"{ "resolvedPlugins": [], "_ccyncProjection": {} }"#;
        with_fake_personal_lock(tmp.path(), lock, |result| {
            assert!(
                result.is_empty(),
                "absent _personalPlugins must return empty vec"
            );
        });
    }

    #[test]
    fn load_enabled_personal_plugins_entries_parsed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let lock = r#"{
            "resolvedPlugins": [],
            "_personalPlugins": [
                {
                    "pluginId": "my-tool",
                    "sourceType": "curated-upstream",
                    "source": "https://github.com/user/my-tool.git"
                },
                {
                    "pluginId": "local-helper",
                    "sourceType": "bundled-local",
                    "source": "~/tools/local-helper"
                }
            ]
        }"#;
        with_fake_personal_lock(tmp.path(), lock, |result| {
            assert_eq!(result.len(), 2);
            assert_eq!(result[0].0, "my-tool");
            assert_eq!(result[0].1, "https://github.com/user/my-tool.git");
            assert_eq!(result[1].0, "local-helper");
            assert_eq!(result[1].1, "~/tools/local-helper");
        });
    }

    #[test]
    fn load_enabled_personal_plugins_missing_source_entry_skipped() {
        let tmp = tempfile::TempDir::new().unwrap();
        let lock = r#"{
            "_personalPlugins": [
                { "pluginId": "no-source-entry", "sourceType": "curated-upstream" },
                { "pluginId": "valid", "source": "https://github.com/user/valid.git" }
            ]
        }"#;
        with_fake_personal_lock(tmp.path(), lock, |result| {
            assert_eq!(result.len(), 1, "entry missing source must be skipped");
            assert_eq!(result[0].0, "valid");
        });
    }

    #[test]
    fn load_enabled_personal_plugins_does_not_read_resolved_plugins() {
        let tmp = tempfile::TempDir::new().unwrap();
        // resolvedPlugins has a bundled-local entry; it must NOT appear in personal result
        let lock = r#"{
            "resolvedPlugins": [
                { "pluginId": "ccync-game-assets", "installStrategy": "bundled-local", "resolvedSource": "ccync-domain/ccync-game-assets" }
            ],
            "_personalPlugins": [
                { "pluginId": "my-tool", "source": "https://github.com/user/my-tool.git" }
            ]
        }"#;
        with_fake_personal_lock(tmp.path(), lock, |result| {
            assert_eq!(
                result.len(),
                1,
                "must only return personal entries, not resolvedPlugins"
            );
            assert_eq!(result[0].0, "my-tool");
        });
    }

    // ── pick_live_dirs() render live-dir selection ─────────────────────────────

    fn cdir(name: &str) -> PathBuf {
        PathBuf::from(format!("/cache/{name}"))
    }

    #[test]
    fn pick_live_dirs_with_pin_selects_exact_dir() {
        let candidates = vec![cdir("a@old"), cdir("a@new")];
        let (chosen, ambiguous) = pick_live_dirs(&candidates, Some("new"), "a");
        assert_eq!(chosen, vec![cdir("a@new")], "only the pinned dir renders");
        assert!(!ambiguous);
    }

    #[test]
    fn pick_live_dirs_no_pin_single_is_unambiguous() {
        let candidates = vec![cdir("a@only")];
        let (chosen, ambiguous) = pick_live_dirs(&candidates, None, "a");
        assert_eq!(chosen, vec![cdir("a@only")]);
        assert!(!ambiguous);
    }

    #[test]
    fn pick_live_dirs_no_pin_multiple_is_ambiguous_fail_closed() {
        let candidates = vec![cdir("a@one"), cdir("a@two")];
        let (chosen, ambiguous) = pick_live_dirs(&candidates, None, "a");
        assert!(
            chosen.is_empty(),
            "ambiguous → render nothing (fail-closed)"
        );
        assert!(ambiguous);
    }

    // ── fetch_personal_plugin() unit tests ─────────────────────────────────────

    #[test]
    fn fetch_personal_plugin_returns_already_present_when_versioned_dir_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        // Pre-create a <id>@sha directory to simulate already-cloned
        let existing = cache.join("my-tool@abc1234");
        std::fs::create_dir_all(&existing).unwrap();
        let result =
            fetch_personal_plugin(&cache, "my-tool", "https://github.com/user/my-tool.git");
        match result {
            PersonalFetchResult::AlreadyPresent(p) => {
                assert_eq!(p, existing, "should return the existing directory path");
            }
            other => panic!("expected AlreadyPresent, got: {other:?}"),
        }
    }

    #[test]
    fn fetch_personal_plugin_prefix_check_does_not_match_other_plugins() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        // A dir for a different plugin should NOT trigger AlreadyPresent
        let other = cache.join("other-tool@abc1234");
        std::fs::create_dir_all(&other).unwrap();
        // Asking for "my-tool" — should NOT find "other-tool@..." as already present.
        // It will try git clone which will fail (non-existent URL), returning CloneFailed.
        let result = fetch_personal_plugin(&cache, "my-tool", "https://localhost/nonexistent.git");
        assert!(
            !matches!(result, PersonalFetchResult::AlreadyPresent(_)),
            "other-plugin dir must not trigger AlreadyPresent for a different plugin_id"
        );
    }

    #[test]
    fn fetch_all_personal_plugins_empty_when_lock_absent() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        let result = fetch_all_personal_plugins();
        assert!(
            result.is_empty(),
            "absent lockfile must produce empty fetch list"
        );
    }

    // ── managed_plugin_dirs() unit tests ───────────────────────────────────────

    #[test]
    fn managed_plugin_dirs_returns_bundle_and_personal_id_pairs() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let lock = r#"{
            "resolvedPlugins": [
                {
                    "pluginId": "my-bundle-plugin",
                    "installStrategy": "bundled-local",
                    "resolvedSource": "domain/my-bundle-plugin"
                }
            ],
            "_personalPlugins": [
                {
                    "pluginId": "my-personal-plugin",
                    "source": "https://example.com/repo.git",
                    "sha": "abc1234"
                }
            ]
        }"#;
        let build_dir = tmp.path().join(".ccync").join("build");
        std::fs::create_dir_all(&build_dir).unwrap();
        std::fs::write(build_dir.join("lock.json"), lock).unwrap();

        let plugins_root = tmp.path().join(".ccync").join("plugins");
        let bundle_dir = plugins_root.join("domain").join("my-bundle-plugin");
        std::fs::create_dir_all(&bundle_dir).unwrap();

        let cache_root = tmp.path().join(".ccync").join("cache");
        let personal_dir = cache_root.join("my-personal-plugin@abc1234");
        std::fs::create_dir_all(&personal_dir).unwrap();

        let dirs = managed_plugin_dirs();
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0], ("my-bundle-plugin".to_string(), bundle_dir));
        assert_eq!(dirs[1], ("my-personal-plugin".to_string(), personal_dir));
    }

    // ── render_canonical_root (canonical-root content render) ──────────────────

    #[test]
    fn stage_canonical_render_absent_root_creates_empty_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = tmp.path().join("canonical");

        let staging = stage_canonical_render(&canonical).unwrap();
        assert!(staging.is_dir());
        assert_eq!(staging.parent(), canonical.parent());
        assert!(std::fs::read_dir(&staging).unwrap().next().is_none());
        std::fs::remove_dir_all(staging).unwrap();
    }

    #[test]
    fn stage_canonical_render_copies_populated_root() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = tmp.path().join("canonical");
        std::fs::create_dir_all(canonical.join("nested")).unwrap();
        std::fs::write(canonical.join("root.txt"), b"root").unwrap();
        std::fs::write(canonical.join("nested").join("child.txt"), b"child").unwrap();

        let staging = stage_canonical_render(&canonical).unwrap();
        assert_eq!(
            collect_dir_snapshot(&canonical).unwrap(),
            collect_dir_snapshot(&staging).unwrap()
        );
        std::fs::remove_dir_all(staging).unwrap();
    }

    #[test]
    fn stage_canonical_render_rejects_link_and_cleans_stage() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = tmp.path().join("canonical");
        std::fs::create_dir_all(&canonical).unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(canonical.join("keep.txt"), b"keep").unwrap();
        let link = canonical.join("linked");
        if ccync_foundation::platform::create_dir_link(&target, &link).is_err() {
            return;
        }

        let before = collect_dir_snapshot(&canonical).unwrap_err();
        let err = stage_canonical_render(&canonical).expect_err("links must fail closed");
        let message = format!("{err}");
        assert!(
            message.contains(&link.display().to_string()),
            "error: {message}"
        );
        assert!(before.contains(&link.display().to_string()));
        assert!(std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".ccync-render-stage-")));
    }

    #[test]
    fn render_canonical_root_successfully_replaces_rendered_content() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path().join("plugin");
        for component in COMPONENT_DIRS {
            std::fs::create_dir_all(plugin.join(component)).unwrap();
            std::fs::write(
                plugin.join(component).join(format!("{component}.txt")),
                format!("{component} content"),
            )
            .unwrap();
        }
        std::fs::write(
            plugin.join(".mcp.json"),
            r#"{"mcpServers":{"server":{"command":"tool"}}}"#,
        )
        .unwrap();

        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[("plugin".into(), plugin)]).unwrap();
        for component in COMPONENT_DIRS {
            assert!(canonical.join(component).is_dir());
        }
        let mcp: Value =
            serde_json::from_str(&std::fs::read_to_string(canonical.join(".mcp.json")).unwrap())
                .unwrap();
        assert_eq!(mcp["servers"]["server"]["command"], "tool");
    }

    #[test]
    fn render_canonical_root_preserves_unmanaged_entry() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let canonical = tmp.path().join("canonical");
        std::fs::create_dir_all(&canonical).unwrap();
        std::fs::write(canonical.join("user-file.txt"), b"user content").unwrap();

        render_canonical_root(&canonical, &[]).unwrap();
        assert_eq!(
            std::fs::read(canonical.join("user-file.txt")).unwrap(),
            b"user content"
        );
    }

    #[test]
    fn render_canonical_root_failure_preserves_root_byte_snapshot() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let canonical = tmp.path().join("canonical");
        std::fs::create_dir_all(canonical.join("skills")).unwrap();
        std::fs::write(canonical.join("skills").join("old.md"), b"old").unwrap();
        std::fs::create_dir(canonical.join(".mcp.json")).unwrap();
        std::fs::write(canonical.join(".mcp.json").join("marker"), b"marker").unwrap();

        let plugin = tmp.path().join("plugin");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join(".mcp.json"),
            r#"{"mcpServers":{"server":{"command":"tool"}}}"#,
        )
        .unwrap();
        let before = collect_dir_snapshot(&canonical).unwrap();

        let err = render_canonical_root(&canonical, &[("plugin".into(), plugin)])
            .expect_err("directory at .mcp.json must make the staged write fail");
        assert!(!format!("{err}").is_empty());
        assert_eq!(before, collect_dir_snapshot(&canonical).unwrap());
    }

    #[test]
    fn render_canonical_root_failure_removes_staging_sibling() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let canonical = tmp.path().join("canonical");
        std::fs::create_dir_all(canonical.join(".mcp.json")).unwrap();
        let plugin = tmp.path().join("plugin");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join(".mcp.json"),
            r#"{"mcpServers":{"server":{"command":"tool"}}}"#,
        )
        .unwrap();

        assert!(render_canonical_root(&canonical, &[("plugin".into(), plugin)]).is_err());
        assert!(std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".ccync-render-stage-")));
    }

    #[test]
    fn render_canonical_root_corrupt_plugin_mcp_validates_before_staging() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let canonical = tmp.path().join("canonical");
        std::fs::create_dir_all(&canonical).unwrap();
        std::fs::write(canonical.join("keep.txt"), b"keep").unwrap();
        let before = collect_dir_snapshot(&canonical).unwrap();

        let plugin = tmp.path().join("plugin");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(plugin.join(".mcp.json"), b"{ invalid json").unwrap();
        let err = render_canonical_root(&canonical, &[("plugin".into(), plugin)])
            .expect_err("corrupt plugin MCP must fail before staging");

        assert!(format!("{err}").contains(".mcp.json"));
        assert_eq!(before, collect_dir_snapshot(&canonical).unwrap());
        assert!(std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".ccync-render-stage-")));
    }

    #[test]
    fn render_canonical_root_materializes_managed_plugin_skill() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        // one managed plugin dir with a skill
        let plugin = tmp.path().join("my-plugin");
        let skill = plugin.join("skills").join("doc-sync");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "# doc-sync").unwrap();

        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[("plugin".into(), plugin)]).unwrap();

        let projected = canonical.join("skills").join("doc-sync").join("SKILL.md");
        assert!(
            projected.is_file(),
            "managed plugin skill must be materialized into the canonical root"
        );
        assert_eq!(std::fs::read_to_string(&projected).unwrap(), "# doc-sync");
    }

    #[test]
    fn render_canonical_root_materializes_loose_skill_in_same_pass() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());
        write_loose_skills_lockfile(tmp.path(), &["loose-skill"]);

        let source = tmp
            .path()
            .join(".claude")
            .join("skills")
            .join("loose-skill");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("SKILL.md"), "# loose-skill").unwrap();

        let canonical = tmp.path().join("canonical");
        let warnings = render_canonical_root(&canonical, &[]).unwrap();
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert_eq!(
            std::fs::read_to_string(
                canonical
                    .join("skills")
                    .join("loose-skill")
                    .join("SKILL.md")
            )
            .unwrap(),
            "# loose-skill"
        );
    }

    #[test]
    fn render_canonical_root_missing_loose_source_warns_and_succeeds() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());
        write_loose_skills_lockfile(tmp.path(), &["missing-skill"]);

        let canonical = tmp.path().join("canonical");
        let warnings = render_canonical_root(&canonical, &[]).unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("missing-skill"));
    }

    #[test]
    fn render_canonical_root_loose_copy_failure_preserves_live_root() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());
        write_loose_skills_lockfile(tmp.path(), &["broken-skill"]);

        let source = tmp
            .path()
            .join(".claude")
            .join("skills")
            .join("broken-skill");
        std::fs::create_dir_all(&source).unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        if ccync_foundation::platform::create_dir_link(&target, &source.join("linked")).is_err() {
            return;
        }

        let canonical = tmp.path().join("canonical");
        std::fs::create_dir_all(canonical.join("skills")).unwrap();
        std::fs::write(canonical.join("skills").join("existing.md"), b"existing").unwrap();
        let before = collect_dir_snapshot(&canonical).unwrap();

        assert!(render_canonical_root(&canonical, &[]).is_err());
        assert_eq!(before, collect_dir_snapshot(&canonical).unwrap());
    }

    /// `PayloadShape::PluginRoot`: a reverse-domain extension directory sitting
    /// beside `skills/` at the plugin root must never reach the canonical root.
    /// Only the four component directories are copied.
    #[test]
    fn render_canonical_root_ignores_reverse_domain_dirs() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let plugin = tmp.path().join("plugin-root-fixture");
        let skill = plugin.join("skills").join("x");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "# x").unwrap();

        let vendor_dir = plugin.join("com.example.client");
        std::fs::create_dir_all(&vendor_dir).unwrap();
        std::fs::write(vendor_dir.join("file.txt"), "vendor payload").unwrap();

        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[("plugin".into(), plugin)]).unwrap();

        assert!(
            !canonical.join("com.example.client").exists(),
            "reverse-domain extension dir must never reach the canonical root"
        );
        assert!(
            canonical
                .join("skills")
                .join("x")
                .join("SKILL.md")
                .is_file(),
            "the PluginRoot skills/ content must still be materialized"
        );
    }

    /// `PayloadShape::BareSkill`: current behavior copies every root entry
    /// except `.git`, so a reverse-domain vendor directory sitting beside the
    /// root `SKILL.md` DOES land inside the materialized skill directory.
    /// This test bounds that existing behavior as documented, not as a
    /// requirement introduced by this task.
    #[test]
    fn render_canonical_root_bare_skill_copies_vendor_dir() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let plugin = tmp.path().join("bare-skill-fixture");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join("SKILL.md"),
            "---\nname: bare-skill-fixture\n---\n",
        )
        .unwrap();

        let vendor_dir = plugin.join("com.example.client");
        std::fs::create_dir_all(&vendor_dir).unwrap();
        std::fs::write(vendor_dir.join("file.txt"), "vendor payload").unwrap();

        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[("plugin".into(), plugin)]).unwrap();

        assert!(
            canonical
                .join("skills")
                .join("bare-skill-fixture")
                .join("com.example.client")
                .join("file.txt")
                .is_file(),
            "BareSkill's current copy-everything-but-.git behavior carries the \
             vendor dir into skills/<name>/com.example.client/"
        );
    }

    #[test]
    fn render_canonical_root_skill_collision_first_directory_wins_and_warns() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let plugin1 = tmp.path().join("plugin1");
        let plugin2 = tmp.path().join("plugin2");

        let skill1 = plugin1.join("skills").join("shared-skill");
        std::fs::create_dir_all(&skill1).unwrap();
        std::fs::write(skill1.join("SKILL.md"), "# Skill 1").unwrap();

        let skill2 = plugin2.join("skills").join("shared-skill");
        std::fs::create_dir_all(&skill2).unwrap();
        std::fs::write(skill2.join("SKILL.md"), "# Skill 2 (Duplicate)").unwrap();

        let unique_skill = plugin2.join("skills").join("unique-skill");
        std::fs::create_dir_all(&unique_skill).unwrap();
        std::fs::write(unique_skill.join("SKILL.md"), "# Unique Skill").unwrap();

        let cmd1 = plugin1.join("commands").join("c1.md");
        std::fs::create_dir_all(plugin1.join("commands")).unwrap();
        std::fs::write(cmd1, "cmd 1").unwrap();

        let cmd2 = plugin2.join("commands").join("c2.md");
        std::fs::create_dir_all(plugin2.join("commands")).unwrap();
        std::fs::write(cmd2, "cmd 2").unwrap();

        let canonical = tmp.path().join("canonical");
        let warnings = render_canonical_root(
            &canonical,
            &[("plugin1".into(), plugin1), ("plugin2".into(), plugin2)],
        )
        .unwrap();

        assert_eq!(warnings.len(), 1, "expected 1 warning: {warnings:?}");
        let w = &warnings[0];
        assert!(w.contains("shared-skill"), "warning names skill: {w}");
        assert!(w.contains("plugin1"), "warning names retained plugin: {w}");
        assert!(w.contains("plugin2"), "warning names duplicate plugin: {w}");

        let shared_content = std::fs::read_to_string(
            canonical
                .join("skills")
                .join("shared-skill")
                .join("SKILL.md"),
        )
        .unwrap();
        assert_eq!(shared_content, "# Skill 1", "first directory wins");

        let unique_content = std::fs::read_to_string(
            canonical
                .join("skills")
                .join("unique-skill")
                .join("SKILL.md"),
        )
        .unwrap();
        assert_eq!(unique_content, "# Unique Skill");

        assert!(canonical.join("commands").join("c1.md").is_file());
        assert!(canonical.join("commands").join("c2.md").is_file());
    }

    /// A plugin whose `skills/` holds a loose file next to a skill directory
    /// must render both. `component_count` counts every entry under `skills/`,
    /// so a render that carries only the directories reports more components
    /// than it materializes.
    #[test]
    fn render_canonical_root_copies_loose_files_under_skills() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let plugin = tmp.path().join("mixed-plugin");
        let skill_dir = plugin.join("skills").join("real-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "# Real Skill").unwrap();
        std::fs::write(plugin.join("skills").join("loose.md"), "loose skill").unwrap();

        assert_eq!(component_count(&plugin), 2, "count sees both entries");

        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[("plugin".into(), plugin)]).unwrap();

        assert!(canonical
            .join("skills")
            .join("real-skill")
            .join("SKILL.md")
            .is_file());
        let loose = canonical.join("skills").join("loose.md");
        assert!(
            loose.is_file(),
            "a loose file under skills/ must be rendered, not silently dropped"
        );
        assert_eq!(std::fs::read_to_string(loose).unwrap(), "loose skill");
    }

    /// An empty `skills/` renders nothing but must not fail, and a source whose
    /// `skills/` holds only files still produces the directory that holds them.
    #[test]
    fn render_canonical_root_skills_dir_with_only_files_still_renders() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let plugin = tmp.path().join("files-only-plugin");
        std::fs::create_dir_all(plugin.join("skills")).unwrap();
        std::fs::write(plugin.join("skills").join("one.md"), "one").unwrap();
        std::fs::write(plugin.join("skills").join("two.md"), "two").unwrap();

        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[("plugin".into(), plugin)]).unwrap();

        assert!(canonical.join("skills").join("one.md").is_file());
        assert!(canonical.join("skills").join("two.md").is_file());
    }

    #[test]
    fn render_canonical_root_bare_skill_materialization() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let plugin = tmp.path().join("bare-skill-plugin");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join("SKILL.md"),
            "---\nname: iso-24495\n---\n# ISO 24495 Skill",
        )
        .unwrap();

        let ref_dir = plugin.join("references");
        std::fs::create_dir_all(&ref_dir).unwrap();
        std::fs::write(ref_dir.join("guide.txt"), "reference content").unwrap();

        let git_dir = plugin.join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main").unwrap();

        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[("plugin".into(), plugin.clone())]).unwrap();

        let skill_dir = canonical.join("skills").join("iso-24495");
        assert!(skill_dir.join("SKILL.md").is_file());
        assert!(skill_dir.join("references").join("guide.txt").is_file());
        assert!(
            !skill_dir.join(".git").exists(),
            ".git VCS directory must be excluded"
        );

        // Byte-identical on second run
        let first_skill_md = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
        render_canonical_root(&canonical, &[("plugin".into(), plugin)]).unwrap();
        let second_skill_md = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
        assert_eq!(first_skill_md, second_skill_md);

        // Root plugin.json conforms to standard and omits skills per Probe A listed branch
        let manifest_path = canonical.join("plugin.json");
        let manifest: Value =
            serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
        assert_eq!(
            manifest["$schema"],
            "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json"
        );
        assert!(manifest.get("skills").is_none());

        // Dropped plugin cleans up canonical skills
        render_canonical_root(&canonical, &[]).unwrap();
        assert!(!skill_dir.exists());
    }

    #[test]
    fn render_canonical_root_bare_skill_first_collision_with_plugin_shaped_source() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let bare_plugin = tmp.path().join("bare-plugin");
        std::fs::create_dir_all(&bare_plugin).unwrap();
        std::fs::write(
            bare_plugin.join("SKILL.md"),
            "---\nname: shared-skill\n---\n# Bare Shared Skill",
        )
        .unwrap();

        let plugin_shaped = tmp.path().join("plugin-shaped");
        let skill2 = plugin_shaped.join("skills").join("shared-skill");
        std::fs::create_dir_all(&skill2).unwrap();
        std::fs::write(skill2.join("SKILL.md"), "# Plugin Shared Skill").unwrap();

        let canonical = tmp.path().join("canonical");
        let warnings = render_canonical_root(
            &canonical,
            &[
                ("bare-plugin".into(), bare_plugin.clone()),
                ("plugin-shaped".into(), plugin_shaped),
            ],
        )
        .unwrap();

        assert_eq!(warnings.len(), 1, "expected 1 warning: {warnings:?}");
        let w = &warnings[0];
        assert!(w.contains("shared-skill"), "warning names skill: {w}");

        let content = std::fs::read_to_string(
            canonical
                .join("skills")
                .join("shared-skill")
                .join("SKILL.md"),
        )
        .unwrap();
        assert_eq!(content, "---\nname: shared-skill\n---\n# Bare Shared Skill");
    }

    #[test]
    fn render_canonical_root_two_bare_skills_same_name_collision() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let bare1 = tmp.path().join("bare1");
        std::fs::create_dir_all(&bare1).unwrap();
        std::fs::write(
            bare1.join("SKILL.md"),
            "---\nname: shared-skill\n---\n# Bare 1",
        )
        .unwrap();

        let bare2 = tmp.path().join("bare2");
        std::fs::create_dir_all(&bare2).unwrap();
        std::fs::write(
            bare2.join("SKILL.md"),
            "---\nname: shared-skill\n---\n# Bare 2",
        )
        .unwrap();

        let canonical = tmp.path().join("canonical");
        let warnings = render_canonical_root(
            &canonical,
            &[("bare1".into(), bare1), ("bare2".into(), bare2)],
        )
        .unwrap();

        assert_eq!(warnings.len(), 1, "expected 1 warning: {warnings:?}");
        let w = &warnings[0];
        assert!(w.contains("shared-skill"), "warning names skill: {w}");

        let content = std::fs::read_to_string(
            canonical
                .join("skills")
                .join("shared-skill")
                .join("SKILL.md"),
        )
        .unwrap();
        assert_eq!(content, "---\nname: shared-skill\n---\n# Bare 1");
    }

    #[test]
    fn render_canonical_root_writes_claude_plugin_manifest() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[]).unwrap();

        let manifest_path = canonical.join(".claude-plugin").join("plugin.json");
        assert!(
            manifest_path.is_file(),
            "render_canonical_root must materialize .claude-plugin/plugin.json"
        );
        let manifest: Value =
            serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
        assert_eq!(manifest["name"], "ccync");
        assert_eq!(
            manifest["description"],
            "ccync — cross-agent plugin / MCP / skills manager"
        );
        assert!(
            manifest.get("version").is_none(),
            "plugin manifest must omit version"
        );
    }

    #[test]
    fn render_canonical_root_writes_root_level_plugin_manifest() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let plugin = tmp.path().join("my-plugin");
        for name in ["doc-sync", "adversarial-review"] {
            let skill = plugin.join("skills").join(name);
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(skill.join("SKILL.md"), format!("# {name}")).unwrap();
        }

        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[("plugin".into(), plugin)]).unwrap();

        let manifest_path = canonical.join("plugin.json");
        assert!(
            manifest_path.is_file(),
            "render_canonical_root must materialize a root-level plugin.json \
             so the agy-* junctions resolve to a valid Antigravity plugin"
        );
        let manifest: Value =
            serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
        assert_eq!(
            manifest["$schema"],
            "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json"
        );
        assert_eq!(manifest["name"], "ccync");
        assert_eq!(
            manifest["description"],
            "ccync — cross-agent plugin / MCP / skills manager"
        );
        assert!(
            manifest.get("skills").is_none(),
            "skills key must be absent per Probe A listed branch"
        );
        assert!(
            !canonical.join("mcp.json").exists(),
            "mcp.json must not exist under canonical root"
        );
    }

    /// A machine whose skills all arrive through the loose-skill path (no
    /// managed plugin dirs at all) must publish the loose skill in the
    /// canonical root while keeping the root manifest schema-compliant.
    #[test]
    fn full_run_root_plugin_manifest_lists_loose_skills() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        set_fake_home(tmp.path());

        let source_skill = tmp.path().join(".claude").join("skills").join("graphify");
        std::fs::create_dir_all(&source_skill).unwrap();
        std::fs::write(source_skill.join("SKILL.md"), "# graphify").unwrap();

        let build_root = tmp.path().join(".ccync").join("build");
        std::fs::create_dir_all(&build_root).unwrap();
        std::fs::write(
            build_root.join("lock.json"),
            serde_json::to_string_pretty(&json!({
                "_looseSkills": [{ "name": "graphify", "origin": source_skill }]
            }))
            .unwrap(),
        )
        .unwrap();

        write_selected_runtimes_config(&["claude"], "claude");
        run_update_with_evidence_using(
            &CcyncConfig::default(),
            &crate::delivery_apply::UnavailableNativeRunner,
        )
        .expect("run_update_with_evidence should succeed against a fake home");

        let canonical = build_root.join("render");
        assert!(
            canonical.join("skills").join("graphify").is_dir(),
            "the loose skill must have been materialized into the canonical root"
        );
        let manifest: Value =
            serde_json::from_str(&std::fs::read_to_string(canonical.join("plugin.json")).unwrap())
                .unwrap();
        assert_eq!(
            manifest["$schema"],
            "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json"
        );
        assert_eq!(manifest["name"], "ccync");
        assert_eq!(
            manifest["description"],
            "ccync — cross-agent plugin / MCP / skills manager"
        );
        assert!(
            manifest.get("skills").is_none(),
            "the root manifest omits skills per Probe A listed branch"
        );
    }

    #[test]
    fn render_canonical_root_merges_mcp_first_wins() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let p1 = tmp.path().join("p1");
        let p2 = tmp.path().join("p2");
        std::fs::create_dir_all(&p1).unwrap();
        std::fs::create_dir_all(&p2).unwrap();
        std::fs::write(
            p1.join(".mcp.json"),
            r#"{"servers":{"memory":{"command":"npx"}}}"#,
        )
        .unwrap();
        std::fs::write(
            p2.join(".mcp.json"),
            r#"{"servers":{"memory":{"command":"OVERRIDE"},"fetch":{"command":"uvx"}}}"#,
        )
        .unwrap();

        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[("p1".into(), p1), ("p2".into(), p2)]).unwrap();

        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(canonical.join(".mcp.json")).unwrap())
                .unwrap();
        assert_eq!(
            v["servers"]["memory"]["command"], "npx",
            "first plugin wins on collision"
        );
        assert_eq!(
            v["servers"]["fetch"]["command"], "uvx",
            "second plugin's unique server merged"
        );
    }

    #[test]
    fn render_canonical_root_cross_plugin_collision_warns_once_naming_plugins() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let alpha = tmp.path().join("alpha");
        let beta = tmp.path().join("beta");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        std::fs::write(
            alpha.join(".mcp.json"),
            r#"{"mcpServers":{"memory":{"command":"npx"}}}"#,
        )
        .unwrap();
        std::fs::write(
            beta.join(".mcp.json"),
            r#"{"mcpServers":{"memory":{"command":"OVERRIDE"}}}"#,
        )
        .unwrap();

        let canonical = tmp.path().join("canonical");
        let warnings = render_canonical_root(
            &canonical,
            &[("alpha".into(), alpha), ("beta".into(), beta)],
        )
        .unwrap();

        // Exactly one deterministic warning, naming the server and both
        // plugin labels — and never a server definition.
        assert_eq!(
            warnings.len(),
            1,
            "one collision → one warning: {warnings:?}"
        );
        let w = &warnings[0];
        assert!(w.contains("memory"), "warning names the server: {w}");
        assert!(
            w.contains("alpha"),
            "warning names the retained plugin: {w}"
        );
        assert!(w.contains("beta"), "warning names the skipped plugin: {w}");
        assert!(
            !w.contains("npx") && !w.contains("OVERRIDE"),
            "warning must never include server definitions: {w}"
        );

        // First (lockfile-order) plugin still wins.
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(canonical.join(".mcp.json")).unwrap())
                .unwrap();
        assert_eq!(v["servers"]["memory"]["command"], "npx");
    }

    #[test]
    fn author_mcp_servers_reader_prefers_mcpservers_over_legacy() {
        // Standard key alone.
        let std_only: Value = serde_json::json!({ "mcpServers": { "a": { "command": "x" } } });
        assert!(read_author_mcp_servers(&std_only)
            .unwrap()
            .contains_key("a"));
        // Legacy key alone still accepted.
        let legacy: Value = serde_json::json!({ "servers": { "b": { "command": "y" } } });
        assert!(read_author_mcp_servers(&legacy).unwrap().contains_key("b"));
        // Both present → mcpServers wins.
        let both: Value = serde_json::json!({
            "mcpServers": { "s": { "command": "STANDARD" } },
            "servers":    { "s": { "command": "LEGACY" } }
        });
        assert_eq!(
            read_author_mcp_servers(&both).unwrap()["s"]["command"],
            "STANDARD"
        );
    }

    #[test]
    fn author_mcp_servers_render_and_managed_read_standard_key() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();

        // A standard MCP plugin uses the `mcpServers` key.
        let plugin = tmp.path().join(".ccync/plugins/plugin");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join(".mcp.json"),
            r#"{"mcpServers":{"memory":{"command":"npx","args":["-y","srv"]}}}"#,
        )
        .unwrap();

        // Consumer 1: render_canonical_root writes the internal `servers` shape.
        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[("plugin".into(), plugin)]).unwrap();
        let rendered: Value =
            serde_json::from_str(&std::fs::read_to_string(canonical.join(".mcp.json")).unwrap())
                .unwrap();
        assert_eq!(
            rendered["servers"]["memory"]["command"], "npx",
            "standard mcpServers reaches the internal servers map"
        );
        assert!(
            rendered.get("mcpServers").is_none(),
            "generated file keeps internal `servers`"
        );

        // Consumer 2: generate_managed_mcp reads the same canonical file identically.
        let build = tmp.path().join(".ccync").join("build");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(build.join("lock.json"), r#"{"resolvedPlugins":[{"pluginId":"plugin","installStrategy":"bundled-local","resolvedSource":"plugin"}]}"#).unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        generate_managed_mcp(&canonical, serde_json::Map::new()).unwrap();
        let managed: Value = serde_json::from_str(
            &std::fs::read_to_string(build.join("mcp").join("managed.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            managed["servers"]["memory"]["command"], "npx",
            "both consumers produce the same internal server map"
        );
    }

    #[test]
    fn author_mcp_collisions_core_bundle_and_bundle_adopted() {
        fn map(pairs: &[(&str, &str)]) -> serde_json::Map<String, Value> {
            let mut m = serde_json::Map::new();
            for (k, cmd) in pairs {
                m.insert(k.to_string(), serde_json::json!({ "command": cmd }));
            }
            m
        }

        let sources = vec![
            ("core (.mcp.json)".to_string(), map(&[("memory", "CORE")])),
            (
                "bundle X".to_string(),
                map(&[("memory", "BUNDLE"), ("fetch", "BUNDLE")]),
            ),
            ("adopted master".to_string(), map(&[("fetch", "ADOPTED")])),
        ];

        let (merged, warnings) = merge_mcp_sources_with_warnings(sources);

        // Precedence unchanged: core wins memory, bundle wins fetch.
        assert_eq!(merged["memory"]["command"], "CORE");
        assert_eq!(merged["fetch"]["command"], "BUNDLE");

        assert_eq!(warnings.len(), 2, "one warning per collision");
        assert!(
            warnings.iter().any(|w| w.contains("memory")
                && w.contains("core (.mcp.json)")
                && w.contains("bundle X")),
            "core/bundle collision names both sources: {warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("fetch")
                && w.contains("bundle X")
                && w.contains("adopted master")),
            "bundle/adopted collision names both sources: {warnings:?}"
        );
    }

    #[test]
    fn fetch_dir_plugin_stable_hash_idempotent_and_rejects_links() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("cache");

        // Source directory with a nested file.
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), b"alpha").unwrap();
        std::fs::write(src.join("sub").join("b.txt"), b"beta").unwrap();

        let r1 = fetch_dir_plugin(&cache, "myplug", &src);
        let (path1, sha1) = match &r1 {
            PersonalFetchResult::Cloned { path, sha } => (path.clone(), sha.clone()),
            other => panic!("expected Cloned, got {other:?}"),
        };
        assert_eq!(std::fs::read(path1.join("a.txt")).unwrap(), b"alpha");
        assert_eq!(
            std::fs::read(path1.join("sub").join("b.txt")).unwrap(),
            b"beta"
        );

        // Idempotent: the same content resolves to the same key → AlreadyPresent.
        assert!(matches!(
            fetch_dir_plugin(&cache, "myplug", &src),
            PersonalFetchResult::AlreadyPresent(_)
        ));

        // Content-addressed + separator-normalized: an identical tree under a
        // different id/path hashes identically (paths are `/`-normalized).
        let src2 = tmp.path().join("src2");
        std::fs::create_dir_all(src2.join("sub")).unwrap();
        std::fs::write(src2.join("a.txt"), b"alpha").unwrap();
        std::fs::write(src2.join("sub").join("b.txt"), b"beta").unwrap();
        let sha3 = match fetch_dir_plugin(&cache, "other", &src2) {
            PersonalFetchResult::Cloned { sha, .. } => sha,
            other => panic!("expected Cloned, got {other:?}"),
        };
        assert_eq!(sha1, sha3, "identical trees hash identically");

        // Link rejection leaves no staging residue.
        let linked = tmp.path().join("linked");
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(linked.join("real.txt"), b"x").unwrap();
        let made_link = {
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(linked.join("real.txt"), linked.join("link.txt")).is_ok()
            }
            #[cfg(windows)]
            {
                std::os::windows::fs::symlink_file(linked.join("real.txt"), linked.join("link.txt"))
                    .is_ok()
            }
        };
        // On unix, symlink creation always succeeds — assert it so the
        // security-contract block below can never be silently skipped there.
        #[cfg(unix)]
        assert!(made_link, "unix symlink creation must succeed");
        // On Windows, symlink_file needs Developer Mode or admin; skipping the
        // symlink half there is acceptable because the junction case below
        // provides unprivileged reparse-entry-rejection coverage on every
        // Windows box.
        if made_link {
            let r = fetch_dir_plugin(&cache, "linkplug", &linked);
            match r {
                PersonalFetchResult::CloneFailed(msg) => assert!(
                    msg.contains("refusing to snapshot link/reparse entry"),
                    "symlink must be rejected with the reparse message: {msg}"
                ),
                other => panic!("expected CloneFailed for symlink, got {other:?}"),
            }
            assert!(
                !cache.join("linkplug@__copying__").exists(),
                "no staging residue after a rejected snapshot"
            );
            assert!(
                std::fs::read_dir(&cache)
                    .unwrap()
                    .flatten()
                    .all(|e| !e.file_name().to_string_lossy().starts_with("linkplug@")),
                "no cache entry left for a rejected snapshot"
            );
        }

        // Windows junction: creating one needs NO privileges (unlike a
        // symlink), so this half asserts unconditionally — the same
        // reparse-entry-rejection contract is exercised on every Windows box,
        // elevated or not.
        #[cfg(windows)]
        {
            let junc_src = tmp.path().join("junc");
            std::fs::create_dir_all(&junc_src).unwrap();
            let outside = tmp.path().join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("secret.txt"), b"s").unwrap();
            let status = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(junc_src.join("dirlink"))
                .arg(&outside)
                .status()
                .expect("cmd /C mklink must spawn");
            assert!(
                status.success(),
                "mklink /J requires no privileges and must succeed"
            );

            let r = fetch_dir_plugin(&cache, "juncplug", &junc_src);
            match r {
                PersonalFetchResult::CloneFailed(msg) => assert!(
                    msg.contains("refusing to snapshot link/reparse entry"),
                    "junction must be rejected with the reparse message: {msg}"
                ),
                other => panic!("expected CloneFailed for junction, got {other:?}"),
            }
            assert!(
                !cache.join("juncplug@__copying__").exists(),
                "no staging residue after a rejected junction snapshot"
            );
            assert!(
                std::fs::read_dir(&cache)
                    .unwrap()
                    .flatten()
                    .all(|e| !e.file_name().to_string_lossy().starts_with("juncplug@")),
                "no cache entry left for a rejected junction snapshot"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn dir_snapshot_rejects_backslash_in_unix_filename() {
        // On Unix `\` is a legal filename character; normalizing it to `/`
        // would rewrite the file `a\b` into a traversal into directory `a/`.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a\\b"), b"x").unwrap();
        let err = collect_dir_snapshot(tmp.path())
            .expect_err("a filename containing '\\' must be rejected on unix");
        assert!(
            err.contains("a\\b"),
            "error must name the offending path: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dir_snapshot_rejects_non_utf8_unix_filename() {
        // Two distinct non-UTF-8 names can lossy-map to the same rel; the
        // walker must fail closed instead of silently dropping a file.
        use std::os::unix::ffi::OsStrExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let name = std::ffi::OsStr::from_bytes(&[0x66, 0x6f, 0x80]);
        std::fs::write(tmp.path().join(name), b"x").unwrap();
        let err =
            collect_dir_snapshot(tmp.path()).expect_err("a non-UTF-8 filename must be rejected");
        assert!(
            err.contains("non-UTF-8"),
            "error must state the rejection reason: {err}"
        );
    }

    #[test]
    fn dir_snapshot_hash_framing_is_injective_across_nul_bytes() {
        // Under the old NUL-delimited framing these two distinct trees hashed
        // identically (proven collision from the batch-8 review): tree A is a
        // single file `a` whose BYTES contain the delimiter; tree B re-frames
        // the same byte stream as two files. Length-prefixed framing must
        // separate them.
        let tree_a = vec![("a".to_string(), vec![0x00, 0x62, 0x00, 0x51])];
        let tree_b = vec![("a".to_string(), vec![]), ("b".to_string(), vec![0x51])];
        assert_ne!(
            dir_snapshot_sha_prefix(&tree_a),
            dir_snapshot_sha_prefix(&tree_b),
            "distinct trees must never share a snapshot hash"
        );
    }

    #[test]
    fn author_mcp_collisions_none_without_duplicates() {
        let mut a = serde_json::Map::new();
        a.insert("x".to_string(), serde_json::json!({}));
        let mut b = serde_json::Map::new();
        b.insert("y".to_string(), serde_json::json!({}));
        let (merged, warnings) =
            merge_mcp_sources_with_warnings(vec![("a".to_string(), a), ("b".to_string(), b)]);
        assert_eq!(merged.len(), 2);
        assert!(warnings.is_empty(), "no collision → no warning");
    }

    #[test]
    fn render_canonical_root_empty_set_no_mcp_file() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[]).unwrap();
        assert!(
            canonical.is_dir(),
            "canonical root is created even with no plugins"
        );
        assert!(
            !canonical.join(".mcp.json").exists(),
            "no servers → no .mcp.json"
        );
    }

    // ── render_canonical_root — hooks projection ───────────────────────────────

    #[test]
    fn render_canonical_root_materializes_hooks() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let plugin = tmp.path().join("hooks-plugin");
        let hooks_dir = plugin.join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::write(
            hooks_dir.join("hooks.json"),
            r#"{"hooks":{"PreToolUse":[]}}"#,
        )
        .unwrap();

        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[("plugin".into(), plugin)]).unwrap();

        assert!(
            canonical.join("hooks").join("hooks.json").is_file(),
            "hooks/hooks.json must be materialized into the canonical root"
        );
    }

    #[test]
    fn render_canonical_root_empty_hooks_dir_not_created() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let plugin = tmp.path().join("no-hooks-plugin");
        std::fs::create_dir_all(plugin.join("skills")).unwrap();

        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[("plugin".into(), plugin)]).unwrap();

        assert!(
            !canonical.join("hooks").exists(),
            "empty plugin with no hooks dir must not create hooks in canonical root"
        );
    }

    #[test]
    fn render_canonical_root_prunes_removed_plugin_hooks() {
        let _env = env_guard();
        // After a hooks-bearing plugin is removed, a re-render with the
        // reduced plugin_dirs must prune hooks from the canonical root.
        let tmp = tempfile::TempDir::new().unwrap();
        let plugin_with_hooks = tmp.path().join("plugin-with-hooks");
        let hooks_dir = plugin_with_hooks.join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::write(hooks_dir.join("hooks.json"), r#"{"hooks":{}}"#).unwrap();

        let sibling_plugin = tmp.path().join("sibling-plugin");
        let skill_dir = sibling_plugin.join("skills").join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "# skill").unwrap();

        let canonical = tmp.path().join("canonical");

        // First render: both plugins installed.
        render_canonical_root(
            &canonical,
            &[
                ("plugin_with_hooks".into(), plugin_with_hooks.clone()),
                ("sibling_plugin".into(), sibling_plugin.clone()),
            ],
        )
        .unwrap();
        assert!(
            canonical.join("hooks").join("hooks.json").is_file(),
            "hooks present after first render"
        );
        assert!(
            canonical
                .join("skills")
                .join("my-skill")
                .join("SKILL.md")
                .is_file(),
            "sibling skill present after first render"
        );

        // Second render: hooks plugin removed, only sibling remains.
        render_canonical_root(&canonical, &[("sibling_plugin".into(), sibling_plugin)]).unwrap();
        assert!(
            !canonical.join("hooks").exists(),
            "hooks must be pruned after hooks-bearing plugin is removed"
        );
        assert!(
            canonical
                .join("skills")
                .join("my-skill")
                .join("SKILL.md")
                .is_file(),
            "sibling skill must survive the re-render (siblings untouched)"
        );
    }

    #[test]
    fn render_canonical_root_prunes_stale_mcp_on_re_render() {
        let _env = env_guard();
        // When a plugin that contributed MCP servers is removed, the .mcp.json
        // must not carry stale entries forward.
        let tmp = tempfile::TempDir::new().unwrap();
        let plugin = tmp.path().join("mcp-plugin");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join(".mcp.json"),
            r#"{"servers":{"memory":{"command":"npx"}}}"#,
        )
        .unwrap();

        let canonical = tmp.path().join("canonical");

        // First render: plugin with MCP installed.
        render_canonical_root(&canonical, &[("plugin".into(), plugin)]).unwrap();
        assert!(
            canonical.join(".mcp.json").is_file(),
            ".mcp.json present after first render"
        );

        // Second render: plugin removed → .mcp.json must be gone (no servers).
        render_canonical_root(&canonical, &[]).unwrap();
        assert!(
            !canonical.join(".mcp.json").exists(),
            "stale .mcp.json must be pruned when all MCP plugins are removed"
        );
    }

    // ── materialize_loose_skills ────────────────────────────────────────────────

    /// Write a minimal lockfile with a `_looseSkills` array under `home/.ccync/build/lock.json`
    /// (the path `plugins_lock_path()` resolves to under a faked HOME).
    fn write_loose_skills_lockfile(home: &Path, names: &[&str]) {
        let lock_path = home.join(".ccync").join("build").join("lock.json");
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let entries: Vec<Value> = names
            .iter()
            .map(|n| json!({ "name": n, "origin": home.join(".claude").join("skills").join(n) }))
            .collect();
        std::fs::write(
            &lock_path,
            serde_json::to_string_pretty(&json!({ "_looseSkills": entries })).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn loose_skill_filter_copies_and_compares_the_same_tree_idempotently() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        write_loose_skills_lockfile(tmp.path(), &["filtered-skill"]);
        let src = tmp
            .path()
            .join(".claude")
            .join("skills")
            .join("filtered-skill");
        for dir in ["assets", "nested", ".git/nested", "nested/.git"] {
            std::fs::create_dir_all(src.join(dir)).unwrap();
        }
        for (path, contents) in [
            ("SKILL.md", "# filtered-skill"),
            ("assets/data.json", "{\"ready\":true}"),
            ("nested/keep.txt", "keep"),
            ("backup.bak", "backup"),
            ("patch.orig", "original"),
            ("editor~", "temporary"),
            ("nested/deep.bak", "deep backup"),
            (".git/config", "git metadata"),
            (".git/nested/inside.txt", "nested git metadata"),
            ("nested/.git/inside.txt", "deep git metadata"),
        ] {
            std::fs::write(src.join(path), contents).unwrap();
        }

        let canonical = tmp.path().join("canonical");
        let warnings = materialize_loose_skills(&canonical).unwrap();
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        let dest = canonical.join("skills/filtered-skill");
        for path in ["SKILL.md", "assets/data.json", "nested/keep.txt"] {
            assert!(dest.join(path).is_file(), "ordinary asset missing: {path}");
        }
        for path in [
            "backup.bak",
            "patch.orig",
            "editor~",
            "nested/deep.bak",
            ".git",
            "nested/.git",
        ] {
            assert!(!dest.join(path).exists(), "excluded path copied: {path}");
        }
        assert!(tree_matches(&src, &dest).unwrap());

        let revision = source_content_revision(&src, true).unwrap();
        std::fs::write(src.join("backup.bak"), "changed excluded content").unwrap();
        assert_eq!(source_content_revision(&src, true).unwrap(), revision);

        std::fs::write(dest.join("assets/leftover.orig"), "keep existing file").unwrap();
        let skill_before = std::fs::read(dest.join("SKILL.md")).unwrap();
        let asset_before = std::fs::read(dest.join("assets/data.json")).unwrap();
        let warnings = materialize_loose_skills(&canonical).unwrap();
        assert!(
            warnings.is_empty(),
            "unexpected second-run warnings: {warnings:?}"
        );
        assert!(tree_matches(&src, &dest).unwrap());
        assert_eq!(std::fs::read(dest.join("SKILL.md")).unwrap(), skill_before);
        assert_eq!(
            std::fs::read(dest.join("assets/data.json")).unwrap(),
            asset_before
        );
        assert_eq!(
            std::fs::read(dest.join("assets/leftover.orig")).unwrap(),
            b"keep existing file"
        );
    }

    #[test]
    fn materialize_loose_skills_copies_from_claude_skills_dir_idempotently() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        write_loose_skills_lockfile(tmp.path(), &["my-loose-skill"]);

        let src = tmp
            .path()
            .join(".claude")
            .join("skills")
            .join("my-loose-skill");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("SKILL.md"), "# my-loose-skill").unwrap();

        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[]).unwrap();
        let warnings = materialize_loose_skills(&canonical).unwrap();
        assert!(
            warnings.is_empty(),
            "expected no warnings, got: {warnings:?}"
        );

        let dest = canonical
            .join("skills")
            .join("my-loose-skill")
            .join("SKILL.md");
        assert!(
            dest.is_file(),
            "loose skill must be materialized into canonical skills/"
        );
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "# my-loose-skill");

        // Re-run: idempotent, no error, no duplication, same content.
        render_canonical_root(&canonical, &[]).unwrap();
        let warnings2 = materialize_loose_skills(&canonical).unwrap();
        assert!(warnings2.is_empty());
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "# my-loose-skill");
        let siblings: Vec<_> = std::fs::read_dir(canonical.join("skills"))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(
            siblings.len(),
            1,
            "re-render must not duplicate the skill dir"
        );
    }

    #[test]
    fn materialize_loose_skills_missing_source_warns_and_skips() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        // Lockfile references a skill whose source dir does not exist anywhere.
        write_loose_skills_lockfile(tmp.path(), &["ghost-skill"]);

        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[]).unwrap();
        let warnings = materialize_loose_skills(&canonical).unwrap();

        assert_eq!(
            warnings.len(),
            1,
            "missing source dir must warn exactly once"
        );
        assert!(warnings[0].contains("ghost-skill"));
        assert!(
            !canonical.join("skills").join("ghost-skill").exists(),
            "missing-source skill must not be created"
        );
    }

    #[test]
    fn materialize_loose_skills_uses_each_recorded_origin_and_keeps_output_when_missing() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        let first = tmp.path().join("source-one/shared");
        let second = tmp.path().join("source-two/shared");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("SKILL.md"), "one").unwrap();
        std::fs::write(second.join("SKILL.md"), "two").unwrap();

        for (source, expected) in [(&first, "one"), (&second, "two")] {
            let canonical = tmp.path().join(expected);
            render_canonical_root(&canonical, &[]).unwrap();
            let skill = crate::truth::LooseSkill {
                name: "shared".into(),
                source: None,
                origin: Some(source.clone()),
            };
            assert!(materialize_loose_skills_from(&canonical, &[skill])
                .unwrap()
                .is_empty());
            assert_eq!(
                std::fs::read_to_string(canonical.join("skills/shared/SKILL.md")).unwrap(),
                expected
            );
        }

        let canonical = tmp.path().join("missing-origin");
        render_canonical_root(&canonical, &[]).unwrap();
        let output = canonical.join("skills/shared/SKILL.md");
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        std::fs::write(&output, "previous output").unwrap();
        let missing = crate::truth::LooseSkill {
            name: "shared".into(),
            source: None,
            origin: Some(tmp.path().join("does-not-exist")),
        };
        assert_eq!(
            materialize_loose_skills_from(&canonical, &[missing])
                .unwrap()
                .len(),
            1
        );
        assert_eq!(std::fs::read_to_string(output).unwrap(), "previous output");
    }

    #[test]
    fn materialize_loose_skills_plugin_owned_name_wins_over_loose() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        // A plugin already owns a skill named "shared-name".
        let plugin = tmp.path().join("my-plugin");
        let plugin_skill = plugin.join("skills").join("shared-name");
        std::fs::create_dir_all(&plugin_skill).unwrap();
        std::fs::write(plugin_skill.join("SKILL.md"), "# plugin-owned").unwrap();

        // A loose skill of the same name also exists under the master's skill dir.
        write_loose_skills_lockfile(tmp.path(), &["shared-name"]);
        let loose_src = tmp
            .path()
            .join(".claude")
            .join("skills")
            .join("shared-name");
        std::fs::create_dir_all(&loose_src).unwrap();
        std::fs::write(loose_src.join("SKILL.md"), "# loose-owned").unwrap();

        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[("plugin".into(), plugin)]).unwrap();
        let warnings = materialize_loose_skills(&canonical).unwrap();
        assert_eq!(
            warnings.len(),
            1,
            "collision must be reported: {warnings:?}"
        );
        assert!(warnings[0].contains("plugin-owned skill already occupies"));
        assert!(warnings[0].contains("shared-name"));

        let dest = canonical
            .join("skills")
            .join("shared-name")
            .join("SKILL.md");
        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "# plugin-owned",
            "plugin-owned skill must win over a same-named loose skill"
        );
    }

    #[test]
    fn materialize_loose_skills_no_lockfile_returns_no_warnings() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[]).unwrap();
        let warnings = materialize_loose_skills(&canonical).unwrap();
        assert!(
            warnings.is_empty(),
            "absent lockfile must not produce warnings"
        );
    }

    #[test]
    fn materialize_loose_skills_duplicate_lockfile_entry_copies_once_harmlessly() {
        // A duplicate `name` entry in `_looseSkills` must not error or produce a
        // second warning/attempt beyond the first copy — the second iteration's
        // `dest.exists()` check should just skip it silently.
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        write_loose_skills_lockfile(tmp.path(), &["dup-skill", "dup-skill"]);

        let src = tmp.path().join(".claude").join("skills").join("dup-skill");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("SKILL.md"), "# dup-skill").unwrap();

        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[]).unwrap();
        let warnings = materialize_loose_skills(&canonical).unwrap();

        assert!(
            warnings.is_empty(),
            "duplicate lockfile entry must not produce a warning: {warnings:?}"
        );
        let dest = canonical.join("skills").join("dup-skill").join("SKILL.md");
        assert!(dest.is_file());
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "# dup-skill");
    }

    #[test]
    fn materialize_loose_skills_malformed_and_legacy_rows_are_visible_and_valid_rows_survive() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let lock_path = tmp.path().join(".ccync").join("build").join("lock.json");
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let good_src = tmp.path().join(".claude").join("skills").join("good-skill");
        std::fs::create_dir_all(&good_src).unwrap();
        std::fs::write(good_src.join("SKILL.md"), "# good-skill").unwrap();
        let lock = json!({
            "_looseSkills": [
                { "name": 42 },
                { "name": "good-skill", "source": "claude", "origin": good_src },
                { "name": "legacy-skill", "source": "claude" }
            ]
        });
        std::fs::write(&lock_path, serde_json::to_string_pretty(&lock).unwrap()).unwrap();

        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[]).unwrap();
        let warnings = materialize_loose_skills(&canonical).unwrap();

        assert!(
            warnings
                .iter()
                .any(|w| w.contains("loose skill row 0 is malformed")),
            "malformed row must be reported: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("legacy-skill") && w.contains("rescan")),
            "legacy row must stay visible as rescan-required: {warnings:?}"
        );
        assert_eq!(
            std::fs::read_to_string(canonical.join("skills/good-skill/SKILL.md")).unwrap(),
            "# good-skill",
            "valid rows must still materialize"
        );
    }

    #[test]
    fn materialize_loose_skills_empty_source_dir_materializes_empty_dest() {
        // Source dir exists but has zero files inside — copy_dir_all should
        // still succeed and produce an empty (but present) destination dir;
        // this counts as "materialized", not "missing".
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        write_loose_skills_lockfile(tmp.path(), &["empty-skill"]);
        let src = tmp
            .path()
            .join(".claude")
            .join("skills")
            .join("empty-skill");
        std::fs::create_dir_all(&src).unwrap();

        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[]).unwrap();
        let warnings = materialize_loose_skills(&canonical).unwrap();

        assert!(
            warnings.is_empty(),
            "empty source dir must not warn: {warnings:?}"
        );
        let dest = canonical.join("skills").join("empty-skill");
        assert!(
            dest.is_dir(),
            "empty source dir must still materialize an empty dest dir"
        );
        assert_eq!(
            std::fs::read_dir(&dest).unwrap().count(),
            0,
            "dest dir must be empty, matching the empty source"
        );
    }

    #[test]
    fn materialize_loose_skills_standalone_without_prior_render_creates_skills_dir() {
        // Calling materialize_loose_skills without a preceding render_canonical_root
        // means canonical_root/skills/ does not exist yet. copy_dir_all's
        // fs::create_dir_all(dst) must create the destination tree, not panic.
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        write_loose_skills_lockfile(tmp.path(), &["standalone-skill"]);
        let src = tmp
            .path()
            .join(".claude")
            .join("skills")
            .join("standalone-skill");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("SKILL.md"), "# standalone").unwrap();

        // Canonical root itself doesn't even exist yet — no prior render call.
        let canonical = tmp.path().join("canonical-standalone");
        assert!(!canonical.exists());

        let warnings = materialize_loose_skills(&canonical).unwrap();
        assert!(
            warnings.is_empty(),
            "standalone call must not warn: {warnings:?}"
        );
        let dest = canonical
            .join("skills")
            .join("standalone-skill")
            .join("SKILL.md");
        assert!(
            dest.is_file(),
            "standalone call must create canonical_root/skills/ as needed, not panic"
        );
    }

    // ── fetch_archive_plugin ──────────────────────────────────────────────────

    #[test]
    fn fetch_archive_plugin_extracts_zip_to_cache() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("cache");
        let archive = tmp.path().join("my-plugin.zip");

        // Build a minimal zip with a skills/ file.
        {
            use std::io::Write;
            let file = std::fs::File::create(&archive).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file("skills/SKILL.md", zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"# skill").unwrap();
            zip.finish().unwrap();
        }

        let result = fetch_archive_plugin(&cache, "my-plugin", &archive);
        match result {
            PersonalFetchResult::Cloned { ref path, ref sha } => {
                assert!(path.starts_with(&cache), "extracted into cache root");
                assert_eq!(sha.len(), 12, "sha prefix is 12 hex chars");
                assert!(
                    path.to_string_lossy().contains("my-plugin@"),
                    "cache dir has plugin@sha naming"
                );
                assert!(
                    path.join("skills").join("SKILL.md").is_file(),
                    "skills/SKILL.md materialized inside cache dir"
                );
            }
            other => panic!("expected Cloned, got: {other:?}"),
        }
    }

    #[test]
    fn fetch_archive_plugin_idempotent_returns_already_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("cache");
        let archive = tmp.path().join("my-plugin.zip");

        {
            use std::io::Write;
            let file = std::fs::File::create(&archive).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file("README.md", zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"hello").unwrap();
            zip.finish().unwrap();
        }

        // First fetch
        let r1 = fetch_archive_plugin(&cache, "my-plugin", &archive);
        assert!(matches!(r1, PersonalFetchResult::Cloned { .. }));

        // Second fetch with same archive → AlreadyPresent
        let r2 = fetch_archive_plugin(&cache, "my-plugin", &archive);
        assert!(
            matches!(r2, PersonalFetchResult::AlreadyPresent(_)),
            "same archive fetched twice must return AlreadyPresent"
        );
    }

    #[test]
    fn promote_single_root_dir_does_not_unwrap_multiple_top_level_entries() {
        let tmp = tempfile::TempDir::new().unwrap();
        let wrapper = tmp.path().join("repo-main");
        std::fs::create_dir_all(&wrapper).unwrap();
        std::fs::write(wrapper.join("SKILL.md"), "---\nname: wrapped-skill\n---\n").unwrap();
        std::fs::write(tmp.path().join("archive-note.txt"), "extra").unwrap();

        promote_single_root_dir(tmp.path()).unwrap();

        assert!(
            wrapper.is_dir(),
            "multiple top-level entries must stay wrapped"
        );
        assert!(wrapper.join("SKILL.md").is_file());
        assert!(tmp.path().join("archive-note.txt").is_file());
        assert!(!tmp.path().join("SKILL.md").exists());
    }

    #[test]
    fn promote_single_root_dir_does_not_unwrap_skills_component_wrapper() {
        let tmp = tempfile::TempDir::new().unwrap();
        let wrapper = tmp.path().join("skills");
        fs::create_dir_all(&wrapper).unwrap();
        fs::write(wrapper.join("SKILL.md"), "---\nname: wrapped-skill\n---\n").unwrap();

        promote_single_root_dir(tmp.path()).unwrap();

        assert!(wrapper.is_dir(), "skills component dir must stay wrapped");
        assert!(wrapper.join("SKILL.md").is_file());
        assert!(!tmp.path().join("SKILL.md").exists());
    }

    #[test]
    fn promote_single_root_dir_unwraps_bare_skill_single_root_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let wrapper = tmp.path().join("repo-main");
        fs::create_dir_all(&wrapper).unwrap();
        fs::write(wrapper.join("SKILL.md"), "---\nname: bare-skill\n---\n").unwrap();

        promote_single_root_dir(tmp.path()).unwrap();

        assert!(
            !wrapper.exists(),
            "single root dir wrapper must be unwrapped"
        );
        assert!(tmp.path().join("SKILL.md").is_file());
        assert_eq!(
            payload_shape(tmp.path()),
            PayloadShape::BareSkill("bare-skill".to_string())
        );
    }

    #[test]
    fn is_contained_relative_rejects_traversal_and_absolute() {
        // Safe relative paths pass.
        assert!(is_contained_relative(Path::new("skills/SKILL.md")));
        assert!(is_contained_relative(Path::new("./hooks/hooks.json")));
        assert!(is_contained_relative(Path::new("a/b/c.txt")));
        // Traversal and absolute paths are rejected.
        assert!(!is_contained_relative(Path::new("../escape.txt")));
        assert!(!is_contained_relative(Path::new("a/../../escape.txt")));
        assert!(!is_contained_relative(Path::new("/etc/passwd")));
        #[cfg(windows)]
        assert!(!is_contained_relative(Path::new(
            r"C:\Windows\System32\evil"
        )));
    }

    /// Hand-build a single-entry tar.gz at the byte level so an unsafe entry
    /// name (`..` traversal) can be embedded — `tar::Builder` refuses to write
    /// `..` paths, so it cannot produce the malicious input this guard defends
    /// against. Emits one ustar regular-file entry + the end-of-archive blocks,
    /// gzip-compressed.
    fn build_tar_gz_with_entry_name(name: &[u8], data: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name);
        header[100..108].copy_from_slice(b"0000644\0"); // mode
        header[108..116].copy_from_slice(b"0000000\0"); // uid
        header[116..124].copy_from_slice(b"0000000\0"); // gid
        header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes()); // size
        header[136..148].copy_from_slice(b"00000000000\0"); // mtime
        header[156] = b'0'; // typeflag: regular file
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        // Checksum: sum all bytes with the chksum field treated as spaces.
        for b in &mut header[148..156] {
            *b = b' ';
        }
        let sum: u32 = header.iter().map(|&b| b as u32).sum();
        header[148..156].copy_from_slice(format!("{:06o}\0 ", sum).as_bytes());

        let mut tar = Vec::new();
        tar.extend_from_slice(&header);
        tar.extend_from_slice(data);
        let pad = (512 - data.len() % 512) % 512;
        tar.extend(std::iter::repeat_n(0u8, pad));
        tar.extend(std::iter::repeat_n(0u8, 1024)); // two zero blocks = EOF

        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&tar).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn fetch_archive_plugin_rejects_tar_slip_traversal() {
        // A crafted .tar.gz whose entry path escapes the extraction dir must
        // fail (fail-closed) and write nothing outside the cache root.
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("nested").join("cache");
        let archive = tmp.path().join("evil.tar.gz");
        let bytes = build_tar_gz_with_entry_name(b"../../escape.txt", b"pwned");
        std::fs::write(&archive, &bytes).unwrap();

        let result = fetch_archive_plugin(&cache, "evil", &archive);
        assert!(
            matches!(result, PersonalFetchResult::CloneFailed(_)),
            "tar-slip traversal entry must fail extraction, got: {result:?}"
        );
        // The traversal target (two levels above the cache dir) must not exist.
        assert!(
            !tmp.path().join("escape.txt").exists(),
            "traversal entry must not write outside the extraction dir"
        );
    }

    #[test]
    fn fetch_archive_plugin_extracts_tar_gz_to_cache() {
        // Positive path: a well-formed tar.gz extracts its real content.
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("cache");
        let archive = tmp.path().join("good.tar.gz");

        {
            use flate2::write::GzEncoder;
            use flate2::Compression;
            let file = std::fs::File::create(&archive).unwrap();
            let enc = GzEncoder::new(file, Compression::default());
            let mut builder = tar::Builder::new(enc);
            let data = b"# skill";
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o644);
            builder
                .append_data(&mut header, "skills/SKILL.md", &data[..])
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }

        let result = fetch_archive_plugin(&cache, "good", &archive);
        match result {
            PersonalFetchResult::Cloned { ref path, .. } => {
                assert!(
                    path.join("skills").join("SKILL.md").is_file(),
                    "tar.gz content must materialize inside the cache dir"
                );
            }
            other => panic!("expected Cloned, got: {other:?}"),
        }
    }

    #[test]
    fn fetch_archive_plugin_strips_single_root_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("cache");
        let archive = tmp.path().join("repo.zip");

        // GitHub-style archive: all files under a single root dir.
        {
            use std::io::Write;
            let file = std::fs::File::create(&archive).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.add_directory("repo-main/", zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.start_file(
                "repo-main/skills/SKILL.md",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            zip.write_all(b"# skill").unwrap();
            zip.finish().unwrap();
        }

        let result = fetch_archive_plugin(&cache, "repo", &archive);
        let path = match result {
            PersonalFetchResult::Cloned { path, .. } => path,
            other => panic!("expected Cloned, got: {other:?}"),
        };
        assert!(
            path.join("skills").join("SKILL.md").is_file(),
            "single root dir must be stripped; skills/ at top level"
        );
        assert!(
            !path.join("repo-main").exists(),
            "inner root dir must not remain after promotion"
        );
    }

    // ── load_uninstall_prereqs ───────────────────────────────────────────────

    #[test]
    fn preflight_missing_managed_json_warns_and_does_not_claim_mcp_cleanup() {
        let home = crate::test_support::TestHome::new();
        let ccync_home = home.path().join(".ccync");
        // Create build/ but NOT build/mcp/managed.json
        std::fs::create_dir_all(ccync_home.join("build").join("mcp")).unwrap();
        std::fs::create_dir_all(ccync_home.join("build")).unwrap();
        std::fs::write(ccync_home.join("config.json"), "{}").unwrap();
        std::fs::write(ccync_home.join("build").join("lock.json"), "{}").unwrap();
        // managed.json intentionally absent

        let layout = ccync_foundation::paths::EffectiveLayout::load_or_legacy().unwrap();
        let (prereqs, warnings) = load_uninstall_prereqs(&layout);

        // Must warn about the missing surface by name.
        assert!(
            warnings.iter().any(|w| w.contains("managed.json")),
            "missing managed.json must produce a named warning; got: {warnings:?}"
        );
        // Must not claim complete MCP cleanup.
        assert!(
            warnings.iter().any(|w| w.contains("skipped")),
            "warning must state MCP de-projection is skipped; got: {warnings:?}"
        );
        // The prereq struct reflects absence.
        assert!(
            prereqs.managed_json.is_none(),
            "managed_json should be None when absent"
        );
        // Present files are resolved correctly.
        assert!(
            prereqs.lock_json.is_some(),
            "lock_json should be Some when present"
        );
        assert!(
            prereqs.config_json.is_some(),
            "config_json should be Some when present"
        );
    }

    #[test]
    fn preflight_all_files_present_no_warnings() {
        let home = crate::test_support::TestHome::new();
        let ccync_home = home.path().join(".ccync");
        std::fs::create_dir_all(ccync_home.join("build").join("mcp")).unwrap();
        std::fs::write(ccync_home.join("config.json"), "{}").unwrap();
        std::fs::write(ccync_home.join("build").join("lock.json"), "{}").unwrap();
        std::fs::write(
            ccync_home.join("build").join("mcp").join("managed.json"),
            "{}",
        )
        .unwrap();

        let layout = ccync_foundation::paths::EffectiveLayout::load_or_legacy().unwrap();
        let (_prereqs, warnings) = load_uninstall_prereqs(&layout);
        assert!(
            warnings.is_empty(),
            "no warnings when all prereq files present: {warnings:?}"
        );
    }

    // ── preserved_files_note: report exactly which input files are present ──

    #[test]
    fn preserved_files_note_both_present() {
        assert_eq!(
            preserved_files_note(true, true, false, false),
            "config.json + plugins.json"
        );
    }

    #[test]
    fn preserved_files_note_only_plugins_json_present() {
        // Regression: must never claim "(none present)" when plugins.json
        // actually exists.
        let note = preserved_files_note(false, true, false, false);
        assert!(note.contains("plugins.json"));
        assert!(!note.contains("config.json"));
    }

    #[test]
    fn preserved_files_note_only_config_json_present() {
        let note = preserved_files_note(true, false, false, false);
        assert!(note.contains("config.json"));
        assert!(!note.contains("plugins.json"));
    }

    #[test]
    fn preserved_files_note_neither_present() {
        assert_eq!(
            preserved_files_note(false, false, false, false),
            "(none present)"
        );
    }

    #[test]
    fn preserved_files_note_reports_mcp_json_when_present() {
        assert_eq!(preserved_files_note(false, false, false, true), "mcp.json");
    }

    #[test]
    fn preserved_files_note_all_four_inputs_present() {
        assert_eq!(
            preserved_files_note(true, true, true, true),
            "config.json + plugins.json + AGENTS.md + mcp.json"
        );
    }

    #[test]
    fn preserved_files_note_reports_only_existing_inputs() {
        assert_eq!(
            preserved_files_note(true, false, false, true),
            "config.json + mcp.json"
        );
        assert_eq!(
            preserved_files_note(false, true, true, false),
            "plugins.json + AGENTS.md"
        );
    }

    // ── MCP de-projection in run_uninstall ──────────────────────────────────

    #[test]
    fn mcp_managed_entries_removed_user_entries_preserved_on_uninstall() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let fake_home = tmp.path();

        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", fake_home);
            std::env::set_var("HOME", fake_home);
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", fake_home);
        }

        // Seed managed.json with the managed server name.
        let build_mcp = fake_home.join(".ccync").join("build").join("mcp");
        std::fs::create_dir_all(&build_mcp).unwrap();
        std::fs::write(
            build_mcp.join("managed.json"),
            serde_json::json!({
                "mcpServers": {
                    "ccync-managed-tool": { "command": "ccync-tool", "args": [] }
                }
            })
            .to_string(),
        )
        .unwrap();

        // Seed committed per-host ownership beside managed.json (the write-ahead
        // model: uninstall removes only names ccync proved it owns per host).
        std::fs::write(
            build_mcp.join("projected-state.json"),
            serde_json::json!({
                "version": 1,
                "hosts": { "claude": ["ccync-managed-tool"] }
            })
            .to_string(),
        )
        .unwrap();

        // Seed ~/.claude.json with one managed entry + one user entry.
        let claude_json = fake_home.join(".claude.json");
        std::fs::write(
            &claude_json,
            serde_json::json!({
                "mcpServers": {
                    "ccync-managed-tool": { "command": "ccync-tool", "args": [] },
                    "user-personal-tool": { "command": "my-tool", "args": [] }
                }
            })
            .to_string(),
        )
        .unwrap();

        // Create canonical root so run_uninstall can find it.
        std::fs::create_dir_all(fake_home.join(".ccync").join("build").join("render")).unwrap();

        run_uninstall().unwrap();

        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&claude_json).unwrap()).unwrap();
        let servers = after["mcpServers"].as_object().unwrap();
        assert!(
            !servers.contains_key("ccync-managed-tool"),
            "managed entry must be removed; servers: {servers:?}"
        );
        assert!(
            servers.contains_key("user-personal-tool"),
            "user entry must be preserved; servers: {servers:?}"
        );
    }

    // ── TP-05a: Claude marketplace / plugin-state residue removal ────────────

    #[test]
    fn claude_installed_plugins_drops_ccync_keeps_user_records() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp
            .path()
            .join(".claude")
            .join("plugins")
            .join("installed_plugins.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({
                "plugins": {
                    "ccync@ccync": [{ "version": "1.0.1" }],
                    "sometool@othermarket": [{ "version": "2.0.0" }]
                }
            })
            .to_string(),
        )
        .unwrap();

        let mut warnings = Vec::new();
        remove_claude_installed_plugins_residue(tmp.path(), &BTreeSet::new(), &mut warnings);

        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let plugins = after["plugins"].as_object().unwrap();
        assert!(!plugins.contains_key("ccync@ccync"), "ccync record removed");
        assert!(
            plugins.contains_key("sometool@othermarket"),
            "user record preserved"
        );
        assert!(
            warnings.is_empty(),
            "clean removal emits no failure: {warnings:?}"
        );
    }

    #[test]
    fn provider_lifecycle_claude_residue_keeps_only_failed_selector_for_retry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp
            .path()
            .join(".claude")
            .join("plugins")
            .join("installed_plugins.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({"plugins": {
                "ccync@ccync": [{"version": "1"}],
                "other@ccync": [{"version": "1"}],
                "user@other": [{"version": "1"}]
            }})
            .to_string(),
        )
        .unwrap();

        let mut warnings = Vec::new();
        remove_claude_installed_plugins_residue(
            tmp.path(),
            &["other@ccync".to_string()].into_iter().collect(),
            &mut warnings,
        );
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let plugins = after["plugins"].as_object().unwrap();
        assert!(!plugins.contains_key("ccync@ccync"));
        assert!(plugins.contains_key("other@ccync"));
        assert!(plugins.contains_key("user@other"));
        assert!(warnings.is_empty());
    }

    /// Fixture uses the REAL flat shape Claude Code writes (top-level key is
    /// the marketplace name, no `"marketplaces"` wrapper), de-identified from
    /// an observed `known_marketplaces.json` — including the
    /// `{"source":"github","repo":"owner/repo"}` discriminator that a
    /// Claude-registered entry actually carries.
    #[test]
    fn claude_known_marketplaces_drops_ccync_keeps_user_marketplaces() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp
            .path()
            .join(".claude")
            .join("plugins")
            .join("known_marketplaces.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({
                "ccync": { "source": { "type": "local" } },
                "user-market": {
                    "source": { "source": "github", "repo": "example/user-market" },
                    "installLocation": "/somewhere/user-market"
                }
            })
            .to_string(),
        )
        .unwrap();

        let mut warnings = Vec::new();
        remove_claude_known_marketplaces_residue(tmp.path(), &mut warnings);

        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let mkts = after.as_object().unwrap();
        assert!(!mkts.contains_key("ccync"), "ccync marketplace removed");
        assert!(
            mkts.contains_key("user-market"),
            "user marketplace preserved"
        );
        assert_eq!(
            mkts["user-market"]["source"]["repo"], "example/user-market",
            "unrelated marketplace preserved verbatim, not just by key"
        );
        assert!(warnings.is_empty(), "clean removal warns about nothing");
    }

    /// A real file that holds marketplaces but no `ccync` entry must not be
    /// rewritten, and must not claim the file lacks marketplaces. Guards the
    /// message regression where a flat file was reported as having "no
    /// `marketplaces` object".
    #[test]
    fn claude_known_marketplaces_without_ccync_is_left_byte_identical() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp
            .path()
            .join(".claude")
            .join("plugins")
            .join("known_marketplaces.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = serde_json::json!({
            "user-market": { "source": { "source": "github", "repo": "example/user-market" } }
        })
        .to_string();
        std::fs::write(&path, &original).unwrap();

        let mut warnings = Vec::new();
        remove_claude_known_marketplaces_residue(tmp.path(), &mut warnings);

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "no ccync entry: file must not be rewritten at all"
        );
        assert!(
            warnings.is_empty(),
            "a no-op de-registration is not a warning"
        );
    }

    #[test]
    fn claude_installed_plugins_malformed_preserved_byte_for_byte_with_warning() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp
            .path()
            .join(".claude")
            .join("plugins")
            .join("installed_plugins.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = "{ this is not valid json @@@";
        std::fs::write(&path, original).unwrap();

        let mut warnings = Vec::new();
        remove_claude_installed_plugins_residue(tmp.path(), &BTreeSet::new(), &mut warnings);

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "malformed file must be preserved byte-for-byte"
        );
        assert!(
            warnings.iter().any(|w| w.contains("parse failed")),
            "a named parse warning must be emitted: {warnings:?}"
        );
    }

    // ── TP-05b: Codex de-registration skip when CLI absent ───────────────────

    // Mirrors projection's `normalize_managed_artifact_path` so a hand-written
    // lockfile key matches what the authority computes for a real path.
    fn norm_key(p: &Path) -> String {
        projection::normalize_managed_artifact_path(p)
    }

    #[cfg(windows)]
    #[test]
    fn projection_path_key_preserves_windows_golden_values() {
        for (input, expected) in [
            (r"C:/Users/ALICE/Skill", r"c:\users\alice\skill"),
            (r"\\SERVER/Share/Item", r"\\server\share\item"),
            (r"\\?\C:/Folder/Item", r"\\?\c:\folder\item"),
            (r"C:/Ä/İ", r"c:\Ä\İ"),
        ] {
            assert_eq!(path_key(Path::new(input)), expected);
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn legacy_source_path_key_preserves_source_identifiers() {
        let path = Path::new("/home/Alice/plugins/Example");
        assert_eq!(format!("plugin:example:{}", legacy_source_path_key(path)),
                   r"plugin:example:\home\Alice\plugins\Example");
        assert_eq!(format!("loose-origin:{}", legacy_source_path_key(path)),
                   r"loose-origin:\home\Alice\plugins\Example");
    }

    #[cfg(not(windows))]
    #[test]
    fn prune_legacy_key_removes_managed_copy_and_preserves_user_file() {
        use sha2::{Digest, Sha256};
        let tmp = tempfile::tempdir().unwrap();
        let managed = tmp.path().join("skills/managed");
        let user = tmp.path().join("skills/user/SKILL.md");
        fs::create_dir_all(&managed).unwrap();
        fs::write(managed.join("SKILL.md"), "managed").unwrap();
        fs::create_dir_all(user.parent().unwrap()).unwrap();
        fs::write(&user, "user content").unwrap();
        let before = Sha256::digest(fs::read(&user).unwrap());
        let lock = tmp.path().join("lock.json");
        let legacy = managed.to_string_lossy().replace('/', "\\");
        fs::write(&lock, json!({"_ccyncProjection": {"directSkillProjectionPaths": [legacy]}}).to_string()).unwrap();
        let records = projection::direct_records(&lock);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].path, managed);
        let mut warnings = Vec::new();
        prune_unplanned_direct_records(&lock, &records, |_| false, &mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!managed.exists());
        assert_eq!(before, Sha256::digest(fs::read(&user).unwrap()));
    }

    #[cfg(not(windows))]
    #[test]
    fn uninstall_legacy_key_removes_managed_copy_and_preserves_user_file() {
        use sha2::{Digest, Sha256};
        let tmp = tempfile::tempdir().unwrap();
        let surface = tmp.path().join("skills");
        let managed = surface.join("managed");
        let user = surface.join("user/SKILL.md");
        fs::create_dir_all(&managed).unwrap();
        fs::write(managed.join("SKILL.md"), "managed").unwrap();
        fs::create_dir_all(user.parent().unwrap()).unwrap();
        fs::write(&user, "user content").unwrap();
        let before = Sha256::digest(fs::read(&user).unwrap());
        let lock = tmp.path().join("lock.json");
        fs::write(&lock, json!({"_ccyncProjection": {"skillProjectionPaths": [managed.to_string_lossy().replace('/', "\\")]}}).to_string()).unwrap();
        let authority = projection::ManagedDeprojection::from_lockfile(&lock);
        let mut warnings = Vec::new();
        assert!(!deproject_managed_surfaces(&lock, &authority, &[surface], &mut warnings));
        assert!(!managed.exists());
        assert_eq!(before, Sha256::digest(fs::read(&user).unwrap()));
    }

    // ── registry-guarded surface de-projection ──────────────────────────────

    #[test]
    fn deproject_managed_surfaces_removes_only_authorized_entries() {
        let tmp = tempfile::TempDir::new().unwrap();
        let surface = tmp.path().join(".claude").join("skills");
        std::fs::create_dir_all(&surface).unwrap();
        let managed = surface.join("ccync");
        let user_sibling = surface.join("user-tool");
        std::fs::create_dir_all(&managed).unwrap();
        std::fs::write(managed.join("SKILL.md"), "x").unwrap();
        std::fs::create_dir_all(&user_sibling).unwrap();
        std::fs::write(user_sibling.join("SKILL.md"), "y").unwrap();
        // AGY GUI TOML analogue outside the scanned surfaces — must survive.
        let agy_gui = tmp
            .path()
            .join(".gemini")
            .join("antigravity-ide")
            .join("settings.toml");
        std::fs::create_dir_all(agy_gui.parent().unwrap()).unwrap();
        std::fs::write(&agy_gui, "[user]\nkeep = true\n").unwrap();

        // Record `managed` as a ccync skill projection in a prior lockfile.
        let lock = tmp.path().join(".ccync").join("build").join("lock.json");
        std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
        std::fs::write(
            &lock,
            serde_json::json!({
                "_ccyncProjection": { "skillProjectionPaths": [ norm_key(&managed) ] }
            })
            .to_string(),
        )
        .unwrap();

        let authority = projection::ManagedDeprojection::from_lockfile(&lock);
        let mut warnings = Vec::new();
        let retain = deproject_managed_surfaces(
            &lock,
            &authority,
            std::slice::from_ref(&surface),
            &mut warnings,
        );

        assert!(!retain, "a clean legacy removal keeps no direct rows");
        assert!(!managed.exists(), "ccync-managed entry must be removed");
        assert!(
            user_sibling.exists(),
            "user-owned sibling must be preserved"
        );
        assert!(
            agy_gui.exists(),
            "AGY GUI TOML must survive (outside scanned surfaces)"
        );
        assert!(
            warnings.is_empty(),
            "clean removal emits no warnings: {warnings:?}"
        );
    }

    #[test]
    fn planned_command_skill_record_keeps_its_skill_file() {
        let home = Path::new("home");
        let planned = BTreeSet::from([path_key(&home.join(".copilot/skills/deploy"))]);

        // The record names SKILL.md, while the route names the directory.
        assert!(is_planned_direct_copy(
            &home.join(".copilot/skills/deploy/SKILL.md"),
            &planned
        ));
        assert!(!is_planned_direct_copy(
            &home.join(".copilot/skills/stale/SKILL.md"),
            &planned
        ));
    }

    #[test]
    #[cfg(windows)]
    fn deproject_direct_removal_failure_keeps_row_and_retry_removes_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let agents = tmp.path().join(".copilot").join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let managed = agents.join("helper.agent.md");
        std::fs::write(&managed, "managed").unwrap();
        // A handle that shares nothing blocks the deletion on Windows, which
        // forces the failure. The default open shares delete, so it would not.
        let held = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&managed)
                .unwrap()
        };
        let sibling = agents.join("user.agent.md");
        std::fs::write(&sibling, "user").unwrap();

        let lock = tmp.path().join(".ccync").join("build").join("lock.json");
        std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
        std::fs::write(
            &lock,
            serde_json::json!({
                "_ccyncProjection": { "directAgentProjectionPaths": [ norm_key(&managed) ] }
            })
            .to_string(),
        )
        .unwrap();

        let authority = projection::ManagedDeprojection::from_lockfile(&lock);
        let mut warnings = Vec::new();
        let retain = deproject_managed_surfaces(&lock, &authority, &[], &mut warnings);
        assert!(retain, "a failed direct removal must report retention");
        assert!(managed.exists(), "the undeletable copy stays on disk");
        assert!(
            projection::direct_records(&lock)
                .iter()
                .any(|record| record.path == Path::new(&norm_key(&managed))),
            "the row for the surviving copy must be kept"
        );
        assert!(sibling.exists(), "the foreign sibling is untouched");

        drop(held);
        let authority = projection::ManagedDeprojection::from_lockfile(&lock);
        let mut warnings = Vec::new();
        let retain = deproject_managed_surfaces(&lock, &authority, &[], &mut warnings);
        assert!(!retain, "a successful retry keeps nothing");
        assert!(!managed.exists(), "the retry removes the copy");
        assert!(
            projection::direct_records(&lock).is_empty(),
            "the row goes only after its copy is gone"
        );
        assert!(sibling.exists(), "the foreign sibling survives the retry");
    }

    #[test]
    fn prune_build_keeps_lock_json_while_direct_rows_are_retained() {
        let tmp = tempfile::TempDir::new().unwrap();
        let build = tmp.path().join("build");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(build.join("lock.json"), "{}").unwrap();
        std::fs::write(build.join("scratch.txt"), "x").unwrap();

        let mut warnings = Vec::new();
        prune_build_except_retained_rows(&build, false, false, false, true, &mut warnings);
        assert!(build.join("lock.json").is_file(), "lock.json must survive");
        assert!(!build.join("scratch.txt").exists());

        prune_build_except_retained_rows(&build, false, false, false, false, &mut warnings);
        assert!(!build.join("lock.json").exists());
    }

    #[test]
    fn deproject_managed_surfaces_empty_prior_preserves_user_sibling() {
        let tmp = tempfile::TempDir::new().unwrap();
        let surface = tmp.path().join(".claude").join("skills");
        std::fs::create_dir_all(&surface).unwrap();
        // A same-named entry a ccync projection WOULD use, but here user-owned.
        let same_name = surface.join("ccync");
        std::fs::create_dir_all(&same_name).unwrap();

        // Missing prior lockfile → authority authorizes nothing (no allow-all).
        let lock = tmp.path().join("nolock.json");
        let authority = projection::ManagedDeprojection::from_lockfile(&lock);
        let mut warnings = Vec::new();
        deproject_managed_surfaces(
            &lock,
            &authority,
            std::slice::from_ref(&surface),
            &mut warnings,
        );

        assert!(
            same_name.exists(),
            "empty/missing prior must NOT delete a same-named user entry"
        );
    }

    #[test]
    fn codex_deregister_absent_cli_emits_named_skip_only() {
        // codex_available = false → a single named skip note (informational),
        // never a claim of completion and never a hard failure.
        let notes = deregister_codex_marketplace_impl(false);
        assert_eq!(notes.len(), 1, "exactly one skip note: {notes:?}");
        assert!(
            notes[0].contains("skipped"),
            "skip note must name the skipped Codex de-registration: {notes:?}"
        );
    }

    mod projection_run_accumulator {
        use super::*;

        fn written(path: &str) -> projection::ProjectionPathResult {
            projection::ProjectionPathResult {
                path: PathBuf::from(path),
                change: projection::ProjectionPathChange::Written,
            }
        }

        fn evidence_with_target(
            target: ProjectionTargetId,
            target_evidence: projection::ProjectionTargetEvidence,
            completed: bool,
        ) -> projection::ProjectionEvidence {
            let mut evidence = projection::ProjectionEvidence::default();
            evidence.by_target.insert(target, target_evidence);
            if completed {
                evidence.completed_targets.insert(target);
            }
            evidence
        }

        /// Row: a target with a recorded failure in either phase is `Failed`,
        /// even when it also completed both phases (failure wins over
        /// completion).
        #[test]
        fn failure_wins_over_both_phase_completion() {
            let target = ProjectionTargetId::Claude;
            let failing_evidence = projection::ProjectionTargetEvidence::new(
                vec![written("claude/skills/ccync")],
                Vec::new(),
                None,
                Some(projection::ProjectionExpectedFailure {
                    path: Some(PathBuf::from("claude/skills/ccync")),
                    reason: "permission denied".to_string(),
                }),
            )
            .unwrap();

            let mut acc = ProjectionRunAccumulator::new("normal", [target]);
            acc.record_canonical(&evidence_with_target(target, failing_evidence, true));
            acc.record_machine(&evidence_with_target(
                target,
                projection::ProjectionTargetEvidence::default(),
                true,
            ));

            let report = acc.finish();
            let result = &report.targets[&target];
            assert_eq!(
                result.status,
                ProjectionTargetStatus::Failed {
                    path: Some(PathBuf::from("claude/skills/ccync")),
                    reason: "permission denied".to_string(),
                }
            );
        }

        fn failure_at(path: &str, reason: &str) -> projection::ProjectionExpectedFailure {
            projection::ProjectionExpectedFailure {
                path: Some(PathBuf::from(path)),
                reason: reason.to_string(),
            }
        }

        /// Merge row: an earlier target failure survives a later entry with no
        /// failure. The merged entry takes the later paths and has no skip
        /// state, even though the later entry recorded a skip.
        #[test]
        fn merge_keeps_earlier_failure_over_later_entry_without_failure() {
            let target = ProjectionTargetId::Claude;
            let earlier = projection::ProjectionTargetEvidence::new(
                vec![written("claude/skills/old")],
                Vec::new(),
                None,
                Some(failure_at("claude/skills/old", "permission denied")),
            )
            .unwrap();
            let later = projection::ProjectionTargetEvidence::new(
                vec![written("claude/skills/new")],
                Vec::new(),
                Some("user opted out".to_string()),
                None,
            )
            .unwrap();

            let mut merged = evidence_with_target(target, earlier, false);
            merge_projection_evidence(&mut merged, evidence_with_target(target, later, false));

            let entry = &merged.by_target[&target];
            assert_eq!(
                entry.failure,
                Some(failure_at("claude/skills/old", "permission denied"))
            );
            assert_eq!(entry.paths, vec![written("claude/skills/new")]);
            assert!(entry.skipped.is_none());
        }

        /// Merge row: a later target failure replaces an earlier one.
        #[test]
        fn merge_later_failure_replaces_earlier_failure() {
            let target = ProjectionTargetId::Claude;
            let earlier = projection::ProjectionTargetEvidence::new(
                vec![written("claude/skills/old")],
                Vec::new(),
                None,
                Some(failure_at("claude/skills/old", "first error")),
            )
            .unwrap();
            let later = projection::ProjectionTargetEvidence::new(
                vec![written("claude/skills/new")],
                Vec::new(),
                None,
                Some(failure_at("claude/skills/new", "second error")),
            )
            .unwrap();

            let mut merged = evidence_with_target(target, earlier, false);
            merge_projection_evidence(&mut merged, evidence_with_target(target, later, false));

            let entry = &merged.by_target[&target];
            assert_eq!(
                entry.failure,
                Some(failure_at("claude/skills/new", "second error"))
            );
            assert_eq!(entry.paths, vec![written("claude/skills/new")]);
        }

        /// Merge row: a failure kept by a merge still derives `Failed` in the
        /// run accumulator, even when the target is in `completed_targets` for
        /// both phases.
        #[test]
        fn merged_kept_failure_derives_failed_despite_both_phase_completion() {
            let target = ProjectionTargetId::Claude;
            let earlier = projection::ProjectionTargetEvidence::new(
                vec![written("claude/skills/old")],
                Vec::new(),
                None,
                Some(failure_at("claude/skills/old", "permission denied")),
            )
            .unwrap();
            let later = projection::ProjectionTargetEvidence::default();

            let mut merged = evidence_with_target(target, earlier, true);
            merge_projection_evidence(&mut merged, evidence_with_target(target, later, true));

            let mut acc = ProjectionRunAccumulator::new("normal", [target]);
            acc.record_canonical(&merged);
            acc.record_machine(&evidence_with_target(
                target,
                projection::ProjectionTargetEvidence::default(),
                true,
            ));

            let report = acc.finish();
            assert_eq!(
                report.targets[&target].status,
                ProjectionTargetStatus::Failed {
                    path: Some(PathBuf::from("claude/skills/old")),
                    reason: "permission denied".to_string(),
                }
            );
        }

        /// Sequential phase merge chain: an initial failure is retained when
        /// followed by an entry with paths and no failure (clearing any skip),
        /// and then a subsequent entry with its own failure replaces the kept failure.
        #[test]
        fn merge_evidence_sequence_retains_failure_clears_skip_then_replaces_failure() {
            let target = ProjectionTargetId::Claude;
            let first = projection::ProjectionTargetEvidence::new(
                vec![written("claude/skills/phase1")],
                Vec::new(),
                None,
                Some(failure_at("claude/skills/phase1", "disk full")),
            )
            .unwrap();
            let second = projection::ProjectionTargetEvidence::new(
                vec![written("claude/skills/phase2")],
                Vec::new(),
                Some("skipped by policy".to_string()),
                None,
            )
            .unwrap();
            let third = projection::ProjectionTargetEvidence::new(
                vec![written("claude/skills/phase3")],
                Vec::new(),
                None,
                Some(failure_at("claude/skills/phase3", "network error")),
            )
            .unwrap();

            let mut merged = evidence_with_target(target, first, false);

            // Merge second: retains phase1 failure, takes phase2 paths, clears skip.
            merge_projection_evidence(&mut merged, evidence_with_target(target, second, false));
            let mid_entry = &merged.by_target[&target];
            assert_eq!(
                mid_entry.failure,
                Some(failure_at("claude/skills/phase1", "disk full"))
            );
            assert_eq!(mid_entry.paths, vec![written("claude/skills/phase2")]);
            assert!(mid_entry.skipped.is_none());

            let mut acc_mid = ProjectionRunAccumulator::new("normal", [target]);
            acc_mid.record_canonical(&merged);
            acc_mid.record_machine(&evidence_with_target(
                target,
                projection::ProjectionTargetEvidence::default(),
                true,
            ));
            let mid_report = acc_mid.finish();
            assert_eq!(
                mid_report.targets[&target].status,
                ProjectionTargetStatus::Failed {
                    path: Some(PathBuf::from("claude/skills/phase1")),
                    reason: "disk full".to_string(),
                }
            );

            // Merge third: replaces with phase3 failure and phase3 paths.
            merge_projection_evidence(&mut merged, evidence_with_target(target, third, false));
            let final_entry = &merged.by_target[&target];
            assert_eq!(
                final_entry.failure,
                Some(failure_at("claude/skills/phase3", "network error"))
            );
            assert_eq!(final_entry.paths, vec![written("claude/skills/phase3")]);

            let mut acc_final = ProjectionRunAccumulator::new("normal", [target]);
            acc_final.record_canonical(&merged);
            acc_final.record_machine(&evidence_with_target(
                target,
                projection::ProjectionTargetEvidence::default(),
                true,
            ));
            let final_report = acc_final.finish();
            assert_eq!(
                final_report.targets[&target].status,
                ProjectionTargetStatus::Failed {
                    path: Some(PathBuf::from("claude/skills/phase3")),
                    reason: "network error".to_string(),
                }
            );
        }

        /// Row: a target with a recorded skip reason (and no failure) is
        /// `Skipped`, even when it also completed both phases (skip wins over
        /// completion). `ProjectionTargetEvidence::new` rejects a
        /// skip+failure combination by construction (its own exclusivity
        /// guard), so the "does failure win over skip" sub-question in the
        /// decision table is untestable as a live state — this row instead
        /// pins the one reachable skip case: skip alone, with completion.
        #[test]
        fn skip_wins_over_both_phase_completion() {
            let target = ProjectionTargetId::Copilot;
            let skipped_evidence = projection::ProjectionTargetEvidence::new(
                vec![written("copilot/managed")],
                Vec::new(),
                Some("user opted out".to_string()),
                None,
            )
            .unwrap();

            let mut acc = ProjectionRunAccumulator::new("normal", [target]);
            acc.record_canonical(&evidence_with_target(target, skipped_evidence, true));
            acc.record_machine(&evidence_with_target(
                target,
                projection::ProjectionTargetEvidence::default(),
                true,
            ));

            let report = acc.finish();
            let result = &report.targets[&target];
            assert_eq!(
                result.status,
                ProjectionTargetStatus::Skipped {
                    reason: "user opted out".to_string(),
                }
            );
        }

        /// Row: a target that completed only `Canonical` (not `Machine`)
        /// stays `NotRun`.
        #[test]
        fn canonical_only_completion_is_not_run() {
            let target = ProjectionTargetId::Codex;
            let mut acc = ProjectionRunAccumulator::new("normal", [target]);
            acc.record_canonical(&evidence_with_target(
                target,
                projection::ProjectionTargetEvidence::new(
                    vec![written("codex/managed")],
                    Vec::new(),
                    None,
                    None,
                )
                .unwrap(),
                true,
            ));

            let report = acc.finish();
            assert_eq!(
                report.targets[&target].status,
                ProjectionTargetStatus::NotRun
            );
        }

        /// Row: a target that completed only `Machine` (not `Canonical`)
        /// stays `NotRun`.
        #[test]
        fn machine_only_completion_is_not_run() {
            let target = ProjectionTargetId::Codex;
            let mut acc = ProjectionRunAccumulator::new("normal", [target]);
            acc.record_machine(&evidence_with_target(
                target,
                projection::ProjectionTargetEvidence::new(
                    vec![written("codex/managed")],
                    Vec::new(),
                    None,
                    None,
                )
                .unwrap(),
                true,
            ));

            let report = acc.finish();
            assert_eq!(
                report.targets[&target].status,
                ProjectionTargetStatus::NotRun
            );
        }

        /// Row: a target that completed both phases with recorded write
        /// evidence is `Applied`.
        #[test]
        fn both_phase_completion_with_write_evidence_is_applied() {
            let target = ProjectionTargetId::Claude;
            let mut acc = ProjectionRunAccumulator::new("normal", [target]);
            acc.record_canonical(&evidence_with_target(
                target,
                projection::ProjectionTargetEvidence::new(
                    vec![written("claude/skills/ccync")],
                    Vec::new(),
                    None,
                    None,
                )
                .unwrap(),
                true,
            ));
            acc.record_machine(&evidence_with_target(
                target,
                projection::ProjectionTargetEvidence::default(),
                true,
            ));

            let report = acc.finish();
            assert_eq!(
                report.targets[&target].status,
                ProjectionTargetStatus::Applied
            );
        }

        /// Row: a target that completed both phases with NO write evidence
        /// (empty paths, no failure, no skip) is `Unchanged`.
        #[test]
        fn both_phase_completion_with_no_write_evidence_is_unchanged() {
            let target = ProjectionTargetId::Claude;
            let mut acc = ProjectionRunAccumulator::new("normal", [target]);
            acc.record_canonical(&evidence_with_target(
                target,
                projection::ProjectionTargetEvidence::default(),
                true,
            ));
            acc.record_machine(&evidence_with_target(
                target,
                projection::ProjectionTargetEvidence::default(),
                true,
            ));

            let report = acc.finish();
            assert_eq!(
                report.targets[&target].status,
                ProjectionTargetStatus::Unchanged
            );
        }

        /// Row: a target registered as selected but never appearing in any
        /// evidence at all is still `NotRun`, not absent from the report.
        #[test]
        fn selected_target_with_zero_evidence_is_not_run_not_absent() {
            let target = ProjectionTargetId::AgyGui;
            let acc = ProjectionRunAccumulator::new("normal", [target]);

            let report = acc.finish();
            assert!(
                report.targets.contains_key(&target),
                "a selected target with zero evidence must still appear in the report"
            );
            assert_eq!(
                report.targets[&target].status,
                ProjectionTargetStatus::NotRun
            );
        }

        /// `common`/`run_warnings` merge across a Canonical call and a
        /// Machine call — both contribute, nothing is lost or overwritten.
        #[test]
        fn common_and_run_warnings_merge_across_both_phases() {
            let mut acc = ProjectionRunAccumulator::new("normal", []);

            let mut canonical_evidence = projection::ProjectionEvidence::default();
            canonical_evidence.common.push(written("common/canonical"));
            canonical_evidence
                .run_warnings
                .push("canonical warning".to_string());
            acc.record_canonical(&canonical_evidence);

            let mut machine_evidence = projection::ProjectionEvidence::default();
            machine_evidence.common.push(written("common/machine"));
            machine_evidence
                .run_warnings
                .push("machine warning".to_string());
            acc.record_machine(&machine_evidence);

            let report = acc.finish();
            assert_eq!(
                report.common,
                vec![written("common/canonical"), written("common/machine")]
            );
            assert_eq!(
                report.run_warnings,
                vec![
                    "canonical warning".to_string(),
                    "machine warning".to_string()
                ]
            );
        }

        /// `mode` carries through from the constructor to the final report
        /// unchanged.
        #[test]
        fn mode_carries_through_to_report() {
            let acc = ProjectionRunAccumulator::new("normal", []);
            let report = acc.finish();
            assert_eq!(report.mode, "normal");
        }

        fn removed(path: &str) -> projection::ProjectionPathResult {
            projection::ProjectionPathResult {
                path: PathBuf::from(path),
                change: projection::ProjectionPathChange::Removed,
            }
        }

        /// Net-effect regression: a target whose only recorded path is
        /// `[Written, Removed]` (the exact shape `apply_agy_evidence`'s
        /// rollback fix produces for a rolled-back `AgyCli`) must derive
        /// `Unchanged`, not `Applied`, once both phases complete — the write
        /// was undone within the same run, so it leaves no lasting effect.
        /// Uses a non-AGY target (`Claude`) so the AGY single-phase gate
        /// below is not itself exercised here; both phases are recorded
        /// explicitly to isolate the net-effect fix in isolation.
        #[test]
        fn written_then_removed_same_path_derives_unchanged_not_applied() {
            let target = ProjectionTargetId::Claude;
            let mut acc = ProjectionRunAccumulator::new("normal", [target]);
            acc.record_canonical(&evidence_with_target(
                target,
                projection::ProjectionTargetEvidence::new(
                    vec![
                        written("claude/skills/ccync"),
                        removed("claude/skills/ccync"),
                    ],
                    Vec::new(),
                    None,
                    None,
                )
                .unwrap(),
                true,
            ));
            acc.record_machine(&evidence_with_target(
                target,
                projection::ProjectionTargetEvidence::default(),
                true,
            ));

            let report = acc.finish();
            assert_eq!(
                report.targets[&target].status,
                ProjectionTargetStatus::Unchanged
            );
        }

        /// Net-effect regression: a target whose only recorded path is a
        /// standalone `Removed` (no preceding `Written` for that path) is a
        /// genuine removal of something that already existed — real change
        /// evidence, so it still derives `Applied` once both phases
        /// complete. Only a same-path `Written`-then-`Removed` pair cancels.
        #[test]
        fn standalone_removed_with_no_preceding_written_derives_applied() {
            let target = ProjectionTargetId::Claude;
            let mut acc = ProjectionRunAccumulator::new("normal", [target]);
            acc.record_canonical(&evidence_with_target(
                target,
                projection::ProjectionTargetEvidence::new(
                    vec![removed("claude/skills/stale-ccync")],
                    Vec::new(),
                    None,
                    None,
                )
                .unwrap(),
                true,
            ));
            acc.record_machine(&evidence_with_target(
                target,
                projection::ProjectionTargetEvidence::default(),
                true,
            ));

            let report = acc.finish();
            assert_eq!(
                report.targets[&target].status,
                ProjectionTargetStatus::Applied
            );
        }

        /// Net-effect regression (multi-path adversarial): a target with
        /// evidence on TWO distinct paths — one path's history is
        /// `[Written, Removed]` (cancels, no lasting effect) and a second,
        /// different path is a standalone `Written` (real, surviving
        /// change) — must derive `Applied`, not `Unchanged`. `has_net_change`
        /// groups per-path and asks whether ANY distinct path has surviving
        /// evidence; a bug that stopped at the first (cancelling) path or
        /// conflated cross-path history into one bucket would wrongly
        /// derive `Unchanged` here.
        #[test]
        fn one_cancelled_path_and_one_real_change_path_derives_applied() {
            let target = ProjectionTargetId::Claude;
            let mut acc = ProjectionRunAccumulator::new("normal", [target]);
            acc.record_canonical(&evidence_with_target(
                target,
                projection::ProjectionTargetEvidence::new(
                    vec![
                        written("claude/skills/ccync-a"),
                        removed("claude/skills/ccync-a"),
                        written("claude/skills/ccync-b"),
                    ],
                    Vec::new(),
                    None,
                    None,
                )
                .unwrap(),
                true,
            ));
            acc.record_machine(&evidence_with_target(
                target,
                projection::ProjectionTargetEvidence::default(),
                true,
            ));

            let report = acc.finish();
            assert_eq!(
                report.targets[&target].status,
                ProjectionTargetStatus::Applied,
                "a second, distinct path's real Written must count as net change even \
                 though the first path's Written was cancelled by its own Removed"
            );
        }

        /// Canonical-only-gate refinement: `AgyCli`/`AgyIde` are completed
        /// only by the `Canonical` phase's AGY junction apply, yet still
        /// derive `Applied` from write evidence recorded in that one phase
        /// alone. (`AgyGui` is deliberately NOT in this list: it is a
        /// machine-only target — see the machine-only tests below.)
        #[test]
        fn agy_junction_targets_derive_applied_from_canonical_phase_alone() {
            for target in [ProjectionTargetId::AgyCli, ProjectionTargetId::AgyIde] {
                let mut acc = ProjectionRunAccumulator::new("normal", [target]);
                acc.record_canonical(&evidence_with_target(
                    target,
                    projection::ProjectionTargetEvidence::new(
                        vec![written("agy/plugins/ccync")],
                        Vec::new(),
                        None,
                        None,
                    )
                    .unwrap(),
                    true,
                ));
                // Deliberately no `record_machine` call for this target.

                let report = acc.finish();
                assert_eq!(
                    report.targets[&target].status,
                    ProjectionTargetStatus::Applied,
                    "{target:?} must derive Applied from Canonical-only completion"
                );
            }
        }

        /// The single most consequential invariant in this chain, combined
        /// in one test: a rolled-back `AgyCli` (`[Written, Removed]` on the
        /// same path, still present in `completed_targets` per the AGY
        /// rollback-evidence fix) must derive `Unchanged`, never `Applied`,
        /// even though the AGY single-phase-gate refinement lets it reach
        /// the Applied/Unchanged branch from Canonical-phase completion
        /// alone. The two mechanisms are tested separately above; this pins
        /// them working correctly together, not just in isolation.
        #[test]
        fn agy_rollback_with_single_phase_gate_derives_unchanged_not_applied() {
            let target = ProjectionTargetId::AgyCli;
            let mut acc = ProjectionRunAccumulator::new("normal", [target]);
            acc.record_canonical(&evidence_with_target(
                target,
                projection::ProjectionTargetEvidence::new(
                    vec![
                        written("agy/plugins/ccync-cli"),
                        removed("agy/plugins/ccync-cli"),
                    ],
                    Vec::new(),
                    None,
                    None,
                )
                .unwrap(),
                true,
            ));
            // Deliberately no `record_machine` call — AgyCli must still
            // reach the Applied/Unchanged branch via the single-phase gate.

            let report = acc.finish();
            assert_eq!(
                report.targets[&target].status,
                ProjectionTargetStatus::Unchanged,
                "a rolled-back AgyCli reaching Applied/Unchanged via the \
                 single-phase gate must still derive Unchanged from the \
                 net-effect check, never Applied"
            );
        }

        /// Non-AGY targets keep requiring both phases even after the AGY
        /// single-phase-gate refinement: `Claude` completing only
        /// `Canonical` (never `Machine`) stays `NotRun`, exactly as the
        /// original both-phase gate already proved for `Codex` above —
        /// re-asserted explicitly here since this diff touches the same
        /// gate the AGY refinement lives in.
        #[test]
        fn non_agy_target_canonical_only_completion_stays_not_run() {
            let target = ProjectionTargetId::Claude;
            let mut acc = ProjectionRunAccumulator::new("normal", [target]);
            acc.record_canonical(&evidence_with_target(
                target,
                projection::ProjectionTargetEvidence::new(
                    vec![written("claude/skills/ccync")],
                    Vec::new(),
                    None,
                    None,
                )
                .unwrap(),
                true,
            ));
            // Deliberately no `record_machine` call.

            let report = acc.finish();
            assert_eq!(
                report.targets[&target].status,
                ProjectionTargetStatus::NotRun
            );
        }

        /// Machine-only-gate refinement: `Opencode`/`AgyGui` are
        /// completed only by the `Machine` phase (the `Canonical` phase's
        /// completed set never contains them), so `Machine`-phase completion
        /// with write evidence must derive `Applied` — not the permanent
        /// `NotRun` the old canonical-conjunct gate produced for them even on
        /// a fully clean run.
        #[test]
        fn machine_only_targets_derive_applied_from_machine_phase_alone() {
            for target in [ProjectionTargetId::Opencode, ProjectionTargetId::AgyGui] {
                let mut acc = ProjectionRunAccumulator::new("normal", [target]);
                acc.record_machine(&evidence_with_target(
                    target,
                    projection::ProjectionTargetEvidence::new(
                        vec![written("machine/skills/ccync")],
                        Vec::new(),
                        None,
                        None,
                    )
                    .unwrap(),
                    true,
                ));
                // Deliberately no `record_canonical` call — the Canonical
                // phase never completes these targets.

                let report = acc.finish();
                assert_eq!(
                    report.targets[&target].status,
                    ProjectionTargetStatus::Applied,
                    "{target:?} must derive Applied from Machine-only completion"
                );
            }
        }

        /// Machine-only-gate refinement, no-net-change side: a machine-only
        /// target that completed the `Machine` phase with zero recorded path
        /// evidence derives `Unchanged` (completed, nothing to do) — not
        /// `NotRun`.
        #[test]
        fn machine_only_targets_derive_unchanged_from_machine_completion_without_change() {
            for target in [ProjectionTargetId::Opencode, ProjectionTargetId::AgyGui] {
                let mut acc = ProjectionRunAccumulator::new("normal", [target]);
                acc.record_machine(&evidence_with_target(
                    target,
                    projection::ProjectionTargetEvidence::default(),
                    true,
                ));

                let report = acc.finish();
                assert_eq!(
                    report.targets[&target].status,
                    ProjectionTargetStatus::Unchanged,
                    "{target:?} must derive Unchanged from Machine-only completion \
                     with no recorded change evidence"
                );
            }
        }

        #[test]
        fn payload_shape_classifies_plugin_root_and_bare_skill() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cache_dir = tmp.path();

            // 1. Component-dir fixture -> PluginRoot
            let skills_dir = cache_dir.join("skills");
            fs::create_dir_all(&skills_dir).unwrap();
            assert_eq!(payload_shape(cache_dir), PayloadShape::PluginRoot);

            // 2. Both component-dir and root SKILL.md -> PluginRoot
            fs::write(cache_dir.join("SKILL.md"), "---\nname: my-skill\n---\n").unwrap();
            assert_eq!(payload_shape(cache_dir), PayloadShape::PluginRoot);

            // 3. Remove skills component dir -> BareSkill("my-skill")
            fs::remove_dir_all(&skills_dir).unwrap();
            assert_eq!(
                payload_shape(cache_dir),
                PayloadShape::BareSkill("my-skill".to_string())
            );

            // Double quotes
            fs::write(
                cache_dir.join("SKILL.md"),
                "---\nname: \"iso-24495\"\n---\n",
            )
            .unwrap();
            assert_eq!(
                payload_shape(cache_dir),
                PayloadShape::BareSkill("iso-24495".to_string())
            );

            // Single quotes
            fs::write(cache_dir.join("SKILL.md"), "---\nname: 'iso-24495'\n---\n").unwrap();
            assert_eq!(
                payload_shape(cache_dir),
                PayloadShape::BareSkill("iso-24495".to_string())
            );
        }

        #[test]
        fn component_count_matches_payload_shape() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cache_dir = tmp.path();

            // 1. Unrecognized -> 0
            assert_eq!(component_count(cache_dir), 0);

            // 2. BareSkill -> 1
            fs::write(
                cache_dir.join("SKILL.md"),
                "---\nname: my-bare-skill\n---\n",
            )
            .unwrap();
            assert_eq!(component_count(cache_dir), 1);

            // 3. PluginRoot -> sum of component directory entries
            let skills_dir = cache_dir.join("skills");
            let commands_dir = cache_dir.join("commands");
            fs::create_dir_all(&skills_dir).unwrap();
            fs::create_dir_all(&commands_dir).unwrap();
            fs::write(skills_dir.join("s1.md"), "skill 1").unwrap();
            fs::write(commands_dir.join("c1.md"), "cmd 1").unwrap();
            fs::write(commands_dir.join("c2.md"), "cmd 2").unwrap();

            assert_eq!(component_count(cache_dir), 3);
        }

        #[test]
        fn contributes_to_render_component_dirs() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cache_dir = tmp.path();
            let skills_dir = cache_dir.join("skills");
            fs::create_dir_all(&skills_dir).unwrap();
            fs::write(skills_dir.join("a"), "a").unwrap();
            fs::write(skills_dir.join("b"), "b").unwrap();

            assert!(contributes_to_render("test-plugin", cache_dir));
        }

        #[test]
        fn contributes_to_render_bare_skill() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cache_dir = tmp.path();
            fs::write(
                cache_dir.join("SKILL.md"),
                "---\nname: my-bare-skill\n---\n",
            )
            .unwrap();

            assert!(contributes_to_render("test-plugin", cache_dir));
        }

        #[test]
        fn contributes_to_render_mcp_server_declared() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cache_dir = tmp.path();
            fs::write(
                cache_dir.join(".mcp.json"),
                r#"{"mcpServers":{"zhtw":{"command":"zhtw-mcp"}}}"#,
            )
            .unwrap();

            assert!(contributes_to_render("test-plugin", cache_dir));
        }

        #[test]
        fn contributes_to_render_empty_component_dir_no_mcp() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cache_dir = tmp.path();
            fs::create_dir_all(cache_dir.join("skills")).unwrap();

            assert!(!contributes_to_render("test-plugin", cache_dir));
        }

        #[test]
        fn contributes_to_render_nothing_present() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cache_dir = tmp.path();

            assert!(!contributes_to_render("test-plugin", cache_dir));
        }

        #[test]
        fn contributes_to_render_mcp_servers_empty() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cache_dir = tmp.path();
            fs::write(cache_dir.join(".mcp.json"), r#"{"mcpServers":{}}"#).unwrap();

            assert!(!contributes_to_render("test-plugin", cache_dir));
        }

        #[test]
        fn contributes_to_render_mcp_file_unreadable() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cache_dir = tmp.path();
            fs::write(cache_dir.join(".mcp.json"), "{ not json").unwrap();

            assert!(contributes_to_render("test-plugin", cache_dir));
        }

        #[test]
        fn fallible_contribution_distinguishes_valid_empty_and_corrupt() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cache_dir = tmp.path();

            assert!(matches!(
                inspect_render_contribution("test-plugin", cache_dir),
                Ok(false)
            ));

            fs::create_dir(cache_dir.join("skills")).unwrap();
            fs::write(cache_dir.join("skills/component.md"), "component").unwrap();
            assert!(matches!(
                inspect_render_contribution("test-plugin", cache_dir),
                Ok(true)
            ));
            fs::remove_dir_all(cache_dir.join("skills")).unwrap();

            fs::write(
                cache_dir.join("mcp.json"),
                r#"{"mcpServers":{"server":{"command":"run"}}}"#,
            )
            .unwrap();
            assert!(matches!(
                inspect_render_contribution("test-plugin", cache_dir),
                Ok(true)
            ));

            fs::write(cache_dir.join("mcp.json"), r#"{"mcpServers":{}}"#).unwrap();
            assert!(matches!(
                inspect_render_contribution("test-plugin", cache_dir),
                Ok(false)
            ));

            fs::write(cache_dir.join("mcp.json"), "{ invalid json").unwrap();
            assert!(inspect_render_contribution("test-plugin", cache_dir).is_err());
            assert!(contributes_to_render("test-plugin", cache_dir));
        }

        #[test]
        fn fallible_contribution_reports_unreadable_manifest() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cache_dir = tmp.path();
            fs::create_dir(cache_dir.join("mcp.json")).unwrap();

            assert!(inspect_render_contribution("test-plugin", cache_dir).is_err());
            assert!(contributes_to_render("test-plugin", cache_dir));
        }

        #[test]
        fn payload_shape_unrecognized_cases() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cache_dir = tmp.path();

            // Empty dir -> Unrecognized
            assert_eq!(payload_shape(cache_dir), PayloadShape::Unrecognized);

            // SKILL.md without frontmatter opening --- -> Unrecognized
            fs::write(cache_dir.join("SKILL.md"), "name: test-skill\n").unwrap();
            assert_eq!(payload_shape(cache_dir), PayloadShape::Unrecognized);

            // SKILL.md with frontmatter opening --- but no closing --- or no name -> Unrecognized
            fs::write(cache_dir.join("SKILL.md"), "---\ndescription: test\n---\n").unwrap();
            assert_eq!(payload_shape(cache_dir), PayloadShape::Unrecognized);

            // Unclosed closing --- -> Unrecognized
            fs::write(cache_dir.join("SKILL.md"), "---\nname: test\n").unwrap();
            assert_eq!(payload_shape(cache_dir), PayloadShape::Unrecognized);
        }

        #[test]
        fn payload_shape_rejects_unsafe_names() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cache_dir = tmp.path();

            let unsafe_names = ["../../evil", "a/b", "a\\b", "", ".", "..", "...", "sub/"];

            for unsafe_name in unsafe_names {
                let content = format!("---\nname: {unsafe_name}\n---\n");
                fs::write(cache_dir.join("SKILL.md"), content).unwrap();
                assert_eq!(
                    payload_shape(cache_dir),
                    PayloadShape::Unrecognized,
                    "PayloadShape must be Unrecognized for unsafe name: {unsafe_name:?}"
                );
            }
        }

        /// A drive-prefixed value carries no separator character, so a denylist
        /// of separators lets it through — and `Path::join` then throws the
        /// canonical root away and returns the prefixed path verbatim, writing
        /// the payload wherever that value points. Windows-only, matching the
        /// platform gate on `is_contained_relative_rejects_traversal_and_absolute`.
        #[test]
        #[cfg(windows)]
        fn payload_shape_rejects_drive_prefixed_names() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cache_dir = tmp.path();

            for unsafe_name in ["C:evil", "C:", r"C:\Windows\System32\evil"] {
                let content = format!("---\nname: {unsafe_name}\n---\n");
                fs::write(cache_dir.join("SKILL.md"), content).unwrap();
                assert_eq!(
                    payload_shape(cache_dir),
                    PayloadShape::Unrecognized,
                    "PayloadShape must be Unrecognized for drive-prefixed name: {unsafe_name:?}"
                );

                // The escape this guards against is `join` discarding its base.
                let escaped = Path::new("canonical").join("skills").join(unsafe_name);
                assert!(
                    !escaped.starts_with("canonical"),
                    "fixture must actually escape, else the guard proves nothing: {escaped:?}"
                );
            }
        }

        #[test]
        fn read_plugin_mcp_servers_dot_mcp_only() {
            let tmp = tempfile::TempDir::new().unwrap();
            let plugin_dir = tmp.path();
            fs::write(
                plugin_dir.join(".mcp.json"),
                r#"{"mcpServers":{"srv1":{"command":"tool1"}}}"#,
            )
            .unwrap();

            let res = read_plugin_mcp_servers("my-plugin", plugin_dir).unwrap();
            let read = res.expect("must return Some");
            assert!(read.servers.contains_key("srv1"));
            assert!(read.warnings.is_empty());
        }

        #[test]
        fn read_plugin_mcp_servers_bare_mcp_only() {
            let tmp = tempfile::TempDir::new().unwrap();
            let plugin_dir = tmp.path();
            fs::write(
                plugin_dir.join("mcp.json"),
                r#"{"mcpServers":{"srv2":{"command":"tool2"}}}"#,
            )
            .unwrap();

            let res = read_plugin_mcp_servers("my-plugin", plugin_dir).unwrap();
            let read = res.expect("must return Some");
            assert!(read.servers.contains_key("srv2"));
            assert!(read.warnings.is_empty());
        }

        #[test]
        fn read_plugin_mcp_servers_both_present_dot_wins_with_warning() {
            let tmp = tempfile::TempDir::new().unwrap();
            let plugin_dir = tmp.path();
            fs::write(
                plugin_dir.join(".mcp.json"),
                r#"{"mcpServers":{"from_dot":{"command":"dot"}}}"#,
            )
            .unwrap();
            fs::write(
                plugin_dir.join("mcp.json"),
                r#"{"mcpServers":{"from_bare":{"command":"bare"}}}"#,
            )
            .unwrap();

            let res = read_plugin_mcp_servers("my-plugin", plugin_dir).unwrap();
            let read = res.expect("must return Some");
            assert!(read.servers.contains_key("from_dot"));
            assert!(!read.servers.contains_key("from_bare"));
            assert_eq!(read.warnings.len(), 1);
            assert!(
                read.warnings[0].contains("mcp.json"),
                "warning must name mcp.json: {}",
                read.warnings[0]
            );
        }

        #[test]
        fn read_plugin_mcp_servers_neither_present() {
            let tmp = tempfile::TempDir::new().unwrap();
            let plugin_dir = tmp.path();

            let res = read_plugin_mcp_servers("my-plugin", plugin_dir).unwrap();
            assert!(res.is_none());
        }

        #[test]
        fn read_plugin_mcp_servers_corrupt_fails_closed() {
            let tmp = tempfile::TempDir::new().unwrap();
            let plugin_dir = tmp.path();
            fs::write(plugin_dir.join("mcp.json"), "{ invalid json").unwrap();

            let res = read_plugin_mcp_servers("my-plugin", plugin_dir);
            assert!(res.is_err(), "must fail closed on corrupt mcp.json");
            let err = res.unwrap_err();
            let msg = format!("{err:?}");
            assert!(
                msg.contains("mcp.json"),
                "error message must name the path: {msg}"
            );
        }

        #[test]
        fn doctor_contributes_to_render_creates_no_dirs() {
            let tmp = tempfile::TempDir::new().unwrap();
            let plugin_dir = tmp.path().join("fake-plugin");
            fs::create_dir_all(&plugin_dir).unwrap();
            fs::write(
                plugin_dir.join("mcp.json"),
                r#"{"mcpServers":{"stdio_srv":{"command":"cmd","args":["echo"]}}}"#,
            )
            .unwrap();

            let contributes = contributes_to_render("fake-plugin", &plugin_dir);
            assert!(contributes);

            // Assert that calling contributes_to_render did not create any data dirs
            if let Some(data_root) = ccync_foundation::paths::ccync_data_root() {
                assert!(
                    !data_root.join("fake-plugin").exists(),
                    "doctor/contributes_to_render must not create <data root>/<id>"
                );
            }
        }

        #[test]
        fn read_plugin_mcp_servers_normalizes_streamable_http() {
            let tmp = tempfile::TempDir::new().unwrap();
            let plugin_dir = tmp.path();
            fs::write(
                plugin_dir.join(".mcp.json"),
                r#"{
                    "mcpServers": {
                        "streamable": {
                            "type": "streamable-http",
                            "url": "https://example.com/mcp"
                        },
                        "sse_srv": {
                            "type": "sse",
                            "url": "https://example.com/sse"
                        },
                        "stdio_srv": {
                            "type": "stdio",
                            "command": "cmd"
                        }
                    }
                }"#,
            )
            .unwrap();

            let res = read_plugin_mcp_servers("my-plugin", plugin_dir).unwrap();
            let read = res.expect("must return Some");
            assert_eq!(
                read.servers.get("streamable").and_then(|s| s.get("type")),
                Some(&Value::String("http".to_string())),
                "streamable-http must be normalized to http"
            );
            assert_eq!(
                read.servers.get("sse_srv").and_then(|s| s.get("type")),
                Some(&Value::String("sse".to_string())),
                "sse type must remain untouched"
            );
            assert_eq!(
                read.servers.get("stdio_srv").and_then(|s| s.get("type")),
                Some(&Value::String("stdio".to_string())),
                "stdio type must remain untouched"
            );
        }

        #[test]
        fn plugin_stdio_expansion_dot_slash_command() {
            let tmp = tempfile::TempDir::new().unwrap();
            let plugin_dir = tmp.path();
            fs::write(
                plugin_dir.join(".mcp.json"),
                r#"{
                    "mcpServers": {
                        "stdio_srv": {
                            "command": "./bin/tool"
                        }
                    }
                }"#,
            )
            .unwrap();

            let res = read_plugin_mcp_servers("my-plugin", plugin_dir).unwrap();
            let read = res.expect("must return Some");
            let srv = read.servers.get("stdio_srv").expect("must have stdio_srv");
            let expected = plugin_dir.join("bin/tool").display().to_string();
            assert_eq!(
                srv.get("command").and_then(Value::as_str),
                Some(expected.as_str()),
                "./bin/tool must become <dir>/bin/tool"
            );
        }

        #[test]
        fn plugin_stdio_expansion_bare_command_untouched() {
            let tmp = tempfile::TempDir::new().unwrap();
            let plugin_dir = tmp.path();
            fs::write(
                plugin_dir.join(".mcp.json"),
                r#"{
                    "mcpServers": {
                        "stdio_srv": {
                            "command": "node"
                        }
                    }
                }"#,
            )
            .unwrap();

            let res = read_plugin_mcp_servers("my-plugin", plugin_dir).unwrap();
            let read = res.expect("must return Some");
            let srv = read.servers.get("stdio_srv").expect("must have stdio_srv");
            assert_eq!(
                srv.get("command").and_then(Value::as_str),
                Some("node"),
                "bare command must remain untouched"
            );
        }

        #[test]
        fn plugin_stdio_expansion_placeholders_in_args_env_and_cwd() {
            let _env = env_guard();
            let tmp = tempfile::TempDir::new().unwrap();
            let plugin_dir = tmp.path();
            fs::write(
                plugin_dir.join(".mcp.json"),
                r#"{
                    "mcpServers": {
                        "stdio_srv": {
                            "command": "tool",
                            "args": ["--root=${PLUGIN_ROOT}", "--data=${PLUGIN_DATA}", "${UNKNOWN}"],
                            "env": {
                                "ROOT": "${PLUGIN_ROOT}",
                                "DATA": "${PLUGIN_DATA}",
                                "OTHER": "${UNKNOWN}"
                            },
                            "cwd": "${PLUGIN_ROOT}/subdir"
                        }
                    }
                }"#,
            )
            .unwrap();

            let res = read_plugin_mcp_servers("my-plugin", plugin_dir).unwrap();
            let read = res.expect("must return Some");
            let srv = read.servers.get("stdio_srv").expect("must have stdio_srv");

            let plugin_root = plugin_dir.display().to_string();
            let data_root =
                ccync_foundation::paths::ccync_data_root().expect("data root available");
            let plugin_data = data_root.join("my-plugin").display().to_string();

            // args: each placeholder replaced exactly once, ${UNKNOWN} kept literal
            let args = srv
                .get("args")
                .and_then(Value::as_array)
                .expect("args array");
            assert_eq!(args[0], Value::String(format!("--root={plugin_root}")));
            assert_eq!(args[1], Value::String(format!("--data={plugin_data}")));
            assert_eq!(args[2], Value::String("${UNKNOWN}".to_string()));

            // env: values replaced, ${UNKNOWN} kept literal
            let env = srv
                .get("env")
                .and_then(Value::as_object)
                .expect("env object");
            assert_eq!(env.get("ROOT"), Some(&Value::String(plugin_root.clone())));
            assert_eq!(env.get("DATA"), Some(&Value::String(plugin_data)));
            assert_eq!(
                env.get("OTHER"),
                Some(&Value::String("${UNKNOWN}".to_string()))
            );

            // cwd: replaced
            assert_eq!(
                srv.get("cwd").and_then(Value::as_str),
                Some(format!("{plugin_root}/subdir").as_str())
            );
        }

        #[test]
        fn plugin_stdio_expansion_replacement_text_containing_plugin_data_not_reexpanded() {
            let _env = env_guard();
            let tmp = tempfile::TempDir::new().unwrap();
            let plugin_dir = tmp.path().join("path_with_${PLUGIN_DATA}_in_name");
            fs::create_dir_all(&plugin_dir).unwrap();
            fs::write(
                plugin_dir.join(".mcp.json"),
                r#"{
                    "mcpServers": {
                        "stdio_srv": {
                            "command": "tool",
                            "args": ["${PLUGIN_ROOT}"],
                            "cwd": "${PLUGIN_ROOT}"
                        }
                    }
                }"#,
            )
            .unwrap();

            let res = read_plugin_mcp_servers("my-plugin", &plugin_dir).unwrap();
            let read = res.expect("must return Some");
            let srv = read.servers.get("stdio_srv").expect("must have stdio_srv");

            let plugin_root = plugin_dir.display().to_string();
            assert_eq!(
                srv.get("args")
                    .and_then(|a| a.get(0))
                    .and_then(Value::as_str),
                Some(plugin_root.as_str()),
                "replacement text containing ${{PLUGIN_DATA}} must not be re-expanded"
            );
            assert_eq!(
                srv.get("cwd").and_then(Value::as_str),
                Some(plugin_root.as_str()),
                "cwd replacement text containing ${{PLUGIN_DATA}} must not be re-expanded"
            );
        }

        #[test]
        fn plugin_stdio_expansion_url_and_headers_untouched() {
            let tmp = tempfile::TempDir::new().unwrap();
            let plugin_dir = tmp.path();
            fs::write(
                plugin_dir.join(".mcp.json"),
                r#"{
                    "mcpServers": {
                        "remote_srv": {
                            "type": "http",
                            "url": "https://example.com/${PLUGIN_ROOT}",
                            "headers": {
                                "Authorization": "Bearer ${PLUGIN_DATA}"
                            }
                        }
                    }
                }"#,
            )
            .unwrap();

            let res = read_plugin_mcp_servers("my-plugin", plugin_dir).unwrap();
            let read = res.expect("must return Some");
            let srv = read
                .servers
                .get("remote_srv")
                .expect("must have remote_srv");

            assert_eq!(
                srv.get("url").and_then(Value::as_str),
                Some("https://example.com/${PLUGIN_ROOT}"),
                "remote url must remain untouched"
            );
            assert_eq!(
                srv.get("headers")
                    .and_then(|h| h.get("Authorization"))
                    .and_then(Value::as_str),
                Some("Bearer ${PLUGIN_DATA}"),
                "remote headers must remain untouched"
            );
        }

        #[test]
        fn plugin_stdio_expansion_command_expanded_after_dot_slash_rewrite() {
            let tmp = tempfile::TempDir::new().unwrap();
            let plugin_dir = tmp.path();
            fs::write(
                plugin_dir.join(".mcp.json"),
                r#"{
                    "mcpServers": {
                        "cmd_srv": {
                            "command": "${PLUGIN_ROOT}/bin/tool"
                        }
                    }
                }"#,
            )
            .unwrap();

            let res = read_plugin_mcp_servers("my-plugin", plugin_dir).unwrap();
            let read = res.expect("must return Some");
            let srv = read.servers.get("cmd_srv").expect("must have cmd_srv");

            let expected_command = format!("{}/bin/tool", plugin_dir.display());
            assert_eq!(
                srv.get("command").and_then(Value::as_str),
                Some(expected_command.as_str()),
                "command placeholder must expand after the ./ rewrite"
            );
        }

        #[test]
        fn plugin_stdio_expansion_skips_placeholders_when_ccync_data_root_is_none() {
            let _env = env_guard();
            let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
            let prior_var = std::env::var_os(var);

            std::env::remove_var(var);

            let tmp = tempfile::TempDir::new().unwrap();
            let plugin_dir = tmp.path();
            fs::write(
                plugin_dir.join(".mcp.json"),
                r#"{
                    "mcpServers": {
                        "stdio_srv": {
                            "command": "./bin/tool",
                            "args": ["${PLUGIN_ROOT}"]
                        }
                    }
                }"#,
            )
            .unwrap();

            let res = read_plugin_mcp_servers("my-plugin", plugin_dir);

            // Restore environment immediately
            if let Some(v) = prior_var {
                std::env::set_var(var, v);
            }

            let read = res.unwrap().expect("must return Some");
            let srv = read.servers.get("stdio_srv").expect("must have stdio_srv");
            let expected_cmd = plugin_dir.join("bin/tool").display().to_string();
            assert_eq!(
                srv.get("command").and_then(Value::as_str),
                Some(expected_cmd.as_str()),
                "./ rewrite still works when data root is None"
            );
            assert_eq!(
                srv.get("args")
                    .and_then(|a| a.get(0))
                    .and_then(Value::as_str),
                Some("${PLUGIN_ROOT}"),
                "placeholder expansion skipped when ccync_data_root() is None"
            );
        }

        #[test]
        fn plugin_stdio_env_injection() {
            let _env = env_guard();
            let tmp = tempfile::TempDir::new().unwrap();
            let plugin_dir = tmp.path();
            fs::write(
                plugin_dir.join(".mcp.json"),
                r#"{
                    "mcpServers": {
                        "stdio_srv": {
                            "command": "tool",
                            "args": ["run"]
                        }
                    }
                }"#,
            )
            .unwrap();

            let data_root =
                ccync_foundation::paths::ccync_data_root().expect("data root available");
            let expected_data_dir = data_root.join("test-plugin-id");

            // Ensure clean state before calling helper
            if expected_data_dir.exists() {
                let _ = fs::remove_dir_all(&expected_data_dir);
            }

            let res = read_plugin_mcp_servers("test-plugin-id", plugin_dir).unwrap();
            let read = res.expect("must return Some");
            let srv = read.servers.get("stdio_srv").expect("must have stdio_srv");

            let env = srv
                .get("env")
                .and_then(Value::as_object)
                .expect("env object");
            assert_eq!(
                env.get("PLUGIN_ROOT").and_then(Value::as_str),
                Some(plugin_dir.display().to_string().as_str()),
                "env.PLUGIN_ROOT must equal plugin dir"
            );
            assert_eq!(
                env.get("PLUGIN_DATA").and_then(Value::as_str),
                Some(expected_data_dir.display().to_string().as_str()),
                "env.PLUGIN_DATA must equal <data root>/<id>"
            );

            assert!(
                read.data_dirs.contains(&expected_data_dir),
                "data_dirs must contain <data root>/<id>"
            );
            assert!(
                !expected_data_dir.exists(),
                "calling read_plugin_mcp_servers alone must not create the data directory"
            );
        }

        #[test]
        fn plugin_stdio_env_reserved_root_or_data_skipped() {
            let _env = env_guard();
            let tmp = tempfile::TempDir::new().unwrap();
            let plugin_dir = tmp.path();
            fs::write(
                plugin_dir.join(".mcp.json"),
                r#"{
                    "mcpServers": {
                        "bad_root": {
                            "command": "tool",
                            "env": { "PLUGIN_ROOT": "/custom/root" }
                        },
                        "bad_data": {
                            "command": "tool",
                            "env": { "PLUGIN_DATA": "/custom/data" }
                        },
                        "valid_srv": {
                            "command": "tool",
                            "env": { "CUSTOM_VAR": "val" }
                        }
                    }
                }"#,
            )
            .unwrap();

            let res = read_plugin_mcp_servers("my-plugin", plugin_dir).unwrap();
            let read = res.expect("must return Some");

            assert!(
                !read.servers.contains_key("bad_root"),
                "server defining PLUGIN_ROOT must be skipped"
            );
            assert!(
                !read.servers.contains_key("bad_data"),
                "server defining PLUGIN_DATA must be skipped"
            );
            assert!(
                read.servers.contains_key("valid_srv"),
                "valid server must be kept"
            );

            assert!(
                read.warnings.contains(&"server 'bad_root' from plugin my-plugin: env must not set PLUGIN_ROOT or PLUGIN_DATA (Agent Plugins §9.2); skipped".to_string()),
                "must emit warning for bad_root: {:?}",
                read.warnings
            );
            assert!(
                read.warnings.contains(&"server 'bad_data' from plugin my-plugin: env must not set PLUGIN_ROOT or PLUGIN_DATA (Agent Plugins §9.2); skipped".to_string()),
                "must emit warning for bad_data: {:?}",
                read.warnings
            );
        }

        #[test]
        fn plugin_stdio_env_remote_untouched() {
            let _env = env_guard();
            let tmp = tempfile::TempDir::new().unwrap();
            let plugin_dir = tmp.path();
            fs::write(
                plugin_dir.join(".mcp.json"),
                r#"{
                    "mcpServers": {
                        "remote_srv": {
                            "type": "http",
                            "url": "https://example.com/mcp",
                            "env": {
                                "MY_ENV": "123"
                            }
                        }
                    }
                }"#,
            )
            .unwrap();

            let res = read_plugin_mcp_servers("remote-plugin", plugin_dir).unwrap();
            let read = res.expect("must return Some");
            let srv = read
                .servers
                .get("remote_srv")
                .expect("must have remote_srv");

            let env = srv
                .get("env")
                .and_then(Value::as_object)
                .expect("env object");
            assert_eq!(
                env.get("PLUGIN_ROOT"),
                None,
                "remote server must not have PLUGIN_ROOT injected"
            );
            assert_eq!(
                env.get("PLUGIN_DATA"),
                None,
                "remote server must not have PLUGIN_DATA injected"
            );
            assert_eq!(
                env.get("MY_ENV").and_then(Value::as_str),
                Some("123"),
                "original env key must be preserved"
            );
            assert!(
                read.data_dirs.is_empty(),
                "remote server must add nothing to data_dirs"
            );
        }

        #[test]
        fn plugin_stdio_env_injection_diff() {
            let _env = env_guard();
            let tmp = tempfile::TempDir::new().unwrap();
            let plugin_dir = tmp.path();
            fs::write(
                plugin_dir.join(".mcp.json"),
                r#"{
                    "mcpServers": {
                        "stdio_srv": {
                            "command": "node",
                            "args": ["script.js"],
                            "cwd": "app",
                            "env": {
                                "EXISTING_KEY": "existing_val"
                            },
                            "customField": 42
                        }
                    }
                }"#,
            )
            .unwrap();

            let data_root =
                ccync_foundation::paths::ccync_data_root().expect("data root available");
            let expected_data_dir = data_root.join("diff-plugin");

            let res = read_plugin_mcp_servers("diff-plugin", plugin_dir).unwrap();
            let read = res.expect("must return Some");
            let srv = read.servers.get("stdio_srv").expect("must have stdio_srv");
            let srv_obj = srv.as_object().expect("server is object");

            // Check top-level keys: only the original keys exist, no third new top-level key added
            let mut top_keys: Vec<&String> = srv_obj.keys().collect();
            top_keys.sort();
            assert_eq!(
                top_keys,
                vec!["args", "command", "customField", "cwd", "env"],
                "no unexpected top-level fields added"
            );

            // Other fields byte-identical / value-identical
            assert_eq!(
                srv_obj.get("command"),
                Some(&Value::String("node".to_string()))
            );
            assert_eq!(
                srv_obj.get("args"),
                Some(&Value::Array(vec![Value::String("script.js".to_string())]))
            );
            assert_eq!(srv_obj.get("cwd"), Some(&Value::String("app".to_string())));
            assert_eq!(srv_obj.get("customField"), Some(&serde_json::json!(42)));

            // env object: exactly two added keys (PLUGIN_ROOT and PLUGIN_DATA), no third new key
            let env_obj = srv_obj
                .get("env")
                .and_then(Value::as_object)
                .expect("env object");
            let mut env_keys: Vec<&String> = env_obj.keys().collect();
            env_keys.sort();
            assert_eq!(
                env_keys,
                vec!["EXISTING_KEY", "PLUGIN_DATA", "PLUGIN_ROOT"],
                "env must have exactly two added keys and no third new key"
            );

            assert_eq!(
                env_obj.get("EXISTING_KEY"),
                Some(&Value::String("existing_val".to_string())),
                "original env key must be byte-identical"
            );
            assert_eq!(
                env_obj.get("PLUGIN_ROOT"),
                Some(&Value::String(plugin_dir.display().to_string())),
                "PLUGIN_ROOT injected correctly"
            );
            assert_eq!(
                env_obj.get("PLUGIN_DATA"),
                Some(&Value::String(expected_data_dir.display().to_string())),
                "PLUGIN_DATA injected correctly"
            );
        }

        #[test]
        fn render_canonical_root_creates_plugin_data_dirs() {
            let _env = env_guard();
            let tmp = tempfile::TempDir::new().unwrap();
            let canonical = tmp.path().join("canonical");
            let plugin_dir = tmp.path().join("my-plugin");
            fs::create_dir_all(&canonical).unwrap();
            fs::create_dir_all(&plugin_dir).unwrap();

            fs::write(
                plugin_dir.join(".mcp.json"),
                r#"{
                    "mcpServers": {
                        "stdio_srv": {
                            "command": "cmd",
                            "args": ["/c", "dir"]
                        }
                    }
                }"#,
            )
            .unwrap();

            let data_root =
                ccync_foundation::paths::ccync_data_root().expect("data root available");
            let test_plugin_id = "test-render-data-plugin";
            let expected_data_dir = data_root.join(test_plugin_id);
            if expected_data_dir.exists() {
                let _ = fs::remove_dir_all(&expected_data_dir);
            }

            render_canonical_root(&canonical, &[(test_plugin_id.to_string(), plugin_dir)]).unwrap();

            assert!(
                expected_data_dir.exists(),
                "render_canonical_root must create data_dirs"
            );

            // Clean up
            let _ = fs::remove_dir_all(&expected_data_dir);
        }
    }
}

#[cfg(test)]
mod p7_t12_recovery_tests {
    use super::*;
    use crate::plugin_native::{
        committed_view_path, observe_view_digest, NativeCommandRunner, NativeExecutable,
        NativePathEvidence, NativeRegistration,
    };
    use crate::tool_manager::lock::LifecycleLock;
    use ccync_foundation::provider::{
        CapabilityInventory, ClientScope, GenerationId, NativeSelector, PackageDigest, PathRoot,
        PreimageRef, ProviderGeneration, ProviderSourceKey, RelativePath, ScopedPath,
    };
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::path::Path;

    /// View files of the owned fixture, sorted by relative path.
    const VIEW_ENTRIES: [(&str, &str); 2] = [
        ("plugin.json", "{\"name\":\"demo\"}"),
        ("skills/demo/SKILL.md", "# demo\n"),
    ];

    /// Fake native manager holding one registration per selector. A removal
    /// drops the entry, so later observations reflect it.
    struct FakeNative {
        registrations: RefCell<BTreeMap<String, NativeRegistration>>,
        calls: RefCell<Vec<String>>,
        fail_remove: bool,
        /// Answers the next observations in order, before the stored state.
        scripted_observations: RefCell<std::collections::VecDeque<NativeRegistration>>,
    }

    impl FakeNative {
        fn with(selectors: &[&str]) -> Self {
            let registrations = selectors
                .iter()
                .map(|selector| ((*selector).to_string(), registration(selector)))
                .collect();
            Self {
                registrations: RefCell::new(registrations),
                calls: RefCell::new(Vec::new()),
                fail_remove: false,
                scripted_observations: RefCell::new(std::collections::VecDeque::new()),
            }
        }
    }

    impl NativeCommandRunner for FakeNative {
        fn mutation_verified(&self, _executable: NativeExecutable) -> bool {
            true
        }

        fn run(&self, _executable: NativeExecutable, args: &[&str]) -> Result<Vec<u8>, String> {
            self.calls.borrow_mut().push(args.join(" "));
            match args {
                ["plugin", "remove", selector] | ["plugin", "uninstall", selector, ..] => {
                    if self.fail_remove {
                        return Err("remove refused by the native manager".to_string());
                    }
                    self.registrations.borrow_mut().remove(*selector);
                    Ok(Vec::new())
                }
                _ => Err(format!("unexpected invocation {args:?}")),
            }
        }

        fn observe(
            &self,
            _executable: NativeExecutable,
            selector: &NativeSelector,
        ) -> Result<NativeRegistration, String> {
            if let Some(next) = self.scripted_observations.borrow_mut().pop_front() {
                return Ok(next);
            }
            Ok(self
                .registrations
                .borrow()
                .get(selector.as_str())
                .cloned()
                .unwrap_or_else(|| NativeRegistration {
                    available: true,
                    present: false,
                    enabled: false,
                    broken: false,
                    selector: None,
                    source: None,
                    upstream_revision: None,
                    package_digest: None,
                    capabilities: CapabilityInventory::new(),
                }))
        }

        fn marketplace_root(
            &self,
            _executable: NativeExecutable,
            _marketplace: &str,
        ) -> Result<Option<String>, String> {
            Ok(None)
        }
    }

    fn registration(selector: &str) -> NativeRegistration {
        NativeRegistration {
            available: true,
            present: true,
            enabled: true,
            broken: false,
            selector: Some(NativeSelector::new(selector).unwrap()),
            source: Some("demo".to_string()),
            upstream_revision: Some("rev1".to_string()),
            package_digest: None,
            capabilities: CapabilityInventory::new(),
        }
    }

    /// Isolated home with no inherited render or journal override.
    fn isolated_home() -> crate::test_support::TestHome {
        let home = crate::test_support::TestHome::new();
        unsafe { std::env::remove_var("CCYNC_HOME") };
        home
    }

    fn write_entries(root: &Path) {
        for (rel, bytes) in VIEW_ENTRIES {
            let file = root.join(rel);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, bytes).unwrap();
        }
    }

    /// Committed record whose view is published under its render root. The
    /// view digest is the one the observation hashes from the same files.
    fn owned_record(selector: &str, generation: &str) -> CommittedOwnership {
        let scratch = tempfile::TempDir::new().unwrap();
        write_entries(scratch.path());
        let NativePathEvidence::Digest(view_digest) = observe_view_digest(scratch.path()) else {
            panic!("fixture view must digest");
        };
        let record = CommittedOwnership {
            target: ProjectionTargetId::Codex,
            selector: NativeSelector::new(selector).unwrap(),
            plugin_name: "demo".to_string(),
            upstream_revision: Some("rev1".to_string()),
            generation: ProviderGeneration {
                source: ProviderSourceKey::new("demo").unwrap(),
                generation: GenerationId::new(generation).unwrap(),
                package_digest: PackageDigest::new("sha256:abc").unwrap(),
            },
            capabilities: CapabilityInventory::new(),
            client_scope: Some(ClientScope::new("demo-client").unwrap()),
            source_digest: Some(PackageDigest::new("sha256:src").unwrap()),
            view_digest: Some(view_digest),
            observed_preimage: None,
            enablement_intent: None,
            group_substep: None,
        };
        write_entries(&committed_view_path(&record).expect("render root resolves"));
        record
    }

    fn intent_for(record: &CommittedOwnership, phase: IntentPhase) -> PendingIntent {
        PendingIntent {
            target: record.target,
            ownership_fingerprints: vec![OwnershipFingerprint {
                selector: record.selector.clone(),
                package_digest: record.generation.package_digest.clone(),
            }],
            scoped_paths: Vec::new(),
            preimage_refs: Vec::new(),
            previous_generation: None,
            desired_generation: record.generation.clone(),
            phase,
            client_scope: record.client_scope.clone(),
            source_digest: record.source_digest.clone(),
            view_digest: record.view_digest.clone(),
            observed_preimage: None,
            enablement_intent: None,
            group_substep: None,
        }
    }

    /// Save a preimage under the build root, as delivery does before an intent.
    fn save_preimage(name: &str) -> PreimageRef {
        let build = provider_state_base().unwrap();
        let saved = format!("preimages/{name}");
        let file = build.join(&saved);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, b"prior content").unwrap();
        PreimageRef {
            path: ScopedPath {
                root: PathRoot::UserHome,
                path: RelativePath::new(".claude/skills/demo").unwrap(),
            },
            saved: RelativePath::new(&saved).unwrap(),
        }
    }

    fn write_lock(committed: Vec<CommittedOwnership>) {
        let build = provider_state_base().unwrap();
        fs::create_dir_all(&build).unwrap();
        let mut lock = json!({});
        ProviderLifecycleLock {
            schema_version: 1,
            committed,
        }
        .write_to_lock(&mut lock)
        .unwrap();
        fs::write(build.join("lock.json"), serde_json::to_vec(&lock).unwrap()).unwrap();
    }

    fn lock_committed() -> Vec<CommittedOwnership> {
        let build = provider_state_base().unwrap();
        let value: Value =
            serde_json::from_slice(&fs::read(build.join("lock.json")).unwrap()).unwrap();
        ProviderLifecycleLock::from_lock(&value).unwrap().committed
    }

    fn write_journal(intents: Vec<PendingIntent>) {
        let path = provider_journal_path().unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            ProviderLifecycleJournal {
                schema_version: 1,
                intents,
                ..Default::default()
            }
            .to_json_string()
            .unwrap(),
        )
        .unwrap();
    }

    fn journal_intents() -> Vec<PendingIntent> {
        let path = provider_journal_path().unwrap();
        match fs::read_to_string(path) {
            Ok(text) => {
                ProviderLifecycleJournal::from_json_str(&text)
                    .unwrap()
                    .intents
            }
            Err(_) => Vec::new(),
        }
    }

    #[test]
    fn p7_t12_failed_removal_keeps_registration_generation_and_journal() {
        let _home = isolated_home();
        let owned = owned_record("demo@ccync", "g-0001");
        write_lock(vec![owned.clone()]);
        let mut fake = FakeNative::with(&["demo@ccync"]);
        fake.fail_remove = true;
        let guard = LifecycleLock::acquire().unwrap();
        let mut warnings = Vec::new();

        let retained = remove_committed_providers_for_uninstall(
            &guard.held_lock_proof(),
            &mut warnings,
            &fake,
        );

        assert!(retained.contains("demo@ccync"));
        assert_eq!(lock_committed(), vec![owned.clone()], "ownership kept");
        assert_eq!(journal_intents().len(), 1, "removal intent kept for retry");
        assert!(committed_view_path(&owned).unwrap().is_dir(), "view kept");
        assert!(fake.registrations.borrow().contains_key("demo@ccync"));
        assert!(warnings.iter().any(|w| w.contains("retained for retry")));
    }

    #[test]
    fn p7_t12_retry_removes_only_the_owned_provider() {
        let _home = isolated_home();
        let owned = owned_record("demo@ccync", "g-0001");
        write_lock(vec![owned]);
        // A second provider that ccync never committed is outside the removal.
        let fake = FakeNative::with(&["demo@ccync", "foreign@user"]);
        let guard = LifecycleLock::acquire().unwrap();
        let mut warnings = Vec::new();

        let retained = remove_committed_providers_for_uninstall(
            &guard.held_lock_proof(),
            &mut warnings,
            &fake,
        );

        assert!(retained.is_empty(), "{retained:?}");
        assert_eq!(*fake.calls.borrow(), ["plugin remove demo@ccync"]);
        assert!(!fake.registrations.borrow().contains_key("demo@ccync"));
        assert!(
            fake.registrations.borrow().contains_key("foreign@user"),
            "a second provider stays registered"
        );
        assert!(lock_committed().is_empty(), "the owned record is gone");
        assert!(journal_intents().is_empty(), "removal intent cleared");
    }

    #[test]
    fn p7_t12_crash_after_native_removal_converges_on_retry() {
        let _home = isolated_home();
        let owned = owned_record("demo@ccync", "g-0001");
        write_lock(vec![owned]);
        // Crash point: the native removal finished, but ownership and the intent
        // were not cleared yet.
        write_journal(vec![intent_for(&lock_committed()[0], IntentPhase::Pruning)]);
        let fake = FakeNative::with(&[]);
        let guard = LifecycleLock::acquire().unwrap();
        let mut warnings = Vec::new();

        let retained = remove_committed_providers_for_uninstall(
            &guard.held_lock_proof(),
            &mut warnings,
            &fake,
        );

        assert!(retained.is_empty(), "{retained:?}");
        assert!(
            lock_committed().is_empty(),
            "ownership converges to removed"
        );
        assert!(journal_intents().is_empty(), "intent converges to cleared");
        assert!(fake.calls.borrow().is_empty(), "no repeated native call");
    }

    #[test]
    fn p7_t12_crash_after_ownership_commit_clears_intent_and_preimage() {
        let _home = isolated_home();
        let owned = owned_record("demo@ccync", "g-0001");
        write_lock(vec![owned.clone()]);
        let mut intent = intent_for(&owned, IntentPhase::Staged);
        let preimage = save_preimage("codex-demo/0");
        intent.preimage_refs = vec![preimage.clone()];
        write_journal(vec![intent]);
        let fake = FakeNative::with(&["demo@ccync"]);
        let guard = LifecycleLock::acquire().unwrap();
        let mut warnings = Vec::new();

        reconcile_native_intents(&guard.held_lock_proof(), &fake, &mut warnings);

        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(journal_intents().is_empty(), "proven intent cleared");
        let build = provider_state_base().unwrap();
        assert!(
            !build.join(preimage.saved.as_str()).exists(),
            "preimage removed"
        );
        assert_eq!(lock_committed(), vec![owned], "ownership untouched");
        assert!(
            fake.calls.borrow().is_empty(),
            "reconciliation only observes"
        );
    }

    #[test]
    fn p7_t12_foreign_drift_keeps_intent_preimage_and_blocks_removal() {
        let _home = isolated_home();
        let owned = owned_record("demo@ccync", "g-0001");
        write_lock(vec![owned.clone()]);
        let mut intent = intent_for(&owned, IntentPhase::Staged);
        let preimage = save_preimage("codex-demo/1");
        intent.preimage_refs = vec![preimage.clone()];
        write_journal(vec![intent]);
        // A foreign edit lands inside the owned view after the intent was recorded.
        let view = committed_view_path(&owned).unwrap();
        fs::write(view.join("foreign.txt"), b"not ours").unwrap();
        let fake = FakeNative::with(&["demo@ccync"]);
        let guard = LifecycleLock::acquire().unwrap();
        let mut warnings = Vec::new();

        reconcile_native_intents(&guard.held_lock_proof(), &fake, &mut warnings);
        assert_eq!(journal_intents().len(), 1, "ambiguous intent kept");
        let build = provider_state_base().unwrap();
        assert!(
            build.join(preimage.saved.as_str()).exists(),
            "preimage kept"
        );

        let retained = remove_committed_providers_for_uninstall(
            &guard.held_lock_proof(),
            &mut warnings,
            &fake,
        );
        assert!(
            retained.contains("demo@ccync"),
            "drifted registration retained"
        );
        assert!(
            fake.calls.borrow().is_empty(),
            "no removal on drifted proof"
        );
        assert_eq!(lock_committed(), vec![owned], "ownership retained");
        assert!(view.join("foreign.txt").exists(), "foreign file untouched");
    }

    #[test]
    fn p7_t12_superseded_intent_is_dropped_only_with_a_committed_newer_generation() {
        let _home = isolated_home();
        let newer = owned_record("demo@ccync", "g-0002");
        write_lock(vec![newer.clone()]);
        let mut older = intent_for(&newer, IntentPhase::Staged);
        older.desired_generation = ProviderGeneration {
            generation: GenerationId::new("g-0001").unwrap(),
            ..newer.generation.clone()
        };
        let older_preimage = save_preimage("codex-demo/2");
        older.preimage_refs = vec![older_preimage.clone()];
        let current = intent_for(&newer, IntentPhase::Staged);
        write_journal(vec![older.clone(), current]);
        let fake = FakeNative::with(&["demo@ccync"]);
        let guard = LifecycleLock::acquire().unwrap();
        let mut warnings = Vec::new();

        reconcile_native_intents(&guard.held_lock_proof(), &fake, &mut warnings);

        assert!(
            journal_intents().is_empty(),
            "superseded and completed intents cleared"
        );
        let build = provider_state_base().unwrap();
        assert!(!build.join(older_preimage.saved.as_str()).exists());

        // Without a committed newer generation, the older intent stays.
        write_journal(vec![older.clone()]);
        reconcile_native_intents(&guard.held_lock_proof(), &fake, &mut warnings);
        assert_eq!(journal_intents().len(), 1, "unproven older intent kept");
    }

    #[test]
    fn p7_t12_user_change_between_observation_and_mutation_blocks_removal() {
        let _home = isolated_home();
        let owned = owned_record("demo@ccync", "g-0001");
        write_lock(vec![owned.clone()]);
        let fake = FakeNative::with(&["demo@ccync"]);
        // The first observation matches the committed record. The user then
        // changes the registration to another revision before the mutation.
        let user_changed = NativeRegistration {
            upstream_revision: Some("user-rev".to_string()),
            ..registration("demo@ccync")
        };
        fake.scripted_observations
            .borrow_mut()
            .extend([registration("demo@ccync"), user_changed]);
        let guard = LifecycleLock::acquire().unwrap();
        let mut warnings = Vec::new();

        let retained = remove_committed_providers_for_uninstall(
            &guard.held_lock_proof(),
            &mut warnings,
            &fake,
        );

        assert!(retained.contains("demo@ccync"), "{retained:?}");
        assert!(
            fake.calls.borrow().is_empty(),
            "no removal after the change"
        );
        assert_eq!(lock_committed(), vec![owned.clone()], "ownership kept");
        assert_eq!(
            journal_intents().len(),
            1,
            "removal intent kept as evidence"
        );
        assert!(committed_view_path(&owned).unwrap().is_dir(), "view kept");
        assert!(fake.registrations.borrow().contains_key("demo@ccync"));
        assert!(warnings.iter().any(|w| w.contains("retained for retry")));
    }

    #[test]
    fn p7_t12_exit_zero_removal_still_listed_is_not_success() {
        let _home = isolated_home();
        let owned = owned_record("demo@ccync", "g-0001");
        write_lock(vec![owned.clone()]);
        let fake = FakeNative::with(&["demo@ccync"]);
        // The pre-check, the confirmation, and the fresh listing after the
        // removal all still show the registration, although the CLI exits 0.
        fake.scripted_observations.borrow_mut().extend([
            registration("demo@ccync"),
            registration("demo@ccync"),
            registration("demo@ccync"),
        ]);
        let guard = LifecycleLock::acquire().unwrap();
        let mut warnings = Vec::new();

        let retained = remove_committed_providers_for_uninstall(
            &guard.held_lock_proof(),
            &mut warnings,
            &fake,
        );

        assert!(retained.contains("demo@ccync"), "{retained:?}");
        assert_eq!(*fake.calls.borrow(), ["plugin remove demo@ccync"]);
        assert_eq!(lock_committed(), vec![owned], "ownership kept");
        assert_eq!(journal_intents().len(), 1, "removal intent kept for retry");
        assert!(warnings.iter().any(|w| w.contains("retained for retry")));
    }

    #[test]
    fn p7_t12_disabled_owned_registration_is_removed_with_fresh_proof() {
        let _home = isolated_home();
        let mut owned = owned_record("demo@ccync", "g-0001");
        owned.enablement_intent = Some(ccync_foundation::provider::EnablementIntent::Disabled);
        write_lock(vec![owned.clone()]);
        let fake = FakeNative::with(&["demo@ccync", "foreign@user"]);
        fake.registrations
            .borrow_mut()
            .get_mut("demo@ccync")
            .unwrap()
            .enabled = false;
        let guard = LifecycleLock::acquire().unwrap();
        let mut warnings = Vec::new();

        let retained = remove_committed_providers_for_uninstall(
            &guard.held_lock_proof(),
            &mut warnings,
            &fake,
        );

        assert!(retained.is_empty(), "{retained:?}");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(*fake.calls.borrow(), ["plugin remove demo@ccync"]);
        assert!(!fake.registrations.borrow().contains_key("demo@ccync"));
        assert!(
            fake.registrations.borrow().contains_key("foreign@user"),
            "only the owned registration is removed"
        );
        assert!(lock_committed().is_empty(), "ownership cleared");
        assert!(journal_intents().is_empty(), "removal intent cleared");
    }

    #[test]
    fn p7_t12_uninstall_retry_removes_owned_views_and_registration() {
        let _home = isolated_home();
        let owned = owned_record("demo@ccync", "g-0001");
        write_lock(vec![owned.clone()]);
        let view = committed_view_path(&owned).unwrap();
        assert!(view.is_dir(), "fixture view exists before the retry");
        let fake = FakeNative::with(&["demo@ccync", "foreign@user"]);

        run_uninstall_using(&fake).expect("uninstall retry succeeds");

        assert_eq!(*fake.calls.borrow(), ["plugin remove demo@ccync"]);
        assert!(!fake.registrations.borrow().contains_key("demo@ccync"));
        assert!(
            fake.registrations.borrow().contains_key("foreign@user"),
            "another provider's registration is left alone"
        );
        assert!(
            !view.exists(),
            "the owned view is removed with its registration"
        );
        let lock_path = provider_state_base().unwrap().join("lock.json");
        assert!(
            !lock_path.exists() || lock_committed().is_empty(),
            "the owned ownership record is gone"
        );
    }

    #[test]
    fn p7_t12_uninstall_keeps_preimages_named_by_a_retained_intent() {
        let _home = isolated_home();
        let owned = owned_record("demo@ccync", "g-0001");
        write_lock(vec![owned.clone()]);
        // A foreign edit inside the owned view blocks removal, so the
        // registration and its intent are retained for the retry.
        let view = committed_view_path(&owned).unwrap();
        fs::write(view.join("foreign.txt"), b"not ours").unwrap();
        let mut intent = intent_for(&owned, IntentPhase::Staged);
        let preimage = save_preimage("codex-demo/3");
        intent.preimage_refs = vec![preimage.clone()];
        write_journal(vec![intent]);
        let fake = FakeNative::with(&["demo@ccync"]);

        let _ = run_uninstall_using(&fake);

        assert!(
            fake.calls.borrow().is_empty(),
            "no removal on drifted proof"
        );
        let build = provider_state_base().unwrap();
        assert!(
            build.join(preimage.saved.as_str()).exists(),
            "the preimage a retained intent names survives uninstall"
        );
        let intents = journal_intents();
        assert_eq!(intents.len(), 1, "the retained intent is kept");
        validate_intent(&intents[0]).expect("the retained intent still validates");
    }
}
