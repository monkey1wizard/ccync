//! MCP (Model Context Protocol) config orchestration crate.
//!
//! Owns variable resolution, safe CCYNC-managed merge, per-host write, and the
//! `HealthCheck` impl for MCP config state.
//!
//! Dependency law: `mcp` → `base`. No CCYNC dep cycles. MCP host config
//! serializers live in the `serializers` submodule.

pub mod serializers;

mod entry;
mod evidence;
mod health;
mod host_render;
mod io;
mod merge;
mod migration;
mod planner;
mod projection;
mod resolver;
mod routing;
mod state;
mod transaction;
mod windows_support;

#[cfg(test)]
mod default_placeholder_tests;
#[cfg(test)]
mod health_secret_tests;
#[cfg(test)]
mod planner_retention_tests;
#[cfg(test)]
mod report_skip_tests;
#[cfg(test)]
mod tolerant_resolution_tests;

pub use ccync_foundation::mcp::{McpError, McpManifest, McpServer, Result};
pub use evidence::*;
pub use health::*;
pub use projection::*;
pub use resolver::*;

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReselectHold {
    identity: ccync_foundation::mcp::DeclarationIdentity,
    host: String,
}

impl ReselectHold {
    pub fn identity(&self) -> &ccync_foundation::mcp::DeclarationIdentity {
        &self.identity
    }

    pub fn host(&self) -> &str {
        &self.host
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReselectOutcome {
    Applied,
    Previewed,
    Blocked(String),
    NotHeld,
}

fn reselect_transaction_hold(id: &str) -> Option<ReselectHold> {
    serde_json::from_str(id.strip_prefix("reselect-")?).ok()
}

pub fn read_reselect_holds(
    managed_json: &Path,
) -> std::result::Result<Vec<ReselectHold>, McpUpdateError> {
    let paths = live_host_paths();
    let live = read_live_entries(&paths);
    let mut unreadable: std::collections::BTreeSet<String> = live
        .failures
        .keys()
        .map(|h| h.as_str().to_owned())
        .collect();
    for host in host_render::Host::ALL {
        if !paths.contains_key(&host)
            || paths.get(&host).is_some_and(|p| p.exists() && !p.is_file())
        {
            unreadable.insert(host.as_str().to_owned());
        }
    }
    let prior = if managed_json.is_file() {
        Some(load_manifest(managed_json)?)
    } else {
        None
    };
    let (prep, ()) = prepare_projection_recovery(
        managed_json,
        prior.as_ref(),
        &live.by_host,
        &live.unaddressable_by_host,
        &unreadable,
        PrepareMode::Inspect,
        || Ok(()),
    )?;
    Ok(prep.holds)
}

/// The caller holds the lifecycle lock and supplies the current desired manifest.
pub fn reselect_mcp_server(
    managed_json: &Path,
    machine_config_path: &Path,
    desired_manifest: &McpManifest,
    identity: &ccync_foundation::mcp::DeclarationIdentity,
    host: &str,
    preview: bool,
) -> std::result::Result<ReselectOutcome, McpUpdateError> {
    use std::collections::{BTreeMap, BTreeSet};
    let Some(target) = host_render::Host::ALL
        .into_iter()
        .find(|h| h.as_str() == host)
    else {
        return Ok(ReselectOutcome::Blocked("unknown MCP host".into()));
    };
    let paths = live_host_paths();
    let live = read_live_entries(&paths);
    let mut unreadable: BTreeSet<String> = live
        .failures
        .keys()
        .map(|h| h.as_str().to_owned())
        .collect();
    for h in host_render::Host::ALL {
        if !paths.contains_key(&h) || paths.get(&h).is_some_and(|p| p.exists() && !p.is_file()) {
            unreadable.insert(h.as_str().to_owned());
        }
    }
    let prior = if managed_json.is_file() {
        Some(load_manifest(managed_json)?)
    } else {
        None
    };
    let (prep, ()) = prepare_projection_recovery(
        managed_json,
        prior.as_ref(),
        &live.by_host,
        &live.unaddressable_by_host,
        &unreadable,
        if preview {
            PrepareMode::Inspect
        } else {
            PrepareMode::Normal
        },
        || Ok(()),
    )?;
    let state = state::ProjectedState::load(&state::ProjectedState::path_beside(managed_json))?;
    let hold = ReselectHold {
        identity: identity.clone(),
        host: host.to_owned(),
    };
    if !prep.holds.contains(&hold) {
        return Ok(ReselectOutcome::NotHeld);
    }
    if unreadable.contains(host) {
        return Ok(ReselectOutcome::Blocked(
            "host configuration is unreadable".into(),
        ));
    }
    let manifest = desired_manifest;
    if !manifest
        .provenance
        .get(&identity.server)
        .is_some_and(|p| p.identity() == identity)
    {
        return Ok(ReselectOutcome::Blocked(
            "declaration identity is unavailable or changed".into(),
        ));
    }
    let routes = routing::route_decisions(manifest, state.migration.as_ref())?;
    let (desired, _) = render_desired_by_host(manifest, machine_config_path, &routes)?;
    let Some(entry) = desired
        .get(&target)
        .and_then(|m| m.get(&identity.server))
        .cloned()
    else {
        return Ok(ReselectOutcome::Blocked(
            "declaration is not publishable on this host".into(),
        ));
    };
    if live
        .by_host
        .get(host)
        .is_some_and(|m| m.contains_key(&identity.server))
        || live
            .unaddressable_by_host
            .get(host)
            .is_some_and(|s| s.contains(&identity.server))
    {
        return Ok(ReselectOutcome::Blocked(
            "a live entry already exists".into(),
        ));
    }
    if state.migration.as_ref().is_some_and(|m| {
        m.unproven
            .iter()
            .any(|u| u.host == host && u.server == identity.server)
    }) {
        return Ok(ReselectOutcome::Blocked(
            "historical identity is unproven".into(),
        ));
    }
    if preview {
        return Ok(ReselectOutcome::Previewed);
    }
    let Some(path) = paths.get(&target) else {
        return Ok(ReselectOutcome::Blocked("host path is unavailable".into()));
    };
    let retained = prep
        .owned_by_host
        .get(host)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|name| name != &identity.server)
        .collect();
    let transaction_id = format!("reselect-{}", serde_json::to_string(&hold)?);
    let report = apply_projection_transaction(
        managed_json,
        &BTreeMap::from([(target, path.clone())]),
        &BTreeMap::from([(target, BTreeMap::from([(identity.server.clone(), entry)]))]),
        &live.by_host,
        &live.unaddressable_by_host,
        &prep.owned_by_host,
        &BTreeMap::from([(target, retained)]),
        &transaction_id,
        |_| Ok(()),
    )?;
    if let Some((_, reason)) = report.host_failures.into_iter().next() {
        return Ok(ReselectOutcome::Blocked(reason));
    }
    if report.written == 1 {
        Ok(ReselectOutcome::Applied)
    } else {
        Ok(ReselectOutcome::Blocked(
            "publication did not complete".into(),
        ))
    }
}

/// Route intent carries no ownership or permission to remove an entry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MigratedRoute {
    #[serde(flatten)]
    identity: ccync_foundation::mcp::DeclarationIdentity,
    #[serde(default)]
    digest: String,
    hosts: Vec<String>,
}

impl MigratedRoute {
    pub fn new(identity: ccync_foundation::mcp::DeclarationIdentity, hosts: Vec<String>) -> Self {
        Self {
            identity,
            digest: String::new(),
            hosts,
        }
    }
    pub fn identity(&self) -> &ccync_foundation::mcp::DeclarationIdentity {
        &self.identity
    }
    pub fn hosts(&self) -> &[String] {
        &self.hosts
    }
}

pub fn read_migrated_routes(
    managed_json: &Path,
) -> std::result::Result<Vec<MigratedRoute>, McpUpdateError> {
    Ok(
        state::ProjectedState::load(&state::ProjectedState::path_beside(managed_json))?
            .migration
            .map(|m| m.routes)
            .unwrap_or_default(),
    )
}

/// The caller must hold the lifecycle lock throughout this operation.
pub fn commit_restored_routes(
    managed_json: &Path,
    routes: &[MigratedRoute],
) -> std::result::Result<(), McpUpdateError> {
    let path = state::ProjectedState::path_beside(managed_json);
    let preview = preview_restored_routes(managed_json, routes)?;
    commit_routes_preview(&path, &preview)
}

fn read_optional_state(path: &Path) -> std::result::Result<Option<Vec<u8>>, McpUpdateError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn commit_routes_preview(
    path: &Path,
    preview: &RestoredRoutesPreview,
) -> std::result::Result<(), McpUpdateError> {
    let current = read_optional_state(path)?;
    if current.as_deref() == Some(preview.after()) {
        return Ok(());
    }
    if current.as_deref() != preview.before() {
        return Err(McpUpdateError::AmbiguousProjection(
            "projected state changed during restore".into(),
        ));
    }
    ensure_parent(path)?;
    io::write_atomic(
        path,
        std::str::from_utf8(preview.after()).expect("JSON is UTF-8"),
    )
}

pub struct RestoredRoutesPreview {
    before: Option<Vec<u8>>,
    after: Vec<u8>,
}

impl RestoredRoutesPreview {
    pub fn before(&self) -> Option<&[u8]> {
        self.before.as_deref()
    }
    pub fn after(&self) -> &[u8] {
        &self.after
    }
}

pub fn preview_restored_routes(
    managed_json: &Path,
    routes: &[MigratedRoute],
) -> std::result::Result<RestoredRoutesPreview, McpUpdateError> {
    use std::collections::BTreeSet;
    let mut names = BTreeSet::new();
    let mut identities = BTreeSet::new();
    let mut normalized = Vec::new();
    for route in routes {
        if route.identity.source.is_empty()
            || route.identity.server.is_empty()
            || !names.insert(route.identity.server.clone())
            || !identities.insert((route.identity.source.clone(), route.identity.server.clone()))
            || route
                .hosts
                .iter()
                .any(|h| !host_render::Host::ALL.iter().any(|v| v.as_str() == h))
        {
            return Err(McpUpdateError::AmbiguousProjection(
                "invalid or conflicting restored MCP route".into(),
            ));
        }
        let mut route = route.clone();
        route.hosts = host_render::Host::ALL
            .iter()
            .filter(|h| route.hosts.iter().any(|s| s == h.as_str()))
            .map(|h| h.as_str().to_owned())
            .collect();
        normalized.push(route);
    }
    normalized.sort_by(|a, b| {
        (&a.identity.source, &a.identity.server).cmp(&(&b.identity.source, &b.identity.server))
    });
    let path = state::ProjectedState::path_beside(managed_json);
    let before = read_optional_state(&path)?;
    let mut state = state::ProjectedState::load(&path)?;
    if read_optional_state(&path)? != before {
        return Err(McpUpdateError::AmbiguousProjection(
            "projected state changed during restore preview".into(),
        ));
    }
    let original = state.clone();
    if let Some(existing) = &state.migration {
        if normalized.iter().any(|new| {
            existing.routes.iter().any(|old| {
                old.identity.server == new.identity.server && old.identity != new.identity
            })
        }) {
            return Err(McpUpdateError::AmbiguousProjection(
                "conflicting restored MCP route identity".into(),
            ));
        }
    }
    let section = state
        .migration
        .get_or_insert_with(|| state::MigrationSection {
            complete: true,
            routes: Vec::new(),
            unproven: Vec::new(),
        });
    section.complete = true;
    section.routes = normalized;
    state.version = 2;
    let after = if before.is_some() && original == state {
        before.clone().expect("checked above")
    } else {
        state.to_bytes()?
    };
    Ok(RestoredRoutesPreview { before, after })
}

/// Shared test-only helpers for `#[cfg(test)]` modules in this crate.
///
/// Tests that mutate process-wide env vars (`HOME`/`USERPROFILE`/`APPDATA`/
/// `XDG_CONFIG_HOME`) must serialize against a single shared lock, or two
/// tests can race on the same env var concurrently under `cargo test`'s
/// default multithreading.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Poison-tolerant: a panicking test must not cascade-fail every other
    /// env test by poisoning the lock.
    pub(crate) fn env_guard() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Write-ahead projection: pre-render recovery / bootstrap preparation
// ─────────────────────────────────────────────────────────────────────────────

/// Whether preparation may persist state, or only inspect and report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrepareMode {
    /// May commit recovered ownership, persist bootstrap claims, and clear a
    /// recovered transaction.
    Normal,
    /// Reports the same recovery/bootstrap actions but writes, commits and
    /// clears nothing.
    ///
    /// Reached in production through [`inspect_mcp_projection`], which
    /// `ccync sync --dry-run` calls. This mode commits no ownership and clears
    /// no journal, but it is only half of a no-write path: `render` is supplied
    /// by the caller and this mode still invokes it. `inspect_mcp_projection`
    /// supplies a no-op render, which is what makes the whole call write-free.
    Inspect,
}

/// In-memory result of pre-render preparation, handed to the apply step.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProjectionPrep {
    /// Committed ownership after recovery/bootstrap: host key -> owned names.
    pub owned_by_host: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    /// Human-readable recovery/bootstrap actions (for inspect/report output).
    pub actions: Vec<String>,
    pub holds: Vec<ReselectHold>,
}

/// Flatten a per-host live fingerprint map into `(host, name)` keyed pairs for
/// the recovery classifier.
fn flatten_live(
    live_by_host: &std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<String, crate::entry::EntryFingerprint>,
    >,
) -> std::collections::BTreeMap<(String, String), crate::entry::EntryFingerprint> {
    let mut out = std::collections::BTreeMap::new();
    for (host, entries) in live_by_host {
        for (name, fp) in entries {
            out.insert((host.clone(), name.clone()), fp.clone());
        }
    }
    out
}

/// Prepare the MCP projection *before* the canonical render overwrites the prior
/// `managed.json`.
///
/// Order of operations: (1) classify any pending write-ahead transaction;
/// (2) invoke `render` and validate the new input; (3) in `Normal`
/// mode, commit the proven ownership and clear a recovered transaction. A third state
/// is fatal and leaves state and journal untouched.
///
/// `Inspect` mode writes no *ownership state* — it commits nothing and clears
/// no journal — and returns the same in-memory prep. It still invokes `render`,
/// which does write (overwriting `managed.json` is what `render` is for), so
/// this mode is not by itself a no-write path: a dry-run caller must supply a
/// `render` that does not write. [`inspect_mcp_projection`] is that caller and
/// supplies a no-op render; prefer it over constructing an `Inspect` call here.
///
/// `live_by_host` is the caller-supplied snapshot of current live entry
/// fingerprints (keyed by host key then server name), so this function needs no
/// knowledge of host file formats.
///
/// `unreadable` names the hosts whose live config could not be parsed. A pending
/// transaction that touches one of them is fatal: its entries cannot be
/// classified, and treating the unreadable host's entries as `absent` would
/// silently classify a removal as *landed* and grant ownership on the strength
/// of a file ccync never read.
pub(crate) fn prepare_projection_recovery<T>(
    managed_json: &Path,
    _prior_manifest: Option<&McpManifest>,
    live_by_host: &std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<String, crate::entry::EntryFingerprint>,
    >,
    _unaddressable_by_host: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    unreadable: &std::collections::BTreeSet<String>,
    mode: PrepareMode,
    render: impl FnOnce() -> std::result::Result<T, McpUpdateError>,
) -> std::result::Result<(ProjectionPrep, T), McpUpdateError> {
    prepare_projection_recovery_with(
        managed_json,
        _prior_manifest,
        live_by_host,
        _unaddressable_by_host,
        unreadable,
        mode,
        render,
        |_, _| false,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "recovery requires independent state, host, mode, and callback inputs"
)]
fn prepare_projection_recovery_with<T>(
    managed_json: &Path,
    _prior_manifest: Option<&McpManifest>,
    live_by_host: &std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<String, crate::entry::EntryFingerprint>,
    >,
    _unaddressable_by_host: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    unreadable: &std::collections::BTreeSet<String>,
    mode: PrepareMode,
    render: impl FnOnce() -> std::result::Result<T, McpUpdateError>,
    update: impl FnOnce(&mut state::ProjectedState, &T) -> bool,
) -> std::result::Result<(ProjectionPrep, T), McpUpdateError> {
    use crate::host_render::Host;
    use crate::planner::classify_recovery;
    use crate::state::ProjectedState;
    use crate::transaction::ProjectionTransaction;

    let state_path = ProjectedState::path_beside(managed_json);
    let txn_path = ProjectionTransaction::path_beside(managed_json);

    let mut state = ProjectedState::load(&state_path)?;
    let mut actions: Vec<String> = Vec::new();
    let mut pending_transaction = false;
    let mut state_changed = false;

    if let Some(txn) = ProjectionTransaction::load(&txn_path)? {
        pending_transaction = true;
        // An interrupted transaction can only be classified against live entries
        // ccync can actually read. If it touches an unreadable host, stop: no
        // ownership may be granted, and the journal must survive for a later run.
        let blocked: Vec<String> = txn
            .entries
            .iter()
            .filter(|e| {
                unreadable.contains(&e.host)
                    || !Host::ALL.iter().any(|host| host.as_str() == e.host)
            })
            .map(|e| format!("{}/{}", e.host, e.name))
            .collect();
        if !blocked.is_empty() {
            return Err(McpUpdateError::AmbiguousProjection(format!(
                "an interrupted MCP transaction covers host(s) whose live config cannot be read: {}; live entries preserved, no deletion performed — repair the host config, then re-run sync",
                blocked.join(", ")
            )));
        }

        // A transaction was interrupted — classify it against the live entries.
        let live_pairs = flatten_live(live_by_host);
        let outcome = classify_recovery(&txn, &live_pairs);

        if outcome.has_third_state() {
            // Fail closed: name the ambiguous pairs, change nothing.
            let named = outcome
                .third_states
                .iter()
                .map(|(h, n)| format!("{h}/{n}"))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(McpUpdateError::AmbiguousProjection(format!(
                "interrupted MCP transaction is ambiguous for: {named}; live entries preserved, no deletion performed — resolve manually then re-run sync"
            )));
        }

        // Report the journal itself, not just its landed subset. A transaction
        // whose entries all classify as NotLanded grants no ownership and so
        // pushes no "recovered" line below — but it is still a pending
        // transaction that was found and resolved, and a caller reporting only
        // the landed lines would show an empty list for a run that did recover
        // an interrupted projection.
        actions.push(format!(
            "pending transaction {} found, covering {} host/name pair(s); classified against live entries",
            txn.transaction_id,
            txn.entries.len()
        ));

        // Grant ownership only for landed pairs; NotLanded preserves prior state.
        for (host, names) in &outcome.landed_by_host {
            let owned = state.hosts.entry(host.clone()).or_default();
            for name in names {
                owned.insert(name.clone());
            }
            if let Some(selected) = reselect_transaction_hold(&txn.transaction_id) {
                if selected.host == *host && names.contains(&selected.identity.server) {
                    state.holds.retain(|hold| hold != &selected);
                }
            }
            actions.push(format!(
                "recovered {} landed entr(y/ies) on {host}",
                names.len()
            ));
        }
        for (host, names) in &outcome.removed_by_host {
            if let Some(owned) = state.hosts.get_mut(host) {
                for name in names {
                    owned.remove(name);
                }
                if owned.is_empty() {
                    state.hosts.remove(host);
                }
            }
            state
                .publications
                .retain(|item| item.host != *host || !names.contains(&item.identity.server));
            actions.push(format!(
                "recovered {} landed removal(s) on {host}; ownership revoked",
                names.len()
            ));
        }
        state_changed = true;
    }

    let mut unproven_missing = Vec::new();
    for (host, names) in state.hosts.clone() {
        if unreadable.contains(&host) {
            continue;
        }
        for name in names {
            let known = state
                .publications
                .iter()
                .find(|item| item.host == host && item.identity.server == name)
                .cloned();
            let item = known.or_else(|| {
                _prior_manifest
                    .and_then(|manifest| manifest.provenance.get(&name))
                    .filter(|p| p.identity().server == name && !p.identity().source.is_empty())
                    .map(|p| ReselectHold {
                        identity: p.identity().clone(),
                        host: host.clone(),
                    })
            });
            if let Some(item) = item {
                if !state.publications.contains(&item) {
                    state.publications.push(item.clone());
                    state_changed = true;
                }
                if !live_by_host
                    .get(&host)
                    .is_some_and(|entries| entries.contains_key(&name))
                    && !state.holds.contains(&item)
                {
                    state.holds.push(item);
                    state_changed = true;
                    actions.push(format!("{host}/{name}: missing, pending reselection"));
                }
            } else if !live_by_host
                .get(&host)
                .is_some_and(|entries| entries.contains_key(&name))
            {
                unproven_missing.push(state::UnprovenRoute {
                    host: host.clone(),
                    server: name.clone(),
                    missing: vec!["publication-source".into()],
                });
                actions.push(format!("{host}/{name}: unproven publication source"));
            }
        }
    }

    let rendered = render()?;
    if let PrepareMode::Normal = mode {
        state_changed |= update(&mut state, &rendered);
        if !unproven_missing.is_empty() {
            let section = state
                .migration
                .get_or_insert_with(|| state::MigrationSection {
                    complete: true,
                    routes: Vec::new(),
                    unproven: Vec::new(),
                });
            for item in unproven_missing {
                if !section
                    .unproven
                    .iter()
                    .any(|existing| existing.host == item.host && existing.server == item.server)
                {
                    section.unproven.push(item);
                    state_changed = true;
                }
            }
        }
        if state_changed {
            state.save(&state_path)?;
            if ProjectedState::load(&state_path)? != state {
                return Err(McpUpdateError::AmbiguousProjection(
                    "projected state verification failed".into(),
                ));
            }
        }
        if pending_transaction {
            ProjectionTransaction::remove(&txn_path)?;
        }
    }

    let prep = ProjectionPrep {
        owned_by_host: state.hosts.clone(),
        actions,
        holds: state.holds.clone(),
    };
    Ok((prep, rendered))
}

