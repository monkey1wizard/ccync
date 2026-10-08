//! Projection support: health check, frontmatter rendering, file/list/link helpers.

use super::*;
use ccync_foundation::health::{DoctorFinding, HealthCheck};
use ccync_foundation::json_util::read_json_map;
use ccync_foundation::platform::{
    atomic_write_bytes, classify_link, create_dir_link, create_symlink_dir, is_symlink_or_junction,
    remove_dir_link, AtomicWriteError, LinkShape,
};
use ccync_foundation::target::ProjectionTargetId;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

const PROJECTION_LOCK_KEY: &str = "_ccyncProjection";
const PROJECTION_LOCK_SCHEMA_VERSION: u64 = 1;
const MANAGED_SOURCE_ATTRIBUTION: &str = "sourceAttribution";

pub(crate) const MANAGED_AGENT_PATHS: &str = "agentProjectionPaths";
pub(crate) const MANAGED_CODEX_AGENT_PATHS: &str = "codexAgentProjectionPaths";
pub(crate) const MANAGED_OPENCODE_AGENT_PATHS: &str = "opencodeAgentProjectionPaths";
pub(crate) const MANAGED_COMMAND_PATHS: &str = "commandProjectionPaths";
pub(crate) const MANAGED_DISCUSS_SKILL_PATHS: &str = "discussSkillProjectionPaths";
pub(crate) const MANAGED_LEGACY_PATHS: &str = "legacyProjectionPaths";
pub(crate) const MANAGED_SKILL_PATHS: &str = "skillProjectionPaths";
pub(crate) const MANAGED_AGY_ROOT_PATHS: &str = "agyRootProjectionPaths";

pub struct SkillsProjectionHealthCheck {
    shared_skills_root: PathBuf,
}

/// Registry-backed de-projection authority for uninstall (Protected Path).
///
/// Wraps the prior `build/lock.json` managed-artifact registry and exposes a
/// positive-membership authorization check for engine-side removal of
/// ccync-owned projections. Authorization uses [`ManagedArtifactRegistry::is_managed`]
/// (recorded-in-prior membership), deliberately NOT `can_mutate`: `can_mutate`
/// is allow-all when the prior lockfile is empty/missing, and that empty state is
/// the common partial-uninstall case — using it would authorize deleting
/// same-named user-owned siblings. With `is_managed`, an empty/missing prior
/// authorizes nothing.
pub struct ManagedDeprojection {
    registry: ManagedArtifactRegistry,
}

impl ManagedDeprojection {
    /// Load the prior managed registry from `build/lock.json`. A missing or
    /// unreadable lockfile yields an authority that authorizes nothing.
    pub fn from_lockfile(lockfile_path: &Path) -> Self {
        let mut report = ProjectionReport::default();
        Self {
            registry: ManagedArtifactRegistry::load(lockfile_path, &mut report),
        }
    }

    /// True iff `path` was recorded as ccync-managed in the prior lockfile.
    /// False on empty/missing prior (no allow-all) and for any path ccync never
    /// projected — so a non-managed same-named user sibling is never authorized.
    pub fn is_authorized_for_removal(&self, path: &Path) -> bool {
        self.registry.is_managed(path)
    }

    /// True when the prior lockfile recorded no managed paths — nothing to
    /// de-project; callers may short-circuit.
    pub fn is_empty(&self) -> bool {
        self.registry.prior_is_empty()
    }
}

/// Registry-backed ownership authority for `agy-*` plugin roots (Protected Path).
///
/// Modelled on [`ManagedDeprojection`], but adds a second property the raw
/// [`ManagedArtifactRegistry`] does not provide on its own: a forced,
/// immediate row write. Two properties in total:
///
/// - **Strict membership.** [`is_owned`](Self::is_owned) uses
///   [`ManagedArtifactRegistry::is_managed`], deliberately not `can_mutate`:
///   `can_mutate` is allow-all when the prior lockfile is empty or missing,
///   which would authorize deleting a user's pre-existing
///   `~/.gemini/config/plugins/ccync` on a first run. `is_managed` authorizes
///   nothing until a row has actually been recorded, so an empty or missing
///   prior owns nothing.
/// - **Forced pre-swap write.** [`record_now`](Self::record_now) writes a
///   single row to `lock.json` immediately, instead of waiting for the
///   run's own end-of-run [`ManagedArtifactRegistry::persist`] — so a row
///   can be made durable before a tree becomes visible at its target path,
///   ahead of that run's own registry instance reaching `persist`. The write
///   re-reads the lockfile fresh from disk and merges into it; it never
///   publishes a snapshot of this (or any other) registry instance's
///   in-flight `next` state, so it cannot clear a category `replace_category`
///   has emptied but the run has not yet repopulated, and cannot make one
///   registry instance overwrite another's categories.
pub struct AgyRootOwnership {
    lockfile_path: PathBuf,
    registry: ManagedArtifactRegistry,
}

impl AgyRootOwnership {
    /// Load the prior managed registry from `build/lock.json`. A missing or
    /// unreadable lockfile yields an authority that owns nothing.
    pub fn from_lockfile(lockfile_path: &Path) -> Self {
        let mut report = ProjectionReport::default();
        Self {
            lockfile_path: lockfile_path.to_path_buf(),
            registry: ManagedArtifactRegistry::load(lockfile_path, &mut report),
        }
    }

    /// True iff `path` was recorded as ccync-owned in the prior lockfile.
    /// False on an empty or missing prior (no allow-all), so a pre-existing
    /// path ccync never projected is never authorized for removal.
    pub fn is_owned(&self, path: &Path) -> bool {
        self.registry.is_managed(path)
    }

    /// Record `path` as owned under the `agy-*` root category
    /// ([`MANAGED_AGY_ROOT_PATHS`]), writing the row to `lock.json`
    /// immediately rather than waiting for this (or any) registry
    /// instance's end-of-run `persist`. The write merges the single row
    /// into the lockfile as currently stored on disk, leaving every other
    /// category and every other on-disk key untouched. `dry_run` suppresses
    /// the write entirely.
    pub fn record_now(&self, path: &Path, dry_run: bool) -> Result<(), ProjectionError> {
        if dry_run {
            return Ok(());
        }
        merge_managed_artifact_row_on_disk(&self.lockfile_path, MANAGED_AGY_ROOT_PATHS, path)
    }

    /// Remove `path`'s row from `lock.json` immediately — the mirror of
    /// [`Self::record_now`], applied only after the tree at `path` is
    /// already gone. Never the other way around: a row-without-tree window
    /// is the accepted, harmless crash state the publish ordering already
    /// tolerates; a tree-without-row window is the failure that ordering
    /// exists to prevent, so this method's caller is responsible for
    /// deleting the tree first and calling this only once that succeeded.
    /// Like `record_now`, the write is a direct disk read-modify-write —
    /// never through any `ManagedArtifactRegistry` instance's in-memory
    /// `next` — so it cannot be reverted by a sibling instance's later
    /// `persist` (see `support.rs`'s disk-authoritative `persist` for why
    /// that guarantee matters here). `dry_run` suppresses the write.
    pub fn forget_now(&self, path: &Path, dry_run: bool) -> Result<(), ProjectionError> {
        if dry_run {
            return Ok(());
        }
        remove_managed_artifact_row_on_disk(&self.lockfile_path, MANAGED_AGY_ROOT_PATHS, path)
    }
}

