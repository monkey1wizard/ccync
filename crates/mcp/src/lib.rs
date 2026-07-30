//! MCP (Model Context Protocol) config orchestration crate.
//!
//! Owns variable resolution, safe CCYNC-managed merge, per-host write, and the
//! `HealthCheck` impl for MCP config state.
//!
//! Dependency law: `mcp` → `base`. No CCYNC dep cycles. MCP host config
//! serializers live in the `serializers` submodule.

pub mod serializers;

mod entry;
mod health;
mod host_render;
mod io;
mod merge;
mod planner;
mod projection;
mod resolver;
mod state;
mod transaction;

pub use ccync_foundation::mcp::{McpError, McpManifest, McpServer, Result};
pub use health::*;
pub use projection::*;
pub use resolver::*;

use std::path::Path;

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
/// Order of operations: (1) recover any pending write-ahead transaction, or, if
/// there is no committed ownership yet, conservatively bootstrap ownership by
/// exact comparison of the prior manifest's rendered entries against the live
/// entries; (2) in `Normal` mode, commit the proven ownership and clear a
/// recovered transaction; (3) only then invoke `render` (which overwrites
/// `managed.json` / writes the new canonical state). A transaction third state
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
/// of a file ccync never read. Legacy bootstrap needs no such guard — it claims
/// only on a positive fingerprint match, and an unreadable host contributes no
/// fingerprints to match against.
pub(crate) fn prepare_projection_recovery<T>(
    managed_json: &Path,
    prior_manifest: Option<&McpManifest>,
    live_by_host: &std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<String, crate::entry::EntryFingerprint>,
    >,
    unaddressable_by_host: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    unreadable: &std::collections::BTreeSet<String>,
    mode: PrepareMode,
    render: impl FnOnce() -> std::result::Result<T, McpUpdateError>,
) -> std::result::Result<(ProjectionPrep, T), McpUpdateError> {
    use crate::host_render::{render_host_entries, Host};
    use crate::planner::classify_recovery;
    use crate::state::ProjectedState;
    use crate::transaction::ProjectionTransaction;

    let state_path = ProjectedState::path_beside(managed_json);
    let txn_path = ProjectionTransaction::path_beside(managed_json);

    let mut state = ProjectedState::load(&state_path)?;
    let mut actions: Vec<String> = Vec::new();

    if let Some(txn) = ProjectionTransaction::load(&txn_path)? {
        // An interrupted transaction can only be classified against live entries
        // ccync can actually read. If it touches an unreadable host, stop: no
        // ownership may be granted, and the journal must survive for a later run.
        let blocked: Vec<String> = txn
            .entries
            .iter()
            .filter(|e| unreadable.contains(&e.host))
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
            actions.push(format!(
                "recovered {} landed entr(y/ies) on {host}",
                names.len()
            ));
        }

        if let PrepareMode::Normal = mode {
            state.save(&state_path)?;
            ProjectionTransaction::remove(&txn_path)?;
        }
    } else if state.hosts.is_empty() {
        // No pending transaction and no committed ownership: legacy bootstrap.
        // Claim only names whose live entry exactly matches the prior render.
        if let Some(prior) = prior_manifest {
            for host in Host::ALL {
                let prior_entries = render_host_entries(host, prior)?;
                let live = live_by_host.get(host.as_str());
                let unaddressable = unaddressable_by_host.get(host.as_str());
                let mut claimed = std::collections::BTreeSet::new();
                for (name, prior_entry) in &prior_entries {
                    // An entry the writer cannot address is never claimed, even
                    // though it can fingerprint-match: claiming it would later
                    // authorize a removal the writer cannot actually perform.
                    if unaddressable.is_some_and(|u| u.contains(name)) {
                        continue;
                    }
                    if let Some(live_fp) = live.and_then(|m| m.get(name)) {
                        if &prior_entry.fingerprint()? == live_fp {
                            claimed.insert(name.clone());
                        }
                    }
                }
                if !claimed.is_empty() {
                    actions.push(format!(
                        "bootstrapped {} owned entr(y/ies) on {}",
                        claimed.len(),
                        host.as_str()
                    ));
                    state.set_owned(host, claimed);
                }
            }
            if let PrepareMode::Normal = mode {
                if !state.hosts.is_empty() {
                    state.save(&state_path)?;
                }
            }
        }
    }

    // Recovery/bootstrap is committed (Normal) — only now render.
    let rendered = render()?;

    let prep = ProjectionPrep {
        owned_by_host: state.hosts.clone(),
        actions,
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
    }
}