/// Checkpoints at which the apply step invokes its fault hook. Used only to
/// inject failures in tests (simulating a crash); production passes a no-op hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FaultPoint {
    /// After the transaction is persisted, before the first host write.
    BeforeFirstHost,
    /// After a host's delta is applied.
    AfterHost(crate::host_render::Host),
    /// After all hosts are written, before ownership is committed.
    BeforeCommit,
    /// After ownership is committed, before the transaction is cleared.
    AfterCommit,
}

/// What kind of writer a host uses.
enum HostWriteKind {
    /// JSON provider with the given server-map key.
    Json(&'static str),
    /// Codex TOML sections.
    Codex,
}

fn host_write_kind(host: crate::host_render::Host) -> HostWriteKind {
    use crate::host_render::Host;
    match host {
        Host::Claude => HostWriteKind::Json("mcpServers"),
        Host::Copilot => HostWriteKind::Json("mcpServers"),
        Host::OpenCode => HostWriteKind::Json("mcp"),
        Host::Codex => HostWriteKind::Codex,
        Host::Agy => HostWriteKind::Json("mcpServers"),
    }
}

/// Report of an applied projection transaction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ApplyReport {
    pub hosts_written: Vec<String>,
    pub removed: usize,
    pub written: usize,
    pub host_failures: Vec<(String, String)>,
}