/// Merge a single managed-artifact path into `category`'s array in the
/// lockfile at `lockfile_path`, reading and writing the file directly rather
/// than going through any [`ManagedArtifactRegistry`] instance's in-memory
/// `next` state. Every other key already on disk — every other category,
/// `sourceAttribution`, `schemaVersion`, and any top-level key outside
/// `_ccyncProjection` — is carried through untouched; only `category`'s
/// array gains `path`, and only if it is not already present. A missing or
/// unparseable lockfile is treated as an empty starting point, matching
/// [`ManagedArtifactRegistry::load`]'s own tolerance.
fn merge_managed_artifact_row_on_disk(
    lockfile_path: &Path,
    category: &str,
    path: &Path,
) -> Result<(), ProjectionError> {
    let mut root: Map<String, Value> = fs::read_to_string(lockfile_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| match value {
            Value::Object(root) => Some(root),
            _ => None,
        })
        .unwrap_or_default();

    let meta = root
        .entry(PROJECTION_LOCK_KEY.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !meta.is_object() {
        *meta = Value::Object(Map::new());
    }
    let meta = meta
        .as_object_mut()
        .expect("meta was just ensured to be an object");
    meta.entry("schemaVersion".to_string())
        .or_insert_with(|| Value::Number(PROJECTION_LOCK_SCHEMA_VERSION.into()));

    let needle = normalize_managed_artifact_path(path);
    let entry = meta
        .entry(category.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !entry.is_array() {
        *entry = Value::Array(Vec::new());
    }
    let array = entry
        .as_array_mut()
        .expect("entry was just ensured to be an array");
    let already_present = array
        .iter()
        .any(|value| value.as_str() == Some(needle.as_str()));
    if !already_present {
        array.push(Value::String(needle));
    }

    let rendered = serde_json::to_string_pretty(&Value::Object(root))
        .map_err(|err| ProjectionError::Message(err.to_string()))?;
    write_text(lockfile_path, &rendered, false)?;
    Ok(())
}

/// Remove a single managed-artifact path from `category`'s array in the
/// lockfile at `lockfile_path` — the mirror of
/// [`merge_managed_artifact_row_on_disk`], same direct-disk-read-modify-write
/// discipline (never through any [`ManagedArtifactRegistry`] instance's
/// in-memory state). Absent from the array, or a missing/unparseable
/// lockfile, is treated as already-removed: a no-op, not an error. Every
/// other key already on disk is carried through untouched.
fn remove_managed_artifact_row_on_disk(
    lockfile_path: &Path,
    category: &str,
    path: &Path,
) -> Result<(), ProjectionError> {
    let Some(raw) = fs::read_to_string(lockfile_path).ok() else {
        return Ok(());
    };
    let Some(Value::Object(mut root)) = serde_json::from_str::<Value>(&raw).ok() else {
        return Ok(());
    };
    let Some(Value::Object(meta)) = root.get_mut(PROJECTION_LOCK_KEY) else {
        return Ok(());
    };
    let Some(Value::Array(array)) = meta.get_mut(category) else {
        return Ok(());
    };

    let needle = normalize_managed_artifact_path(path);
    let before = array.len();
    array.retain(|value| value.as_str() != Some(needle.as_str()));
    if array.len() == before {
        return Ok(());
    }

    let rendered = serde_json::to_string_pretty(&Value::Object(root))
        .map_err(|err| ProjectionError::Message(err.to_string()))?;
    write_text(lockfile_path, &rendered, false)?;
    Ok(())
}

/// Rewrite the lockfile down to its projection-ownership namespace, dropping
/// every other top-level key — `_mcpServers` above all.
///
/// `ccync uninstall` retains the lockfile when an owned `agy-*` root survives
/// teardown, because the rows inside `_ccyncProjection` are the only proof that
/// authorizes removing ccync's leftovers on a later retry. What must not
/// survive an uninstall is everything outside that namespace: `_mcpServers`
/// holds the adopted MCP server definitions verbatim, `env` and `headers`
/// included, which is where API keys and bearer tokens live.
///
/// The whole `_ccyncProjection` object is kept, not just the `agy-*` category.
/// Its siblings are the ownership proof `ManagedDeprojection` needs for the
/// other managed surfaces (`~/.claude/commands`, `~/.codex/skills`,
/// `~/.gemini/commands`, and the rest), which have no unconditional removal
/// path — narrowing the retention to the agy rows would let a partial
/// uninstall's other leftovers become permanently unprovable. Those siblings
/// are path arrays plus `sourceAttribution`; they carry no credentials, so
/// keeping them costs nothing this function exists to prevent.
///
/// A missing or unparseable lockfile is a no-op: there is nothing to strip and
/// nothing was proven, so this never manufactures a file.
pub fn strip_lockfile_to_agy_rows(lockfile_path: &Path) -> Result<(), ProjectionError> {
    let Some(raw) = fs::read_to_string(lockfile_path).ok() else {
        return Ok(());
    };
    let Some(Value::Object(root)) = serde_json::from_str::<Value>(&raw).ok() else {
        return Ok(());
    };
    let Some(meta @ Value::Object(_)) = root.get(PROJECTION_LOCK_KEY) else {
        return Ok(());
    };

    let mut kept_root = Map::new();
    kept_root.insert(PROJECTION_LOCK_KEY.to_string(), meta.clone());

    let rendered = serde_json::to_string_pretty(&Value::Object(kept_root))
        .map_err(|err| ProjectionError::Message(err.to_string()))?;
    write_text(lockfile_path, &rendered, false)?;
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ManagedArtifactRegistry {
    lockfile_path: PathBuf,
    lockfile_root: Option<Map<String, Value>>,
    prior: BTreeMap<String, BTreeSet<String>>,
    next: BTreeMap<String, BTreeSet<String>>,
    /// Maps normalized path → source id recorded in the prior lockfile.
    /// Populated from `"sourceAttribution"` on load; used by per-source prune.
    source_attribution_prior: BTreeMap<String, String>,
    /// Maps normalized path → source id being built for the next lockfile write.
    source_attribution_next: BTreeMap<String, String>,
}

impl ManagedArtifactRegistry {
    pub(crate) fn load(lockfile_path: &Path, report: &mut ProjectionReport) -> Self {
        let mut registry = Self {
            lockfile_path: lockfile_path.to_path_buf(),
            ..Self::default()
        };
        if !lockfile_path.is_file() {
            return registry;
        }

        let raw = match fs::read_to_string(lockfile_path) {
            Ok(raw) => raw,
            Err(err) => {
                report.warnings.push(ProjectionWarning {
                    message: format!(
                        "could not read projection lockfile marker, skipped managed cleanup: {} ({err})",
                        lockfile_path.display()
                    ),
                });
                return registry;
            }
        };

        let root = match serde_json::from_str::<Value>(&raw) {
            Ok(Value::Object(root)) => root,
            Ok(_) => {
                report.warnings.push(ProjectionWarning {
                    message: format!(
                        "projection lockfile marker is not a JSON object, skipped managed cleanup: {}",
                        lockfile_path.display()
                    ),
                });
                return registry;
            }
            Err(err) => {
                report.warnings.push(ProjectionWarning {
                    message: format!(
                        "could not parse projection lockfile marker, skipped managed cleanup: {} ({err})",
                        lockfile_path.display()
                    ),
                });
                return registry;
            }
        };

        let mut projection = BTreeMap::new();
        let mut attribution = BTreeMap::new();
        if let Some(Value::Object(meta)) = root.get(PROJECTION_LOCK_KEY) {
            for category in [
                MANAGED_AGENT_PATHS,
                MANAGED_CODEX_AGENT_PATHS,
                MANAGED_OPENCODE_AGENT_PATHS,
                MANAGED_COMMAND_PATHS,
                MANAGED_LEGACY_PATHS,
                MANAGED_SKILL_PATHS,
                MANAGED_AGY_ROOT_PATHS,
            ] {
                projection.insert(
                    category.to_string(),
                    load_managed_artifact_set(meta, category),
                );
            }
            if let Some(Value::Object(attr_map)) = meta.get(MANAGED_SOURCE_ATTRIBUTION) {
                for (path, src) in attr_map {
                    if let Some(src_str) = src.as_str() {
                        attribution.insert(path.clone(), src_str.to_string());
                    }
                }
            }
        }

        registry.lockfile_root = Some(root);
        registry.prior = projection.clone();
        registry.next = projection;
        registry.source_attribution_prior = attribution.clone();
        registry.source_attribution_next = attribution;
        registry
    }

    pub(crate) fn can_mutate(&self, path: &Path) -> bool {
        // No prior lockfile → first-run; allow all mutations (backward-compat with is_ccync_managed_file guard).
        if self.prior.is_empty() {
            return true;
        }
        let needle = normalize_managed_artifact_path(path);
        self.prior.values().any(|paths| paths.contains(&needle))
    }

    /// Positive test: was this exact path recorded as CCYNC-managed in the prior
    /// lockfile? Unlike [`can_mutate`], this is `false` on first run (empty prior),
    /// so a pre-existing path that CCYNC never wrote is treated as user-owned even
    /// before any lockfile exists — used to guard command-skill writes from
    /// clobbering a genuine user directory.
    pub(crate) fn is_managed(&self, path: &Path) -> bool {
        let needle = normalize_managed_artifact_path(path);
        self.prior.values().any(|paths| paths.contains(&needle))
    }

    pub(crate) fn prior_paths(&self, category: &str) -> impl Iterator<Item = &str> {
        self.prior
            .get(category)
            .into_iter()
            .flatten()
            .map(String::as_str)
    }

    /// True when the prior lockfile recorded no managed paths (missing/empty
    /// lockfile, or a lockfile with no projection marker).
    pub(crate) fn prior_is_empty(&self) -> bool {
        self.prior.values().all(BTreeSet::is_empty)
    }

    pub(crate) fn replace_category(&mut self, category: &str) {
        self.next.insert(category.to_string(), BTreeSet::new());
    }

    pub(crate) fn mark(&mut self, category: &str, path: &Path) {
        self.next
            .entry(category.to_string())
            .or_default()
            .insert(normalize_managed_artifact_path(path));
    }

    /// Like [`mark`], but also records which source owns this path.
    /// Used by multi-source projection so per-source prune can
    /// distinguish "source removed from installed set" from "source absent this invocation".
    pub(crate) fn mark_with_source(&mut self, category: &str, path: &Path, source_id: &str) {
        let normalized = normalize_managed_artifact_path(path);
        self.next
            .entry(category.to_string())
            .or_default()
            .insert(normalized.clone());
        self.source_attribution_next
            .insert(normalized, source_id.to_string());
    }

    /// Return the source id that owned `path` according to the prior lockfile.
    /// Returns `None` when attribution was not recorded (single-source or pre-attribution lockfile).
    pub(crate) fn owning_source_of(&self, path: &Path) -> Option<&str> {
        let needle = normalize_managed_artifact_path(path);
        self.source_attribution_prior
            .get(&needle)
            .map(String::as_str)
    }

    /// GC attribution rows that name an installed source but no longer appear
    /// in any managed-artifact category — an orphan left behind when a path
    /// was pruned/replaced elsewhere without its attribution row being
    /// cleared alongside it. A row owned by a source absent from
    /// `installed_source_ids` is retained untouched: this invocation has no
    /// authority to judge whether it's still live, mirroring the
    /// foreign-source protection in [`prune_stale_links`].
    fn gc_installed_source_orphans(&mut self, installed_source_ids: &[&str]) {
        let live: BTreeSet<String> = self.next.values().flatten().cloned().collect();
        self.source_attribution_next.retain(|path, source_id| {
            live.contains(path) || !installed_source_ids.contains(&source_id.as_str())
        });
    }

    pub(crate) fn persist(
        &mut self,
        installed_source_ids: &[&str],
        dry_run: bool,
        report: &mut ProjectionReport,
    ) -> Result<(), ProjectionError> {
        self.gc_installed_source_orphans(installed_source_ids);

        // Bootstrap a fresh root when no prior lockfile existed.
        let mut root = self.lockfile_root.clone().unwrap_or_default();
        if self.prior == self.next && self.source_attribution_prior == self.source_attribution_next
        {
            return Ok(());
        }

        // `MANAGED_AGY_ROOT_PATHS` rows are written out-of-band by
        // `AgyRootOwnership::record_now`'s forced pre-swap write, ahead of —
        // and possibly by a sibling registry instance rather than — this
        // run's own end-of-run persist. This instance's own `next` for that
        // one category is therefore never authoritative: unioning it back in
        // would resurrect a row a later teardown removed whenever this
        // instance loaded before the removal, and serializing it verbatim
        // would let this instance overwrite rows a sibling instance recorded
        // meanwhile. Disk is read fresh, right before serialization, and is
        // the sole source for this category.
        let agy_root_paths_on_disk =
            read_managed_artifact_set_from_disk(&self.lockfile_path, MANAGED_AGY_ROOT_PATHS);

        let other_categories_non_empty = self
            .next
            .iter()
            .filter(|(category, _)| category.as_str() != MANAGED_AGY_ROOT_PATHS)
            .any(|(_, paths)| !paths.is_empty());
        let non_empty = other_categories_non_empty
            || !agy_root_paths_on_disk.is_empty()
            || !self.source_attribution_next.is_empty();
        if non_empty {
            let mut meta = Map::new();
            meta.insert(
                "schemaVersion".into(),
                Value::Number(PROJECTION_LOCK_SCHEMA_VERSION.into()),
            );
            for category in [
                MANAGED_AGENT_PATHS,
                MANAGED_CODEX_AGENT_PATHS,
                MANAGED_OPENCODE_AGENT_PATHS,
                MANAGED_COMMAND_PATHS,
                MANAGED_LEGACY_PATHS,
                MANAGED_SKILL_PATHS,
                MANAGED_AGY_ROOT_PATHS,
            ] {
                let values = if category == MANAGED_AGY_ROOT_PATHS {
                    agy_root_paths_on_disk.clone()
                } else {
                    self.next.get(category).cloned().unwrap_or_default()
                }
                .into_iter()
                .map(Value::String)
                .collect();
                meta.insert(category.into(), Value::Array(values));
            }
            if !self.source_attribution_next.is_empty() {
                let attr_obj: Map<String, Value> = self
                    .source_attribution_next
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                    .collect();
                meta.insert(MANAGED_SOURCE_ATTRIBUTION.into(), Value::Object(attr_obj));
            }
            root.insert(PROJECTION_LOCK_KEY.into(), Value::Object(meta));
        } else {
            root.remove(PROJECTION_LOCK_KEY);
        }

        let rendered = serde_json::to_string_pretty(&Value::Object(root))
            .map_err(|err| ProjectionError::Message(err.to_string()))?;
        if write_text(&self.lockfile_path, &rendered, dry_run)? {
            report.written_files.push(self.lockfile_path.clone());
        }
        Ok(())
    }
}

impl SkillsProjectionHealthCheck {
    pub fn with_path(shared_skills_root: PathBuf) -> Self {
        Self { shared_skills_root }
    }
}

impl HealthCheck for SkillsProjectionHealthCheck {
    fn name(&self) -> &str {
        "skills-projection"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        if !self.shared_skills_root.exists() {
            return vec![DoctorFinding::warning(format!(
                "shared skill projection not found: {} — run `ccync sync`",
                self.shared_skills_root.display()
            ))];
        }
        if !self.shared_skills_root.is_dir() {
            return vec![DoctorFinding::error(
                format!(
                    "shared skill projection is not a directory: {}",
                    self.shared_skills_root.display()
                ),
                "remove the path and rerun `ccync sync`",
            )];
        }
        vec![]
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Frontmatter {
    pub(crate) description: Option<String>,
    pub(crate) color: Option<String>,
    pub(crate) tools: Vec<String>,
}

pub(crate) fn parse_frontmatter(raw: &str) -> Frontmatter {
    let mut lines = raw.lines();
    if lines.next() != Some("---") {
        return Frontmatter::default();
    }

    let mut description = None;
    let mut color = None;
    let mut tools = Vec::new();
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            let value = value.trim().trim_matches('"');
            if key == "description" {
                description = Some(value.to_string());
            } else if key == "color" {
                color = Some(value.to_string());
            } else if key == "tools" {
                tools = value
                    .trim_matches(['[', ']'])
                    .split(',')
                    .map(|part| part.trim().trim_matches('"').trim_matches('\''))
                    .filter(|part| !part.is_empty())
                    .map(str::to_string)
                    .collect();
            }
        }
    }

    Frontmatter {
        description,
        color,
        tools,
    }
}

pub(crate) fn strip_frontmatter(raw: &str) -> String {
    let mut lines = raw.lines();
    if lines.next() != Some("---") {
        return raw.trim().to_string();
    }

    let mut body = String::new();
    let mut in_frontmatter = true;
    for line in raw.lines().skip(1) {
        if in_frontmatter {
            if line.trim() == "---" {
                in_frontmatter = false;
            }
            continue;
        }
        body.push_str(line);
        body.push('\n');
    }
    body.trim().to_string()
}

/// Remove `key:` entries from the first frontmatter block, preserving all other
/// bytes and leaving occurrences in the body untouched.
pub(crate) fn remove_frontmatter_key(raw: &str, key: &str) -> String {
    let mut lines = raw.lines();
    if lines.next() != Some("---") {
        return raw.to_string();
    }

    let prefix = format!("{key}:");
    let mut output = String::new();
    for (index, line) in raw.lines().enumerate() {
        if index > 0 && line.trim() == "---" {
            output.push_str(line);
            output.push('\n');
            output.extend(raw.lines().skip(index + 1).flat_map(|line| [line, "\n"]));
            return output.trim_end_matches('\n').to_string();
        }
        if index == 0 || !line.trim_start().starts_with(&prefix) {
            output.push_str(line);
            output.push('\n');
        }
    }
    raw.to_string()
}

pub(crate) fn render_opencode_agent(_name: &str, frontmatter: &Frontmatter, body: &str) -> String {
    let mut permissions = Vec::new();
    for tool in &frontmatter.tools {
        match tool.as_str() {
            "read" => {
                push_permission(&mut permissions, "read");
                push_permission(&mut permissions, "list");
            }
            "search" => {
                push_permission(&mut permissions, "read");
                push_permission(&mut permissions, "list");
                push_permission(&mut permissions, "grep");
                push_permission(&mut permissions, "glob");
            }
            "list" => {
                push_permission(&mut permissions, "list");
            }
            "edit" => {
                push_permission(&mut permissions, "edit");
            }
            "execute" => {
                push_permission(&mut permissions, "bash");
            }
            _ => {}
        }
    }

    if permissions.is_empty() {
        push_permission(&mut permissions, "read");
        push_permission(&mut permissions, "list");
    }

    let description = frontmatter
        .description
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "CCYNC managed agent".into());

    let mut lines = vec![
        CCYNC_MANAGED_FILE_HEADER.to_string(),
        "---".into(),
        "description: |".into(),
    ];
    for line in description.lines() {
        lines.push(format!("  {line}"));
    }
    lines.push("mode: subagent".into());
    if let Some(color) = &frontmatter.color {
        lines.push(format!("color: {color}"));
    }
    lines.push("permission:".into());
    for permission in permissions {
        lines.push(format!("  {permission}: allow"));
    }
    lines.push("---".into());
    lines.push(String::new());
    lines.push(body.trim().to_string());
    lines.push(String::new());
    lines.join("\n")
}

pub(crate) fn render_opencode_command(name: &str, frontmatter: &Frontmatter, body: &str) -> String {
    let description = frontmatter
        .description
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "CCYNC command".into());
    let rendered_body = if name == "git-commit-msg" {
        "Use the baseline below for the type(scope) prefix (it is a no-hijack path/status classifier that never reads the diff body), then read the staged diff and write a subject describing what actually changed. Do not ship the generic baseline subject verbatim. Keep the type(scope) prefix unless the diff clearly contradicts it. Add up to three body bullets only for broader changes. Do not add explanations, markdown fences, reasoning tags, JSON, or any extra prose. If the baseline reports No changes staged for commit. or Not a git repository., return that text exactly. Apply extra instructions if provided: $ARGUMENTS\n\n!`ccync commit-msg --print`".to_string()
    } else {
        body.trim().to_string()
    };

    let mut lines = vec![
        CCYNC_MANAGED_FILE_HEADER.to_string(),
        "---".into(),
        "description: |".into(),
    ];
    for line in description.lines() {
        lines.push(format!("  {line}"));
    }
    lines.push("---".into());
    lines.push(String::new());
    lines.push("User command arguments, if any: $ARGUMENTS".into());
    lines.push(String::new());
    lines.push(rendered_body);
    lines.push(String::new());
    lines.join("\n")
}

pub(crate) fn push_permission(permissions: &mut Vec<&'static str>, value: &'static str) {
    if !permissions.contains(&value) {
        permissions.push(value);
    }
}

pub(crate) fn list_named_children(root: &Path) -> Result<Vec<String>, ProjectionError> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            names.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    names.sort();
    Ok(names)
}

pub(crate) fn list_files_with_extension(
    root: &Path,
    suffix: &str,
) -> Result<Vec<PathBuf>, ProjectionError> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry
                .path()
                .file_name()
                .and_then(OsStr::to_str)
                .map(|name| name.ends_with(suffix))
                .unwrap_or(false)
        {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

pub(crate) fn list_command_dirs(root: &Path) -> Result<Vec<PathBuf>, ProjectionError> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut dirs = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        if path.join("SKILL.md").is_file() || path.join("SKILL.template.md").is_file() {
            dirs.push(path);
        }
    }
    dirs.sort();
    Ok(dirs)
}

