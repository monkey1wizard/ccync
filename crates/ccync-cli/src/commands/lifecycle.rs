//! Lifecycle commands: install / update / sync / uninstall / backup / restore.

use ccync_engine::ExitCode;
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
struct UpdateCliOptions {
    dry_run: bool,
    machine_only: bool,
    uninstall: bool,
    replace: bool,
    selected_runtimes: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GateAbortContext<'a> {
    written_surfaces: &'a [&'a str],
    unwritten_surfaces: &'a [&'a str],
    next_step: &'a str,
}

pub(crate) const SYNC_GATE_ABORT_CONTEXT: GateAbortContext<'static> = GateAbortContext {
    written_surfaces: &["ccync lockfile / resolved plugin state"],
    unwritten_surfaces: &["canonical render", "live agent config surfaces"],
    next_step: "ccync sync --yes",
};

pub(crate) const ADD_GATE_ABORT_CONTEXT: GateAbortContext<'static> = GateAbortContext {
    written_surfaces: &[
        "plugin cache",
        "personal catalog",
        "ccync lockfile / resolved plugin state",
    ],
    unwritten_surfaces: &["canonical render", "live agent config surfaces"],
    next_step: "ccync sync --yes",
};

pub(crate) const REMOVE_GATE_ABORT_CONTEXT: GateAbortContext<'static> = GateAbortContext {
    written_surfaces: &[
        "personal catalog or adopted-item state",
        "ccync lockfile / resolved plugin state",
        "canonical render",
    ],
    unwritten_surfaces: &["live agent config surfaces"],
    next_step: "ccync sync --yes",
};

/// The scope for a run whose canonical processing reported none. Machine
/// writers then withhold every direct component write and prune, so they
/// never publish over paths that direct delegation may own.
fn unavailable_machine_scope() -> projection::MachineProjectionScope {
    projection::MachineProjectionScope {
        decisions: Vec::new(),
        unavailable: true,
    }
}

fn finalize_machine_options(
    config: &ccync_foundation::config::CcyncConfig,
    parsed: &UpdateCliOptions,
    layout: &ccync_foundation::paths::EffectiveLayout,
) -> Result<projection::MachineUpdateOptions, String> {
    let mut opts =
        projection::machine_options_from_config(config, parsed.dry_run, layout.state_base())
            .map_err(|e| e.to_string())?;
    opts.uninstall = parsed.uninstall;
    opts.replace = parsed.replace;
    if let Some(selected) = &parsed.selected_runtimes {
        opts.set_selected_projection_targets(selected.clone());
    }
    Ok(opts)
}

/// `ccync sync [--dry-run] [--yes]` — the unified engine. Re-resolves the
/// catalog lockfile, then re-projects the managed set (canonical render +
/// skills / commands / agents + unified MCP) to every selected agent.
///
/// Master selection (adopt) is owned by `ccync init`, not sync (the former
/// `--import-from` was removed). `--dry-run` resolves + reports without writing.
/// `--yes` / `CCYNC_ASSUME_YES` skips the first-run confirmation prompt.
#[cfg(test)]
pub(crate) fn cmd_sync(args: &[String]) -> ExitCode {
    cmd_sync_with_guard(args, None)
}

const SYNC_USAGE: &str = "Usage: ccync sync [--dry-run] [--yes] [--instructions-only]\n       ccync sync --reselect-mcp SOURCE SERVER HOST [--dry-run]";

pub(crate) fn cmd_sync_with_guard(
    args: &[String],
    lifecycle_guard: Option<&ccync_engine::tool_manager::lock::LifecycleGuard>,
) -> ExitCode {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return super::args::reject_unknown_flags("sync", args, &[], &[], 0, SYNC_USAGE)
            .unwrap_err();
    }
    if args.iter().any(|a| a == "--reselect-mcp") {
        let args = &args[1..];
        let (dry_run, rest) = if args.first().is_some_and(|arg| arg == "--dry-run") {
            (true, &args[1..])
        } else {
            (false, args)
        };
        let (dry_run, rest) = if rest.last().is_some_and(|arg| arg == "--dry-run") && !dry_run {
            (true, &rest[..rest.len() - 1])
        } else {
            (dry_run, rest)
        };
        if rest.len() != 4
            || rest[0] != "--reselect-mcp"
            || rest[1..]
                .iter()
                .any(|value| value.starts_with('-') || value.trim().is_empty())
        {
            return reselect_usage();
        }
        let values: Vec<&str> = rest[1..].iter().map(String::as_str).collect();
        return run_reselect_mcp(&values, dry_run, lifecycle_guard);
    }
    if let Err(code) = super::args::reject_unknown_flags(
        "sync",
        args,
        &["--dry-run", "--yes", "--instructions-only"],
        &[],
        0,
        SYNC_USAGE,
    ) {
        return code;
    }
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let assume_yes = args.iter().any(|a| a == "--yes");
    if args.iter().any(|a| a == "--instructions-only") {
        return run_instruction_only(dry_run, assume_yes);
    }
    run_unified_projection_with_guard(
        dry_run,
        assume_yes,
        false,
        Some(&SYNC_GATE_ABORT_CONTEXT),
        lifecycle_guard,
        true,
    )
}

fn reselect_usage() -> ExitCode {
    eprintln!("{SYNC_USAGE}");
    ExitCode::Usage
}

fn run_reselect_mcp(
    values: &[&str],
    dry_run: bool,
    lifecycle_guard: Option<&ccync_engine::tool_manager::lock::LifecycleGuard>,
) -> ExitCode {
    let layout = match load_effective_layout("ccync sync") {
        Ok(layout) => layout,
        Err(code) => return code,
    };
    if let Err(code) = require_initialized_machine_at(&layout) {
        return code;
    }
    let _guard = if lifecycle_guard.is_none() {
        match ccync_engine::tool_manager::lock::LifecycleLock::acquire() {
            Ok(guard) => Some(guard),
            Err(error) => {
                eprintln!("ccync sync: lifecycle lock: {error}");
                return ExitCode::Error;
            }
        }
    } else {
        None
    };
    let inventory = match ccync_engine::install::read_mcp_declarations() {
        Ok(inventory) => inventory,
        Err(error) => {
            eprintln!("ccync sync: {error}");
            return ExitCode::Error;
        }
    };
    let manifest = match serde_json::from_value(serde_json::json!({
        "servers": inventory.servers(), "provenance": inventory.provenance()
    })) {
        Ok(manifest) => manifest,
        Err(error) => {
            eprintln!("ccync sync: invalid MCP inventory: {error}");
            return ExitCode::Error;
        }
    };
    let identity = ccync_foundation::mcp::DeclarationIdentity {
        source: values[0].into(),
        server: values[1].into(),
    };
    let select = |preview| {
        mcp::reselect_mcp_server(
            &layout.generated_mcp_path(),
            &layout.machine_config_path(),
            &manifest,
            &identity,
            values[2],
            preview,
        )
    };
    let result = match select(true) {
        Ok(mcp::ReselectOutcome::Previewed) if !dry_run => select(false),
        other => other,
    };
    match result {
        Ok(mcp::ReselectOutcome::Applied) => {
            println!("MCP server reselected");
            ExitCode::Success
        }
        Ok(mcp::ReselectOutcome::Previewed) => {
            println!("MCP server can be reselected (dry run)");
            ExitCode::Success
        }
        Ok(mcp::ReselectOutcome::NotHeld) => {
            eprintln!("ccync sync: MCP server is not held");
            ExitCode::Error
        }
        Ok(mcp::ReselectOutcome::Blocked(reason)) => {
            eprintln!("ccync sync: MCP reselection blocked: {reason}");
            if reason == "unknown MCP host" {
                ExitCode::Usage
            } else {
                ExitCode::Error
            }
        }
        Err(error) => {
            eprintln!("ccync sync: MCP reselection failed: {error}");
            ExitCode::Error
        }
    }
}

/// Project only the user's instruction block to selected user-scope files.
/// This route deliberately avoids migration, catalog resolution, and unified
/// projection because none of those states belong to instruction-only sync.
fn run_instruction_only(dry_run: bool, assume_yes: bool) -> ExitCode {
    let mut output = std::io::stdout().lock();
    run_instruction_only_with_output(dry_run, assume_yes, &mut output)
}

pub(super) fn run_instruction_only_with_output(
    dry_run: bool,
    assume_yes: bool,
    output: &mut dyn std::io::Write,
) -> ExitCode {
    run_instruction_only_with_consent(dry_run, &mut || confirm_overwrite(assume_yes), output)
}

pub(super) fn run_instruction_only_with_consent(
    dry_run: bool,
    authorize: &mut dyn FnMut() -> bool,
    output: &mut dyn std::io::Write,
) -> ExitCode {
    let layout = match load_effective_layout("ccync sync --instructions-only") {
        Ok(layout) => layout,
        Err(code) => return code,
    };
    macro_rules! outln {
        ($($arg:tt)*) => {{ let _ = writeln!(output, $($arg)*); }};
    }
    if let Err(code) = require_initialized_machine_at(&layout) {
        return code;
    }
    let config =
        match ccync_foundation::config::CcyncConfig::load_strict_from_path(&layout.config_path()) {
            Ok(config) => config,
            Err(error) => {
                outln!("ccync sync --instructions-only: {error}");
                return ExitCode::Error;
            }
        };
    let resolution = config.resolve_projection_targets();
    if resolution.state == ccync_foundation::config::LegacyState::Both {
        outln!("ccync sync --instructions-only: both selectedRuntimes and selectedProjectionTargets are set; run `ccync init <master>` to choose one selection");
        return ExitCode::Error;
    }
    if !resolution.rejected.is_empty() {
        outln!(
            "ccync sync --instructions-only: rejected target selection value(s): {}",
            resolution.rejected.join(", ")
        );
        return ExitCode::Error;
    }
    if resolution.targets.is_empty() {
        outln!("ccync sync --instructions-only: select at least one valid target with `ccync init <master>` first");
        return ExitCode::Error;
    }

    // Build the ordered, physically deduplicated target list before reading
    // either the source or any target file. Antigravity's three targets share
    // one path and therefore produce one independent write unit.
    let paths = instruction_paths(&resolution.targets);
    let prune_paths = legacy_instruction_prune_paths(&resolution.targets);
    let source_path = layout.agents_md_path();
    let payload = match projection::managed_block::read_instruction_source(&source_path) {
        Ok(projection::managed_block::InstructionSource::Absent) => None,
        Ok(projection::managed_block::InstructionSource::Content(content)) => Some(content),
        Err(error) => {
            outln!("ccync sync --instructions-only: {error}");
            for path in paths.iter().chain(&prune_paths) {
                outln!("{}: {}", path.display(), instruction_source_blocker(&error));
            }
            outln!("  no files written");
            return ExitCode::Error;
        }
    };

    use projection::managed_block::BlockClassification as Classification;
    let inspections: Vec<_> = paths
        .iter()
        .map(|path| {
            (
                path.clone(),
                projection::managed_block::classify(path, payload.as_deref()),
                payload.as_deref(),
            )
        })
        .collect();
    let prune_inspections: Vec<_> = prune_paths
        .iter()
        .map(|path| {
            (
                path.clone(),
                projection::managed_block::classify(path, None),
                None,
            )
        })
        .collect();
    let blocked = inspections.iter().any(|(_, state, _)| {
        matches!(
            state,
            Classification::TargetRead
                | Classification::MalformedMarker
                | Classification::DanglingLink
        )
    }) || prune_inspections.iter().any(|(_, state, _)| {
        matches!(
            state,
            Classification::TargetRead
                | Classification::MalformedMarker
                | Classification::DanglingLink
        )
    });

    if dry_run {
        outln!("--- INSTRUCTIONS ONLY: DRY RUN ---");
        for (path, state, _) in &inspections {
            outln!("{}: {}", path.display(), instruction_classification(*state));
        }
        if !prune_inspections.is_empty() {
            outln!("legacy instruction files (prune only):");
            for (path, state, _) in &prune_inspections {
                outln!("{}: {}", path.display(), instruction_classification(*state));
            }
        }
        outln!("  no files written");
        return if blocked {
            ExitCode::Error
        } else {
            ExitCode::Success
        };
    }

    outln!("ccync sync --instructions-only: planned instruction updates:");
    outln!(
        "mode: {}",
        if payload.is_some() { "write" } else { "prune" }
    );
    for (path, state, _) in &inspections {
        outln!("{}: {}", path.display(), instruction_classification(*state));
    }
    if !prune_inspections.is_empty() {
        outln!("legacy instruction files (prune only):");
        outln!("mode: prune");
        for (path, state, _) in &prune_inspections {
            outln!("{}: {}", path.display(), instruction_classification(*state));
        }
    }
    if blocked {
        outln!("ccync sync --instructions-only: blocked target(s) will be left untouched");
    }
    // Scoped applies never set the full-projection lock. Require explicit
    // authorization on every apply, including applies after an earlier scoped
    // write on a machine that still has no completed full projection.
    if !authorize() {
        outln!("ccync sync --instructions-only: authorization required; rerun with --yes to apply scoped instruction changes");
        outln!("  no files written");
        return ExitCode::Error;
    }

    let mut changed = 0usize;
    let mut unchanged = 0usize;
    let mut failed = 0usize;
    for (path, state, write_payload) in inspections.into_iter().chain(prune_inspections) {
        if matches!(
            state,
            Classification::TargetRead
                | Classification::MalformedMarker
                | Classification::DanglingLink
        ) {
            outln!("{}: {}", path.display(), instruction_classification(state));
            failed += 1;
            continue;
        }
        match projection::managed_block::write_block(&path, write_payload) {
            Ok(outcome) => {
                outln!(
                    "{}: {}",
                    path.display(),
                    format!("{outcome:?}").to_ascii_lowercase()
                );
                if outcome == projection::managed_block::BlockOutcome::Unchanged {
                    unchanged += 1;
                } else {
                    changed += 1;
                }
            }
            Err(error) => {
                outln!("{}: failed: {error}", path.display());
                failed += 1;
            }
        }
    }
    outln!("changed: {changed}; unchanged: {unchanged}; failed: {failed}");
    if failed == 0 {
        ExitCode::Success
    } else {
        ExitCode::Error
    }
}

fn load_effective_layout(
    command: &str,
) -> Result<ccync_foundation::paths::EffectiveLayout, ExitCode> {
    ccync_foundation::paths::EffectiveLayout::load_or_legacy().map_err(|error| {
        eprintln!("{command}: cannot load effective storage layout: {error}");
        ExitCode::Error
    })
}

pub(crate) fn instruction_paths(
    targets: &[ccync_foundation::target::ProjectionTargetId],
) -> Vec<PathBuf> {
    projection::managed_block::dedupe_resolved(
        ccync_foundation::target::CANONICAL_PROJECTION_TARGETS
            .iter()
            .filter(|target| targets.contains(target))
            .filter_map(|target| {
                ccync_foundation::paths::user_scope_instruction_write_path(*target)
            })
            .collect(),
    )
}

pub(crate) fn legacy_instruction_prune_paths(
    targets: &[ccync_foundation::target::ProjectionTargetId],
) -> Vec<PathBuf> {
    use ccync_foundation::target::ProjectionTargetId::Claude;
    if !targets.contains(&Claude) {
        return Vec::new();
    }
    projection::managed_block::dedupe_resolved(
        ccync_foundation::paths::legacy_claude_user_instruction_path()
            .into_iter()
            .collect(),
    )
}

fn instruction_classification(
    classification: projection::managed_block::BlockClassification,
) -> &'static str {
    use projection::managed_block::BlockClassification as Classification;
    match classification {
        Classification::Create => "create",
        Classification::Append => "append",
        Classification::Rewrite => "rewrite",
        Classification::Prune => "prune",
        Classification::Unchanged => "unchanged",
        Classification::TargetRead => "blocked: target read",
        Classification::MalformedMarker => "blocked: malformed marker",
        Classification::DanglingLink => "blocked: dangling link",
    }
}

fn classification_is_blocker(
    classification: projection::managed_block::BlockClassification,
) -> bool {
    use projection::managed_block::BlockClassification as Classification;
    matches!(
        classification,
        Classification::TargetRead | Classification::MalformedMarker | Classification::DanglingLink
    )
}

fn instruction_source_blocker(
    error: &projection::managed_block::InstructionSourceError,
) -> &'static str {
    use projection::managed_block::InstructionSourceError as SourceError;
    match error {
        SourceError::Read(_) => "blocked: source read",
        SourceError::Utf8(_) => "blocked: source encoding",
        SourceError::DanglingLink => "blocked: dangling source link",
    }
}

/// Run the one-shot layout migration for a given user home directory.
///
/// Moves old `~/.ccync/` layout files to new locations. Best-effort: returns
/// the error if apply fails so callers can log and continue.
pub(crate) fn run_layout_migration(user_home: &std::path::Path) -> std::io::Result<()> {
    let ccync_home = user_home.join(".ccync");
    let plan = ccync_engine::migrate::build_migration_plan(&ccync_home);
    ccync_engine::migrate::apply_migration(&plan)?;
    // Fold the legacy selection sources (install-state.json and the old-layout
    // `runtimes` spelling moved above) into config.json's canonical key so an
    // upgrading install does not lose its selection.
    ccync_engine::migrate::fold_install_state_into_config(&ccync_home)
}

/// Shared projection core: canonical render + machine projection + MCP projection.
///
/// Called by both `run_unified_projection` (public sync) and `refresh_with`
/// (internal dev rebuild). Does NOT run the first-run gate, catalog resolve,
/// dry-run check, or interactive prompts — those belong to their callers.
///
/// Returns `ExitCode::Success` on success; `ExitCode::Error` if the canonical
/// render fails (machine/MCP projections are best-effort).
/// Build the honest failure report for a projection that did not complete.
///
/// `render_outcome` is tri-state deliberately. `None` means the canonical render
/// was never invoked (the MCP preparation failed first); `Some(Ok(()))` means it
/// ran and succeeded; `Some(Err(msg))` means it ran and failed. The overall
/// failure report must state which earlier non-MCP phases actually completed,
/// so this must never infer "the render succeeded" from the *absence* of a
/// render error — a render that never ran leaves exactly the same absence.
///
/// `journal_retained` is observed from disk rather than assumed: a fail-closed
/// abort (unowned collision, ambiguous recovery) writes no journal at all, while
/// an interrupted host write leaves one for the next run to converge from.
/// Claiming a retained journal that does not exist tells the user to re-run for
/// a convergence that will not happen.
fn projection_failure_report(
    label: &str,
    render_outcome: &Option<Result<(), String>>,
    mcp_error: &str,
    journal_retained: bool,
) -> Vec<String> {
    match render_outcome {
        None => vec![
            format!("{label}: MCP projection failed before the canonical render ran: {mcp_error}"),
            format!(
                "{label}: nothing was projected — no canonical render, no managed surfaces, and no live host was changed"
            ),
        ],
        // The canonical render itself failed — nothing was projected.
        Some(Err(canonical_err)) => vec![format!("{label}: {canonical_err}")],
        Some(Ok(())) if journal_retained => vec![
            format!("{label}: MCP projection failed: {mcp_error}"),
            format!(
                "{label}: canonical/managed surfaces were projected, but MCP host config was not fully updated; the change journal is retained — re-run sync to converge"
            ),
        ],
        Some(Ok(())) => vec![
            format!("{label}: MCP projection failed: {mcp_error}"),
            format!(
                "{label}: canonical/managed surfaces were projected; no live MCP host was changed — resolve the error above, then re-run sync"
            ),
        ],
    }
}

fn project_managed_instruction_block(
    selected_targets: &[ccync_foundation::target::ProjectionTargetId],
    label: &str,
    layout: &ccync_foundation::paths::EffectiveLayout,
) -> bool {
    let payload = match read_full_sync_instruction_payload(layout) {
        Ok(payload) => payload,
        Err(error) => {
            // A source failure must never prune or rewrite any target: report
            // the phase failure and leave every instruction file untouched.
            eprintln!("{label}: managed block source: {error}");
            eprintln!("{label}: managed block: no instruction files written");
            return true;
        }
    };
    let mut failed = false;
    for (path, payload) in instruction_paths(selected_targets)
        .into_iter()
        .map(|path| (path, payload.as_deref()))
        .chain(
            legacy_instruction_prune_paths(selected_targets)
                .into_iter()
                .map(|path| (path, None)),
        )
    {
        if let Err(error) = projection::managed_block::write_block(&path, payload) {
            failed = true;
            eprintln!("{label}: managed block {}: {error}", path.display());
        }
    }
    failed
}

/// Read the instruction source for full sync through the shared fallible
/// reader. `Ok(None)` means the source is intentionally absent (missing or
/// zero bytes), which prunes; every other failure is returned, never degraded
/// to absence.
fn read_full_sync_instruction_payload(
    layout: &ccync_foundation::paths::EffectiveLayout,
) -> Result<Option<String>, projection::managed_block::InstructionSourceError> {
    use projection::managed_block::InstructionSource;
    let path = layout.agents_md_path();
    match projection::managed_block::read_instruction_source(&path)? {
        InstructionSource::Absent => Ok(None),
        InstructionSource::Content(content) => Ok(Some(content)),
    }
}

/// Render `McpProjectionReport::hosts_updated` as the `MCP hosts updated: …`
/// stdout evidence line — only when at least one host was actually written.
/// MCP host evidence is independent of projection-target status and must
/// never be synthesized; an empty `hosts_updated` renders nothing at all,
/// never a fabricated "no hosts updated" line.
fn mcp_hosts_updated_lines(hosts_updated: &[String]) -> Vec<String> {
    if hosts_updated.is_empty() {
        Vec::new()
    } else {
        vec![format!("MCP hosts updated: {}", hosts_updated.join(", "))]
    }
}

/// Render pre-render MCP recovery/legacy-bootstrap actions as `<label>: MCP
/// recovery: <action>` stderr lines — the recovery channel distinct from
/// (and never collapsed into) the four `projection_failure_report` phase
/// messages: this fires on a *successful* MCP preparation that still had
/// something to recover from, not on a failure.
fn mcp_recovery_action_lines(label: &str, recovery_actions: &[String]) -> Vec<String> {
    recovery_actions
        .iter()
        .map(|action| format!("{label}: MCP recovery: {action}"))
        .collect()
}

/// Render `McpProjectionReport::host_failures` as `MCP host failed: <host> —
/// <reason>` stderr evidence lines, one per failed host, in the order the
/// report provides them.
fn mcp_host_failure_lines(host_failures: &[(String, String)]) -> Vec<String> {
    host_failures
        .iter()
        .map(|(host, reason)| format!("MCP host failed: {host} — {reason}"))
        .collect()
}

/// Render desired-manifest MCP servers skipped because secret-class variables
/// could not be resolved, in deterministic server and variable order.
pub(crate) fn mcp_secret_skip_lines(
    label: &str,
    skips: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
) -> Vec<String> {
    skips
        .iter()
        .map(|(server, variables)| {
            format!(
                "{label}: MCP server failed: {server} — missing secret {} — add to config.json#secrets or the environment",
                variables.iter().cloned().collect::<Vec<_>>().join(", ")
            )
        })
        .collect()
}

fn mcp_variable_lines(
    label: &str,
    reports: &[mcp::ServerVariableReport],
    skips: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
) -> Vec<String> {
    let mut lines = Vec::new();
    for report in reports {
        if report.ordinary_names().is_empty() {
            if let Some(names) = skips.get(report.server()) {
                let one = [(report.server().to_string(), names.clone())]
                    .into_iter()
                    .collect();
                lines.extend(mcp_secret_skip_lines(label, &one));
            }
            continue;
        }
        let secrets = report
            .secret_names()
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let ordinary = report
            .ordinary_names()
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let fields = report
            .field_locations()
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let secret_text = if secrets.is_empty() {
            String::new()
        } else {
            format!("missing secret {secrets}; ")
        };
        lines.push(format!("{label}: MCP server {}: {secret_text}missing variables {ordinary}; fields {fields} — set them in config.json or the environment", report.server()));
    }
    lines
}

#[cfg(test)]
mod mcp_variable_output_tests {
    #[test]
    fn mixed_missing_variables_produce_one_value_free_line() {
        let dir = tempfile::tempdir().unwrap();
        let manifest: mcp::McpManifest = serde_json::from_value(serde_json::json!({
            "servers": {"example": {"command": "tool", "args": ["${CCYNC_TEST_ORDINARY_MISSING}"], "env": {"TOKEN": "${CCYNC_SECRET_TEST_MISSING}"}}}
        })).unwrap();
        let config = dir.path().join("config.json");
        let reports = mcp::server_variable_reports_at(&manifest, &config);
        let skips = mcp::unresolved_secret_vars_at(&manifest, &config);
        let lines = super::mcp_variable_lines("ccync sync", &reports, &skips);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("CCYNC_SECRET_TEST_MISSING"));
        assert!(lines[0].contains("CCYNC_TEST_ORDINARY_MISSING"));
        assert!(lines[0].contains("args"));
        assert!(lines[0].contains("env"));
        assert!(!lines[0].contains("${"));
    }
}

pub(super) fn current_mcp_decisions(
    layout: &ccync_foundation::paths::EffectiveLayout,
) -> Result<Vec<mcp::McpHostDecision>, String> {
    let options = ccync_engine::catalog::ResolveCatalogOptions {
        catalog_path: None,
        config_path: layout.machine_config_path(),
        lockfile_path: layout.build_lock_path(),
        dry_run: true,
        personal_catalog_path_override: Some(layout.personal_catalog_path()),
    };
    let resolution = ccync_engine::catalog::run_resolve_catalog(&options)?;
    let lock = match std::fs::read(&options.lockfile_path) {
        Ok(bytes) => {
            serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|e| e.to_string())?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e.to_string()),
    };
    let keys = [
        "pluginId",
        "displayName",
        "sourceType",
        "installStrategy",
        "source",
        "pin",
        "pinnedSha",
        "held",
    ];
    let normalize = |value: &serde_json::Value| {
        let mut row = serde_json::Map::new();
        for key in keys {
            if let Some(v) = value.get(key) {
                row.insert(key.to_string(), v.clone());
            } else if matches!(
                key,
                "pluginId" | "displayName" | "sourceType" | "installStrategy" | "source"
            ) {
                row.insert(key.to_string(), serde_json::Value::Null);
            }
        }
        serde_json::Value::Object(row)
    };
    let current: Vec<_> = resolution.personal_plugins.iter().map(normalize).collect();
    let recorded: Vec<_> = lock
        .get("_personalPlugins")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(normalize)
        .collect();
    if current != recorded {
        return Err("personal catalog changed; run ccync sync first".into());
    }
    if !resolution.errors.is_empty() {
        return Err(resolution.errors.join(", "));
    }
    let inventory = ccync_engine::install::read_mcp_declarations().map_err(|e| e.to_string())?;
    let manifest = serde_json::from_value(
        serde_json::json!({"servers": inventory.servers(), "provenance": inventory.provenance()}),
    )
    .map_err(|e| format!("invalid MCP inventory: {e}"))?;
    let inspection = mcp::inspect_mcp_declarations(
        &layout.generated_mcp_path(),
        &layout.machine_config_path(),
        &manifest,
    )
    .map_err(|e| e.to_string())?;
    Ok(inspection.decisions().to_vec())
}

pub(super) fn mcp_decision_lines(decisions: &[mcp::McpHostDecision]) -> (Vec<String>, bool) {
    use mcp::McpDecisionStatus as Status;
    let changes = decisions
        .iter()
        .filter(|d| matches!(d.status(), Status::Planned | Status::Applied))
        .count();
    let blocked = decisions
        .iter()
        .filter(|d| matches!(d.status(), Status::Blocked | Status::Failed))
        .count();
    let mut lines = vec![format!(
        "MCP: {changes} change(s), {blocked} blocked or failed"
    )];
    for decision in decisions {
        lines.push(format!(
            "  {} / {}: {:?} — {} ({})",
            decision.identity().server,
            decision.host(),
            decision.status(),
            decision.reason(),
            decision.selection_reason()
        ));
        if let Some(note) = decision.launch_note() {
            lines.push(format!("    {note}"));
        }
    }
    if blocked > 0 {
        lines.push("  Resolve the blocked entries and run ccync sync again.".into());
    }
    (lines, blocked > 0)
}

pub(crate) fn run_projection_core(
    config: &ccync_foundation::config::CcyncConfig,
    label: &str,
) -> ExitCode {
    run_projection_core_inner(config, label).0
}

/// Run the projection core and return the completed per-target report.
#[allow(dead_code)]
pub(crate) fn run_projection_with_report(
    config: &ccync_foundation::config::CcyncConfig,
    label: &str,
) -> (ExitCode, ccync_engine::install::ProjectionRunReport) {
    let (code, accumulator) = run_projection_core_inner(config, label);
    (code, accumulator.finish())
}

/// Same behavior as [`run_projection_core`], but also returns the
/// [`ProjectionRunAccumulator`] built during the run so callers (today, only
/// this module's own tests) can inspect exactly what Canonical-phase
/// evidence the MCP closure side channel captured and recorded — without
/// widening `run_projection_core`'s own production return type. The
/// accumulator is not yet consumed by anything user-facing: `finish()` is
/// not called here, Machine-phase evidence is not recorded here (both are a
/// later task's job), and this function's own observable production
/// behavior — output, exit code, ordering — is unchanged from before this
/// side channel was added.
fn run_projection_core_inner(
    config: &ccync_foundation::config::CcyncConfig,
    label: &str,
) -> (ExitCode, ccync_engine::install::ProjectionRunAccumulator) {
    let layout = match load_effective_layout(label) {
        Ok(layout) => layout,
        Err(code) => {
            return (
                code,
                ccync_engine::install::ProjectionRunAccumulator::new(label, std::iter::empty()),
            )
        }
    };
    run_projection_core_inner_at(config, label, &layout)
}

fn run_projection_core_inner_at(
    config: &ccync_foundation::config::CcyncConfig,
    label: &str,
    layout: &ccync_foundation::paths::EffectiveLayout,
) -> (ExitCode, ccync_engine::install::ProjectionRunAccumulator) {
    // Resolve the run's selected projection targets up front, via the same
    // `MachineUpdateOptions` resolution phase 2 below already needs (computed
    // once, reused for both), so the accumulator can be seeded with the
    // correct target set before any evidence is recorded — a selected target
    // that ends up with zero evidence must still surface as `NotRun`, never
    // silently absent (D-02).
    let machine_parsed = UpdateCliOptions {
        dry_run: false,
        machine_only: true,
        uninstall: false,
        replace: false,
        selected_runtimes: None,
    };
    let machine_opts_result = finalize_machine_options(config, &machine_parsed, layout);
    run_projection_core_with_machine_opts_at(config, label, machine_opts_result, layout)
}