/// Apply one write-ahead MCP transaction across the four hosts.
///
/// Consumes the committed ownership from [`prepare_projection_recovery`] plus the
/// freshly rendered desired entries. It plans each host (via the pure planner),
/// fails closed on any unowned same-name collision *before* writing anything,
/// persists the complete write-ahead transaction before the first host write,
/// applies each host's removal+overlay delta sequentially, commits per-host
/// ownership only after every write succeeds, and clears the transaction last.
///
/// A host write error (or an injected `fault`) returns immediately, leaving the
/// transaction journal in place so the next non-dry run can recover. `fault` is
/// the internal fault-injection hook (no public test mode); production passes a
/// hook that always returns `Ok`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_projection_transaction(
    managed_json: &Path,
    host_paths: &std::collections::BTreeMap<crate::host_render::Host, std::path::PathBuf>,
    desired_by_host: &std::collections::BTreeMap<
        crate::host_render::Host,
        std::collections::BTreeMap<String, crate::host_render::HostEntry>,
    >,
    live_by_host: &std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<String, crate::entry::EntryFingerprint>,
    >,
    unaddressable_by_host: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    committed_by_host: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    retained_by_host: &std::collections::BTreeMap<
        crate::host_render::Host,
        std::collections::BTreeSet<String>,
    >,
    txn_id: &str,
    fault: impl Fn(&FaultPoint) -> std::result::Result<(), McpUpdateError>,
) -> std::result::Result<ApplyReport, McpUpdateError> {
    use crate::entry::EntryFingerprint;
    use crate::host_render::{Host, HostEntry};
    use crate::merge::{write_codex_delta, write_json_provider_delta, WriteOutcome};
    use crate::planner::{plan_host, HostPlan};
    use crate::state::ProjectedState;
    use crate::transaction::ProjectionTransaction;
    use std::collections::BTreeMap;

    let empty_prior: BTreeMap<String, HostEntry> = BTreeMap::new();
    let loaded_state = ProjectedState::load(&ProjectedState::path_beside(managed_json))?;
    let holds = loaded_state.holds.clone();
    let mut protected = loaded_state
        .migration
        .map(|m| m.unproven)
        .unwrap_or_default();
    if managed_json.is_file() {
        let manifest = load_manifest(managed_json)?;
        protected.retain(|entry| {
            entry
                .missing
                .iter()
                .any(|missing| missing == "publication-source")
                || !manifest
                    .provenance
                    .get(&entry.server)
                    .is_some_and(|p| p.hosts().is_some())
        });
    }

    // 1. Plan every *participating* host; fail closed on any collision before
    //    writing. A host absent from `host_paths` does not participate: its
    //    config path did not resolve on this platform, or its live config could
    //    not be read. It is not planned, not journaled, not written, and — the
    //    point — neither gains nor loses ownership. Committing ownership for a
    //    host ccync never wrote is ownership granted with zero proof: the
    //    recorded name would later authorize `run_mcp_remove_managed` to
    //    delete a same-named entry the user wrote themselves.
    let mut plans: Vec<(Host, &std::path::Path, HostPlan)> = Vec::new();
    let mut report = ApplyReport::default();
    for host in Host::ALL {
        let Some(path) = host_paths.get(&host) else {
            continue;
        };
        let committed = committed_by_host
            .get(host.as_str())
            .cloned()
            .unwrap_or_default();
        let live = live_by_host.get(host.as_str()).cloned().unwrap_or_default();
        let unaddressable = unaddressable_by_host
            .get(host.as_str())
            .cloned()
            .unwrap_or_default();
        let mut desired = desired_by_host.get(&host).cloned().unwrap_or_default();
        let mut retained = retained_by_host.get(&host).cloned().unwrap_or_default();
        if reselect_transaction_hold(txn_id).is_none() {
            for item in holds.iter().filter(|item| item.host == host.as_str()) {
                desired.remove(&item.identity.server);
                retained.insert(item.identity.server.clone());
            }
        }
        for entry in protected.iter().filter(|entry| entry.host == host.as_str()) {
            desired.remove(&entry.server);
            retained.insert(entry.server.clone());
        }
        let mut plan = plan_host(
            &committed,
            &live,
            &unaddressable,
            &empty_prior,
            &desired,
            &retained,
        )?;
        for entry in protected
            .iter()
            .filter(|entry| entry.host == host.as_str() && committed.contains(&entry.server))
        {
            plan.next_owned.insert(entry.server.clone());
        }
        for item in holds
            .iter()
            .filter(|item| item.host == host.as_str() && committed.contains(&item.identity.server))
        {
            plan.next_owned.insert(item.identity.server.clone());
        }
        if !plan.collisions.is_empty() {
            report.host_failures.push((
                host.as_str().to_owned(),
                format!(
                    "unowned MCP entries collide with managed servers: {}; live entries preserved",
                    plan.collisions
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
            continue;
        }
        plans.push((host, path.as_path(), plan));
    }

    // 2. Build the write-ahead transaction (pre/post per affected pair; no-ops omitted).
    let mut txn = ProjectionTransaction::new(txn_id);
    for (host, _, plan) in &plans {
        let live = live_by_host.get(host.as_str());
        let desired = desired_by_host.get(host);
        for name in &plan.removals {
            let pre = live
                .and_then(|m| m.get(name))
                .cloned()
                .unwrap_or(EntryFingerprint::Absent);
            txn.record(*host, name, &pre, &EntryFingerprint::Absent);
        }
        for name in &plan.overlays {
            let pre = live
                .and_then(|m| m.get(name))
                .cloned()
                .unwrap_or(EntryFingerprint::Absent);
            let post = match desired.and_then(|m| m.get(name)) {
                Some(entry) => entry.fingerprint()?,
                None => EntryFingerprint::Absent,
            };
            txn.record(*host, name, &pre, &post);
        }
    }

    let state_path = ProjectedState::path_beside(managed_json);
    let txn_path = ProjectionTransaction::path_beside(managed_json);

    let has_writes = !txn.entries.is_empty();
    if has_writes {
        txn.save(&txn_path)?; // write-ahead, before any host write
    }
    fault(&FaultPoint::BeforeFirstHost)?;

    // 3. Apply each participating host's delta in sequence.
    let mut failed_hosts = std::collections::BTreeSet::new();
    for (host, path, plan) in &plans {
        let current = read_live_entries(&BTreeMap::from([(*host, path.to_path_buf())]));
        let mismatch = current.failures.contains_key(host)
            || txn
                .entries
                .iter()
                .filter(|e| e.host == host.as_str())
                .any(|e| {
                    current
                        .by_host
                        .get(host.as_str())
                        .and_then(|m| m.get(&e.name))
                        .cloned()
                        .unwrap_or(EntryFingerprint::Absent)
                        .as_token()
                        != e.pre
                        || current
                            .unaddressable_by_host
                            .get(host.as_str())
                            .is_some_and(|names| names.contains(&e.name))
                });
        if mismatch {
            failed_hosts.insert(*host);
            report.host_failures.push((
                host.as_str().to_owned(),
                "live MCP entries changed before writing; live entries preserved".into(),
            ));
            txn.entries.retain(|e| e.host != host.as_str());
            if has_writes {
                txn.save(&txn_path)?;
            }
            continue;
        }
        if plan.removals.is_empty() && plan.overlays.is_empty() {
            fault(&FaultPoint::AfterHost(*host))?;
            continue;
        }
        let desired = desired_by_host.get(host);
        let outcome = match host_write_kind(*host) {
            HostWriteKind::Json(servers_key) => {
                let mut overlays: BTreeMap<String, serde_json::Value> = BTreeMap::new();
                for name in &plan.overlays {
                    if let Some(HostEntry::Json(v)) = desired.and_then(|m| m.get(name)) {
                        overlays.insert(name.clone(), v.clone());
                    }
                }
                write_json_provider_delta(path, servers_key, &plan.removals, &overlays)?
            }
            HostWriteKind::Codex => {
                let mut overlays: BTreeMap<String, String> = BTreeMap::new();
                for name in &plan.overlays {
                    if let Some(HostEntry::CodexToml(s)) = desired.and_then(|m| m.get(name)) {
                        overlays.insert(name.clone(), s.clone());
                    }
                }
                write_codex_delta(path, &plan.removals, &overlays)?
            }
        };
        if outcome == WriteOutcome::Written {
            report.hosts_written.push(host.as_str().to_string());
        }
        report.removed += plan.removals.len();
        report.written += plan.overlays.len();
        fault(&FaultPoint::AfterHost(*host))?;
    }

    fault(&FaultPoint::BeforeCommit)?;

    for (host, path, plan) in &plans {
        if failed_hosts.contains(host) {
            continue;
        }
        let current = read_live_entries(&BTreeMap::from([(*host, path.to_path_buf())]));
        if current.failures.contains_key(host) {
            return Err(McpUpdateError::AmbiguousProjection(
                "host configuration could not be verified after writing".into(),
            ));
        }
        for name in plan.overlays.iter().chain(plan.noops.iter()) {
            let expected = desired_by_host
                .get(host)
                .and_then(|entries| entries.get(name));
            let actual = current
                .by_host
                .get(host.as_str())
                .and_then(|entries| entries.get(name));
            if expected
                .map(|entry| entry.fingerprint())
                .transpose()?
                .as_ref()
                != actual
            {
                return Err(McpUpdateError::AmbiguousProjection(
                    "host entry could not be verified after writing".into(),
                ));
            }
        }
    }

    // 4. Commit per-host ownership only after every write proved out. Only
    //    participating hosts appear in `plans`, so a non-participating host's
    //    committed set is left exactly as it was.
    let mut state = ProjectedState::load(&state_path)?;
    for (host, _, plan) in &plans {
        if !failed_hosts.contains(host) {
            state.set_owned(*host, plan.next_owned.clone());
            state.publications.retain(|item| {
                item.host != host.as_str() || !plan.removals.contains(&item.identity.server)
            });
            if managed_json.is_file() {
                let manifest = load_manifest(managed_json)?;
                for name in plan.overlays.iter().chain(plan.noops.iter()) {
                    if let Some(p) = manifest
                        .provenance
                        .get(name)
                        .filter(|p| p.identity().server == *name && !p.identity().source.is_empty())
                    {
                        let item = ReselectHold {
                            identity: p.identity().clone(),
                            host: host.as_str().to_owned(),
                        };
                        if !state.publications.contains(&item) {
                            state.publications.push(item);
                        }
                    }
                }
            }
            if let Some(selected) = reselect_transaction_hold(txn_id) {
                if selected.host == host.as_str()
                    && plan.overlays.contains(&selected.identity.server)
                {
                    state.holds.retain(|item| item != &selected);
                }
            }
        }
    }
    state.save(&state_path)?;
    if ProjectedState::load(&state_path)? != state {
        return Err(McpUpdateError::AmbiguousProjection(
            "projected state verification failed".into(),
        ));
    }

    fault(&FaultPoint::AfterCommit)?;

    // 5. Clear the transaction last.
    if has_writes {
        ProjectionTransaction::remove(&txn_path)?;
    }
    Ok(report)
}

// ─────────────────────────────────────────────────────────────────────────────
// Public write-ahead MCP projection seam
// ─────────────────────────────────────────────────────────────────────────────

/// Result of a write-ahead MCP projection.
pub struct McpProjectionReport {
    /// Host keys whose live config was actually changed.
    pub hosts_updated: Vec<String>,
    /// Recovery/bootstrap actions performed before the render (for reporting).
    pub recovery_actions: Vec<String>,
    /// Desired-manifest servers skipped because secret-class variables could
    /// not be resolved, keyed by server name and missing variable names.
    pub secret_skips: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    pub variable_reports: Vec<ServerVariableReport>,
    /// Hosts skipped because their live config could not be read or parsed:
    /// `(host key, reason)` pairs, the reason naming the path and why the
    /// read failed. Non-empty means the projection did not fully converge:
    /// the render and the other hosts succeeded, but the caller must still
    /// report failure.
    pub host_failures: Vec<(String, String)>,
    pub decisions: Vec<McpHostDecision>,
}

impl McpProjectionReport {
    pub fn decisions(&self) -> &[McpHostDecision] {
        &self.decisions
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpDecisionStatus {
    Unrouted,
    Excluded,
    Disabled,
    Unsupported,
    MissingSecret,
    UnresolvedOrdinaryVariable,
    SourcePreserved,
    Blocked,
    Failed,
    Applied,
    Planned,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpHostDecision {
    identity: ccync_foundation::mcp::DeclarationIdentity,
    host: String,
    status: McpDecisionStatus,
    reason: String,
    source_kind: Option<ccync_foundation::mcp::McpSourceKind>,
    native_format: &'static str,
    release_form: &'static str,
    selection_reason: &'static str,
    launch_note: Option<&'static str>,
}

impl McpHostDecision {
    pub fn identity(&self) -> &ccync_foundation::mcp::DeclarationIdentity {
        &self.identity
    }
    pub fn host(&self) -> &str {
        &self.host
    }
    pub fn status(&self) -> McpDecisionStatus {
        self.status
    }
    pub fn reason(&self) -> &str {
        &self.reason
    }
    pub fn source_kind(&self) -> Option<ccync_foundation::mcp::McpSourceKind> {
        self.source_kind
    }
    pub fn native_format(&self) -> &str {
        self.native_format
    }
    pub fn release_form(&self) -> &str {
        self.release_form
    }
    pub fn selection_reason(&self) -> &str {
        self.selection_reason
    }
    pub fn launch_note(&self) -> Option<&str> {
        self.launch_note
    }
}

pub struct McpDeclarationInspection {
    decisions: Vec<McpHostDecision>,
}

impl McpDeclarationInspection {
    pub fn decisions(&self) -> &[McpHostDecision] {
        &self.decisions
    }
}

/// The five live host config paths ccync projects onto (those that resolve).
fn live_host_paths() -> std::collections::BTreeMap<crate::host_render::Host, std::path::PathBuf> {
    use crate::host_render::Host;
    let mut m = std::collections::BTreeMap::new();
    if let Some(p) = ccync_foundation::paths::claude_config_path() {
        m.insert(Host::Claude, p);
    }
    if let Some(p) = copilot_cli_mcp_path() {
        m.insert(Host::Copilot, p);
    }
    if let Some(p) = opencode_config_path() {
        m.insert(Host::OpenCode, p);
    }
    if let Some(p) = codex_config_path() {
        m.insert(Host::Codex, p);
    }
    if let Some(p) = agy_mcp_config_path() {
        m.insert(Host::Agy, p);
    }
    m
}

/// Read a JSON host's live server entries and fingerprint each.
fn read_live_json_entries(
    path: &Path,
    servers_key: &str,
) -> std::result::Result<
    std::collections::BTreeMap<String, crate::entry::EntryFingerprint>,
    McpUpdateError,
> {
    let mut out = std::collections::BTreeMap::new();
    let content = std::fs::read_to_string(path)?;
    if content.trim().is_empty() {
        return Ok(out);
    }
    let root: serde_json::Value = serde_json::from_str(&content)?;
    if let Some(servers) = root.get(servers_key).and_then(|v| v.as_object()) {
        for (name, entry) in servers {
            out.insert(name.clone(), crate::entry::fingerprint_json(entry));
        }
    }
    Ok(out)
}

/// Read a Codex host's live server entries and fingerprint each. Each server's
/// subtree is wrapped as `{mcp_servers: {name: …}}` before fingerprinting, so the
/// digest matches the desired render (which fingerprints the same wrapper shape).
///
/// Returns the fingerprints plus the names the *writer* cannot address. This
/// read is semantic (`toml::from_str`), so it sees `mcp_servers.foo` whatever
/// the file's layout; the writer is line-based and can only strip a
/// `[mcp_servers.foo]` table header. Where the two models disagree — most
/// plainly `[mcp_servers]` with `foo = { … }` as an inline key, which is legal
/// TOML and semantically identical to what ccync renders — the reader must not
/// let ownership be claimed. If it did, ccync would fingerprint-match and claim
/// `foo`, and then either (a) silently fail to remove it, having recorded
/// `post=absent` and dropped ownership, stranding it permanently, or (b) append
/// a fresh `[mcp_servers.foo]` beside the surviving inline key, producing a
/// duplicate key that stops Codex loading its config at all.
///
/// The writer stays line-based on purpose: a `toml::from_str` → mutate →
/// `to_string` round-trip would destroy the user's comments and formatting, and
/// non-destructiveness to unrelated user content is a hard contract here. So the
/// two models are made to agree on what they *refuse* rather than on what they
/// rewrite: an unaddressable name is reported as an unowned collision,
/// preserved, named, and fatal to MCP mutation.
fn read_live_codex_entries(
    path: &Path,
) -> std::result::Result<
    (
        std::collections::BTreeMap<String, crate::entry::EntryFingerprint>,
        std::collections::BTreeSet<String>,
    ),
    McpUpdateError,
> {
    let mut out = std::collections::BTreeMap::new();
    let mut unaddressable = std::collections::BTreeSet::new();
    let content = std::fs::read_to_string(path)?;
    if content.trim().is_empty() {
        return Ok((out, unaddressable));
    }
    let parsed: toml::Value = toml::from_str(&content).map_err(|e| {
        // `toml::de::Error`'s Display output embeds the offending source
        // snippet. A Codex MCP entry can contain credentials, so keep the
        // diagnostic structural and content-free.
        McpUpdateError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "invalid Codex config.toml: {} (byte span {:?})",
                e.message(),
                e.span()
            ),
        ))
    })?;
    let declared = crate::merge::codex_header_declared_names(&content);
    if let Some(servers) = parsed.get("mcp_servers").and_then(|v| v.as_table()) {
        for (name, server_val) in servers {
            let mut inner = toml::map::Map::new();
            inner.insert(name.clone(), server_val.clone());
            let mut wrapper = toml::map::Map::new();
            wrapper.insert("mcp_servers".to_string(), toml::Value::Table(inner));
            let json = serde_json::to_value(toml::Value::Table(wrapper))?;
            out.insert(name.clone(), crate::entry::fingerprint_json(&json));
            if !declared.contains(name) {
                unaddressable.insert(name.clone());
            }
        }
    }
    Ok((out, unaddressable))
}

/// The live entries ccync could read, plus the hosts it could not.
pub(crate) struct LiveEntries {
    /// host key -> server name -> fingerprint, for hosts that read cleanly.
    by_host: std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<String, crate::entry::EntryFingerprint>,
    >,
    /// host key -> names present live in a layout that host's writer cannot
    /// faithfully address (see [`read_live_codex_entries`]). Ownership of these
    /// is never claimed; they fail closed as collisions.
    unaddressable_by_host: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    /// Hosts whose live config could not be read or parsed, with the reason.
    failures: std::collections::BTreeMap<crate::host_render::Host, String>,
}

/// Read every live host's entry fingerprints (hosts with no file are skipped).
///
/// A host whose config cannot be read or parsed is *isolated*, not propagated:
/// it lands in `failures` and the projection carries on without it. This is a
/// blast-radius boundary. One hand-edited `~/.codex/config.toml` — or a
/// `~/.claude.json` momentarily truncated because Claude Code is mid-write —
/// must not disable the canonical render, the skills/commands/agents
/// projection, and the other three MCP hosts.
///
/// Isolation is not tolerance: a failed host must not be written to, and must
/// neither gain nor lose ownership, because ccync cannot prove anything about a
/// file it cannot parse. The caller drops it from `host_paths` (so it never
/// participates in the transaction) and still reports overall failure.
fn read_live_entries(
    host_paths: &std::collections::BTreeMap<crate::host_render::Host, std::path::PathBuf>,
) -> LiveEntries {
    let mut by_host = std::collections::BTreeMap::new();
    let mut unaddressable_by_host = std::collections::BTreeMap::new();
    let mut failures = std::collections::BTreeMap::new();
    for (host, path) in host_paths {
        if matches!(path.try_exists(), Ok(false)) {
            continue;
        }
        // JSON hosts have no unaddressable layouts: their writer removes a name
        // by deleting a map key, which is exactly what the reader enumerates.
        let entries = match host_write_kind(*host) {
            HostWriteKind::Json(servers_key) => read_live_json_entries(path, servers_key)
                .map(|e| (e, std::collections::BTreeSet::new())),
            HostWriteKind::Codex => read_live_codex_entries(path),
        };
        match entries {
            Ok((entries, unaddressable)) => {
                if !entries.is_empty() {
                    by_host.insert(host.as_str().to_string(), entries);
                }
                if !unaddressable.is_empty() {
                    unaddressable_by_host.insert(host.as_str().to_string(), unaddressable);
                }
            }
            Err(e) => {
                // The host identity is kept separately by callers (see
                // `McpProjectionReport::host_failures`/`McpInspectReport::host_failures`,
                // both `Vec<(String, String)>`), so this reason string no
                // longer repeats the host name — only what a caller cannot
                // derive from the key: the path and why the read failed.
                failures.insert(
                    *host,
                    format!(
                        "{}: {e} — left untouched; ccync cannot safely write a config it cannot read",
                        path.display()
                    ),
                );
            }
        }
    }
    LiveEntries {
        by_host,
        unaddressable_by_host,
        failures,
    }
}

/// Resolve `${VAR}` placeholders and render a manifest into per-host desired
/// entries.
#[allow(clippy::type_complexity)]
fn render_desired_by_host(
    manifest: &McpManifest,
    machine_config_path: &Path,
    routes: &std::collections::BTreeMap<String, crate::routing::RouteDecision>,
) -> std::result::Result<
    (
        std::collections::BTreeMap<
            crate::host_render::Host,
            std::collections::BTreeMap<String, crate::host_render::HostEntry>,
        >,
        std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    ),
    McpUpdateError,
> {
    let (entries, skipped, _) = render_desired_with_blocked(manifest, machine_config_path, routes)?;
    Ok((entries, skipped))
}

#[allow(
    clippy::type_complexity,
    reason = "rendering returns separate maps for desired entries, skipped variables, and blocked entries"
)]
fn render_desired_with_blocked(
    manifest: &McpManifest,
    machine_config_path: &Path,
    routes: &std::collections::BTreeMap<String, crate::routing::RouteDecision>,
) -> std::result::Result<
    (
        std::collections::BTreeMap<
            crate::host_render::Host,
            std::collections::BTreeMap<String, crate::host_render::HostEntry>,
        >,
        std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
        std::collections::BTreeMap<crate::host_render::Host, std::collections::BTreeSet<String>>,
    ),
    McpUpdateError,
> {
    use crate::host_render::{render_host_entries, Host};
    let resolver = McpVariableResolver::new(build_variable_map_from(Some(machine_config_path)));
    let (resolved, skipped) = resolver.resolve_manifest(manifest);
    let mut out = std::collections::BTreeMap::new();
    let mut blocked = std::collections::BTreeMap::new();
    for host in Host::ALL {
        let mut entries = render_host_entries(host, &resolved)?;
        let mut blocked_names = crate::host_render::blocked_windows_stdio(host, &resolved);
        blocked_names.extend(
            manifest
                .provenance
                .iter()
                .filter_map(|(name, p)| p.native_package_entry().then(|| name.clone())),
        );
        entries.retain(|name, _| !blocked_names.contains(name));
        blocked.insert(host, blocked_names);
        entries.retain(|name, _| matches!(routes.get(name), Some(crate::routing::RouteDecision::Hosts(hosts)) if hosts.contains(&host)));
        out.insert(host, entries);
    }
    Ok((out, skipped, blocked))
}

pub fn inspect_mcp_declarations(
    managed_json: &Path,
    machine_config_path: &Path,
    manifest: &McpManifest,
) -> std::result::Result<McpDeclarationInspection, McpUpdateError> {
    let paths = live_host_paths();
    let live = read_live_entries(&paths);
    let unreadable = host_render::Host::ALL
        .into_iter()
        .filter(|host| {
            live.failures.contains_key(host)
                || paths.get(host).is_none_or(|p| p.exists() && !p.is_file())
        })
        .map(|h| h.as_str().to_owned())
        .collect();
    let prior = if managed_json.is_file() {
        Some(load_manifest(managed_json)?)
    } else {
        None
    };
    let (prep, ()) = prepare_projection_recovery(
        managed_json,
        prior.as_ref(),
        &live.by_host,
        &live.unaddressable_by_host,
        &unreadable,
        PrepareMode::Inspect,
        || Ok(()),
    )?;
    let mut state = state::ProjectedState::load(&state::ProjectedState::path_beside(managed_json))?;
    if state.migration.is_none() {
        state.hosts = prep.owned_by_host.clone();
        state.migration = Some(migration::migrate_legacy_routes(
            &state,
            prior.as_ref(),
            manifest,
        ));
    }
    let routes = routing::route_decisions(manifest, state.migration.as_ref())?;
    let (rendered, skips, blocked) =
        render_desired_with_blocked(manifest, machine_config_path, &routes)?;
    let variables = server_variable_reports_at(manifest, machine_config_path);
    let mut colliding_hosts = std::collections::BTreeSet::new();
    for host in host_render::Host::ALL {
        let mut desired = rendered.get(&host).cloned().unwrap_or_default();
        let mut retained: std::collections::BTreeSet<String> = routes
            .iter()
            .filter_map(|(name, route)| {
                let keep = match route {
                    routing::RouteDecision::Hosts(hosts) => {
                        hosts.contains(&host)
                            && (skips.contains_key(name)
                                || blocked.get(&host).is_some_and(|names| names.contains(name)))
                    }
                    routing::RouteDecision::Unrouted | routing::RouteDecision::SourceHostOnly => {
                        true
                    }
                };
                keep.then(|| name.clone())
            })
            .collect();
        for hold in prep.holds.iter().filter(|h| h.host == host.as_str()) {
            desired.remove(&hold.identity.server);
            retained.insert(hold.identity.server.clone());
        }
        if let Some(migration) = &state.migration {
            for entry in migration
                .unproven
                .iter()
                .filter(|entry| entry.host == host.as_str())
            {
                if entry
                    .missing
                    .iter()
                    .any(|value| value == "publication-source")
                    || !manifest
                        .provenance
                        .get(&entry.server)
                        .is_some_and(|p| p.hosts().is_some())
                {
                    desired.remove(&entry.server);
                    retained.insert(entry.server.clone());
                }
            }
        }
        let plan = planner::plan_host(
            &prep
                .owned_by_host
                .get(host.as_str())
                .cloned()
                .unwrap_or_default(),
            &live.by_host.get(host.as_str()).cloned().unwrap_or_default(),
            &live
                .unaddressable_by_host
                .get(host.as_str())
                .cloned()
                .unwrap_or_default(),
            &Default::default(),
            &desired,
            &retained,
        )?;
        if !plan.collisions.is_empty() {
            colliding_hosts.insert(host);
        }
    }
    let mut decisions = Vec::new();
    let mut names: Vec<_> = manifest.servers.keys().collect();
    names.sort();
    for name in names {
        let p = manifest.provenance.get(name);
        let identity = p.map(|p| p.identity().clone()).unwrap_or_else(|| {
            ccync_foundation::mcp::DeclarationIdentity {
                source: String::new(),
                server: name.clone(),
            }
        });
        for host in host_render::Host::ALL {
            let host_name = host.as_str();
            let (status, reason) = if p.is_some_and(|p| !p.enabled()) {
                (McpDecisionStatus::Disabled, "declaration is disabled")
            } else if p.is_some_and(|p| p.source_preserved()) {
                (
                    McpDecisionStatus::SourcePreserved,
                    "source entry is preserved",
                )
            } else if matches!(
                routes.get(name),
                Some(routing::RouteDecision::Unrouted) | None
            ) {
                (McpDecisionStatus::Unrouted, "no host route selected")
            } else if !matches!(routes.get(name), Some(routing::RouteDecision::Hosts(hosts)) if hosts.contains(&host))
            {
                (McpDecisionStatus::Excluded, "host is not selected")
            } else if p.is_some_and(|p| p.native_package_entry()) {
                (
                    McpDecisionStatus::Blocked,
                    "native package registration may already exist",
                )
            } else if skips.contains_key(name) {
                (
                    McpDecisionStatus::MissingSecret,
                    "secret variable is unresolved",
                )
            } else if blocked.get(&host).is_some_and(|names| names.contains(name)) {
                (
                    McpDecisionStatus::Blocked,
                    windows_support::blocked_reason(host_name),
                )
            } else if unreadable.contains(host_name) {
                (
                    McpDecisionStatus::Failed,
                    "host configuration is unreadable",
                )
            } else if prep
                .holds
                .iter()
                .any(|h| h.host == host_name && h.identity.server == *name)
                || state.migration.as_ref().is_some_and(|m| {
                    m.unproven.iter().any(|u| {
                        u.host == host_name
                            && u.server == *name
                            && (u.missing.iter().any(|v| v == "publication-source")
                                || !p.is_some_and(|p| p.hosts().is_some()))
                    })
                })
            {
                (
                    McpDecisionStatus::Blocked,
                    "publication is held or historical identity is unproven",
                )
            } else if !rendered
                .get(&host)
                .is_some_and(|entries| entries.contains_key(name))
            {
                (
                    McpDecisionStatus::Unsupported,
                    "host cannot render this declaration",
                )
            } else if colliding_hosts.contains(&host) {
                (
                    McpDecisionStatus::Blocked,
                    "live entry cannot be safely replaced",
                )
            } else if variables
                .iter()
                .any(|v| v.server() == name && !v.ordinary_names().is_empty())
            {
                (
                    McpDecisionStatus::UnresolvedOrdinaryVariable,
                    "ordinary variable is unresolved",
                )
            } else {
                (McpDecisionStatus::Planned, "selected for publication")
            };
            let selection_reason = match routes.get(name) {
                Some(routing::RouteDecision::Hosts(hosts)) if hosts.contains(&host) => {
                    if p.is_none() {
                        "legacy route"
                    } else if p.is_some_and(|p| p.hosts().is_some()) {
                        "explicit host selection"
                    } else {
                        "migrated host selection"
                    }
                }
                Some(routing::RouteDecision::Hosts(_)) => "host excluded",
                Some(routing::RouteDecision::SourceHostOnly) => "source preserved",
                _ => "no host selected",
            };
            let server = &manifest.servers[name];
            let release_form = match server.server_type.as_deref() {
                Some("sse") => "sse",
                Some("http") => "http",
                _ if server.command.is_some() => "stdio",
                _ if server.url.is_some() => "http",
                _ => "unknown",
            };
            let launch_note = (release_form == "stdio"
                && matches!(routes.get(name), Some(routing::RouteDecision::Hosts(hosts)) if hosts.len() > 1 && hosts.contains(&host)))
            .then_some("each active host session may launch the process");
            decisions.push(McpHostDecision {
                identity: identity.clone(),
                host: host_name.into(),
                status,
                reason: reason.into(),
                source_kind: p.map(|p| p.kind()),
                native_format: if host == host_render::Host::Codex {
                    "toml"
                } else {
                    "json"
                },
                release_form,
                selection_reason,
                launch_note,
            });
        }
    }
    Ok(McpDeclarationInspection { decisions })
}

/// Run the full write-ahead MCP projection.
///
/// Reads the prior `managed_json` and the live host entries, prepares recovery /
/// legacy bootstrap (committing proven ownership *before* `render` overwrites
/// `managed_json`), then applies the newly rendered desired manifest across the
/// four hosts under one write-ahead transaction. `render` is the canonical
/// render step (it must overwrite `managed_json`). Any ambiguous state, unowned
/// collision, or host-write failure returns an error with the journal retained.
///
/// A host whose live config cannot be *read* is isolated rather than fatal: it
/// is dropped from the projection entirely (not written, ownership untouched)
/// and reported in `host_failures`, so one malformed host config cannot disable
/// the canonical render or the other three hosts. The caller must still treat a
/// non-empty `host_failures` as failure.
pub fn run_mcp_projection(
    managed_json: &Path,
    render: impl FnOnce() -> std::result::Result<(), McpUpdateError>,
) -> std::result::Result<McpProjectionReport, McpUpdateError> {
    let layout = ccync_foundation::paths::EffectiveLayout::load_or_legacy()
        .map_err(|error| McpUpdateError::Io(std::io::Error::other(error)))?;
    if layout.authority() == ccync_foundation::paths::EffectiveLayoutAuthority::MutationBlocked {
        return Err(McpUpdateError::Io(std::io::Error::other(
            "effective storage layout authority is MutationBlocked",
        )));
    }
    let machine_config_path = layout.machine_config_path();
    run_mcp_projection_at(managed_json, &machine_config_path, render)
}

/// Run the MCP projection while resolving variables from an explicit machine config.
pub fn run_mcp_projection_at(
    managed_json: &Path,
    machine_config_path: &Path,
    render: impl FnOnce() -> std::result::Result<(), McpUpdateError>,
) -> std::result::Result<McpProjectionReport, McpUpdateError> {
    run_mcp_projection_at_with_fault(managed_json, machine_config_path, render, |_| Ok(()))
}

pub(crate) fn run_mcp_projection_at_with_fault(
    managed_json: &Path,
    machine_config_path: &Path,
    render: impl FnOnce() -> std::result::Result<(), McpUpdateError>,
    fault: impl Fn(&FaultPoint) -> std::result::Result<(), McpUpdateError>,
) -> std::result::Result<McpProjectionReport, McpUpdateError> {
    let all_host_paths = live_host_paths();
    let live = read_live_entries(&all_host_paths);

    // Hosts that failed to read do not participate: ccync must not write to a
    // config it cannot parse, and must not grant or revoke ownership on one.
    let mut unreadable: std::collections::BTreeSet<String> = live
        .failures
        .keys()
        .map(|h| h.as_str().to_string())
        .collect();
    {
        for (host, path) in &all_host_paths {
            if path.exists() && !path.is_file() {
                unreadable.insert(host.as_str().to_string());
            }
        }
        for host in crate::host_render::Host::ALL {
            if !all_host_paths.contains_key(&host) {
                unreadable.insert(host.as_str().to_string());
            }
        }
    }
    let host_paths: std::collections::BTreeMap<crate::host_render::Host, std::path::PathBuf> =
        all_host_paths
            .iter()
            .filter(|(host, _)| !live.failures.contains_key(host))
            .map(|(host, path)| (*host, path.clone()))
            .collect();

    // Prior manifest (old managed.json), resolved in memory for exact-match
    // bootstrap — read before `render` overwrites it.
    let prior = if managed_json.is_file() {
        Some(load_manifest(managed_json)?)
    } else {
        None
    };

    let (prep, desired_manifest) = prepare_projection_recovery_with(
        managed_json,
        prior.as_ref(),
        &live.by_host,
        &live.unaddressable_by_host,
        &unreadable,
        PrepareMode::Normal,
        || {
            render()?;
            let manifest = load_manifest(managed_json)?;
            routing::route_decisions(&manifest, None)?;
            Ok(manifest)
        },
        |state, desired| {
            if state.migration.is_some() {
                return false;
            }
            state.migration = Some(migration::migrate_legacy_routes(
                state,
                prior.as_ref(),
                desired,
            ));
            state.version = 2;
            true
        },
    )?;

    let state = state::ProjectedState::load(&state::ProjectedState::path_beside(managed_json))?;
    let routes = routing::route_decisions(&desired_manifest, state.migration.as_ref())?;
    let mut decisions =
        inspect_mcp_declarations(managed_json, machine_config_path, &desired_manifest)?.decisions;
    let (desired_by_host, desired_secret_skips, blocked_by_host) =
        render_desired_with_blocked(&desired_manifest, machine_config_path, &routes)?;
    let mut retained_by_host = std::collections::BTreeMap::new();
    for host in host_render::Host::ALL {
        let retained = routes
            .iter()
            .filter_map(|(name, route)| {
                let retain = match route {
                    routing::RouteDecision::Hosts(hosts) => {
                        hosts.contains(&host)
                            && (desired_secret_skips.contains_key(name)
                                || blocked_by_host
                                    .get(&host)
                                    .is_some_and(|names| names.contains(name)))
                    }
                    routing::RouteDecision::Unrouted | routing::RouteDecision::SourceHostOnly => {
                        true
                    }
                };
                retain.then(|| name.clone())
            })
            .collect();
        retained_by_host.insert(host, retained);
    }

    let txn_id = format!(
        "txn-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );

    let report = apply_projection_transaction(
        managed_json,
        &host_paths,
        &desired_by_host,
        &live.by_host,
        &live.unaddressable_by_host,
        &prep.owned_by_host,
        &retained_by_host,
        &txn_id,
        fault,
    )?;

    let recovery_actions = prep.actions;
    let mut host_failures: Vec<_> = live
        .failures
        .into_iter()
        .map(|(host, reason)| (host.as_str().to_string(), reason))
        .collect();
    host_failures.extend(report.host_failures);
    for decision in &mut decisions {
        if matches!(
            decision.status,
            McpDecisionStatus::Planned | McpDecisionStatus::UnresolvedOrdinaryVariable
        ) {
            if host_failures.iter().any(|(host, _)| host == &decision.host) {
                decision.status = McpDecisionStatus::Failed;
                decision.reason = "host projection failed".into();
            } else if decision.status == McpDecisionStatus::Planned {
                decision.status = McpDecisionStatus::Applied;
                decision.reason = "publication committed".into();
            }
        }
    }
    Ok(McpProjectionReport {
        hosts_updated: report.hosts_written,
        recovery_actions,
        secret_skips: desired_secret_skips,
        variable_reports: server_variable_reports_at(&desired_manifest, machine_config_path),
        host_failures,
        decisions,
    })
}

/// An action class `inspect_mcp_projection` structurally *cannot* decide, and
/// why.
///
/// This type exists so a caller cannot report "no collisions" when it means "I
/// never looked". An empty list of collisions and an undecidable collision
/// check are different claims, and only one of them is true here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndeterminedAction {
    /// The action class, e.g. `"planned deletion"`.
    pub class: String,
    /// Why inspection cannot decide it.
    pub reason: String,
}

/// What a read-only MCP inspection could — and could not — determine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpInspectReport {
    /// Recovery / legacy-bootstrap actions inspection *did* determine. Empty
    /// means genuinely none found, which inspection can prove.
    pub recovery_actions: Vec<String>,
    /// Prior-manifest servers with unresolved secret-class variables.
    pub prior_secret_skips: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    /// Hosts whose live config could not be read, so nothing was determined
    /// for them at all: `(host key, reason)` pairs, mirroring
    /// `McpProjectionReport::host_failures`.
    pub host_failures: Vec<(String, String)>,
    /// Action classes inspection cannot reach. Never empty: see
    /// [`inspect_mcp_projection`].
    pub undetermined: Vec<UndeterminedAction>,
}