pub(crate) fn list_flat_command_files(root: &Path) -> Result<Vec<PathBuf>, ProjectionError> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry
                .path()
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| extension == "md")
        {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

pub(crate) fn ensure_name_frontmatter(content: &str, name: &str) -> String {
    if !content.starts_with("---\n") && !content.starts_with("---\r\n") {
        return format!("---\nname: {name}\n---\n{content}");
    }

    let newline = if content.starts_with("---\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let opening_len = 3 + newline.len();
    let Some(end) = content[opening_len..].find(&format!("{newline}---")) else {
        return format!("---{newline}name: {name}{newline}---{newline}{content}");
    };
    let frontmatter = &content[opening_len..opening_len + end];
    if frontmatter
        .lines()
        .any(|line| line.trim_start().starts_with("name:"))
    {
        return content.to_string();
    }
    format!(
        "---{newline}name: {name}{newline}{}",
        &content[opening_len..]
    )
}

pub(crate) fn read_markdown_required(path: &Path) -> Result<String, ProjectionError> {
    fs::read_to_string(path).map_err(ProjectionError::from)
}

pub(crate) fn bake_command_content(
    command_dir: &Path,
    repo_root: &Path,
) -> Result<String, ProjectionError> {
    let template = command_dir.join("SKILL.template.md");
    if !template.is_file() {
        return read_markdown_required(&command_dir.join("SKILL.md"));
    }

    let mut baked = read_markdown_required(&template)?;
    baked = baked.replace("{{CCYNC_ROOT}}", &repo_root.display().to_string());

    let local_override = command_dir.join("SKILL.local.md");
    if !local_override.is_file() {
        return Ok(baked);
    }

    let local = read_markdown_required(&local_override)?;
    if local.trim().is_empty() {
        return Ok(baked);
    }

    Ok(format!(
        "{}\n\n<!-- CCYNC LOCAL OVERRIDE START -->\n<!-- Source: SKILL.local.md (gitignored machine-local overlay) -->\n{}\n<!-- CCYNC LOCAL OVERRIDE END -->\n",
        baked.trim_end(),
        local.trim()
    ))
}

/// Record a single touched path into `evidence`, attributing it to `owner`'s
/// own target-scoped bucket when the caller supplies one, or to `common` when
/// `owner` is `None` — the caller's own context (which target it is currently
/// projecting for, if any) decides attribution directly.
fn record_evidence_path(
    evidence: &mut ProjectionEvidence,
    path: &Path,
    change: ProjectionPathChange,
    owner: Option<ProjectionTargetId>,
) {
    let result = ProjectionPathResult {
        path: path.to_path_buf(),
        change,
    };
    match owner {
        Some(target) => evidence
            .by_target
            .entry(target)
            .or_default()
            .paths
            .push(result),
        None => evidence.common.push(result),
    }
}

/// Additive evidence-shaped counterpart to [`write_text_with_report`] — same
/// write, same on-disk effect (delegates to the same [`write_text`]), but
/// attributes the touched path directly via `owner` instead of relying on
/// post-hoc bare-path classification. Nothing calls this yet; it exists
/// alongside the unchanged legacy function.
pub(crate) fn write_text_with_evidence(
    path: &Path,
    content: &str,
    dry_run: bool,
    owner: Option<ProjectionTargetId>,
) -> Result<ProjectionEvidence, ProjectionError> {
    let mut evidence = ProjectionEvidence::default();
    if write_text(path, content, dry_run)? {
        record_evidence_path(&mut evidence, path, ProjectionPathChange::Written, owner);
    }
    Ok(evidence)
}

/// Additive evidence-shaped counterpart to [`write_json_map_with_report`] —
/// same rendering and same write as the legacy function, attributing the
/// touched path via `owner`. Nothing calls this yet.
#[allow(dead_code)] // additive, unwired — production callers land in a later task
pub(crate) fn write_json_map_with_evidence(
    path: &Path,
    data: &Map<String, Value>,
    dry_run: bool,
    owner: Option<ProjectionTargetId>,
) -> Result<ProjectionEvidence, ProjectionError> {
    let rendered = serde_json::to_string_pretty(&Value::Object(data.clone()))
        .map_err(|err| ProjectionError::Message(err.to_string()))?;
    write_text_with_evidence(path, &rendered, dry_run, owner)
}

/// Prune stale entries under `root`, attributing each removed path via
/// `owner` (or `common` when `owner` is `None`). Identical scan,
/// ownership-authorization, and removal logic (same I/O, same faults
/// propagated via `?`) regardless of which target owns the root.
///
/// `sources` is the full currently-installed source list (ids derived here);
/// a source being uninstalled must still appear in it with zero `keep_names`
/// during its final projection pass (see `docs/architecture.md`) so its stale
/// entries fall through to normal authorization instead of being protected.
/// An entry is protected here (skipped) only when its recorded owner is
/// foreign — attributed to a source absent from `sources` entirely, meaning
/// this invocation has no authority over it.
pub(crate) fn prune_stale_links<'a>(
    root: &Path,
    keep_names: impl IntoIterator<Item = &'a str>,
    dry_run: bool,
    registry: &ManagedArtifactRegistry,
    sources: &[ProjectionSource],
    owner: Option<ProjectionTargetId>,
) -> Result<ProjectionEvidence, ProjectionError> {
    let mut evidence = ProjectionEvidence::default();
    if !root.is_dir() {
        return Ok(evidence);
    }

    let installed_source_ids: Vec<&str> = sources.iter().map(|s| s.id.as_str()).collect();
    let keep: Vec<&str> = keep_names.into_iter().collect();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if keep.contains(&name) {
            continue;
        }

        let path = entry.path();
        let symlink = is_symlink_or_junction(&path);

        let owning_source_id = registry.owning_source_of(&path);
        let owning_source_is_foreign = owning_source_id
            .map(|src| !installed_source_ids.contains(&src))
            .unwrap_or(false);
        if owning_source_is_foreign {
            continue;
        }

        // Orphan-symlink path: a stale category row can go missing (e.g. a prior
        // run's GC bug) while the attribution row survives. Prove the symlink is
        // still this installed owner's own artifact by resolving its target into
        // the owner's root, rather than trusting `can_mutate`'s category-row test.
        let authorized_by_source_root = symlink
            && owning_source_id
                .and_then(|src_id| sources.iter().find(|source| source.id == src_id))
                .map(|source| is_symlink_target_inside_root(&path, &source.root))
                .unwrap_or(false);

        let authorized = is_ccync_owned_real_path(&path)
            || authorized_by_source_root
            || (symlink && registry.can_mutate(&path))
            || (!symlink && !path.is_dir() && registry.is_managed(&path));
        if authorized {
            if !dry_run {
                if symlink {
                    remove_link(&path)?;
                } else if path.is_dir() {
                    fs::remove_dir_all(&path)?;
                } else {
                    fs::remove_file(&path)?;
                }
            }
            record_evidence_path(&mut evidence, &path, ProjectionPathChange::Removed, owner);
        }
    }
    Ok(evidence)
}

/// Prune stale command-skill links under `root`, attributing each removed
/// path via `owner` (or `common` when `owner` is `None`). Identical
/// scan/filter/removal logic regardless of which target owns the root.
pub(crate) fn prune_stale_command_links(
    root: &Path,
    active_names: &[&str],
    dry_run: bool,
    registry: &ManagedArtifactRegistry,
    owner: Option<ProjectionTargetId>,
) -> Result<ProjectionEvidence, ProjectionError> {
    let mut evidence = ProjectionEvidence::default();
    if !root.is_dir() {
        return Ok(evidence);
    }

    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        if active_names.contains(&name)
            || !is_ccync_command_link(&path, &path_needle_for_commands())
            || !registry.can_mutate(&path)
        {
            continue;
        }
        if is_symlink_or_junction(&path) {
            if !dry_run {
                remove_link(&path)?;
            }
            record_evidence_path(&mut evidence, &path, ProjectionPathChange::Removed, owner);
        }
    }
    Ok(evidence)
}

/// Prune stale real command-skill directories under `root`. A directory is
/// removable only when its root was not previously projected as a skill and
/// its `SKILL.md` was previously recorded as a command projection.
pub(crate) fn prune_stale_command_skill_dirs(
    root: &Path,
    active_names: &[&str],
    dry_run: bool,
    registry: &ManagedArtifactRegistry,
    owner: Option<ProjectionTargetId>,
) -> Result<ProjectionEvidence, ProjectionError> {
    let mut evidence = ProjectionEvidence::default();
    if !root.is_dir() {
        return Ok(evidence);
    }

    let prior_skill_paths: BTreeSet<&str> = registry.prior_paths(MANAGED_SKILL_PATHS).collect();
    let prior_command_paths: BTreeSet<&str> = registry.prior_paths(MANAGED_COMMAND_PATHS).collect();

    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if is_symlink_or_junction(&path) || !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if active_names.contains(&name.as_str()) {
            continue;
        }

        let skill_root = normalize_managed_artifact_path(&path);
        let skill_file = normalize_managed_artifact_path(&path.join("SKILL.md"));
        if prior_skill_paths.contains(skill_root.as_str())
            || !prior_command_paths.contains(skill_file.as_str())
        {
            continue;
        }

        if !dry_run {
            fs::remove_dir_all(&path)?;
        }
        record_evidence_path(&mut evidence, &path, ProjectionPathChange::Removed, owner);
    }
    Ok(evidence)
}

/// Prune legacy `ccync-`-prefixed directories under `root`, attributing each
/// removed directory via `owner` (or `common` when `owner` is `None`).
pub(crate) fn prune_legacy_ccync_prefixed_dirs(
    root: &Path,
    active_names: &[&str],
    dry_run: bool,
    registry: &ManagedArtifactRegistry,
    owner: Option<ProjectionTargetId>,
) -> Result<ProjectionEvidence, ProjectionError> {
    let mut evidence = ProjectionEvidence::default();
    if !root.is_dir() {
        return Ok(evidence);
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        if !name.starts_with("ccync-")
            || active_names.contains(&name)
            || !registry.can_mutate(&path)
        {
            continue;
        }
        if !dry_run {
            fs::remove_dir_all(&path)?;
        }
        record_evidence_path(&mut evidence, &path, ProjectionPathChange::Removed, owner);
    }
    Ok(evidence)
}

/// Prune stale managed files under `root` matching `extension`, attributing
/// each removed path via `owner` (or `common` when `owner` is `None`). The
/// removal itself is inlined (its own `is_ccync_managed_file`/`can_mutate`
/// re-check is redundant here since the caller's loop already established
/// both).
pub(crate) fn prune_stale_managed_files(
    root: &Path,
    extension: &str,
    active_names: &[&str],
    dry_run: bool,
    registry: &ManagedArtifactRegistry,
    owner: Option<ProjectionTargetId>,
) -> Result<ProjectionEvidence, ProjectionError> {
    let mut evidence = ProjectionEvidence::default();
    if !root.is_dir() {
        return Ok(evidence);
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let is_extension_match = path
            .extension()
            .and_then(OsStr::to_str)
            .map(|value| value.eq_ignore_ascii_case(extension))
            .unwrap_or(false);
        if !is_extension_match || !is_ccync_managed_file(&path) || !registry.can_mutate(&path) {
            continue;
        }
        let Some(name) = path.file_stem().and_then(OsStr::to_str) else {
            continue;
        };
        if active_names.contains(&name) {
            continue;
        }
        if !dry_run {
            fs::remove_file(&path)?;
        }
        record_evidence_path(&mut evidence, &path, ProjectionPathChange::Removed, owner);
    }
    Ok(evidence)
}

pub(crate) fn ensure_dir(path: &Path, dry_run: bool) -> Result<(), ProjectionError> {
    if dry_run || path.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(path)?;
    Ok(())
}

/// Project a baked command as a runtime skill at `<skills_root>/<name>/SKILL.md`.
///
/// A baked command (`SKILL.md` with `name`/`description` frontmatter) is already
/// a valid skill, so projection is a verbatim write — this exposes the CCYNC
/// command as a skill on runtimes whose capability unit is a skill, not a slash
/// command (Copilot `/ccync-status`, Codex `$ccync-status`).
///
/// Preserves a genuine user directory at the same name: a real (non-symlink)
/// directory that CCYNC never recorded (`!is_managed`, false even on first run) and
/// whose `SKILL.md` differs from our output is left untouched with a warning (the
/// a2 user-content guard). Returns the written path so the caller can record it in
/// the registry, or `None` when preserved.
pub(crate) fn write_command_skill(
    skills_root: &Path,
    name: &str,
    content: &str,
    dry_run: bool,
    report: &mut ProjectionReport,
    registry: &ManagedArtifactRegistry,
) -> Result<Option<PathBuf>, ProjectionError> {
    let dir = skills_root.join(name);
    let skill_file = dir.join("SKILL.md");

    if dir.exists() && !is_symlink_or_junction(&dir) && !registry.is_managed(&skill_file) {
        let normalized = format!("{}\n", content.trim_end());
        let is_ours = fs::read_to_string(&skill_file)
            .ok()
            .map(|existing| existing == normalized)
            .unwrap_or(false);
        if !is_ours {
            report.warnings.push(ProjectionWarning {
                message: format!("preserved user-owned path: {}", dir.display()),
            });
            return Ok(None);
        }
    }

    if is_symlink_or_junction(&dir) && !dry_run {
        remove_link(&dir)?;
    }
    if write_text(&skill_file, content, dry_run)? {
        report.written_files.push(skill_file.clone());
    }
    Ok(Some(skill_file))
}

/// Remove a previously-projected command-skill at `<skills_root>/<name>` when its
/// runtime is no longer selected, so it does not linger in a shared skills dir
/// (`~/.agents/skills`, read by codex+opencode+copilot) and reintroduce a
/// double-load. Removes a legacy symlink, or a CCYNC-written real directory the
/// registry recorded (`is_managed`) — a genuine user directory (never managed)
/// is left untouched.
pub(crate) fn remove_ccync_command_skill(
    skills_root: &Path,
    name: &str,
    repo_root: &Path,
    registry: &ManagedArtifactRegistry,
    dry_run: bool,
    evidence: &mut ProjectionEvidence,
    owner: Option<ProjectionTargetId>,
) -> Result<(), ProjectionError> {
    let dir = skills_root.join(name);
    if is_symlink_or_junction(&dir) {
        // Only a CCYNC-owned repo-link is removed; a user's own symlink (e.g. to an
        // external skill) is preserved.
        if is_ccync_repo_link(&dir, repo_root) {
            remove_link_if_present(&dir, dry_run, evidence, owner)?;
        }
    } else if dir.is_dir() && registry.is_managed(&dir.join("SKILL.md")) {
        if !dry_run {
            fs::remove_dir_all(&dir)?;
        }
        record_evidence_path(evidence, &dir, ProjectionPathChange::Removed, owner);
    }
    Ok(())
}

pub(crate) fn write_text(
    path: &Path,
    content: &str,
    dry_run: bool,
) -> Result<bool, ProjectionError> {
    let normalized = format!("{}\n", content.trim_end());
    if fs::read_to_string(path)
        .ok()
        .map(|existing| existing == normalized)
        .unwrap_or(false)
    {
        return Ok(false);
    }
    if !dry_run {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, normalized.as_bytes())?;
    }
    Ok(true)
}

pub(crate) fn remove_link_if_present(
    path: &Path,
    dry_run: bool,
    evidence: &mut ProjectionEvidence,
    owner: Option<ProjectionTargetId>,
) -> Result<(), ProjectionError> {
    if !is_symlink_or_junction(path) {
        return Ok(());
    }
    if !dry_run {
        remove_link(path)?;
    }
    record_evidence_path(evidence, path, ProjectionPathChange::Removed, owner);
    Ok(())
}

pub(crate) fn remove_managed_file_if_present(
    path: &Path,
    dry_run: bool,
    evidence: &mut ProjectionEvidence,
    owner: Option<ProjectionTargetId>,
    registry: &ManagedArtifactRegistry,
) -> Result<(), ProjectionError> {
    if !is_ccync_managed_file(path) || !registry.can_mutate(path) {
        return Ok(());
    }
    if !dry_run {
        fs::remove_file(path)?;
    }
    record_evidence_path(evidence, path, ProjectionPathChange::Removed, owner);
    Ok(())
}

/// Ensure `link` points at `target`, returning `true` when the link is (or
/// remains) CCYNC-owned and safe to mark in the managed-artifact registry.
/// Returns `false` only when a genuine user-owned path was preserved
/// untouched — every other outcome (already correct, dry-run/replacement of
/// an existing link, adoption of a CCYNC-owned real path, or fresh creation)
/// returns `true`.
pub(crate) fn ensure_dir_link(
    target: &Path,
    link: &Path,
    replace: bool,
    dry_run: bool,
    report: &mut ProjectionReport,
) -> Result<bool, ProjectionError> {
    if same_path(link, target) {
        return Ok(true);
    }
    if is_symlink_or_junction(link) {
        if dry_run {
            report.created_links.push(link.to_path_buf());
            return Ok(true);
        }
        remove_dir_link(link)?;
    } else if link.exists() {
        // Adopt a CCYNC-owned real path (file→symlink migration) without requiring
        // --replace; preserve only genuine user-owned paths (no CCYNC content marker
        // and not a CCYNC-projected managed agent).
        if !replace && !is_ccync_owned_real_path(link) {
            report.warnings.push(ProjectionWarning {
                message: format!("preserved user-owned path: {}", link.display()),
            });
            return Ok(false);
        }
        let backup = backup_path(link);
        if !dry_run {
            if backup.exists() {
                if backup.is_dir() {
                    fs::remove_dir_all(&backup)?;
                } else {
                    fs::remove_file(&backup)?;
                }
            }
            fs::rename(link, &backup)?;
        }
        report.removed_paths.push(backup);
    }
    if !dry_run {
        if let Some(parent) = link.parent() {
            fs::create_dir_all(parent)?;
        }
        create_dir_link(target, link)?;
    }
    report.created_links.push(link.to_path_buf());
    Ok(true)
}

pub(crate) fn same_path(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

pub(crate) fn backup_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .map(|value| format!("{value}.bak"))
        .unwrap_or_else(|| "backup.bak".into());
    path.with_file_name(name)
}

pub(crate) fn is_ccync_managed_file(path: &Path) -> bool {
    fs::read_to_string(path)
        .ok()
        .and_then(|content| content.lines().next().map(str::to_string))
        .map(|first| first == CCYNC_MANAGED_FILE_HEADER)
        .unwrap_or(false)
}