/// Report of an applied projection transaction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ApplyReport {
    pub hosts_written: Vec<String>,
    pub removed: usize,
    pub written: usize,
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

    // 1. Plan every *participating* host; fail closed on any collision before
    //    writing. A host absent from `host_paths` does not participate: its
    //    config path did not resolve on this platform, or its live config could
    //    not be read. It is not planned, not journaled, not written, and — the
    //    point — neither gains nor loses ownership. Committing ownership for a
    //    host ccync never wrote is ownership granted with zero proof: the
    //    recorded name would later authorize `run_mcp_remove_managed` to
    //    delete a same-named entry the user wrote themselves.
    let mut plans: Vec<(Host, &std::path::Path, HostPlan)> = Vec::new();
    let mut collisions: Vec<String> = Vec::new();
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
        let desired = desired_by_host.get(&host).cloned().unwrap_or_default();
        let plan = plan_host(&committed, &live, &unaddressable, &empty_prior, &desired)?;
        for name in &plan.collisions {
            collisions.push(format!("{}/{}", host.as_str(), name));
        }
        plans.push((host, path.as_path(), plan));
    }
    if !collisions.is_empty() {
        return Err(McpUpdateError::AmbiguousProjection(format!(
            "unowned MCP entries collide with managed servers: {}; live entries preserved, no deletion performed — resolve manually then re-run sync",
            collisions.join(", ")
        )));
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
    let mut report = ApplyReport::default();
    for (host, path, plan) in &plans {
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

    // 4. Commit per-host ownership only after every write proved out. Only
    //    participating hosts appear in `plans`, so a non-participating host's
    //    committed set is left exactly as it was.
    let mut state = ProjectedState::load(&state_path)?;
    for (host, _, plan) in &plans {
        state.set_owned(*host, plan.next_owned.clone());
    }
    state.save(&state_path)?;

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
    /// Hosts skipped because their live config could not be read or parsed:
    /// `(host key, reason)` pairs, the reason naming the path and why the
    /// read failed. Non-empty means the projection did not fully converge:
    /// the render and the other hosts succeeded, but the caller must still
    /// report failure.
    pub host_failures: Vec<(String, String)>,
}

/// The four live host config paths ccync projects onto (those that resolve).
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
        if !path.is_file() {
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
fn render_desired_by_host(
    manifest: &McpManifest,
) -> std::result::Result<
    std::collections::BTreeMap<
        crate::host_render::Host,
        std::collections::BTreeMap<String, crate::host_render::HostEntry>,
    >,
    McpUpdateError,
> {
    use crate::host_render::{render_host_entries, Host};
    let resolver = McpVariableResolver::new(build_variable_map());
    let resolved = resolver.resolve_manifest(manifest)?;
    let mut out = std::collections::BTreeMap::new();
    for host in Host::ALL {
        out.insert(host, render_host_entries(host, &resolved)?);
    }
    Ok(out)
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
    let all_host_paths = live_host_paths();
    let live = read_live_entries(&all_host_paths);

    // Hosts that failed to read do not participate: ccync must not write to a
    // config it cannot parse, and must not grant or revoke ownership on one.
    let unreadable: std::collections::BTreeSet<String> = live
        .failures
        .keys()
        .map(|h| h.as_str().to_string())
        .collect();
    let host_paths: std::collections::BTreeMap<crate::host_render::Host, std::path::PathBuf> =
        all_host_paths
            .iter()
            .filter(|(host, _)| !live.failures.contains_key(host))
            .map(|(host, path)| (*host, path.clone()))
            .collect();

    // Prior manifest (old managed.json), resolved in memory for exact-match
    // bootstrap — read before `render` overwrites it.
    let prior = if managed_json.is_file() {
        let m = load_manifest(managed_json)?;
        let resolved = McpVariableResolver::new(build_variable_map()).resolve_manifest(&m)?;
        Some(resolved)
    } else {
        None
    };

    let (prep, ()) = prepare_projection_recovery(
        managed_json,
        prior.as_ref(),
        &live.by_host,
        &live.unaddressable_by_host,
        &unreadable,
        PrepareMode::Normal,
        render,
    )?;

    // The render has now produced the new desired managed.json.
    let desired_manifest = load_manifest(managed_json)?;
    let desired_by_host = render_desired_by_host(&desired_manifest)?;

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
        &txn_id,
        |_| Ok(()),
    )?;

    Ok(McpProjectionReport {
        hosts_updated: report.hosts_written,
        recovery_actions: prep.actions,
        host_failures: live
            .failures
            .into_iter()
            .map(|(host, reason)| (host.as_str().to_string(), reason))
            .collect(),
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
    let all_host_paths = live_host_paths();
    let live = read_live_entries(&all_host_paths);

    let unreadable: std::collections::BTreeSet<String> = live
        .failures
        .keys()
        .map(|h| h.as_str().to_string())
        .collect();

    // The manifest on disk is still the *prior* one: no render has run.
    let prior = if managed_json.is_file() {
        let m = load_manifest(managed_json)?;
        let resolved = McpVariableResolver::new(build_variable_map()).resolve_manifest(&m)?;
        Some(resolved)
    } else {
        None
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

/// Result of a `run_mcp_remove_managed` run.
pub struct McpRemoveReport {
    pub hosts_updated: Vec<String>,
    pub warnings: Vec<String>,
}

/// Remove previously-managed MCP server entries from all live agent configs.
///
/// Reads `previous_manifest_path` (e.g. `~/.ccync/build/mcp/managed.json`) to
/// derive the managed server names, then removes those entries from the Claude,
/// Copilot, and OpenCode JSON configs and from the Codex TOML. A missing or
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
    )
}

fn run_mcp_remove_managed_inner(
    previous_manifest_path: &Path,
    claude_path: Option<std::path::PathBuf>,
    copilot_path: Option<std::path::PathBuf>,
    opencode_path: Option<std::path::PathBuf>,
    codex_path: Option<std::path::PathBuf>,
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

/// Inner helper so tests can inject a fake home without fighting `dirs` on Windows.
fn live_mcp_target_paths_inner(
    home: Option<std::path::PathBuf>,
    config_dir: Option<std::path::PathBuf>,
) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::with_capacity(4);
    if let Some(ref h) = home {
        paths.push(h.join(".claude.json"));
        paths.push(h.join(".codex").join("config.toml"));
        paths.push(h.join(".copilot").join("mcp-config.json"));
    }
    if let Some(ref c) = config_dir {
        paths.push(c.join("opencode").join("opencode.json"));
    }
    paths
}

/// Returns the live agent-config paths that the MCP projection writes to.
///
/// Order: claude `~/.claude.json`, codex `~/.codex/config.toml`,
/// copilot `~/.copilot/mcp-config.json`, opencode config dir `opencode.json`.
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

    // ── McpVariableResolver ──────────────────────────────────────────────────

    #[test]
    fn resolver_substitutes_known_var() {
        let mut vars = HashMap::new();
        vars.insert("HOME".to_string(), "/home/user".to_string());
        let r = McpVariableResolver::new(vars);
        assert_eq!(
            r.resolve_string("${HOME}/vault").unwrap(),
            "/home/user/vault"
        );
    }

    #[test]
    fn resolver_substitutes_multiple_vars() {
        let mut vars = HashMap::new();
        vars.insert("HOME".to_string(), "/home/user".to_string());
        vars.insert("PROJECT".to_string(), "myproject".to_string());
        let r = McpVariableResolver::new(vars);
        assert_eq!(
            r.resolve_string("${HOME}/projects/${PROJECT}").unwrap(),
            "/home/user/projects/myproject"
        );
    }

    #[test]
    fn resolver_rejects_unresolved_secret() {
        let r = McpVariableResolver::empty();
        assert!(matches!(
            r.resolve_string("${API_KEY}"),
            Err(McpError::UnresolvedSecret(_))
        ));
    }

    #[test]
    fn resolver_leaves_unresolved_non_secret_as_is() {
        let r = McpVariableResolver::empty();
        assert_eq!(r.resolve_string("${SOME_VAR}").unwrap(), "${SOME_VAR}");
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
    fn live_mcp_target_paths_returns_four_known_files() {
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
        assert_eq!(paths.len(), 4, "expected exactly 4 paths, got: {s:?}");
    }

    #[test]
    fn build_variable_map_resolves_secret_from_env() {
        // Defect-1 regression: an empty resolver hard-failed on secret-named
        // placeholders. The map must pick up a secret from the environment so
        // resolve_manifest succeeds instead of returning UnresolvedSecret.
        let key = "CCYNC_TEST_MCP_API_KEY";
        std::env::set_var(key, "sekret");
        let map = build_variable_map_from(None);
        let r = McpVariableResolver::new(map);
        assert_eq!(
            r.resolve_string("${CCYNC_TEST_MCP_API_KEY}").unwrap(),
            "sekret"
        );
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

        let report =
            run_mcp_remove_managed_inner(&managed_json, Some(claude.clone()), None, None, None)
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

        let result =
            run_mcp_remove_managed_inner(&managed_json, Some(claude.clone()), None, None, None);

        assert!(result.is_err(), "partial host failure is fatal");
        assert!(
            state_path.exists(),
            "committed ownership retained for retry after a failure"
        );
    }

    #[test]
    fn mcp_projection_does_not_treat_agy_as_an_mcp_provider() {
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
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "@mcp/server-memory"]
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
        assert_eq!(gate_paths.len(), 4, "gate list must have 4 entries");
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
        assert!(!report.hosts_updated.contains(&"agy".to_string()));

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

        // --- recovery: all-landed txn commits before render, then clears ---
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
                // Ownership must already be committed when render runs.
                let s = ProjectedState::load(&sp).unwrap();
                assert!(
                    s.owned(Host::Claude).contains("a"),
                    "committed before render"
                );
                Ok(())
            },
        )
        .unwrap();
        assert!(prep.owned_by_host.get("claude").unwrap().contains("a"));
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

        // --- bootstrap: no state, exact prior-render match claimed ---
        let tmp3 = TempDir::new().unwrap();
        let managed3 = tmp3.path().join("managed.json");
        let state3 = ProjectedState::path_beside(&managed3);
        let prior = McpManifest {
            servers: std::iter::once((
                "m".to_string(),
                McpServer {
                    server_type: Some("stdio".to_string()),
                    command: Some("node".to_string()),
                    args: Some(vec!["m.js".to_string()]),
                    env: None,
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
        assert!(prep3.owned_by_host.get("claude").unwrap().contains("m"));
        assert!(state3.exists(), "bootstrap state persisted");

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
            servers: std::iter::once((
                "srv-a".to_string(),
                McpServer {
                    server_type: Some("stdio".to_string()),
                    command: Some("node".to_string()),
                    args: Some(vec!["a.js".to_string()]),
                    env: None,
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
        run_mcp_remove_managed_inner(&managed, None, None, Some(opencode.clone()), None).unwrap();
        assert_eq!(
            fs::read_to_string(&opencode).unwrap(),
            user_owned,
            "the user's same-named entry survives: ccync never proved it owned srv-a on opencode"
        );
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
            servers: std::iter::once((
                "srv".to_string(),
                McpServer {
                    server_type: Some("stdio".to_string()),
                    command: Some("node".to_string()),
                    args: Some(vec!["s.js".to_string()]),
                    env: None,
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