impl Default for McpInspectReport {
    fn default() -> Self {
        Self {
            recovery_actions: Vec::new(),
            prior_secret_skips: std::collections::BTreeMap::new(),
            host_failures: Vec::new(),
            undetermined: undetermined_without_render(),
        }
    }
}

/// Inspect MCP projection state without writing anything.
///
/// The read-only counterpart to [`run_mcp_projection`], and the reason it lives
/// here rather than in the CLI: it needs the same live-entry prologue (which
/// hosts exist, how each host's file is parsed, how entries are fingerprinted).
/// Deriving that a second time in a caller would put four-host config knowledge
/// outside the crate that owns it, and the two copies would drift.
///
/// Writes nothing: `PrepareMode::Inspect` commits no ownership and clears no
/// journal, the supplied `render` is a no-op, and no host writer is invoked. It
/// therefore leaves `projected-state.json`, `projection.txn.json`,
/// `managed_json` and every live host config byte-identical.
///
/// # What this cannot determine
///
/// Deletion and collision are decided by the planner from the *desired*
/// manifest, and the desired manifest only exists after the canonical render —
/// which writes live surfaces, so a dry run must not invoke it. Those two
/// classes are therefore reported as [`UndeterminedAction`]s rather than
/// omitted or defaulted to empty: reporting an empty collision list here would
/// assert something this function never checked.
pub fn inspect_mcp_projection(
    managed_json: &Path,
) -> std::result::Result<McpInspectReport, McpUpdateError> {
    let layout = ccync_foundation::paths::EffectiveLayout::load_or_legacy()
        .map_err(|error| McpUpdateError::Io(std::io::Error::other(error)))?;
    if layout.authority() == ccync_foundation::paths::EffectiveLayoutAuthority::MutationBlocked {
        return Err(McpUpdateError::Io(std::io::Error::other(
            "effective storage layout authority is MutationBlocked",
        )));
    }
    let machine_config_path = layout.machine_config_path();
    inspect_mcp_projection_at(managed_json, &machine_config_path)
}

/// Inspect MCP projection state using an explicit machine config for secrets.
pub fn inspect_mcp_projection_at(
    managed_json: &Path,
    machine_config_path: &Path,
) -> std::result::Result<McpInspectReport, McpUpdateError> {
    let all_host_paths = live_host_paths();
    let live = read_live_entries(&all_host_paths);

    let mut unreadable: std::collections::BTreeSet<String> = live
        .failures
        .keys()
        .map(|h| h.as_str().to_string())
        .collect();
    for host in crate::host_render::Host::ALL {
        match all_host_paths.get(&host) {
            Some(path) if path.exists() && !path.is_file() => {
                unreadable.insert(host.as_str().to_string());
            }
            None => {
                unreadable.insert(host.as_str().to_string());
            }
            _ => {}
        }
    }

    // The manifest on disk is still the *prior* one: no render has run.
    let (prior, prior_secret_skips) = if managed_json.is_file() {
        let m = load_manifest(managed_json)?;
        let resolver = McpVariableResolver::new(build_variable_map_from(Some(machine_config_path)));
        let (resolved, prior_secret_skips) = resolver.resolve_manifest(&m);
        (Some(resolved), prior_secret_skips)
    } else {
        (None, std::collections::BTreeMap::new())
    };

    // The no-op render is what keeps this call write-free; `Inspect` alone only
    // suppresses the ownership/journal writes.
    let (prep, ()) = prepare_projection_recovery(
        managed_json,
        prior.as_ref(),
        &live.by_host,
        &live.unaddressable_by_host,
        &unreadable,
        PrepareMode::Inspect,
        || Ok(()),
    )?;

    Ok(McpInspectReport {
        recovery_actions: prep.actions,
        prior_secret_skips,
        host_failures: live
            .failures
            .into_iter()
            .map(|(host, reason)| (host.as_str().to_string(), reason))
            .collect(),
        undetermined: undetermined_without_render(),
    })
}

/// The action classes no pre-render inspection can decide.
///
/// Named, so the set and its reason have one definition rather than being spelt
/// out at each construction site. If a future `desired_manifest(config)` seam
/// makes deletion and collision decidable before the render, this is the single
/// place that stops claiming otherwise.
fn undetermined_without_render() -> Vec<UndeterminedAction> {
    let reason = "requires the desired manifest, which only exists after the canonical render; \
                  the render writes live surfaces, so a dry run cannot produce it"
        .to_string();
    vec![
        UndeterminedAction {
            class: "planned deletion".to_string(),
            reason: reason.clone(),
        },
        UndeterminedAction {
            class: "collision".to_string(),
            reason,
        },
    ]
}

/// True when a write-ahead MCP transaction journal is still on disk beside
/// `managed_json`.
///
/// Callers report *what actually happened* after a failed projection instead of
/// assuming a journal exists. It does not always: a fail-closed abort (unowned
/// collision, ambiguous recovery) stops before the journal is written, while an
/// interrupted host write leaves one behind for the next run to converge from.
pub fn projection_journal_retained(managed_json: &Path) -> bool {
    crate::transaction::ProjectionTransaction::path_beside(managed_json).is_file()
}

#[cfg(test)]
mod reselection_tests {
    use super::*;
    use crate::host_render::{Host, HostEntry};
    use std::collections::{BTreeMap, BTreeSet};

    fn holds_on_disk(managed: &Path) -> Vec<ReselectHold> {
        state::ProjectedState::load(&state::ProjectedState::path_beside(managed))
            .unwrap()
            .holds
    }

    #[test]
    fn first_install_writes_without_a_publication_record() {
        let tmp = tempfile::TempDir::new().unwrap();
        let managed = tmp.path().join("managed.json");
        let path = tmp.path().join("claude.json");
        let paths = BTreeMap::from([(Host::Claude, path.clone())]);
        let live = read_live_entries(&paths);
        let (prep, ()) = prepare_projection_recovery(
            &managed,
            None,
            &live.by_host,
            &live.unaddressable_by_host,
            &BTreeSet::new(),
            PrepareMode::Normal,
            || Ok(()),
        )
        .unwrap();
        let desired = BTreeMap::from([(
            Host::Claude,
            BTreeMap::from([(
                "a".into(),
                HostEntry::Json(serde_json::json!({"command":"a"})),
            )]),
        )]);
        apply_projection_transaction(
            &managed,
            &paths,
            &desired,
            &live.by_host,
            &live.unaddressable_by_host,
            &prep.owned_by_host,
            &BTreeMap::new(),
            "ordinary",
            |_| Ok(()),
        )
        .unwrap();
        assert!(read_live_entries(&paths).by_host["claude"].contains_key("a"));
        assert!(holds_on_disk(&managed).is_empty());
    }

    #[test]
    fn missing_legacy_entry_without_source_is_kept_unproven() {
        let tmp = tempfile::TempDir::new().unwrap();
        let managed = tmp.path().join("managed.json");
        let path = tmp.path().join("claude.json");
        let paths = BTreeMap::from([(Host::Claude, path.clone())]);
        let mut state = state::ProjectedState::new();
        state.set_owned(Host::Claude, BTreeSet::from(["a".into()]));
        state
            .save(&state::ProjectedState::path_beside(&managed))
            .unwrap();
        let live = read_live_entries(&paths);
        let (prep, ()) = prepare_projection_recovery(
            &managed,
            None,
            &live.by_host,
            &live.unaddressable_by_host,
            &BTreeSet::new(),
            PrepareMode::Normal,
            || Ok(()),
        )
        .unwrap();
        assert!(prep.actions.iter().any(|s| s.contains("unproven")));
        let desired = BTreeMap::from([(
            Host::Claude,
            BTreeMap::from([(
                "a".into(),
                HostEntry::Json(serde_json::json!({"command":"a"})),
            )]),
        )]);
        apply_projection_transaction(
            &managed,
            &paths,
            &desired,
            &live.by_host,
            &live.unaddressable_by_host,
            &prep.owned_by_host,
            &BTreeMap::new(),
            "ordinary",
            |_| Ok(()),
        )
        .unwrap();
        assert!(!path.exists());
        let saved =
            state::ProjectedState::load(&state::ProjectedState::path_beside(&managed)).unwrap();
        assert!(saved.owned(Host::Claude).contains("a"));
        assert!(saved
            .migration
            .unwrap()
            .unproven
            .iter()
            .any(|u| u.server == "a"));
    }

    #[test]
    fn interrupted_single_reselection_recovers_only_the_selected_item() {
        let tmp = tempfile::TempDir::new().unwrap();
        let managed = tmp.path().join("managed.json");
        let host_path = tmp.path().join("claude.json");
        let paths = BTreeMap::from([(Host::Claude, host_path.clone())]);
        let mut state = state::ProjectedState::new();
        state.set_owned(Host::Claude, BTreeSet::from(["a".into(), "b".into()]));
        for name in ["a", "b"] {
            state.publications.push(ReselectHold {
                identity: ccync_foundation::mcp::DeclarationIdentity {
                    source: "package".into(),
                    server: name.into(),
                },
                host: "claude".into(),
            });
        }
        state
            .save(&state::ProjectedState::path_beside(&managed))
            .unwrap();
        let live = read_live_entries(&paths);
        let (prep, ()) = prepare_projection_recovery(
            &managed,
            None,
            &live.by_host,
            &live.unaddressable_by_host,
            &BTreeSet::new(),
            PrepareMode::Normal,
            || Ok(()),
        )
        .unwrap();
        assert_eq!(prep.holds.len(), 2);
        let entries = BTreeMap::from([
            (
                "a".into(),
                HostEntry::Json(serde_json::json!({"command":"a"})),
            ),
            (
                "b".into(),
                HostEntry::Json(serde_json::json!({"command":"b"})),
            ),
        ]);
        let desired = BTreeMap::from([(Host::Claude, entries.clone())]);
        let empty = BTreeMap::new();
        apply_projection_transaction(
            &managed,
            &paths,
            &desired,
            &live.by_host,
            &live.unaddressable_by_host,
            &prep.owned_by_host,
            &empty,
            "ordinary",
            |_| Ok(()),
        )
        .unwrap();
        assert!(!host_path.exists());
        assert_eq!(holds_on_disk(&managed).len(), 2);
        let single = BTreeMap::from([(
            Host::Claude,
            BTreeMap::from([("b".into(), entries["b"].clone())]),
        )]);
        let retained = BTreeMap::from([(Host::Claude, BTreeSet::from(["a".into()]))]);
        let selection_id = format!(
            "reselect-{}",
            serde_json::to_string(&ReselectHold {
                identity: ccync_foundation::mcp::DeclarationIdentity {
                    source: "package".into(),
                    server: "b".into()
                },
                host: "claude".into(),
            })
            .unwrap()
        );
        let failure = apply_projection_transaction(
            &managed,
            &paths,
            &single,
            &live.by_host,
            &live.unaddressable_by_host,
            &prep.owned_by_host,
            &retained,
            &selection_id,
            |point| {
                if *point == FaultPoint::BeforeFirstHost {
                    Err(McpUpdateError::AmbiguousProjection("interrupted".into()))
                } else {
                    Ok(())
                }
            },
        );
        assert!(failure.is_err());
        assert_eq!(holds_on_disk(&managed).len(), 2);
        prepare_projection_recovery(
            &managed,
            None,
            &live.by_host,
            &live.unaddressable_by_host,
            &BTreeSet::new(),
            PrepareMode::Normal,
            || Ok(()),
        )
        .unwrap();
        assert_eq!(holds_on_disk(&managed).len(), 2);
        let failure = apply_projection_transaction(
            &managed,
            &paths,
            &single,
            &live.by_host,
            &live.unaddressable_by_host,
            &prep.owned_by_host,
            &retained,
            &selection_id,
            |point| {
                if *point == FaultPoint::BeforeCommit {
                    Err(McpUpdateError::AmbiguousProjection("interrupted".into()))
                } else {
                    Ok(())
                }
            },
        );
        assert!(failure.is_err());
        assert_eq!(holds_on_disk(&managed).len(), 2);
        let landed = read_live_entries(&paths);
        let (recovered, ()) = prepare_projection_recovery(
            &managed,
            None,
            &landed.by_host,
            &landed.unaddressable_by_host,
            &BTreeSet::new(),
            PrepareMode::Normal,
            || Ok(()),
        )
        .unwrap();
        let holds = holds_on_disk(&managed);
        assert_eq!(holds.len(), 1);
        assert_eq!(holds[0].identity.server, "a");
        apply_projection_transaction(
            &managed,
            &paths,
            &desired,
            &landed.by_host,
            &landed.unaddressable_by_host,
            &recovered.owned_by_host,
            &empty,
            "ordinary",
            |_| Ok(()),
        )
        .unwrap();
        let final_live = read_live_entries(&paths);
        assert!(final_live.by_host["claude"].contains_key("b"));
        assert!(!final_live.by_host["claude"].contains_key("a"));
        assert_eq!(holds_on_disk(&managed), holds);
    }
}

/// Read-only inspection result for the MCP projection journal beside a
/// caller-supplied `managed.json` path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpJournalInspection {
    /// No journal exists, or the journal is empty.
    Absent,
    /// A supported pending transaction containing fingerprint tokens only.
    Pending { entries: Vec<McpJournalEntry> },
    /// The journal uses a schema version this crate does not support.
    UnsupportedVersion { version: u32 },
    /// The journal could not be read or parsed.
    Corrupt { reason: String },
}

/// One host/name pair and its before/after fingerprint tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpJournalEntry {
    pub host: String,
    pub name: String,
    pub pre: String,
    pub post: String,
}

/// Inspect the write-ahead MCP journal beside `managed_json` without modifying
/// either file. The returned pending entries contain fingerprint tokens, not
/// MCP entry contents.
pub fn inspect_mcp_journal(managed_json: &Path) -> McpJournalInspection {
    use std::io::ErrorKind;

    let journal_path = crate::transaction::ProjectionTransaction::path_beside(managed_json);
    let content = match std::fs::read_to_string(journal_path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return McpJournalInspection::Absent;
        }
        Err(error) => {
            return McpJournalInspection::Corrupt {
                reason: error.to_string(),
            };
        }
    };
    if content.trim().is_empty() {
        return McpJournalInspection::Absent;
    }

    let transaction: crate::transaction::ProjectionTransaction =
        match serde_json::from_str(&content) {
            Ok(transaction) => transaction,
            Err(error) => {
                return McpJournalInspection::Corrupt {
                    reason: error.to_string(),
                };
            }
        };
    if transaction.version != crate::transaction::TRANSACTION_VERSION {
        return McpJournalInspection::UnsupportedVersion {
            version: transaction.version,
        };
    }

    McpJournalInspection::Pending {
        entries: transaction
            .entries
            .into_iter()
            .map(|entry| McpJournalEntry {
                host: entry.host,
                name: entry.name,
                pre: entry.pre,
                post: entry.post,
            })
            .collect(),
    }
}

#[cfg(test)]
mod mcp_journal_inspection_tests {
    use super::{inspect_mcp_journal, McpJournalEntry, McpJournalInspection};
    use std::collections::BTreeMap;
    use std::path::Path;
    use tempfile::TempDir;

    #[test]
    fn preview_restored_routes_after_bytes_equal_committed_bytes() {
        let tmp = TempDir::new().unwrap();
        let managed = tmp.path().join("managed.json");
        let routes = vec![crate::MigratedRoute::new(
            ccync_foundation::mcp::DeclarationIdentity {
                source: "personal".into(),
                server: "example".into(),
            },
            vec!["claude".into()],
        )];
        let preview = crate::preview_restored_routes(&managed, &routes).unwrap();
        assert_eq!(preview.before(), None);
        assert!(!tmp.path().join("projected-state.json").exists());
        crate::commit_restored_routes(&managed, &routes).unwrap();
        assert_eq!(
            std::fs::read(tmp.path().join("projected-state.json")).unwrap(),
            preview.after()
        );
        let path = tmp.path().join("projected-state.json");
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        crate::commit_routes_preview(&path, &preview).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        crate::commit_restored_routes(&managed, &routes).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        std::fs::write(&path, b"third value").unwrap();
        assert!(matches!(
            crate::commit_routes_preview(&path, &preview),
            Err(crate::McpUpdateError::AmbiguousProjection(_))
        ));
        assert_eq!(std::fs::read(path).unwrap(), b"third value");
    }