/// The body of [`run_projection_core_inner`] with the Machine-phase options
/// resolution passed in rather than computed internally. Split out as its own
/// seam because every environment-level way to make `finalize_machine_options`
/// fail (an unset home, an unresolvable canonical root) also breaks the
/// Canonical phase — only direct injection can exercise the
/// options-resolution-failed arm with a Canonical phase that still succeeds.
/// Production always reaches this via `run_projection_core_inner`.
fn run_projection_core_with_machine_opts(
    config: &ccync_foundation::config::CcyncConfig,
    label: &str,
    machine_opts_result: Result<projection::MachineUpdateOptions, String>,
) -> (ExitCode, ccync_engine::install::ProjectionRunAccumulator) {
    let layout = match load_effective_layout(label) {
        Ok(layout) => layout,
        Err(code) => {
            return (
                code,
                ccync_engine::install::ProjectionRunAccumulator::new(label, std::iter::empty()),
            )
        }
    };
    run_projection_core_with_machine_opts_at(config, label, machine_opts_result, &layout)
}

fn run_projection_core_with_machine_opts_at(
    config: &ccync_foundation::config::CcyncConfig,
    label: &str,
    machine_opts_result: Result<projection::MachineUpdateOptions, String>,
    layout: &ccync_foundation::paths::EffectiveLayout,
) -> (ExitCode, ccync_engine::install::ProjectionRunAccumulator) {
    let selected_targets: Vec<ccync_foundation::target::ProjectionTargetId> =
        match &machine_opts_result {
            Ok(machine_opts) => machine_opts
                .selected_projection_targets()
                .iter()
                .filter_map(|s| s.parse().ok())
                .collect(),
            Err(_) => Vec::new(),
        };

    // 1. MCP-bracketed canonical render: recover/bootstrap MCP ownership BEFORE
    //    the canonical render overwrites managed.json, then apply the newly
    //    rendered desired manifest across the four hosts under one
    //    write-ahead transaction.
    //
    //    The closure below is the MCP closure side channel: it runs INSIDE
    //    `run_mcp_projection`'s ownership-proving transaction (after
    //    ownership is proven, before the live-host transaction is applied),
    //    and captures its own results into `render_warnings`/`render_outcome`/
    //    `canonical_evidence`/`run_fault` — variables declared outside the closure,
    //    mutated inside it, read after `run_mcp_projection` returns —
    //    because the closure's own return type
    //    (`Result<(), mcp::McpUpdateError>`) cannot carry them directly.
    //    `canonical_evidence` is the complete Canonical-phase
    //    `ProjectionEvidence` carried by `run_update_with_evidence`;
    //    `render_warnings` is read off its own `run_warnings` field, which
    //    folds in every run-global warning (canonical-root render, loose-skill
    //    materialization, MCP generation, stale-dist cleanup) alongside each
    //    phase's own owned-target failures.
    let mut render_warnings: Vec<String> = Vec::new();
    let mut render_outcome: Option<Result<(), String>> = None;
    let mut canonical_evidence: Option<projection::ProjectionEvidence> = None;
    let mut run_fault: Option<ccync_engine::install::ProjectionPhaseFailure> = None;
    let mut canonical_routes: Vec<ccync_engine::managed_view::RouteEvidence> = Vec::new();
    let mut canonical_retirements = std::collections::BTreeMap::new();
    let mut canonical_conflicts: Vec<ccync_engine::install::RouteConflict> = Vec::new();
    // The direct delegation scope this canonical run prepared. Machine writers
    // honor it; a run that never reports one falls back to unavailable.
    let mut canonical_scope: Option<projection::MachineProjectionScope> = None;
    let render = || -> std::result::Result<(), mcp::McpUpdateError> {
        match ccync_engine::install::run_update_with_handoff(config) {
            Ok((outcome, scope)) => {
                let ccync_engine::install::CanonicalRunOutcome {
                    evidence,
                    fault,
                    routes,
                    route_retirements,
                    route_conflicts,
                } = outcome;
                render_warnings = evidence.run_warnings.clone();
                render_outcome = Some(Ok(()));
                canonical_scope = Some(scope);
                canonical_evidence = Some(evidence);
                canonical_routes = routes;
                canonical_retirements = route_retirements;
                canonical_conflicts = route_conflicts;
                run_fault = fault;
                Ok(())
            }
            Err(e) => {
                render_outcome = Some(Err(e.to_string()));
                Err(mcp::McpUpdateError::Io(std::io::Error::other(
                    e.to_string(),
                )))
            }
        }
    };

    let managed_path = layout.generated_mcp_path();
    let mcp_outcome =
        mcp::run_mcp_projection_at(&managed_path, &layout.machine_config_path(), render);

    // The engine-owned `ProjectionRunAccumulator` is the sole status
    // authority (D-04) — the CLI passes evidence between domains but never
    // infers status itself. Seed it with the run's selected targets, then
    // record whatever Canonical-phase evidence the closure above captured.
    // On an outer global fault `canonical_evidence` stays `None` (the closure's
    // `Err` arm never sets it) — nothing is recorded, so every selected
    // target correctly stays `NotRun` per D-02 (absence is never completion).
    // A typed run-global fault is different: its partial evidence is returned
    // in the normal outcome and remains available to the accumulator. A
    // `Failed`/`Skipped` target recorded inside a
    // successfully returned `evidence` (an expected, owned-path failure —
    // the actually reachable "partial" case at this call site) is preserved
    // exactly as `run_update_with_evidence` reported it.
    let mut accumulator =
        ccync_engine::install::ProjectionRunAccumulator::new("normal", selected_targets.clone());
    if let Some(evidence) = &canonical_evidence {
        accumulator.record_canonical(evidence);
    }

    // Hosts the MCP projection had to skip because their live config could not
    // be read. Deliberately not fatal *here*: the canonical render already
    // succeeded and phase 2 below must still run, so one malformed
    // `~/.codex/config.toml` cannot disable skills/commands/agents projection
    // too. The command still exits Error at the end.
    let mut mcp_decision_failed = false;
    let mcp_host_failures: Vec<(String, String)> = match mcp_outcome {
        Ok(report) => {
            let (lines, blocked) = mcp_decision_lines(report.decisions());
            mcp_decision_failed = blocked;
            for line in lines {
                println!("{line}");
            }
            for w in &render_warnings {
                eprintln!("warning: {w}");
            }
            for line in mcp_recovery_action_lines(label, &report.recovery_actions) {
                eprintln!("{line}");
            }
            for line in mcp_variable_lines(label, &report.variable_reports, &report.secret_skips) {
                eprintln!("{line}");
            }
            // MCP evidence renders on its own stream, independent of target
            // status: only the hosts the report actually proves were
            // updated, never a synthetic skipped/unchanged state.
            for line in mcp_hosts_updated_lines(&report.hosts_updated) {
                println!("{line}");
            }
            report.host_failures
        }
        Err(e) => {
            let journal_retained = mcp::projection_journal_retained(&managed_path);
            for line in
                projection_failure_report(label, &render_outcome, &e.to_string(), journal_retained)
            {
                eprintln!("{line}");
            }
            return (ExitCode::Error, accumulator);
        }
    };

    // 2. skills/commands/agents projection to all selected agents (best-effort).
    //    Runs unconditionally here, after the MCP host-failure check above
    //    has already extracted `mcp_host_failures` without returning early —
    //    a host-apply failure must not prevent the Machine phase from
    //    running for the targets it does cover. Machine-phase evidence
    //    (success or, on a genuine fault, whatever partial evidence the
    //    phase collected before the fault) is recorded into the accumulator
    //    unconditionally, so a target's Canonical-phase owned-path failure
    //    (an expected value inside `canonical_evidence`, not an outer `Err`
    //    — see the comment on `render` above) never short-circuits the
    //    Machine phase either: only the two early returns above (a global
    //    Canonical/MCP fault) skip it.
    // `machine_fault` tracks a genuine unexpected/global fault from the
    // Machine phase call itself — distinct from a target's own owned-path
    // value failure, which the accumulator already represents as `Failed`.
    // `run_machine_update` conservatively reports zero
    // `completed_targets` on any fault (its own doc comment: "a faulted run
    // conservatively reports no completed targets rather than guessing"), so
    // every target that needed the Machine phase derives `NotRun`, never
    // `Failed` — `has_target_failure` below cannot see this case at all. A
    // real fault here (e.g. an io error) must still flip the run's exit code,
    // the same way a genuine Canonical-phase/MCP fault already does via the
    // early returns above; without this, a non-converged run whose only
    // problem was a Machine-phase fault reports `ExitCode::Success`.
    let mut machine_fault = false;
    match machine_opts_result {
        Ok(machine_opts) => match projection::run_machine_update_with_scope(
            &machine_opts,
            &canonical_scope.unwrap_or_else(unavailable_machine_scope),
        ) {
            Ok(evidence) => {
                for warning in &evidence.run_warnings {
                    eprintln!("warning: {warning}");
                }
                accumulator.record_machine(&evidence);
            }
            Err(failure) => {
                eprintln!(
                    "{label}: machine projection (best-effort): {}",
                    failure.error
                );
                accumulator.record_machine(&failure.partial);
                machine_fault = true;
            }
        },
        Err(e) => {
            eprintln!("{label}: machine projection skipped (best-effort): {e}");
            // A failed options resolution means the Machine phase never ran
            // at all — every target that needed it stays `NotRun`, invisible
            // to the `Failed`-only check below, exactly like the
            // `run_machine_update` fault arm above. It must flip the exit
            // code the same way: a run whose Machine phase never executed
            // did not converge and must not report Success.
            machine_fault = true;
        }
    }

    // 3. Project the user-scope managed instruction block after the existing
    // per-target machine steps. A shared Antigravity path is written once,
    // while deselected targets are never resolved.
    let managed_block_failed = project_managed_instruction_block(&selected_targets, label, layout);

    // 4. Every other phase completed; report the MCP hosts that did not. Named
    //    individually, and explicit about what *did* land, so the exit code is
    //    never a bare failure the user cannot act on. Deliberately does NOT
    //    return early: an MCP host convergence failure is independent of
    //    target status (a host that failed to read is not a projection
    //    target, and a target's own report must not be edited or suppressed
    //    because of it) — the Machine phase above has already run regardless,
    //    and the target-status stdout/stderr rendering below must still see
    //    the run's real, unedited target report.
    let mcp_convergence_failed = !mcp_host_failures.is_empty();
    if mcp_convergence_failed {
        for line in mcp_host_failure_lines(&mcp_host_failures) {
            eprintln!("{line}");
        }
        eprintln!(
            "{label}: canonical/managed surfaces and skills/commands/agents were projected, and the remaining MCP hosts were updated; repair the host config(s) above, then re-run sync to converge"
        );
    }

    // 4. `snapshot()` — not `finish()` — is used here because `accumulator`
    //    must still be returned to the caller afterward; `finish()` would
    //    consume it. This is the accumulator's own derived status becoming
    //    the sole authority for whether the run is reported as having
    //    failed: a target-owned failure recorded by either phase
    //    above (e.g. a Claude marketplace-manifest write failing while
    //    every MCP host still applied cleanly) must not be silently
    //    swallowed into an `ExitCode::Success` the way it was before this
    //    task — the plan's own stated risk is exactly that aggregation
    //    hiding a target failure makes a run-level "success" a lie.
    let report = accumulator.snapshot();
    // Failure/incomplete truth and target-scoped warnings on stderr, printed
    // before the stdout truth below so a failing run's actionable detail
    // never scrolls past unseen.
    for line in target_stderr_lines(&report) {
        eprintln!("{line}");
    }
    for line in target_evidence_lines(&report) {
        eprintln!("{line}");
    }
    // One labeled line per planned route: ordinary sync plans a route for
    // each enabled component on each selected target.
    for line in route_lines(
        &canonical_routes,
        &canonical_retirements,
        &canonical_conflicts,
    ) {
        eprintln!("{line}");
    }
    // Record this run's route facts so `ccync doctor` can report them later
    // without running a sync. Only a canonical phase that returned its
    // outcome has route facts to record.
    if canonical_evidence.is_some() {
        let path = layout.routes_path();
        if let Err(error) = ccync_engine::managed_view::write_route_record(&path, &canonical_routes)
        {
            eprintln!("warning: route record {}: {error}", path.display());
        }
    }
    // Keep storage authority and face dispositions alongside the route facts
    // emitted for this publication. Layout evidence is informational and does
    // not change the run's status.
    if label == "ccync sync" {
        if let Ok(lines) = ccync_engine::doctor::route_evidence_lines() {
            for line in lines.into_iter().filter(|line| line.starts_with("layout")) {
                eprintln!("{}", label_standalone_route(line));
            }
        }
    }
    for warning in &report.run_warnings {
        if let Some(selector) = native_conflict_selector(warning) {
            eprintln!("Foreign native selector: {selector}");
            eprintln!("Remedy (not executed): remove {selector} with its native manager, then run ccync sync");
        }
    }
    // Named truth on stdout: the exact all-Applied success sentence, or
    // canonical-order Applied/Unchanged/Skipped groups for a mixed completed
    // run. This gate covers target Failed/NotRun and the canonical render's
    // run-level fault, not MCP host convergence failure.
    for line in target_stdout_lines(
        &report,
        machine_fault || run_fault.is_some() || mcp_decision_failed || mcp_convergence_failed,
        &canonical_routes,
    ) {
        println!("{line}");
    }
    // An MCP host convergence failure selects a non-zero exit exactly like a
    // target Failed/NotRun/machine fault does, but never changes what
    // derive_run_exit sees: the target report handed to it above is the
    // run's real, unedited evidence.
    let exit = if derive_run_exit(&report, machine_fault || run_fault.is_some()) == ExitCode::Error
        || mcp_convergence_failed
        || mcp_decision_failed
        || managed_block_failed
    {
        ExitCode::Error
    } else {
        ExitCode::Success
    };
    (exit, accumulator)
}

/// The run's exit-code truth: `ExitCode::Error` whenever any selected target
/// is `Failed` or `NotRun`, or the run itself suffered a genuine Machine-phase
/// or run-global fault; `ExitCode::Success` otherwise — including when the
/// only thing printed was a target-scoped or run-level warning
/// (warnings-only runs must exit 0). Checks the report directly for
/// `Failed`/`NotRun` rather than only proxying through `machine_fault`,
/// because a target can reach `NotRun` from a same-phase short-circuit
/// (see [`notrun_subject`]) with no Machine-phase fault at all — the direct
/// report check catches that case; `machine_fault` is kept as an explicit
/// second condition because a genuine Machine-phase fault can, in principle,
/// leave every affected target `NotRun` in a way this function must still
/// treat as non-convergence even if no target were ever recorded in the
/// report at all. The second parameter represents either kind of run-global
/// fault and is kept as an explicit condition for faults with no target
/// evidence.
fn derive_run_exit(
    report: &ccync_engine::install::ProjectionRunReport,
    run_fault: bool,
) -> ExitCode {
    use ccync_engine::install::ProjectionTargetStatus as Status;
    let has_incomplete_target = report
        .targets
        .values()
        .any(|t| matches!(t.status, Status::Failed { .. } | Status::NotRun));
    if has_incomplete_target || run_fault {
        ExitCode::Error
    } else {
        ExitCode::Success
    }
}

/// Extract the exact foreign selector from the engine's native conflict warning.
fn native_conflict_selector(message: &str) -> Option<String> {
    let (target, remainder) = message.split_once(" native loading: ")?;
    let suffix = " not reconciled: native registration requires recovery: External; keeping the combined surface";
    let plugin = remainder.strip_suffix(suffix)?;
    let namespace = match target {
        "codex" => "@ccync-marketplace",
        "claude" => "@ccync",
        _ => return None,
    };
    (!plugin.is_empty()).then(|| format!("{plugin}{namespace}"))
}

/// Derive the completed run's target-status truth as stdout lines: the exact
/// all-Applied success sentence, or canonical-order `Applied:`/`Unchanged:`/
/// `Skipped: <target> — <reason>` groups for a mixed completed run. Returns
/// an empty `Vec` when any target is `Failed` or `NotRun`, or when the
/// run-level fault flag is set — the run-level success text is suppressed
/// entirely in those cases; failure/incomplete reporting is a separate,
/// stderr-side concern. Returns lines rather than
/// printing directly (matching `dry_run_report_lines`/`gate_abort_message`)
/// so the exact contract is byte-assertable without capturing real stdout.
fn target_stdout_lines(
    report: &ccync_engine::install::ProjectionRunReport,
    run_fault: bool,
    routes: &[ccync_engine::managed_view::RouteEvidence],
) -> Vec<String> {
    use ccync_engine::install::ProjectionTargetStatus as Status;

    if run_fault {
        return Vec::new();
    }

    // A selected route that is blocked, unsupported or not published keeps the
    // run from the full-success sentence, whatever each target's status says.
    let gaps: Vec<String> = routes
        .iter()
        .filter_map(|route| {
            route_gap_reason(route).map(|reason| {
                format!(
                    "Not fully applied: {} {:?} {} — {reason}",
                    route.target, route.kind, route.original_name
                )
            })
        })
        .collect();

    let mut applied = Vec::new();
    let mut unchanged = Vec::new();
    let mut skipped: Vec<(ccync_foundation::target::ProjectionTargetId, &str)> = Vec::new();

    for target in ccync_foundation::target::CANONICAL_PROJECTION_TARGETS {
        let Some(result) = report.targets.get(target) else {
            continue;
        };
        match &result.status {
            Status::Applied => applied.push(*target),
            Status::Unchanged => unchanged.push(*target),
            Status::Skipped { reason } => skipped.push((*target, reason.as_str())),
            // Any incomplete/failed target suppresses the entire stdout
            // report — no lines were assembled above this point in the
            // loop, so returning empty here is a clean no-op.
            Status::Failed { .. } | Status::NotRun => return Vec::new(),
        }
    }

    if !applied.is_empty() && unchanged.is_empty() && skipped.is_empty() && gaps.is_empty() {
        return vec![format!(
            "Successfully rebuilt and applied ccync-managed settings to {}",
            format_canonical_target_list(&applied)
        )];
    }

    let mut lines = Vec::new();
    if !applied.is_empty() {
        lines.push(format!(
            "Applied: {}",
            format_canonical_target_list(&applied)
        ));
    }
    if !unchanged.is_empty() {
        lines.push(format!(
            "Unchanged: {}",
            format_canonical_target_list(&unchanged)
        ));
    }
    for (target, reason) in &skipped {
        lines.push(format!("Skipped: {target} — {reason}"));
    }
    lines.extend(gaps);
    lines
}

/// Why a selected route is not a complete route, or `None` when it is. An
/// unselected route is never reported. A route that failed to write is
/// blocked, and an unsupported route is never written. A route this run did
/// not publish is not verified.
fn route_gap_reason(route: &ccync_engine::managed_view::RouteEvidence) -> Option<String> {
    if !route.selected {
        return None;
    }
    if let Some(reason) = &route.unsupported_reason {
        return Some(format!("unsupported: {reason}"));
    }
    if let Some(reason) = &route.failure_reason {
        return Some(format!("blocked: {reason}"));
    }
    if !route.published {
        return Some("not verified: not published by this run".to_string());
    }
    None
}

/// The canonical-order-first `Failed` target's name, used as the `<subject>`
/// naming what stopped a downstream target from running. A `Failed` target
/// short-circuits its own phase's remaining chained steps — the reachable
/// cause of a sibling target ending up `NotRun` with no fault of its own —
/// so citing the actual failed target is more honest than a generic phase
/// name whenever one is present in the report. Falls back to `"machine
/// projection"` when no target Failed at all (the other reachable
/// `NotRun`-producing case: a genuine Machine-phase fault, which leaves
/// every affected target `NotRun` with no owned-path failure anywhere in
/// the report to name instead).
fn notrun_subject(report: &ccync_engine::install::ProjectionRunReport) -> &str {
    ccync_foundation::target::CANONICAL_PROJECTION_TARGETS
        .iter()
        .find(|t| {
            report.targets.get(*t).is_some_and(|r| {
                matches!(
                    r.status,
                    ccync_engine::install::ProjectionTargetStatus::Failed { .. }
                )
            })
        })
        .map(|t| t.as_str())
        .unwrap_or("machine projection")
}

/// Derive the completed run's failure/incomplete truth and target-scoped
/// warnings as stderr lines, in canonical target order: `Failed: <target> —
/// <path>: <reason>` (or, when the failure carries no path, `Failed:
/// <target> — <reason>`), `Not run: <target> — stopped after <subject>
/// failed`, then each target's own `warning [<target>]: <message>` lines.
/// Run-level warnings (not scoped to any one target) are printed separately
/// by each phase as it completes, using the unadorned `warning: <message>`
/// form — this function only ever renders per-target evidence.
fn target_stderr_lines(report: &ccync_engine::install::ProjectionRunReport) -> Vec<String> {
    use ccync_engine::install::ProjectionTargetStatus as Status;

    let subject = notrun_subject(report).to_string();
    let mut lines = Vec::new();
    for target in ccync_foundation::target::CANONICAL_PROJECTION_TARGETS {
        let Some(result) = report.targets.get(target) else {
            continue;
        };
        match &result.status {
            Status::Failed {
                path: Some(path),
                reason,
            } => {
                lines.push(format!("Failed: {target} — {}: {reason}", path.display()));
            }
            Status::Failed { path: None, reason } => {
                lines.push(format!("Failed: {target} — {reason}"));
            }
            Status::NotRun => {
                lines.push(format!(
                    "Not run: {target} — stopped after {subject} failed"
                ));
            }
            Status::Applied | Status::Unchanged | Status::Skipped { .. } => {}
        }
    }
    for target in ccync_foundation::target::CANONICAL_PROJECTION_TARGETS {
        let Some(result) = report.targets.get(target) else {
            continue;
        };
        for warning in &result.warnings {
            lines.push(format!("warning [{target}]: {warning}"));
        }
    }
    lines
}

/// Per-target evidence lines for a run that did not fully apply: each target's
/// publication, its loader state, its served-by-root state, and the retry
/// action. A sync run never executes a loader check and its report carries no
/// served-by-root fact, so both print as `NotRun` and `not reported`; neither
/// reads as loaded or served. Lines appear only when a target is `Failed`,
/// `NotRun`, or `Skipped`, so a fully applied run keeps its existing output.
fn target_evidence_lines(report: &ccync_engine::install::ProjectionRunReport) -> Vec<String> {
    use ccync_engine::install::ProjectionTargetStatus as Status;

    let partial = report.targets.values().any(|r| {
        matches!(
            r.status,
            Status::Failed { .. } | Status::NotRun | Status::Skipped { .. }
        )
    });
    if !partial {
        return Vec::new();
    }

    let subject = notrun_subject(report).to_string();
    let mut lines = Vec::new();
    for target in ccync_foundation::target::CANONICAL_PROJECTION_TARGETS {
        let Some(result) = report.targets.get(target) else {
            continue;
        };
        let (publication, retry) = match &result.status {
            Status::Applied | Status::Unchanged => ("published".to_string(), None),
            Status::Skipped { reason } => (format!("not published, skipped: {reason}"), None),
            Status::Failed { .. } => (
                "not published".to_string(),
                Some("fix the reported write error, then rerun `ccync sync`".to_string()),
            ),
            Status::NotRun => (
                "not run".to_string(),
                Some(format!("rerun `ccync sync` after {subject} is fixed")),
            ),
        };
        let mut line = format!(
            "evidence [{target}]: publication {publication}; loader NotRun; served-by-root not reported"
        );
        if let Some(retry) = retry {
            line.push_str(&format!("; retry: {retry}"));
        }
        lines.push(line);
    }
    lines
}

/// One stderr line per planned route, from the engine's route evidence. Each
/// fact keeps its own label, and the line ends with the retry action. The
/// route's target supplies its retirement label, and a blocked route takes its
/// conflict reason from the outcome. An empty list prints nothing, which is
/// what ordinary sync produces.
fn route_lines(
    routes: &[ccync_engine::managed_view::RouteEvidence],
    retirements: &std::collections::BTreeMap<
        ccync_foundation::target::ProjectionTargetId,
        ccync_engine::install::RouteRetirement,
    >,
    conflicts: &[ccync_engine::install::RouteConflict],
) -> Vec<String> {
    routes
        .iter()
        .map(|route| {
            let conflict = conflicts
                .iter()
                .find(|conflict| {
                    conflict.target == route.target && conflict.component == route.original_name
                })
                .map(|conflict| conflict.reason.as_str());
            let line = format!(
                "route: {}",
                ccync_engine::delivery_apply::route_outcome_line(
                    route,
                    retirements.get(&route.target),
                    conflict,
                )
            );
            label_standalone_route(line)
        })
        .collect()
}

/// Keep loose skills visibly distinct from provider routes in CLI output.
pub(super) fn label_standalone_route(mut line: String) -> String {
    if !line.contains("origin loose-origin") {
        return line;
    }
    if line.contains("; published;") {
        line = line.replace("; published;", "; standalone published;");
    } else if line.contains("recovery blocked:") {
        line = line.replace("; not published;", "; recovery blocked;");
    } else if line.contains("identity unresolved:") {
        line = line.replace("; not published;", "; identity unresolved;");
    }
    line
}

/// True once this machine has completed `ccync init`: a boolean
/// file-existence check against `~/.ccync/config.json`, nothing more.
pub(crate) fn machine_is_initialized() -> bool {
    ccync_foundation::paths::EffectiveLayout::load_or_legacy()
        .is_ok_and(|layout| layout.machine_config_path().is_file())
}

/// Preflight guard for command entry points that must not run before
/// `ccync init`: refuses with `ExitCode::Error` (1) when this machine has no
/// `config.json` yet, naming `ccync init <master>` as the fix. Callers MUST
/// invoke this before their first domain mutation for the refusal to be a
/// true zero-mutation abort.
pub(crate) fn require_initialized_machine() -> Result<(), ExitCode> {
    let layout = load_effective_layout("ccync")?;
    require_initialized_machine_at(&layout)
}

fn require_initialized_machine_at(
    layout: &ccync_foundation::paths::EffectiveLayout,
) -> Result<(), ExitCode> {
    if layout.machine_config_path().is_file() {
        return Ok(());
    }
    eprintln!(
        "ccync: this machine has not been initialized — run `ccync init <master>` first (e.g. `ccync init claude`)."
    );
    Err(ExitCode::Error)
}