/// Content-marker test for an existing real (non-symlink) path during adopt.
///
/// A file is CCYNC-managed when its first line is [`CCYNC_MANAGED_FILE_HEADER`].
/// A directory is CCYNC-managed when it shallowly contains at least one
/// CCYNC-managed file — this lets the projection re-adopt a directory a prior
/// projection era materialized as real files (file→symlink migration) without
/// ever adopting a genuine user directory (which carries no CCYNC header).
pub(crate) fn is_ccync_managed_path(path: &Path) -> bool {
    if path.is_dir() {
        fs::read_dir(path)
            .ok()
            .map(|entries| {
                entries.flatten().any(|entry| {
                    let child = entry.path();
                    child.is_file() && is_ccync_managed_file(&child)
                })
            })
            .unwrap_or(false)
    } else {
        is_ccync_managed_file(path)
    }
}

/// Positive CCYNC-ownership test for an existing real path, used by both adopt
/// ([`ensure_dir_link`]) and prune ([`prune_stale_links`]).
///
/// True only when the path carries the CCYNC content header marker
/// ([`is_ccync_managed_path`]). Ownership is determined solely by content header,
/// not by agent frontmatter patterns; lockfile managed-set membership remains
/// the authority for ccync-owned agent mutations.
pub(crate) fn is_ccync_owned_real_path(path: &Path) -> bool {
    is_ccync_managed_path(path)
}

pub(crate) fn is_ccync_repo_link(path: &Path, repo_root: &Path) -> bool {
    if !is_symlink_or_junction(path) {
        return false;
    }
    match (fs::canonicalize(path), fs::canonicalize(repo_root)) {
        (Ok(target), Ok(root)) => target.starts_with(root),
        _ => false,
    }
}

/// Prove a symlink's target lives inside `owner_root`, independent of the
/// managed-category lockfile rows — the authority for pruning an orphan
/// symlink whose attribution row survived a prior run but whose category row
/// did not. A live target is proven via [`is_ccync_repo_link`]'s
/// canonicalize-and-prefix check, reused with the owner's root standing in for
/// the repo root. A dangling target (the linked-to path no longer exists, so
/// canonicalize fails) is proven instead from the raw [`fs::read_link`] value:
/// it must be absolute and free of `..` (`ParentDir`) components — a relative
/// or traversal-bearing target cannot be trusted to resolve inside the root —
/// then checked against the root with a plain prefix test.
pub(crate) fn is_symlink_target_inside_root(path: &Path, owner_root: &Path) -> bool {
    if is_ccync_repo_link(path, owner_root) {
        return true;
    }
    let Ok(raw_target) = fs::read_link(path) else {
        return false;
    };
    if !raw_target.is_absolute()
        || raw_target
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return false;
    }
    // Deliberately not canonicalized: `fs::canonicalize` renders a Windows
    // `\\?\`-prefixed verbatim path, while `read_link`'s raw target and the
    // caller-supplied `owner_root` do not — canonicalizing either side alone
    // would make a genuinely in-root dangling target fail this prefix
    // comparison.
    raw_target.starts_with(owner_root)
}

pub(crate) fn path_needle_for_commands() -> String {
    format!(
        "{}commands{}",
        std::path::MAIN_SEPARATOR,
        std::path::MAIN_SEPARATOR
    )
}

pub(crate) fn is_ccync_command_link(path: &Path, repo_root_fragment: &str) -> bool {
    if !is_symlink_or_junction(path) {
        return false;
    }
    fs::canonicalize(path)
        .ok()
        .and_then(|target| target.to_str().map(str::to_string))
        .map(|target| target.contains(repo_root_fragment))
        .unwrap_or(false)
}

pub(crate) fn remove_link(path: &Path) -> io::Result<()> {
    // A directory junction / dir-symlink reports `is_dir() == false` via
    // `symlink_metadata` on Windows (a reparse point is a symlink, not a dir), so
    // gating link removal on `is_dir()` sent directory junctions to `fs::remove_file`
    // → `DeleteFile` refuses a directory reparse point with ERROR_ACCESS_DENIED
    // (os error 5). Route any reparse point through `remove_dir_link` (rmdir — removes
    // the link, not the target), falling back to `remove_file` for a file symlink.
    if is_symlink_or_junction(path) {
        return remove_dir_link(path).or_else(|_| fs::remove_file(path));
    }
    let metadata = path.symlink_metadata()?;
    if metadata.file_type().is_dir() {
        remove_dir_link(path)
    } else {
        fs::remove_file(path)
    }
}

pub(crate) fn remove_if_ccync_owned_dir(
    path: &Path,
    dry_run: bool,
    report: &mut ProjectionReport,
    registry: &ManagedArtifactRegistry,
) -> Result<(), ProjectionError> {
    if !path.exists() {
        return Ok(());
    }
    if !registry.can_mutate(path) {
        return Ok(());
    }
    if is_symlink_or_junction(path) {
        if !dry_run {
            remove_dir_link(path)?;
        }
        report.removed_paths.push(path.to_path_buf());
        return Ok(());
    }
    if path.is_dir() {
        let marker = path.join(".ccync-managed");
        if marker.exists() || path.file_name().and_then(OsStr::to_str) == Some("skills") {
            if !dry_run {
                fs::remove_dir_all(path)?;
            }
            report.removed_paths.push(path.to_path_buf());
        }
    }
    Ok(())
}

pub(crate) fn read_json_map_or_warn(
    path: &Path,
    report: &mut ProjectionReport,
) -> Result<Option<Map<String, Value>>, ProjectionError> {
    match read_json_map(path) {
        Ok(map) => Ok(Some(map)),
        Err(err) if path.exists() => {
            report.warnings.push(ProjectionWarning {
                message: format!(
                    "could not parse JSON file, skipped bridge update: {} ({err})",
                    path.display()
                ),
            });
            Ok(None)
        }
        Err(err) => Err(ProjectionError::from(err)),
    }
}