    fn snapshot(directory: &Path) -> BTreeMap<String, Vec<u8>> {
        std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    std::fs::read(entry.path()).unwrap(),
                )
            })
            .collect()
    }

    fn inspect_unchanged(journal: Option<&str>) -> McpJournalInspection {
        let fixture = TempDir::new().unwrap();
        let managed_json = fixture.path().join("managed.json");
        std::fs::write(&managed_json, b"{\"servers\":{}}\n").unwrap();
        if let Some(journal) = journal {
            std::fs::write(
                fixture.path().join("projection.txn.json"),
                journal.as_bytes(),
            )
            .unwrap();
        }

        let before = snapshot(fixture.path());
        let result = inspect_mcp_journal(&managed_json);
        assert_eq!(snapshot(fixture.path()), before);
        result
    }

    #[test]
    fn mcp_journal_inspection_reports_each_fixture_without_writes() {
        assert_eq!(inspect_unchanged(None), McpJournalInspection::Absent);

        assert_eq!(
            inspect_unchanged(Some(
                r#"{"version":1,"transaction_id":"txn-1","entries":[{"host":"claude","name":"srv","pre":"absent","post":"sha256:abc"}]}"#
            )),
            McpJournalInspection::Pending {
                entries: vec![McpJournalEntry {
                    host: "claude".to_string(),
                    name: "srv".to_string(),
                    pre: "absent".to_string(),
                    post: "sha256:abc".to_string(),
                }],
            }
        );

        assert_eq!(
            inspect_unchanged(Some(
                r#"{"version":2,"transaction_id":"txn-2","entries":[]}"#
            )),
            McpJournalInspection::UnsupportedVersion { version: 2 }
        );

        assert!(matches!(
            inspect_unchanged(Some("{ malformed")),
            McpJournalInspection::Corrupt { .. }
        ));
    }
}

/// Result of a `run_mcp_remove_managed` run.
pub struct McpRemoveReport {
    pub hosts_updated: Vec<String>,
    pub warnings: Vec<String>,
}

/// Remove previously-managed MCP server entries from all live agent configs.
///
/// Reads `previous_manifest_path` to
/// derive the managed server names, then removes those entries from the Claude,
/// Copilot, OpenCode, and agy JSON configs and from the Codex TOML. A missing or
/// unparseable previous manifest emits a warning without writing to any host.
/// Does not treat the AGY GUI TOML as an MCP host.
pub fn run_mcp_remove_managed(
    previous_manifest_path: &Path,
) -> std::result::Result<McpRemoveReport, McpUpdateError> {
    run_mcp_remove_managed_inner(
        previous_manifest_path,
        ccync_foundation::paths::claude_config_path(),
        copilot_cli_mcp_path(),
        opencode_config_path(),
        codex_config_path(),
        agy_mcp_config_path(),
    )
}

fn run_mcp_remove_managed_inner(
    previous_manifest_path: &Path,
    claude_path: Option<std::path::PathBuf>,
    copilot_path: Option<std::path::PathBuf>,
    opencode_path: Option<std::path::PathBuf>,
    codex_path: Option<std::path::PathBuf>,
    agy_path: Option<std::path::PathBuf>,
) -> std::result::Result<McpRemoveReport, McpUpdateError> {
    use crate::host_render::Host;
    use crate::merge::{write_codex_delta, write_json_provider_delta};
    use crate::state::ProjectedState;
    use std::collections::BTreeMap;

    let mut hosts_updated = Vec::new();
    let mut warnings = Vec::new();

    // Ownership is proven per host by the committed projected-state, sitting
    // beside the (previous) managed manifest. Only names ccync committed on a
    // host are removed there; a same-named user entry ccync never owned stays.
    let state_path = ProjectedState::path_beside(previous_manifest_path);
    let state = ProjectedState::load(&state_path)?;

    if state.hosts.is_empty() {
        warnings.push(
            "no committed MCP ownership found — live host entries preserved, nothing removed"
                .to_string(),
        );
        return Ok(McpRemoveReport {
            hosts_updated,
            warnings,
        });
    }

    let empty_json: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let empty_codex: BTreeMap<String, String> = BTreeMap::new();

    let host_specs = [
        (Host::Claude, claude_path),
        (Host::Copilot, copilot_path),
        (Host::OpenCode, opencode_path),
        (Host::Codex, codex_path),
        (Host::Agy, agy_path),
    ];

    for (host, path) in host_specs {
        let owned = state.owned(host);
        if owned.is_empty() {
            continue;
        }
        let Some(dest) = path else { continue };

        let result = match host_write_kind(host) {
            HostWriteKind::Json(servers_key) => {
                write_json_provider_delta(&dest, servers_key, &owned, &empty_json).map(|_| ())
            }
            HostWriteKind::Codex => write_codex_delta(&dest, &owned, &empty_codex).map(|_| ()),
        };

        match result {
            Ok(()) => hosts_updated.push(host.as_str().to_string()),
            // Partial failure is fatal. Leave the committed state intact so the
            // engine uninstall caller can retry a converging removal — that
            // caller must also preserve it on disk (see prune_build_except_mcp_state),
            // or this promise would be false and the entries stranded forever.
            Err(e) => {
                return Err(McpUpdateError::Io(std::io::Error::other(format!(
                    "MCP uninstall failed on {}: {e}; committed ownership retained — re-run `ccync uninstall` to remove the remaining entries",
                    host.as_str()
                ))));
            }
        }
    }

    Ok(McpRemoveReport {
        hosts_updated,
        warnings,
    })
}

pub(crate) fn ensure_parent(path: &Path) -> std::result::Result<(), McpUpdateError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn copilot_cli_mcp_path() -> Option<std::path::PathBuf> {
    ccync_foundation::paths::user_home().map(|h| h.join(".copilot").join("mcp-config.json"))
}

fn codex_config_path() -> Option<std::path::PathBuf> {
    ccync_foundation::paths::codex_config_path()
}

fn opencode_config_path() -> Option<std::path::PathBuf> {
    ccync_foundation::paths::config_dir().map(|c| c.join("opencode").join("opencode.json"))
}

/// The agy host's live MCP config path, `~/.gemini/config/mcp_config.json`.
///
/// Distinct from the AGY GUI junction target under `~/.gemini/config/plugins/`
/// (see `crates/projection`): this is the plain MCP server list Antigravity's
/// `agy` face reads, not the GUI's own config surface.
fn agy_mcp_config_path() -> Option<std::path::PathBuf> {
    ccync_foundation::paths::user_home()
        .map(|h| h.join(".gemini").join("config").join("mcp_config.json"))
}

/// Inner helper so tests can inject a fake home without fighting `dirs` on Windows.
fn live_mcp_target_paths_inner(
    home: Option<std::path::PathBuf>,
    config_dir: Option<std::path::PathBuf>,
) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::with_capacity(5);
    if let Some(ref h) = home {
        paths.push(h.join(".claude.json"));
        paths.push(h.join(".codex").join("config.toml"));
        paths.push(h.join(".copilot").join("mcp-config.json"));
        paths.push(h.join(".gemini").join("config").join("mcp_config.json"));
    }
    if let Some(ref c) = config_dir {
        paths.push(c.join("opencode").join("opencode.json"));
    }
    paths
}

/// Returns the live agent-config paths that the MCP projection writes to.
///
/// Order: claude `~/.claude.json`, codex `~/.codex/config.toml`,
/// copilot `~/.copilot/mcp-config.json`, agy `~/.gemini/config/mcp_config.json`,
/// opencode config dir `opencode.json`.
/// Entries are absent when the platform cannot resolve home/config directory.
/// Pure: no file I/O. Used by the first-run overwrite-visibility gate.
pub fn live_mcp_target_paths() -> Vec<std::path::PathBuf> {
    live_mcp_target_paths_inner(
        ccync_foundation::paths::user_home(),
        ccync_foundation::paths::config_dir(),
    )
}