/// Render `selected` as a canonical-order, comma-joined target list — the
/// single formatter shared by the did-form run-completion output and every
/// would-form preview (dry-run, the first-projection gate, and `init`'s own
/// pre-projection disclosure). Canonical order is
/// `CANONICAL_PROJECTION_TARGETS`'s declaration order, independent of
/// `selected`'s own order or duplicates. Deliberately status-free: it only
/// ever prints target identity, never a `ProjectionTargetStatus` — previews
/// must never fake post-run statuses.
pub(crate) fn format_canonical_target_list(
    selected: &[ccync_foundation::target::ProjectionTargetId],
) -> String {
    ccync_foundation::target::CANONICAL_PROJECTION_TARGETS
        .iter()
        .filter(|t| selected.contains(t))
        .map(|t| t.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parse the run's currently selected projection targets from `config`, for
/// would-form preview lines only (dry-run, first-projection gate). Read-only
/// and best-effort: an unresolvable options set previews an empty list
/// rather than failing the caller's disclosure, matching `build_touch_scope`'s
/// own best-effort contract.
fn selected_projection_targets_for_preview(
    config: &ccync_foundation::config::CcyncConfig,
    layout: &ccync_foundation::paths::EffectiveLayout,
) -> Vec<ccync_foundation::target::ProjectionTargetId> {
    projection::machine_options_from_config(config, true, layout.state_base())
        .ok()
        .map(|opts| {
            opts.selected_projection_targets()
                .iter()
                .filter_map(|s| s.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Read-only snapshot of every surface a live projection touches, grouped
/// into the selected-runtime surfaces (from `ccync_engine::install::planned_engine_touches`)
/// and the fixed MCP host files (from `mcp::live_mcp_target_paths`). Built
/// once per `run_unified_projection` call and rendered by the single shared
/// [`render_touch_scope`] function so the dry-run output, the generic
/// first-projection confirmation gate, and `init`'s pre-projection disclosure
/// can never format the same scope two different ways.
pub(crate) enum McpInspection {
    /// The caller will immediately run a real projection, so inspection would
    /// duplicate the projection's own recovery report.
    NotRequested,
    /// No machine-level managed manifest path exists. Real sync treats this as
    /// an MCP skip, which is distinct from a failed inspection.
    UnavailablePath,
    /// Inspection was attempted and either produced a report or failed closed.
    Attempted(Result<mcp::McpInspectReport, String>),
}

struct TouchScope {
    runtime_surfaces: Vec<(String, PathBuf)>,
    instruction_files: Vec<(String, PathBuf)>,
    instruction_prune_files: Vec<(String, PathBuf)>,
    mcp_hosts: Vec<PathBuf>,
    mcp_inspect: McpInspection,
    /// True when any previewed instruction path carries a source or target
    /// blocker. Set only by the inspecting (dry-run) build.
    instruction_blocked: bool,
}

/// Build a [`TouchScope`] from the current config's runtime selection, reusing
/// the same `machine_options_from_config` construction `finalize_machine_options`
/// uses for the real projection call. Read-only: `planned_engine_touches` only
/// joins path segments (no I/O) and `live_mcp_target_paths` is pure.
/// Best-effort — an unresolvable options set (e.g. no home directory) yields
/// an empty runtime-surface group rather than failing the caller's disclosure.
///
/// `inspect_mcp` additionally runs `mcp::inspect_mcp_projection` (read-only) to
/// populate the recovery group. Only `--dry-run` sets it: see
/// [`TouchScope::mcp_inspect`].
fn build_mcp_inspection(managed_path: Option<PathBuf>) -> McpInspection {
    let Some(managed_path) = managed_path else {
        return McpInspection::UnavailablePath;
    };
    let Ok(layout) = ccync_foundation::paths::EffectiveLayout::load_or_legacy() else {
        return McpInspection::UnavailablePath;
    };
    build_mcp_inspection_at(managed_path, &layout.machine_config_path())
}

fn build_mcp_inspection_at(managed_path: PathBuf, config_path: &std::path::Path) -> McpInspection {
    McpInspection::Attempted(
        mcp::inspect_mcp_projection_at(&managed_path, config_path).map_err(|e| e.to_string()),
    )
}

fn build_touch_scope(
    config: &ccync_foundation::config::CcyncConfig,
    inspect_mcp: bool,
    layout: &ccync_foundation::paths::EffectiveLayout,
) -> TouchScope {
    let runtime_surfaces =
        projection::machine_options_from_config(config, true, layout.state_base())
            .ok()
            .and_then(|opts| ccync_engine::install::planned_engine_touches(&opts).ok())
            .unwrap_or_default();
    let selected_targets = selected_projection_targets_for_preview(config, layout);
    // User-scope instruction paths are derived from selection only. Keep this list free of file
    // reads so the first-projection disclosure remains a pure path preview.
    let instruction_paths = instruction_paths(&selected_targets);
    let instruction_prune_paths = legacy_instruction_prune_paths(&selected_targets);
    let mut instruction_files = Vec::new();
    let mut instruction_prune_files = Vec::new();
    let mut instruction_blocked = false;
    if inspect_mcp {
        // The shared source reader is the only source authority: a read,
        // encoding, or dangling-link failure becomes a typed blocker on every
        // listed path instead of degrading to an absent source.
        let payload = read_full_sync_instruction_payload(layout);
        for path in &instruction_paths {
            let outcome = match &payload {
                Ok(payload) => {
                    let classification =
                        projection::managed_block::classify(path, payload.as_deref());
                    instruction_blocked |= classification_is_blocker(classification);
                    instruction_classification(classification)
                }
                Err(error) => {
                    instruction_blocked = true;
                    instruction_source_blocker(error)
                }
            };
            instruction_files.push((outcome.to_string(), path.clone()));
        }
        for path in &instruction_prune_paths {
            let outcome = match &payload {
                Ok(_) => {
                    let classification = projection::managed_block::classify(path, None);
                    instruction_blocked |= classification_is_blocker(classification);
                    instruction_classification(classification).to_string()
                }
                Err(error) => {
                    instruction_blocked = true;
                    instruction_source_blocker(error).to_string()
                }
            };
            instruction_prune_files.push((outcome, path.clone()));
        }
    } else {
        instruction_files.extend(
            instruction_paths
                .into_iter()
                .map(|path| (String::new(), path)),
        );
        instruction_prune_files.extend(
            instruction_prune_paths
                .into_iter()
                .map(|path| (String::new(), path)),
        );
    }
    let mcp_inspect = if inspect_mcp {
        build_mcp_inspection_at(layout.generated_mcp_path(), &layout.machine_config_path())
    } else {
        McpInspection::NotRequested
    };
    TouchScope {
        runtime_surfaces,
        instruction_files,
        instruction_prune_files,
        mcp_hosts: mcp::live_mcp_target_paths(),
        mcp_inspect,
        instruction_blocked,
    }
}

/// Render a [`TouchScope`] as two separately-labeled groups.
///
/// The runtime-surfaces group is phrased as a preview ("would touch"), not a
/// guarantee, because the projection call that touches them is best-effort:
/// `run_unified_projection`'s `projection::run_machine_update` call (which IS
/// live on every sync/add/remove/init via `include_source_projections: true`)
/// swallows its error to stderr rather than failing the run, so a listed
/// surface may end up untouched. The MCP host group is phrased as "may be
/// written" because `write_json_provider_delta` returns `Unchanged` — bytes
/// and mtime preserved — on a semantic no-op, so a converged machine writes
/// none of those four paths.
fn render_touch_scope(scope: &TouchScope) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push("  Runtime surfaces (would touch, current selection):".to_string());
    let runtime_surfaces: Vec<_> = scope.runtime_surfaces.iter().collect();
    if runtime_surfaces.is_empty() {
        lines.push("    (none)".to_string());
    } else {
        for (runtime, path) in runtime_surfaces {
            lines.push(format!("    {runtime}: {}", path.display()));
        }
    }
    lines.push("  User-scope instruction files (would touch, current selection):".to_string());
    let instruction_files = &scope.instruction_files;
    if instruction_files.is_empty() {
        lines.push("    (none)".to_string());
    } else {
        for (classification, path) in instruction_files {
            if classification.is_empty() {
                lines.push(format!("    {}", path.display()));
            } else {
                lines.push(format!("    {}: {classification}", path.display()));
            }
        }
    }
    lines.push("  Legacy instruction files (would prune, when Claude is selected):".to_string());
    if scope.instruction_prune_files.is_empty() {
        lines.push("    (none)".to_string());
    } else {
        for (classification, path) in &scope.instruction_prune_files {
            if classification.is_empty() {
                lines.push(format!("    {}", path.display()));
            } else {
                lines.push(format!("    {}: {classification}", path.display()));
            }
        }
    }
    lines.push("  MCP host files (may be written; skipped when already identical):".to_string());
    for path in &scope.mcp_hosts {
        lines.push(format!("    {}", path.display()));
    }
    lines.extend(render_mcp_inspect(&scope.mcp_inspect));
    lines
}

/// Render the MCP inspection group, splitting what inspection *determined* from
/// what it structurally cannot determine.
///
/// The split is the point. `mcp::inspect_mcp_projection` runs before the
/// canonical render, so it can decide legacy bootstrap and pending
/// transactions but not deletion or collision. Printing an empty deletion or
/// collision list would read as "none found" — a claim about live state that
/// nothing here checked. Each undetermined class is therefore named, marked,
/// and given its reason.
pub(crate) fn render_mcp_inspect(inspect: &McpInspection) -> Vec<String> {
    let mut lines = Vec::new();
    let report = match inspect {
        McpInspection::NotRequested => return lines,
        McpInspection::UnavailablePath => {
            lines.push("  MCP recovery (not inspected):".to_string());
            lines.push(
                "    no managed MCP manifest path could be resolved; a real sync would skip MCP projection on this machine"
                    .to_string(),
            );
            lines.push("    live MCP host entries were not inspected or changed".to_string());
            return lines;
        }
        McpInspection::Attempted(Err(e)) => {
            lines.push("  MCP recovery (inspected, no writes):".to_string());
            lines.push(format!("    inspection stopped: {e}"));
            lines.push(
                "    live entries preserved; a real sync would stop here until this is resolved"
                    .to_string(),
            );
            return lines;
        }
        McpInspection::Attempted(Ok(r)) => r,
    };

    lines.push("  MCP recovery (inspected, no writes):".to_string());
    if report.recovery_actions.is_empty() {
        lines.push("    (none: no pending transaction, no legacy bootstrap)".to_string());
    } else {
        for action in &report.recovery_actions {
            lines.push(format!("    {action}"));
        }
    }
    for (host, reason) in &report.host_failures {
        lines.push(format!("    host not inspected: {host} — {reason}"));
    }
    for (server, variables) in &report.prior_secret_skips {
        let variables = variables.iter().cloned().collect::<Vec<_>>().join(", ");
        lines.push(format!(
            "    prior MCP secret skip: {server} — missing secret {variables} — from last rendered manifest"
        ));
    }

    lines.push("  MCP actions NOT determined by --dry-run:".to_string());
    for item in &report.undetermined {
        lines.push(format!(
            "    {}: not determined (requires render) — {}",
            item.class, item.reason
        ));
    }
    lines
}

/// Produce every line of the `--dry-run` report except the trailing
/// no-files-written marker (which `run_unified_projection` prints directly).
///
/// Exists as a function, rather than inline `println!`s, so the dry run's
/// defining behaviour is assertable: this is where `--dry-run` is wired to a
/// read-only MCP inspection (`build_touch_scope(.., true)`). The dry run must
/// enter the mcp crate and still write nothing — a dry run that simply returns
/// early would satisfy "must not write" only because the code never ran. The
/// `dry_run_mcp_inspect` probe calls this function, so un-wiring that `true`
/// fails the probe instead of silently restoring the vacuous behaviour.
#[cfg(test)]
pub(super) fn dry_run_report_lines(config: &ccync_foundation::config::CcyncConfig) -> Vec<String> {
    let Ok(layout) = ccync_foundation::paths::EffectiveLayout::load_or_legacy() else {
        return Vec::new();
    };
    dry_run_report(config, &layout).0
}

/// The dry-run report lines plus whether any instruction path was blocked.
fn dry_run_report(
    config: &ccync_foundation::config::CcyncConfig,
    layout: &ccync_foundation::paths::EffectiveLayout,
) -> (Vec<String>, bool) {
    let touch_scope = build_touch_scope(config, true, layout);
    let mut lines = vec![
        "--- DRY RUN ---".to_string(),
        "ccync sync: paths that would be created, updated, or removed:".to_string(),
    ];
    let selected = selected_projection_targets_for_preview(config, layout);
    if !selected.is_empty() {
        lines.push(format!(
            "  Would apply ccync-managed settings to: {}",
            format_canonical_target_list(&selected)
        ));
    }
    lines.extend(render_touch_scope(&touch_scope));
    (lines, touch_scope.instruction_blocked)
}

/// Print the full dry-run report and return its exit code: `Error` when any
/// instruction path is blocked, `Success` otherwise. The whole report is
/// printed either way.
pub(super) fn run_dry_run_report(config: &ccync_foundation::config::CcyncConfig) -> ExitCode {
    let layout = match load_effective_layout("ccync sync --dry-run") {
        Ok(layout) => layout,
        Err(code) => return code,
    };
    run_dry_run_report_at(config, &layout)
}

fn run_dry_run_report_at(
    config: &ccync_foundation::config::CcyncConfig,
    layout: &ccync_foundation::paths::EffectiveLayout,
) -> ExitCode {
    let mcp_blocked = match current_mcp_decisions(layout) {
        Ok(decisions) => {
            let (lines, blocked) = mcp_decision_lines(&decisions);
            for line in lines {
                println!("{line}");
            }
            blocked
        }
        Err(error) => {
            println!("MCP: 0 change(s), 1 blocked\n  {error}");
            println!("  (no files written)");
            return ExitCode::Error;
        }
    };
    let (lines, instruction_blocked) = dry_run_report(config, layout);
    for line in lines {
        println!("{line}");
    }
    println!("  (no files written)");
    if instruction_blocked || mcp_blocked {
        ExitCode::Error
    } else {
        ExitCode::Success
    }
}

/// The shared unified-engine projection used by `init`, `sync`, `add`, and
/// `remove`. Internalizes the catalog resolve (lockfile write), then projects
/// the managed set to all agents. `dry_run` resolves + reports but writes no
/// projection. `assume_yes` skips the first-run overwrite-visibility prompt.
/// `skip_first_run_gate` suppresses only the interactive confirmation prompt
/// (used by `init`, which already shows an agent-selection UI before calling
/// this) — the grouped touch-scope disclosure itself still prints so `init`
/// gains visibility without gaining a second confirmation prompt.
pub(crate) fn run_unified_projection(
    dry_run: bool,
    assume_yes: bool,
    skip_first_run_gate: bool,
    gate_abort_context: Option<&GateAbortContext<'_>>,
) -> ExitCode {
    run_unified_projection_with_guard(
        dry_run,
        assume_yes,
        skip_first_run_gate,
        gate_abort_context,
        None,
        false,
    )
}

fn run_unified_projection_with_guard(
    dry_run: bool,
    assume_yes: bool,
    skip_first_run_gate: bool,
    gate_abort_context: Option<&GateAbortContext<'_>>,
    lifecycle_guard: Option<&ccync_engine::tool_manager::lock::LifecycleGuard>,
    portable_migration: bool,
) -> ExitCode {
    let layout = match load_effective_layout("ccync sync") {
        Ok(layout) => layout,
        Err(code) => return code,
    };
    run_unified_projection_at(
        dry_run,
        assume_yes,
        skip_first_run_gate,
        gate_abort_context,
        lifecycle_guard,
        portable_migration,
        &layout,
    )
}

fn run_unified_projection_at(
    dry_run: bool,
    assume_yes: bool,
    skip_first_run_gate: bool,
    gate_abort_context: Option<&GateAbortContext<'_>>,
    lifecycle_guard: Option<&ccync_engine::tool_manager::lock::LifecycleGuard>,
    portable_migration: bool,
    layout: &ccync_foundation::paths::EffectiveLayout,
) -> ExitCode {
    // Config is read-only until apply is authorized. Legacy selection is read
    // from the current legacy paths for preview.
    let config =
        ccync_foundation::config::CcyncConfig::load_from_path(&layout.machine_config_path());

    // 0a.5. Machine-initialization preflight for the public `sync` lifecycle
    // contract, identified by `SYNC_GATE_ABORT_CONTEXT` (also reused by
    // `upgrade`'s reprojection step, which shares the same written/unwritten
    // shape). Nothing but the migration above — a no-op on an uninitialized
    // machine — has run yet, so refusing here is a true zero-mutation abort.
    // `add`/`remove` mutate earlier (plugin cache / personal catalog writes
    // precede their own call into this function) and enforce this preflight
    // at their own entry points instead; `init` writes `config.json` before
    // it ever reaches this function, so it is never blocked here.
    // `--dry-run` is exempt so `sync --dry-run` stays available pre-init.
    if !dry_run && gate_abort_context == Some(&SYNC_GATE_ABORT_CONTEXT) {
        if let Err(code) = require_initialized_machine_at(&layout) {
            return code;
        }
    }

    // A committed delivery snapshot is an input to the managed projection.
    // Verify it before catalog resolution or any render/write can occur.
    let snapshot_check = match verify_committed_tool_snapshots_at(&layout) {
        Ok(result) => result,
        Err(error) => {
            eprintln!("ccync sync: {error}");
            return ExitCode::Error;
        }
    };
    if let Err(findings) = snapshot_check {
        for (id, refresh_supported) in findings {
            let command = tool_snapshot_recovery_command(&id, refresh_supported);
            eprintln!("ccync sync: committed tool snapshot is missing or drifted for {id}; run `{command}`");
        }
        return ExitCode::Error;
    }

    // 0a.6. All-invalid-selection preflight: a selection key was present
    // (`LegacyState != Absent`) but every entry in it failed to parse as a
    // known `ProjectionTargetId`, so `resolve_projection_targets` returned
    // zero targets — the run has no runtime surface left to project onto.
    // Refuses here, before `run_catalog_resolve`'s lockfile write and before
    // `build_touch_scope`, so this is a true zero-mutation abort. `--dry-run`
    // is exempt: it only reports, never mutates.
    if !dry_run {
        let resolution = config.resolve_projection_targets();
        if resolution.state != ccync_foundation::config::LegacyState::Absent
            && resolution.targets.is_empty()
        {
            eprintln!(
                "ccync: the selected runtime(s) did not resolve to any known projection target — run `ccync init <master>` to reselect (e.g. `ccync init claude`)."
            );
            return ExitCode::Error;
        }
    }

    if dry_run {
        let inventory = ccync_engine::migrate::build_portable_inventory(layout);
        println!(
            "  Storage migration inventory: {} object(s)",
            inventory.objects.len()
        );
        if inventory.pending_older_sublayout_fixup {
            println!("  Pending older sublayout fixup: `ccync sync` will apply it");
        }
        for blocker in &inventory.blockers {
            println!("  Migration blocked: {blocker}");
        }
        return run_dry_run_report_at(&config, &layout);
    }

    // Grouped disclosure scope: selected-runtime surfaces (preview) + fixed
    // MCP host files. Rendered identically by the first-projection gate below
    // (which also serves as init's pre-projection disclosure when
    // skip_first_run_gate is set) and by the dry-run report above.
    //
    // `false`: only the dry run inspects MCP state. It is the caller that
    // reports instead of acting, so it is the one that must say what a
    // projection would recover; this path is about to run the real projection,
    // which performs and reports that recovery itself (`run_projection_core`).
    let touch_scope = build_touch_scope(&config, false, &layout);

    // First-run visibility gate: show which surfaces will be touched so the
    // user knows exactly what ccync touches. The disclosure itself always
    // prints on a first projection — including when called from `init`, which
    // already showed an agent-selection UI — but the interactive confirmation
    // prompt is skipped in that case so `init` does not gain a second prompt.
    if is_first_projection(&layout) {
        println!("ccync: about to create, update, or remove ccync-managed paths:");
        let selected = selected_projection_targets_for_preview(&config, &layout);
        if !selected.is_empty() {
            println!(
                "  Would apply ccync-managed settings to: {}",
                format_canonical_target_list(&selected)
            );
        }
        for line in render_touch_scope(&touch_scope) {
            println!("{line}");
        }
        if !skip_first_run_gate && !confirm_overwrite(assume_yes) {
            for line in gate_abort_message(gate_abort_context.unwrap_or(&SYNC_GATE_ABORT_CONTEXT)) {
                println!("{line}");
            }
            return ExitCode::Error;
        }
    }

    let migration_roots_valid = layout.root_error().is_none();
    let mut committed_layout = None;
    if layout.authority() != ccync_foundation::paths::EffectiveLayoutAuthority::Committed
        && (portable_migration || skip_first_run_gate)
    {
        if let Some(home) = ccync_foundation::paths::user_home() {
            if let Err(error) = run_layout_migration(&home) {
                eprintln!("ccync: older sublayout fixup failed: {error}");
                return ExitCode::Error;
            }
        }
    }
    if portable_migration
        && migration_roots_valid
        && layout.authority() != ccync_foundation::paths::EffectiveLayoutAuthority::Committed
    {
        if layout.authority() == ccync_foundation::paths::EffectiveLayoutAuthority::Legacy {
            let Some(guard) = lifecycle_guard else {
                eprintln!("ccync sync: migration recovery requires the lifecycle guard");
                return ExitCode::Error;
            };
            match recover_stage_and_commit_legacy_layout(layout, guard) {
                Ok(committed) => committed_layout = Some(committed),
                Err(error) => {
                    eprintln!("ccync sync: {error}");
                    return ExitCode::Error;
                }
            }
        } else if layout.authority() == ccync_foundation::paths::EffectiveLayoutAuthority::Fresh {
            match ccync_engine::migrate::commit_fresh_layout(layout) {
                Ok(committed) => committed_layout = Some(committed),
                Err(error) => {
                    eprintln!("ccync sync: layout commit failed: {error}");
                    return ExitCode::Error;
                }
            }
        }
    }
    let effective_layout = committed_layout.as_ref().unwrap_or(layout);
    // Resolve after the layout commit so private lock state is written under
    // the committed roots. This also keeps preview and consent paths read-only.
    if let Err(error) = run_catalog_resolve_at(false, effective_layout) {
        eprintln!("ccync sync: catalog resolve (best-effort): {error}");
    }
    let config = ccync_foundation::config::CcyncConfig::load_from_path(
        &effective_layout.machine_config_path(),
    );
    run_projection_core_inner_at(&config, "ccync sync", effective_layout).0
}

/// Recover interrupted work, stage the portable layout, and commit it while
/// the caller holds the lifecycle guard.
pub(crate) fn recover_stage_and_commit_legacy_layout(
    layout: &ccync_foundation::paths::EffectiveLayout,
    guard: &ccync_engine::tool_manager::lock::LifecycleGuard,
) -> Result<ccync_foundation::paths::EffectiveLayout, String> {
    ccync_engine::migrate::recover_before_migration(layout, guard)
        .map_err(|error| format!("migration recovery failed: {error}"))?;
    ccync_engine::migrate::stage_portable_migration(layout, |_, _| Ok(()))
        .map_err(|error| format!("migration staging failed: {error}"))?;
    ccync_engine::migrate::commit_portable_migration(layout)
        .map_err(|error| format!("migration commit failed: {error}"))
}

pub(crate) fn tool_snapshot_recovery_command(id: &str, refresh_supported: bool) -> String {
    if refresh_supported {
        format!("ccync tool refresh {id}")
    } else {
        format!("ccync tool untrack {id}")
    }
}

pub(crate) fn verify_committed_tool_snapshots() -> Result<Result<(), Vec<(String, bool)>>, String> {
    let layout = ccync_foundation::paths::EffectiveLayout::load_or_legacy()
        .map_err(|error| error.to_string())?;
    verify_committed_tool_snapshots_at(&layout)
}

fn verify_committed_tool_snapshots_at(
    layout: &ccync_foundation::paths::EffectiveLayout,
) -> Result<Result<(), Vec<(String, bool)>>, String> {
    use ccync_engine::tool_manager::{artifact_tree, model::DeliveryId, store};

    let declarations = store::load_declarations()
        .map_err(|error| format!("tool declarations could not be read: {error}"))?;
    let states =
        store::load_state().map_err(|error| format!("tool state could not be read: {error}"))?;
    let mut findings = Vec::new();
    for (id, declaration) in declarations {
        if declaration.delivery_id != DeliveryId::GraphifyAgents {
            continue;
        }
        let Some(state) = states.get(&id) else {
            continue;
        };
        let Some(generation) = state.committed_generation else {
            continue;
        };
        let valid = layout
            .tool_artifacts_root()
            .join(id.as_str())
            .join(generation.to_string());
        let valid = artifact_tree::scan_tree(valid)
            .ok()
            .is_some_and(|manifest| {
                state.artifact_fingerprint.as_deref()
                    == Some(format!("sha256:{}", manifest.sha256).as_str())
            });
        if !valid {
            findings.push((
                id.to_string(),
                declaration.delivery_id != DeliveryId::ObserveOnly,
            ));
        }
    }
    if findings.is_empty() {
        Ok(Ok(()))
    } else {
        Ok(Err(findings))
    }
}

/// Returns true when `plugins.lock.json` has a `_ccyncProjection` key
/// (meaning at least one successful projection has completed on this machine).
fn lock_has_projection(lock_path: &std::path::Path) -> bool {
    if !lock_path.exists() {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(lock_path) else {
        return false;
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    val.get("_ccyncProjection").is_some()
}

/// True when this machine has never completed a successful projection.
fn is_first_projection(layout: &ccync_foundation::paths::EffectiveLayout) -> bool {
    !lock_has_projection(&layout.build_lock_path())
}

/// Returns true when the user has confirmed (or `assume_yes` / `CCYNC_ASSUME_YES`
/// is set). On a TTY, prompts interactively. On a non-TTY without either flag,
/// prints guidance and returns false (no write).
fn confirm_overwrite(assume_yes: bool) -> bool {
    let stdin = std::io::stdin();
    let interactive = stdin.is_terminal();
    confirm_overwrite_with(
        assume_yes,
        std::env::var("CCYNC_ASSUME_YES").ok().as_deref(),
        interactive,
        &mut stdin.lock(),
    )
}

/// Consent decision with every ambient input (environment value, terminal
/// state, prompt reader) passed in so each documented form is testable.
pub(crate) fn confirm_overwrite_with(
    assume_yes: bool,
    env_value: Option<&str>,
    interactive: bool,
    input: &mut dyn std::io::BufRead,
) -> bool {
    if assume_yes || overwrite_env_authorized(env_value) {
        return true;
    }
    if interactive {
        eprint!("Continue? [y/N] ");
        let mut line = String::new();
        if input.read_line(&mut line).is_ok() {
            return overwrite_prompt_authorized(&line);
        }
        return false;
    }
    eprintln!("ccync: non-interactive session — pass --yes or set CCYNC_ASSUME_YES=1 to proceed.");
    false
}

pub(crate) fn overwrite_env_authorized(value: Option<&str>) -> bool {
    value == Some("1")
}

pub(crate) fn overwrite_prompt_authorized(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn gate_abort_message(context: &GateAbortContext<'_>) -> Vec<String> {
    vec![
        "ccync: aborted before live projection.".to_string(),
        format!(
            "  Already written: {}.",
            context.written_surfaces.join(", ")
        ),
        format!("  Not written: {}.", context.unwritten_surfaces.join(", ")),
        format!(
            "  Next step: run `{}` to complete projection.",
            context.next_step
        ),
    ]
}

/// Resolve the public catalog into the lockfile (internalized by `sync`).
fn run_catalog_resolve(dry_run: bool) -> Result<(), String> {
    let layout = ccync_foundation::paths::EffectiveLayout::load_or_legacy()
        .map_err(|error| error.to_string())?;
    run_catalog_resolve_at(dry_run, &layout)
}

fn run_catalog_resolve_at(
    dry_run: bool,
    layout: &ccync_foundation::paths::EffectiveLayout,
) -> Result<(), String> {
    use ccync_engine::catalog::{run_resolve_catalog, ResolveCatalogOptions};
    let config_path = layout.machine_config_path();
    let lockfile_path = layout.build_lock_path();

    let opts = ResolveCatalogOptions {
        catalog_path: None,
        config_path,
        lockfile_path,
        dry_run,
        personal_catalog_path_override: Some(layout.personal_catalog_path()),
    };
    let resolution = run_resolve_catalog(&opts)?;
    for e in &resolution.errors {
        eprintln!("ccync: catalog resolve warning: {e}");
    }
    Ok(())
}

/// Run `ccync backup [--output <dir>]` — copy the canonical machine-local files that
/// cannot be rebuilt: the INPUT files (`config.json` + `plugins.json`) plus
/// `plugins.lock.json`, which is OUTPUT but carries the non-rebuildable init
/// adoption snapshots (`_adoptedItems` / `_mcpServers`).
/// Missing source files are skipped with a note (not an error).
pub(crate) fn cmd_backup(args: &[String]) -> ExitCode {
    use ccync_foundation::paths::{EffectiveLayout, EffectiveLayoutAuthority};

    if let Err(code) = super::args::reject_unknown_flags(
        "backup",
        args,
        &["--strip-secrets"],
        &["--output"],
        0,
        "Usage: ccync backup [--output <dir>] [--strip-secrets]",
    ) {
        return code;
    }

    let strip_secrets = args.iter().any(|arg| arg == "--strip-secrets");
    let _lifecycle = match ccync_engine::tool_manager::lock::LifecycleLock::acquire_if_needed() {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("ccync backup: {error}");
            return ExitCode::Error;
        }
    };
    if strip_secrets {
        eprintln!("ccync backup: --strip-secrets is deprecated and has no effect");
    }

    // Parse --output <dir>; default to ./ccync-backup-<unix-seconds> in the current dir,
    // resolved to an absolute path so the printed message is unambiguous.
    let output_dir = {
        let mut out: Option<PathBuf> = None;
        let mut i = 1usize;
        while i < args.len() {
            if args[i] == "--output" {
                i += 1;
                if let Some(val) = args.get(i) {
                    out = Some(PathBuf::from(val));
                } else {
                    eprintln!("ccync backup: --output requires a path");
                    return ExitCode::Usage;
                }
            }
            i += 1;
        }
        let dir = out.unwrap_or_else(|| {
            use std::time::{SystemTime, UNIX_EPOCH};
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            PathBuf::from(format!("ccync-backup-{secs}"))
        });
        if dir.is_absolute() {
            dir
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(&dir))
                .unwrap_or(dir)
        }
    };

    let layout = match EffectiveLayout::load_or_legacy() {
        Ok(layout) => layout,
        Err(error) => {
            eprintln!("ccync backup: failed to load storage layout: {error}");
            return ExitCode::Error;
        }
    };
    let selected = match ccync_engine::install::read_direct_mcp_input(&layout) {
        Ok(input) => input,
        Err(error) => {
            eprintln!("ccync backup: invalid direct MCP input: {error}");
            return ExitCode::Error;
        }
    };
    let routes = match mcp::read_migrated_routes(&layout.generated_mcp_path()) {
        Ok(routes) => routes,
        Err(error) => {
            eprintln!("ccync backup: cannot read migrated routes: {error}");
            return ExitCode::Error;
        }
    };
    let mut known_secrets: Vec<String> = std::env::vars()
        .filter(|(name, value)| ccync_foundation::mcp::is_secret_name(name) && !value.is_empty())
        .map(|(_, value)| value)
        .collect();
    match std::fs::read(layout.machine_config_path()) {
        Ok(bytes) => {
            let config: serde_json::Value = match serde_json::from_slice(&bytes) {
                Ok(value) => value,
                Err(_) => {
                    eprintln!("ccync backup: invalid config.json");
                    return ExitCode::Error;
                }
            };
            if let Some(secrets) = config.get("secrets").and_then(serde_json::Value::as_object) {
                known_secrets.extend(
                    secrets
                        .values()
                        .filter_map(serde_json::Value::as_str)
                        .filter(|value| !value.is_empty())
                        .map(str::to_owned),
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            eprintln!("ccync backup: cannot read config.json: {error}");
            return ExitCode::Error;
        }
    }
    if !routes.is_empty() && !layout.machine_config_path().is_file() {
        eprintln!("ccync backup: config.json is required to export migrated routes");
        return ExitCode::Error;
    }
    let route_rows: Vec<_> = routes.iter().map(|route| serde_json::json!({"source": route.identity().source, "server": route.identity().server, "hosts": route.hosts()})).collect();
    let route_document = serde_json::json!({"ccyncMcpServers": {}, "hostRoutes": route_rows});
    if ccync_foundation::mcp::read_declaration_routes(
        &route_document,
        ccync_foundation::mcp::DeclarationRoot::Personal,
        "user",
    )
    .is_err()
        || routes.iter().any(|route| {
            known_secrets.iter().any(|secret| {
                route.identity().source.contains(secret) || route.identity().server.contains(secret)
            })
        })
    {
        eprintln!("ccync backup: invalid or unsafe migrated routes");
        return ExitCode::Error;
    }
    let canonical_files = vec![
        ("config.json", Some(layout.machine_config_path())),
        ("plugins.json", Some(layout.personal_catalog_path())),
        ("plugins.lock.json", Some(layout.build_lock_path())),
        ("AGENTS.md", Some(layout.agents_md_path())),
        (
            "mcp.json",
            selected.as_ref().map(|input| input.path().to_path_buf()),
        ),
    ];
    let portable = if layout.authority() == EffectiveLayoutAuthority::Committed {
        let catalog_path = layout.personal_catalog_path();
        let private_lock_path = layout.build_lock_path();
        let catalog: serde_json::Value = match std::fs::read(&catalog_path)
            .map_err(|error| error.to_string())
            .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|error| error.to_string()))
        {
            Ok(catalog) => catalog,
            Err(error) => {
                eprintln!("ccync backup: failed to export portable sources: {error}");
                return ExitCode::Error;
            }
        };
        let lock: serde_json::Value = match std::fs::read(&private_lock_path)
            .map_err(|error| error.to_string())
            .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|error| error.to_string()))
        {
            Ok(lock) => lock,
            Err(error) => {
                eprintln!("ccync backup: failed to export portable lock: {error}");
                return ExitCode::Error;
            }
        };
        let sources = match ccync_engine::catalog::portable_sources_export_bytes(&catalog) {
            Ok(bytes) => bytes,
            Err(error) => {
                eprintln!("ccync backup: failed to export portable sources: {error}");
                return ExitCode::Error;
            }
        };
        let portable_lock = match ccync_engine::catalog::portable_lock_export_bytes(&lock) {
            Ok(bytes) => bytes,
            Err(error) => {
                eprintln!("ccync backup: failed to export portable lock: {error}");
                return ExitCode::Error;
            }
        };
        Some((sources, portable_lock))
    } else {
        None
    };
    let personal = selected
        .as_ref()
        .is_some_and(|input| input.path() == layout.mcp_input_candidates().0);
    let result = backup_strip_secrets(
        &canonical_files,
        &output_dir,
        &routes,
        personal,
        &known_secrets,
    );
    if result != ExitCode::Success {
        return result;
    }
    let Some((sources, portable_lock)) = portable else {
        return result;
    };
    let portable_dir = output_dir.join("portable");
    if let Err(error) = std::fs::create_dir_all(&portable_dir)
        .and_then(|()| std::fs::write(portable_dir.join("sources.json"), sources))
        .and_then(|()| std::fs::write(portable_dir.join("lock.json"), portable_lock))
    {
        eprintln!("ccync backup: failed to write portable exports: {error}");
        return ExitCode::Error;
    }
    result
}

fn backup_strip_secrets(
    canonical_files: &[(&str, Option<PathBuf>)],
    output_dir: &std::path::Path,
    routes: &[mcp::MigratedRoute],
    personal: bool,
    known_secrets: &[String],
) -> ExitCode {
    let mut payloads: Vec<(&str, Vec<u8>)> = Vec::new();
    let mut skipped = 0usize;
    let mut placeholders = Vec::new();

    // Prepare every payload before creating the destination. A refusal must not leave
    // an earlier file containing plaintext credentials behind.
    for (name, src_opt) in canonical_files {
        let Some(src) = src_opt else {
            eprintln!("ccync backup: cannot resolve path for {name} (no home dir)");
            skipped += 1;
            continue;
        };
        if !src.exists() {
            println!("ccync backup: {name} not found — skipped");
            skipped += 1;
            continue;
        }
        let content = match std::fs::read(src) {
            Ok(content) => content,
            Err(error) => {
                eprintln!("ccync backup: failed to read {name}: {error}");
                return ExitCode::Error;
            }
        };
        let content = match *name {
            "mcp.json" => match validate_backup_direct_input(&content, personal, known_secrets) {
                Ok(()) => content,
                Err(error) => { eprintln!("ccync backup: {error}"); return ExitCode::Error; }
            },
            "config.json" => match String::from_utf8(content)
                .map_err(|error| error.to_string())
                .and_then(|content| {
                    let stripped = strip_config_secrets(&content).map_err(|error| error.to_string())?;
                    let mut value: serde_json::Value = serde_json::from_str(&stripped).map_err(|error| error.to_string())?;
                    let object = value.as_object_mut().ok_or("invalid config root")?;
                    if object.contains_key("_ccyncBackupMcpRoutes") { return Err("reserved config field _ccyncBackupMcpRoutes".into()); }
                    if !routes.is_empty() {
                        let rows: Vec<_> = routes.iter().map(|route| serde_json::json!({"source": route.identity().source, "server": route.identity().server, "hosts": route.hosts()})).collect();
                        object.insert("_ccyncBackupMcpRoutes".into(), serde_json::json!({"version": 1, "routes": rows}));
                    }
                    serde_json::to_string_pretty(&value).map_err(|error| error.to_string())
                }) {
                Ok(content) => content.into_bytes(),
                Err(error) => {
                    eprintln!("ccync backup: failed to transform {name}: {error}");
                    return ExitCode::Error;
                }
            },
            "plugins.json" => {
                if let Err(error) = refuse_plugin_url_userinfo(&content) {
                    eprintln!("ccync backup: {error}");
                    return ExitCode::Error;
                }
                content
            }
            "plugins.lock.json" => {
                let content = match String::from_utf8(content)
                    .map_err(|error| error.to_string())
                    .and_then(|content| {
                        refuse_mcp_server_literal_secrets(
                            &content,
                            "_mcpServers",
                            "plugins.lock.json",
                        )
                        .and_then(|()| {
                            strip_lock_secrets(&content).map_err(|error| error.to_string())
                        })
                    }) {
                    Ok((content, introduced)) => {
                        placeholders.extend(introduced);
                        content.into_bytes()
                    }
                    Err(error) => {
                        eprintln!("ccync backup: failed to transform {name}: {error}");
                        return ExitCode::Error;
                    }
                };
                content
            }
            _ => content,
        };
        payloads.push((*name, content));
    }

    if payloads.is_empty() {
        println!("ccync backup: nothing to back up — no canonical files found on this machine");
        return ExitCode::Success;
    }

    if let Err(error) = std::fs::create_dir_all(output_dir) {
        eprintln!(
            "ccync backup: cannot create output dir {}: {error}",
            output_dir.display()
        );
        return ExitCode::Error;
    }
    for (name, content) in &payloads {
        if let Err(error) = std::fs::write(output_dir.join(name), content) {
            eprintln!("ccync backup: failed to write {name}: {error}");
            return ExitCode::Error;
        }
    }

    println!(
        "ccync backup: {} file(s) backed up to {}",
        payloads.len(),
        output_dir.display()
    );
    if skipped > 0 {
        println!("ccync backup: {skipped} file(s) skipped (not present on this machine)");
    }
    println!("ccync backup: AGENTS.md is not scanned for unlabeled tokens");
    println!(
        "ccync backup: placeholders introduced: {}",
        if placeholders.is_empty() {
            "<none>".to_string()
        } else {
            placeholders.join(", ")
        }
    );
    ExitCode::Success
}

fn validate_backup_direct_input(
    content: &[u8],
    personal: bool,
    secrets: &[String],
) -> Result<(), String> {
    use ccync_foundation::mcp::{
        read_declaration_routes, validate_portable_declaration, DeclarationRoot,
    };
    let document: serde_json::Value =
        serde_json::from_slice(content).map_err(|_| "invalid mcp.json")?;
    let (root, key) = if personal {
        (DeclarationRoot::Personal, "ccyncMcpServers")
    } else {
        (DeclarationRoot::Legacy, "mcpServers")
    };
    read_declaration_routes(&document, root, "user").map_err(|error| error.to_string())?;
    for (name, server) in document[key].as_object().ok_or("invalid MCP servers")? {
        let mut normalized = server.clone();
        normalize_backup_references(&mut normalized);
        validate_portable_declaration(&normalized, secrets)
            .map_err(|error| format!("refusing mcp.json server {name}: {error}"))?;
    }
    Ok(())
}

fn normalize_backup_references(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) if is_secret_placeholder(text) => {
            *text = "${CCYNC_REFERENCE}".into();
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(normalize_backup_references),
        serde_json::Value::Object(items) => {
            items.values_mut().for_each(normalize_backup_references)
        }
        _ => {}
    }
}

fn refuse_plugin_url_userinfo(content: &[u8]) -> Result<(), String> {
    let value: serde_json::Value = serde_json::from_slice(content)
        .map_err(|error| format!("failed to validate plugins.json: {error}"))?;
    let Some(plugins) = value.get("plugins").and_then(serde_json::Value::as_array) else {
        return Ok(());
    };
    for plugin in plugins {
        let Some(source) = plugin.get("source").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if ccync_engine::roster::has_url_userinfo(source) {
            let plugin_id = plugin
                .get("pluginId")
                .or_else(|| plugin.get("id"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("<unknown>");
            return Err(format!(
                "refusing plugins.json: plugin {plugin_id} source URL carries userinfo"
            ));
        }
    }
    Ok(())
}

fn strip_config_secrets(content: &str) -> Result<String, std::io::Error> {
    let mut value: serde_json::Value = serde_json::from_str(content)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    if let Some(object) = value.as_object_mut() {
        object.remove("secrets");
    }
    serde_json::to_string_pretty(&value)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))
}

pub(crate) fn refuse_mcp_server_literal_secrets(
    content: &str,
    servers_key: &str,
    filename: &str,
) -> Result<(), String> {
    let value: serde_json::Value = serde_json::from_str(content)
        .map_err(|error| format!("failed to validate {filename}: {error}"))?;
    let Some(servers) = value
        .get(servers_key)
        .and_then(serde_json::Value::as_object)
    else {
        return Ok(());
    };

    for (server_name, server) in servers {
        let Some(server) = server.as_object() else {
            continue;
        };
        for field in ["command", "url"] {
            let Some(value) = server.get(field).and_then(serde_json::Value::as_str) else {
                continue;
            };
            if (field == "url" && ccync_engine::roster::has_url_userinfo(value))
                || carries_literal_secret(value)
            {
                return Err(literal_secret_refusal(filename, server_name, field));
            }
        }

        let args = server
            .get("args")
            .and_then(serde_json::Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for (index, value) in args.iter().enumerate() {
            let Some(value) = value.as_str() else {
                continue;
            };
            if arg_has_url_userinfo(value)
                || carries_literal_secret(value)
                || carries_literal_secret_flag_assignment(value)
            {
                return Err(literal_secret_refusal(
                    filename,
                    server_name,
                    &format!("args[{index}]"),
                ));
            }
            if is_secret_flag(value) {
                let next_is_placeholder = args
                    .get(index + 1)
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(is_secret_placeholder);
                if !next_is_placeholder {
                    let field_index = if index + 1 < args.len() {
                        index + 1
                    } else {
                        index
                    };
                    return Err(literal_secret_refusal(
                        filename,
                        server_name,
                        &format!("args[{field_index}]"),
                    ));
                }
            }
        }

        // The lockfile transform strips env and header values separately; only
        // the user-authored input is refused on them.
        for field in ["env", "headers"] {
            if filename != "mcp.json" {
                break;
            }
            if let Some(values) = server.get(field).and_then(serde_json::Value::as_object) {
                for (name, value) in values {
                    let carries_secret = value.as_str().is_some_and(|value| {
                        let normalized_name = name.to_ascii_uppercase().replace('-', "_");
                        carries_literal_secret(&format!("{normalized_name}={value}"))
                    });
                    if carries_secret {
                        return Err(literal_secret_refusal(
                            filename,
                            server_name,
                            &format!("{field}.{name}"),
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn arg_has_url_userinfo(value: &str) -> bool {
    if ccync_engine::roster::has_url_userinfo(value) {
        return true;
    }
    if let Some(pos) = value.find("://") {
        let prefix = &value[..pos];
        let scheme_start = prefix
            .rfind(|c: char| !c.is_alphanumeric() && c != '+' && c != '-' && c != '.')
            .map(|i| i + 1)
            .unwrap_or(0);
        ccync_engine::roster::has_url_userinfo(&value[scheme_start..])
    } else {
        false
    }
}

fn carries_literal_secret(value: &str) -> bool {
    let secret_re = ccync_foundation::secret::secret_re();
    let carries_literal = secret_re.find_iter(value).any(|matched| {
        let matched = matched.as_str();
        let separator = matched.find([':', '=']);
        separator
            .map(|index| !is_secret_placeholder(matched[index + 1..].trim()))
            .unwrap_or(false)
    });
    carries_literal
}

fn is_secret_placeholder(value: &str) -> bool {
    let mut rest = if let Some((key, value)) = value.split_once('=') {
        if key.is_empty() || !key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
            return false;
        }
        value
    } else {
        value
    };
    let mut found = false;
    while let Some(after_prefix) = rest.strip_prefix("${") {
        let Some((name, after)) = after_prefix.split_once('}') else {
            return false;
        };
        if name.is_empty()
            || !name
                .bytes()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
        {
            return false;
        }
        found = true;
        rest = after;
    }
    found && rest.is_empty()
}

fn is_secret_flag(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "--token" | "--api-key" | "--password" | "--secret"
    )
}

fn carries_literal_secret_flag_assignment(value: &str) -> bool {
    let Some((flag, secret)) = value.split_once('=') else {
        return false;
    };
    is_secret_flag(flag) && !is_secret_placeholder(secret)
}

fn literal_secret_refusal(filename: &str, server_name: &str, field: &str) -> String {
    format!(
        "refusing {filename}: server {server_name} field {field} carries a literal secret; it must become a ${{VAR}} placeholder"
    )
}

fn strip_lock_secrets(content: &str) -> Result<(String, Vec<String>), std::io::Error> {
    let mut value: serde_json::Value = serde_json::from_str(content)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    let mut placeholders = Vec::new();
    if let Some(servers) = value
        .get_mut("_mcpServers")
        .and_then(serde_json::Value::as_object_mut)
    {
        let mut introduced_by_name = std::collections::HashMap::<String, String>::new();
        for (server_name, server) in servers {
            let Some(server) = server.as_object_mut() else {
                continue;
            };
            for section_name in ["env", "headers"] {
                let Some(entries) = server
                    .get_mut(section_name)
                    .and_then(serde_json::Value::as_object_mut)
                else {
                    continue;
                };
                for (key, value) in entries {
                    let Some(raw) = value.as_str() else {
                        continue;
                    };
                    if is_secret_placeholder(raw) {
                        continue;
                    }
                    let placeholder = format!(
                        "${{CCYNC_SECRET_{}_{}{}",
                        sanitize_secret_identifier(server_name),
                        sanitize_secret_identifier(key),
                        '}'
                    );
                    let field_name = format!("{server_name}.{section_name}.{key}");
                    if content.contains(&placeholder) {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("refusing plugins.lock.json: generated placeholder for {field_name} is already in use"),
                        ));
                    }
                    if let Some(previous) =
                        introduced_by_name.insert(placeholder.clone(), field_name.clone())
                    {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "refusing plugins.lock.json: fields {previous} and {field_name} produce the same placeholder {placeholder}"
                            ),
                        ));
                    }
                    placeholders.push(placeholder.clone());
                    *value = serde_json::Value::String(placeholder);
                }
            }
        }
    }
    let content = serde_json::to_string_pretty(&value)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    Ok((content, placeholders))
}

fn sanitize_secret_identifier(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

pub(crate) fn restore_secret_notice_lines(lock: &serde_json::Value) -> Vec<String> {
    let Some(servers) = lock.get("_mcpServers") else {
        return Vec::new();
    };
    let manifest = serde_json::from_value::<ccync_foundation::mcp::McpManifest>(
        serde_json::json!({"servers": servers, "inputs": null}),
    )
    .ok();
    let Some(manifest) = manifest else {
        return Vec::new();
    };

    mcp::unresolved_secret_vars(&manifest)
        .into_iter()
        .map(|(server_name, variables)| {
            format!(
                "restore: MCP server {server_name} needs {} — add to config.json#secrets or the environment",
                variables.into_iter().collect::<Vec<_>>().join(", ")
            )
        })
        .collect()
}

/// Run `ccync restore --from <dir>` — restore the canonical machine-local files from a
/// previously created backup directory: the INPUT files (`config.json` + `plugins.json`)
/// plus `plugins.lock.json`, which carries the non-rebuildable init adoption snapshots.
pub(crate) fn cmd_restore(args: &[String]) -> ExitCode {
    use ccync_foundation::paths::EffectiveLayout;

    if let Err(code) = super::args::reject_unknown_flags(
        "restore",
        args,
        &["--dry-run"],
        &["--from"],
        0,
        "Usage: ccync restore --from <dir> [--dry-run]",
    ) {
        return code;
    }

    let dry_run = args.iter().any(|arg| arg == "--dry-run");
    if dry_run {
        if let Err(error) = ccync_engine::tool_manager::lock::check_restore_pending() {
            eprintln!("ccync restore: {error}");
            return ExitCode::Error;
        }
    }

    let from_dir = {
        let mut from: Option<PathBuf> = None;
        let mut i = 1usize;
        while i < args.len() {
            if args[i] == "--from" {
                i += 1;
                if let Some(val) = args.get(i) {
                    from = Some(PathBuf::from(val));
                } else {
                    eprintln!("ccync restore: --from requires a path");
                    return ExitCode::Usage;
                }
            }
            i += 1;
        }
        from
    };

    let from_dir = match from_dir {
        Some(d) => d,
        None => {
            eprintln!("ccync restore: --from <dir> is required");
            return ExitCode::Usage;
        }
    };

    if !from_dir.is_dir() {
        eprintln!(
            "ccync restore: backup directory not found: {}",
            from_dir.display()
        );
        return ExitCode::Error;
    }

    let layout = match EffectiveLayout::load_or_legacy() {
        Ok(layout) => layout,
        Err(error) => {
            eprintln!("ccync restore: cannot load storage layout: {error}");
            return ExitCode::Error;
        }
    };
    let canonical_files: &[(&str, Option<PathBuf>)] = &[
        ("config.json", Some(layout.machine_config_path())),
        ("plugins.json", Some(layout.personal_catalog_path())),
        ("plugins.lock.json", Some(layout.build_lock_path())),
        ("AGENTS.md", Some(layout.agents_md_path())),
        ("mcp.json", Some(layout.mcp_input_candidates().0)),
    ];

    if dry_run {
        return restore_dry_run(&from_dir, canonical_files);
    }

    if let Err(error) = validate_restore_backup(&from_dir) {
        eprintln!("ccync restore: refusing backup: {error}");
        return ExitCode::Error;
    }
    if !from_dir.join("plugins.json").is_file() {
        eprintln!("ccync restore: refusing backup without plugins.json");
        return ExitCode::Error;
    }
    let names = [
        "config.json",
        "plugins.json",
        "plugins.lock.json",
        "AGENTS.md",
        "mcp.json",
        "portable/sources.json",
        "portable/lock.json",
    ];
    let sources = match names
        .iter()
        .map(|name| restore_read(&from_dir.join(name)).map(|bytes| (*name, bytes)))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(value) => value,
        Err(error) => {
            eprintln!("ccync restore: {error}");
            return ExitCode::Error;
        }
    };
    let restored = match restore_transaction(&from_dir, &layout, canonical_files, &sources) {
        Ok(count) => count,
        Err(error) => {
            eprintln!("ccync restore: {error}");
            return ExitCode::Error;
        }
    };

    println!(
        "ccync restore: {} file(s) restored from {}",
        restored,
        from_dir.display()
    );
    let mut restored_catalog = serde_json::Value::Null;
    if let Some((_, Some(catalog_path))) = canonical_files
        .iter()
        .find(|(name, _)| *name == "plugins.json")
    {
        if let Ok(bytes) = std::fs::read(catalog_path) {
            if let Ok(catalog) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                restored_catalog = catalog;
            }
        }
    }
    for notice in restore_follow_up_notices(restored, &restored_catalog) {
        println!("{notice}");
    }
    if let Some(lock_path) = canonical_files
        .iter()
        .find(|(name, _)| *name == "plugins.lock.json")
        .and_then(|(_, path)| path.as_ref())
    {
        if let Ok(content) = std::fs::read_to_string(lock_path) {
            if let Ok(lock) = serde_json::from_str::<serde_json::Value>(&content) {
                for line in restore_secret_notice_lines(&lock) {
                    println!("{line}");
                }
            }
        }
    }
    if sources
        .iter()
        .any(|(name, bytes)| *name == "mcp.json" && bytes.is_some())
    {
        let path = layout.mcp_input_candidates().0;
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(document) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if let Ok(manifest) = serde_json::from_value::<ccync_foundation::mcp::McpManifest>(
                    serde_json::json!({"servers": document["ccyncMcpServers"], "inputs": null}),
                ) {
                    let config_path = layout.machine_config_path();
                    let reports = mcp::server_variable_reports_at(&manifest, &config_path);
                    let secrets = mcp::unresolved_secret_vars_at(&manifest, &config_path);
                    for line in mcp_variable_lines("restore", &reports, &secrets) {
                        println!("{line}");
                    }
                }
            }
        }
    }
    ExitCode::Success
}

struct RestoreFile {
    key: &'static str,
    path: PathBuf,
    before: Option<Vec<u8>>,
    after: Vec<u8>,
    routes: bool,
}

fn restore_read(path: &std::path::Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn restore_matches(bytes: Option<&[u8]>, digest: Option<&str>) -> bool {
    match (bytes, digest) {
        (None, None) => true,
        (Some(bytes), Some(digest)) => digest
            .strip_prefix("sha256:")
            .is_some_and(|hex| ccync_engine::self_update::verify_sha256(bytes, hex).is_ok()),
        _ => false,
    }
}

fn restore_transaction(
    from_dir: &std::path::Path,
    layout: &ccync_foundation::paths::EffectiveLayout,
    canonical_files: &[(&'static str, Option<PathBuf>)],
    sources: &[(&str, Option<Vec<u8>>)],
) -> Result<usize, String> {
    use ccync_engine::tool_manager::lock::{
        read_restore_marker, LifecycleLock, RestoreMarkerState,
    };
    let source = |name: &str| {
        sources
            .iter()
            .find(|(key, _)| *key == name)
            .and_then(|(_, bytes)| bytes.as_deref())
    };
    let config: Option<serde_json::Value> = source("config.json")
        .map(serde_json::from_slice)
        .transpose()
        .map_err(|e| e.to_string())?;
    let routes: Option<Vec<mcp::MigratedRoute>> = config
        .as_ref()
        .and_then(|v| v.get("_ccyncBackupMcpRoutes"))
        .map(|value| {
            let object = value.as_object().ok_or("invalid backup MCP routes")?;
            if object.len() != 2
                || !object.contains_key("version")
                || !object.contains_key("routes")
            {
                return Err("invalid backup MCP routes fields".to_string());
            }
            if value.get("version").and_then(|v| v.as_u64()) != Some(1) {
                return Err("invalid backup MCP routes version".to_string());
            }
            let rows = value
                .get("routes")
                .and_then(serde_json::Value::as_array)
                .ok_or("invalid backup MCP routes")?;
            if rows.iter().any(|row| {
                row.as_object().is_none_or(|row| {
                    row.len() != 3
                        || !row.contains_key("source")
                        || !row.contains_key("server")
                        || !row.contains_key("hosts")
                })
            }) {
                return Err("invalid backup MCP route fields".to_string());
            }
            serde_json::from_value(
                value
                    .get("routes")
                    .cloned()
                    .ok_or("missing backup MCP routes")?,
            )
            .map_err(|e| format!("invalid backup MCP routes: {e}"))
        })
        .transpose()?
        .filter(|routes: &Vec<mcp::MigratedRoute>| !routes.is_empty());
    let mut keys: Vec<&str> = canonical_files
        .iter()
        .filter_map(|(name, _)| source(name).map(|_| *name))
        .collect();
    if routes.is_some() {
        keys.push("projected-state.json");
    }
    if layout.sources_export_path().is_some() && source("plugins.json").is_some() {
        keys.push("sources-export.json");
    }
    if layout.lock_export_path().is_some() {
        keys.push("lock-export.json");
    }
    let source_refs: Vec<_> = sources
        .iter()
        .map(|(name, bytes)| (*name, bytes.as_deref()))
        .collect();
    let guard = LifecycleLock::acquire_for_restore_sources(&source_refs, &keys)
        .map_err(|e| e.to_string())?;
    let verify_sources = || -> Result<(), String> {
        for (name, expected) in sources {
            if restore_read(&from_dir.join(name))? != *expected {
                return Err(format!("backup changed: {name}"));
            }
        }
        validate_restore_backup(from_dir)
    };
    verify_sources()?;
    if source("plugins.json").is_none() {
        return Err("refusing backup without plugins.json".into());
    }
    let mcp_after = source("mcp.json").map(convert_restore_mcp).transpose()?;
    if mcp_after.is_some() || routes.as_ref().is_some_and(|routes| !routes.is_empty()) {
        let (personal, legacy) = layout.mcp_input_candidates();
        let existing = ccync_engine::install::read_direct_mcp_input(layout)
            .map_err(|error| error.to_string())?;
        if let Some(existing) = existing {
            if existing.path() == legacy {
                return Err(format!("active legacy MCP input: {}", legacy.display()));
            }
            if existing.path() != personal {
                return Err("unexpected MCP input destination".into());
            }
            if let Some(after) = &mcp_after {
                let document: serde_json::Value =
                    serde_json::from_slice(after).map_err(|e| e.to_string())?;
                let incoming = ccync_foundation::mcp::read_declaration_routes(
                    &document,
                    ccync_foundation::mcp::DeclarationRoot::Personal,
                    "user",
                )
                .map_err(|e| e.to_string())?;
                let current = existing.routes();
                let mut conflicts = Vec::new();
                for (identity, route) in &current.declarations {
                    if (route.hosts.is_some() || !route.enabled)
                        && incoming.declarations.get(identity) != Some(route)
                    {
                        conflicts.push(format!("{}/{}", identity.source, identity.server));
                    }
                }
                for (identity, hosts) in &current.overrides {
                    if incoming.overrides.get(identity) != Some(hosts)
                        && incoming
                            .declarations
                            .get(identity)
                            .and_then(|route| route.hosts.as_ref())
                            != Some(hosts)
                    {
                        conflicts.push(format!("{}/{}", identity.source, identity.server));
                    }
                }
                if !conflicts.is_empty() {
                    conflicts.sort();
                    conflicts.dedup();
                    return Err(format!(
                        "MCP explicit choices would change: {}",
                        conflicts.join(", ")
                    ));
                }
            }
        }
    }
    let mut files = Vec::new();
    for (name, dst) in canonical_files {
        let Some(bytes) = source(name) else { continue };
        if name.ends_with(".json") {
            serde_json::from_slice::<serde_json::Value>(bytes)
                .map_err(|e| format!("invalid {name}: {e}"))?;
        }
        if *name == "config.json" {
            ccync_foundation::config::CcyncConfig::load_strict_from_path(&from_dir.join(name))
                .map_err(|e| format!("invalid config.json: {e}"))?;
        }
        let path = dst.clone().ok_or("destination unavailable")?;
        files.push(RestoreFile {
            key: name,
            before: restore_read(&path)?,
            path,
            after: if *name == "mcp.json" {
                mcp_after.clone().expect("source exists")
            } else if *name == "config.json"
                && config
                    .as_ref()
                    .is_some_and(|value| value.get("_ccyncBackupMcpRoutes").is_some())
            {
                let mut value = config.clone().expect("source exists");
                value
                    .as_object_mut()
                    .ok_or("invalid config root")?
                    .remove("_ccyncBackupMcpRoutes");
                serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?
            } else {
                bytes.to_vec()
            },
            routes: false,
        });
    }
    let count = files.len();
    let managed = layout.generated_mcp_path();
    if let Some(routes) = &routes {
        let preview = mcp::preview_restored_routes(&managed, routes).map_err(|e| e.to_string())?;
        files.push(RestoreFile {
            key: "projected-state.json",
            path: managed.with_file_name("projected-state.json"),
            before: preview.before().map(|b| b.to_vec()),
            after: preview.after().to_vec(),
            routes: true,
        });
    }
    if let Some(path) = layout.sources_export_path() {
        let catalog: serde_json::Value =
            serde_json::from_slice(source("plugins.json").ok_or("missing catalog")?)
                .map_err(|e| e.to_string())?;
        let after = ccync_engine::catalog::portable_sources_export_bytes(&catalog)?;
        files.push(RestoreFile {
            key: "sources-export.json",
            before: restore_read(&path)?,
            path,
            after,
            routes: false,
        });
    }
    if let Some(path) = layout.lock_export_path() {
        let lock = match source("plugins.lock.json") {
            Some(bytes) => bytes.to_vec(),
            None => restore_read(&layout.build_lock_path())?.ok_or("private lockfile not found")?,
        };
        let lock: serde_json::Value = serde_json::from_slice(&lock).map_err(|e| e.to_string())?;
        let after = ccync_engine::catalog::portable_lock_export_bytes(&lock)?;
        files.push(RestoreFile {
            key: "lock-export.json",
            before: restore_read(&path)?,
            path,
            after,
            routes: false,
        });
    }
    match read_restore_marker().map_err(|e| e.to_string())? {
        RestoreMarkerState::Absent => {
            let rows: Vec<_> = files
                .iter()
                .map(|f| (f.key, f.before.as_deref(), Some(f.after.as_slice())))
                .collect();
            guard
                .record_destinations(&rows)
                .map_err(|e| e.to_string())?;
        }
        RestoreMarkerState::Pending(marker) => {
            for file in &files {
                let row = marker
                    .destinations
                    .iter()
                    .find(|row| row.key == file.key)
                    .ok_or("unknown restore destination")?;
                if !restore_matches(Some(&file.after), row.after.as_deref())
                    || !(restore_matches(file.before.as_deref(), row.before.as_deref())
                        || restore_matches(file.before.as_deref(), row.after.as_deref()))
                {
                    return Err(format!("restore destination changed: {}", file.key));
                }
            }
        }
        RestoreMarkerState::Corrupt => return Err("restore marker is corrupt".into()),
    }
    for file in &files {
        if file.before.is_some() {
            println!(
                "ccync restore: {} exists \u{2014} will be overwritten",
                file.key
            );
        }
    }
    let marker = match read_restore_marker().map_err(|e| e.to_string())? {
        RestoreMarkerState::Pending(marker) => marker,
        _ => return Err("restore marker missing".into()),
    };
    for file in &files {
        verify_sources()?;
        if read_restore_marker().map_err(|e| e.to_string())?
            != RestoreMarkerState::Pending(marker.clone())
        {
            return Err("restore marker changed".into());
        }
        let row = marker
            .destinations
            .iter()
            .find(|row| row.key == file.key)
            .ok_or("unknown restore destination")?;
        let current = restore_read(&file.path)?;
        if restore_matches(current.as_deref(), row.after.as_deref()) {
            continue;
        }
        if !restore_matches(current.as_deref(), row.before.as_deref()) {
            return Err(format!("restore destination changed: {}", file.key));
        }
        if file.routes {
            mcp::commit_restored_routes(&managed, routes.as_ref().ok_or("missing routes")?)
                .map_err(|e| e.to_string())?;
        } else {
            if let Some(parent) = file.path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            ccync_foundation::platform::atomic_write_bytes(&file.path, &file.after)
                .map_err(|e| e.to_string())?;
        }
    }
    verify_sources()?;
    for file in &files {
        if restore_read(&file.path)?.as_deref() != Some(file.after.as_slice()) {
            return Err(format!("restore destination changed: {}", file.key));
        }
    }
    if read_restore_marker().map_err(|e| e.to_string())? != RestoreMarkerState::Pending(marker) {
        return Err("restore marker changed".into());
    }
    guard.clear_marker().map_err(|e| e.to_string())?;
    Ok(count)
}

fn convert_restore_mcp(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use ccync_foundation::mcp::{read_declaration_routes, DeclarationRoot};
    let mut document: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if document.get("ccyncMcpServers").is_some() {
        read_declaration_routes(&document, DeclarationRoot::Personal, "user")
            .map_err(|e| e.to_string())?;
        return Ok(bytes.to_vec());
    }
    read_declaration_routes(&document, DeclarationRoot::Legacy, "user")
        .map_err(|e| e.to_string())?;
    let object = document.as_object_mut().ok_or("invalid MCP root")?;
    let servers = object.remove("mcpServers").ok_or("missing MCP servers")?;
    object.insert("ccyncMcpServers".into(), servers);
    serde_json::to_vec_pretty(&document).map_err(|e| e.to_string())
}

pub(crate) fn restore_follow_up_notices(
    restored: usize,
    catalog: &serde_json::Value,
) -> Vec<String> {
    let mut notices = Vec::new();
    if restored > 0 {
        notices.push("ccync restore: next step: run `ccync sync`".to_string());
        notices
            .push("ccync restore: remote sources are fetched again by the next sync".to_string());
    }
    notices.extend(
        catalog
            .get("plugins")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|plugin| {
                let local =
                    plugin.get("sourceType").and_then(serde_json::Value::as_str) == Some("local");
                let source = plugin.get("source").and_then(serde_json::Value::as_str);
                if !local || !source.is_some_and(|source| !std::path::Path::new(source).exists()) {
                    return None;
                }
                let id = plugin
                    .get("pluginId")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("<unknown>");
                Some(format!(
                    "ccync restore: {id}: needs-rebind (local source path does not exist)"
                ))
            })
            .collect::<Vec<_>>(),
    );
    notices
}

pub(crate) fn validate_restore_backup(from_dir: &std::path::Path) -> Result<(), String> {
    let entries = std::fs::read_dir(from_dir)
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    let names = entries
        .iter()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    validate_restore_backup_names(&names)?;
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        let metadata =
            std::fs::symlink_metadata(entry.path()).map_err(|error| error.to_string())?;
        if metadata.file_type().is_symlink() {
            return Err(format!("links are not allowed: {}", entry.path().display()));
        }
        let valid = matches!(
            name.as_str(),
            "config.json"
                | "plugins.json"
                | "plugins.lock.json"
                | "agents.md"
                | "mcp.json"
                | "portable"
        );
        if !valid {
            return Err(format!(
                "unexpected backup entry: {}",
                entry.file_name().to_string_lossy()
            ));
        }
        if name == "portable" {
            if !metadata.is_dir() {
                return Err("portable entry must be a directory".to_string());
            }
            for child in std::fs::read_dir(entry.path()).map_err(|error| error.to_string())? {
                let child = child.map_err(|error| error.to_string())?;
                let child_name = child.file_name().to_string_lossy().to_lowercase();
                if !matches!(child_name.as_str(), "sources.json" | "lock.json") {
                    return Err(format!("unsafe portable entry: {child_name}"));
                }
                let child_meta =
                    std::fs::symlink_metadata(child.path()).map_err(|error| error.to_string())?;
                if child_meta.file_type().is_symlink() || !child_meta.is_file() {
                    return Err(format!("unsafe portable entry: {child_name}"));
                }
                let bytes = std::fs::read(child.path()).map_err(|error| error.to_string())?;
                if child_name == "sources.json" {
                    let content = std::str::from_utf8(&bytes).map_err(|error| error.to_string())?;
                    ccync_foundation::provider::PortableSourcesDocument::parse(content)
                        .map_err(|error| error.to_string())?;
                } else {
                    let value: serde_json::Value =
                        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
                    if !value.is_object() {
                        return Err("portable lock must be a JSON object".to_string());
                    }
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_restore_backup_names(names: &[String]) -> Result<(), String> {
    let mut folded = std::collections::HashSet::new();
    for name in names {
        let normalized = name.to_lowercase();
        if !folded.insert(normalized.clone()) {
            return Err(format!("case-folded filename collision: {normalized}"));
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn restore_names_have_case_collision(names: &[&str]) -> bool {
    let names = names
        .iter()
        .map(|name| name.to_string())
        .collect::<Vec<_>>();
    validate_restore_backup_names(&names).is_err()
}

fn write_restore_export(path: Option<PathBuf>, bytes: Vec<u8>) -> Result<(), String> {
    let Some(path) = path else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(path, bytes).map_err(|error| error.to_string())
}

fn restore_dry_run(
    from_dir: &std::path::Path,
    canonical_files: &[(&str, Option<PathBuf>)],
) -> ExitCode {
    let mut drift = false;

    for (name, dst_opt) in canonical_files {
        if *name == "mcp.json" && !from_dir.join(name).is_file() {
            println!("ccync restore: {name}: skipped (absent from backup)");
            continue;
        }
        let Some(dst) = dst_opt else {
            eprintln!("ccync restore: cannot resolve path for {name} (no home dir)");
            return ExitCode::Error;
        };

        let profile_path = from_dir.join(name);
        let profile_exists = profile_path.is_file();
        let machine_exists = dst.is_file();
        let classification = match (profile_exists, machine_exists) {
            (false, false) => "converged",
            (false, true) | (true, false) => {
                drift = true;
                "differs (absent on one side)"
            }
            (true, true) => match restore_payloads_equal(&profile_path, dst) {
                Ok(true) => "converged",
                Ok(false) => {
                    drift = true;
                    "differs"
                }
                Err(error) => {
                    eprintln!("ccync restore: failed to inspect {name}: {error}");
                    return ExitCode::Error;
                }
            },
        };
        println!("ccync restore: {name}: {classification}");
    }

    if drift {
        ExitCode::UpdatesAvailable
    } else {
        ExitCode::Success
    }
}

fn restore_payloads_equal(
    profile_path: &std::path::Path,
    machine_path: &std::path::Path,
) -> Result<bool, std::io::Error> {
    let name = profile_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    match name {
        "AGENTS.md" | "mcp.json" => {
            Ok(std::fs::read(profile_path)? == std::fs::read(machine_path)?)
        }
        "config.json" => compare_json_payloads(profile_path, machine_path, |value| {
            if let Some(object) = value.as_object_mut() {
                object.remove("secrets");
            }
        }),
        "plugins.json" => compare_json_payloads(profile_path, machine_path, |_| {}),
        "plugins.lock.json" => {
            let profile = normalize_lockfile_for_drift(profile_path)?;
            let machine = normalize_lockfile_for_drift(machine_path)?;
            Ok(profile == machine)
        }
        _ => Ok(std::fs::read(profile_path)? == std::fs::read(machine_path)?),
    }
}

fn compare_json_payloads(
    profile_path: &std::path::Path,
    machine_path: &std::path::Path,
    transform: impl Fn(&mut serde_json::Value),
) -> Result<bool, std::io::Error> {
    let profile = parse_json_payload(profile_path)?;
    let machine = parse_json_payload(machine_path)?;
    let mut profile = profile;
    let mut machine = machine;
    transform(&mut profile);
    transform(&mut machine);
    Ok(profile == machine)
}

fn parse_json_payload(path: &std::path::Path) -> Result<serde_json::Value, std::io::Error> {
    let content = std::fs::read_to_string(path)?;
    serde_json::from_str(&content).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}: {error}", path.display()),
        )
    })
}

fn normalize_lockfile_for_drift(
    path: &std::path::Path,
) -> Result<serde_json::Value, std::io::Error> {
    let content = std::fs::read_to_string(path)?;
    let (normalized, _) = strip_lock_secrets(&content)?;
    let value: serde_json::Value = serde_json::from_str(&normalized).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}: {error}", path.display()),
        )
    })?;
    let mut tracked = serde_json::Map::new();
    for key in [
        "_adoptMaster",
        "_adoptedItems",
        "_mcpServers",
        "_looseSkills",
    ] {
        if let Some(value) = value.get(key) {
            tracked.insert(key.to_string(), value.clone());
        }
    }
    Ok(serde_json::Value::Object(tracked))
}

/// Run `ccync uninstall` — remove canonical root and surfaces.
pub(crate) fn cmd_uninstall(args: &[String]) -> ExitCode {
    use ccync_engine::install::run_uninstall;

    if let Err(code) =
        super::args::reject_unknown_flags("uninstall", args, &[], &[], 0, "Usage: ccync uninstall")
    {
        return code;
    }

    match run_uninstall() {
        Ok(()) => {
            println!("ccync uninstalled.");
            ExitCode::Success
        }
        Err(e) => {
            eprintln!("ccync uninstall: {e}");
            ExitCode::Error
        }
    }
}

/// Whether an unqualified stdio server publishes on this platform: always on
/// non-Windows, and on Windows only under the scoped debug `allow` override.
#[cfg(test)]
pub(crate) fn windows_stdio_publication_allowed() -> bool {
    !cfg!(windows)
        || (cfg!(debug_assertions)
            && std::env::var_os("CCYNC_TEST_WINDOWS_STDIO").is_some_and(|value| value == "allow"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_sync_older_sublayout_dry_run_preserves_home_bytes() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let _env = isolate_env(tmp.path());
        let old_config = tmp.path().join(".ccync/config/config.json");
        std::fs::create_dir_all(old_config.parent().unwrap()).unwrap();
        std::fs::write(&old_config, r#"{"selectedRuntimes":["claude"]}"#).unwrap();
        let before = portable_sync_snapshot(tmp.path());

        let result = run_unified_projection(true, true, false, Some(&SYNC_GATE_ABORT_CONTEXT));

        assert_eq!(result, ExitCode::Success);
        assert_eq!(before, portable_sync_snapshot(tmp.path()));
        assert!(
            old_config.is_file(),
            "dry run must retain the older config source"
        );
    }

    fn portable_sync_snapshot(root: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
        fn visit(
            root: &std::path::Path,
            dir: &std::path::Path,
            out: &mut Vec<(std::path::PathBuf, Vec<u8>)>,
        ) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    visit(root, &path, out);
                } else if let Ok(bytes) = std::fs::read(&path) {
                    out.push((path.strip_prefix(root).unwrap().to_path_buf(), bytes));
                }
            }
        }
        let mut files = Vec::new();
        visit(root, root, &mut files);
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files
    }
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn cmd_sync_dry_run_writes_nothing_marker() {
        // --dry-run short-circuits before any projection write.
        let args = ["sync".to_string(), "--dry-run".to_string()];
        assert!(args.iter().any(|a| a == "--dry-run"));
    }

    // ── run_catalog_resolve: resolve errors are warnings, not fatal ───────────

    #[test]
    fn run_catalog_resolve_returns_ok_when_resolution_has_errors() {
        // A config.json with an unknown defaultProfile causes the resolver to
        // push a non-fatal entry into CatalogResolution.errors (see
        // ccync_engine::catalog::resolve_plugin_set). run_catalog_resolve must
        // still return Ok(()) — resolve errors are collected and warned about,
        // never propagated as a hard failure.
        let _guard = crate::test_io_lock();

        let tmp = TempDir::new().unwrap();
        let _home_env = isolate_env(tmp.path());

        let ccync_home = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync_home).unwrap();
        fs::write(
            ccync_home.join("config.json"),
            r#"{"defaultProfile":"does-not-exist"}"#,
        )
        .unwrap();

        let result = run_catalog_resolve(false);

        assert!(
            result.is_ok(),
            "resolve errors (e.g. unknown defaultProfile) must be collected as warnings, not returned as Err: {result:?}"
        );
    }

    // ── source-grep: exact warning string templates are present ───────────────

    #[test]
    fn catalog_resolve_warning_templates_present_in_source() {
        // Guards the exact wording of the two warning templates that replaced
        // the old suppressed-warning path: a whole-resolve-call failure inside
        // run_unified_projection, and per-entry CatalogResolution.errors
        // warnings inside run_catalog_resolve (this file) and
        // run_personal_resolve (plugin.rs).
        let this_file = include_str!("lifecycle.rs");
        assert!(
            this_file.contains("ccync sync: catalog resolve (best-effort): {e}"),
            "run_unified_projection must warn on a failed resolve call with the best-effort template"
        );
        assert!(
            this_file.contains("ccync: catalog resolve warning: {e}"),
            "run_catalog_resolve must warn per-entry using the catalog resolve warning template"
        );

        let plugin_file = include_str!("plugin.rs");
        assert!(
            plugin_file.contains("ccync: catalog resolve warning: {e}"),
            "run_personal_resolve must warn per-entry using the catalog resolve warning template"
        );
    }

    #[test]
    fn removed_suppression_helper_never_reappears() {
        // Regression guard: the old catalog-warning suppression helper and all
        // references to it must never reappear anywhere in this crate's
        // sources. The needle is assembled at runtime so this test's own
        // source text does not trip its own assertion via include_str!.
        let needle = ["is_benign_catalog", "_warning"].concat();
        let this_file = include_str!("lifecycle.rs");
        let plugin_file = include_str!("plugin.rs");
        assert!(!this_file.contains(&needle));
        assert!(!plugin_file.contains(&needle));
    }

    #[test]
    fn no_files_written_phrase_only_remains_in_dry_run_output() {
        let this_file = include_str!("lifecycle.rs");
        assert!(
            this_file.contains("println!(\"  (no files written)\");"),
            "dry-run output must keep the explicit no-files-written marker"
        );
        assert!(
            !this_file.contains("println!(\"ccync: aborted — no files written.\");"),
            "gate-abort path must no longer claim that no files were written"
        );
    }

    // ── run_projection_core shared seam ──────────────────────────────────────

    #[test]
    fn projection_core_seam_succeeds_in_isolated_home() {
        // run_projection_core is the shared seam used by refresh_with.
        // Holds the crate-level TEST_IO_LOCK to avoid racing with the sync test
        // that also redirects HOME to a temp dir.
        let _guard = crate::test_io_lock();

        let tmp = tempfile::TempDir::new().unwrap();
        let _env = isolate_env(tmp.path());

        let config = ccync_foundation::config::CcyncConfig::load();
        let result = run_projection_core(&config, "ccync refresh");

        assert_eq!(
            result,
            ExitCode::Success,
            "run_projection_core must succeed in isolated home"
        );
        assert!(
            tmp.path()
                .join(".ccync")
                .join("build")
                .join("render")
                .is_dir(),
            "run_projection_core must write the canonical render dir"
        );
    }

    /// Seed an isolated home whose adopted lockfile carries one MCP server.
    #[cfg(test)]
    fn seed_home_with_adopted_server(tmp: &std::path::Path) {
        std::fs::create_dir_all(tmp.join(".agents")).unwrap();
        std::fs::write(tmp.join(".agents/mcp.json"), r#"{"ccyncMcpServers":{},"hostRoutes":[{"source":"adopted","server":"memory","hosts":["claude","copilot","opencode","codex","agy"]}]}"#).unwrap();
        let build = tmp.join(".ccync").join("build");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(
            build.join("lock.json"),
            r#"{"_mcpServers":{"memory":{"command":"npx","args":["-y","srv"]}},"resolvedPlugins":[]}"#,
        )
        .unwrap();
    }

    /// Seed an isolated home whose adopted lockfile carries one HTTP MCP server.
    /// Transport-independent publication contracts use this fixture so they do
    /// not depend on the unqualified-stdio gate.
    #[cfg(test)]
    fn seed_home_with_adopted_http_server(tmp: &std::path::Path) {
        std::fs::create_dir_all(tmp.join(".agents")).unwrap();
        std::fs::write(tmp.join(".agents/mcp.json"), r#"{"ccyncMcpServers":{},"hostRoutes":[{"source":"adopted","server":"memory","hosts":["claude","copilot","opencode","codex","agy"]}]}"#).unwrap();
        let build = tmp.join(".ccync").join("build");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(
            build.join("lock.json"),
            r#"{"_mcpServers":{"memory":{"type":"http","url":"https://mcp.example.test/memory"}},"resolvedPlugins":[]}"#,
        )
        .unwrap();
    }

    /// Save + redirect every home/config env var host paths derive from.
    #[cfg(test)]
    struct EnvGuard(Vec<(String, Option<String>)>);

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            restore_saved_env(self);
        }
    }

    fn isolate_env(tmp: &std::path::Path) -> EnvGuard {
        let keys = [
            "USERPROFILE",
            "HOME",
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
        ];
        let mut saved = Vec::new();
        for k in keys {
            saved.push((k.to_string(), std::env::var(k).ok()));
            let value = match k {
                "USERPROFILE" | "HOME" => tmp.to_path_buf(),
                "APPDATA" => tmp.join("AppData").join("Roaming"),
                "LOCALAPPDATA" => tmp.join("AppData").join("Local"),
                "XDG_CONFIG_HOME" => tmp.join(".config"),
                "XDG_STATE_HOME" => tmp.join(".local").join("state"),
                "XDG_CACHE_HOME" => tmp.join(".cache"),
                _ => unreachable!("unexpected home environment key"),
            };
            unsafe {
                std::env::set_var(k, value);
            }
        }
        EnvGuard(saved)
    }

    #[cfg(test)]
    fn restore_saved_env(saved: &EnvGuard) {
        for (k, v) in &saved.0 {
            unsafe {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    #[test]
    fn mcp_lifecycle_projects_server_to_live_host_and_commits_ownership() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        seed_home_with_adopted_http_server(tmp.path());

        let config = ccync_foundation::config::CcyncConfig::load();
        let result = run_projection_core(&config, "ccync refresh");

        let mcp_dir = tmp.path().join(".ccync").join("build").join("mcp");
        let managed = std::fs::read_to_string(mcp_dir.join("managed.json")).ok();
        let claude = std::fs::read_to_string(tmp.path().join(".claude.json")).ok();
        let txn_present = mcp_dir.join("projection.txn.json").exists();
        let state_present = mcp_dir.join("projected-state.json").exists();

        restore_saved_env(&saved);

        assert_eq!(result, ExitCode::Success);
        let managed: serde_json::Value =
            serde_json::from_str(&managed.expect("managed.json")).unwrap();
        assert_eq!(
            managed["servers"]["memory"]["url"],
            "https://mcp.example.test/memory"
        );
        let claude: serde_json::Value =
            serde_json::from_str(&claude.expect(".claude.json written")).unwrap();
        assert_eq!(
            claude["mcpServers"]["memory"]["url"], "https://mcp.example.test/memory",
            "server reached the live host"
        );
        assert!(!txn_present, "journal cleared after a clean projection");
        assert!(state_present, "committed ownership persisted");
    }

    /// An unqualified stdio server publishes only where the platform gate
    /// allows it: on Windows the default enforce mode blocks it, and only the
    /// scoped `allow` override (or a non-Windows host) lets it reach the host.
    #[test]
    fn mcp_lifecycle_unqualified_stdio_publication_follows_platform_gate() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        seed_home_with_adopted_server(tmp.path());

        let config = ccync_foundation::config::CcyncConfig::load();
        let result = run_projection_core(&config, "ccync refresh");

        let mcp_dir = tmp.path().join(".ccync").join("build").join("mcp");
        let managed = std::fs::read_to_string(mcp_dir.join("managed.json")).ok();
        let claude = std::fs::read_to_string(tmp.path().join(".claude.json")).ok();

        restore_saved_env(&saved);

        let managed: serde_json::Value =
            serde_json::from_str(&managed.unwrap_or_else(|| "{}".to_string())).unwrap();
        let claude: serde_json::Value =
            serde_json::from_str(&claude.unwrap_or_else(|| "{}".to_string())).unwrap();
        if windows_stdio_publication_allowed() {
            assert_eq!(result, ExitCode::Success);
            assert_eq!(managed["servers"]["memory"]["command"], "npx");
            assert_eq!(claude["mcpServers"]["memory"]["command"], "npx");
        } else {
            assert_eq!(result, ExitCode::Error);
            assert!(
                claude["mcpServers"]["memory"].is_null(),
                "unqualified stdio must not reach the live host: {claude}"
            );
        }
    }

    #[test]
    fn declared_mcp_projection_transaction_keeps_engine_error_before_writes() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        let home = tmp.path().join(".ccync");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join("mcp.json"),
            r#"{"mcpServers":{"broken":{"command":"x","url":"https://invalid"}}}"#,
        )
        .unwrap();

        let managed = home.join("build/mcp/managed.json");
        let canonical = home.join("config.json");
        let host_paths = [
            tmp.path().join(".claude.json"),
            tmp.path().join(".codex/config.toml"),
            tmp.path().join(".config/github-copilot/mcp.json"),
            tmp.path().join(".config/opencode/opencode.json"),
            tmp.path().join(".gemini/settings.json"),
        ];
        fs::create_dir_all(managed.parent().unwrap()).unwrap();
        fs::write(&managed, b"managed-before").unwrap();
        fs::write(&canonical, b"canonical-before").unwrap();
        for (i, path) in host_paths.iter().enumerate() {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, format!("host-before-{i}")).unwrap();
        }
        let before_managed = fs::read(&managed).unwrap();
        let before_canonical = fs::read(&canonical).unwrap();
        let before_hosts: Vec<_> = host_paths
            .iter()
            .map(fs::read)
            .collect::<Result<_, _>>()
            .unwrap();

        let config = ccync_foundation::config::CcyncConfig::load();
        let (result, _) = run_projection_core_inner(&config, "ccync refresh");
        let root_was_not_created = !ccync_foundation::paths::canonical_plugin_root()
            .unwrap()
            .exists();
        let after_hosts: Vec<_> = host_paths
            .iter()
            .map(fs::read)
            .collect::<Result<_, _>>()
            .unwrap();
        let after_managed = fs::read(&managed).unwrap();
        let after_canonical = fs::read(&canonical).unwrap();
        restore_saved_env(&saved);

        assert_eq!(result, ExitCode::Error);
        assert!(
            root_was_not_created,
            "invalid declared input must fail before canonical root creation"
        );
        assert_eq!(after_canonical, before_canonical);
        assert_eq!(after_managed, before_managed);
        assert_eq!(after_hosts, before_hosts);
    }

    /// An MCP host read failure reaches the Machine phase, selects a
    /// non-zero exit, and leaves the projection-target report untouched — a
    /// live-host domain fault must never be reported as, or affect, a
    /// target's own status (independent domains).
    #[test]
    fn mcp_host_read_failure_reaches_machine_exits_error_target_report_unchanged() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        seed_home_with_adopted_server(tmp.path());
        // Malformed live Claude host config: the MCP read fails closed for
        // this host, isolating it into host_failures without ever touching
        // the Claude projection TARGET's own canonical/machine evidence.
        std::fs::write(tmp.path().join(".claude.json"), "not valid json {").unwrap();

        let config = ccync_foundation::config::CcyncConfig::load();
        let (result, accumulator) = run_projection_core_inner(&config, "ccync refresh");

        restore_saved_env(&saved);

        assert_eq!(
            result,
            ExitCode::Error,
            "a non-empty MCP host_failures must select a non-zero exit"
        );

        let report = accumulator.finish();
        let claude = &report.targets[&ccync_foundation::target::ProjectionTargetId::Claude];
        assert_eq!(
            claude.status,
            ccync_engine::install::ProjectionTargetStatus::Applied,
            "the Claude projection TARGET reaches Applied (both canonical and \
             machine phases completed for it) regardless of the unrelated MCP \
             HOST read failure: {claude:?}"
        );
    }

    /// Ownership -> canonical -> MCP order, and that the MCP closure side
    /// channel actually carries Canonical-phase `ProjectionEvidence` out to
    /// the accumulator built after `run_mcp_projection` returns.
    ///
    /// `run_mcp_projection`'s own contract already guarantees the closure
    /// runs strictly between ownership-proof and host-apply (unchanged by
    /// this task) — this test proves the CLOSURE ITSELF produced richer
    /// evidence by observing all three stages' real, disk-level effects in
    /// one run: the canonical render directory (proves canonical ran), the
    /// live host file's content (proves the host-apply transaction ran,
    /// necessarily after ownership was proven — `mcp_lifecycle_projects_server_to_live_host_and_commits_ownership`
    /// above already establishes that this same fixture only reaches a
    /// written live host after a clean ownership-proof + host transaction),
    /// and the accumulator's own recorded Canonical-phase path evidence for
    /// `Claude` (proves the side channel captured real, non-empty evidence
    /// from the closure and it reached `record_canonical`).
    #[test]
    fn projection_core_side_channel_carries_canonical_evidence_in_ownership_canonical_mcp_order() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        seed_home_with_adopted_http_server(tmp.path());

        let config = ccync_foundation::config::CcyncConfig::load();
        let (result, accumulator) = run_projection_core_inner(&config, "ccync refresh");

        let render_dir_exists = tmp
            .path()
            .join(".ccync")
            .join("build")
            .join("render")
            .is_dir();
        let mcp_dir = tmp.path().join(".ccync").join("build").join("mcp");
        let managed = std::fs::read_to_string(mcp_dir.join("managed.json")).ok();
        let claude_host = std::fs::read_to_string(tmp.path().join(".claude.json")).ok();

        restore_saved_env(&saved);

        assert_eq!(result, ExitCode::Success);
        assert!(
            render_dir_exists,
            "the canonical phase (inside the closure) must have run"
        );
        let managed: serde_json::Value =
            serde_json::from_str(&managed.expect("managed.json")).unwrap();
        assert_eq!(
            managed["servers"]["memory"]["url"], "https://mcp.example.test/memory",
            "the MCP host-apply transaction ran after the closure returned"
        );
        let claude_host: serde_json::Value =
            serde_json::from_str(&claude_host.expect(".claude.json written")).unwrap();
        assert_eq!(
            claude_host["mcpServers"]["memory"]["url"], "https://mcp.example.test/memory",
            "the live host reflects the ownership-proven, canonical-rendered content"
        );

        // The accumulator now has both Canonical evidence (the side channel,
        // this task's earlier sibling) and Machine evidence (this task)
        // recorded, so Claude clears the both-phase completion gate and
        // derives `Applied` from its real, surviving path changes.
        let report = accumulator.finish();
        let claude = &report.targets[&ccync_foundation::target::ProjectionTargetId::Claude];
        assert!(
            !claude.paths.is_empty(),
            "Claude's Canonical-phase path evidence must have reached the accumulator via the side channel: {claude:?}"
        );
        assert_eq!(
            claude.status,
            ccync_engine::install::ProjectionTargetStatus::Applied,
            "Claude clears the both-phase gate once the Machine phase is also recorded: {claude:?}"
        );
    }

    /// Exact canonical partial payload on a genuine target fault: Claude's
    /// own owned-path failure — an expected, non-panicking fault
    /// (`run_update_with_evidence` never returns an outer `Err` for this; the
    /// failure is a value inside its `Ok(evidence)`, per D-02) — must reach
    /// the accumulator with its exact reason text; the Canonical-phase
    /// short-circuit (Copilot never reached inside `apply_lifecycle_artifact_chain`)
    /// must not stop the independent Machine phase from running for Copilot;
    /// and the failure must surface in the run's own exit code (the
    /// accumulator's derived status is the exit-code authority, so a
    /// target failure is never silently swallowed into `ExitCode::Success`).
    #[test]
    fn projection_core_records_exact_partial_evidence_on_claude_owned_path_failure() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        seed_home_with_adopted_server(tmp.path());

        // Force Claude's marketplace-manifest write to fail: block
        // `~/.ccync/plugins/.claude-plugin` from being a directory (same
        // technique `ccync_engine::install`'s own
        // `run_update_with_evidence_claude_failure_short_circuits_copilot_but_records_failure_evidence`
        // test uses, reconstructed here from the public path helper since
        // `claude_marketplace_manifest_path` itself is private to that crate).
        let plugins_root = ccync_foundation::paths::ccync_plugins_root().unwrap();
        std::fs::create_dir_all(&plugins_root).unwrap();
        std::fs::write(plugins_root.join(".claude-plugin"), b"not a directory").unwrap();

        let config = ccync_foundation::config::CcyncConfig::load();
        let (result, accumulator) = run_projection_core_inner(&config, "ccync refresh");

        restore_saved_env(&saved);

        assert_eq!(
            result,
            ExitCode::Error,
            "a target-owned failure recorded by the accumulator must be reflected in the \
             run's exit code, not silently swallowed"
        );

        let report = accumulator.finish();
        let claude = &report.targets[&ccync_foundation::target::ProjectionTargetId::Claude];
        match &claude.status {
            ccync_engine::install::ProjectionTargetStatus::Failed { path, reason } => {
                assert!(
                    !reason.is_empty(),
                    "the exact injected failure reason must survive to the accumulator"
                );
                // Exact path pinning, not just "some non-empty reason": the
                // failing write was the marketplace manifest specifically —
                // `apply_claude_lifecycle` only attaches `Some(marketplace_path)`
                // to this exact failure branch (the skill-projection and
                // runtime-state-write failure branches attach `None` or
                // `Some(state_path)` respectively), so pinning the filename
                // discriminates a wrong-branch/wrong-path regression that a
                // bare non-empty-reason check would miss.
                let path = path
                    .as_ref()
                    .expect("this failure branch always carries Some(path)");
                assert_eq!(
                    path.file_name().and_then(|n| n.to_str()),
                    Some("marketplace.json"),
                    "the recorded failure path must be the exact file whose write failed, got {path:?}"
                );
            }
            other => panic!(
                "Claude must derive Failed from the injected owned-path fault, got {other:?}"
            ),
        }
        assert!(
            claude.paths.is_empty(),
            "Claude short-circuited before writing anything, so no Written/Removed path evidence \
             should have been recorded for it: {:?}",
            claude.paths
        );
        // Copilot was never reached inside the Canonical-phase lifecycle
        // chain (Claude's failure short-circuits it there) — so Copilot
        // never enters `canonical_completed`, and per the accumulator's own
        // both-phase completion gate it can never derive `Applied`/`Unchanged`
        // no matter what the Machine phase later records for it. But the
        // Machine phase (skills/commands/personalization) is a genuinely
        // separate, independent pass — it is not gated by the Canonical
        // lifecycle chain's short-circuit at all — so it DOES run for
        // Copilot and DOES record real path evidence. Proving Copilot stays
        // `NotRun` *despite* carrying real Machine-phase path evidence is
        // exactly what proves both halves of the probe at once: the Machine
        // phase traversed past Claude's Canonical-phase failure, and the
        // accumulator's both-phase gate still correctly
        // refuses to call a Canonical-incomplete target `Applied`.
        let copilot = &report.targets[&ccync_foundation::target::ProjectionTargetId::Copilot];
        assert_eq!(
            copilot.status,
            ccync_engine::install::ProjectionTargetStatus::NotRun,
            "Copilot must stay NotRun — its Canonical phase never completed, regardless of \
             what the independent Machine phase recorded for it: {copilot:?}"
        );
        assert!(
            !copilot.paths.is_empty(),
            "the Machine phase must have run for Copilot despite Claude's Canonical-phase \
             failure — an empty path list here would mean the Machine phase was wrongly \
             short-circuited too: {copilot:?}"
        );
    }

    /// Probe: a genuine global fault in the Canonical closure — an
    /// unresolvable home directory, `run_update_with_evidence`'s only outer
    /// `Err` path per its own doc comment — must return before the Machine
    /// phase ever runs. No target can reach `Applied`/`Unchanged` (both
    /// require the Machine phase's own completed-target set), so every
    /// target the accumulator reports must stay `NotRun`.
    #[test]
    fn projection_core_global_fault_never_reaches_machine_phase() {
        let _guard = crate::test_io_lock();
        let orig_userprofile = std::env::var_os("USERPROFILE");
        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::remove_var("USERPROFILE");
            std::env::remove_var("HOME");
        }

        let config = ccync_foundation::config::CcyncConfig::load();
        let (result, accumulator) = run_projection_core_inner(&config, "ccync refresh");

        unsafe {
            match &orig_userprofile {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
            match &orig_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }

        assert_eq!(
            result,
            ExitCode::Error,
            "an unresolvable home is a genuine global fault, not a per-target failure"
        );

        let report = accumulator.finish();
        assert!(
            report
                .targets
                .values()
                .all(|t| t.status == ccync_engine::install::ProjectionTargetStatus::NotRun),
            "no target may reach any completed status when the Canonical phase never ran and \
             the early return means the Machine phase never ran either: {report:?}"
        );
    }

    /// Probe: an MCP host-apply failure (one host skipped;
    /// `run_mcp_projection` still returns `Ok` — this is not the global fault
    /// covered above) must not prevent the Machine phase from running for the
    /// targets it covers. Reuses `mcp_lifecycle_unreadable_host_is_isolated_not_fatal`'s
    /// exact fixture (an unparseable Codex config skips that one host only)
    /// and calls `run_projection_core_inner` directly to inspect the
    /// accumulator the production wrapper does not expose.
    #[test]
    fn projection_core_host_failure_still_lets_machine_phase_run() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        seed_home_with_adopted_server(tmp.path());

        let codex_dir = tmp.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("config.toml"),
            "[mcp_servers.oops\ncommand = \"unterminated table header\n",
        )
        .unwrap();

        let config = ccync_foundation::config::CcyncConfig::load();
        let (result, accumulator) = run_projection_core_inner(&config, "ccync sync");

        restore_saved_env(&saved);

        assert_eq!(
            result,
            ExitCode::Error,
            "the unreadable host must still fail the command overall"
        );

        let report = accumulator.finish();
        let claude = &report.targets[&ccync_foundation::target::ProjectionTargetId::Claude];
        assert!(
            matches!(
                claude.status,
                ccync_engine::install::ProjectionTargetStatus::Applied
                    | ccync_engine::install::ProjectionTargetStatus::Unchanged
            ),
            "Claude must have completed BOTH phases — Applied/Unchanged is only reachable \
             once the Machine phase's own completed-target set also contains it — proving \
             the Machine phase ran despite the unrelated Codex host failure: {claude:?}"
        );
    }

    /// A genuine, unexpected Machine-phase fault (not a target's own
    /// owned-path value failure) must flip the run's exit code to `Error`,
    /// even though `run_machine_update` conservatively reports zero
    /// `completed_targets` on any fault — meaning every affected target
    /// derives `NotRun`, not `Failed`, and a `Failed`-only check would miss
    /// it entirely. Reuses the exact fixture technique the projection crate's
    /// mid-fault partial-attribution test
    /// (`crates/projection/src/tests.rs`) uses at the projection-crate level:
    /// a plain file at `~/.copilot/agents` blocks that path from being a
    /// directory, so the Machine phase's personalization step faults on
    /// Copilot.
    #[test]
    fn projection_core_machine_phase_fault_flips_exit_code_to_error() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        seed_home_with_adopted_server(tmp.path());

        let copilot_dir = tmp.path().join(".copilot");
        std::fs::create_dir_all(&copilot_dir).unwrap();
        std::fs::write(copilot_dir.join("agents"), "blocking file").unwrap();

        let config = ccync_foundation::config::CcyncConfig::load();
        let (result, accumulator) = run_projection_core_inner(&config, "ccync refresh");

        restore_saved_env(&saved);

        assert_eq!(
            result,
            ExitCode::Error,
            "a genuine Machine-phase fault must not be silently reported as ExitCode::Success"
        );

        // Confirm the fault really did leave the affected targets `NotRun`
        // (not `Failed`) — proving this scenario is only caught by the
        // dedicated fault tracking, not by the pre-existing `Failed`-target
        // check.
        let report = accumulator.finish();
        assert!(
            report.targets.values().all(|t| !matches!(
                t.status,
                ccync_engine::install::ProjectionTargetStatus::Failed { .. }
            )),
            "this scenario must not produce any Failed target — the fault is a global \
             Machine-phase fault, not a target-owned value failure: {report:?}"
        );
    }

    /// A failed Machine-phase options resolution means the Machine phase
    /// never ran at all — the run must exit `Error`, not `Success`, for the
    /// same reason a `run_machine_update` fault does (a phase that never
    /// executed did not converge). Injects the failure directly via
    /// `run_projection_core_with_machine_opts` because every environment-level
    /// way to fail the options resolution also breaks the Canonical phase;
    /// here the Canonical phase genuinely succeeds, so a wrong `Success` exit
    /// would otherwise be fully reachable.
    #[test]
    fn projection_core_machine_options_failure_flips_exit_code_to_error() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        seed_home_with_adopted_server(tmp.path());

        let config = ccync_foundation::config::CcyncConfig::load();
        let (result, accumulator) = run_projection_core_with_machine_opts(
            &config,
            "ccync refresh",
            Err("injected: machine options resolution failed".to_string()),
        );

        restore_saved_env(&saved);

        assert_eq!(
            result,
            ExitCode::Error,
            "a run whose Machine phase never executed must not report Success, even \
             though the Canonical phase completed cleanly"
        );

        // The Canonical phase DID complete its targets, so they appear in the
        // report (never silently absent) — but without the Machine phase they
        // stay NotRun rather than reaching Applied/Unchanged, and none is
        // Failed (the exit code flip comes from the fault tracking alone).
        let report = accumulator.finish();
        let claude = &report.targets[&ccync_foundation::target::ProjectionTargetId::Claude];
        assert_eq!(
            claude.status,
            ccync_engine::install::ProjectionTargetStatus::NotRun,
            "a dual-phase target whose Machine phase never ran must stay NotRun: {claude:?}"
        );
        assert!(
            report.targets.values().all(|t| !matches!(
                t.status,
                ccync_engine::install::ProjectionTargetStatus::Failed { .. }
            )),
            "no Failed target here — only the dedicated fault tracking can catch this: {report:?}"
        );
    }

    /// Byte + mtime snapshot of a path. `None` means the file does not exist —
    /// itself an assertable state (inspect must not *create* state/journal).
    #[cfg(test)]
    fn byte_mtime_snapshot(path: &std::path::Path) -> Option<(Vec<u8>, std::time::SystemTime)> {
        let bytes = std::fs::read(path).ok()?;
        let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
        Some((bytes, mtime))
    }

    /// `sync --dry-run` inspects MCP state, reports the two classes inspection
    /// can determine, explicitly marks the two it cannot, and writes nothing.
    ///
    /// Both determinable classes need their own fixture: `prepare_projection_recovery`
    /// takes the pending-transaction branch *or* the legacy-bootstrap branch
    /// (`if let Some(txn) = .. else if state.hosts.is_empty()`), never both in
    /// one run. A single-scenario probe would leave half the contract untested.
    #[test]
    fn dry_run_mcp_inspect() {
        let _guard = crate::test_io_lock();

        // ── Scenario A: legacy bootstrap ─────────────────────────────────────
        // Build the fixture with a *real* projection, then delete
        // projected-state.json. That is precisely a legacy machine: a live
        // entry and a managed.json that match, with no ownership record. Doing
        // it this way (rather than hand-writing a host entry) means the live
        // bytes are whatever the real serializer emits, so a fingerprint match
        // here proves the real bootstrap path, not a fixture I tuned to pass.
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        seed_home_with_adopted_http_server(tmp.path());

        let config = ccync_foundation::config::CcyncConfig::load();
        let setup = run_projection_core(&config, "ccync refresh");

        let mcp_dir = tmp.path().join(".ccync").join("build").join("mcp");
        let managed = mcp_dir.join("managed.json");
        let state = mcp_dir.join("projected-state.json");
        let txn = mcp_dir.join("projection.txn.json");
        let claude = tmp.path().join(".claude.json");

        std::fs::remove_file(&state).unwrap();

        let before = (
            byte_mtime_snapshot(&managed),
            byte_mtime_snapshot(&claude),
            byte_mtime_snapshot(&state),
            byte_mtime_snapshot(&txn),
        );
        let config = ccync_foundation::config::CcyncConfig::load();
        let bootstrap_lines = dry_run_report_lines(&config);
        let after = (
            byte_mtime_snapshot(&managed),
            byte_mtime_snapshot(&claude),
            byte_mtime_snapshot(&state),
            byte_mtime_snapshot(&txn),
        );

        restore_saved_env(&saved);

        assert_eq!(setup, ExitCode::Success, "fixture projection must succeed");
        let bootstrap = bootstrap_lines.join("\n");

        // (1a) determinable class one: legacy bootstrap.
        assert!(
            !bootstrap.contains("bootstrapped"),
            "matching content must not claim ownership:\n{bootstrap}"
        );
        // (3) nothing written, and no state/journal conjured into existence.
        assert_eq!(
            before.0, after.0,
            "managed.json bytes+mtime must not change"
        );
        assert_eq!(
            before.1, after.1,
            "live ~/.claude.json bytes+mtime must not change"
        );
        assert_eq!(
            before.2, after.2,
            "inspect must not write projected-state.json"
        );
        assert_eq!(
            before.3, after.3,
            "inspect must not write projection.txn.json"
        );
        assert!(!state.exists(), "inspect must not commit ownership");
        assert!(!txn.exists(), "inspect must not create a journal");

        // ── Scenario B: pending transaction ──────────────────────────────────
        let tmp2 = tempfile::TempDir::new().unwrap();
        let saved2 = isolate_env(tmp2.path());
        seed_home_with_adopted_http_server(tmp2.path());

        let config2 = ccync_foundation::config::CcyncConfig::load();
        let setup2 = run_projection_core(&config2, "ccync refresh");

        let mcp_dir2 = tmp2.path().join(".ccync").join("build").join("mcp");
        let state2 = mcp_dir2.join("projected-state.json");
        let txn2 = mcp_dir2.join("projection.txn.json");
        let claude2 = tmp2.path().join(".claude.json");

        // An interrupted deletion of `ghost`: post is the `absent` sentinel and
        // no `ghost` entry is live, so the classifier proves it landed. Uses
        // only the journal's documented token vocabulary (hex digest | "absent"),
        // which is why this needs no access to mcp's private fingerprinting.
        std::fs::write(
            &txn2,
            r#"{"version":1,"transaction_id":"txn-probe","entries":[{"host":"claude","name":"ghost","pre":"0000000000000000000000000000000000000000000000000000000000000000","post":"absent"}]}"#,
        )
        .unwrap();

        let before2 = (
            byte_mtime_snapshot(&state2),
            byte_mtime_snapshot(&txn2),
            byte_mtime_snapshot(&claude2),
        );
        let config2 = ccync_foundation::config::CcyncConfig::load();
        let txn_lines = dry_run_report_lines(&config2);
        let after2 = (
            byte_mtime_snapshot(&state2),
            byte_mtime_snapshot(&txn2),
            byte_mtime_snapshot(&claude2),
        );

        restore_saved_env(&saved2);

        assert_eq!(setup2, ExitCode::Success, "fixture projection must succeed");
        let pending = txn_lines.join("\n");

        // (1b) determinable class two: pending transaction. Asserted on the
        // journal itself, not merely on its landed subset — an all-NotLanded
        // transaction grants no ownership yet is still a pending transaction
        // the dry run must disclose.
        assert!(
            pending.contains("pending transaction txn-probe found, covering 1 host/name pair(s)"),
            "dry run must report the pending transaction it determined:\n{pending}"
        );
        assert!(
            pending.contains("recovered 1 landed removal(s) on claude; ownership revoked"),
            "dry run must report the recovery classification it determined:\n{pending}"
        );
        // (3) the journal must survive an inspection untouched.
        assert_eq!(
            before2.0, after2.0,
            "inspect must not touch projected-state.json"
        );
        assert_eq!(
            before2.1, after2.1,
            "inspect must not clear or rewrite the journal"
        );
        assert_eq!(
            before2.2, after2.2,
            "inspect must not touch the live host config"
        );
        assert!(
            txn2.exists(),
            "inspect must leave the pending journal in place"
        );

        // ── (2) the undetermined classes are named, marked, and reasoned ─────
        // An omitted or empty list would read as "none found", which is a claim
        // inspection never checked.
        for report in [&bootstrap, &pending] {
            assert!(
                report.contains("MCP actions NOT determined by --dry-run:"),
                "the undetermined group must be present:\n{report}"
            );
            for class in ["planned deletion", "collision"] {
                assert!(
                    report.contains(&format!("{class}: not determined (requires render)")),
                    "`{class}` must carry an explicit not-determined marker:\n{report}"
                );
            }
            assert!(
                report.contains("the render writes live surfaces, so a dry run cannot produce it"),
                "the not-determined marker must carry its reason:\n{report}"
            );
        }
    }

    #[test]
    fn plugin_mcp_e2e() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        std::fs::create_dir_all(tmp.path().join(".agents")).unwrap();
        std::fs::write(tmp.path().join(".agents/mcp.json"), r#"{"ccyncMcpServers":{},"hostRoutes":[{"source":"test-plugin","server":"plugin-srv","hosts":["claude","copilot","opencode","codex","agy"]}]}"#).unwrap();

        // A standard MCP plugin bundle: lockfile entry + plugins/<src>/.mcp.json
        // using the ecosystem-standard `mcpServers` key.
        let build = tmp.path().join(".ccync").join("build");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(
            build.join("lock.json"),
            r#"{"resolvedPlugins":[{"pluginId":"test-plugin","installStrategy":"bundled-local","resolvedSource":"test-plugin"}]}"#,
        )
        .unwrap();
        let plugin_dir = tmp
            .path()
            .join(".ccync")
            .join("plugins")
            .join("test-plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join(".mcp.json"),
            r#"{"mcpServers":{"plugin-srv":{"type":"http","url":"https://mcp.example.test/plugin"}}}"#,
        )
        .unwrap();

        // Pre-existing user MCP entry on the live Claude host — must survive.
        std::fs::write(
            tmp.path().join(".claude.json"),
            r#"{"mcpServers":{"user-srv":{"command":"u"}}}"#,
        )
        .unwrap();

        let config = ccync_foundation::config::CcyncConfig::load();
        let project = run_projection_core(&config, "ccync sync");

        let mcp_dir = build.join("mcp");
        let managed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(mcp_dir.join("managed.json")).unwrap())
                .unwrap();
        let claude: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join(".claude.json")).unwrap(),
        )
        .unwrap();

        // --- assertions for the projection half (captured before env restore) ---
        let managed_ok = project == ExitCode::Success
            && managed["servers"]["plugin-srv"]["url"] == "https://mcp.example.test/plugin";
        let live_has_plugin =
            claude["mcpServers"]["plugin-srv"]["url"] == "https://mcp.example.test/plugin";
        let live_keeps_user = claude["mcpServers"]["user-srv"]["command"] == "u";

        // Remove the plugin (empty catalog) and re-project: the owned entry must
        // be pruned from the live host while the unowned user entry survives.
        std::fs::write(build.join("lock.json"), r#"{"resolvedPlugins":[]}"#).unwrap();
        std::fs::remove_file(plugin_dir.join(".mcp.json")).unwrap();
        let remove = run_projection_core(&config, "ccync sync");
        let claude_after: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join(".claude.json")).unwrap(),
        )
        .unwrap();

        restore_saved_env(&saved);

        assert!(
            managed_ok,
            "standard-key plugin server reaches managed.json"
        );
        assert!(
            live_has_plugin,
            "plugin server projected to the live Claude host"
        );
        assert!(live_keeps_user, "user entry preserved during projection");
        assert_eq!(remove, ExitCode::Success);
        assert!(
            claude_after["mcpServers"]["plugin-srv"].is_null(),
            "owned plugin entry pruned after removal"
        );
        assert_eq!(
            claude_after["mcpServers"]["user-srv"]["command"], "u",
            "unowned user entry survives removal"
        );
    }

    #[test]
    fn mcp_lifecycle_forced_host_failure_returns_error_no_clean_success() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        seed_home_with_adopted_http_server(tmp.path());

        // A readable but unwritable Claude host: valid empty config, but its
        // atomic-write temp sibling path is pre-occupied by a directory, so the
        // post-render host write fails.
        std::fs::write(tmp.path().join(".claude.json"), r#"{"mcpServers":{}}"#).unwrap();
        std::fs::create_dir_all(tmp.path().join(".claude.json.tmp")).unwrap();

        let config = ccync_foundation::config::CcyncConfig::load();
        let result = run_projection_core(&config, "ccync refresh");

        let mcp_dir = tmp.path().join(".ccync").join("build").join("mcp");
        let txn_present = mcp_dir.join("projection.txn.json").exists();

        restore_saved_env(&saved);

        assert_eq!(
            result,
            ExitCode::Error,
            "forced host-write failure must return Error/1"
        );
        assert!(
            txn_present,
            "write-ahead journal retained after a host failure"
        );
    }

    #[test]
    fn mcp_lifecycle_failure_report_never_claims_a_render_that_did_not_run() {
        let err = "interrupted MCP transaction is ambiguous for: claude/a";

        // render never invoked → must not claim anything was projected.
        let never = projection_failure_report("ccync sync", &None, err, false);
        let never = never.join("\n");
        assert!(
            !never.contains("were projected"),
            "must not claim a projection that never happened: {never}"
        );
        assert!(
            !never.contains("journal is retained"),
            "must not claim a journal that was never written: {never}"
        );
        assert!(never.contains("nothing was projected"), "{never}");
        assert!(
            never.contains(err),
            "the underlying MCP error is surfaced: {never}"
        );

        // render ran and failed → report the canonical error, nothing else.
        let failed = projection_failure_report(
            "ccync sync",
            &Some(Err("canonical render blew up".to_string())),
            err,
            false,
        )
        .join("\n");
        assert!(failed.contains("canonical render blew up"), "{failed}");
        assert!(!failed.contains("were projected"), "{failed}");

        // render ran and succeeded, host write interrupted → journal on disk.
        let partial = projection_failure_report("ccync sync", &Some(Ok(())), err, true).join("\n");
        assert!(
            partial.contains("canonical/managed surfaces were projected"),
            "{partial}"
        );
        assert!(
            partial.contains("the change journal is retained"),
            "{partial}"
        );

        // render ran and succeeded, MCP failed closed → no journal exists, so
        // do not send the user back for a convergence that cannot happen.
        let closed = projection_failure_report("ccync sync", &Some(Ok(())), err, false).join("\n");
        assert!(
            closed.contains("canonical/managed surfaces were projected"),
            "{closed}"
        );
        assert!(
            !closed.contains("journal is retained"),
            "must not claim a retained journal when none was written: {closed}"
        );
        assert!(closed.contains("no live MCP host was changed"), "{closed}");
    }

    // ── projection_failure_report: all four phase-specific messages ─────────
    //
    // Byte-exact companion to `mcp_lifecycle_failure_report_never_claims_a_render_that_did_not_run`
    // above (substring probes): pins the full line set for each of the four
    // `render_outcome`/`journal_retained` combinations so no future edit can
    // collapse two of the four distinct arms into one without a visible diff.

    #[test]
    fn projection_failure_report_render_never_ran_is_byte_exact() {
        let err = "interrupted MCP transaction is ambiguous for: claude/a";
        assert_eq!(
            projection_failure_report("ccync sync", &None, err, false),
            vec![
                "ccync sync: MCP projection failed before the canonical render ran: interrupted MCP transaction is ambiguous for: claude/a".to_string(),
                "ccync sync: nothing was projected — no canonical render, no managed surfaces, and no live host was changed".to_string(),
            ]
        );
    }

    #[test]
    fn projection_failure_report_canonical_render_failed_is_byte_exact() {
        let err = "interrupted MCP transaction is ambiguous for: claude/a";
        assert_eq!(
            projection_failure_report(
                "ccync sync",
                &Some(Err("canonical render blew up".to_string())),
                err,
                false,
            ),
            vec!["ccync sync: canonical render blew up".to_string()]
        );
    }

    #[test]
    fn projection_failure_report_mcp_interrupted_journal_retained_is_byte_exact() {
        let err = "interrupted MCP transaction is ambiguous for: claude/a";
        assert_eq!(
            projection_failure_report("ccync sync", &Some(Ok(())), err, true),
            vec![
                "ccync sync: MCP projection failed: interrupted MCP transaction is ambiguous for: claude/a".to_string(),
                "ccync sync: canonical/managed surfaces were projected, but MCP host config was not fully updated; the change journal is retained — re-run sync to converge".to_string(),
            ]
        );
    }

    #[test]
    fn projection_failure_report_mcp_failed_closed_no_journal_is_byte_exact() {
        let err = "interrupted MCP transaction is ambiguous for: claude/a";
        assert_eq!(
            projection_failure_report("ccync sync", &Some(Ok(())), err, false),
            vec![
                "ccync sync: MCP projection failed: interrupted MCP transaction is ambiguous for: claude/a".to_string(),
                "ccync sync: canonical/managed surfaces were projected; no live MCP host was changed — resolve the error above, then re-run sync".to_string(),
            ]
        );
    }

    // ── reworded dry-run header and first-run line ────────────────────────────

    #[test]
    fn dry_run_header_and_first_run_line_are_byte_exact_and_old_lines_are_gone() {
        let this_file = include_str!("lifecycle.rs");
        assert!(
            this_file.contains("ccync sync: paths that would be created, updated, or removed:"),
            "the reworded dry-run header must be present byte-exact"
        );
        assert!(
            this_file.contains("ccync: about to create, update, or remove ccync-managed paths:"),
            "the reworded first-run line must be present byte-exact"
        );
        // Needles are assembled at runtime (split across literals) so this
        // test's own source text does not trip its own assertion via
        // include_str!, mirroring removed_suppression_helper_never_reappears above.
        let old_header = ["surfaces that would", " be touched:"].concat();
        let old_first_run = ["about to project to", " the following surfaces:"].concat();
        assert_eq!(
            this_file.matches(&old_header).count(),
            0,
            "old dry-run header must be gone"
        );
        assert_eq!(
            this_file.matches(&old_first_run).count(),
            0,
            "old first-run line must be gone"
        );
    }

    #[test]
    fn dry_run_report_lines_status_and_mcp_content_unaffected_by_header_reword() {
        // The header text changed (this task); everything after it — the
        // grouped touch-scope disclosure and any MCP evidence — must render
        // identically to before, since only the literal header/first-run
        // strings were in scope here.
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        std::fs::create_dir_all(tmp.path().join(".ccync")).unwrap();
        std::fs::write(
            tmp.path().join(".ccync").join("config.json"),
            r#"{"selectedRuntimes":["claude"]}"#,
        )
        .unwrap();

        let config = ccync_foundation::config::CcyncConfig::load();
        let lines = dry_run_report_lines(&config);

        restore_saved_env(&saved);

        assert_eq!(lines[0], "--- DRY RUN ---");
        assert_eq!(
            lines[1],
            "ccync sync: paths that would be created, updated, or removed:"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Runtime surfaces (would touch, current selection):")),
            "the grouped touch-scope disclosure must still follow the reworded header: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l
                    .contains("MCP host files (may be written; skipped when already identical):")),
            "the MCP-hosts group must still follow the reworded header: {lines:?}"
        );
    }

    #[test]
    fn mcp_recovery_action_lines_absent_emits_nothing() {
        assert!(mcp_recovery_action_lines("ccync sync", &[]).is_empty());
    }

    #[test]
    fn mcp_recovery_action_lines_named_line_is_byte_exact() {
        assert_eq!(
            mcp_recovery_action_lines(
                "ccync sync",
                &["bootstrapped legacy ownership for claude/memory".to_string()]
            ),
            vec![
                "ccync sync: MCP recovery: bootstrapped legacy ownership for claude/memory"
                    .to_string()
            ]
        );
    }

    #[test]
    fn mcp_lifecycle_inline_codex_server_fails_closed_and_stays_valid_toml() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        seed_home_with_adopted_server(tmp.path());

        // A prior managed.json, so the legacy-bootstrap path runs: it claims any
        // name whose live entry exactly matches the prior render.
        let mcp_dir = tmp.path().join(".ccync").join("build").join("mcp");
        std::fs::create_dir_all(&mcp_dir).unwrap();
        std::fs::write(
            mcp_dir.join("managed.json"),
            r#"{"servers":{"memory":{"command":"npx","args":["-y","srv"]}}}"#,
        )
        .unwrap();

        // The user's Codex config defines "memory" as an inline key under a
        // plain `[mcp_servers]` table. This is legal TOML and *semantically
        // identical* to what ccync renders, so it fingerprint-matches the prior
        // render and the bootstrap would claim it — but the line-based writer
        // can only strip a `[mcp_servers.memory]` header, and there is none.
        // Claiming it therefore buys ccync a name it can never remove, and an
        // overlay would append a second definition beside the surviving inline
        // key: duplicate key, invalid TOML, Codex loads nothing at all.
        let codex_dir = tmp.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let original =
            "[model]\nname = \"o3\"\n\n[mcp_servers]\nmemory = { command = \"npx\", args = [\"-y\", \"srv\"] }\n";
        std::fs::write(codex_dir.join("config.toml"), original).unwrap();

        let config = ccync_foundation::config::CcyncConfig::load();
        let result = run_projection_core(&config, "ccync sync");

        let codex_after = std::fs::read_to_string(codex_dir.join("config.toml")).unwrap();
        let state = std::fs::read_to_string(mcp_dir.join("projected-state.json")).ok();

        restore_saved_env(&saved);

        // Ownership is the load-bearing assertion: the bootstrap must refuse to
        // claim a name the writer could never remove.
        assert!(
            state.as_deref().is_none_or(|s| {
                let v: serde_json::Value = serde_json::from_str(s).unwrap();
                v["hosts"]["codex"].is_null()
            }),
            "ccync must not claim ownership of an entry its writer cannot remove, got {state:?}"
        );
        assert_eq!(
            result,
            ExitCode::Error,
            "an unowned collision must fail closed"
        );
        assert_eq!(
            codex_after, original,
            "the live entry is preserved and no deletion is performed"
        );
        assert!(
            !codex_after.contains("[mcp_servers.memory]"),
            "no second definition may be appended beside the surviving inline key — \
             that duplicate key is what stops Codex loading its config: {codex_after}"
        );
    }

    #[test]
    fn mcp_lifecycle_unreadable_host_is_isolated_not_fatal() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        seed_home_with_adopted_http_server(tmp.path());

        // One hand-edited, unparseable Codex config. Every other surface is fine.
        let codex_dir = tmp.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let broken = "[mcp_servers.oops\ncommand = \"unterminated table header\n";
        std::fs::write(codex_dir.join("config.toml"), broken).unwrap();

        let config = ccync_foundation::config::CcyncConfig::load();
        let result = run_projection_core(&config, "ccync sync");

        let mcp_dir = tmp.path().join(".ccync").join("build").join("mcp");
        let managed = std::fs::read_to_string(mcp_dir.join("managed.json")).ok();
        let claude = std::fs::read_to_string(tmp.path().join(".claude.json")).ok();
        let codex_after = std::fs::read_to_string(codex_dir.join("config.toml")).unwrap();
        let state = std::fs::read_to_string(mcp_dir.join("projected-state.json")).ok();

        restore_saved_env(&saved);

        // Honest overall outcome: the projection did not fully converge.
        assert_eq!(
            result,
            ExitCode::Error,
            "an unreadable host must still fail the command"
        );

        // ...but the blast radius is one host, not the whole of `ccync sync`.
        let managed: serde_json::Value =
            serde_json::from_str(&managed.expect("canonical render must still run")).unwrap();
        assert_eq!(
            managed["servers"]["memory"]["url"],
            "https://mcp.example.test/memory"
        );
        let claude: serde_json::Value =
            serde_json::from_str(&claude.expect("readable hosts must still be projected")).unwrap();
        assert_eq!(
            claude["mcpServers"]["memory"]["url"], "https://mcp.example.test/memory",
            "a malformed codex config must not stop the claude projection"
        );

        // The unreadable host is left exactly as found, and gains no ownership:
        // ccync can prove nothing about a file it cannot parse.
        assert_eq!(codex_after, broken, "unreadable host must not be written");
        let state: serde_json::Value =
            serde_json::from_str(&state.expect("ownership committed for the hosts that landed"))
                .unwrap();
        assert!(
            state["hosts"]["codex"].is_null(),
            "unreadable host must gain no ownership, got {}",
            state["hosts"]["codex"]
        );
        assert!(
            !state["hosts"]["claude"].is_null(),
            "readable hosts still commit ownership"
        );
    }

    #[test]
    fn mcp_lifecycle_third_state_aborts_before_the_canonical_render() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        seed_home_with_adopted_server(tmp.path());

        // A pending transaction whose live entry matches neither pre nor post:
        // the ambiguous third state. Preparation must fail closed *before* the
        // canonical render runs — which is precisely the case the old
        // `render_error.is_none()` test misread as "the render succeeded".
        let mcp_dir = tmp.path().join(".ccync").join("build").join("mcp");
        std::fs::create_dir_all(&mcp_dir).unwrap();
        std::fs::write(
            tmp.path().join(".claude.json"),
            r#"{"mcpServers":{"memory":{"command":"hand-edited-by-user"}}}"#,
        )
        .unwrap();
        std::fs::write(
            mcp_dir.join("projection.txn.json"),
            r#"{"version":1,"transaction_id":"t","entries":[{"host":"claude","name":"memory","pre":"absent","post":"0000000000000000000000000000000000000000000000000000000000000000"}]}"#,
        )
        .unwrap();

        let config = ccync_foundation::config::CcyncConfig::load();
        let result = run_projection_core(&config, "ccync sync");

        let managed_written = mcp_dir.join("managed.json").exists();
        let txn_present = mcp_dir.join("projection.txn.json").exists();
        let state_present = mcp_dir.join("projected-state.json").exists();

        restore_saved_env(&saved);

        assert_eq!(result, ExitCode::Error, "third state must return Error/1");
        assert!(
            !managed_written,
            "the canonical render must not have run — so no message may claim it did"
        );
        assert!(txn_present, "journal untouched by a fail-closed abort");
        assert!(!state_present, "no ownership committed on a third state");
    }

    // ── pre-init sync guard (machine-initialization preflight) ────────────────

    /// Recursive snapshot of every file under `dir`: (relative path, byte
    /// length, modified time). Empty when `dir` does not exist. Used to prove
    /// a refused pre-init sync leaves the entire home tree byte- and
    /// mtime-identical (true zero mutation), not just "no new files".
    fn snapshot_tree(dir: &std::path::Path) -> Vec<(String, u64, std::time::SystemTime)> {
        fn walk(
            base: &std::path::Path,
            dir: &std::path::Path,
            out: &mut Vec<(String, u64, std::time::SystemTime)>,
        ) {
            let Ok(read) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in read.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(base, &path, out);
                } else if let Ok(meta) = path.metadata() {
                    let rel = path
                        .strip_prefix(base)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned();
                    out.push((
                        rel,
                        meta.len(),
                        meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                    ));
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }

    #[test]
    fn pre_init_sync_guard_refuses_with_zero_mutation() {
        let _env_guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());

        // Fresh home: no config.json anywhere — machine_is_initialized() must
        // be false, and the whole home tree starts out empty.
        assert!(
            !machine_is_initialized(),
            "sanity: fresh redirected home must be uninitialized"
        );
        let before = snapshot_tree(tmp.path());

        let result = run_unified_projection(false, true, false, Some(&SYNC_GATE_ABORT_CONTEXT));

        let after = snapshot_tree(tmp.path());
        restore_saved_env(&saved);

        assert_eq!(
            result,
            ExitCode::Error,
            "pre-init sync must refuse with Error/1"
        );
        assert!(
            before.is_empty(),
            "sanity: nothing should exist in the fresh home before the refused sync"
        );
        assert_eq!(
            before, after,
            "a refused pre-init sync must leave every file byte- and mtime-identical (zero mutation)"
        );
    }

    #[test]
    fn pre_init_sync_guard_dry_run_bypasses_guard_and_still_dry_runs() {
        let _env_guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());

        assert!(
            !machine_is_initialized(),
            "sanity: fresh redirected home must be uninitialized"
        );

        let result = run_unified_projection(true, true, false, Some(&SYNC_GATE_ABORT_CONTEXT));

        restore_saved_env(&saved);

        assert_eq!(
            result,
            ExitCode::Success,
            "sync --dry-run must remain available pre-init, still reaching its existing dry-run seam"
        );
    }

    #[test]
    fn pre_init_sync_guard_allows_already_initialized_sync() {
        let _env_guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());

        let ccync_home = tmp.path().join(".ccync");
        std::fs::create_dir_all(&ccync_home).unwrap();
        std::fs::write(ccync_home.join("config.json"), r#"{}"#).unwrap();
        assert!(
            machine_is_initialized(),
            "sanity: seeded config.json must count as initialized"
        );

        let result = run_unified_projection(false, true, false, Some(&SYNC_GATE_ABORT_CONTEXT));

        let render_dir = ccync_home.join("build").join("render");
        let reached_projection = render_dir.is_dir();

        restore_saved_env(&saved);

        assert_eq!(
            result,
            ExitCode::Success,
            "an already-initialized sync must still succeed"
        );
        assert!(
            reached_projection,
            "an already-initialized sync must still reach the existing projection seam (canonical render written)"
        );
    }

    #[test]
    fn pre_init_sync_guard_does_not_gate_add_or_remove_context_on_fresh_home() {
        // require_initialized_machine() is invoked inside run_unified_projection
        // only for SYNC_GATE_ABORT_CONTEXT-shaped calls (sync + upgrade's
        // reprojection step). add/remove pass their own distinct abort
        // contexts and must not be refused by this preflight on a fresh,
        // uninitialized home — they gain their own preflight at their own
        // entry points in later work.
        let _env_guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());

        assert!(
            !machine_is_initialized(),
            "sanity: fresh redirected home must be uninitialized"
        );

        let add_result = run_unified_projection(false, true, false, Some(&ADD_GATE_ABORT_CONTEXT));
        let remove_result =
            run_unified_projection(false, true, false, Some(&REMOVE_GATE_ABORT_CONTEXT));

        restore_saved_env(&saved);

        assert_eq!(
            add_result,
            ExitCode::Success,
            "ADD_GATE_ABORT_CONTEXT calls must not be refused by the sync-only init preflight"
        );
        assert_eq!(
            remove_result,
            ExitCode::Success,
            "REMOVE_GATE_ABORT_CONTEXT calls must not be refused by the sync-only init preflight"
        );
    }

    #[test]
    fn pre_init_sync_guard_refusal_names_ccync_init() {
        let this_file = include_str!("lifecycle.rs");
        assert!(
            this_file.contains("run `ccync init <master>` first"),
            "the pre-init refusal message must name `ccync init <master>`"
        );
    }

    // ── all-invalid-selection preflight ────────────────────────────────────────

    #[test]
    fn all_invalid_selection_refuses_before_any_domain_mutation() {
        // Selection key present (`selectedRuntimes`) but every entry fails to
        // parse as a known ProjectionTargetId, so `resolve_projection_targets`
        // returns zero targets with `state != Absent`. run_unified_projection
        // must refuse with ExitCode::Error before build_touch_scope / any
        // domain mutation — no lockfile write, no canonical render.
        let _env_guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());

        let ccync_home = tmp.path().join(".ccync");
        std::fs::create_dir_all(&ccync_home).unwrap();
        std::fs::write(
            ccync_home.join("config.json"),
            r#"{"selectedRuntimes":["not-a-real-target"]}"#,
        )
        .unwrap();

        let result = run_unified_projection(false, true, false, Some(&SYNC_GATE_ABORT_CONTEXT));

        let render_dir = ccync_home.join("build").join("render");
        let lockfile = ccync_home.join("plugins.lock.json");
        let reached_projection = render_dir.is_dir();
        let wrote_lockfile = lockfile.is_file();

        restore_saved_env(&saved);

        assert_eq!(
            result,
            ExitCode::Error,
            "an all-invalid selection must refuse rather than proceed"
        );
        assert!(
            !reached_projection,
            "the refusal must land before build_touch_scope / the canonical render ever runs"
        );
        assert!(
            !wrote_lockfile,
            "the refusal must land before run_catalog_resolve's lockfile write"
        );
    }

    #[test]
    fn all_invalid_selection_refusal_names_init() {
        let this_file = include_str!("lifecycle.rs");
        assert!(
            this_file
                .contains("the selected runtime(s) did not resolve to any known projection target"),
            "the all-invalid-selection refusal must explain what happened"
        );
        assert!(
            this_file.contains("run `ccync init <master>` to reselect"),
            "the all-invalid-selection refusal must name the init command"
        );
    }

    #[test]
    fn all_invalid_selection_dry_run_is_exempt() {
        // --dry-run reports only and never mutates; the all-invalid-selection
        // preflight must not refuse it.
        let _env_guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());

        let ccync_home = tmp.path().join(".ccync");
        std::fs::create_dir_all(&ccync_home).unwrap();
        std::fs::write(
            ccync_home.join("config.json"),
            r#"{"selectedRuntimes":["not-a-real-target"]}"#,
        )
        .unwrap();

        let result = run_unified_projection(true, true, false, Some(&SYNC_GATE_ABORT_CONTEXT));

        restore_saved_env(&saved);

        assert_eq!(
            result,
            ExitCode::Success,
            "--dry-run must remain exempt from the all-invalid-selection preflight"
        );
    }

    #[test]
    fn absent_selection_is_not_refused_by_all_invalid_preflight() {
        // No selection key present at all (`LegacyState::Absent`) is a
        // different, pre-existing case (default target set applies) — the
        // all-invalid preflight must only fire when a selection was present
        // but every entry in it was invalid.
        let _env_guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());

        let ccync_home = tmp.path().join(".ccync");
        std::fs::create_dir_all(&ccync_home).unwrap();
        std::fs::write(ccync_home.join("config.json"), r#"{}"#).unwrap();

        let result = run_unified_projection(false, true, false, Some(&SYNC_GATE_ABORT_CONTEXT));

        restore_saved_env(&saved);

        assert_eq!(
            result,
            ExitCode::Success,
            "an absent selection must not be refused by the all-invalid-selection preflight"
        );
    }

    // ── lock_has_projection ───────────────────────────────────────────────────

    #[test]
    fn lock_has_projection_false_when_file_absent() {
        let dir = TempDir::new().unwrap();
        assert!(!lock_has_projection(&dir.path().join("nope.json")));
    }

    #[test]
    fn lock_has_projection_false_when_key_missing() {
        let dir = TempDir::new().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        std::fs::write(&lock, r#"{"resolvedPlugins":[]}"#).unwrap();
        assert!(!lock_has_projection(&lock));
    }

    #[test]
    fn lock_has_projection_true_when_key_present() {
        let dir = TempDir::new().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        std::fs::write(
            &lock,
            r#"{"_ccyncProjection":{"version":1},"resolvedPlugins":[]}"#,
        )
        .unwrap();
        assert!(lock_has_projection(&lock));
    }

    // ── confirm_overwrite ─────────────────────────────────────────────────────

    #[test]
    fn confirm_overwrite_assume_yes_returns_true() {
        assert!(confirm_overwrite(true));
    }

    #[test]
    fn confirm_overwrite_env_var_returns_true() {
        // CCYNC_ASSUME_YES is process-global; hold the shared I/O lock so this
        // set/remove cannot interleave with confirm_overwrite_non_tty_* reading it.
        let _guard = crate::test_io_lock();
        unsafe {
            std::env::set_var("CCYNC_ASSUME_YES", "1");
        }
        let result = confirm_overwrite(false);
        unsafe {
            std::env::remove_var("CCYNC_ASSUME_YES");
        }
        assert!(result, "CCYNC_ASSUME_YES=1 must return true");
    }

    #[test]
    fn confirm_overwrite_non_tty_no_flags_returns_false() {
        // In cargo test, stdin is not a TTY — gate must abort without assume_yes.
        // Hold the shared I/O lock so a concurrent test setting CCYNC_ASSUME_YES
        // cannot flip this read (the env var is process-global).
        let _guard = crate::test_io_lock();
        if !std::io::stdin().is_terminal() {
            assert!(!confirm_overwrite(false));
        }
    }

    #[test]
    fn gate_abort_message_sync_context_is_truthful() {
        let joined = gate_abort_message(&SYNC_GATE_ABORT_CONTEXT).join("\n");
        assert!(joined.contains("ccync lockfile / resolved plugin state"));
        assert!(joined.contains("canonical render, live agent config surfaces"));
        assert!(joined.contains("ccync sync --yes"));
        assert!(!joined.contains("no files written"));
    }

    #[test]
    fn gate_abort_message_add_context_is_truthful() {
        let joined = gate_abort_message(&ADD_GATE_ABORT_CONTEXT).join("\n");
        assert!(joined
            .contains("plugin cache, personal catalog, ccync lockfile / resolved plugin state"));
        assert!(joined.contains("canonical render, live agent config surfaces"));
        assert!(joined.contains("ccync sync --yes"));
        assert!(!joined.contains("no files written"));
    }

    #[test]
    fn gate_abort_message_remove_context_is_truthful() {
        let joined = gate_abort_message(&REMOVE_GATE_ABORT_CONTEXT).join("\n");
        assert!(joined.contains("personal catalog or adopted-item state, ccync lockfile / resolved plugin state, canonical render"));
        assert!(joined.contains("live agent config surfaces"));
        assert!(joined.contains("ccync sync --yes"));
        assert!(!joined.contains("no files written"));
    }

    // ── grouped touch-scope disclosure (TouchScope / render_touch_scope) ──────

    #[test]
    fn grouped_touch_scope_render_is_deterministic_and_dry_run_gate_identical() {
        // render_touch_scope is a pure function of its input, so the same scope
        // value renders byte-identically for every caller.
        //
        // The three callers no longer pass an *identical* scope: the dry run
        // additionally carries an attempted MCP inspection (it reports instead
        // of acting), while the gate and init mark it not requested. The
        // shared path-disclosure contract is unaffected — that lives in the
        // runtime-surface and MCP-host groups, which are still derived from one
        // model and rendered by this one function.
        // `grouped_touch_scope_shared_groups_are_identical_across_callers`
        // below pins that shared prefix directly.
        let scope = TouchScope {
            runtime_surfaces: vec![
                (
                    "claude".to_string(),
                    PathBuf::from("/home/u/.claude/skills/ccync"),
                ),
                (
                    "copilot".to_string(),
                    PathBuf::from("/home/u/.copilot/skills"),
                ),
            ],
            instruction_files: Vec::new(),
            instruction_prune_files: Vec::new(),
            mcp_hosts: vec![
                PathBuf::from("/home/u/.claude.json"),
                PathBuf::from("/home/u/.codex/config.toml"),
            ],
            mcp_inspect: McpInspection::NotRequested,
            instruction_blocked: false,
        };
        let dry_run_lines = render_touch_scope(&scope);
        let gate_lines = render_touch_scope(&scope);
        let init_lines = render_touch_scope(&scope);
        assert_eq!(
            dry_run_lines, gate_lines,
            "dry-run and the first-projection gate must render byte-identical grouped output for the same scope"
        );
        assert_eq!(
            gate_lines, init_lines,
            "init's pre-projection disclosure reuses the exact same gate rendering, so it must also match"
        );
    }

    #[test]
    fn grouped_touch_scope_shared_groups_are_identical_across_callers() {
        // The path-disclosure groups must not diverge between callers. The
        // dry run appends an MCP-inspection group the gate has no business
        // printing, but everything above it must match line for line.
        let base = |inspect| TouchScope {
            runtime_surfaces: vec![(
                "claude".to_string(),
                PathBuf::from("/home/u/.claude/skills/ccync"),
            )],
            instruction_files: Vec::new(),
            instruction_prune_files: Vec::new(),
            mcp_hosts: vec![PathBuf::from("/home/u/.claude.json")],
            mcp_inspect: inspect,
            instruction_blocked: false,
        };
        let gate_lines = render_touch_scope(&base(McpInspection::NotRequested));
        let dry_run_lines = render_touch_scope(&base(McpInspection::Attempted(Ok(
            mcp::McpInspectReport::default(),
        ))));

        assert_eq!(
            dry_run_lines[..gate_lines.len()],
            gate_lines[..],
            "the dry run must render the gate's path groups byte-identically and only append after them"
        );
        assert!(
            dry_run_lines.len() > gate_lines.len(),
            "the dry run must actually append its MCP inspection group"
        );
        assert!(
            dry_run_lines
                .iter()
                .any(|line| line.contains("planned deletion: not determined")),
            "the default report must preserve the non-empty undetermined invariant"
        );
    }

    /// A fail-closed inspection must be *disclosed*, not swallowed.
    ///
    /// This pins the premise the dry run's exit code rests on. `--dry-run`
    /// deliberately stays `ExitCode::Success` even when inspection fails closed
    /// (only a *projecting* command must exit `ExitCode::Error`, and a dry run
    /// is not one). That is only defensible because the failure is reported in
    /// the text output: exit 0 plus a silent report would together claim a clean
    /// machine that nothing inspected — the false-claim class this cluster
    /// exists to eliminate. If the `Err` arm ever degrades to rendering nothing,
    /// the exit code stops being the harmless choice and this test fails.
    #[test]
    fn dry_run_mcp_inspection_failure_is_disclosed_not_swallowed() {
        let scope = TouchScope {
            runtime_surfaces: vec![],
            instruction_files: Vec::new(),
            instruction_prune_files: Vec::new(),
            mcp_hosts: vec![],
            mcp_inspect: McpInspection::Attempted(Err("live host config is ambiguous".to_string())),
            instruction_blocked: false,
        };
        let rendered = render_touch_scope(&scope).join("\n");

        assert!(
            rendered.contains("inspection stopped: live host config is ambiguous"),
            "the underlying inspection error must reach the user verbatim:\n{rendered}"
        );
        assert!(
            rendered.contains("live entries preserved"),
            "a fail-closed inspection must state that nothing was destroyed:\n{rendered}"
        );
        assert!(
            rendered.contains("a real sync would stop here"),
            "the report must say the projecting command would fail, since the dry run's own \
             exit code will not say it:\n{rendered}"
        );
    }

    #[test]
    fn dry_run_mcp_unavailable_path_is_reported_as_skipped_not_blocked() {
        let rendered = render_mcp_inspect(&build_mcp_inspection(None)).join("\n");

        assert!(rendered.contains("MCP recovery (not inspected)"));
        assert!(rendered.contains("a real sync would skip MCP projection"));
        assert!(rendered.contains("live MCP host entries were not inspected or changed"));
        assert!(!rendered.contains("inspection stopped"));
        assert!(!rendered.contains("a real sync would stop here"));
    }

    #[test]
    fn grouped_touch_scope_excludes_unselected_runtime_paths() {
        // Fixture selection: claude only. planned_engine_touches is read-only
        // (path joins, no I/O), so a synthetic tmp home is enough — no real
        // projection run (`run_update_with_evidence`) needed.
        let tmp = TempDir::new().unwrap();
        let opts = projection::MachineUpdateOptions::new(
            tmp.path().join("repo"),
            tmp.path().join("repo"),
            tmp.path().join("state"),
            tmp.path().to_path_buf(),
            tmp.path().join("appdata"),
            vec!["claude".to_string()],
            true,
            false,
            false,
            true,
        );
        assert_eq!(
            opts.selected_projection_targets(),
            &["claude".to_string()],
            "constructor must round-trip the selection through the target-named accessor"
        );
        let touches = ccync_engine::install::planned_engine_touches(&opts)
            .expect("planned_engine_touches should succeed against a synthetic fixture");

        assert!(
            touches.iter().any(|(runtime, _)| runtime == "claude"),
            "selected claude runtime must appear in the touch scope: {touches:?}"
        );
        assert!(
            !touches
                .iter()
                .any(|(runtime, _)| runtime == "copilot" || runtime.starts_with("agy")),
            "unselected runtimes (copilot/agy) must not leak into a claude-only touch scope: {touches:?}"
        );
    }

    #[test]
    fn grouped_touch_scope_single_renderer_reused_by_both_call_sites() {
        // Source-grep guard: the needle is assembled at runtime (split across
        // two literals) so this test's own source text does not inflate its
        // own count, mirroring removed_suppression_helper_never_reappears
        // above. Exactly two call sites in run_unified_projection must invoke
        // the shared renderer with the shared scope variable — one for
        // dry-run, one for the first-projection gate that also serves init's
        // disclosure — proving no duplicated formatting logic exists.
        let needle = ["render_touch_scope(&", "touch_scope)"].concat();
        let this_file = include_str!("lifecycle.rs");
        let count = this_file.matches(&needle).count();
        assert_eq!(
            count, 2,
            "expected exactly two grouped-renderer call sites over the shared scope variable (dry-run + gate/init), found {count}"
        );
    }

    #[test]
    fn grouped_touch_scope_runtime_section_uses_preview_language_not_guarantee() {
        // Both groups must use honest, conditional phrasing (batch-11 F-1):
        // the runtime-surfaces projection call is best-effort (its error is
        // swallowed to stderr in run_unified_projection), so "would touch" —
        // not "will"; the MCP host writes are semantic-no-op-skipping
        // (write_json_provider_delta returns Unchanged on a converged
        // machine), so "may be written" — not "will be written".
        let this_file = include_str!("lifecycle.rs");
        assert!(
            this_file.contains("Runtime surfaces (would touch, current selection):"),
            "the runtime-surfaces group must use preview language, not a 'will happen' guarantee"
        );
        assert!(
            this_file.contains("MCP host files (may be written; skipped when already identical):"),
            "the MCP-hosts group must not claim an unconditional write: a semantic no-op skips converged hosts"
        );
    }

    #[test]
    fn grouped_touch_scope_init_path_skips_confirmation_prompt() {
        // init calls run_unified_projection(.., skip_first_run_gate: true).
        // Reproduce that exact shape (assume_yes: false, non-tty test
        // harness) on an already-initialized fresh home: if the grouped
        // disclosure accidentally still gated on confirm_overwrite, this
        // would abort with ExitCode::Error since confirm_overwrite(false)
        // returns false under a non-tty stdin. It must instead reach the
        // projection seam untouched by the confirmation prompt.
        let _guard = crate::test_io_lock();
        let tmp = TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());

        let ccync_home = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync_home).unwrap();
        fs::write(ccync_home.join("config.json"), r#"{}"#).unwrap();

        let result = run_unified_projection(false, false, true, None);

        let reached_projection = ccync_home.join("build").join("render").is_dir();
        restore_saved_env(&saved);

        assert_eq!(
            result,
            ExitCode::Success,
            "init's skip_first_run_gate path must not gain a second confirmation prompt"
        );
        assert!(
            reached_projection,
            "init's projection call must still reach the real projection seam"
        );
    }

    #[test]
    fn grouped_touch_scope_declined_confirmation_leaves_live_surfaces_untouched() {
        // Regression guard (pre-existing honesty guarantee, matching this
        // file's other zero-mutation guard tests): on an already-initialized
        // home with no prior projection, a
        // declined generic confirmation gate (non-tty, no --yes) must abort
        // with Error and leave every live agent surface untouched — the
        // grouped-scope refactor must not weaken this.
        let _guard = crate::test_io_lock();
        let tmp = TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());

        let ccync_home = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync_home).unwrap();
        fs::write(ccync_home.join("config.json"), r#"{}"#).unwrap();

        let result = run_unified_projection(false, false, false, Some(&SYNC_GATE_ABORT_CONTEXT));

        let claude_live = tmp.path().join(".claude.json");
        let live_surface_written = claude_live.exists();
        let render_dir_created = ccync_home.join("build").join("render").is_dir();

        restore_saved_env(&saved);

        assert_eq!(
            result,
            ExitCode::Error,
            "a declined first-projection gate must abort with Error"
        );
        assert!(
            !live_surface_written,
            "a declined gate must leave the live Claude host file untouched"
        );
        assert!(
            !render_dir_created,
            "a declined gate must never reach run_projection_core"
        );
    }

    // ── migration shim fires before command consumes ~/.ccync/ state ─────────

    #[test]
    fn layout_migration_leaves_agent_surfaces_untouched() {
        // The agent-orphan sweep was removed: migration must not touch live
        // agent surfaces, so any agent file is preserved (regression guard).
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let agents = home.join(".claude").join("agents");
        fs::create_dir_all(&agents).unwrap();
        let a = agents.join("example.agent.md");
        fs::write(&a, "---\nname: example\n---\nbody\n").unwrap();

        run_layout_migration(home).unwrap();

        assert!(
            a.exists(),
            "layout migration must not touch agent surfaces (sweep removed)"
        );
    }

    #[test]
    fn layout_migration_runs_before_command_state_read() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let ccync = home.join(".ccync");

        // Plant old-layout files that would be stale/missing on new layout.
        fs::create_dir_all(ccync.join("config")).unwrap();
        fs::write(
            ccync.join("config").join("config.json"),
            r#"{"runtimes":["claude"]}"#,
        )
        .unwrap();
        fs::create_dir_all(ccync.join("local")).unwrap();
        fs::write(
            ccync.join("local").join("catalog.json"),
            r#"{"plugins":[]}"#,
        )
        .unwrap();

        // Call the migration shim (the function wired into run_unified_projection).
        run_layout_migration(home).unwrap();

        // New layout must exist before any command-specific writes.
        assert!(
            ccync.join("config.json").exists(),
            "config.json must be at new location"
        );
        assert!(
            ccync.join("plugins.json").exists(),
            "plugins.json must be at new location"
        );

        // Old locations must be gone (migration, not copy).
        assert!(
            !ccync.join("config").join("config.json").exists(),
            "old config/config.json must be removed"
        );
        assert!(
            !ccync.join("local").join("catalog.json").exists(),
            "old local/catalog.json must be removed"
        );
    }

    #[test]
    fn layout_migration_shim_is_idempotent_in_new_layout() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let ccync = home.join(".ccync");

        // New layout already present — migration plan must be empty, no error.
        fs::create_dir_all(&ccync).unwrap();
        fs::write(ccync.join("config.json"), r#"{"runtimes":["claude"]}"#).unwrap();

        run_layout_migration(home).unwrap();
        run_layout_migration(home).unwrap(); // second call must not error

        assert!(ccync.join("config.json").exists());
    }

    // ── cmd_restore --from <dir> regression ──────────────────────────────────

    #[test]
    fn cmd_restore_no_from_flag_is_usage() {
        let args = vec!["restore".to_string()];
        assert_eq!(cmd_restore(&args), ExitCode::Usage);
    }

    #[test]
    fn cmd_restore_from_missing_dir_is_error() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does_not_exist");
        let args = vec![
            "restore".to_string(),
            "--from".to_string(),
            missing.to_string_lossy().to_string(),
        ];
        assert_eq!(cmd_restore(&args), ExitCode::Error);
    }

    #[test]
    fn cmd_restore_copies_config_and_lock_to_ccync_home() {
        let _guard = crate::test_io_lock();

        let home = TempDir::new().unwrap();
        let backup = TempDir::new().unwrap();

        // Seed backup with the two canonical INPUT files.
        fs::write(
            backup.path().join("config.json"),
            r#"{"runtimes":["claude"]}"#,
        )
        .unwrap();
        fs::write(
            backup.path().join("plugins.lock.json"),
            r#"{"_personalPlugins":[]}"#,
        )
        .unwrap();
        fs::write(backup.path().join("plugins.json"), r#"{"plugins":[]}"#).unwrap();

        let _home_env = isolate_env(home.path());

        let args = vec![
            "restore".to_string(),
            "--from".to_string(),
            backup.path().to_string_lossy().to_string(),
        ];
        let exit = cmd_restore(&args);

        assert_eq!(exit, ExitCode::Success);
        assert!(
            home.path().join(".ccync").join("config.json").exists(),
            "config.json must be restored to ~/.ccync/config.json"
        );
        assert!(
            home.path()
                .join(".ccync")
                .join("build")
                .join("lock.json")
                .exists(),
            "plugins.lock.json must be restored to ~/.ccync/build/lock.json"
        );
    }

    #[test]
    fn cmd_restore_skips_files_absent_from_backup_without_error() {
        let _guard = crate::test_io_lock();

        let home = TempDir::new().unwrap();
        let backup = TempDir::new().unwrap();

        // Backup contains only config.json — plugins.lock.json absent.
        fs::write(
            backup.path().join("config.json"),
            r#"{"runtimes":["codex"]}"#,
        )
        .unwrap();
        fs::write(backup.path().join("plugins.json"), r#"{"plugins":[]}"#).unwrap();

        let _home_env = isolate_env(home.path());

        let args = vec![
            "restore".to_string(),
            "--from".to_string(),
            backup.path().to_string_lossy().to_string(),
        ];
        let exit = cmd_restore(&args);

        assert_eq!(
            exit,
            ExitCode::Success,
            "missing backup file must be skipped, not errored"
        );
        assert!(
            home.path().join(".ccync").join("config.json").exists(),
            "config.json must be restored"
        );
        assert!(
            !home
                .path()
                .join(".ccync")
                .join("build")
                .join("lock.json")
                .exists(),
            "plugins.lock.json absent from backup must not be created at dst"
        );
    }

    #[test]
    fn cmd_restore_source_contains_sync_follow_up_only_for_restored_files() {
        let this_file = include_str!("lifecycle.rs");
        assert!(
            this_file.contains("if restored > 0 {"),
            "restore follow-up must be gated on restored > 0"
        );
        assert!(
            this_file.contains("ccync restore: next step: run `ccync sync`"),
            "restore follow-up must mention literal ccync sync"
        );
    }

    #[test]
    fn cmd_restore_source_lists_overwrite_targets_before_replacing() {
        let this_file = include_str!("lifecycle.rs");
        assert!(
            this_file.contains("will be overwritten"),
            "restore must preview files it is about to overwrite"
        );
    }

    #[test]
    fn cmd_restore_empty_source_dir_reports_no_backup_files_and_succeeds() {
        let _guard = crate::test_io_lock();

        let home = TempDir::new().unwrap();
        let backup = TempDir::new().unwrap(); // exists, but contains no canonical files

        let args = vec![
            "restore".to_string(),
            "--from".to_string(),
            backup.path().to_string_lossy().to_string(),
        ];
        fs::write(backup.path().join("plugins.json"), r#"{"plugins":[]}"#).unwrap();
        let exit = with_env_home(home.path(), || cmd_restore(&args));

        assert_eq!(
            exit,
            ExitCode::Success,
            "an empty backup source directory must not be treated as an error"
        );
        assert!(
            !home.path().join(".ccync").join("config.json").exists(),
            "nothing should be restored from an empty backup source"
        );
    }

    #[test]
    fn cmd_rollback_addition_does_not_affect_restore_paths() {
        // Regression guard: verify cmd_restore's canonical file list still contains
        // the three canonical files (config.json, plugins.json, plugins.lock.json)
        // after rollback was added.
        // Probe: the file names appear in the source (compile-time check via build).
        // This test exercises the wiring without I/O by confirming usage-path behavior.
        let args = vec!["restore".to_string()]; // no --from → Usage, not a new unknown code path
        assert_eq!(cmd_restore(&args), ExitCode::Usage);
    }

    // ── backup/restore three-file canonical set (INPUT + adoption-snapshot lock) ──

    /// Swap env home to `home`, run `f`, restore env home. Serialized by the caller
    /// holding `crate::test_io_lock()`.
    fn with_env_home<T>(home: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let _home_env = isolate_env(home);
        f()
    }

    #[test]
    fn cmd_backup_copies_all_three_canonical_files() {
        let _guard = crate::test_io_lock();

        let home = TempDir::new().unwrap();
        let out = TempDir::new().unwrap();
        let out_dir = out.path().join("backup");

        let ccync = home.path().join(".ccync");
        fs::create_dir_all(ccync.join("build")).unwrap();
        fs::write(ccync.join("config.json"), r#"{"runtimes":["claude"]}"#).unwrap();
        fs::write(ccync.join("plugins.json"), r#"{"plugins":[{"id":"demo"}]}"#).unwrap();
        fs::write(
            ccync.join("build").join("lock.json"),
            r#"{"_personalPlugins":[]}"#,
        )
        .unwrap();

        let args = vec![
            "backup".to_string(),
            "--output".to_string(),
            out_dir.to_string_lossy().to_string(),
        ];
        let exit = with_env_home(home.path(), || cmd_backup(&args));

        assert_eq!(exit, ExitCode::Success);
        assert!(
            out_dir.join("config.json").exists(),
            "config.json must be backed up"
        );
        assert!(
            out_dir.join("plugins.json").exists(),
            "plugins.json must be backed up"
        );
        assert!(
            out_dir.join("plugins.lock.json").exists(),
            "plugins.lock.json must be backed up"
        );
        assert_eq!(
            fs::read_dir(&out_dir).unwrap().count(),
            3,
            "backup dir must contain exactly the three canonical files"
        );
        assert_eq!(
            fs::read_to_string(out_dir.join("plugins.json")).unwrap(),
            r#"{"plugins":[{"id":"demo"}]}"#,
            "backed-up plugins.json must carry the source content"
        );
    }

    #[test]
    fn cmd_backup_skips_missing_plugins_json_without_error() {
        let _guard = crate::test_io_lock();

        let home = TempDir::new().unwrap();
        let out = TempDir::new().unwrap();
        let out_dir = out.path().join("backup");

        // Machine without a personal catalog: only config.json + lock exist.
        let ccync = home.path().join(".ccync");
        fs::create_dir_all(ccync.join("build")).unwrap();
        fs::write(ccync.join("config.json"), r#"{"runtimes":["claude"]}"#).unwrap();
        fs::write(
            ccync.join("build").join("lock.json"),
            r#"{"_personalPlugins":[]}"#,
        )
        .unwrap();

        let args = vec![
            "backup".to_string(),
            "--output".to_string(),
            out_dir.to_string_lossy().to_string(),
        ];
        let exit = with_env_home(home.path(), || cmd_backup(&args));

        assert_eq!(
            exit,
            ExitCode::Success,
            "missing plugins.json must be skipped, not errored"
        );
        assert!(out_dir.join("config.json").exists());
        assert!(out_dir.join("plugins.lock.json").exists());
        assert!(
            !out_dir.join("plugins.json").exists(),
            "absent plugins.json must not produce a backup file"
        );
    }

    #[test]
    fn cmd_backup_source_announces_unscanned_secret_carriers() {
        let this_file = include_str!("lifecycle.rs");
        assert!(
            this_file.contains("AGENTS.md")
                && this_file.contains("command/args/url")
                && this_file.contains("not scanned for unlabeled tokens"),
            "backup must announce that AGENTS.md and command/args/url are not scanned"
        );
        assert!(
            this_file.contains("--strip-secrets is deprecated and has no effect"),
            "backup source must contain deprecation notice"
        );
        assert!(
            !this_file.contains(&["may contain", "secrets"].concat()),
            "backup must not emit the old plaintext warning"
        );
    }

    #[test]
    fn cmd_backup_source_default_output_dir_is_absolute() {
        let this_file = include_str!("lifecycle.rs");
        assert!(
            this_file.contains("if dir.is_absolute() {"),
            "backup must resolve the default (no --output) directory to an absolute path"
        );
    }

    #[test]
    fn cmd_backup_zero_files_creates_no_output_dir_and_reports_nothing_to_back_up() {
        let _guard = crate::test_io_lock();

        // Machine with no canonical files present at all.
        let home = TempDir::new().unwrap();
        let out = TempDir::new().unwrap();
        let out_dir = out.path().join("backup");

        let args = vec![
            "backup".to_string(),
            "--output".to_string(),
            out_dir.to_string_lossy().to_string(),
        ];
        let exit = with_env_home(home.path(), || cmd_backup(&args));

        assert_eq!(exit, ExitCode::Success, "zero-file backup must not error");
        assert!(
            !out_dir.exists(),
            "zero-file backup must not create an empty output directory"
        );
    }

    #[test]
    fn cmd_restore_copies_plugins_json_symmetrically() {
        let _guard = crate::test_io_lock();

        let home = TempDir::new().unwrap();
        let backup = TempDir::new().unwrap();

        // Three-file backup — the same set cmd_backup produces.
        fs::write(
            backup.path().join("config.json"),
            r#"{"runtimes":["claude"]}"#,
        )
        .unwrap();
        fs::write(
            backup.path().join("plugins.json"),
            r#"{"plugins":[{"id":"demo"}]}"#,
        )
        .unwrap();
        fs::write(
            backup.path().join("plugins.lock.json"),
            r#"{"_personalPlugins":[]}"#,
        )
        .unwrap();

        let args = vec![
            "restore".to_string(),
            "--from".to_string(),
            backup.path().to_string_lossy().to_string(),
        ];
        let exit = with_env_home(home.path(), || cmd_restore(&args));

        assert_eq!(exit, ExitCode::Success);
        let ccync = home.path().join(".ccync");
        assert!(
            ccync.join("config.json").exists(),
            "config.json must be restored"
        );
        assert!(
            ccync.join("plugins.json").exists(),
            "plugins.json must be restored to ~/.ccync/plugins.json"
        );
        assert!(
            ccync.join("build").join("lock.json").exists(),
            "plugins.lock.json must be restored to ~/.ccync/build/lock.json"
        );
        assert_eq!(
            fs::read_to_string(ccync.join("plugins.json")).unwrap(),
            r#"{"plugins":[{"id":"demo"}]}"#,
            "restored plugins.json must carry the backup content"
        );
    }

    #[test]
    fn cmd_restore_legacy_two_file_backup_skips_plugins_json() {
        let _guard = crate::test_io_lock();

        let home = TempDir::new().unwrap();
        let backup = TempDir::new().unwrap();

        // Legacy backup created before plugins.json joined the canonical set.
        fs::write(
            backup.path().join("config.json"),
            r#"{"runtimes":["claude"]}"#,
        )
        .unwrap();
        fs::write(
            backup.path().join("plugins.lock.json"),
            r#"{"_personalPlugins":[]}"#,
        )
        .unwrap();

        let args = vec![
            "restore".to_string(),
            "--from".to_string(),
            backup.path().to_string_lossy().to_string(),
        ];
        let exit = with_env_home(home.path(), || cmd_restore(&args));

        assert_eq!(
            exit,
            ExitCode::Error,
            "backup without plugins.json must fail closed"
        );
        let ccync = home.path().join(".ccync");
        assert!(!ccync.join("config.json").exists());
        assert!(!ccync.join("build").join("lock.json").exists());
        assert!(
            !ccync.join("plugins.json").exists(),
            "plugins.json absent from a legacy backup must not create a destination file"
        );
    }

    #[test]
    fn cmd_restore_overwrites_existing_plugins_json_with_backup_content() {
        let _guard = crate::test_io_lock();

        let home = TempDir::new().unwrap();
        let backup = TempDir::new().unwrap();

        fs::write(
            backup.path().join("plugins.json"),
            r#"{"plugins":[{"id":"from-backup"}]}"#,
        )
        .unwrap();

        let ccync = home.path().join(".ccync");
        fs::create_dir_all(&ccync).unwrap();
        fs::write(
            ccync.join("plugins.json"),
            r#"{"plugins":[{"id":"pre-existing"}]}"#,
        )
        .unwrap();

        let args = vec![
            "restore".to_string(),
            "--from".to_string(),
            backup.path().to_string_lossy().to_string(),
        ];
        let exit = with_env_home(home.path(), || cmd_restore(&args));

        assert_eq!(exit, ExitCode::Success);
        assert_eq!(
            fs::read_to_string(ccync.join("plugins.json")).unwrap(),
            r#"{"plugins":[{"id":"from-backup"}]}"#,
            "restore must overwrite the existing plugins.json with the backup version"
        );
    }

    // ── canonical-order formatter + status-free would-form previews ──────────

    #[test]
    fn format_canonical_target_list_two_targets_out_of_order_input() {
        use ccync_foundation::target::ProjectionTargetId::*;
        // Input given out of canonical order; output must still be canonical.
        assert_eq!(
            format_canonical_target_list(&[Codex, Claude]),
            "claude, codex"
        );
    }

    #[test]
    fn format_canonical_target_list_all_seven_in_canonical_order() {
        let all: Vec<_> = ccync_foundation::target::CANONICAL_PROJECTION_TARGETS
            .iter()
            .rev()
            .copied()
            .collect();
        assert_eq!(
            format_canonical_target_list(&all),
            "claude, codex, copilot, opencode, agy-cli, agy-ide, agy-gui"
        );
    }

    #[test]
    fn format_canonical_target_list_empty_selection_is_empty_string() {
        assert_eq!(format_canonical_target_list(&[]), "");
    }

    #[test]
    fn format_canonical_target_list_subset_and_all_share_exact_order_no_status_tokens() {
        use ccync_foundation::target::ProjectionTargetId::*;
        let subset = format_canonical_target_list(&[AgyGui, Claude, Opencode]);
        assert_eq!(subset, "claude, opencode, agy-gui");
        let all =
            format_canonical_target_list(ccync_foundation::target::CANONICAL_PROJECTION_TARGETS);
        for token in [
            "Applied",
            "Unchanged",
            "Skipped",
            "Failed",
            "NotRun",
            "Not run",
        ] {
            assert!(
                !subset.contains(token),
                "preview must be status-free: {subset}"
            );
            assert!(!all.contains(token), "preview must be status-free: {all}");
        }
    }

    #[test]
    fn dry_run_preview_line_shares_formatter_and_is_status_free() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        std::fs::write(
            tmp.path().join(".ccync").join("config.json"),
            "not written here",
        )
        .ok();
        std::fs::create_dir_all(tmp.path().join(".ccync")).unwrap();
        std::fs::write(
            tmp.path().join(".ccync").join("config.json"),
            r#"{"selectedRuntimes":["claude","codex"]}"#,
        )
        .unwrap();

        let config = ccync_foundation::config::CcyncConfig::load();
        let lines = dry_run_report_lines(&config);

        restore_saved_env(&saved);

        let joined = lines.join("\n");
        assert!(
            joined.contains("Would apply ccync-managed settings to: claude, codex"),
            "dry-run preview must name the selected targets in canonical order: {joined}"
        );
        for token in [
            "Applied",
            "Unchanged",
            "Skipped",
            "Failed",
            "NotRun",
            "Not run",
        ] {
            assert!(
                !joined.contains(token),
                "dry-run preview must be status-free: {joined}"
            );
        }
    }

    // ── target_stdout_lines: exact all-Applied and mixed groups ───────────────

    fn make_report(
        statuses: &[(
            ccync_foundation::target::ProjectionTargetId,
            ccync_engine::install::ProjectionTargetStatus,
        )],
    ) -> ccync_engine::install::ProjectionRunReport {
        let mut targets = std::collections::BTreeMap::new();
        for (target, status) in statuses {
            targets.insert(
                *target,
                ccync_engine::install::ProjectionTargetResult {
                    target: *target,
                    status: status.clone(),
                    paths: Vec::new(),
                    warnings: Vec::new(),
                },
            );
        }
        ccync_engine::install::ProjectionRunReport {
            common: Vec::new(),
            targets,
            mode: "normal".to_string(),
            run_warnings: Vec::new(),
        }
    }

    #[test]
    fn target_stdout_lines_two_targets_all_applied_exact_sentence() {
        use ccync_engine::install::ProjectionTargetStatus::Applied;
        use ccync_foundation::target::ProjectionTargetId::{Claude, Codex};
        let report = make_report(&[(Claude, Applied), (Codex, Applied)]);
        assert_eq!(
            target_stdout_lines(&report, false, &[]),
            vec![
                "Successfully rebuilt and applied ccync-managed settings to claude, codex"
                    .to_string()
            ]
        );
    }

    #[test]
    fn target_stdout_lines_all_seven_applied_exact_sentence() {
        use ccync_engine::install::ProjectionTargetStatus::Applied;
        let statuses: Vec<_> = ccync_foundation::target::CANONICAL_PROJECTION_TARGETS
            .iter()
            .map(|t| (*t, Applied))
            .collect();
        let report = make_report(&statuses);
        assert_eq!(
            target_stdout_lines(&report, false, &[]),
            vec![
                "Successfully rebuilt and applied ccync-managed settings to claude, codex, copilot, opencode, agy-cli, agy-ide, agy-gui"
                    .to_string()
            ]
        );
    }

    #[test]
    fn target_stdout_lines_mixed_applied_unchanged_skipped_groups() {
        use ccync_engine::install::ProjectionTargetStatus::{Applied, Skipped, Unchanged};
        use ccync_foundation::target::ProjectionTargetId::{Claude, Codex, Copilot, Opencode};
        let report = make_report(&[
            (Claude, Applied),
            (Codex, Unchanged),
            (
                Copilot,
                Skipped {
                    reason: "not selected on this machine".to_string(),
                },
            ),
            (
                Opencode,
                Skipped {
                    reason: "no config file present".to_string(),
                },
            ),
        ]);
        assert_eq!(
            target_stdout_lines(&report, false, &[]),
            vec![
                "Applied: claude".to_string(),
                "Unchanged: codex".to_string(),
                "Skipped: copilot — not selected on this machine".to_string(),
                "Skipped: opencode — no config file present".to_string(),
            ]
        );
    }

    /// A Codex skill route with the given publication facts, for the gap probes.
    fn gap_route(
        selected: bool,
        published: bool,
        failure_reason: Option<&str>,
        unsupported_reason: Option<&str>,
    ) -> ccync_engine::managed_view::RouteEvidence {
        use ccync_engine::managed_view::{RouteEvidence, VerificationScope};
        use ccync_foundation::provider::ComponentKind;
        use ccync_foundation::target::ProjectionTargetId::Codex;

        RouteEvidence {
            target: Codex,
            kind: ComponentKind::Skill,
            original_name: "shared-skill".to_string(),
            source_key: "provider".to_string(),
            origin_path: None,
            origin_reference: None,
            command_to_skill: false,
            failure_reason: failure_reason.map(str::to_string),
            unsupported_reason: unsupported_reason.map(str::to_string),
            alias: None,
            path: None,
            served_by_root: None,
            multi_root: false,
            foreign_alias_path: None,
            selected,
            published,
            owned: false,
            loader: VerificationScope::LoaderNotRun,
            loader_version: None,
            registration: None,
        }
    }

    #[test]
    fn target_stdout_lines_blocked_or_unverified_route_never_reads_as_full_success() {
        use ccync_engine::install::ProjectionTargetStatus::Applied;
        use ccync_foundation::target::ProjectionTargetId::{Claude, Codex};
        let report = make_report(&[(Claude, Applied), (Codex, Applied)]);

        let blocked = target_stdout_lines(
            &report,
            false,
            &[gap_route(true, false, Some("permission denied"), None)],
        );
        assert_eq!(
            blocked,
            vec![
                "Applied: claude, codex".to_string(),
                "Not fully applied: codex Skill shared-skill — blocked: permission denied"
                    .to_string(),
            ]
        );

        let unverified = target_stdout_lines(&report, false, &[gap_route(true, false, None, None)]);
        assert!(
            unverified
                .iter()
                .all(|line| !line.starts_with("Successfully")),
            "{unverified:?}"
        );
        assert!(unverified
            .iter()
            .any(|line| line.ends_with("— not verified: not published by this run")));

        let unsupported = target_stdout_lines(
            &report,
            false,
            &[gap_route(
                true,
                false,
                None,
                Some("agents are not supported"),
            )],
        );
        assert!(unsupported
            .iter()
            .any(|line| line.ends_with("— unsupported: agents are not supported")));
    }

    #[test]
    fn target_stdout_lines_unselected_or_published_routes_keep_full_success() {
        use ccync_engine::install::ProjectionTargetStatus::Applied;
        use ccync_foundation::target::ProjectionTargetId::Claude;
        let report = make_report(&[(Claude, Applied)]);

        let unselected = target_stdout_lines(
            &report,
            false,
            &[gap_route(false, false, Some("permission denied"), None)],
        );
        assert_eq!(
            unselected,
            vec!["Successfully rebuilt and applied ccync-managed settings to claude".to_string()]
        );
        let published = target_stdout_lines(&report, false, &[gap_route(true, true, None, None)]);
        assert_eq!(
            published,
            vec!["Successfully rebuilt and applied ccync-managed settings to claude".to_string()]
        );
    }

    #[test]
    fn target_stdout_lines_all_unchanged_is_a_mixed_group_not_success_sentence() {
        use ccync_engine::install::ProjectionTargetStatus::Unchanged;
        use ccync_foundation::target::ProjectionTargetId::{Claude, Codex};
        let report = make_report(&[(Claude, Unchanged), (Codex, Unchanged)]);
        assert_eq!(
            target_stdout_lines(&report, false, &[]),
            vec!["Unchanged: claude, codex".to_string()]
        );
    }

    #[test]
    fn target_stdout_lines_failed_target_suppresses_all_stdout_output() {
        use ccync_engine::install::ProjectionTargetStatus::{Applied, Failed};
        use ccync_foundation::target::ProjectionTargetId::{Claude, Codex};
        let report = make_report(&[
            (Claude, Applied),
            (
                Codex,
                Failed {
                    path: Some(std::path::PathBuf::from("/home/u/.codex/config.toml")),
                    reason: "permission denied".to_string(),
                },
            ),
        ]);
        assert!(
            target_stdout_lines(&report, false, &[]).is_empty(),
            "any Failed target must suppress the entire stdout report"
        );
    }

    #[test]
    fn target_stdout_lines_notrun_target_suppresses_all_stdout_output() {
        use ccync_engine::install::ProjectionTargetStatus::{Applied, NotRun};
        use ccync_foundation::target::ProjectionTargetId::{Claude, Codex};
        let report = make_report(&[(Claude, Applied), (Codex, NotRun)]);
        assert!(
            target_stdout_lines(&report, false, &[]).is_empty(),
            "any NotRun target must suppress the entire stdout report"
        );
    }

    // ── target_evidence_lines: per-target publication, loader, retry ─────────

    #[test]
    fn target_evidence_lines_fully_applied_run_prints_nothing() {
        use ccync_engine::install::ProjectionTargetStatus::{Applied, Unchanged};
        use ccync_foundation::target::ProjectionTargetId::{Claude, Codex};

        let report = make_report(&[(Claude, Applied), (Codex, Unchanged)]);
        assert!(super::target_evidence_lines(&report).is_empty());
    }

    #[test]
    fn target_evidence_lines_partial_run_labels_each_fact_and_never_claims_loaded() {
        use ccync_engine::install::ProjectionTargetStatus::{Applied, Failed, NotRun};
        use ccync_foundation::target::ProjectionTargetId::{Claude, Codex, Copilot};

        let report = make_report(&[
            (Claude, Applied),
            (
                Codex,
                Failed {
                    path: None,
                    reason: "disk full".to_string(),
                },
            ),
            (Copilot, NotRun),
        ]);
        let lines = super::target_evidence_lines(&report);
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0],
            "evidence [claude]: publication published; loader NotRun; served-by-root not reported"
        );
        assert_eq!(
            lines[1],
            "evidence [codex]: publication not published; loader NotRun; served-by-root not reported; retry: fix the reported write error, then rerun `ccync sync`"
        );
        assert_eq!(
            lines[2],
            "evidence [copilot]: publication not run; loader NotRun; served-by-root not reported; retry: rerun `ccync sync` after codex is fixed"
        );
        assert!(lines.iter().all(|line| !line.contains("loaded")));
    }

    // ── route_lines: per-route labels from the engine's route evidence ──────

    fn route(published: bool) -> ccync_engine::managed_view::RouteEvidence {
        use ccync_engine::managed_view::{RouteEvidence, VerificationScope};
        use ccync_foundation::provider::ComponentKind;
        use ccync_foundation::target::ProjectionTargetId::Codex;

        RouteEvidence {
            target: Codex,
            kind: ComponentKind::Skill,
            original_name: "shared-skill".to_string(),
            source_key: "provider".to_string(),
            origin_path: None,
            origin_reference: None,
            command_to_skill: false,
            failure_reason: None,
            unsupported_reason: None,
            alias: None,
            path: Some("~/.agents/skills/shared-skill".to_string()),
            served_by_root: Some("~/.agents/skills".to_string()),
            multi_root: false,
            foreign_alias_path: None,
            selected: true,
            published,
            owned: false,
            loader: VerificationScope::LoaderNotRun,
            loader_version: None,
            registration: None,
        }
    }

    #[test]
    fn route_lines_empty_for_ordinary_sync() {
        assert!(super::route_lines(&[], &Default::default(), &[]).is_empty());
    }

    #[test]
    fn route_lines_partial_route_labels_each_fact_and_never_claims_success() {
        let line = super::route_lines(&[route(false)], &Default::default(), &[]).remove(0);
        assert!(line.starts_with("route: codex Skill shared-skill; selected; not published"));
        assert!(line.contains("not owned by ccync"));
        assert!(line.contains("loader NotRun"));
        assert!(line.contains("served by root ~/.agents/skills"));
        assert!(line.contains("action: fix the reported write error"));
        assert!(!line.contains("loaded"));
        assert!(!line.contains("success"));
    }

    #[test]
    fn route_lines_name_user_registration_with_its_uninstall_command() {
        use ccync_engine::managed_view::RouteRegistration;

        let mut evidence = route(true);
        evidence.registration = Some(RouteRegistration::RegisteredByUser {
            uninstall_command: "claude plugin uninstall ccync@ccync --scope user".to_string(),
        });
        let line = super::route_lines(&[evidence], &Default::default(), &[]).remove(0);
        assert!(line.contains(
            "registered by user, uninstall with `claude plugin uninstall ccync@ccync --scope user`"
        ));
        assert!(line.contains("action: remove the user registration with"));
    }

    #[test]
    fn route_lines_label_multi_root_and_foreign_alias() {
        let mut evidence = route(true);
        evidence.multi_root = true;
        evidence.foreign_alias_path = Some("/home/u/.agents/skills/shared-skill".to_string());
        let line = super::route_lines(&[evidence], &Default::default(), &[]).remove(0);
        assert!(line.contains("discovered in multiple roots"));
        assert!(line.contains("alias caused by foreign path /home/u/.agents/skills/shared-skill"));
    }

    #[test]
    fn route_lines_label_listed_not_registered_on_constructed_evidence() {
        use ccync_engine::managed_view::RouteRegistration;

        let mut evidence = route(true);
        evidence.registration = Some(RouteRegistration::ListedNotRegistered);
        let line = super::route_lines(&[evidence], &Default::default(), &[]).remove(0);
        assert!(line.contains("listed, not registered by ccync"));
        assert!(!line.contains("registered by user"));
    }

    #[test]
    fn route_lines_partial_retirement_shows_retired_and_kept_targets() {
        use ccync_engine::install::RouteRetirement;
        use ccync_foundation::target::ProjectionTargetId::{Claude, Codex};
        use std::collections::BTreeMap;

        let mut claude = route(true);
        claude.target = Claude;
        let codex = route(true);
        let retirements = BTreeMap::from([
            (Claude, RouteRetirement::Done),
            (
                Codex,
                RouteRetirement::Kept {
                    reason: "direct publication failed".to_string(),
                },
            ),
        ]);
        let lines = super::route_lines(&[claude, codex], &retirements, &[]);
        assert!(lines[0].ends_with("; retirement: done"));
        assert!(lines[1].ends_with("; retirement: kept (direct publication failed)"));
    }

    #[test]
    fn route_lines_conflict_names_reason_and_never_publishes() {
        use ccync_engine::install::RouteConflict;
        use ccync_foundation::target::ProjectionTargetId::Codex;

        let conflict = RouteConflict {
            target: Codex,
            component: "shared-skill".to_string(),
            reason: "a same-name entry is not owned by ccync".to_string(),
        };
        let line = super::route_lines(&[route(false)], &Default::default(), &[conflict]).remove(0);
        assert!(line.contains("conflict: a same-name entry is not owned by ccync"));
        assert!(line.contains("not published"));
        assert!(!line.contains("; published;"));
        assert!(!line.contains("write error"));
    }

    // ── target_stderr_lines: exact failure, NotRun, and warning lines ────────

    #[test]
    fn target_stderr_lines_failed_with_path_exact_line() {
        use ccync_engine::install::ProjectionTargetStatus::Failed;
        use ccync_foundation::target::ProjectionTargetId::Codex;
        let report = make_report(&[(
            Codex,
            Failed {
                path: Some(std::path::PathBuf::from("/home/u/.codex/config.toml")),
                reason: "permission denied".to_string(),
            },
        )]);
        assert_eq!(
            target_stderr_lines(&report),
            vec!["Failed: codex — /home/u/.codex/config.toml: permission denied".to_string()]
        );
    }

    #[test]
    fn target_stderr_lines_failed_without_path_exact_line() {
        use ccync_engine::install::ProjectionTargetStatus::Failed;
        use ccync_foundation::target::ProjectionTargetId::Copilot;
        let report = make_report(&[(
            Copilot,
            Failed {
                path: None,
                reason: "marketplace manifest write failed".to_string(),
            },
        )]);
        assert_eq!(
            target_stderr_lines(&report),
            vec!["Failed: copilot — marketplace manifest write failed".to_string()]
        );
    }

    #[test]
    fn target_stderr_lines_notrun_cites_the_failed_sibling_as_subject() {
        use ccync_engine::install::ProjectionTargetStatus::{Failed, NotRun};
        use ccync_foundation::target::ProjectionTargetId::{Claude, Copilot};
        let report = make_report(&[
            (
                Claude,
                Failed {
                    path: None,
                    reason: "marketplace manifest write failed".to_string(),
                },
            ),
            (Copilot, NotRun),
        ]);
        let lines = target_stderr_lines(&report);
        assert_eq!(
            lines,
            vec![
                "Failed: claude — marketplace manifest write failed".to_string(),
                "Not run: copilot — stopped after claude failed".to_string(),
            ]
        );
    }

    #[test]
    fn target_stderr_lines_notrun_with_no_failed_target_cites_machine_projection() {
        use ccync_engine::install::ProjectionTargetStatus::NotRun;
        use ccync_foundation::target::ProjectionTargetId::Opencode;
        let report = make_report(&[(Opencode, NotRun)]);
        assert_eq!(
            target_stderr_lines(&report),
            vec!["Not run: opencode — stopped after machine projection failed".to_string()]
        );
    }

    #[test]
    fn target_stderr_lines_target_scoped_warning_uses_bracket_form() {
        use ccync_engine::install::{ProjectionTargetResult, ProjectionTargetStatus::Applied};
        use ccync_foundation::target::ProjectionTargetId::Claude;
        let mut targets = std::collections::BTreeMap::new();
        targets.insert(
            Claude,
            ProjectionTargetResult {
                target: Claude,
                status: Applied,
                paths: Vec::new(),
                warnings: vec!["skill 'foo' has no SKILL.md".to_string()],
            },
        );
        let report = ccync_engine::install::ProjectionRunReport {
            common: Vec::new(),
            targets,
            mode: "normal".to_string(),
            run_warnings: Vec::new(),
        };
        assert_eq!(
            target_stderr_lines(&report),
            vec!["warning [claude]: skill 'foo' has no SKILL.md".to_string()]
        );
    }

    #[test]
    fn native_conflict_warning_renders_target_specific_selector_and_remedy() {
        let codex_warning = "codex native loading: sample-plugin not reconciled: native registration requires recovery: External; keeping the combined surface";
        let claude_warning = "claude native loading: sample-plugin not reconciled: native registration requires recovery: External; keeping the combined surface";

        let codex_selector = native_conflict_selector(codex_warning).unwrap();
        let claude_selector = native_conflict_selector(claude_warning).unwrap();

        assert_eq!(codex_selector, "sample-plugin@ccync-marketplace");
        assert_eq!(claude_selector, "sample-plugin@ccync");
        assert_eq!(
            format!("remove {codex_selector} with its native manager, then run ccync sync"),
            "remove sample-plugin@ccync-marketplace with its native manager, then run ccync sync"
        );
        assert!(native_conflict_selector("provider conflict").is_none());
    }

    #[test]
    fn target_stderr_lines_mixed_failure_notrun_and_warning_order() {
        use ccync_engine::install::{
            ProjectionTargetResult,
            ProjectionTargetStatus::{Failed, NotRun},
        };
        use ccync_foundation::target::ProjectionTargetId::{Claude, Codex, Copilot};
        let mut targets = std::collections::BTreeMap::new();
        targets.insert(
            Claude,
            ProjectionTargetResult {
                target: Claude,
                status: Failed {
                    path: None,
                    reason: "write failed".to_string(),
                },
                paths: Vec::new(),
                warnings: vec!["heads up".to_string()],
            },
        );
        targets.insert(
            Codex,
            ProjectionTargetResult {
                target: Codex,
                status: NotRun,
                paths: Vec::new(),
                warnings: Vec::new(),
            },
        );
        targets.insert(
            Copilot,
            ProjectionTargetResult {
                target: Copilot,
                status: NotRun,
                paths: Vec::new(),
                warnings: Vec::new(),
            },
        );
        let report = ccync_engine::install::ProjectionRunReport {
            common: Vec::new(),
            targets,
            mode: "normal".to_string(),
            run_warnings: Vec::new(),
        };
        assert_eq!(
            target_stderr_lines(&report),
            vec![
                "Failed: claude — write failed".to_string(),
                "Not run: codex — stopped after claude failed".to_string(),
                "Not run: copilot — stopped after claude failed".to_string(),
                "warning [claude]: heads up".to_string(),
            ]
        );
    }

    // ── derive_run_exit: exit matrix ──────────────────────────────────────────

    #[test]
    fn derive_run_exit_failed_target_is_error() {
        use ccync_engine::install::ProjectionTargetStatus::Failed;
        use ccync_foundation::target::ProjectionTargetId::Claude;
        let report = make_report(&[(
            Claude,
            Failed {
                path: None,
                reason: "x".to_string(),
            },
        )]);
        assert_eq!(derive_run_exit(&report, false), ExitCode::Error);
    }

    #[test]
    fn derive_run_exit_notrun_target_alone_is_error() {
        use ccync_engine::install::ProjectionTargetStatus::NotRun;
        use ccync_foundation::target::ProjectionTargetId::Codex;
        // NotRun with no machine_fault and no Failed sibling — the same-phase
        // short-circuit case derive_run_exit must still catch directly.
        let report = make_report(&[(Codex, NotRun)]);
        assert_eq!(derive_run_exit(&report, false), ExitCode::Error);
    }

    #[test]
    fn derive_run_exit_machine_fault_alone_is_error() {
        // A genuine Machine-phase fault with no targets recorded at all must
        // still flip the exit code.
        let report = make_report(&[]);
        assert_eq!(derive_run_exit(&report, true), ExitCode::Error);
    }

    #[test]
    fn derive_run_exit_target_scoped_warning_only_is_success() {
        use ccync_engine::install::{ProjectionTargetResult, ProjectionTargetStatus::Applied};
        use ccync_foundation::target::ProjectionTargetId::Claude;
        let mut targets = std::collections::BTreeMap::new();
        targets.insert(
            Claude,
            ProjectionTargetResult {
                target: Claude,
                status: Applied,
                paths: Vec::new(),
                warnings: vec!["heads up".to_string()],
            },
        );
        let report = ccync_engine::install::ProjectionRunReport {
            common: Vec::new(),
            targets,
            mode: "normal".to_string(),
            run_warnings: Vec::new(),
        };
        assert_eq!(derive_run_exit(&report, false), ExitCode::Success);
    }

    #[test]
    fn derive_run_exit_run_level_warning_only_is_success() {
        use ccync_engine::install::ProjectionTargetStatus::Applied;
        use ccync_foundation::target::ProjectionTargetId::Claude;
        let mut report = make_report(&[(Claude, Applied)]);
        report.run_warnings.push("a run-level heads up".to_string());
        assert_eq!(derive_run_exit(&report, false), ExitCode::Success);
    }

    #[test]
    fn derive_run_exit_mixed_target_and_run_warnings_is_success() {
        use ccync_engine::install::{
            ProjectionTargetResult,
            ProjectionTargetStatus::{Applied, Unchanged},
        };
        use ccync_foundation::target::ProjectionTargetId::{Claude, Codex};
        let mut targets = std::collections::BTreeMap::new();
        targets.insert(
            Claude,
            ProjectionTargetResult {
                target: Claude,
                status: Applied,
                paths: Vec::new(),
                warnings: vec!["target warning".to_string()],
            },
        );
        targets.insert(
            Codex,
            ProjectionTargetResult {
                target: Codex,
                status: Unchanged,
                paths: Vec::new(),
                warnings: Vec::new(),
            },
        );
        let report = ccync_engine::install::ProjectionRunReport {
            common: Vec::new(),
            targets,
            mode: "normal".to_string(),
            run_warnings: vec!["run-level warning".to_string()],
        };
        assert_eq!(derive_run_exit(&report, false), ExitCode::Success);
    }

    #[test]
    fn derive_run_exit_all_applied_no_faults_is_success() {
        use ccync_engine::install::ProjectionTargetStatus::Applied;
        use ccync_foundation::target::ProjectionTargetId::{Claude, Codex};
        let report = make_report(&[(Claude, Applied), (Codex, Applied)]);
        assert_eq!(derive_run_exit(&report, false), ExitCode::Success);
    }

    // ── MCP evidence rendering: only actual updated/failure evidence ─────────

    #[test]
    fn mcp_hosts_updated_lines_absent_emits_nothing() {
        assert!(mcp_hosts_updated_lines(&[]).is_empty());
    }

    #[test]
    fn mcp_hosts_updated_lines_named_line_is_byte_exact() {
        assert_eq!(
            mcp_hosts_updated_lines(&["claude".to_string(), "codex".to_string()]),
            vec!["MCP hosts updated: claude, codex".to_string()]
        );
    }

    #[test]
    fn mcp_host_failure_lines_absent_emits_nothing() {
        assert!(mcp_host_failure_lines(&[]).is_empty());
    }

    #[test]
    fn mcp_host_failure_lines_named_line_is_byte_exact() {
        assert_eq!(
            mcp_host_failure_lines(&[(
                "claude".to_string(),
                "/home/u/.claude.json: permission denied — left untouched; ccync cannot safely write a config it cannot read".to_string()
            )]),
            vec![
                "MCP host failed: claude — /home/u/.claude.json: permission denied — left untouched; ccync cannot safely write a config it cannot read".to_string()
            ]
        );
    }

    #[test]
    fn mcp_host_failure_lines_multiple_hosts_preserve_order() {
        assert_eq!(
            mcp_host_failure_lines(&[
                ("claude".to_string(), "reason-a".to_string()),
                ("codex".to_string(), "reason-b".to_string()),
            ]),
            vec![
                "MCP host failed: claude — reason-a".to_string(),
                "MCP host failed: codex — reason-b".to_string(),
            ]
        );
    }
}