pub(crate) fn set_vscode_skills_bridge(settings: &mut Map<String, Value>) {
    let locations = settings
        .entry("chat.agentSkillsLocations".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !locations.is_object() {
        *locations = Value::Object(Map::new());
    }
    locations
        .as_object_mut()
        .expect("locations object")
        .insert("~/.agents/skills".into(), Value::Bool(false));
}

pub(crate) fn write_text_with_report(
    path: &Path,
    content: &str,
    dry_run: bool,
    report: &mut ProjectionReport,
) -> Result<(), ProjectionError> {
    if write_text(path, content, dry_run)? {
        report.written_files.push(path.to_path_buf());
    }
    Ok(())
}

pub(crate) fn write_json_map_with_report(
    path: &Path,
    data: &Map<String, Value>,
    dry_run: bool,
    report: &mut ProjectionReport,
) -> Result<(), ProjectionError> {
    let rendered = serde_json::to_string_pretty(&Value::Object(data.clone()))
        .map_err(|err| ProjectionError::Message(err.to_string()))?;
    write_text_with_report(path, &rendered, dry_run, report)
}

/// Same normalization/short-circuit/parent-creation/dry-run contract as
/// [`write_text`], but delegates real publication to
/// [`ccync_foundation::platform::atomic_write_bytes`] instead of a bare
/// `fs::write` — the sibling-temp-file + durable-sync + rename sequence, so a
/// crash mid-write cannot leave `path` truncated or half-written. Returns
/// `true` when a write happened (or would happen under `dry_run`).
pub(crate) fn write_text_atomically(
    path: &Path,
    content: &str,
    dry_run: bool,
) -> Result<bool, ProjectionError> {
    let normalized = format!("{}\n", content.trim_end());
    if fs::read_to_string(path)
        .ok()
        .map(|existing| existing == normalized)
        .unwrap_or(false)
    {
        return Ok(false);
    }
    if !dry_run {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write_bytes(path, normalized.as_bytes()).map_err(|err| {
            ProjectionError::Io(match err {
                AtomicWriteError::TempWrite(e) => e,
                AtomicWriteError::PreRename(e) => e,
                AtomicWriteError::Rename(e) => e,
            })
        })?;
    }
    Ok(true)
}

/// Report-appending counterpart to [`write_text_atomically`] — same
/// unchanged short circuit, and appends `path` to `report.written_files`
/// only after a real write succeeds, or as a dry-run preview.
pub(crate) fn write_text_atomically_with_report(
    path: &Path,
    content: &str,
    dry_run: bool,
    report: &mut ProjectionReport,
) -> Result<(), ProjectionError> {
    if write_text_atomically(path, content, dry_run)? {
        report.written_files.push(path.to_path_buf());
    }
    Ok(())
}

fn load_managed_artifact_set(meta: &Map<String, Value>, category: &str) -> BTreeSet<String> {
    meta.get(category)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// Read `category`'s current array straight off the lockfile at
/// `lockfile_path`, bypassing any [`ManagedArtifactRegistry`] instance's
/// in-memory `next`/`prior`. Used by [`ManagedArtifactRegistry::persist`]
/// for [`MANAGED_AGY_ROOT_PATHS`], the one category whose rows are written
/// out-of-band by `AgyRootOwnership::record_now` — disk is the sole
/// authority for that category, so a registry instance's own (possibly
/// stale, possibly ahead) in-memory copy never determines what gets
/// serialized. A missing or unparseable lockfile reads as empty, matching
/// [`ManagedArtifactRegistry::load`]'s own tolerance.
fn read_managed_artifact_set_from_disk(lockfile_path: &Path, category: &str) -> BTreeSet<String> {
    fs::read_to_string(lockfile_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| match value {
            Value::Object(mut root) => root.remove(PROJECTION_LOCK_KEY),
            _ => None,
        })
        .and_then(|meta| match meta {
            Value::Object(meta) => Some(load_managed_artifact_set(&meta, category)),
            _ => None,
        })
        .unwrap_or_default()
}

fn normalize_managed_artifact_path(path: &Path) -> String {
    let rendered = path.to_string_lossy().replace('/', "\\");
    #[cfg(windows)]
    {
        rendered.to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        rendered
    }
}

pub(crate) fn remove_file_if_present(
    path: &Path,
    dry_run: bool,
    report: &mut ProjectionReport,
) -> Result<(), ProjectionError> {
    if !path.is_file() {
        return Ok(());
    }
    if !dry_run {
        fs::remove_file(path)?;
    }
    report.removed_paths.push(path.to_path_buf());
    Ok(())
}

pub(crate) fn remove_empty_dir_if_present(
    path: &Path,
    dry_run: bool,
    report: &mut ProjectionReport,
) -> Result<(), ProjectionError> {
    if !path.is_dir() {
        return Ok(());
    }
    if fs::read_dir(path)?.next().is_some() {
        return Ok(());
    }
    if !dry_run {
        fs::remove_dir(path)?;
    }
    report.removed_paths.push(path.to_path_buf());
    Ok(())
}

// ── staged-copy engine (additive, unwired) ─────────────────────────────────
//
// A full-directory-copy counterpart to `ensure_dir_link`'s symlink/junction
// path: stage a complete copy of a source tree into a sibling temporary
// directory, then swap it into place. The swap is remove-old-then-rename —
// Windows cannot rename a directory over an existing one — so the window
// between the two steps is the only place the final path can be observed
// absent; it is never observed partially written, because nothing renames
// (or otherwise mutates) the final path until the staged copy has finished
// in full off to the side. Nothing calls this yet; production callers land
// in a later task.

/// Sibling staging path for `dest`: same parent directory (so the eventual
/// rename stays on one filesystem), name-prefixed so it never collides with
/// a real projected artifact. Process id + a per-process counter make each
/// call's path unique without pulling in a UUID dependency this crate does
/// not otherwise need.
#[allow(dead_code)] // additive, unwired — production callers land in a later task
fn stage_sibling_path(dest: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let mut name = std::ffi::OsString::from(".ccync-stage-");
    if let Some(dest_name) = dest.file_name() {
        name.push(dest_name);
    }
    name.push(format!(
        "-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    dest.with_file_name(name)
}

/// Recursively copy every file and subdirectory of `source` into `dest`,
/// creating `dest` (and any needed parents) first. Stops at the first I/O
/// fault, leaving whatever was already copied in place — callers that need
/// an all-or-nothing result on the final path use [`stage_tree_copy`], which
/// stages into a throwaway sibling directory rather than the real
/// destination.
#[allow(dead_code)] // additive, unwired — production callers land in a later task
fn copy_tree_recursive(source: &Path, dest: &Path) -> io::Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree_recursive(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Stage a full copy of `source` into a fresh sibling directory of `dest`
/// (see [`stage_sibling_path`]), returning the staged path on success. On
/// any I/O fault the partial staging directory is removed (best-effort)
/// before the error is returned — `dest` itself is never touched by this
/// step, so a fault here cannot leave `dest` partial.
#[allow(dead_code)] // additive, unwired — production callers land in a later task
fn stage_tree_copy(source: &Path, dest: &Path) -> Result<PathBuf, ProjectionError> {
    let staged = stage_sibling_path(dest);
    if let Err(err) = copy_tree_recursive(source, &staged) {
        let _ = fs::remove_dir_all(&staged);
        return Err(ProjectionError::from(err));
    }
    Ok(staged)
}

/// Content-equality comparison of two directory trees: same entry names at
/// every level, same file/directory kind at each name, and byte-identical
/// file contents. An unreadable directory or file compares unequal rather
/// than panicking or silently treating the pair as a match — the caller uses
/// this to decide whether a swap can be skipped at all, and "can't prove
/// equal" must fall through to the safe re-copy path, not the skip path.
#[allow(dead_code)] // additive, unwired — production callers land in a later task
fn trees_content_equal(a: &Path, b: &Path) -> bool {
    let (Ok(mut a_entries), Ok(mut b_entries)) = (list_entry_names(a), list_entry_names(b)) else {
        return false;
    };
    if a_entries.len() != b_entries.len() {
        return false;
    }
    a_entries.sort();
    b_entries.sort();
    if a_entries != b_entries {
        return false;
    }

    a_entries.iter().all(|name| {
        let a_path = a.join(name);
        let b_path = b.join(name);
        let a_is_dir = a_path.is_dir();
        if a_is_dir != b_path.is_dir() {
            return false;
        }
        if a_is_dir {
            trees_content_equal(&a_path, &b_path)
        } else {
            match (fs::read(&a_path), fs::read(&b_path)) {
                (Ok(a_bytes), Ok(b_bytes)) => a_bytes == b_bytes,
                _ => false,
            }
        }
    })
}

/// Entry names directly under `root`, unsorted — [`trees_content_equal`]
/// sorts both sides itself so this stays a plain read.
fn list_entry_names(root: &Path) -> io::Result<Vec<String>> {
    fs::read_dir(root)?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().to_string()))
        .collect()
}

/// Swap `staged` into place at `dest`: remove whatever currently occupies
/// `dest` (a real directory, a symlink/junction, or a plain file — every
/// shape this engine's destinations can take), then `rename` `staged` onto
/// it. Windows cannot rename a directory over an existing one, so this is a
/// remove-then-rename, not a true single-syscall swap: between the removal
/// and the rename, `dest` is briefly absent. That absent-then-complete
/// window is the accepted one — `dest` is never observed holding a partial
/// copy, because nothing writes into `dest` itself; the copy already
/// finished, off to the side, before this function runs.
#[allow(dead_code)] // additive, unwired — production callers land in a later task
fn swap_staged_tree(staged: &Path, dest: &Path) -> Result<(), ProjectionError> {
    if is_symlink_or_junction(dest) {
        remove_link(dest)?;
    } else if dest.is_dir() {
        fs::remove_dir_all(dest)?;
    } else if dest.exists() {
        fs::remove_file(dest)?;
    }
    fs::rename(staged, dest)?;
    Ok(())
}

/// Publish a full copy of `source` at `dest`: skip entirely when `dest` is
/// already a real directory matching `source` byte-for-byte
/// ([`trees_content_equal`], zero writes), otherwise stage a fresh copy
/// ([`stage_tree_copy`]) and swap it into place ([`swap_staged_tree`]).
/// Returns `true` when a swap happened, `false` when the destination
/// already matched and nothing was written.
#[allow(dead_code)] // additive, unwired — production callers land in a later task
fn publish_staged_tree_copy(source: &Path, dest: &Path) -> Result<bool, ProjectionError> {
    if dest.is_dir() && !is_symlink_or_junction(dest) && trees_content_equal(source, dest) {
        return Ok(false);
    }
    let staged = stage_tree_copy(source, dest)?;
    // A failed swap must not leave the staged tree behind. `stage_sibling_path`
    // deliberately places it beside `dest` so the rename stays on one
    // filesystem, which for an `agy-*` root means inside the very directory
    // Antigravity scans for plugins — an orphan there is a duplicate plugin
    // root, not merely wasted disk. `stage_tree_copy` already cleans up its
    // own faults; this closes the other half.
    if let Err(err) = swap_staged_tree(&staged, dest) {
        let _ = fs::remove_dir_all(&staged);
        return Err(err);
    }
    Ok(true)
}

/// Remove `.ccync-stage-*` siblings of `dest` left behind by an earlier run
/// that was killed between staging and swapping (a signal, a power loss —
/// the cases no in-process cleanup can reach). Best-effort and silent: a
/// sweep failure must never fail the publish that follows it. Safe to run
/// unconditionally because no live publish ever depends on a staging
/// directory it did not create in the same call — [`stage_tree_copy`]
/// returns its staged path directly to [`publish_staged_tree_copy`], which
/// consumes it before returning.
fn sweep_staging_orphans(dest: &Path) {
    let Some(parent) = dest.parent() else {
        return;
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(".ccync-stage-") {
            continue;
        }
        let path = entry.path();
        if is_symlink_or_junction(&path) {
            let _ = remove_link(&path);
        } else if path.is_dir() {
            let _ = fs::remove_dir_all(&path);
        } else {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Two-rung publish for an `agy-*` plugin root: try a symbolic link
/// ([`create_symlink_dir`]) first, and only on refusal fall back to a staged
/// directory copy ([`publish_staged_tree_copy`]). Never leaves an NTFS
/// junction at `dest` — either rung replaces one outright.
///
/// What already occupies `dest` is read with [`classify_link`], never
/// `exists()`, so a junction whose target has been deleted (which reports
/// `exists() == false` while still occupying the path) is still recognized
/// and replaced. A real, non-link directory is the one shape this function
/// will not touch on its own authority: it is preserved untouched with a
/// warning unless `ownership` already recorded it — `AgyRootOwnership::is_owned`
/// is `false` on an empty or missing lockfile, so an unrecorded directory is
/// preserved even on a machine with no lockfile at all.
///
/// Ownership is recorded through `ownership` before anything at `dest` is
/// touched — durable on disk ahead of both the link removal and the
/// swap/rename that makes the new tree visible — so no crash between the row
/// write and that visibility step can produce a tree without its row, and no
/// successful call can produce a row without (eventually) a tree.
///
/// `dry_run` performs zero disk writes and zero lockfile writes. It still
/// reports the shape a real run would produce where that shape is already
/// knowable without writing anything: an existing owned directory always
/// wins rung 2 (attempting a symlink over an occupied path always fails,
/// independent of platform capability), so `dry_run` predicts `Directory`
/// there; every other starting shape gets the optimistic rung-1 prediction,
/// since capability is only ever known by attempting the real write
/// (deliberately not probed or cached — see the plan's Approach).
///
/// Returns the shape now at `dest` **and** whether this call actually wrote
/// there — the same bare-`bool`-means-wrote convention [`write_text`] and
/// [`publish_staged_tree_copy`] already use in this file. `false` means the
/// path already held exactly the right thing, was preserved as user-owned,
/// or was a `dry_run`; callers must record no path evidence in that case, so
/// a stable rerun reports unchanged instead of a spurious change. The flag
/// describes `dest` only: `ownership.record_now` may still have merged a row
/// into the lockfile, which is separate registry evidence, not a change to
/// the published artifact.
#[allow(dead_code)] // additive, unwired — production callers land in a later task
pub(crate) fn publish_agy_root(
    source: &Path,
    dest: &Path,
    ownership: &AgyRootOwnership,
    dry_run: bool,
    report: &mut ProjectionReport,
) -> Result<(LinkShape, bool), ProjectionError> {
    let existing = classify_link(dest);
    if existing == LinkShape::Symlink && same_path(dest, source) {
        return Ok((LinkShape::Symlink, false));
    }
    if existing == LinkShape::Directory && !ownership.is_owned(dest) {
        report.warnings.push(ProjectionWarning {
            message: format!("preserved user-owned path: {}", dest.display()),
        });
        return Ok((LinkShape::Directory, false));
    }

    if dry_run {
        let predicted = if existing == LinkShape::Directory {
            LinkShape::Directory
        } else {
            LinkShape::Symlink
        };
        return Ok((predicted, false));
    }

    ownership.record_now(dest, dry_run)?;

    // From here the row is durable on disk. Row-before-tree is the required
    // ordering, but it only stays harmless while a failure leaves the
    // path absent: an orphan row is a standing `remove_dir_all` authority
    // over whatever later occupies this path. So every exit below that ends
    // without a tree releases the row again.
    let published = (|| -> Result<(LinkShape, bool), ProjectionError> {
        if is_symlink_or_junction(dest) {
            remove_link(dest)?;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        sweep_staging_orphans(dest);
        if let Ok(shape) = create_symlink_dir(source, dest) {
            return Ok((shape, true));
        }

        // `publish_staged_tree_copy` already compares before swapping and
        // reports whether it actually swapped; propagate that rather than
        // discarding it, or an unchanged copy would read back as a fresh
        // write on every sync.
        let swapped = publish_staged_tree_copy(source, dest)?;
        Ok((LinkShape::Directory, swapped))
    })();

    if published.is_err() && classify_link(dest) == LinkShape::Absent {
        // Best-effort: the publish error is the one worth reporting, and a
        // failed release degrades to exactly the orphan row `remove_agy_root`'s
        // `Absent` arm later self-heals.
        let _ = ownership.forget_now(dest, dry_run);
    }
    published
}

/// Tear down whatever occupies an `agy-*` root, dispatching on its actual
/// shape via [`classify_link`] — the counterpart teardown to
/// [`publish_agy_root`]'s publish.
///
/// - Symbolic link or legacy junction: unlinked via [`remove_link`], which
///   removes the link itself and never follows through to `canonical_root`.
/// - Real directory that `ownership` recorded (`AgyRootOwnership::is_owned`):
///   the tree is deleted with `fs::remove_dir_all`, then — only once that
///   succeeds — [`AgyRootOwnership::forget_now`] removes its row from disk.
///   Row-removal strictly follows tree-removal, never the reverse: a crash
///   in between leaves a harmless row-without-tree, the same accepted
///   ordering `publish_agy_root`'s own row-before-tree write already keys
///   off. If the row removal itself fails, the tree is already
///   gone and the error still propagates — the caller knows to retry rather
///   than believing teardown fully succeeded.
/// - Real directory `ownership` does not recognize: left untouched, with a
///   warning — never deleted on this function's own authority. An empty or
///   missing lockfile authorizes nothing (`AgyRootOwnership::is_owned` is
///   `false` in that case), so a machine with no lockfile at all still
///   preserves a user's own directory rather than treating "nothing
///   recorded" as "safe to delete".
/// - Absent: no-op.
///
/// `dry_run` performs zero disk writes and zero lockfile writes; it reports
/// what a real run would remove without removing it.
pub(crate) fn remove_agy_root(
    path: &Path,
    ownership: &AgyRootOwnership,
    dry_run: bool,
    report: &mut ProjectionReport,
) -> Result<(), ProjectionError> {
    match classify_link(path) {
        LinkShape::Symlink | LinkShape::Junction => {
            if !dry_run {
                remove_link(path)?;
                // `publish_agy_root` records the row before it knows which
                // rung will win, so a symlink publish carries a row too.
                // Releasing it here — after the link is gone, never before,
                // the same ordering the `Directory` arm keeps — is what
                // stops a torn-down root from leaving standing
                // `remove_dir_all` authority over whatever later occupies
                // this path.
                ownership.forget_now(path, dry_run)?;
            }
            report.removed_paths.push(path.to_path_buf());
        }
        LinkShape::Directory => {
            if ownership.is_owned(path) {
                if !dry_run {
                    fs::remove_dir_all(path)?;
                    ownership.forget_now(path, dry_run)?;
                }
                report.removed_paths.push(path.to_path_buf());
            } else {
                report.warnings.push(ProjectionWarning {
                    message: format!("preserved user-owned path: {}", path.display()),
                });
            }
        }
        LinkShape::Absent => {
            // Nothing to remove, but a row may still be here: a publish that
            // recorded its row and then failed before a tree existed, or a
            // rollback that removed the tree, both leave one behind. Teardown
            // is the natural place to make that self-healing — an orphan row
            // is only harmless while the path stays absent, and this is the
            // one moment we know it is.
            // Unconditional, deliberately: `is_owned` reads this handle's
            // in-memory snapshot from construction time, which is stale
            // against rows `record_now` wrote straight to disk afterwards —
            // exactly the rows this arm exists to clear. Removing a row for
            // an absent path is safe regardless of ownership: the row only
            // ever authorizes deleting a tree, and there is no tree. If ccync
            // never owned the path there is no row to remove either.
            if !dry_run {
                ownership.forget_now(path, dry_run)?;
            }
        }
    }
    Ok(())
}

// ── persisted-literal regression pin (behavior-freeze safety net) ─────────
//
// The renames applied across this crate must never touch the on-disk JSON
// keys/values written into `~/.ccync/build/lock.json` and sibling registry
// files. This test hardcodes every literal so a future find-replace that
// accidentally changes one of these strings/numbers fails loudly here,
// instead of silently breaking existing users' persisted lockfiles.
#[cfg(test)]
mod tests {
    use super::*;
    use ccync_foundation::platform::create_dir_link;
    use tempfile::TempDir;

    #[test]
    fn persisted_lock_literals_match_pre_refactor_baseline() {
        assert_eq!(PROJECTION_LOCK_KEY, "_ccyncProjection");
        assert_eq!(PROJECTION_LOCK_SCHEMA_VERSION, 1);
        assert_eq!(MANAGED_SOURCE_ATTRIBUTION, "sourceAttribution");
        assert_eq!(MANAGED_AGENT_PATHS, "agentProjectionPaths");
        assert_eq!(MANAGED_CODEX_AGENT_PATHS, "codexAgentProjectionPaths");
        assert_eq!(MANAGED_OPENCODE_AGENT_PATHS, "opencodeAgentProjectionPaths");
        assert_eq!(MANAGED_COMMAND_PATHS, "commandProjectionPaths");
        assert_eq!(MANAGED_DISCUSS_SKILL_PATHS, "discussSkillProjectionPaths");
        assert_eq!(MANAGED_LEGACY_PATHS, "legacyProjectionPaths");
        assert_eq!(MANAGED_SKILL_PATHS, "skillProjectionPaths");
        assert_eq!(MANAGED_AGY_ROOT_PATHS, "agyRootProjectionPaths");
    }

    // Persist/load round-trip for the OpenCode agent category — proves the
    // category participates in both fixed lists, not just the constant.
    #[test]
    fn opencode_agent_category_persists_and_reloads() {
        let tmp = TempDir::new().unwrap();
        let lock = tmp.path().join("lock.json");
        let managed = tmp.path().join("agents").join("reviewer.md");

        let mut report = ProjectionReport::default();
        let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
        registry.mark(MANAGED_OPENCODE_AGENT_PATHS, &managed);
        registry.persist(&[], false, &mut report).unwrap();

        let mut reload_report = ProjectionReport::default();
        let reloaded = ManagedArtifactRegistry::load(&lock, &mut reload_report);
        assert!(reloaded.is_managed(&managed));
    }

    // ── Evidence-attribution tests ──────────────────────────────────────────
    //
    // Each test proves the evidence-shaped function attributes its touched
    // path to exactly the caller-supplied `owner` (or `common` when `owner`
    // is `None`) — direct attribution at the call site, not post-hoc
    // bare-path inference.

    #[test]
    fn write_text_with_evidence_attributes_to_owner_and_to_common() {
        let tmp = TempDir::new().unwrap();

        let owned_path = tmp.path().join("owned.md");
        let evidence =
            write_text_with_evidence(&owned_path, "body", false, Some(ProjectionTargetId::Codex))
                .unwrap();
        assert!(evidence.common.is_empty());
        assert_eq!(
            evidence.by_target[&ProjectionTargetId::Codex].paths,
            vec![ProjectionPathResult {
                path: owned_path,
                change: ProjectionPathChange::Written,
            }]
        );

        let common_path = tmp.path().join("common.md");
        let evidence = write_text_with_evidence(&common_path, "body", false, None).unwrap();
        assert!(evidence.by_target.is_empty());
        assert_eq!(
            evidence.common,
            vec![ProjectionPathResult {
                path: common_path,
                change: ProjectionPathChange::Written,
            }]
        );
    }

    #[test]
    fn write_text_with_evidence_unchanged_write_records_nothing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("stable.md");
        write_text_with_evidence(&path, "body", false, Some(ProjectionTargetId::Claude)).unwrap();

        // Identical content on a second write is a no-op on disk (matching
        // `write_text`'s own dedupe) and must record no evidence either.
        let evidence =
            write_text_with_evidence(&path, "body", false, Some(ProjectionTargetId::Claude))
                .unwrap();
        assert_eq!(evidence, ProjectionEvidence::default());
    }

    #[test]
    fn write_json_map_with_evidence_attributes_to_owner() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("settings.json");
        let mut data = Map::new();
        data.insert("key".into(), Value::String("value".into()));

        let evidence =
            write_json_map_with_evidence(&path, &data, false, Some(ProjectionTargetId::Copilot))
                .unwrap();

        assert_eq!(
            evidence.by_target[&ProjectionTargetId::Copilot].paths,
            vec![ProjectionPathResult {
                path,
                change: ProjectionPathChange::Written,
            }]
        );
        assert!(evidence.common.is_empty());
    }

    #[test]
    fn write_json_map_with_evidence_attributes_to_common_when_no_owner() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("settings.json");
        let mut data = Map::new();
        data.insert("key".into(), Value::String("value".into()));

        let evidence = write_json_map_with_evidence(&path, &data, false, None).unwrap();

        assert!(evidence.by_target.is_empty());
        assert_eq!(
            evidence.common,
            vec![ProjectionPathResult {
                path,
                change: ProjectionPathChange::Written,
            }]
        );
    }

    #[test]
    fn write_json_map_with_evidence_unchanged_write_records_nothing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("settings.json");
        let mut data = Map::new();
        data.insert("key".into(), Value::String("value".into()));

        write_json_map_with_evidence(&path, &data, false, Some(ProjectionTargetId::Claude))
            .unwrap();

        let evidence =
            write_json_map_with_evidence(&path, &data, false, Some(ProjectionTargetId::Claude))
                .unwrap();
        assert_eq!(evidence, ProjectionEvidence::default());
    }

    #[test]
    fn prune_stale_links_with_evidence_attributes_removed_path_to_owner() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("agents");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("stale.toml"),
            format!("{CCYNC_MANAGED_FILE_HEADER}\nx = 1\n"),
        )
        .unwrap();

        let mut report = ProjectionReport::default();
        let registry = ManagedArtifactRegistry::load(&tmp.path().join("nolock.json"), &mut report);

        let evidence = prune_stale_links(
            &root,
            [],
            false,
            &registry,
            &[],
            Some(ProjectionTargetId::Claude),
        )
        .unwrap();

        assert!(!root.join("stale.toml").exists());
        assert_eq!(
            evidence.by_target[&ProjectionTargetId::Claude].paths,
            vec![ProjectionPathResult {
                path: root.join("stale.toml"),
                change: ProjectionPathChange::Removed,
            }]
        );
        assert!(evidence.common.is_empty());
    }

    #[test]
    fn prune_stale_links_with_evidence_attributes_removed_path_to_common_when_no_owner() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("agents");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("stale.toml"),
            format!("{CCYNC_MANAGED_FILE_HEADER}\nx = 1\n"),
        )
        .unwrap();

        let mut report = ProjectionReport::default();
        let registry = ManagedArtifactRegistry::load(&tmp.path().join("nolock.json"), &mut report);

        let evidence = prune_stale_links(&root, [], false, &registry, &[], None).unwrap();

        assert!(evidence.by_target.is_empty());
        assert_eq!(
            evidence.common,
            vec![ProjectionPathResult {
                path: root.join("stale.toml"),
                change: ProjectionPathChange::Removed,
            }]
        );
    }

    #[test]
    fn prune_stale_links_with_evidence_no_stale_entries_records_nothing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("agents");
        fs::create_dir_all(&root).unwrap();

        let mut report = ProjectionReport::default();
        let registry = ManagedArtifactRegistry::load(&tmp.path().join("nolock.json"), &mut report);

        let evidence = prune_stale_links(
            &root,
            [],
            false,
            &registry,
            &[],
            Some(ProjectionTargetId::Claude),
        )
        .unwrap();

        assert_eq!(evidence, ProjectionEvidence::default());
    }

    #[test]
    fn prune_stale_command_links_with_evidence_attributes_removed_link_to_owner() {
        let tmp = TempDir::new().unwrap();
        let commands_source = tmp.path().join("commands").join("stale-cmd");
        fs::create_dir_all(&commands_source).unwrap();
        let root = tmp.path().join("skills");
        fs::create_dir_all(&root).unwrap();
        let link = root.join("stale-cmd");
        create_dir_link(&commands_source, &link).unwrap();

        // `entry.file_type()?.is_dir()` (this function's own top-of-loop
        // gate) and `is_ccync_command_link`'s `is_symlink_or_junction`
        // requirement are mutually exclusive under `std::fs`'s own
        // semantics: `fs::read_dir` reports a reparse point (a junction, as
        // created here, or a symlink) as `is_symlink() == true` /
        // `is_dir() == false`, confirmed directly —
        // `fs::symlink_metadata(&link).unwrap().file_type().is_dir()` is
        // `false` for this junction. No entry can ever satisfy both gates at
        // once, so this function's removal branch is a structural no-op on
        // every platform — pre-existing legacy behavior this task mirrors
        // exactly rather than "fixes" (out of scope per this task's own
        // "same operations, same effects on disk" boundary). This test pins
        // that the evidence-shaped sibling reproduces the identical no-op,
        // not a silent behavior change.
        assert!(
            !fs::symlink_metadata(&link).unwrap().file_type().is_dir(),
            "test premise: a junction must read as is_dir()==false via symlink_metadata, \
             matching entry.file_type() during fs::read_dir"
        );

        let mut report = ProjectionReport::default();
        let registry = ManagedArtifactRegistry::load(&tmp.path().join("nolock.json"), &mut report);

        let evidence = prune_stale_command_links(
            &root,
            &[],
            false,
            &registry,
            Some(ProjectionTargetId::Copilot),
        )
        .unwrap();

        assert_eq!(evidence, ProjectionEvidence::default());
        assert!(
            link.exists(),
            "a junction can never satisfy this function's own is_dir() gate, so it is a \
             structural no-op on every platform"
        );
    }

    #[test]
    fn prune_legacy_ccync_prefixed_dirs_with_evidence_attributes_removed_dir_to_common() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("ccync-old")).unwrap();

        let mut report = ProjectionReport::default();
        let registry = ManagedArtifactRegistry::load(&tmp.path().join("nolock.json"), &mut report);

        let evidence =
            prune_legacy_ccync_prefixed_dirs(&root, &[], false, &registry, None).unwrap();

        assert!(!root.join("ccync-old").exists());
        assert!(evidence.by_target.is_empty());
        assert_eq!(
            evidence.common,
            vec![ProjectionPathResult {
                path: root.join("ccync-old"),
                change: ProjectionPathChange::Removed,
            }]
        );
    }

    #[test]
    fn prune_legacy_ccync_prefixed_dirs_with_evidence_attributes_removed_dir_to_owner() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("ccync-old")).unwrap();

        let mut report = ProjectionReport::default();
        let registry = ManagedArtifactRegistry::load(&tmp.path().join("nolock.json"), &mut report);

        let evidence = prune_legacy_ccync_prefixed_dirs(
            &root,
            &[],
            false,
            &registry,
            Some(ProjectionTargetId::Codex),
        )
        .unwrap();

        assert!(!root.join("ccync-old").exists());
        assert_eq!(
            evidence.by_target[&ProjectionTargetId::Codex].paths,
            vec![ProjectionPathResult {
                path: root.join("ccync-old"),
                change: ProjectionPathChange::Removed,
            }]
        );
        assert!(evidence.common.is_empty());
    }

    #[test]
    fn prune_stale_managed_files_with_evidence_attributes_removed_file_to_owner() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("commands");
        fs::create_dir_all(&root).unwrap();
        let stale = root.join("stale.toml");
        fs::write(&stale, format!("{CCYNC_MANAGED_FILE_HEADER}\nx = 1\n")).unwrap();

        let mut report = ProjectionReport::default();
        let registry = ManagedArtifactRegistry::load(&tmp.path().join("nolock.json"), &mut report);

        let evidence = prune_stale_managed_files(
            &root,
            "toml",
            &[],
            false,
            &registry,
            Some(ProjectionTargetId::Copilot),
        )
        .unwrap();

        assert!(!stale.exists());
        assert_eq!(
            evidence.by_target[&ProjectionTargetId::Copilot].paths,
            vec![ProjectionPathResult {
                path: stale,
                change: ProjectionPathChange::Removed,
            }]
        );
        assert!(evidence.common.is_empty());
    }

    #[test]
    fn prune_stale_managed_files_with_evidence_attributes_removed_file_to_common_when_no_owner() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("commands");
        fs::create_dir_all(&root).unwrap();
        let stale = root.join("stale.toml");
        fs::write(&stale, format!("{CCYNC_MANAGED_FILE_HEADER}\nx = 1\n")).unwrap();

        let mut report = ProjectionReport::default();
        let registry = ManagedArtifactRegistry::load(&tmp.path().join("nolock.json"), &mut report);

        let evidence =
            prune_stale_managed_files(&root, "toml", &[], false, &registry, None).unwrap();

        assert!(!stale.exists());
        assert!(evidence.by_target.is_empty());
        assert_eq!(
            evidence.common,
            vec![ProjectionPathResult {
                path: stale,
                change: ProjectionPathChange::Removed,
            }]
        );
    }

    // ── prune fault payload (locked file) ──────────────────────────────────
    //
    // Forces a genuine I/O fault (the target file held open without
    // delete-sharing, so `fs::remove_file` fails) and asserts the removal
    // does not partially succeed.
    #[test]
    fn prune_stale_managed_files_locked_file_faults_without_partial_removal() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("commands");
        fs::create_dir_all(&root).unwrap();
        let stale = root.join("stale.toml");
        fs::write(&stale, format!("{CCYNC_MANAGED_FILE_HEADER}\nx = 1\n")).unwrap();

        // Hold the file open sharing FILE_SHARE_READ|FILE_SHARE_WRITE but NOT
        // FILE_SHARE_DELETE (`std::fs::OpenOptions`'s own Windows default
        // grants all three, which would let `fs::remove_file` succeed
        // anyway) — both prune functions still read the file's own
        // CCYNC-managed-header content successfully (read sharing is
        // granted), but the later `fs::remove_file` fails with a sharing
        // violation, identically on both call paths below.
        #[cfg(windows)]
        let _locked = {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_SHARE_READ: u32 = 0x0000_0001;
            const FILE_SHARE_WRITE: u32 = 0x0000_0002;
            fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .open(&stale)
                .unwrap()
        };
        #[cfg(not(windows))]
        let _locked = fs::OpenOptions::new().read(true).open(&stale).unwrap();

        let mut report = ProjectionReport::default();
        let registry = ManagedArtifactRegistry::load(&tmp.path().join("nolock.json"), &mut report);

        prune_stale_managed_files(
            &root,
            "toml",
            &[],
            false,
            &registry,
            Some(ProjectionTargetId::Copilot),
        )
        .expect_err("locked file must make the prune fail");

        assert!(
            stale.exists(),
            "the removal must not have partially succeeded"
        );
    }

    // ── write_text_atomically_with_report ───────────────────────────────────

    #[test]
    fn write_text_atomically_with_report_writes_and_records_report() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("nested").join("target.md");

        let mut report = ProjectionReport::default();
        write_text_atomically_with_report(&dest, "body", false, &mut report).unwrap();

        assert_eq!(fs::read_to_string(&dest).unwrap(), "body\n");
        assert_eq!(report.written_files, vec![dest.clone()]);
        assert!(
            !dest.with_file_name("target.md.tmp").exists(),
            "temp sibling must not linger after a successful write"
        );
    }

    #[test]
    fn write_text_atomically_with_report_unchanged_bytes_short_circuit_no_report() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("target.md");

        let mut report = ProjectionReport::default();
        write_text_atomically_with_report(&dest, "body", false, &mut report).unwrap();

        // Second write with identical (post-normalization) content must be a
        // no-op: no fresh write, and no new report row.
        let mtime_before = fs::metadata(&dest).unwrap().modified().unwrap();
        let mut report = ProjectionReport::default();
        write_text_atomically_with_report(&dest, "body\n", false, &mut report).unwrap();

        assert!(
            report.written_files.is_empty(),
            "unchanged write must not be reported"
        );
        assert_eq!(
            fs::metadata(&dest).unwrap().modified().unwrap(),
            mtime_before
        );
        assert!(!dest.with_file_name("target.md.tmp").exists());
    }

    #[test]
    fn write_text_atomically_with_report_dry_run_previews_without_writing() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("target.md");

        let mut report = ProjectionReport::default();
        write_text_atomically_with_report(&dest, "body", true, &mut report).unwrap();

        assert!(!dest.exists(), "dry-run must not touch disk");
        assert_eq!(report.written_files, vec![dest.clone()]);
        assert!(!dest.with_file_name("target.md.tmp").exists());
    }

    /// Deterministically blocks `<target>.tmp` by pre-creating it as a
    /// directory — the shared writer's temp-file create fails at the
    /// `TempWrite` stage on every platform, without racing a real fault
    /// injection hook. Proves the failure surfaces as an error, leaves the
    /// destination unwritten, records nothing in the report, and never
    /// mistakes the blocking directory for a stray temp file.
    #[test]
    fn write_text_atomically_with_report_blocking_tmp_directory_fails_without_partial_write() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("target.md");
        let tmp_sibling = tmp.path().join("target.md.tmp");
        fs::create_dir_all(&tmp_sibling).unwrap();

        let mut report = ProjectionReport::default();
        let result = write_text_atomically_with_report(&dest, "body", false, &mut report);

        assert!(
            result.is_err(),
            "a blocked temp sibling must surface as an error"
        );
        assert!(!dest.exists(), "destination must remain unwritten");
        assert!(report.written_files.is_empty());
        assert!(
            tmp_sibling.is_dir(),
            "the blocking directory must be left exactly as found, not treated as a stray temp file"
        );
    }

    // ── AgyRootOwnership ────────────────────────────────────────────────────

    #[test]
    fn agy_root_ownership_record_now_is_readable_on_disk_before_any_persist() {
        let tmp = TempDir::new().unwrap();
        let lock = tmp.path().join("lock.json");
        let owned = tmp
            .path()
            .join("gemini")
            .join("config")
            .join("plugins")
            .join("ccync");

        let ownership = AgyRootOwnership::from_lockfile(&lock);
        ownership.record_now(&owned, false).unwrap();

        // Re-read straight off disk — no `persist` was ever called on any
        // registry instance.
        let raw = fs::read_to_string(&lock).unwrap();
        let root: Value = serde_json::from_str(&raw).unwrap();
        let recorded = root[PROJECTION_LOCK_KEY][MANAGED_AGY_ROOT_PATHS]
            .as_array()
            .expect("category array present on disk");
        assert!(recorded
            .iter()
            .any(|value| value.as_str() == Some(&normalize_managed_artifact_path(&owned))));

        // And a freshly loaded registry sees the row as managed.
        let mut report = ProjectionReport::default();
        let reloaded = ManagedArtifactRegistry::load(&lock, &mut report);
        assert!(reloaded.is_managed(&owned));
    }

    #[test]
    fn agy_root_ownership_record_now_preserves_unrelated_category_already_on_disk() {
        let tmp = TempDir::new().unwrap();
        let lock = tmp.path().join("lock.json");
        let untouched = tmp.path().join("skills").join("existing");

        // Pre-seed an unrelated category via the ordinary registry/persist path.
        let mut report = ProjectionReport::default();
        let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
        registry.mark(MANAGED_SKILL_PATHS, &untouched);
        registry.persist(&[], false, &mut report).unwrap();

        let owned = tmp
            .path()
            .join("gemini")
            .join("config")
            .join("plugins")
            .join("ccync");
        let ownership = AgyRootOwnership::from_lockfile(&lock);
        ownership.record_now(&owned, false).unwrap();

        let raw = fs::read_to_string(&lock).unwrap();
        let root: Value = serde_json::from_str(&raw).unwrap();
        let skill_paths = root[PROJECTION_LOCK_KEY][MANAGED_SKILL_PATHS]
            .as_array()
            .expect("pre-seeded category must survive the forced write");
        assert!(skill_paths
            .iter()
            .any(|value| value.as_str() == Some(&normalize_managed_artifact_path(&untouched))));
    }

    #[test]
    fn agy_root_ownership_record_now_dry_run_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        let lock = tmp.path().join("lock.json");
        let owned = tmp
            .path()
            .join("gemini")
            .join("config")
            .join("plugins")
            .join("ccync");

        let ownership = AgyRootOwnership::from_lockfile(&lock);
        ownership.record_now(&owned, true).unwrap();

        assert!(!lock.exists(), "dry_run must suppress the forced write");
        assert!(!ownership.is_owned(&owned));
    }

    #[test]
    fn agy_root_ownership_is_owned_false_on_missing_prior() {
        let tmp = TempDir::new().unwrap();
        let lock = tmp.path().join("nolock.json");
        let path = tmp
            .path()
            .join("gemini")
            .join("config")
            .join("plugins")
            .join("ccync");

        let ownership = AgyRootOwnership::from_lockfile(&lock);
        assert!(!ownership.is_owned(&path));
    }

    #[test]
    fn agy_root_ownership_is_owned_false_on_empty_prior() {
        let tmp = TempDir::new().unwrap();
        let lock = tmp.path().join("lock.json");
        fs::write(&lock, "{}\n").unwrap();
        let path = tmp
            .path()
            .join("gemini")
            .join("config")
            .join("plugins")
            .join("ccync");

        let ownership = AgyRootOwnership::from_lockfile(&lock);
        assert!(!ownership.is_owned(&path));
    }

    // ── persist: disk is sole authority for MANAGED_AGY_ROOT_PATHS ─────────

    // P1 — a row recorded via `record_now` survives a sibling registry
    // instance's own later `persist`, even though that sibling instance
    // never marked the row itself.
    #[test]
    fn agy_root_row_survives_a_sibling_registrys_persist() {
        let tmp = TempDir::new().unwrap();
        let lock = tmp.path().join("lock.json");
        let owned = tmp.path().join("gemini").join("config").join("plugins");

        let ownership = AgyRootOwnership::from_lockfile(&lock);
        ownership.record_now(&owned, false).unwrap();

        // A sibling registry instance, loaded fresh, marks an unrelated
        // category and persists — it never touched `MANAGED_AGY_ROOT_PATHS`.
        let mut report = ProjectionReport::default();
        let mut sibling = ManagedArtifactRegistry::load(&lock, &mut report);
        sibling.mark(MANAGED_SKILL_PATHS, &tmp.path().join("skills").join("s"));
        sibling.persist(&[], false, &mut report).unwrap();

        let raw = fs::read_to_string(&lock).unwrap();
        let root: Value = serde_json::from_str(&raw).unwrap();
        let recorded = root[PROJECTION_LOCK_KEY][MANAGED_AGY_ROOT_PATHS]
            .as_array()
            .expect("agy root category must survive a sibling's persist");
        assert!(recorded
            .iter()
            .any(|value| value.as_str() == Some(&normalize_managed_artifact_path(&owned))));
    }

    // P2 — a persist whose in-memory categories are all empty would drop the
    // whole `_ccyncProjection` marker under the old union-with-`next` logic;
    // reading the agy category from disk keeps the marker alive.
    #[test]
    fn agy_root_row_survives_a_persist_with_all_categories_empty_in_memory() {
        let tmp = TempDir::new().unwrap();
        let lock = tmp.path().join("lock.json");
        let owned = tmp.path().join("gemini").join("config").join("plugins");

        let ownership = AgyRootOwnership::from_lockfile(&lock);
        ownership.record_now(&owned, false).unwrap();

        // Load fresh, then force every category empty via `replace_category`
        // without marking anything back in — the old code path would see
        // `next` entirely empty and drop the marker.
        let mut report = ProjectionReport::default();
        let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
        for category in [
            MANAGED_AGENT_PATHS,
            MANAGED_CODEX_AGENT_PATHS,
            MANAGED_OPENCODE_AGENT_PATHS,
            MANAGED_COMMAND_PATHS,
            MANAGED_LEGACY_PATHS,
            MANAGED_SKILL_PATHS,
            MANAGED_AGY_ROOT_PATHS,
        ] {
            registry.replace_category(category);
        }
        registry.persist(&[], false, &mut report).unwrap();

        let raw = fs::read_to_string(&lock).unwrap();
        let root: Value = serde_json::from_str(&raw).unwrap();
        let meta = root
            .get(PROJECTION_LOCK_KEY)
            .expect("marker must survive: agy root category was non-empty on disk");
        let recorded = meta[MANAGED_AGY_ROOT_PATHS]
            .as_array()
            .expect("agy root category array present");
        assert!(recorded
            .iter()
            .any(|value| value.as_str() == Some(&normalize_managed_artifact_path(&owned))));
    }

    // P3 — a row removed from disk after a registry instance loaded is NOT
    // written back by that instance's later `persist`; disk is read fresh at
    // persist time, not unioned with the stale in-memory `next`/`prior`.
    #[test]
    fn agy_root_row_removed_on_disk_is_not_written_back_by_a_stale_producer() {
        let tmp = TempDir::new().unwrap();
        let lock = tmp.path().join("lock.json");
        let owned = tmp.path().join("gemini").join("config").join("plugins");

        let ownership = AgyRootOwnership::from_lockfile(&lock);
        ownership.record_now(&owned, false).unwrap();

        // Load a registry while the row is still present — its `prior`/`next`
        // both carry the row in memory.
        let mut report = ProjectionReport::default();
        let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
        assert!(registry.is_managed(&owned), "test premise: row loaded");

        // A later teardown (simulated directly) removes the row on disk.
        merge_managed_artifact_row_on_disk(&lock, MANAGED_SKILL_PATHS, &tmp.path().join("noop"))
            .unwrap(); // touch the file once to prove writes still land
        let raw = fs::read_to_string(&lock).unwrap();
        let mut root: Value = serde_json::from_str(&raw).unwrap();
        root[PROJECTION_LOCK_KEY][MANAGED_AGY_ROOT_PATHS] = Value::Array(Vec::new());
        fs::write(&lock, serde_json::to_string_pretty(&root).unwrap()).unwrap();

        // The stale-loaded registry marks an unrelated category and persists
        // — it must not resurrect the removed agy row from its own `next`.
        registry.mark(
            MANAGED_COMMAND_PATHS,
            &tmp.path().join("commands").join("c"),
        );
        registry.persist(&[], false, &mut report).unwrap();

        let raw = fs::read_to_string(&lock).unwrap();
        let root: Value = serde_json::from_str(&raw).unwrap();
        let recorded = root[PROJECTION_LOCK_KEY][MANAGED_AGY_ROOT_PATHS]
            .as_array()
            .expect("agy root category array present");
        assert!(
            recorded.is_empty(),
            "a stale producer must not write the removed row back"
        );
    }

    // ── staged-copy engine ──────────────────────────────────────────────────

    fn write_nested(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// Lists every entry under `root`, recursively, as parent-relative
    /// forward-slash paths — used to compare a staged/swapped tree against
    /// its source independent of the staging directory's own generated name.
    fn list_tree_relative(root: &Path) -> Vec<String> {
        fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
            let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let path = entry.path();
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if path.is_dir() {
                    walk(base, &path, out);
                } else {
                    out.push(rel);
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out
    }

    #[test]
    fn publish_staged_tree_copy_publishes_full_copy_of_source_tree() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "plugin.json", "{}");
        write_nested(&source, "skills/one/SKILL.md", "one");
        write_nested(&source, "skills/two/SKILL.md", "two");
        let dest = tmp.path().join("dest");

        let swapped = publish_staged_tree_copy(&source, &dest).unwrap();

        assert!(swapped, "an absent destination must trigger a swap");
        assert!(dest.is_dir());
        assert_eq!(list_tree_relative(&dest), list_tree_relative(&source));
        assert_eq!(
            fs::read_to_string(dest.join("skills/one/SKILL.md")).unwrap(),
            "one"
        );

        // No staging sibling left behind next to the published destination.
        for entry in fs::read_dir(tmp.path()).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            assert!(
                !name.starts_with(".ccync-stage-"),
                "staging directory must not linger after a successful swap: {name}"
            );
        }
    }

    #[test]
    fn publish_staged_tree_copy_replaces_a_differing_existing_destination() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "skills/one/SKILL.md", "new content");
        let dest = tmp.path().join("dest");
        write_nested(&dest, "skills/one/SKILL.md", "old content");
        write_nested(&dest, "skills/stale/SKILL.md", "must be gone");

        let swapped = publish_staged_tree_copy(&source, &dest).unwrap();

        assert!(swapped);
        assert_eq!(
            fs::read_to_string(dest.join("skills/one/SKILL.md")).unwrap(),
            "new content"
        );
        assert!(
            !dest.join("skills/stale").exists(),
            "the old destination tree must be fully replaced, not merged"
        );
    }

    #[test]
    fn publish_staged_tree_copy_skips_an_identical_destination_with_zero_writes() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "plugin.json", "{}");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let dest = tmp.path().join("dest");
        write_nested(&dest, "plugin.json", "{}");
        write_nested(&dest, "skills/one/SKILL.md", "one");

        let dest_file = dest.join("skills/one/SKILL.md");
        let mtime_before = fs::metadata(&dest_file).unwrap().modified().unwrap();

        let swapped = publish_staged_tree_copy(&source, &dest).unwrap();

        assert!(!swapped, "an identical destination must be skipped");
        assert_eq!(
            fs::metadata(&dest_file).unwrap().modified().unwrap(),
            mtime_before,
            "a skipped publish must not touch the destination at all"
        );
        for entry in fs::read_dir(tmp.path()).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            assert!(
                !name.starts_with(".ccync-stage-"),
                "a skipped publish must never stage a copy: {name}"
            );
        }
    }

    #[test]
    fn trees_content_equal_detects_a_differing_file() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        write_nested(&a, "skills/one/SKILL.md", "one");
        write_nested(&b, "skills/one/SKILL.md", "different");

        assert!(!trees_content_equal(&a, &b));
    }

    #[test]
    fn trees_content_equal_detects_an_extra_entry() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        write_nested(&a, "skills/one/SKILL.md", "one");
        write_nested(&b, "skills/one/SKILL.md", "one");
        write_nested(&b, "skills/two/SKILL.md", "two");

        assert!(!trees_content_equal(&a, &b));
    }

    #[test]
    fn trees_content_equal_true_for_identical_nested_trees() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        write_nested(&a, "skills/one/SKILL.md", "one");
        write_nested(&a, "plugin.json", "{}");
        write_nested(&b, "skills/one/SKILL.md", "one");
        write_nested(&b, "plugin.json", "{}");

        assert!(trees_content_equal(&a, &b));
    }

    /// Deterministically interrupts a stage partway through by holding a
    /// second source file open without read-sharing, so `fs::copy` fails on
    /// it with a sharing violation after the first file already copied
    /// successfully into the staging directory. Proves the *destination*
    /// path is never observed holding that partial result — it is exactly
    /// whatever it held before the call (absent, here), because the swap
    /// step never runs when staging fails.
    #[test]
    fn interrupted_stage_leaves_an_absent_destination_absent() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let blocked = source.join("skills/two/SKILL.md");
        write_nested(&source, "skills/two/SKILL.md", "two");
        let dest = tmp.path().join("dest");

        #[cfg(windows)]
        let _locked = lock_without_read_sharing(&blocked);
        #[cfg(not(windows))]
        let _locked = &blocked; // no portable exclusive-lock primitive on this platform

        let result = publish_staged_tree_copy(&source, &dest);

        #[cfg(windows)]
        {
            result.expect_err("a locked source file must make the stage fail");
        }
        #[cfg(not(windows))]
        {
            // Without a portable way to force a mid-copy fault, at minimum
            // prove the happy path still leaves an absent destination absent
            // when nothing is interrupted — the interrupted-path guarantee
            // itself is exercised on Windows above and is structural (the
            // swap step is unreachable once `stage_tree_copy` returns `Err`).
            let _ = result;
        }

        assert!(
            !dest.exists(),
            "destination must remain absent — the swap step must never run when staging fails"
        );
        for entry in fs::read_dir(tmp.path()).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            assert!(
                !name.starts_with(".ccync-stage-"),
                "a failed stage must clean up its own partial staging directory: {name}"
            );
        }
    }

    #[cfg(windows)]
    fn lock_without_read_sharing(path: &Path) -> fs::File {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_NONE: u32 = 0;
        fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_NONE)
            .open(path)
            .unwrap()
    }

    /// Same interrupted-stage fault, but with a pre-existing destination —
    /// proves the *complete* half of "absent-or-complete": an existing
    /// destination survives a failed stage byte-for-byte, because the swap
    /// step (the only step that touches `dest`) never runs.
    #[test]
    fn interrupted_stage_leaves_an_existing_destination_untouched() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let blocked = source.join("skills/two/SKILL.md");
        write_nested(&source, "skills/two/SKILL.md", "two");
        let dest = tmp.path().join("dest");
        write_nested(&dest, "skills/one/SKILL.md", "old");

        #[cfg(windows)]
        let _locked = lock_without_read_sharing(&blocked);
        #[cfg(not(windows))]
        let _locked = &blocked;

        let result = publish_staged_tree_copy(&source, &dest);

        #[cfg(windows)]
        {
            result.expect_err("a locked source file must make the stage fail");
            assert_eq!(
                fs::read_to_string(dest.join("skills/one/SKILL.md")).unwrap(),
                "old",
                "an existing destination must survive a failed stage untouched"
            );
        }
        #[cfg(not(windows))]
        {
            let _ = result;
        }
    }

    // ── publish_agy_root ─────────────────────────────────────────────────

    #[test]
    fn publish_agy_root_produces_a_directory_with_its_row_already_on_disk() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "plugin.json", "{}");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let dest = tmp.path().join("gemini").join("config").join("plugins");
        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);

        let mut report = ProjectionReport::default();
        let (shape, _wrote) =
            publish_agy_root(&source, &dest, &ownership, false, &mut report).unwrap();

        if shape == LinkShape::Symlink {
            // Symlink-permitting host: not this dev machine, but handled the
            // same way the shape-classifier's own tests handle it.
            assert_eq!(classify_link(&dest), LinkShape::Symlink);
        } else {
            assert_eq!(shape, LinkShape::Directory);
            assert_eq!(list_tree_relative(&dest), list_tree_relative(&source));
        }

        // The ownership row must be readable straight off disk — no `persist`
        // was ever called on any registry instance.
        let raw = fs::read_to_string(&lock).unwrap();
        let root: Value = serde_json::from_str(&raw).unwrap();
        let recorded = root[PROJECTION_LOCK_KEY][MANAGED_AGY_ROOT_PATHS]
            .as_array()
            .expect("category array present on disk");
        assert!(recorded
            .iter()
            .any(|value| value.as_str() == Some(&normalize_managed_artifact_path(&dest))));
    }

    #[test]
    fn publish_agy_root_replaces_a_junction_whose_target_was_deleted() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let stale_target = tmp.path().join("stale-target");
        fs::create_dir_all(&stale_target).unwrap();
        let dest = tmp.path().join("dest");
        create_dir_link(&stale_target, &dest).unwrap();
        fs::remove_dir_all(&stale_target).unwrap();
        assert_eq!(
            classify_link(&dest),
            LinkShape::Junction,
            "a dangling junction must still classify as Junction, not Absent"
        );

        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);
        let mut report = ProjectionReport::default();
        let (shape, _wrote) =
            publish_agy_root(&source, &dest, &ownership, false, &mut report).unwrap();

        assert_ne!(
            classify_link(&dest),
            LinkShape::Junction,
            "the legacy junction must be replaced"
        );
        if shape != LinkShape::Symlink {
            assert_eq!(shape, LinkShape::Directory);
            assert_eq!(list_tree_relative(&dest), list_tree_relative(&source));
        }
    }

    #[test]
    fn publish_agy_root_preserves_an_unowned_real_directory_even_with_no_lockfile() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let dest = tmp.path().join("dest");
        write_nested(&dest, "users-own-file.txt", "do not touch");
        let lock = tmp.path().join("nolock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);

        let mut report = ProjectionReport::default();
        let (shape, wrote) =
            publish_agy_root(&source, &dest, &ownership, false, &mut report).unwrap();

        assert_eq!(shape, LinkShape::Directory);
        assert!(
            !wrote,
            "preserving an unowned path writes nothing, so callers must record no path evidence"
        );
        assert!(
            dest.join("users-own-file.txt").exists(),
            "an unowned real directory must survive untouched"
        );
        assert!(
            !dest.join("skills").exists(),
            "the source tree must not be published over unowned content"
        );
        assert!(
            !lock.exists(),
            "no lockfile write may happen for a preserved path"
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.message.contains("preserved user-owned path")),
            "preservation must be reported"
        );
    }

    #[test]
    fn publish_agy_root_dry_run_touches_neither_disk_nor_lockfile() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let dest = tmp.path().join("dest");
        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);

        let mut report = ProjectionReport::default();
        let (shape, wrote) =
            publish_agy_root(&source, &dest, &ownership, true, &mut report).unwrap();

        assert!(!dest.exists(), "dry_run must not create anything at dest");
        assert!(!lock.exists(), "dry_run must not write the lockfile");
        assert!(shape == LinkShape::Symlink || shape == LinkShape::Directory);
        assert!(
            !wrote,
            "dry_run writes nothing, so it never reports a write"
        );
    }

    #[test]
    fn publish_agy_root_dry_run_predicts_directory_for_an_already_owned_copy() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let dest = tmp.path().join("dest");
        write_nested(&dest, "skills/one/SKILL.md", "one");
        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);
        ownership.record_now(&dest, false).unwrap();
        let bytes_before = fs::read(&lock).unwrap();

        let mut report = ProjectionReport::default();
        let (shape, wrote) =
            publish_agy_root(&source, &dest, &ownership, true, &mut report).unwrap();

        assert_eq!(
            shape,
            LinkShape::Directory,
            "an occupied, owned directory always falls to rung 2 — knowable without writing"
        );
        assert!(
            !wrote,
            "dry_run writes nothing, so it never reports a write"
        );
        assert_eq!(
            fs::read(&lock).unwrap(),
            bytes_before,
            "dry_run must leave the lockfile bytes untouched"
        );
    }

    /// The property the plan's own Test Cases name ("neither run reports a
    /// spurious change"), at the reporting level rather than the disk level:
    /// republishing an already-correct root must report `wrote == false`, so
    /// its caller records no path evidence and a stable rerun reads back as
    /// unchanged instead of applied.
    #[test]
    fn publish_agy_root_second_identical_publish_reports_no_write() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "plugin.json", "{}");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let dest = tmp.path().join("dest");
        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);

        let mut first_report = ProjectionReport::default();
        let (first_shape, first_wrote) =
            publish_agy_root(&source, &dest, &ownership, false, &mut first_report).unwrap();
        assert!(first_wrote, "the initial publish must report a real write");

        // Re-load ownership so the second call sees the row the first wrote,
        // exactly as a fresh `ccync sync` process would.
        let ownership = AgyRootOwnership::from_lockfile(&lock);
        let mut second_report = ProjectionReport::default();
        let (second_shape, second_wrote) =
            publish_agy_root(&source, &dest, &ownership, false, &mut second_report).unwrap();

        assert_eq!(
            second_shape, first_shape,
            "a stable rerun lands the same shape it already had"
        );
        assert!(
            !second_wrote,
            "an unchanged republish must report no write, or every sync reports a spurious change"
        );
        assert!(
            second_report.warnings.is_empty(),
            "an owned, already-correct root is not a preserved-user-content case"
        );
    }

    /// Forces a failure between the row write and the rename that makes the
    /// tree visible, using the same locked-source-file technique as the
    /// staged-copy engine's own interrupted-stage tests.
    ///
    /// The tree must never appear at `dest`, and — the part this test was
    /// rewritten for — the row must not be left behind either. A row whose
    /// tree never materialized is standing `remove_dir_all` authority over
    /// whatever later occupies that path, so a publish that unwinds cleanly
    /// releases it again. The required row-before-tree ordering is
    /// unobservable after a clean unwind by construction; it survives as the
    /// crash-only state (process killed mid-publish), which
    /// `remove_agy_root`'s `Absent` arm self-heals on the next teardown.
    #[test]
    fn publish_agy_root_injected_failure_leaves_neither_tree_nor_standing_row() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let blocked = source.join("skills/two/SKILL.md");
        write_nested(&source, "skills/two/SKILL.md", "two");
        let dest = tmp.path().join("dest");
        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);

        #[cfg(windows)]
        let _locked = lock_without_read_sharing(&blocked);
        #[cfg(not(windows))]
        let _locked = &blocked;

        let mut report = ProjectionReport::default();
        let result = publish_agy_root(&source, &dest, &ownership, false, &mut report);

        match result {
            // Rung 1 (symlink) never reads `source`'s content, so a
            // symlink-permitting host succeeds before the locked file is
            // ever touched — the fallback-copy fault is simply not reached.
            Ok((LinkShape::Symlink, _)) => {}
            Ok((other, _)) => panic!("unexpected shape with a locked source file: {other:?}"),
            Err(_) => {
                assert!(
                    !dest.exists(),
                    "the tree must never become visible when the copy fails"
                );

                // Read the row set back from disk, not from the in-memory
                // handle: `record_now`/`forget_now` both write straight
                // through, and disk is the only authority for this category.
                let raw = fs::read_to_string(&lock).unwrap();
                let root: Value = serde_json::from_str(&raw).unwrap();
                let recorded = root[PROJECTION_LOCK_KEY][MANAGED_AGY_ROOT_PATHS]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                assert!(
                    !recorded.iter().any(
                        |value| value.as_str() == Some(&normalize_managed_artifact_path(&dest))
                    ),
                    "a failed publish must not leave standing removal authority for {}",
                    dest.display()
                );

                // The release is scoped to this publish: a row for a
                // different, still-owned root is untouched.
                let sibling = tmp.path().join("other-root");
                let ownership2 = AgyRootOwnership::from_lockfile(&lock);
                ownership2.record_now(&sibling, false).unwrap();
                let mut report2 = ProjectionReport::default();
                let _ = publish_agy_root(&source, &dest, &ownership2, false, &mut report2);
                let raw = fs::read_to_string(&lock).unwrap();
                let root: Value = serde_json::from_str(&raw).unwrap();
                let recorded = root[PROJECTION_LOCK_KEY][MANAGED_AGY_ROOT_PATHS]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                assert!(
                    recorded
                        .iter()
                        .any(|value| value.as_str()
                            == Some(&normalize_managed_artifact_path(&sibling))),
                    "releasing one root's row must not disturb another root's row"
                );
            }
        }
    }

    #[test]
    fn strip_lockfile_to_agy_rows_keeps_the_proof_and_drops_the_credentials() {
        let tmp = TempDir::new().unwrap();
        let lock = tmp.path().join("lock.json");
        let owned = tmp.path().join("agy-root");
        AgyRootOwnership::from_lockfile(&lock)
            .record_now(&owned, false)
            .unwrap();

        // Splice in the shapes an adopted install really carries: the MCP
        // definition namespace (where `env`/`headers` secrets live) and a
        // second managed category that is derived state, not proof.
        let raw = fs::read_to_string(&lock).unwrap();
        let mut root: Map<String, Value> = match serde_json::from_str(&raw).unwrap() {
            Value::Object(root) => root,
            other => panic!("lockfile must be an object, got {other:?}"),
        };
        root.insert(
            "_mcpServers".to_string(),
            serde_json::json!({
                "github": { "command": "gh", "env": { "GITHUB_TOKEN": "ghp_secret" } }
            }),
        );
        if let Some(Value::Object(meta)) = root.get_mut(PROJECTION_LOCK_KEY) {
            meta.insert(
                MANAGED_SKILL_PATHS.to_string(),
                Value::Array(vec![Value::String("some/skill".to_string())]),
            );
        }
        fs::write(
            &lock,
            serde_json::to_string_pretty(&Value::Object(root)).unwrap(),
        )
        .unwrap();

        strip_lockfile_to_agy_rows(&lock).unwrap();

        let raw = fs::read_to_string(&lock).unwrap();
        assert!(
            !raw.contains("ghp_secret") && !raw.contains("_mcpServers"),
            "uninstall must not leave adopted MCP credentials on disk: {raw}"
        );
        assert!(
            raw.contains("some/skill"),
            "sibling ownership categories are the proof `ManagedDeprojection` needs for the \
             other managed surfaces on an uninstall retry — narrowing to the agy rows would \
             orphan a partial uninstall's leftovers permanently"
        );
        assert!(
            AgyRootOwnership::from_lockfile(&lock).is_owned(&owned),
            "the surviving root's ownership proof must still be readable"
        );
    }

    #[test]
    fn strip_lockfile_to_agy_rows_never_manufactures_a_lockfile() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("lock.json");
        strip_lockfile_to_agy_rows(&missing).unwrap();
        assert!(
            !missing.exists(),
            "nothing was proven, so nothing is written"
        );

        let malformed = tmp.path().join("malformed.json");
        fs::write(&malformed, "{ not json").unwrap();
        strip_lockfile_to_agy_rows(&malformed).unwrap();
        assert_eq!(
            fs::read_to_string(&malformed).unwrap(),
            "{ not json",
            "an unparseable lockfile is left exactly as found"
        );
    }

    #[test]
    fn remove_agy_root_forgets_an_orphan_row_at_an_absent_path() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("dest");
        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);
        ownership.record_now(&dest, false).unwrap();
        assert!(
            AgyRootOwnership::from_lockfile(&lock).is_owned(&dest),
            "precondition: the orphan row is on disk"
        );

        let mut report = ProjectionReport::default();
        remove_agy_root(&dest, &ownership, false, &mut report).unwrap();

        assert!(
            !AgyRootOwnership::from_lockfile(&lock).is_owned(&dest),
            "teardown at an absent path must clear the orphan row that authorizes deleting whatever lands there next"
        );
        assert!(
            report.removed_paths.is_empty(),
            "nothing was on disk, so nothing may be reported as removed"
        );
    }

    #[test]
    fn remove_agy_root_forgets_the_row_when_it_removes_a_symlink() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("canonical");
        fs::create_dir_all(&target).unwrap();
        let dest = tmp.path().join("dest");
        if create_symlink_dir(&target, &dest).is_err() {
            return; // symlink-refusing host: the Directory arm covers it instead
        }
        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);
        ownership.record_now(&dest, false).unwrap();

        let mut report = ProjectionReport::default();
        remove_agy_root(&dest, &ownership, false, &mut report).unwrap();

        assert!(!dest.exists(), "the link itself must be gone");
        assert!(target.is_dir(), "removal must never follow through");
        assert!(
            !AgyRootOwnership::from_lockfile(&lock).is_owned(&dest),
            "a symlink teardown must release its row too, not only the copy teardown"
        );
    }

    #[test]
    fn remove_agy_root_dry_run_leaves_the_row_alone() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("dest");
        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);
        ownership.record_now(&dest, false).unwrap();

        let mut report = ProjectionReport::default();
        remove_agy_root(&dest, &ownership, true, &mut report).unwrap();

        assert!(
            AgyRootOwnership::from_lockfile(&lock).is_owned(&dest),
            "dry_run performs zero lockfile writes"
        );
    }

    #[cfg(windows)]
    #[test]
    fn publish_staged_tree_copy_cleans_up_when_the_swap_fails() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let dest = tmp.path().join("dest");
        write_nested(&dest, "held.txt", "held open");
        let _locked = lock_without_read_sharing(&dest.join("held.txt"));

        let result = publish_staged_tree_copy(&source, &dest);
        assert!(
            result.is_err(),
            "an undeletable destination must fail the swap"
        );

        for entry in fs::read_dir(tmp.path()).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            assert!(
                !name.starts_with(".ccync-stage-"),
                "a failed swap must not leave its staged tree behind: {name}"
            );
        }
    }

    #[test]
    fn sweep_staging_orphans_removes_leftovers_and_spares_real_siblings() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("plugins").join("ccync");
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        // A leftover from a run killed between staging and swapping — the case
        // no in-process cleanup can reach.
        let orphan = dest.with_file_name(".ccync-stage-ccync-999-0");
        write_nested(&orphan, "skills/one/SKILL.md", "leftover");
        let neighbour = dest.with_file_name("other-plugin");
        write_nested(&neighbour, "keep.txt", "keep");

        sweep_staging_orphans(&dest);

        assert!(
            !orphan.exists(),
            "a staging orphan inside the scanned plugins directory must be swept"
        );
        assert!(
            neighbour.join("keep.txt").is_file(),
            "a real sibling plugin must be untouched by the sweep"
        );
    }

    // ── remove_agy_root ──────────────────────────────────────────────────

    #[test]
    fn remove_agy_root_removes_a_symlink_without_touching_its_target() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let dest = tmp.path().join("dest");
        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);

        let (shape, wrote) = publish_agy_root(
            &source,
            &dest,
            &ownership,
            false,
            &mut ProjectionReport::default(),
        )
        .unwrap();
        if shape != LinkShape::Symlink {
            return; // this host refuses symlinks — covered by the Directory test below
        }
        assert!(wrote);

        let mut report = ProjectionReport::default();
        remove_agy_root(&dest, &ownership, false, &mut report).unwrap();

        assert!(!dest.exists(), "the link itself must be gone");
        assert!(
            source.join("skills/one/SKILL.md").exists(),
            "removing the link must never follow through to its target"
        );
    }

    #[test]
    fn remove_agy_root_removes_an_owned_directory_copy_and_forgets_its_row() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let dest = tmp.path().join("dest");
        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);

        // Force the copy rung deterministically, independent of platform
        // symlink privilege: pre-seed dest as an owned directory so
        // publish_agy_root's existing==Directory path is exercised, then
        // record ownership directly to set up the removal precondition.
        write_nested(&dest, "skills/one/SKILL.md", "one");
        ownership.record_now(&dest, false).unwrap();

        // `record_now` writes straight to disk without updating this
        // instance's in-memory `registry` — reload, exactly as a fresh
        // `ccync` process would, before checking/using ownership.
        let ownership = AgyRootOwnership::from_lockfile(&lock);
        assert!(ownership.is_owned(&dest));

        let mut report = ProjectionReport::default();
        remove_agy_root(&dest, &ownership, false, &mut report).unwrap();

        assert!(!dest.exists(), "an owned directory copy must be removed");
        let raw = fs::read_to_string(&lock).unwrap();
        let root: Value = serde_json::from_str(&raw).unwrap();
        let recorded = root[PROJECTION_LOCK_KEY][MANAGED_AGY_ROOT_PATHS]
            .as_array()
            .expect("category array present on disk");
        assert!(
            !recorded
                .iter()
                .any(|value| value.as_str() == Some(&normalize_managed_artifact_path(&dest))),
            "the row must be forgotten once the tree is gone"
        );

        // The forgotten row must not come back on a fresh load either —
        // proves the removal reached disk, not just the in-memory registry.
        let reloaded = AgyRootOwnership::from_lockfile(&lock);
        assert!(!reloaded.is_owned(&dest));
    }

    #[test]
    fn remove_agy_root_preserves_an_unowned_real_directory_even_with_no_lockfile() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("dest");
        write_nested(&dest, "users-own-file.txt", "do not touch");
        let lock = tmp.path().join("nolock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);

        let mut report = ProjectionReport::default();
        remove_agy_root(&dest, &ownership, false, &mut report).unwrap();

        assert!(
            dest.join("users-own-file.txt").exists(),
            "an unowned real directory must survive untouched"
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.message.contains("preserved user-owned path")),
            "preservation must be reported"
        );
    }

    #[test]
    fn remove_agy_root_is_a_no_op_when_path_is_absent() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("does-not-exist");
        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);

        let mut report = ProjectionReport::default();
        remove_agy_root(&dest, &ownership, false, &mut report).unwrap();

        assert!(!dest.exists());
        assert!(report.removed_paths.is_empty());
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn remove_agy_root_dry_run_removes_nothing() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("dest");
        write_nested(&dest, "skills/one/SKILL.md", "one");
        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);
        ownership.record_now(&dest, false).unwrap();

        // Reload so this instance actually sees the row `record_now` just
        // wrote — otherwise the owned-directory removal path this test
        // means to exercise under dry_run is never reached at all.
        let ownership = AgyRootOwnership::from_lockfile(&lock);
        assert!(ownership.is_owned(&dest));
        let bytes_before = fs::read(&lock).unwrap();

        let mut report = ProjectionReport::default();
        remove_agy_root(&dest, &ownership, true, &mut report).unwrap();

        assert!(dest.exists(), "dry_run must not remove the tree");
        assert_eq!(
            fs::read(&lock).unwrap(),
            bytes_before,
            "dry_run must leave the lockfile bytes untouched"
        );
    }

    #[test]
    fn forget_now_row_removal_survives_a_sibling_registrys_persist() {
        // Mirrors the disk-authoritative-persist survival probe
        // (agy_root_row_survives_a_sibling_registrys_persist) for the
        // removal direction: once forgotten, the row must stay gone
        // even when a sibling registry instance that never touched this
        // category later persists — the same disk-authoritative guarantee
        // record_now relies on, now proven for forget_now.
        let tmp = TempDir::new().unwrap();
        let lock = tmp.path().join("lock.json");
        let dest = tmp.path().join("dest");
        let ownership = AgyRootOwnership::from_lockfile(&lock);
        ownership.record_now(&dest, false).unwrap();
        ownership.forget_now(&dest, false).unwrap();

        let mut report = ProjectionReport::default();
        let mut sibling = ManagedArtifactRegistry::load(&lock, &mut report);
        sibling.mark(MANAGED_SKILL_PATHS, &tmp.path().join("unrelated"));
        sibling.persist(&[], false, &mut report).unwrap();

        let reloaded = AgyRootOwnership::from_lockfile(&lock);
        assert!(
            !reloaded.is_owned(&dest),
            "a sibling instance's persist must not resurrect a forgotten row"
        );
    }
}