/// Errors from the MCP projection backend that are not covered by `McpError`.
#[derive(Debug, thiserror::Error)]
pub enum McpUpdateError {
    #[error("manifest error: {0}")]
    Manifest(#[from] McpError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// An MCP entry is in an ambiguous state ccync cannot safely resolve
    /// (interrupted-transaction third state, or an unowned same-name collision).
    /// The live entry is preserved and no deletion is performed; MCP mutation
    /// stops. The message names the affected host and server.
    #[error("{0}")]
    AmbiguousProjection(String),
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests (moved from ccync-engine::mcp)
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::{codex_table_server_name, remove_codex_managed_sections};

    /// "every host read cleanly" — the default for tests that supply live
    /// fingerprints directly rather than reading host files.
    fn no_unreadable() -> std::collections::BTreeSet<String> {
        std::collections::BTreeSet::new()
    }

    /// "every live name is writer-addressable" — true for all JSON hosts and for
    /// any Codex file whose servers each have their own `[mcp_servers.NAME]`
    /// table header.
    fn no_unaddressable() -> std::collections::BTreeMap<String, std::collections::BTreeSet<String>>
    {
        std::collections::BTreeMap::new()
    }

    use ccync_foundation::health::HealthCheck;
    use std::collections::HashMap;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn decisions_block_addition_when_native_package_entry() {
        use ccync_foundation::mcp::{DeclarationIdentity, McpServerProvenance, McpSourceKind};
        use std::ffi::OsString;
        struct Restore(Vec<(&'static str, Option<OsString>)>);
        impl Drop for Restore {
            fn drop(&mut self) {
                for (key, value) in &self.0 {
                    unsafe {
                        if let Some(value) = value {
                            std::env::set_var(key, value)
                        } else {
                            std::env::remove_var(key)
                        }
                    }
                }
            }
        }
        let _guard = crate::test_support::env_guard();
        let tmp = TempDir::new().unwrap();
        let keys = [
            "USERPROFILE",
            "HOME",
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
        ];
        let _restore = Restore(
            keys.iter()
                .map(|key| (*key, std::env::var_os(key)))
                .collect(),
        );
        for key in keys {
            unsafe { std::env::set_var(key, tmp.path()) }
        }
        let paths = live_host_paths();
        let mut before = std::collections::BTreeMap::new();
        for (host, path) in &paths {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let bytes = match host {
                host_render::Host::Claude => {
                    br#"{"mcpServers":{"x":{"command":"native"}}}"#.as_slice()
                }
                host_render::Host::Codex => b"# existing\n".as_slice(),
                _ => b"{}".as_slice(),
            };
            fs::write(path, bytes).unwrap();
            before.insert(*host, bytes.to_vec());
        }
        let mut manifest: McpManifest = serde_json::from_str(
            r#"{"servers":{"x":{"type":"http","url":"https://example.com/mcp"}}}"#,
        )
        .unwrap();
        manifest.set_provenance(
            "x".into(),
            McpServerProvenance::new(
                DeclarationIdentity {
                    source: "package".into(),
                    server: "x".into(),
                },
                McpSourceKind::Package,
                None,
                "digest".into(),
                Some(
                    host_render::Host::ALL
                        .iter()
                        .map(|h| h.as_str().into())
                        .collect(),
                ),
                true,
                false,
                true,
            ),
        );
        let managed = tmp.path().join("managed.json");
        let config = tmp.path().join("machine.json");
        let preview = inspect_mcp_declarations(&managed, &config, &manifest).unwrap();
        assert_eq!(preview.decisions().len(), 5);
        assert!(preview
            .decisions()
            .iter()
            .all(|d| d.status() == McpDecisionStatus::Blocked));
        let report = run_mcp_projection_at(&managed, &config, || {
            fs::write(&managed, serde_json::to_vec(&manifest)?)?;
            Ok(())
        })
        .unwrap();
        assert!(report
            .decisions()
            .iter()
            .all(|d| d.status() == McpDecisionStatus::Blocked));
        for (host, path) in &paths {
            assert_eq!(fs::read(path).unwrap(), before[host]);
        }

        manifest.set_provenance(
            "x".into(),
            McpServerProvenance::new(
                DeclarationIdentity {
                    source: "package".into(),
                    server: "x".into(),
                },
                McpSourceKind::Package,
                None,
                "digest".into(),
                Some(
                    host_render::Host::ALL
                        .iter()
                        .map(|h| h.as_str().into())
                        .collect(),
                ),
                true,
                false,
                false,
            ),
        );
        let preview = inspect_mcp_declarations(&managed, &config, &manifest).unwrap();
        assert!(preview
            .decisions()
            .iter()
            .all(|d| d.status() != McpDecisionStatus::Applied));
        assert_eq!(
            preview
                .decisions()
                .iter()
                .find(|d| d.host() == "claude")
                .unwrap()
                .status(),
            McpDecisionStatus::Blocked
        );
        let report = run_mcp_projection_at(&managed, &config, || {
            fs::write(&managed, serde_json::to_vec(&manifest)?)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            report
                .decisions()
                .iter()
                .find(|d| d.host() == "claude")
                .unwrap()
                .status(),
            McpDecisionStatus::Blocked
        );
        for decision in preview
            .decisions()
            .iter()
            .filter(|d| d.status() == McpDecisionStatus::Planned)
        {
            assert_eq!(
                report
                    .decisions()
                    .iter()
                    .find(|d| d.host() == decision.host())
                    .unwrap()
                    .status(),
                McpDecisionStatus::Applied
            );
        }
        assert_eq!(
            fs::read(&paths[&host_render::Host::Claude]).unwrap(),
            before[&host_render::Host::Claude]
        );
    }

    #[test]
    fn mcp_layout_config_resolves_from_committed_layout() {
        use std::ffi::OsString;

        struct EnvRestore(Vec<(&'static str, Option<OsString>)>);
        impl Drop for EnvRestore {
            fn drop(&mut self) {
                for (key, value) in self.0.drain(..) {
                    unsafe {
                        match value {
                            Some(value) => std::env::set_var(key, value),
                            None => std::env::remove_var(key),
                        }
                    }
                }
            }
        }

        let _guard = crate::test_support::env_guard();
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        let legacy_config = home.join(".ccync").join("config.json");
        let explicit_config = temp.path().join("explicit-b.json");
        fs::write(&explicit_config, r#"{"secrets":{"MCP_TOKEN":"from-b"}}"#).unwrap();

        let env_keys = [
            "USERPROFILE",
            "HOME",
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
        ];
        let _previous_env = EnvRestore(
            env_keys
                .iter()
                .map(|key| (*key, std::env::var_os(key)))
                .collect(),
        );
        unsafe {
            std::env::set_var("USERPROFILE", &home);
            std::env::set_var("HOME", &home);
            std::env::set_var("APPDATA", home.join("appdata"));
            std::env::set_var("LOCALAPPDATA", home.join("localappdata"));
            std::env::set_var("XDG_CONFIG_HOME", home.join("config"));
            std::env::set_var("XDG_STATE_HOME", home.join("state"));
            std::env::set_var("XDG_CACHE_HOME", home.join("cache"));
        }

        let initial_layout = ccync_foundation::paths::EffectiveLayout::load_or_legacy().unwrap();
        let roots = initial_layout.storage_roots();
        let committed = ccync_foundation::provider::StorageLayoutCommit {
            layout_version: 1,
            roots: vec![
                ("portable", roots.portable.clone()),
                ("config", roots.config.clone()),
                ("state", roots.state.clone()),
                ("cache", roots.cache.clone()),
            ]
            .into_iter()
            .map(
                |(name, path)| ccync_foundation::provider::LayoutRootIdentity {
                    name: name.into(),
                    path,
                },
            )
            .collect(),
        };
        let bytes = serde_json::to_vec(&committed).unwrap();
        fs::create_dir_all(initial_layout.layout_path().parent().unwrap()).unwrap();
        fs::write(initial_layout.layout_path(), &bytes).unwrap();
        fs::create_dir_all(initial_layout.migration_sentinel_path().parent().unwrap()).unwrap();
        fs::write(initial_layout.migration_sentinel_path(), &bytes).unwrap();
        fs::create_dir_all(legacy_config.parent().unwrap()).unwrap();
        fs::write(
            &legacy_config,
            r#"{"secrets":{"MCP_TOKEN":"legacy-decoy"}}"#,
        )
        .unwrap();
        let committed_layout = ccync_foundation::paths::EffectiveLayout::load_or_legacy().unwrap();
        let machine_config = committed_layout.machine_config_path();
        fs::create_dir_all(machine_config.parent().unwrap()).unwrap();
        fs::write(
            &machine_config,
            r#"{"secrets":{"MCP_TOKEN":"from-layout","MCP_PASSWORD":"decoy-secret"}}"#,
        )
        .unwrap();

        let managed_json = temp.path().join("managed.json");
        fs::write(
            &managed_json,
            r#"{"servers":{"path_probe":{"type":"http","url":"https://example.test/${MCP_TOKEN}"}}}"#,
        )
        .unwrap();
        let manifest = load_manifest(&managed_json).unwrap();
        let default_unresolved = unresolved_secret_vars(&manifest);
        assert!(default_unresolved.is_empty());

        run_mcp_projection(&managed_json, || Ok(())).unwrap();
        let default_merge: Vec<(crate::host_render::Host, Vec<u8>)> = live_host_paths()
            .into_iter()
            .map(|(host, path)| (host, fs::read(path).unwrap()))
            .collect();
        run_mcp_projection_at(&managed_json, &machine_config, || Ok(())).unwrap();
        let explicit_a_merge: Vec<(crate::host_render::Host, Vec<u8>)> = live_host_paths()
            .into_iter()
            .map(|(host, path)| (host, fs::read(path).unwrap()))
            .collect();
        assert_eq!(default_merge, explicit_a_merge);
        assert!(default_merge
            .iter()
            .all(|(_, bytes)| String::from_utf8_lossy(bytes).contains("from-layout")));
        assert!(default_merge
            .iter()
            .all(|(_, bytes)| !String::from_utf8_lossy(bytes).contains("legacy-decoy")));

        run_mcp_projection_at(&managed_json, &explicit_config, || Ok(())).unwrap();
        let explicit_b_merge: Vec<(crate::host_render::Host, Vec<u8>)> = live_host_paths()
            .into_iter()
            .map(|(host, path)| (host, fs::read(path).unwrap()))
            .collect();
        assert!(explicit_b_merge
            .iter()
            .all(|(_, bytes)| String::from_utf8_lossy(bytes).contains("from-b")));
        assert!(explicit_b_merge
            .iter()
            .all(|(_, bytes)| !String::from_utf8_lossy(bytes).contains("from-a")));

        let password_manifest_path = temp.path().join("password-probe.json");
        fs::write(
            &password_manifest_path,
            r#"{"servers":{"password_probe":{"type":"stdio","command":"${MCP_PASSWORD}"}}}"#,
        )
        .unwrap();
        let password_manifest = load_manifest(&password_manifest_path).unwrap();
        assert!(unresolved_secret_vars(&password_manifest).is_empty());
        assert_eq!(
            unresolved_secret_vars_at(&password_manifest, &explicit_config)
                .get("password_probe")
                .unwrap(),
            &std::collections::BTreeSet::from(["MCP_PASSWORD".to_string()])
        );

        let default_inspection = inspect_mcp_projection(&password_manifest_path).unwrap();
        let explicit_inspection =
            inspect_mcp_projection_at(&password_manifest_path, &explicit_config).unwrap();
        assert!(default_inspection.prior_secret_skips.is_empty());
        assert_eq!(
            explicit_inspection.prior_secret_skips["password_probe"],
            std::collections::BTreeSet::from(["MCP_PASSWORD".to_string()])
        );

        fs::write(committed_layout.layout_path(), b"invalid layout").unwrap();
        assert!(run_mcp_projection(&managed_json, || Ok(())).is_err());
        assert!(inspect_mcp_projection(&password_manifest_path).is_err());

        println!("mcp_layout_config evidence: committed_layout_config_used=true; legacy_config_ignored=true; layout_load_errors_returned=true");
    }

    // ── McpVariableResolver ──────────────────────────────────────────────────

    #[test]
    fn resolver_substitutes_known_var() {
        let mut vars = HashMap::new();
        vars.insert("HOME".to_string(), "/home/user".to_string());
        let r = McpVariableResolver::new(vars);
        assert_eq!(r.resolve_string("${HOME}/vault").0, "/home/user/vault");
    }

    #[test]
    fn resolver_substitutes_multiple_vars() {
        let mut vars = HashMap::new();
        vars.insert("HOME".to_string(), "/home/user".to_string());
        vars.insert("PROJECT".to_string(), "myproject".to_string());
        let r = McpVariableResolver::new(vars);
        assert_eq!(
            r.resolve_string("${HOME}/projects/${PROJECT}").0,
            "/home/user/projects/myproject"
        );
    }

    #[test]
    fn resolver_keeps_unresolved_secret_and_reports_name() {
        let r = McpVariableResolver::empty();
        let (value, names) = r.resolve_string("${API_KEY}");
        assert_eq!(value, "${API_KEY}");
        assert_eq!(
            names,
            std::collections::BTreeSet::from(["API_KEY".to_string()])
        );
    }

    #[test]
    fn resolver_leaves_unresolved_non_secret_as_is() {
        let r = McpVariableResolver::empty();
        assert_eq!(r.resolve_string("${SOME_VAR}").0, "${SOME_VAR}");
    }

    // ── check_plugin_root_complete ───────────────────────────────────────────

    #[test]
    fn plugin_root_missing_commands_fails() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("agents")).unwrap();
        fs::create_dir(tmp.path().join("skills")).unwrap();
        assert!(matches!(
            check_plugin_root_complete(tmp.path()),
            Err(McpError::PluginRootIncomplete(_))
        ));
    }

    #[test]
    fn plugin_root_complete_ok() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("commands")).unwrap();
        fs::create_dir(tmp.path().join("agents")).unwrap();
        fs::create_dir(tmp.path().join("skills")).unwrap();
        assert!(check_plugin_root_complete(tmp.path()).is_ok());
    }

    #[test]
    fn save_projection_fails_on_incomplete_root() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("agents")).unwrap();
        fs::create_dir(tmp.path().join("skills")).unwrap();
        let out = tmp.path().join("out.json");
        let proj = McpProjection {
            schema_version: 1,
            mcp_servers: HashMap::new(),
            inputs: None,
            metadata: McpProjectionMetadata {
                ownership: "ccync-managed".into(),
                generated_at: "2026-06-09T00:00:00Z".into(),
                generated_by: "test".into(),
                secret_bearing: false,
                source_files: HashMap::new(),
                preservation: "test".into(),
            },
        };
        assert!(save_projection(&out, &proj, tmp.path()).is_err());
        assert!(!out.exists());
    }

    #[test]
    fn save_projection_ok_on_complete_root() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("commands")).unwrap();
        fs::create_dir(tmp.path().join("agents")).unwrap();
        fs::create_dir(tmp.path().join("skills")).unwrap();
        let out = tmp.path().join("out.json");
        let proj = McpProjection {
            schema_version: 1,
            mcp_servers: HashMap::new(),
            inputs: None,
            metadata: McpProjectionMetadata {
                ownership: "ccync-managed".into(),
                generated_at: "2026-06-09T00:00:00Z".into(),
                generated_by: "test".into(),
                secret_bearing: false,
                source_files: HashMap::new(),
                preservation: "test".into(),
            },
        };
        assert!(save_projection(&out, &proj, tmp.path()).is_ok());
        assert!(out.exists());
    }

    // ── McpProjectionHealthCheck ─────────────────────────────────────────────

    #[test]
    fn health_check_warns_when_projection_missing() {
        use ccync_foundation::health::Severity;
        let tmp = TempDir::new().unwrap();
        let check = McpProjectionHealthCheck::with_path(tmp.path().join("nonexistent.json"));
        let findings = check.check();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Warning);
    }

    #[test]
    fn health_check_ok_when_valid_json() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("managed.json");
        fs::write(
            &path,
            r#"{"schemaVersion":1,"mcpServers":{},"_metadata":{}}"#,
        )
        .unwrap();
        let check = McpProjectionHealthCheck::with_path(path);
        assert!(check.check().is_empty());
    }

    #[test]
    fn health_check_error_on_invalid_json() {
        use ccync_foundation::health::Severity;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("managed.json");
        fs::write(&path, "not json").unwrap();
        let check = McpProjectionHealthCheck::with_path(path);
        let findings = check.check();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Error);
    }

    // ── build_variable_map: resolution layers ────────────────────────────────

    // ── live_mcp_target_paths ─────────────────────────────────────────────────

    #[test]
    fn live_mcp_target_paths_returns_five_known_files() {
        use std::path::PathBuf;
        // Use inner helper so the test is hermetic and injects explicit
        // home/config paths without touching process-global env state.
        // ccync_foundation::paths is env-based (USERPROFILE/HOME), so the
        // production fn is now itself injectable via env override too.
        let fake_home = PathBuf::from("/fakehome");
        let fake_config = PathBuf::from("/fakeconfig");
        let paths = live_mcp_target_paths_inner(Some(fake_home), Some(fake_config));
        let s: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(
            s.iter().any(|p| p.ends_with(".claude.json")),
            "claude path missing: {s:?}"
        );
        assert!(
            s.iter()
                .any(|p| p.contains(".codex") && p.ends_with("config.toml")),
            "codex path missing: {s:?}"
        );
        assert!(
            s.iter().any(|p| p.ends_with("mcp-config.json")),
            "copilot path missing: {s:?}"
        );
        assert!(
            s.iter().any(|p| p.ends_with("opencode.json")),
            "opencode path missing: {s:?}"
        );
        assert!(
            s.iter()
                .any(|p| p.contains(".gemini") && p.ends_with("mcp_config.json")),
            "agy path missing: {s:?}"
        );
        assert_eq!(paths.len(), 5, "expected exactly 5 paths, got: {s:?}");
    }

    #[test]
    fn build_variable_map_resolves_secret_from_env() {
        // The map must pick up a secret from the environment.
        let key = "CCYNC_TEST_MCP_API_KEY";
        let _env_guard = crate::test_support::env_guard();
        std::env::set_var(key, "sekret");
        let map = build_variable_map_from(None);
        let r = McpVariableResolver::new(map);
        assert_eq!(r.resolve_string("${CCYNC_TEST_MCP_API_KEY}").0, "sekret");
        std::env::remove_var(key);
    }

    #[test]
    fn build_variable_map_reads_config_json_path_via_registry() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.json");
        // Generic registry keys: ccyncSkills and tempDir.
        fs::write(
            &cfg,
            r#"{"ccyncSkills":"/home/user/.ccync/skills","tempDir":"/tmp"}"#,
        )
        .unwrap();

        let map = build_variable_map_from(Some(&cfg));
        assert_eq!(
            map.get("CCYNC_SKILLS").map(String::as_str),
            Some("/home/user/.ccync/skills")
        );
        assert_eq!(map.get("TEMP_DIR").map(String::as_str), Some("/tmp"));
    }

    #[test]
    fn build_variable_map_reads_secrets_verbatim() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.json");
        fs::write(&cfg, r#"{"secrets":{"CONTEXT7_API_KEY":"ctx-123"}}"#).unwrap();

        let map = build_variable_map_from(Some(&cfg));
        assert_eq!(
            map.get("CONTEXT7_API_KEY").map(String::as_str),
            Some("ctx-123")
        );
    }

    #[test]
    fn build_variable_map_config_json_wins_over_process_env() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.json");
        // Set env var first; config.json should win.
        std::env::set_var("CCYNC_SKILLS", "/from_env/skills");
        fs::write(&cfg, r#"{"ccyncSkills":"/from_config/skills"}"#).unwrap();

        let map = build_variable_map_from(Some(&cfg));
        // config.json Layer 2 wins over process env Layer 1.
        assert_eq!(
            map.get("CCYNC_SKILLS").map(String::as_str),
            Some("/from_config/skills")
        );
        std::env::remove_var("CCYNC_SKILLS");
    }

    #[test]
    fn build_variable_map_no_config_local_env() {
        // config.local.env must not be read; only env + config.json are valid sources.
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.json");
        fs::write(&cfg, r#"{"secrets":{"MY_TOKEN":"from_config"}}"#).unwrap();

        // There is no legacy env file path parameter any more.
        let map = build_variable_map_from(Some(&cfg));
        assert_eq!(map.get("MY_TOKEN").map(String::as_str), Some("from_config"));
    }

    // ── Codex TOML section parsing + non-destructive merge ────────────────────

    #[test]
    fn codex_table_server_name_parses_bare_and_quoted() {
        assert_eq!(
            codex_table_server_name("[mcp_servers.memory]").as_deref(),
            Some("memory")
        );
        assert_eq!(
            codex_table_server_name("[mcp_servers.memory.env]").as_deref(),
            Some("memory")
        );
        assert_eq!(
            codex_table_server_name(r#"[mcp_servers."upstash/context7"]"#).as_deref(),
            Some("upstash/context7")
        );
        assert_eq!(codex_table_server_name("[model]"), None);
        assert_eq!(codex_table_server_name("command = \"npx\""), None);
    }

    #[test]
    fn codex_remove_preserves_non_mcp_and_user_servers() {
        let raw = "[model]\nname = \"gpt\"\n\n[mcp_servers.ccync_managed]\ncommand = \"npx\"\n\n[mcp_servers.user_kept]\ncommand = \"mine\"\n";
        let managed: std::collections::HashSet<String> =
            ["ccync_managed".to_string()].into_iter().collect();
        let out = remove_codex_managed_sections(raw, &managed);
        assert!(out.contains("[model]"), "non-MCP [model] must survive");
        assert!(out.contains("name = \"gpt\""));
        assert!(
            out.contains("[mcp_servers.user_kept]"),
            "user MCP server must survive"
        );
        assert!(
            !out.contains("ccync_managed"),
            "managed section must be removed"
        );
    }

    // ── run_mcp_remove_managed ───────────────────────────────────────────────

    #[test]
    fn uninstall_ownership_removes_committed_names_per_host() {
        use crate::host_render::Host;
        use crate::state::ProjectedState;

        let tmp = TempDir::new().unwrap();

        // The managed.json path only locates the committed-state file beside it.
        let managed_json = tmp.path().join("managed.json");
        fs::write(&managed_json, r#"{"servers":{}}"#).unwrap();

        // Committed per-host ownership — deliberately different sets per host.
        let mut state = ProjectedState::new();
        state.set_owned(
            Host::Claude,
            ["ccync-server".to_string()].into_iter().collect(),
        );
        state.set_owned(Host::Copilot, ["memory".to_string()].into_iter().collect());
        state.set_owned(
            Host::OpenCode,
            ["ccync-server".to_string()].into_iter().collect(),
        );
        state.set_owned(Host::Codex, ["memory".to_string()].into_iter().collect());
        state
            .save(&ProjectedState::path_beside(&managed_json))
            .unwrap();

        // Claude ~/.claude.json: managed server + user server + non-MCP key.
        let claude = tmp.path().join("claude.json");
        fs::write(
            &claude,
            r#"{"mcpServers":{"ccync-server":{"command":"a"},"user-server":{"command":"b"}},"theme":"dark"}"#,
        )
        .unwrap();

        // Copilot mcp-config.json: managed + user.
        let copilot = tmp.path().join("copilot.json");
        fs::write(
            &copilot,
            r#"{"mcpServers":{"memory":{"command":"npx"},"user-mcp":{"command":"mine"}}}"#,
        )
        .unwrap();

        // OpenCode opencode.json: mcp key, managed + user.
        let opencode = tmp.path().join("opencode.json");
        fs::write(
            &opencode,
            r#"{"mcp":{"ccync-server":{"type":"stdio"},"other":{"type":"stdio"}}}"#,
        )
        .unwrap();

        // Codex config.toml: [model] + managed server + user server.
        let codex = tmp.path().join("config.toml");
        fs::write(
            &codex,
            "[model]\nname = \"o3\"\n\n[mcp_servers.memory]\ncmd = \"npx\"\n\n[mcp_servers.user-tool]\ncmd = \"mine\"\n",
        )
        .unwrap();

        let report = run_mcp_remove_managed_inner(
            &managed_json,
            Some(claude.clone()),
            Some(copilot.clone()),
            Some(opencode.clone()),
            Some(codex.clone()),
            None,
        )
        .unwrap();

        assert!(
            report.warnings.is_empty(),
            "no warnings expected: {:?}",
            report.warnings
        );
        assert!(report.hosts_updated.contains(&"claude".to_string()));
        assert!(report.hosts_updated.contains(&"copilot".to_string()));
        assert!(report.hosts_updated.contains(&"opencode".to_string()));
        assert!(report.hosts_updated.contains(&"codex".to_string()));

        let claude_v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&claude).unwrap()).unwrap();
        assert!(
            claude_v["mcpServers"]["ccync-server"].is_null(),
            "managed entry must be gone from claude"
        );
        assert!(
            claude_v["mcpServers"]["user-server"].is_object(),
            "user entry must remain in claude"
        );
        assert_eq!(
            claude_v["theme"], "dark",
            "non-MCP key must remain in claude"
        );

        let copilot_v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&copilot).unwrap()).unwrap();
        assert!(
            copilot_v["mcpServers"]["memory"].is_null(),
            "managed entry must be gone from copilot"
        );
        assert!(
            copilot_v["mcpServers"]["user-mcp"].is_object(),
            "user entry must remain in copilot"
        );

        let opencode_v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&opencode).unwrap()).unwrap();
        assert!(
            opencode_v["mcp"]["ccync-server"].is_null(),
            "managed entry must be gone from opencode"
        );
        assert!(
            opencode_v["mcp"]["other"].is_object(),
            "user entry must remain in opencode"
        );

        let codex_out = fs::read_to_string(&codex).unwrap();
        assert!(
            codex_out.contains("[model]"),
            "[model] must remain in codex"
        );
        assert!(
            codex_out.contains("[mcp_servers.user-tool]"),
            "user server must remain in codex"
        );
        assert!(
            !codex_out.contains("[mcp_servers.memory]"),
            "managed server must be gone from codex"
        );
    }

    #[test]
    fn uninstall_ownership_missing_state_preserves_and_warns() {
        let tmp = TempDir::new().unwrap();
        // No projected-state.json beside this managed.json → no committed ownership.
        let managed_json = tmp.path().join("managed.json");

        // Claude config with a same-named entry ccync never committed; must survive.
        let claude = tmp.path().join("claude.json");
        let original = r#"{"mcpServers":{"user-server":{"command":"b"}}}"#;
        fs::write(&claude, original).unwrap();

        let report = run_mcp_remove_managed_inner(
            &managed_json,
            Some(claude.clone()),
            None,
            None,
            None,
            None,
        )
        .unwrap();

        assert!(
            !report.warnings.is_empty(),
            "missing committed state must warn"
        );
        assert!(
            report.hosts_updated.is_empty(),
            "no host touched without committed state"
        );
        assert_eq!(
            fs::read_to_string(&claude).unwrap(),
            original,
            "live entries preserved when ownership is unknown"
        );
    }

    #[test]
    fn uninstall_ownership_partial_failure_retains_state() {
        use crate::host_render::Host;
        use crate::state::ProjectedState;

        let tmp = TempDir::new().unwrap();
        let managed_json = tmp.path().join("managed.json");

        let mut state = ProjectedState::new();
        state.set_owned(Host::Claude, ["a".to_string()].into_iter().collect());
        let state_path = ProjectedState::path_beside(&managed_json);
        state.save(&state_path).unwrap();

        // Claude is readable but its atomic-write temp sibling is blocked → write fails.
        let claude = tmp.path().join("claude.json");
        fs::write(&claude, r#"{"mcpServers":{"a":{"command":"x"}}}"#).unwrap();
        fs::create_dir_all(tmp.path().join("claude.json.tmp")).unwrap();

        let result = run_mcp_remove_managed_inner(
            &managed_json,
            Some(claude.clone()),
            None,
            None,
            None,
            None,
        );

        assert!(result.is_err(), "partial host failure is fatal");
        assert!(
            state_path.exists(),
            "committed ownership retained for retry after a failure"
        );
    }

    #[test]
    fn mcp_projection_treats_agy_as_an_mcp_provider() {
        let _guard = crate::test_support::env_guard();
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let appdata = tmp.path().join("appdata");
        let xdg = tmp.path().join("xdg-config");
        let ccync_home = home.join(".ccync");
        let manifest_path = ccync_home
            .join("generated")
            .join("mcp")
            .join("managed.json");

        fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::create_dir_all(home.join(".copilot")).unwrap();
        fs::create_dir_all(appdata.join("Claude")).unwrap();
        fs::create_dir_all(xdg.join("opencode")).unwrap();
        fs::create_dir_all(ccync_home.join("config")).unwrap();

        fs::write(
            &manifest_path,
            r#"{
  "servers": {
    "memory": {
      "type": "http",
      "url": "https://example.test/memory"
    }
  }
}"#,
        )
        .unwrap();

        // Pre-populate ~/.claude.json with an unrelated user server + non-MCP key,
        // to prove the unified projection is non-destructive (conflict-safe).
        fs::write(
            home.join(".claude.json"),
            r#"{"mcpServers":{"user-server":{"command":"mine","args":[]}},"theme":"dark"}"#,
        )
        .unwrap();

        // Snapshot the REAL (pre-override) target paths + their existence/content
        // so we can prove afterward that no same-name side effect landed there
        // The gate list, dry-run behavior, and real writes must agree under an
        // environment override and must never touch the real home/config dir.
        let real_targets = live_mcp_target_paths();
        let real_snapshots: Vec<(std::path::PathBuf, Option<String>)> = real_targets
            .iter()
            .map(|p| (p.clone(), fs::read_to_string(p).ok()))
            .collect();

        let prior_userprofile = std::env::var_os("USERPROFILE");
        let prior_home = std::env::var_os("HOME");
        let prior_appdata = std::env::var_os("APPDATA");
        let prior_xdg = std::env::var_os("XDG_CONFIG_HOME");

        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", &home);
            std::env::set_var("HOME", &home);
            std::env::set_var("APPDATA", &appdata);
            std::env::remove_var("XDG_CONFIG_HOME");
        }

        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("XDG_CONFIG_HOME", &xdg);
            std::env::remove_var("APPDATA");
        }

        // Production gate list (no injection): must resolve entirely under the
        // injected home/config-dir, never the real one snapshotted above.
        let gate_paths = live_mcp_target_paths();
        assert_eq!(gate_paths.len(), 5, "gate list must have 5 entries");
        #[cfg(windows)]
        let injected_config_dir = &appdata;
        #[cfg(not(windows))]
        let injected_config_dir = &xdg;
        for p in &gate_paths {
            assert!(
                p.starts_with(&home) || p.starts_with(injected_config_dir),
                "gate path {} must resolve under injected home/config-dir",
                p.display()
            );
            assert!(
                !real_targets.contains(p),
                "gate path {} must not equal any real-home target",
                p.display()
            );
        }

        // Drive the real write-ahead projection. `render` is the canonical-render
        // callback; the manifest is already on disk, so it is a no-op here.
        let report = run_mcp_projection(&manifest_path, || Ok(())).unwrap();

        #[cfg(windows)]
        unsafe {
            match prior_userprofile {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
            match prior_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prior_appdata {
                Some(v) => std::env::set_var("APPDATA", v),
                None => std::env::remove_var("APPDATA"),
            }
            match prior_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
        #[cfg(not(windows))]
        unsafe {
            match prior_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prior_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match prior_appdata {
                Some(v) => std::env::set_var("APPDATA", v),
                None => std::env::remove_var("APPDATA"),
            }
        }

        assert!(report.hosts_updated.contains(&"codex".to_string()));
        assert!(
            report.hosts_updated.contains(&"agy".to_string()),
            "agy is a registered MCP host and must be written"
        );

        // The unified MCP projection targets Claude = Claude Code (`~/.claude.json`),
        // NOT Claude Desktop, and never gemini-cli.
        // (1) Claude Code target written, server present.
        assert!(report.hosts_updated.contains(&"claude".to_string()));
        let claude_json = home.join(".claude.json");
        assert!(
            claude_json.exists(),
            "Claude Code ~/.claude.json must be written"
        );
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&claude_json).unwrap()).unwrap();
        assert!(
            v["mcpServers"]["memory"].is_object(),
            "server projected into ~/.claude.json mcpServers"
        );
        // Non-destructive: pre-existing unrelated server + non-MCP key preserved.
        assert!(
            v["mcpServers"]["user-server"].is_object(),
            "unrelated user MCP server must be preserved"
        );
        assert_eq!(
            v["theme"], "dark",
            "non-MCP top-level key must be preserved"
        );

        // (1b) codex + copilot writes land under injected home, with content.
        let codex_toml = home.join(".codex").join("config.toml");
        assert!(
            codex_toml.exists(),
            "codex config.toml must be written under injected home"
        );
        let copilot_json = home.join(".copilot").join("mcp-config.json");
        assert!(
            copilot_json.exists(),
            "copilot mcp-config.json must be written under injected home"
        );
        let copilot_v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&copilot_json).unwrap()).unwrap();
        assert!(
            copilot_v["mcpServers"]["memory"].is_object()
                || copilot_v["servers"]["memory"].is_object(),
            "server projected into copilot mcp-config.json"
        );

        // (1c') agy write lands under injected home, with content.
        let agy_json = home.join(".gemini").join("config").join("mcp_config.json");
        assert!(
            agy_json.exists(),
            "agy mcp_config.json must be written under injected home"
        );
        let agy_v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&agy_json).unwrap()).unwrap();
        assert!(
            agy_v["mcpServers"]["memory"].is_object(),
            "server projected into agy mcp_config.json"
        );

        // (1c) opencode write lands under injected config-dir (XDG on Unix;
        // APPDATA-relative resolution N/A here on the Windows CI host — the
        // gate-path assertion above already proved the resolved target).
        assert!(report.hosts_updated.contains(&"opencode".to_string()));

        // (2) Claude Desktop config must NOT be written (D-11).
        assert!(
            !appdata
                .join("Claude")
                .join("claude_desktop_config.json")
                .exists(),
            "Claude Desktop config must not be written"
        );

        // (3) gemini-cli is not an MCP target.
        assert!(!report.hosts_updated.contains(&"gemini".to_string()));
        assert!(!report.hosts_updated.contains(&"gemini-cli".to_string()));

        // (4) No same-name side effect landed in the REAL home/config dir —
        // every real target snapshotted before the override is byte-identical
        // to its pre-run state (unchanged if absent, unchanged if present).
        for (real_path, before) in &real_snapshots {
            let after = fs::read_to_string(real_path).ok();
            assert_eq!(
                &after,
                before,
                "real-home target {} must be unaffected by the env-overridden run",
                real_path.display()
            );
        }
    }

    #[test]
    fn projection_tolerates_unresolved_prior_and_retains_skipped_owned_server() {
        use crate::host_render::Host;
        use crate::state::ProjectedState;
        use std::collections::BTreeSet;

        let _guard = crate::test_support::env_guard();
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let appdata = tmp.path().join("appdata");
        let xdg = tmp.path().join("xdg-config");
        let managed_json = tmp.path().join("mcp").join("managed.json");
        fs::create_dir_all(managed_json.parent().unwrap()).unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::create_dir_all(home.join(".copilot")).unwrap();
        fs::create_dir_all(home.join(".gemini").join("config")).unwrap();
        fs::create_dir_all(appdata.join("opencode")).unwrap();
        fs::create_dir_all(xdg.join("opencode")).unwrap();

        let prior = serde_json::json!({
            "servers": {
                "blocked": { "type": "stdio", "command": "old", "env": { "API_KEY": "${API_KEY}" } }
            }
        });
        fs::write(&managed_json, serde_json::to_string(&prior).unwrap()).unwrap();

        fs::write(
            home.join(".claude.json"),
            r#"{"mcpServers":{"blocked":{"command":"old"}}}"#,
        )
        .unwrap();
        fs::write(
            home.join(".copilot").join("mcp-config.json"),
            r#"{"mcpServers":{"blocked":{"type":"local","command":"old"}}}"#,
        )
        .unwrap();
        fs::write(
            xdg.join("opencode").join("opencode.json"),
            r#"{"mcp":{"blocked":{"type":"local","command":["old"]}}}"#,
        )
        .unwrap();
        fs::write(
            appdata.join("opencode").join("opencode.json"),
            r#"{"mcp":{"blocked":{"type":"local","command":["old"]}}}"#,
        )
        .unwrap();
        fs::write(
            home.join(".gemini").join("config").join("mcp_config.json"),
            r#"{"mcpServers":{"blocked":{"command":"old"}}}"#,
        )
        .unwrap();
        fs::write(
            home.join(".codex").join("config.toml"),
            "[mcp_servers.blocked]\ncommand = \"old\"\n",
        )
        .unwrap();

        let mut state = ProjectedState::new();
        for host in Host::ALL {
            state.set_owned(host, BTreeSet::from(["blocked".to_string()]));
        }
        state
            .save(&ProjectedState::path_beside(&managed_json))
            .unwrap();

        let prior_userprofile = std::env::var_os("USERPROFILE");
        let prior_home = std::env::var_os("HOME");
        let prior_appdata = std::env::var_os("APPDATA");
        let prior_xdg = std::env::var_os("XDG_CONFIG_HOME");
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", &home);
            std::env::set_var("HOME", &home);
            std::env::set_var("APPDATA", &appdata);
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("XDG_CONFIG_HOME", &xdg);
            std::env::remove_var("APPDATA");
        }

        let result = run_mcp_projection(&managed_json, || {
            let desired = serde_json::json!({
                "servers": {
                    "blocked": { "type": "stdio", "command": "new", "env": { "API_KEY": "${API_KEY}" } },
                    "unowned_blocked": { "type": "stdio", "command": "fail", "env": { "SECRET": "${SECRET}" } },
                    "healthy": { "type": "http", "url": "https://example.test/healthy" }
                }
            });
            fs::write(&managed_json, serde_json::to_string(&desired).unwrap())?;
            Ok(())
        });

        #[cfg(windows)]
        unsafe {
            match prior_userprofile {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
            match prior_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prior_appdata {
                Some(v) => std::env::set_var("APPDATA", v),
                None => std::env::remove_var("APPDATA"),
            }
            match prior_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
        #[cfg(not(windows))]
        unsafe {
            match prior_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prior_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match prior_appdata {
                Some(v) => std::env::set_var("APPDATA", v),
                None => std::env::remove_var("APPDATA"),
            }
        }

        let report = result.unwrap();
        assert!(
            report.host_failures.is_empty(),
            "all hosts should remain reachable: {:?}",
            report.host_failures
        );
        assert_eq!(report.hosts_updated.len(), Host::ALL.len());
        let state = ProjectedState::load(&ProjectedState::path_beside(&managed_json)).unwrap();
        for host in Host::ALL {
            assert!(
                state.owned(host).contains("blocked"),
                "blocked ownership missing on {host:?}: {:?}",
                state.owned(host)
            );
            assert!(state.owned(host).contains("healthy"));
            assert!(
                !state.owned(host).contains("unowned_blocked"),
                "unowned_blocked must not be owned on {host:?}"
            );
        }
        let claude: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(home.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(
            claude["mcpServers"]["blocked"]["command"], "old",
            "owned live entry for skipped server must remain byte-identical"
        );
        assert_eq!(
            claude["mcpServers"]["healthy"]["url"],
            "https://example.test/healthy"
        );
        assert!(
            claude["mcpServers"].get("unowned_blocked").is_none(),
            "desired server with unresolved secret must reach no host"
        );
    }

    #[test]
    fn projection_does_not_legacy_bootstrap_skipped_prior_server() {
        use crate::host_render::Host;
        use crate::state::ProjectedState;

        let _guard = crate::test_support::env_guard();
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let appdata = tmp.path().join("appdata");
        let xdg = tmp.path().join("xdg-config");
        let managed_json = tmp.path().join("mcp").join("managed.json");
        fs::create_dir_all(managed_json.parent().unwrap()).unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::create_dir_all(home.join(".copilot")).unwrap();
        fs::create_dir_all(home.join(".gemini").join("config")).unwrap();
        fs::create_dir_all(appdata.join("opencode")).unwrap();
        fs::create_dir_all(xdg.join("opencode")).unwrap();
        fs::write(
            &managed_json,
            r#"{"servers":{"blocked":{"command":"old","env":{"API_KEY":"${API_KEY}"}}}}"#,
        )
        .unwrap();
        fs::write(
            home.join(".claude.json"),
            r#"{"mcpServers":{"blocked":{"command":"old"}}}"#,
        )
        .unwrap();
        fs::write(
            home.join(".copilot").join("mcp-config.json"),
            r#"{"mcpServers":{"blocked":{"command":"old"}}}"#,
        )
        .unwrap();
        fs::write(
            xdg.join("opencode").join("opencode.json"),
            r#"{"mcp":{"blocked":{"command":["old"]}}}"#,
        )
        .unwrap();
        fs::write(
            home.join(".gemini").join("config").join("mcp_config.json"),
            r#"{"mcpServers":{"blocked":{"command":"old"}}}"#,
        )
        .unwrap();
        fs::write(
            home.join(".codex").join("config.toml"),
            "[mcp_servers.blocked]\ncommand = \"old\"\n",
        )
        .unwrap();

        #[cfg(windows)]
        let old_userprofile = std::env::var_os("USERPROFILE");
        let old_home = std::env::var_os("HOME");
        #[cfg(windows)]
        let old_appdata = std::env::var_os("APPDATA");
        #[cfg(not(windows))]
        let old_xdg = std::env::var_os("XDG_CONFIG_HOME");
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", &home);
            std::env::set_var("HOME", &home);
            std::env::set_var("APPDATA", &appdata);
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("XDG_CONFIG_HOME", &xdg);
        }
        let result = run_mcp_projection(&managed_json, || {
            fs::write(&managed_json, r#"{"servers":{}}"#)?;
            Ok(())
        });
        #[cfg(windows)]
        unsafe {
            match old_userprofile {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
            match old_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match old_appdata {
                Some(v) => std::env::set_var("APPDATA", v),
                None => std::env::remove_var("APPDATA"),
            }
        }
        #[cfg(not(windows))]
        unsafe {
            match old_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match old_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }

        result.unwrap();
        let state = ProjectedState::load(&ProjectedState::path_beside(&managed_json)).unwrap();
        for host in Host::ALL {
            assert!(
                state.owned(host).is_empty(),
                "skipped prior name must not be claimed on {host:?}"
            );
        }
    }

    #[test]
    fn prepare_projection_recovery_commits_bootstraps_and_rejects_third() {
        use crate::entry::{fingerprint_json, EntryFingerprint};
        use crate::host_render::{render_host_entries, Host};
        use crate::state::ProjectedState;
        use crate::transaction::ProjectionTransaction;
        use ccync_foundation::mcp::McpServer;
        use serde_json::json;
        use std::collections::BTreeMap;
        use tempfile::TempDir;

        let post = fingerprint_json(&json!({"command":"new"}));
        let pre = fingerprint_json(&json!({"command":"old"}));

        fn one_host(
            fp: EntryFingerprint,
            name: &str,
        ) -> BTreeMap<String, BTreeMap<String, EntryFingerprint>> {
            let mut inner = BTreeMap::new();
            inner.insert(name.to_string(), fp);
            let mut live = BTreeMap::new();
            live.insert("claude".to_string(), inner);
            live
        }

        // --- recovery: all-landed txn commits after validation, then clears ---
        let tmp = TempDir::new().unwrap();
        let managed = tmp.path().join("managed.json");
        let state_path = ProjectedState::path_beside(&managed);
        let txn_path = ProjectionTransaction::path_beside(&managed);
        let mut txn = ProjectionTransaction::new("t");
        txn.record(Host::Claude, "a", &pre, &post);
        txn.save(&txn_path).unwrap();
        let live = one_host(post.clone(), "a");

        let sp = state_path.clone();
        let (prep, ()) = prepare_projection_recovery(
            &managed,
            None,
            &live,
            &no_unaddressable(),
            &no_unreadable(),
            PrepareMode::Normal,
            || {
                let s = ProjectedState::load(&sp).unwrap();
                assert!(
                    !s.owned(Host::Claude).contains("a"),
                    "not committed before validation"
                );
                assert!(txn_path.exists());
                Ok(())
            },
        )
        .unwrap();
        assert!(prep.owned_by_host.get("claude").unwrap().contains("a"));
        assert!(ProjectedState::load(&state_path)
            .unwrap()
            .owned(Host::Claude)
            .contains("a"));
        assert!(
            prep.actions
                .iter()
                .any(|action| action.contains("pending transaction t found")),
            "normal projection must disclose the pending transaction recovery"
        );
        assert!(
            ProjectionTransaction::load(&txn_path).unwrap().is_none(),
            "recovered transaction cleared"
        );

        // --- third state: fatal, state + journal untouched, no render ---
        let tmp2 = TempDir::new().unwrap();
        let managed2 = tmp2.path().join("managed.json");
        let txn2 = ProjectionTransaction::path_beside(&managed2);
        let state2 = ProjectedState::path_beside(&managed2);
        let mut t2 = ProjectionTransaction::new("t2");
        t2.record(Host::Claude, "a", &pre, &post);
        t2.save(&txn2).unwrap();
        let txn2_bytes = std::fs::read(&txn2).unwrap();
        let weird = fingerprint_json(&json!({"command":"weird"}));
        let live2 = one_host(weird, "a");
        let mut rendered = false;
        let r = prepare_projection_recovery(
            &managed2,
            None,
            &live2,
            &no_unaddressable(),
            &no_unreadable(),
            PrepareMode::Normal,
            || {
                rendered = true;
                Ok::<(), McpUpdateError>(())
            },
        );
        assert!(
            matches!(r, Err(McpUpdateError::AmbiguousProjection(_))),
            "third state is fatal"
        );
        assert!(!rendered, "render not called on fatal");
        assert!(!state2.exists(), "no state written on third state");
        assert_eq!(
            std::fs::read(&txn2).unwrap(),
            txn2_bytes,
            "journal untouched"
        );

        // A prior-render match without ownership does not grant ownership.
        let tmp3 = TempDir::new().unwrap();
        let managed3 = tmp3.path().join("managed.json");
        let state3 = ProjectedState::path_beside(&managed3);
        let prior = McpManifest {
            provenance: Default::default(),
            servers: std::iter::once((
                "m".to_string(),
                McpServer {
                    server_type: Some("stdio".to_string()),
                    command: Some("node".to_string()),
                    args: Some(vec!["m.js".to_string()]),
                    env: None,
                    cwd: None,
                    url: None,
                    headers: None,
                },
            ))
            .collect(),
            inputs: None,
        };
        let m_fp = render_host_entries(Host::Claude, &prior)
            .unwrap()
            .get("m")
            .unwrap()
            .fingerprint()
            .unwrap();
        let live3 = one_host(m_fp, "m");
        let (prep3, ()) = prepare_projection_recovery(
            &managed3,
            Some(&prior),
            &live3,
            &no_unaddressable(),
            &no_unreadable(),
            PrepareMode::Normal,
            || Ok(()),
        )
        .unwrap();
        assert!(prep3.owned_by_host.is_empty());
        assert!(!state3.exists());
        assert!(!prep3
            .actions
            .iter()
            .any(|action| action.contains("bootstrap")));

        // --- inspect mode: reports but writes nothing ---
        let tmp4 = TempDir::new().unwrap();
        let managed4 = tmp4.path().join("managed.json");
        let txn4 = ProjectionTransaction::path_beside(&managed4);
        let state4 = ProjectedState::path_beside(&managed4);
        let mut t4 = ProjectionTransaction::new("t4");
        t4.record(Host::Claude, "a", &pre, &post);
        t4.save(&txn4).unwrap();
        let txn4_bytes = std::fs::read(&txn4).unwrap();
        let live4 = one_host(post.clone(), "a");
        let (prep4, ()) = prepare_projection_recovery(
            &managed4,
            None,
            &live4,
            &no_unaddressable(),
            &no_unreadable(),
            PrepareMode::Inspect,
            || Ok(()),
        )
        .unwrap();
        assert!(
            prep4.owned_by_host.get("claude").unwrap().contains("a"),
            "inspect reports grant"
        );
        assert!(!state4.exists(), "inspect writes no state");
        assert_eq!(
            std::fs::read(&txn4).unwrap(),
            txn4_bytes,
            "inspect leaves journal intact"
        );
    }

    #[test]
    fn read_live_codex_entries_refuses_to_claim_an_inline_defined_server() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("config.toml");

        // Legal TOML, and semantically exactly what ccync renders — so the
        // semantic reader sees `foo` and would fingerprint-match it. But the
        // line-based writer only strips `[mcp_servers.NAME]` headers, and there
        // is no header here: `foo` lives as an inline key under `[mcp_servers]`.
        fs::write(
            &dest,
            "[mcp_servers]\nfoo = { command = \"x\" }\n\n[mcp_servers.bar]\ncommand = \"y\"\n",
        )
        .unwrap();

        let (entries, unaddressable) = read_live_codex_entries(&dest).unwrap();
        assert!(
            entries.contains_key("foo"),
            "the semantic reader still sees foo"
        );
        assert!(entries.contains_key("bar"));
        assert_eq!(
            unaddressable.iter().collect::<Vec<_>>(),
            vec!["foo"],
            "only the header-less server is unaddressable"
        );

        // Prove the writer really cannot strip `foo`, i.e. that the refusal is
        // not conservatism — it is the actual limit of the writer.
        let strip: std::collections::HashSet<String> =
            ["foo".to_string(), "bar".to_string()].into_iter().collect();
        let stripped = remove_codex_managed_sections(&fs::read_to_string(&dest).unwrap(), &strip);
        assert!(
            stripped.contains("foo = { command = \"x\" }"),
            "the line-based writer cannot remove an inline-defined server: {stripped}"
        );
        assert!(
            !stripped.contains("[mcp_servers.bar]"),
            "but it does remove a header-declared one: {stripped}"
        );
    }

    #[test]
    fn malformed_codex_config_failure_does_not_disclose_source_content() {
        use crate::host_render::Host;

        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("config.toml");
        let secret = "sk-live-SECRET123";
        let malformed = format!(
            "[mcp_servers.private]\ncommand = \"runner\"\nenv = {{ API_KEY = \"{secret}\"oops\" }}\n"
        );
        let legacy_failure = toml::from_str::<toml::Value>(&malformed)
            .expect_err("fixture must remain malformed")
            .to_string();
        assert!(legacy_failure.contains(secret));
        assert!(legacy_failure.contains("API_KEY"));
        fs::write(&dest, &malformed).unwrap();

        let host_paths = [(Host::Codex, dest)].into_iter().collect();
        let live = read_live_entries(&host_paths);
        let failure = live
            .failures
            .get(&Host::Codex)
            .expect("malformed Codex config must be reported");

        assert!(failure.contains("invalid Codex config.toml"));
        assert!(failure.contains("byte span"));
        assert!(!failure.contains(secret), "secret leaked in: {failure}");
        assert!(
            !failure.contains("API_KEY"),
            "source snippet leaked in: {failure}"
        );
    }

    #[test]
    fn prepare_projection_recovery_refuses_a_transaction_on_an_unreadable_host() {
        use crate::entry::{fingerprint_json, EntryFingerprint};
        use crate::host_render::Host;
        use crate::state::ProjectedState;
        use crate::transaction::ProjectionTransaction;
        use serde_json::json;
        use std::collections::{BTreeMap, BTreeSet};
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let managed = tmp.path().join("managed.json");
        let txn_path = ProjectionTransaction::path_beside(&managed);
        let state_path = ProjectedState::path_beside(&managed);

        // An interrupted removal on codex: pre = the old entry, post = absent.
        let pre = fingerprint_json(&json!({"command":"old"}));
        let mut txn = ProjectionTransaction::new("t");
        txn.record(Host::Codex, "srv", &pre, &EntryFingerprint::Absent);
        txn.save(&txn_path).unwrap();
        let txn_bytes = std::fs::read(&txn_path).unwrap();

        // codex's config is unparseable, so it contributes no live fingerprints.
        // Classifying `srv` would read "absent" and conclude the removal LANDED —
        // granting ownership on the strength of a file that was never read.
        let live: BTreeMap<String, BTreeMap<String, EntryFingerprint>> = BTreeMap::new();
        let unreadable: BTreeSet<String> = ["codex".to_string()].into();

        let mut rendered = false;
        let result = prepare_projection_recovery(
            &managed,
            None,
            &live,
            &no_unaddressable(),
            &unreadable,
            PrepareMode::Normal,
            || {
                rendered = true;
                Ok::<(), McpUpdateError>(())
            },
        );

        match result {
            Err(McpUpdateError::AmbiguousProjection(msg)) => {
                assert!(msg.contains("codex/srv"), "names the blocked pair: {msg}");
                assert!(msg.contains("no deletion performed"), "{msg}");
            }
            other => panic!(
                "expected a fail-closed refusal, got {other:?}",
                other = other.map(|_| ())
            ),
        }
        assert!(!rendered, "render must not run");
        assert!(
            !state_path.exists(),
            "no ownership granted from an unread file"
        );
        assert_eq!(
            std::fs::read(&txn_path).unwrap(),
            txn_bytes,
            "journal retained for a later run that can read the host"
        );
    }

    #[test]
    fn projection_transaction_unwritten_host_gains_no_ownership() {
        use crate::entry::EntryFingerprint;
        use crate::host_render::{render_host_entries, Host, HostEntry};
        use crate::state::ProjectedState;
        use ccync_foundation::mcp::McpServer;
        use std::collections::{BTreeMap, BTreeSet};
        use tempfile::TempDir;

        let manifest = McpManifest {
            provenance: Default::default(),
            servers: std::iter::once((
                "srv-a".to_string(),
                McpServer {
                    server_type: Some("stdio".to_string()),
                    command: Some("node".to_string()),
                    args: Some(vec!["a.js".to_string()]),
                    env: None,
                    cwd: None,
                    url: None,
                    headers: None,
                },
            ))
            .collect(),
            inputs: None,
        };
        let desired_by_host: BTreeMap<Host, BTreeMap<String, HostEntry>> = Host::ALL
            .iter()
            .map(|h| (*h, render_host_entries(*h, &manifest).unwrap()))
            .collect();

        let tmp = TempDir::new().unwrap();
        let managed = tmp.path().join("managed.json");

        // OpenCode's config path did not resolve on this platform, so it is
        // absent from host_paths and ccync writes nothing to it.
        let mut host_paths = BTreeMap::new();
        host_paths.insert(Host::Claude, tmp.path().join("claude.json"));
        host_paths.insert(Host::Copilot, tmp.path().join("copilot.json"));
        host_paths.insert(Host::Codex, tmp.path().join("codex.toml"));

        let empty_live: BTreeMap<String, BTreeMap<String, EntryFingerprint>> = BTreeMap::new();
        let empty_committed: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

        let report = apply_projection_transaction(
            &managed,
            &host_paths,
            &desired_by_host,
            &empty_live,
            &no_unaddressable(),
            &empty_committed,
            &BTreeMap::new(),
            "t",
            |_: &FaultPoint| Ok::<(), McpUpdateError>(()),
        )
        .unwrap();

        assert_eq!(
            report.hosts_written.len(),
            3,
            "only the resolvable hosts are written"
        );
        assert!(!report.hosts_written.contains(&"opencode".to_string()));

        // The point: an unwritten host must not be recorded as owned. Recording
        // it would let a later run delete a same-named entry the user wrote —
        // ownership asserted with zero proof.
        let state = ProjectedState::load(&ProjectedState::path_beside(&managed)).unwrap();
        assert!(
            state.owned(Host::OpenCode).is_empty(),
            "an absent host must gain no ownership, got {:?}",
            state.owned(Host::OpenCode)
        );
        assert!(
            state.owned(Host::Claude).contains("srv-a"),
            "written hosts still commit"
        );

        // Prove the consequence directly: OpenCode later resolves and the user
        // has their own hand-written `srv-a` there. `srv-a` has since left the
        // desired manifest, so uninstall consults committed ownership — which
        // must not authorize deleting the user's entry.
        let opencode = tmp.path().join("opencode.json");
        let user_owned = r#"{"mcp":{"srv-a":{"type":"local","command":["mine"]}}}"#;
        fs::write(&opencode, user_owned).unwrap();
        run_mcp_remove_managed_inner(&managed, None, None, Some(opencode.clone()), None, None)
            .unwrap();
        assert_eq!(
            fs::read_to_string(&opencode).unwrap(),
            user_owned,
            "the user's same-named entry survives: ccync never proved it owned srv-a on opencode"
        );
    }

    #[test]
    fn public_projection_retains_journal_on_late_write_failure() {
        use std::ffi::OsString;
        use tempfile::TempDir;
        struct Restore(Vec<(&'static str, Option<OsString>)>);
        impl Drop for Restore {
            fn drop(&mut self) {
                for (key, value) in &self.0 {
                    unsafe {
                        if let Some(value) = value {
                            std::env::set_var(key, value);
                        } else {
                            std::env::remove_var(key);
                        }
                    }
                }
            }
        }
        let _guard = crate::test_support::env_guard();
        let tmp = TempDir::new().unwrap();
        let keys = [
            "USERPROFILE",
            "HOME",
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
        ];
        let _restore = Restore(
            keys.iter()
                .map(|key| (*key, std::env::var_os(key)))
                .collect(),
        );
        for key in keys {
            unsafe {
                std::env::set_var(key, tmp.path());
            }
        }
        let paths = live_host_paths();
        let ordered: Vec<_> = crate::host_render::Host::ALL
            .iter()
            .filter_map(|host| paths.get(host).map(|path| (*host, path)))
            .collect();
        assert!(ordered.len() >= 2);
        let first_host = *paths.keys().next().unwrap();
        let managed = tmp.path().join("managed.json");
        std::fs::write(
            &managed,
            r#"{"servers":{"srv":{"type":"http","url":"https://example.test/node"}}}"#,
        )
        .unwrap();
        let config = tmp.path().join("config.json");
        assert!(run_mcp_projection_at_with_fault(
            &managed,
            &config,
            || Ok(()),
            |point| {
                if *point == FaultPoint::AfterHost(first_host) {
                    Err(McpUpdateError::AmbiguousProjection("crash".into()))
                } else {
                    Ok(())
                }
            }
        )
        .is_err());
        assert!(projection_journal_retained(&managed));
        let (_, written) = ordered.iter().find(|(_, path)| path.is_file()).unwrap();
        let contents = std::fs::read_to_string(written).unwrap();
        assert!(contents.contains("node"));
        std::fs::write(written, contents.replace("node", "different-command")).unwrap();
        let journal = std::fs::read(crate::transaction::ProjectionTransaction::path_beside(
            &managed,
        ))
        .unwrap();
        let state_before =
            std::fs::read(crate::state::ProjectedState::path_beside(&managed)).unwrap();
        assert!(matches!(
            run_mcp_projection_at(&managed, &config, || Ok(())),
            Err(McpUpdateError::AmbiguousProjection(_))
        ));
        assert_eq!(
            std::fs::read(crate::transaction::ProjectionTransaction::path_beside(
                &managed
            ))
            .unwrap(),
            journal
        );
        assert_eq!(
            std::fs::read(crate::state::ProjectedState::path_beside(&managed)).unwrap(),
            state_before
        );
    }

    #[test]
    fn recovery_withdrawal_and_validation() {
        use crate::entry::{fingerprint_json, EntryFingerprint};
        use crate::host_render::Host;
        use crate::state::ProjectedState;
        use crate::transaction::ProjectionTransaction;
        use std::collections::{BTreeMap, BTreeSet};
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let managed = tmp.path().join("managed.json");
        let state_path = ProjectedState::path_beside(&managed);
        let txn_path = ProjectionTransaction::path_beside(&managed);
        let old = fingerprint_json(&serde_json::json!({"command":"old"}));
        let new = fingerprint_json(&serde_json::json!({"command":"new"}));
        let mut state = ProjectedState::new();
        state.set_owned(
            Host::Claude,
            ["removed".to_string(), "unchanged".to_string()].into(),
        );
        state.save(&state_path).unwrap();
        let original_state = std::fs::read(&state_path).unwrap();
        let mut txn = ProjectionTransaction::new("recovery");
        txn.record(Host::Claude, "removed", &old, &EntryFingerprint::Absent);
        txn.record(Host::Claude, "unowned", &old, &EntryFingerprint::Absent);
        txn.record(Host::Claude, "unchanged", &old, &new);
        txn.record(Host::Claude, "added", &EntryFingerprint::Absent, &new);
        txn.save(&txn_path).unwrap();
        let original_txn = std::fs::read(&txn_path).unwrap();
        let live = BTreeMap::from([(
            "claude".to_string(),
            BTreeMap::from([("unchanged".to_string(), old), ("added".to_string(), new)]),
        )]);

        let result = prepare_projection_recovery(
            &managed,
            None,
            &live,
            &BTreeMap::new(),
            &BTreeSet::new(),
            PrepareMode::Normal,
            || Err::<(), _>(McpUpdateError::AmbiguousProjection("invalid input".into())),
        );
        assert!(result.is_err());
        assert_eq!(std::fs::read(&state_path).unwrap(), original_state);
        assert_eq!(std::fs::read(&txn_path).unwrap(), original_txn);
        let (prep, ()) = prepare_projection_recovery(
            &managed,
            None,
            &live,
            &BTreeMap::new(),
            &BTreeSet::new(),
            PrepareMode::Normal,
            || Ok(()),
        )
        .unwrap();
        assert!(prep
            .actions
            .iter()
            .any(|action| action == "recovered 2 landed removal(s) on claude; ownership revoked"));
        assert!(prep
            .actions
            .iter()
            .any(|action| action == "recovered 1 landed entr(y/ies) on claude"));
        let owned = ProjectedState::load(&state_path)
            .unwrap()
            .owned(Host::Claude);
        assert_eq!(owned, ["added".to_string(), "unchanged".to_string()].into());
        assert!(!txn_path.exists());

        txn.version += 1;
        txn.save(&txn_path).unwrap();
        let before = std::fs::read(&state_path).unwrap();
        assert!(prepare_projection_recovery(
            &managed,
            None,
            &live,
            &BTreeMap::new(),
            &BTreeSet::new(),
            PrepareMode::Normal,
            || Ok(())
        )
        .is_err());
        assert_eq!(std::fs::read(&state_path).unwrap(), before);
        assert!(txn_path.exists());
    }

    #[test]
    fn projection_transaction_crash_matrix_recovers() {
        use crate::entry::EntryFingerprint;
        use crate::host_render::{render_host_entries, Host, HostEntry};
        use crate::state::ProjectedState;
        use crate::transaction::ProjectionTransaction;
        use ccync_foundation::mcp::McpServer;
        use std::collections::{BTreeMap, BTreeSet};
        use tempfile::TempDir;

        // One stdio server rendered for all four hosts.
        let manifest = McpManifest {
            provenance: Default::default(),
            servers: std::iter::once((
                "srv".to_string(),
                McpServer {
                    server_type: Some("stdio".to_string()),
                    command: Some("node".to_string()),
                    args: Some(vec!["s.js".to_string()]),
                    env: None,
                    cwd: None,
                    url: None,
                    headers: None,
                },
            ))
            .collect(),
            inputs: None,
        };
        let desired_by_host: BTreeMap<Host, BTreeMap<String, HostEntry>> = Host::ALL
            .iter()
            .map(|h| (*h, render_host_entries(*h, &manifest).unwrap()))
            .collect();

        fn setup(tmp: &TempDir) -> (std::path::PathBuf, BTreeMap<Host, std::path::PathBuf>) {
            let mut hp = BTreeMap::new();
            hp.insert(Host::Claude, tmp.path().join("claude.json"));
            hp.insert(Host::Copilot, tmp.path().join("copilot.json"));
            hp.insert(Host::OpenCode, tmp.path().join("opencode.json"));
            hp.insert(Host::Codex, tmp.path().join("codex.toml"));
            (tmp.path().join("managed.json"), hp)
        }

        // Live map reflecting all four hosts written (live == post).
        let all_written_live = || -> BTreeMap<String, BTreeMap<String, EntryFingerprint>> {
            let mut live = BTreeMap::new();
            for h in Host::ALL {
                let fp = desired_by_host[&h]["srv"].fingerprint().unwrap();
                let mut inner = BTreeMap::new();
                inner.insert("srv".to_string(), fp);
                live.insert(h.as_str().to_string(), inner);
            }
            live
        };

        let empty_live: BTreeMap<String, BTreeMap<String, EntryFingerprint>> = BTreeMap::new();
        let empty_committed: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let no_fault = |_: &FaultPoint| Ok::<(), McpUpdateError>(());

        // --- happy path: writes, commits ownership, clears journal ---
        let tmp = TempDir::new().unwrap();
        let (managed, hp) = setup(&tmp);
        let report = apply_projection_transaction(
            &managed,
            &hp,
            &desired_by_host,
            &empty_live,
            &no_unaddressable(),
            &empty_committed,
            &BTreeMap::new(),
            "t",
            no_fault,
        )
        .unwrap();
        assert_eq!(report.hosts_written.len(), 4, "all four hosts written");
        let state = ProjectedState::load(&ProjectedState::path_beside(&managed)).unwrap();
        assert!(state.owned(Host::Claude).contains("srv"));
        assert!(
            ProjectionTransaction::load(&ProjectionTransaction::path_beside(&managed))
                .unwrap()
                .is_none()
        );
        let claude: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&hp[&Host::Claude]).unwrap()).unwrap();
        assert_eq!(claude["mcpServers"]["srv"]["command"], "node");

        // --- crash before commit: hosts written, ownership NOT committed, journal kept ---
        let tmp2 = TempDir::new().unwrap();
        let (managed2, hp2) = setup(&tmp2);
        let r = apply_projection_transaction(
            &managed2,
            &hp2,
            &desired_by_host,
            &empty_live,
            &no_unaddressable(),
            &empty_committed,
            &BTreeMap::new(),
            "t",
            |p: &FaultPoint| {
                if *p == FaultPoint::BeforeCommit {
                    Err(McpUpdateError::AmbiguousProjection("crash".into()))
                } else {
                    Ok(())
                }
            },
        );
        assert!(r.is_err());
        let txn_path2 = ProjectionTransaction::path_beside(&managed2);
        assert!(
            ProjectionTransaction::load(&txn_path2).unwrap().is_some(),
            "journal retained"
        );
        assert!(
            !ProjectedState::path_beside(&managed2).exists(),
            "ownership not committed"
        );
        let live2 = all_written_live();
        let (prep2, ()) = prepare_projection_recovery(
            &managed2,
            None,
            &live2,
            &no_unaddressable(),
            &no_unreadable(),
            PrepareMode::Normal,
            || Ok(()),
        )
        .unwrap();
        assert!(
            prep2.owned_by_host.get("claude").unwrap().contains("srv"),
            "recovery grants landed"
        );
        assert!(
            ProjectionTransaction::load(&txn_path2).unwrap().is_none(),
            "recovery clears journal"
        );

        // --- crash after commit: state committed, journal not yet cleared; recovery idempotent ---
        let tmp3 = TempDir::new().unwrap();
        let (managed3, hp3) = setup(&tmp3);
        let r3 = apply_projection_transaction(
            &managed3,
            &hp3,
            &desired_by_host,
            &empty_live,
            &no_unaddressable(),
            &empty_committed,
            &BTreeMap::new(),
            "t",
            |p: &FaultPoint| {
                if *p == FaultPoint::AfterCommit {
                    Err(McpUpdateError::AmbiguousProjection("crash".into()))
                } else {
                    Ok(())
                }
            },
        );
        assert!(r3.is_err());
        assert!(
            ProjectedState::path_beside(&managed3).exists(),
            "state committed before crash"
        );
        let txn_path3 = ProjectionTransaction::path_beside(&managed3);
        assert!(
            ProjectionTransaction::load(&txn_path3).unwrap().is_some(),
            "journal not yet cleared"
        );
        let live3 = all_written_live();
        prepare_projection_recovery(
            &managed3,
            None,
            &live3,
            &no_unaddressable(),
            &no_unreadable(),
            PrepareMode::Normal,
            || Ok(()),
        )
        .unwrap();
        assert!(
            ProjectionTransaction::load(&txn_path3).unwrap().is_none(),
            "recovery clears leftover journal"
        );

        // --- crash before first host: journal persisted, no host writes; recovery not-landed, converges ---
        let tmp4 = TempDir::new().unwrap();
        let (managed4, hp4) = setup(&tmp4);
        let r4 = apply_projection_transaction(
            &managed4,
            &hp4,
            &desired_by_host,
            &empty_live,
            &no_unaddressable(),
            &empty_committed,
            &BTreeMap::new(),
            "t",
            |p: &FaultPoint| {
                if *p == FaultPoint::BeforeFirstHost {
                    Err(McpUpdateError::AmbiguousProjection("crash".into()))
                } else {
                    Ok(())
                }
            },
        );
        assert!(r4.is_err());
        let txn_path4 = ProjectionTransaction::path_beside(&managed4);
        assert!(ProjectionTransaction::load(&txn_path4).unwrap().is_some());
        assert!(
            !hp4[&Host::Claude].exists(),
            "no host written before first-host crash"
        );
        let (prep4, ()) = prepare_projection_recovery(
            &managed4,
            None,
            &empty_live,
            &no_unaddressable(),
            &no_unreadable(),
            PrepareMode::Normal,
            || Ok(()),
        )
        .unwrap();
        assert!(
            prep4
                .owned_by_host
                .get("claude")
                .is_none_or(|s| !s.contains("srv")),
            "nothing landed → no ownership"
        );
        assert!(
            ProjectionTransaction::load(&txn_path4).unwrap().is_none(),
            "recovery clears not-landed journal"
        );
    }
}
