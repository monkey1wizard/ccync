//! `ccync upgrade [<plugin>]` — upgrade installed git-source plugins
//! (brew-upgrade semantics).
//!
//! Thin handler: parse args, load the personal-plugin set, build the engine
//! seams (`git ls-remote` / `git clone` subprocesses + `run_unified_projection`),
//! call `ccync_engine::run_upgrade`, print the report, map the exit code.

use ccync_engine::adopt::adopted_items_without_source;
use ccync_engine::tool_manager::{
    self,
    adapters::{self, RemoteUpdateStatus, ToolAdapter},
    lock::LifecycleGuard,
    model::{ToolId, UpdateState},
    probe::SystemProbeRunner,
    store,
};
use ccync_engine::{
    resolve_adopted_upgrades, run_upgrade, AdoptedUpgradeOutcome, ExitCode, UpgradeOpts,
    UpgradeReport,
};
use ccync_foundation::paths::EffectiveLayout;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;

#[allow(dead_code)]
fn upgrade_reload_guidance(changed: bool, agents: &[String]) -> Option<String> {
    changed.then(|| super::tool_output::reload_line(agents))
}

pub(crate) fn cmd_upgrade(args: &[String], guard: Option<&LifecycleGuard>) -> ExitCode {
    if let Err(code) = super::args::reject_unknown_flags(
        "upgrade",
        args,
        &["--help", "-h", "--dry-run", "--all", "--yes", "--json", "--retry-manager-step"],
        &[],
        1,
        "Usage: ccync upgrade [<plugin>|tool:<id>] [--all] [--yes] [--json] [--dry-run] [--retry-manager-step]",
    ) {
        return code;
    }
    let args = &args[1..]; // skip the "upgrade" verb

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("ccync upgrade [<plugin>|tool:<id>] [--all] [--yes] [--json] [--dry-run] [--retry-manager-step]");
        println!();
        println!("Upgrade installed git-source plugins (Homebrew `brew upgrade` semantics).");
        println!("  <plugin>    Upgrade only this plugin id (default: all git plugins).");
        println!("  --dry-run   Show what would be upgraded; clone/pin/project nothing.");
        return ExitCode::Success;
    }

    let dry_run = args.iter().any(|a| a == "--dry-run");
    let retry_manager_step = args.iter().any(|a| a == "--retry-manager-step");
    let all = args.iter().any(|a| a == "--all");
    let targets: Vec<_> = args.iter().filter(|a| !a.starts_with('-')).collect();
    if targets.len() > 1
        || (all && !targets.is_empty())
        || (retry_manager_step && (all || targets.len() != 1 || !targets[0].starts_with("tool:")))
    {
        eprintln!("ccync upgrade: invalid target/flag combination");
        return ExitCode::Usage;
    }
    if targets.first().is_some_and(|s| s.starts_with("tool:")) {
        let raw_id = targets[0].strip_prefix("tool:").unwrap_or_default();
        let id = match ToolId::new(raw_id.to_string()) {
            Ok(id) => id,
            Err(_) => {
                eprintln!("ccync upgrade: invalid tool id");
                return ExitCode::Usage;
            }
        };
        let layout = match EffectiveLayout::load_or_legacy() {
            Ok(layout) => layout,
            Err(error) => {
                eprintln!("ccync upgrade: cannot load storage layout: {error}");
                return ExitCode::Error;
            }
        };
        return upgrade_one_tool(&layout, args, guard, &id, dry_run, retry_manager_step);
    }

    // Machine-initialization preflight: a non-dry upgrade clones/re-pins git
    // plugins and reprojects the managed set to live agent surfaces, so it
    // must not run before `ccync init` — checked here, before adopted
    // resolution or `build_opts`/`run_upgrade` (the clone/pin path), so a
    // refusal is a true zero-mutation abort. `--dry-run` is exempt, mirroring
    // `sync --dry-run`'s exemption from the same preflight.
    if !dry_run {
        if let Err(code) = super::lifecycle::require_initialized_machine() {
            return code;
        }
    }

    let filter_id = if all {
        None
    } else {
        targets.first().map(|s| {
            s.as_str()
                .strip_prefix("plugin:")
                .unwrap_or(s.as_str())
                .to_string()
        })
    };

    let layout = match EffectiveLayout::load_or_legacy() {
        Ok(layout) => layout,
        Err(error) => {
            eprintln!("ccync upgrade: cannot load storage layout: {error}");
            return ExitCode::Error;
        }
    };

    let opts = match build_opts(&layout, filter_id, dry_run) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("ccync upgrade: {e}");
            return ExitCode::Error;
        }
    };
    if all && !dry_run && !confirm_upgrade(args, false) {
        return ExitCode::Usage;
    }

    // Adopted-item classification is read-only reporting: resolved
    // unconditionally, on every invocation, from the same lockfile `Value`
    // already loaded for the personal-plugin set. Never affects the exit code.
    let adopted = resolve_adopted_upgrades(&opts.plugins);
    let unresolved = unresolved_adopted_names(&layout);

    let report = run_upgrade(&opts, git_ls_remote, clone_into_pinned_cache, project);
    let plugin_exit = report_exit(&layout, &report, &adopted, &unresolved, dry_run);
    if all && !dry_run {
        let declarations = match store::load_declarations() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("ccync upgrade: {e}");
                return ExitCode::Error;
            }
        };
        let list: Vec<_> = declarations.values().cloned().collect();
        let eligibility = tool_manager::upgrade::batch_eligibility(&list);
        let mut result = plugin_exit;
        for item in eligibility {
            if !item.eligible {
                println!("unchanged {} ({})", item.id, item.reasons.join(", "));
                continue;
            }
            let yes = vec!["upgrade".to_string(), "--yes".to_string()];
            let code = upgrade_one_tool(&layout, &yes, guard, &item.id, false, false);
            if code != ExitCode::Success {
                result = code;
            }
        }
        return result;
    }
    plugin_exit
}

/// Lock-derived names of adopted items with no recorded source, read
/// via `ccync_engine::adopt::adopted_items_without_source`. Covers every
/// adopted kind (plugin, MCP, skill) uniformly — unlike the removed
/// plugin-only `unresolved_repair_hint`. Graceful-missing: an unresolvable
/// lock path or unreadable/malformed lock file yields an empty vec, never a
/// panic or a false tip.
fn unresolved_adopted_names(layout: &EffectiveLayout) -> Vec<String> {
    adopted_items_without_source(&layout.build_lock_path()).unwrap_or_default()
}

fn upgrade_one_tool(
    layout: &EffectiveLayout,
    args: &[String],
    guard: Option<&LifecycleGuard>,
    id: &ToolId,
    dry_run: bool,
    retry: bool,
) -> ExitCode {
    upgrade_one_tool_internal_with_layout(
        layout,
        args,
        guard,
        id,
        dry_run,
        retry,
        None,
        &SystemProbeRunner,
    )
}

pub(crate) fn upgrade_one_tool_internal(
    args: &[String],
    guard: Option<&LifecycleGuard>,
    id: &ToolId,
    dry_run: bool,
    retry: bool,
    adapters_override: Option<&[&dyn ToolAdapter]>,
    runner: &dyn ccync_engine::tool_manager::probe::ProbeRunner,
) -> ExitCode {
    let layout = match EffectiveLayout::load_or_legacy() {
        Ok(layout) => layout,
        Err(_) => return ExitCode::Error,
    };
    upgrade_one_tool_internal_with_layout(
        &layout,
        args,
        guard,
        id,
        dry_run,
        retry,
        adapters_override,
        runner,
    )
}

#[allow(clippy::too_many_arguments)]
fn upgrade_one_tool_internal_with_layout(
    layout: &EffectiveLayout,
    args: &[String],
    guard: Option<&LifecycleGuard>,
    id: &ToolId,
    dry_run: bool,
    retry: bool,
    adapters_override: Option<&[&dyn ToolAdapter]>,
    runner: &dyn ccync_engine::tool_manager::probe::ProbeRunner,
) -> ExitCode {
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    upgrade_one_tool_internal_to_with_layout(
        layout,
        args,
        guard,
        id,
        dry_run,
        retry,
        adapters_override,
        runner,
        &mut stdout,
        &mut stderr,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn upgrade_one_tool_internal_to(
    args: &[String],
    guard: Option<&LifecycleGuard>,
    id: &ToolId,
    dry_run: bool,
    retry: bool,
    adapters_override: Option<&[&dyn ToolAdapter]>,
    runner: &dyn ccync_engine::tool_manager::probe::ProbeRunner,
    stdout: &mut dyn std::io::Write,
    stderr: &mut dyn std::io::Write,
) -> ExitCode {
    let layout = match EffectiveLayout::load_or_legacy() {
        Ok(layout) => layout,
        Err(_) => return ExitCode::Error,
    };
    upgrade_one_tool_internal_to_with_layout(
        &layout,
        args,
        guard,
        id,
        dry_run,
        retry,
        adapters_override,
        runner,
        stdout,
        stderr,
    )
}

#[allow(clippy::too_many_arguments)]
fn upgrade_one_tool_internal_to_with_layout(
    layout: &EffectiveLayout,
    args: &[String],
    guard: Option<&LifecycleGuard>,
    id: &ToolId,
    dry_run: bool,
    retry: bool,
    adapters_override: Option<&[&dyn ToolAdapter]>,
    runner: &dyn ccync_engine::tool_manager::probe::ProbeRunner,
    stdout: &mut dyn std::io::Write,
    stderr: &mut dyn std::io::Write,
) -> ExitCode {
    let json = args.iter().any(|arg| arg == "--json");
    let mut emit = |outcome: super::tool_output::ToolOutcome,
                    diagnostic: Option<&str>,
                    update_state: Option<&str>,
                    freshness: Option<&str>,
                    projection_changed: bool,
                    agents: &[String]|
     -> ExitCode {
        emit_tool_upgrade_to(
            json,
            outcome,
            diagnostic,
            update_state,
            freshness,
            projection_changed,
            agents,
            stdout,
            stderr,
        )
    };
    let Some(guard) = guard else {
        return emit(
            super::tool_output::ToolOutcome::CheckFailed,
            Some("lifecycle guard unavailable"),
            None,
            None,
            false,
            &[],
        );
    };
    let recovery = match tool_manager::recovery::recover(
        guard,
        &tool_manager::journal::RetryIntent::Upgrade {
            tool_id: id.clone(),
        },
    ) {
        Ok(tool_manager::recovery::RecoveryDecision::Resume) => {
            if retry && !manager_outcome_unknown_journal(layout, id) {
                let _ = writeln!(
                    stderr,
                    "ccync upgrade: --retry-manager-step requires this tool's manager-outcome-unknown journal"
                );
                return ExitCode::Usage;
            }
            true
        }
        Ok(tool_manager::recovery::RecoveryDecision::None)
        | Ok(tool_manager::recovery::RecoveryDecision::CommittedCleanup) => false,
        Ok(tool_manager::recovery::RecoveryDecision::RecoveryRequired(required)) => {
            if retry {
                let _ = writeln!(
                    stderr,
                    "ccync upgrade: --retry-manager-step requires this tool's manager-outcome-unknown journal"
                );
                return ExitCode::Usage;
            }
            return emit(
                super::tool_output::ToolOutcome::RecoveryRequired,
                Some(&format!("run `{}`", required.retry_command)),
                None,
                None,
                false,
                &[],
            );
        }
        Err(error) => {
            return emit(
                super::tool_output::ToolOutcome::CheckFailed,
                Some(&error.to_string()),
                None,
                None,
                false,
                &[],
            );
        }
    };
    let declarations = match store::load_declarations() {
        Ok(v) => v,
        Err(e) => {
            return emit(
                super::tool_output::ToolOutcome::CheckFailed,
                Some(&format!("{e}")),
                None,
                None,
                false,
                &[],
            );
        }
    };
    let Some(declaration) = declarations.get(id) else {
        return emit(
            super::tool_output::ToolOutcome::Blocked,
            Some("track the tool first"),
            None,
            None,
            false,
            &[],
        );
    };
    if declaration.hold_policy == tool_manager::model::HoldPolicy::Held {
        return emit(
            super::tool_output::ToolOutcome::Blocked,
            Some(&format!("ccync tool hold {id} --off")),
            None,
            None,
            false,
            &[],
        );
    }
    if declaration.identity.adapter_kind == tool_manager::model::AdapterKind::ManualPath {
        return emit(
            super::tool_output::ToolOutcome::Blocked,
            Some("track this tool with a supported package manager"),
            None,
            None,
            false,
            &[],
        );
    }
    if retry && !recovery {
        let _ = writeln!(
            stderr,
            "ccync upgrade: --retry-manager-step requires this tool's manager-outcome-unknown journal"
        );
        return ExitCode::Usage;
    }
    let recorded_adapter;
    let adapter: Option<&dyn ToolAdapter> = if let Some(custom) = adapters_override {
        custom
            .iter()
            .find(|a| a.kind() == declaration.identity.adapter_kind)
            .copied()
    } else {
        match adapters::for_tracked_declaration(declaration) {
            Ok((adapter, _, _)) => {
                recorded_adapter = adapter;
                Some(recorded_adapter.as_ref())
            }
            Err(error) => {
                return emit(
                    super::tool_output::ToolOutcome::Blocked,
                    Some(&format!("{error:?}")),
                    None,
                    None,
                    false,
                    &[],
                )
            }
        }
    };
    let Some(adapter) = adapter else {
        return emit(
            super::tool_output::ToolOutcome::Blocked,
            Some("unsupported tool adapter"),
            None,
            None,
            false,
            &[],
        );
    };
    if !recovery && adapter.capabilities().package_update != adapters::CapabilityState::Supported {
        return emit(
            super::tool_output::ToolOutcome::Blocked,
            Some("package update is unsupported for this tool"),
            None,
            None,
            false,
            &[],
        );
    }
    let states = match store::load_state() {
        Ok(v) => v,
        Err(e) => {
            return emit(
                super::tool_output::ToolOutcome::CheckFailed,
                Some(&format!("{e}")),
                None,
                None,
                false,
                &[],
            );
        }
    };
    let Some(state) = states.get(id) else {
        return emit(
            super::tool_output::ToolOutcome::Blocked,
            Some("refresh the tool state before upgrading"),
            None,
            None,
            false,
            &[],
        );
    };
    let package_update = if recovery {
        UpdateState::UpdateAvailable
    } else {
        match adapter
            .remote_update_status(&tool_manager::discovery::DiscoveryQuery { online: true })
        {
            Ok(RemoteUpdateStatus::UpdateAvailable) => UpdateState::UpdateAvailable,
            Ok(RemoteUpdateStatus::Current) => {
                return emit(
                    super::tool_output::ToolOutcome::Current,
                    Some("no package update is available"),
                    Some("current"),
                    Some("fresh"),
                    false,
                    &[],
                )
            }
            Ok(RemoteUpdateStatus::Unsupported | RemoteUpdateStatus::NotChecked) => {
                return emit(
                    super::tool_output::ToolOutcome::Blocked,
                    Some("package update proof is unavailable"),
                    Some("not-checked"),
                    Some("fresh"),
                    false,
                    &[],
                )
            }
            Err(error) => {
                return emit(
                    super::tool_output::ToolOutcome::CheckFailed,
                    Some(&format!("remote update check failed: {error:?}")),
                    Some("check-failed"),
                    Some("fresh"),
                    false,
                    &[],
                )
            }
        }
    };
    if !dry_run && !confirm_upgrade(args, retry) {
        return emit(
            super::tool_output::ToolOutcome::ConfirmationRequired,
            Some("pass --yes or set CCYNC_ASSUME_YES=1"),
            Some("update-available"),
            Some("fresh"),
            false,
            &[],
        );
    }
    let result = tool_manager::upgrade::upgrade_tool(
        guard,
        id,
        runner,
        || {
            let config = ccync_foundation::config::CcyncConfig::load();
            super::lifecycle::run_projection_with_report(&config, "ccync upgrade tool").1
        },
        tool_manager::upgrade::UpgradeOptions {
            declaration,
            adapter,
            package_update,
            retry_manager_step: retry,
            artifact_current: state.facets.artifact_state
                == tool_manager::model::ArtifactState::Current,
            projection_current: state.facets.projection_state
                == tool_manager::model::ProjectionState::Current,
            dry_run,
        },
    );
    match result {
        Ok(outcome) => {
            let result = match outcome.status {
                tool_manager::refresh::RefreshStatus::Committed => {
                    super::tool_output::ToolOutcome::Committed
                }
                tool_manager::refresh::RefreshStatus::DryRun => {
                    super::tool_output::ToolOutcome::DryRun
                }
                tool_manager::refresh::RefreshStatus::NoOp => {
                    super::tool_output::ToolOutcome::Current
                }
            };
            let next = match result {
                super::tool_output::ToolOutcome::Committed => "ccync outdated",
                super::tool_output::ToolOutcome::DryRun => super::tool_output::dry_run_value(),
                _ => "no change",
            };
            emit(
                result,
                Some(next),
                None,
                Some("stale"),
                upgrade_projection_changed(result, &outcome.reload_advised_agents),
                &outcome.reload_advised_agents,
            )
        }
        Err(tool_manager::upgrade::UpgradeError::Blocked { action }) => emit(
            super::tool_output::ToolOutcome::Blocked,
            Some(&action),
            None,
            None,
            false,
            &[],
        ),
        Err(tool_manager::upgrade::UpgradeError::OutcomeUnknown { retry_command }) => {
            emit(
                super::tool_output::ToolOutcome::RecoveryRequired,
                Some(&format!("run `{retry_command}`")),
                None,
                None,
                false,
                &[],
            )
        }
        Err(e) => {
            let text = e.to_string();
            let outcome = if text.contains("recovery journal") || text.contains("recovery") {
                super::tool_output::ToolOutcome::RecoveryRequired
            } else {
                super::tool_output::ToolOutcome::CheckFailed
            };
            emit(outcome, Some(&text), None, None, false, &[])
        }
    }
}

#[allow(dead_code)]
fn emit_tool_upgrade(
    json: bool,
    outcome: super::tool_output::ToolOutcome,
    diagnostic: Option<&str>,
    update_state: Option<&str>,
    freshness: Option<&str>,
    projection_changed: bool,
    agents: &[String],
) -> ExitCode {
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    emit_tool_upgrade_to(
        json,
        outcome,
        diagnostic,
        update_state,
        freshness,
        projection_changed,
        agents,
        &mut stdout,
        &mut stderr,
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_tool_upgrade_to(
    json: bool,
    outcome: super::tool_output::ToolOutcome,
    diagnostic: Option<&str>,
    update_state: Option<&str>,
    freshness: Option<&str>,
    projection_changed: bool,
    agents: &[String],
    stdout: &mut dyn std::io::Write,
    stderr: &mut dyn std::io::Write,
) -> ExitCode {
    let mut facts = super::tool_output::ToolOutcomeFacts::new(
        super::tool_output::ToolCommand::Upgrade,
        outcome,
    );
    facts.update_state = update_state.map(str::to_owned);
    facts.update_freshness = freshness.map(str::to_owned);
    facts.next_action = diagnostic.map(str::to_owned);
    facts = facts.with_reload(projection_changed, agents);
    if json {
        if let Ok(document) = super::tool_output::render_outcome_json(&facts) {
            let _ = writeln!(stdout, "{document}");
        }
    } else if super::tool_output::output_stream(super::tool_output::ToolCommand::Upgrade, outcome)
        == super::tool_output::OutputStream::Stdout
    {
        let _ = write!(stdout, "{}", super::tool_output::render_outcome_human(&facts));
    } else if let Some(diagnostic) = diagnostic {
        let _ = writeln!(stderr, "ccync upgrade: {diagnostic}");
    }
    outcome.exit_code()
}

fn upgrade_projection_changed(outcome: super::tool_output::ToolOutcome, agents: &[String]) -> bool {
    outcome == super::tool_output::ToolOutcome::Committed && !agents.is_empty()
}

fn manager_outcome_unknown_journal(layout: &EffectiveLayout, id: &ToolId) -> bool {
    std::fs::read(layout.tool_operation_journal_path())
        .ok()
        .and_then(|bytes| {
            serde_json::from_slice::<tool_manager::journal::OperationJournal>(&bytes).ok()
        })
        .is_some_and(|journal| {
            journal.tool_id == *id
                && journal.kind == tool_manager::journal::OperationKind::Upgrade
                && journal.manager_step_state
                    == tool_manager::journal::ManagerStepState::ManagerOutcomeUnknown
        })
}

fn confirm_upgrade(args: &[String], retry: bool) -> bool {
    if args.iter().any(|a| a == "--yes")
        || std::env::var("CCYNC_ASSUME_YES").is_ok_and(|v| v == "1")
    {
        return true;
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!("ccync upgrade: non-interactive session; pass --yes or set CCYNC_ASSUME_YES=1");
        return false;
    }
    eprint!(
        "{} [y/N] ",
        if retry {
            "Retry package manager command?"
        } else {
            "Upgrade tracked tool?"
        }
    );
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).is_ok() && answer.trim().eq_ignore_ascii_case("y")
}

/// Build [`UpgradeOpts`] from the on-disk personal-plugin state. Prefers the
/// lockfile (carries resolved pins); falls back to the personal catalog.
fn build_opts(
    layout: &EffectiveLayout,
    filter_id: Option<String>,
    dry_run: bool,
) -> Result<UpgradeOpts, String> {
    let lock = layout.build_lock_path();
    let catalog = layout.personal_catalog_path();
    let cache_root = layout.cache_path();
    let plugins = read_json(Some(&lock))
        .or_else(|| read_json(Some(&catalog)))
        .unwrap_or_else(|| serde_json::json!({}));
    let pin_files: Vec<PathBuf> = [lock, catalog].into_iter().filter(|p| p.exists()).collect();
    Ok(UpgradeOpts {
        plugins,
        filter_id,
        dry_run,
        cache_root,
        pin_files,
    })
}

/// Read-only upgrade-available report (dry-run resolve): compares each git
/// plugin's remote HEAD against its pin, never cloning or projecting. Shared by
/// `list --upgrade-available`. On opts-build failure the report carries `error`.
pub(crate) fn upgrade_available_report() -> UpgradeReport {
    let layout = match EffectiveLayout::load_or_legacy() {
        Ok(layout) => layout,
        Err(error) => {
            return UpgradeReport {
                error: Some(format!("cannot load storage layout: {error}")),
                ..Default::default()
            };
        }
    };
    upgrade_available_report_with_layout(&layout)
}

pub(crate) fn upgrade_available_report_with_layout(layout: &EffectiveLayout) -> UpgradeReport {
    match build_opts(layout, None, true) {
        Ok(opts) => run_upgrade(&opts, git_ls_remote, clone_into_pinned_cache, project),
        Err(e) => UpgradeReport {
            error: Some(e),
            ..Default::default()
        },
    }
}

/// Read-only adopted-item classification (detect + report only) sharing
/// the same lockfile `Value` as [`upgrade_available_report`]. Returns an empty
/// list when the lockfile/catalog cannot be loaded (mirrors the `error`
/// carried by `upgrade_available_report` in that same case).
pub(crate) fn upgrade_available_adopted(layout: &EffectiveLayout) -> Vec<AdoptedUpgradeOutcome> {
    match build_opts(layout, None, true) {
        Ok(opts) => resolve_adopted_upgrades(&opts.plugins),
        Err(_) => Vec::new(),
    }
}

/// Lock-derived unresolved adopted-item names for `list --upgrade-available`,
/// sharing [`unresolved_adopted_names`]'s graceful-missing semantics.
pub(crate) fn upgrade_available_unresolved(layout: &EffectiveLayout) -> Vec<String> {
    unresolved_adopted_names(layout)
}

fn report_exit(
    layout: &EffectiveLayout,
    report: &UpgradeReport,
    adopted: &[AdoptedUpgradeOutcome],
    unresolved: &[String],
    dry_run: bool,
) -> ExitCode {
    if let Some(err) = &report.error {
        eprintln!("ccync upgrade: {err}");
        return ExitCode::Error;
    }

    if dry_run {
        print!(
            "{}",
            format_upgrade_view_with_packages(
                report,
                adopted,
                unresolved,
                &super::plugin::personal_package_names(layout)
            )
        );
        return report_to_exit(report);
    }

    for id in &report.upgraded {
        println!("upgraded {id}");
    }
    for id in &report.held {
        println!("held {id} (pinned)");
    }
    for (id, reason) in &report.skipped {
        println!("skipped {id} ({reason})");
    }
    for (id, reason) in &report.failed {
        eprintln!("failed {id}: {reason}");
    }
    println!(
        "Summary: {} upgraded, {} unchanged, {} held, {} skipped, {} failed.{}",
        report.upgraded.len(),
        report.unchanged.len(),
        report.held.len(),
        report.skipped.len(),
        report.failed.len(),
        if report.projected { " Projected." } else { "" }
    );

    print!("{}", format_adopted_section(adopted));
    let mut tip = String::new();
    push_repair_tip(&mut tip, unresolved);
    print!("{tip}");

    if report.failed.is_empty() {
        ExitCode::Success
    } else {
        ExitCode::Error
    }
}

fn short(sha: &str) -> String {
    sha.chars().take(12).collect()
}

/// Read-only exit semantics shared by `upgrade --dry-run` and
/// `list --upgrade-available`: a real error → `Error`; pending upgrades →
/// `UpdatesAvailable` (3); otherwise → `Success`. Lets a script tell
/// "updates available" apart from an operational failure.
pub(crate) fn report_to_exit(report: &UpgradeReport) -> ExitCode {
    if report.error.is_some() {
        ExitCode::Error
    } else if !report.pending.is_empty() {
        ExitCode::UpdatesAvailable
    } else {
        ExitCode::Success
    }
}

/// Render the upgrade-available view: one `id  current → latest  source` line
/// per pending change (current = pinned sha or `(unpinned)`), plus skipped
/// entries, plus the adopted-item section. Pure (returns the text) so it is
/// testable and reusable by `list --upgrade-available`.
#[cfg(test)]
pub(crate) fn format_upgrade_view(
    report: &UpgradeReport,
    adopted: &[AdoptedUpgradeOutcome],
    unresolved: &[String],
) -> String {
    format_upgrade_view_with_packages(report, adopted, unresolved, &BTreeMap::new())
}

/// Label one personal plugin: its canonical package name first, then the
/// management ID that every command accepts. A plugin with no verified
/// package name shows its management ID alone.
pub(crate) fn plugin_label(id: &str, packages: &BTreeMap<String, String>) -> String {
    match packages.get(id) {
        Some(package) => format!("{package} (management ID: {id})"),
        None => id.to_string(),
    }
}

/// [`format_upgrade_view`] with the canonical package names of the personal
/// catalog. The suggested commands keep naming the management ID.
pub(crate) fn format_upgrade_view_with_packages(
    report: &UpgradeReport,
    adopted: &[AdoptedUpgradeOutcome],
    unresolved: &[String],
    packages: &BTreeMap<String, String>,
) -> String {
    let mut out = String::new();
    let tracked_count = report.unchanged.len()
        + report.held.len()
        + report.skipped.len()
        + report.failed.len()
        + adopted.len();
    if report.pending.is_empty() {
        if tracked_count == 0 {
            out.push_str("No plugins to check for updates.\n");
        } else {
            out.push_str("Everything up to date.\n");
        }
    } else {
        out.push_str("Updates available:\n");
        for c in &report.pending {
            let current = c
                .old_sha
                .as_deref()
                .map(short)
                .unwrap_or_else(|| "(unpinned)".to_string());
            out.push_str(&format!(
                "  {}  {} → {}  {}\n",
                plugin_label(&c.plugin_id, packages),
                current,
                short(&c.new_sha),
                c.source
            ));
        }
        out.push_str("  run `ccync upgrade` to apply\n");
    }
    for id in &report.held {
        out.push_str(&format!(
            "  held {} (pinned — run `ccync pin --remove {id}`)\n",
            plugin_label(id, packages)
        ));
    }
    for (id, reason) in &report.skipped {
        out.push_str(&format!(
            "  skipped {} ({reason})\n",
            plugin_label(id, packages)
        ));
    }
    out.push_str(&format_adopted_section(adopted));
    push_repair_tip(&mut out, unresolved);
    out
}

/// Render the adopted-item report-only section (detect + report, never
/// auto-apply). `Changed` prints `current -> latest` plus the two activation
/// paths — Claude's own `/plugin` UI, or `ccync add <source>` to take over
/// management. `Unchanged` is omitted entirely, mirroring how the personal-
/// plugin view above never prints unchanged plugins either. `Skipped` mirrors
/// the personal `skipped {id} ({reason})` line style. Pure and testable;
/// never influences `ExitCode` (adopted items have no "pending" concept in
/// [`UpgradeReport`]).
///
/// This no longer prints a per-item repair hint (removed with
/// `AdoptedUpgradeOutcome::unresolved_repair_hint`, which was plugin-only);
/// [`push_repair_tip`] appends a single lock-derived tip line covering every
/// kind after this section instead.
pub(crate) fn format_adopted_section(adopted: &[AdoptedUpgradeOutcome]) -> String {
    let mut out = String::new();
    for outcome in adopted {
        match outcome {
            AdoptedUpgradeOutcome::Changed {
                name,
                source,
                current_version,
                latest_version,
                takeover_supported,
            } => {
                let current = current_version.as_deref().unwrap_or("(unknown)");
                out.push_str(&format!(
                    "  adopted {name}  {current} -> {latest_version}  {source}\n"
                ));
                if *takeover_supported {
                    out.push_str(
                        "    update via Claude Code's own /plugin, or take over management: ",
                    );
                    out.push_str(&format!("ccync add {source}\n"));
                } else {
                    out.push_str(
                        "    update via Claude Code's own /plugin (plugin lives inside the marketplace repo — `ccync add` cannot fetch it directly)\n",
                    );
                }
            }
            AdoptedUpgradeOutcome::Unchanged { .. } => {}
            AdoptedUpgradeOutcome::VersionUnresolved { name, source } => {
                out.push_str(&format!(
                    "  adopted {name} version-unresolved (source: {source})\n"
                ));
            }
            AdoptedUpgradeOutcome::Skipped { name, reason } => {
                out.push_str(&format!("  adopted {name} skipped ({reason})\n"));
            }
        }
    }
    out
}

/// Append a single `Tip: run \`ccync repair\`` line when `unresolved` (from
/// `ccync_engine::adopt::adopted_items_without_source`, lock-derived and
/// covering plugin/MCP/skill uniformly) is non-empty. Printed once per
/// invocation regardless of how many rows are unresolved — never per-item,
/// unlike the removed plugin-only hint.
pub(crate) fn push_repair_tip(out: &mut String, unresolved: &[String]) {
    if !unresolved.is_empty() {
        out.push_str("Tip: run `ccync repair` to resolve missing adopted-item sources.\n");
    }
}

fn read_json(path: Option<&Path>) -> Option<Value> {
    let p = path?;
    let text = std::fs::read_to_string(p).ok()?;
    serde_json::from_str(&text).ok()
}

/// `git ls-remote <url> <ref>` → the remote object sha (first column).
fn ls_remote_args<'a>(url: &'a str, git_ref: &'a str) -> [&'a str; 3] {
    [
        "ls-remote",
        ccync_engine::install::split_git_subpath(url).0,
        git_ref,
    ]
}

fn git_ls_remote(url: &str, git_ref: &str) -> Result<String, String> {
    let out = Command::new("git")
        .args(ls_remote_args(url, git_ref))
        .output()
        .map_err(|e| format!("spawn git ls-remote: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().next())
        .map(str::to_string)
        .ok_or_else(|| format!("git ls-remote {url} {git_ref}: empty result"))
}

/// Materialize a plugin into the exact pinned cache path requested by upgrade.
pub(crate) fn clone_into_pinned_cache(url: &str, dest: &Path) -> Result<(), String> {
    let filename = dest
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("destination has no file name")?;
    let (plugin_id, sha) = filename
        .rsplit_once('@')
        .ok_or("destination must contain plugin id and sha separated by @")?;
    let parent = dest
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or("destination has no parent")?;

    match ccync_engine::install::fetch_personal_plugin_at(parent, plugin_id, url, Some(sha)) {
        ccync_engine::install::PersonalFetchResult::Cloned { path, .. }
        | ccync_engine::install::PersonalFetchResult::AlreadyPresent(path)
            if path == dest =>
        {
            Ok(())
        }
        ccync_engine::install::PersonalFetchResult::Cloned { path, .. }
        | ccync_engine::install::PersonalFetchResult::AlreadyPresent(path) => Err(format!(
            "landed at {} not {}",
            path.display(),
            dest.display()
        )),
        ccync_engine::install::PersonalFetchResult::CloneFailed(message) => Err(message),
        ccync_engine::install::PersonalFetchResult::NoCacheRoot => Err("no cache root".into()),
    }
}

/// Reproject the managed set after pins change (reuses the unified engine).
fn project() -> Result<(), String> {
    match crate::commands::lifecycle::run_unified_projection(
        false,
        true,
        false,
        Some(&crate::commands::lifecycle::SYNC_GATE_ABORT_CONTEXT),
    ) {
        ExitCode::Success => Ok(()),
        other => Err(format!("projection returned {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccync_engine::UpgradeChange;

    fn change(id: &str, old: &str, new: &str) -> UpgradeChange {
        UpgradeChange {
            plugin_id: id.into(),
            source: format!("https://github.com/u/{id}.git"),
            old_sha: Some(old.into()),
            new_sha: new.into(),
        }
    }

    /// Load a layout from a fresh temp home while holding the crate env lock,
    /// so concurrent env-mutating tests cannot change the roots mid-load.
    fn with_isolated_layout<R>(f: impl FnOnce(&EffectiveLayout) -> R) -> R {
        let _guard = crate::test_io_lock();
        let home = tempfile::tempdir().unwrap();
        let keys = [
            "USERPROFILE",
            "HOME",
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
        ];
        let saved: Vec<_> = keys
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        for key in keys {
            let target = match key {
                "APPDATA" | "XDG_CONFIG_HOME" => home.path().join("C"),
                "LOCALAPPDATA" | "XDG_STATE_HOME" => home.path().join("S"),
                "XDG_CACHE_HOME" => home.path().join("K"),
                _ => home.path().to_path_buf(),
            };
            unsafe { std::env::set_var(key, target) };
        }
        let result = EffectiveLayout::load_or_legacy().map(|layout| f(&layout));
        for (key, value) in saved {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
        result.unwrap()
    }

    #[test]
    fn ls_remote_args_use_repository_url_for_selector_sources() {
        assert_eq!(
            ls_remote_args("https://example.com/org/repo.git//plugins/tool", "HEAD"),
            ["ls-remote", "https://example.com/org/repo.git", "HEAD"]
        );
    }

    #[test]
    fn clone_into_pinned_cache_materializes_a_local_fixture_repo() {
        let repo = tempfile::TempDir::new().unwrap();
        let cache = repo.path().join("cache");
        let fixture = repo.path().join("fixture");
        std::fs::create_dir_all(&fixture).unwrap();
        std::fs::write(fixture.join("plugin.txt"), "fixture").unwrap();

        for args in [
            &["init"][..],
            &["config", "user.email", "test@example.invalid"][..],
            &["config", "user.name", "ccync test"][..],
            &["add", "plugin.txt"][..],
            &["commit", "-m", "fixture"][..],
        ] {
            let output = Command::new("git")
                .current_dir(&fixture)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let sha = String::from_utf8(
            Command::new("git")
                .current_dir(&fixture)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        let dest = cache.join(format!("fixture@{sha}"));

        clone_into_pinned_cache(fixture.to_str().unwrap(), &dest).unwrap();

        assert_eq!(
            std::fs::read_to_string(dest.join("plugin.txt")).unwrap(),
            "fixture"
        );
    }

    #[test]
    fn clone_into_pinned_cache_rejects_unpinned_destination() {
        let error = clone_into_pinned_cache("fixture", Path::new("cache/fixture")).unwrap_err();
        assert!(error.contains("@"));
    }

    #[test]
    fn clone_into_pinned_cache_rejects_destination_without_parent() {
        let error = clone_into_pinned_cache("fixture", Path::new("fixture@1234567")).unwrap_err();
        assert!(error.contains("parent"));
    }

    #[test]
    fn report_to_exit_three_states() {
        assert_eq!(report_to_exit(&UpgradeReport::default()), ExitCode::Success);
        let pending = UpgradeReport {
            pending: vec![change("a", "oldsha111", "newsha222")],
            ..Default::default()
        };
        assert_eq!(report_to_exit(&pending), ExitCode::UpdatesAvailable);
        let err = UpgradeReport {
            error: Some("boom".into()),
            ..Default::default()
        };
        assert_eq!(report_to_exit(&err), ExitCode::Error);
    }

    #[test]
    fn format_upgrade_view_renders_current_to_latest() {
        let report = UpgradeReport {
            pending: vec![change("a", "oldsha111", "newsha222")],
            ..Default::default()
        };
        let out = format_upgrade_view(&report, &[], &[]);
        assert!(out.contains('→'), "view must render current → latest arrow");
        assert!(out.contains("oldsha111") && out.contains("newsha222"));
        assert!(out.contains('a'));
    }

    #[test]
    fn format_upgrade_view_names_package_first_and_keeps_management_id_in_commands() {
        let report = UpgradeReport {
            pending: vec![change("plugin", "oldsha111", "newsha222")],
            held: vec!["plugin".to_string()],
            ..Default::default()
        };
        let packages = BTreeMap::from([("plugin".to_string(), "Impeccable".to_string())]);
        let out = format_upgrade_view_with_packages(&report, &[], &[], &packages);
        assert!(
            out.contains("Impeccable (management ID: plugin)  oldsha111"),
            "{out}"
        );
        assert!(
            out.contains("held Impeccable (management ID: plugin) (pinned — run `ccync pin --remove plugin`)"),
            "{out}"
        );
        assert!(!out.contains("ccync pin --remove Impeccable"), "{out}");
    }

    #[test]
    fn plugin_label_without_package_name_is_the_management_id_alone() {
        assert_eq!(plugin_label("graphify", &BTreeMap::new()), "graphify");
    }

    // ── adopted classifications + activation commands ─────────────────────────

    #[test]
    fn adopted_changed_prints_both_activation_commands_and_versions() {
        let adopted = vec![AdoptedUpgradeOutcome::Changed {
            name: "tool@market".into(),
            source: "https://github.com/example/tool-plugin.git".into(),
            current_version: Some("2.0.0".into()),
            latest_version: "2.1.0".into(),
            takeover_supported: true,
        }];
        let out = format_adopted_section(&adopted);
        assert!(out.contains("tool@market"), "must name the adopted item");
        assert!(
            out.contains("2.0.0") && out.contains("2.1.0"),
            "must report current -> latest"
        );
        assert!(
            out.contains("/plugin"),
            "must mention Claude Code's own /plugin activation path"
        );
        assert!(
            out.contains("ccync add https://github.com/example/tool-plugin.git"),
            "must print the take-over-management activation command with the source url"
        );
    }

    /// An embedded (in-repo-subpath) plugin's catalog-resolved source is a
    /// browse path, not a git remote — `cmd_add` would fail at clone. The
    /// view must not hand the user that command.
    #[test]
    fn adopted_changed_without_takeover_support_suppresses_the_add_command() {
        let adopted = vec![AdoptedUpgradeOutcome::Changed {
            name: "tool@market".into(),
            source: "https://github.com/example/market/plugins/tool".into(),
            current_version: Some("2.0.0".into()),
            latest_version: "2.1.0".into(),
            takeover_supported: false,
        }];
        let out = format_adopted_section(&adopted);
        // The explanation names the verb on purpose; what must never appear
        // is a runnable `ccync add <url>` the user could copy and have fail.
        assert!(
            !out.contains("ccync add https://"),
            "a non-clonable source must never be advertised as a runnable add: {out:?}"
        );
        assert!(
            out.contains("/plugin"),
            "Claude's own update path stays available: {out:?}"
        );
        assert!(
            out.contains("cannot fetch it directly"),
            "must say why take-over is unavailable: {out:?}"
        );
        assert!(
            out.contains("2.0.0") && out.contains("2.1.0"),
            "the upgrade itself is still reported: {out:?}"
        );
    }

    #[test]
    fn adopted_changed_with_unknown_current_version_renders_placeholder() {
        let adopted = vec![AdoptedUpgradeOutcome::Changed {
            name: "tool@market".into(),
            source: "https://github.com/example/tool-plugin.git".into(),
            current_version: None,
            latest_version: "3.0.0".into(),
            takeover_supported: true,
        }];
        let out = format_adopted_section(&adopted);
        assert!(
            out.contains("(unknown)"),
            "missing current_version must render a placeholder"
        );
    }

    #[test]
    fn adopted_skipped_prints_reason() {
        let adopted = vec![AdoptedUpgradeOutcome::Skipped {
            name: "memory".into(),
            reason: "adopted MCP server — config entry, not versioned".into(),
        }];
        let out = format_adopted_section(&adopted);
        assert!(out.contains("memory"));
        assert!(out.contains("adopted MCP server — config entry, not versioned"));
    }

    #[test]
    fn adopted_version_unresolved_prints_name_and_source_not_skipped_wording() {
        let adopted = vec![AdoptedUpgradeOutcome::VersionUnresolved {
            name: "tool@market".into(),
            source: "https://github.com/example/tool.git".into(),
        }];
        let out = format_adopted_section(&adopted);
        assert!(out.contains("tool@market"));
        assert!(out.contains("version-unresolved"));
        assert!(out.contains("https://github.com/example/tool.git"));
        assert!(
            !out.contains("skipped"),
            "a source-bearing item must never render as skipped: {out:?}"
        );
    }

    // ── lock-derived repair tip (replaces the removed plugin-only hint) ────────

    #[test]
    fn push_repair_tip_prints_once_when_unresolved_names_present() {
        let mut out = String::new();
        push_repair_tip(&mut out, &["standalone-tool".to_string()]);
        let tip_lines = out.lines().filter(|l| l.contains("ccync repair")).count();
        assert_eq!(
            tip_lines, 1,
            "must print exactly one repair-tip line: {out:?}"
        );
    }

    #[test]
    fn push_repair_tip_absent_when_no_unresolved_names() {
        let mut out = String::new();
        push_repair_tip(&mut out, &[]);
        assert!(
            out.is_empty(),
            "no unresolved rows must print no tip: {out:?}"
        );
    }

    #[test]
    fn push_repair_tip_still_one_line_for_multiple_unresolved_names() {
        // The tip is a single generic line regardless of count — never one
        // line per item, unlike the removed plugin-only hint.
        let mut out = String::new();
        push_repair_tip(
            &mut out,
            &["a".to_string(), "b".to_string(), "c".to_string()],
        );
        assert_eq!(out.lines().count(), 1, "must stay a single line: {out:?}");
    }

    #[test]
    fn format_adopted_section_no_longer_prints_any_repair_hint() {
        // format_adopted_section itself never prints a tip anymore — that is
        // now push_repair_tip's job, driven by lock-derived unresolved
        // names, not per-item Skipped-reason detection.
        let adopted = vec![AdoptedUpgradeOutcome::Skipped {
            name: "standalone-tool".into(),
            reason: "adopted plugin — source-unresolved (no clonable source recorded)".into(),
        }];
        let out = format_adopted_section(&adopted);
        assert!(
            !out.contains("Tip:"),
            "format_adopted_section must not print a tip: {out:?}"
        );
    }

    #[test]
    fn format_upgrade_view_appends_repair_tip_when_unresolved_present() {
        let report = UpgradeReport::default();
        let out = format_upgrade_view(&report, &[], &["standalone-tool".to_string()]);
        assert!(
            out.contains("ccync repair"),
            "must append the repair tip: {out:?}"
        );
    }

    #[test]
    fn format_upgrade_view_omits_repair_tip_when_all_resolved() {
        let report = UpgradeReport::default();
        let out = format_upgrade_view(&report, &[], &[]);
        assert!(
            !out.contains("ccync repair"),
            "must not print a tip when all resolved: {out:?}"
        );
    }

    #[test]
    fn adopted_unchanged_is_not_printed() {
        let adopted = vec![AdoptedUpgradeOutcome::Unchanged {
            name: "tool@market".into(),
        }];
        let out = format_adopted_section(&adopted);
        assert!(
            out.is_empty(),
            "unchanged adopted items are omitted, matching personal-plugin view"
        );
    }

    #[test]
    fn format_upgrade_view_zero_plugins_is_neutral_not_up_to_date() {
        let report = UpgradeReport::default();
        let out = format_upgrade_view(&report, &[], &[]);
        assert!(
            !out.contains("Everything up to date."),
            "zero tracked plugins must not claim everything is up to date"
        );
        assert!(out.contains("No plugins to check for updates."));
    }

    #[test]
    fn format_upgrade_view_pending_includes_apply_hint() {
        let report = UpgradeReport {
            pending: vec![change("a", "oldsha111", "newsha222")],
            ..Default::default()
        };
        let out = format_upgrade_view(&report, &[], &[]);
        assert!(out.contains("run `ccync upgrade` to apply"));
    }

    #[test]
    fn format_upgrade_view_includes_adopted_section() {
        let report = UpgradeReport::default();
        let adopted = vec![AdoptedUpgradeOutcome::Changed {
            name: "tool@market".into(),
            source: "https://github.com/example/tool-plugin.git".into(),
            current_version: Some("1.0.0".into()),
            latest_version: "1.1.0".into(),
            takeover_supported: true,
        }];
        let out = format_upgrade_view(&report, &adopted, &[]);
        assert!(
            out.contains("Everything up to date."),
            "personal section unaffected"
        );
        assert!(out.contains("tool@market"), "adopted section appended");
    }

    /// Combined-rendering case: a real personal `pending` change AND an
    /// adopted `Changed` outcome present simultaneously (the realistic
    /// dry-run scenario when both a git plugin and an adopted plugin have
    /// updates). Both sections must render fully and independently — neither
    /// truncates or clobbers the other's lines.
    #[test]
    fn format_upgrade_view_renders_personal_pending_and_adopted_changed_together() {
        let report = UpgradeReport {
            pending: vec![change("a", "oldsha111", "newsha222")],
            skipped: vec![("b".into(), "non-git source".into())],
            ..Default::default()
        };
        let adopted = vec![
            AdoptedUpgradeOutcome::Changed {
                name: "tool@market".into(),
                source: "https://github.com/example/tool-plugin.git".into(),
                current_version: Some("2.0.0".into()),
                latest_version: "2.1.0".into(),
                takeover_supported: true,
            },
            AdoptedUpgradeOutcome::Skipped {
                name: "memory".into(),
                reason: "adopted MCP server — config entry, not versioned".into(),
            },
        ];
        let out = format_upgrade_view(&report, &adopted, &[]);

        // Personal section: pending change + skipped entry both intact.
        assert!(out.contains("Updates available:"));
        assert!(out.contains('a') && out.contains("oldsha111") && out.contains("newsha222"));
        assert!(out.contains("skipped b (non-git source)"));

        // Adopted section: Changed (with both activation commands) + Skipped
        // both intact, not clobbered by the personal section above it.
        assert!(out.contains(
            "adopted tool@market  2.0.0 -> 2.1.0  https://github.com/example/tool-plugin.git"
        ));
        assert!(out.contains("/plugin"));
        assert!(out.contains("ccync add https://github.com/example/tool-plugin.git"));
        assert!(out
            .contains("adopted memory skipped (adopted MCP server — config entry, not versioned)"));

        // Ordering sanity: personal section precedes adopted section.
        let personal_pos = out.find("Updates available:").unwrap();
        let adopted_pos = out.find("adopted tool@market").unwrap();
        assert!(
            personal_pos < adopted_pos,
            "personal section must precede adopted section"
        );
    }

    /// Adopted `Changed` items are report-only and never affect the exit code.
    /// Zero personal `pending` + a `Changed` adopted item still must exit
    /// `Success` (0), proving the two are decided independently.
    #[test]
    fn adopted_changed_present_does_not_affect_exit_code() {
        let report = UpgradeReport::default(); // no personal pending changes
        let adopted = vec![AdoptedUpgradeOutcome::Changed {
            name: "tool@market".into(),
            source: "https://github.com/example/tool-plugin.git".into(),
            current_version: Some("2.0.0".into()),
            latest_version: "2.1.0".into(),
            takeover_supported: true,
        }];
        // report_to_exit takes only the personal report — adopted has no
        // pathway into it at all.
        assert_eq!(report_to_exit(&report), ExitCode::Success);

        // report_exit (dry-run branch) must also still return Success even
        // though the printed view carries the adopted Changed section.
        assert_eq!(
            with_isolated_layout(|layout| report_exit(layout, &report, &adopted, &[], true)),
            ExitCode::Success
        );
    }

    /// Exit-code invariant, non-dry-run branch: personal `failed` still drives
    /// `Error` (1) regardless of what adopted outcomes are present.
    #[test]
    fn adopted_state_does_not_affect_non_dry_run_error_exit() {
        let report = UpgradeReport {
            failed: vec![("a".into(), "boom".into())],
            ..Default::default()
        };
        let adopted = vec![AdoptedUpgradeOutcome::Changed {
            name: "tool@market".into(),
            source: "https://github.com/example/tool-plugin.git".into(),
            current_version: Some("2.0.0".into()),
            latest_version: "2.1.0".into(),
            takeover_supported: true,
        }];
        assert_eq!(
            with_isolated_layout(|layout| report_exit(layout, &report, &adopted, &[], false)),
            ExitCode::Error
        );
    }

    /// Exit-code invariant, non-dry-run success branch: no personal failures
    /// + an adopted Changed item still exits Success (0).
    #[test]
    fn adopted_state_does_not_affect_non_dry_run_success_exit() {
        let report = UpgradeReport {
            upgraded: vec!["a".into()],
            ..Default::default()
        };
        let adopted = vec![AdoptedUpgradeOutcome::Changed {
            name: "tool@market".into(),
            source: "https://github.com/example/tool-plugin.git".into(),
            current_version: Some("2.0.0".into()),
            latest_version: "2.1.0".into(),
            takeover_supported: true,
        }];
        assert_eq!(
            with_isolated_layout(|layout| report_exit(layout, &report, &adopted, &[], false)),
            ExitCode::Success
        );
    }

    /// exit code 3 (`UpdatesAvailable`) with adopted state present: personal
    /// `pending` still drives the exit code, independent of adopted outcomes.
    #[test]
    fn adopted_state_does_not_affect_updates_available_exit() {
        let report = UpgradeReport {
            pending: vec![change("a", "oldsha", "newsha")],
            ..Default::default()
        };
        let adopted = vec![AdoptedUpgradeOutcome::Skipped {
            name: "memory".into(),
            reason: "adopted MCP server — config entry, not versioned".into(),
        }];
        assert_eq!(
            with_isolated_layout(|layout| report_exit(layout, &report, &adopted, &[], true)),
            ExitCode::UpdatesAvailable
        );
    }

    // ── pre-init upgrade guard (machine-initialization preflight) ─────────────
    //
    // Mirrors lifecycle.rs's pre-init sync guard tests: a non-dry `upgrade`
    // clones/re-pins git plugins and reprojects, so it must refuse before ANY
    // domain mutation on an uninitialized machine, while `upgrade --dry-run`
    // (the exception, same shape as `sync --dry-run`) stays available
    // pre-init.
    mod pre_init_upgrade_guard {
        use super::*;

        fn isolate_env(tmp: &std::path::Path) -> Vec<(String, Option<String>)> {
            let keys = [
                "USERPROFILE",
                "HOME",
                "APPDATA",
                "LOCALAPPDATA",
                "XDG_CONFIG_HOME",
            ];
            let mut saved = Vec::new();
            for k in keys {
                saved.push((k.to_string(), std::env::var(k).ok()));
                unsafe {
                    std::env::set_var(k, tmp);
                }
            }
            saved
        }

        fn restore_saved_env(saved: &[(String, Option<String>)]) {
            for (k, v) in saved {
                unsafe {
                    match v {
                        Some(val) => std::env::set_var(k, val),
                        None => std::env::remove_var(k),
                    }
                }
            }
        }

        /// Recursive snapshot of every file under `dir`: (relative path, byte
        /// length, modified time). Used to prove a refused pre-init upgrade
        /// leaves the entire home tree byte- and mtime-identical — zero
        /// cache/pin/live-surface writes, i.e. the clone/pin/project seams
        /// were never reached.
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

        /// Seed a personal catalog with one git plugin entry so, absent the
        /// preflight, `cmd_upgrade` would reach `resolve_adopted_upgrades` /
        /// `run_upgrade` and attempt to clone/pin it. The source URL is
        /// deliberately unreachable — the guard must refuse before any
        /// git subprocess (`ls-remote`/`clone`) is ever spawned.
        fn seed_personal_git_plugin(home: &std::path::Path) {
            let ccync = home.join(".ccync");
            std::fs::create_dir_all(&ccync).unwrap();
            std::fs::write(
                ccync.join("plugins.json"),
                r#"{"_personalPlugins":[{"pluginId":"a","installStrategy":"personal-git-clone","source":"https://example.invalid/a.git","pin":"oldsha"}]}"#,
            )
            .unwrap();
        }

        #[test]
        fn pre_init_upgrade_guard_refuses_non_dry_with_zero_mutation() {
            let _env_guard = crate::test_io_lock();
            let tmp = tempfile::TempDir::new().unwrap();
            let saved = isolate_env(tmp.path());

            seed_personal_git_plugin(tmp.path());
            let before = snapshot_tree(tmp.path());

            let args = vec!["upgrade".to_string()];
            let exit = cmd_upgrade(&args, None);

            let after = snapshot_tree(tmp.path());
            restore_saved_env(&saved);

            assert_eq!(
                exit,
                ExitCode::Error,
                "a non-dry upgrade on an uninitialized machine must refuse with Error/1"
            );
            assert!(
                !before.is_empty(),
                "sanity: the seeded personal catalog must exist before the refused upgrade"
            );
            assert_eq!(
                before, after,
                "a refused pre-init upgrade must leave every file byte- and mtime-identical — \
                 zero clone/pin/project mutation (the cache root, pin files, and live agent \
                 surfaces are all untouched)"
            );
            assert!(
                !tmp.path().join(".ccync").join("cache").exists(),
                "the clone seam must never run: no cache dir created"
            );
            assert!(
                !tmp.path().join(".ccync").join("build").exists(),
                "the project seam must never run: no build/render output created"
            );
        }

        #[test]
        fn pre_init_upgrade_guard_dry_run_still_succeeds_pre_init() {
            let _env_guard = crate::test_io_lock();
            let tmp = tempfile::TempDir::new().unwrap();
            let saved = isolate_env(tmp.path());

            // Deliberately unseeded — a fresh, uninitialized machine with no
            // personal catalog at all. `--dry-run` must still reach its
            // existing read-only outcome instead of being refused.
            assert!(
                !crate::commands::lifecycle::machine_is_initialized(),
                "sanity: fresh redirected home must be uninitialized"
            );

            let args = vec!["upgrade".to_string(), "--dry-run".to_string()];
            let exit = cmd_upgrade(&args, None);

            restore_saved_env(&saved);

            assert_eq!(
                exit,
                ExitCode::Success,
                "upgrade --dry-run must remain available pre-init, still reaching its \
                 existing read-only outcome (no plugins tracked ⇒ Success)"
            );
        }
    }

    fn isolate_env(tmp: &std::path::Path) -> Vec<(String, Option<String>)> {
        let keys = [
            "USERPROFILE",
            "HOME",
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
            "CCYNC_ASSUME_YES",
        ];
        let mut saved = Vec::new();
        for k in keys {
            saved.push((k.to_string(), std::env::var(k).ok()));
            unsafe {
                std::env::set_var(k, tmp);
            }
        }
        saved
    }

    fn restore_saved_env(saved: &[(String, Option<String>)]) {
        for (k, v) in saved {
            unsafe {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    fn setup_test_tool(
        home: &Path,
        tool_name: &str,
        kind: tool_manager::model::AdapterKind,
        hold_policy: tool_manager::model::HoldPolicy,
    ) -> (
        tool_manager::model::ToolId,
        tool_manager::model::ToolDeclaration,
        store::StateRecord,
    ) {
        let ccync_dir = home.join(".ccync");
        std::fs::create_dir_all(ccync_dir.join("build/tools")).unwrap();
        let exe_dir = home.join("bin");
        std::fs::create_dir_all(&exe_dir).unwrap();
        let exe_path = exe_dir.join(if cfg!(windows) {
            format!("{tool_name}.exe")
        } else {
            tool_name.to_string()
        });
        std::fs::write(&exe_path, b"initial-bytes-v1").unwrap();

        let tool_id = ToolId::new(tool_name).unwrap();
        let mut fields = std::collections::BTreeMap::new();
        fields.insert("executable".into(), exe_path.display().to_string());
        let identity = tool_manager::model::InstallationIdentity {
            adapter_kind: kind,
            fields,
        };
        let decl = tool_manager::model::ToolDeclaration {
            id: tool_id.clone(),
            identity: identity.clone(),
            delivery_id: tool_manager::model::DeliveryId::ObserveOnly,
            hold_policy,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        };
        let facets = tool_manager::model::ToolFacets {
            evidence_level: tool_manager::model::EvidenceLevel::Verified,
            selection_state: tool_manager::model::SelectionState::Unique,
            manager_availability: tool_manager::model::ManagerAvailability::Available,
            install_state: tool_manager::model::InstallState::Installed,
            package_state: tool_manager::model::PackageState::MatchesArtifactSource,
            update_state: tool_manager::model::UpdateState::Current,
            artifact_state: tool_manager::model::ArtifactState::Current,
            projection_state: tool_manager::model::ProjectionState::Current,
            hold_policy,
            manager_pin_state: tool_manager::model::ManagerPinState::Unpinned,
            operation_state: tool_manager::model::OperationState::Committed,
            last_check: None,
        };
        let sha = ccync_engine::tool_manager::file_identity::hash_same_handle(&exe_path).unwrap();
        #[cfg(unix)]
        let file_id = serde_json::json!({"dev": 0, "ino": 0});
        #[cfg(windows)]
        let file_id = serde_json::json!({"volumeSerialNumber": 0, "fileIndex": 0});
        let canonical_target = std::fs::canonicalize(&exe_path).unwrap();
        let source_evidence = serde_json::json!({
            "adapterFields": identity,
            "resolvedTarget": canonical_target,
            "fileIdentity": file_id,
            "sha256": sha,
        });
        let state_record = store::StateRecord {
            committed_generation: Some(1),
            artifact_fingerprint: Some("abc".into()),
            source_evidence,
            operation_id: Some("op-1".into()),
            facets,
        };
        (tool_id, decl, state_record)
    }

    #[derive(Debug)]
    struct MockAdapter {
        kind: tool_manager::model::AdapterKind,
        update_supported: bool,
        update_available: bool,
        update_argv: Option<Vec<String>>,
        identity: tool_manager::model::InstallationIdentity,
    }

    impl ToolAdapter for MockAdapter {
        fn kind(&self) -> tool_manager::model::AdapterKind {
            self.kind
        }
        fn capabilities(&self) -> adapters::AdapterCapabilities {
            adapters::AdapterCapabilities {
                local_enumeration: adapters::CapabilityState::Supported,
                executable_mapping: adapters::CapabilityState::Supported,
                installed_version_lookup: adapters::CapabilityState::Supported,
                remote_discovery: adapters::CapabilityState::Supported,
                package_update: if self.update_supported {
                    adapters::CapabilityState::Supported
                } else {
                    adapters::CapabilityState::Gated
                },
            }
        }
        fn discover(
            &self,
            _query: &tool_manager::discovery::DiscoveryQuery,
        ) -> Result<Vec<tool_manager::discovery::DiscoveryCandidate>, adapters::AdapterError>
        {
            Ok(vec![tool_manager::discovery::DiscoveryCandidate::new(
                self.identity.clone(),
                tool_manager::model::EvidenceLevel::Verified,
                true,
                true,
                false,
            )])
        }
        fn reprobe(
            &self,
            _query: &tool_manager::discovery::DiscoveryQuery,
        ) -> Result<Vec<tool_manager::discovery::DiscoveryCandidate>, adapters::AdapterError>
        {
            Ok(vec![tool_manager::discovery::DiscoveryCandidate::new(
                self.identity.clone(),
                tool_manager::model::EvidenceLevel::Verified,
                true,
                true,
                false,
            )])
        }
        fn remote_update_status(
            &self,
            _query: &tool_manager::discovery::DiscoveryQuery,
        ) -> Result<RemoteUpdateStatus, adapters::AdapterError> {
            Ok(if self.update_available {
                RemoteUpdateStatus::UpdateAvailable
            } else {
                RemoteUpdateStatus::Current
            })
        }
        fn manager_pin(
            &self,
        ) -> Result<tool_manager::model::ManagerPinState, adapters::AdapterError> {
            Ok(tool_manager::model::ManagerPinState::Unpinned)
        }
        fn update_argv(&self) -> Option<Vec<String>> {
            self.update_argv.clone()
        }
    }

    #[derive(Debug, Default)]
    struct CountingProbeRunner {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl tool_manager::probe::ProbeRunner for CountingProbeRunner {
        fn run(
            &self,
            _request: &tool_manager::probe::ProbeRequest,
        ) -> tool_manager::probe::ProbeOutput {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tool_manager::probe::ProbeOutput {
                exit_code: Some(0),
                ..Default::default()
            }
        }
    }

    #[test]
    fn upgrade_blocked_held_tool_creates_no_journal() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        unsafe { std::env::remove_var("CCYNC_ASSUME_YES") };

        let (tool_id, decl, state) = setup_test_tool(
            tmp.path(),
            "held-tool",
            tool_manager::model::AdapterKind::Cargo,
            tool_manager::model::HoldPolicy::Held,
        );
        let mut decls = std::collections::BTreeMap::new();
        decls.insert(tool_id.clone(), decl);
        store::replace_declarations(&decls).unwrap();
        let mut states = std::collections::BTreeMap::new();
        states.insert(tool_id, state);
        store::commit_state(&states).unwrap();

        let lock_guard = tool_manager::lock::LifecycleLock::acquire().unwrap();
        let exit = cmd_upgrade(
            &["upgrade".into(), "tool:held-tool".into(), "--yes".into()],
            Some(&lock_guard),
        );
        restore_saved_env(&saved);

        assert_eq!(exit, ExitCode::UpdatesAvailable); // exit code 3
        let journal_path = ccync_foundation::paths::tool_operation_journal_path();
        assert!(
            journal_path.is_none() || !journal_path.unwrap().exists(),
            "blocked held tool must create no journal"
        );
    }

    #[test]
    fn upgrade_unsupported_tool_creates_no_journal() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        unsafe { std::env::remove_var("CCYNC_ASSUME_YES") };

        let (tool_id, decl, state) = setup_test_tool(
            tmp.path(),
            "manual-tool",
            tool_manager::model::AdapterKind::ManualPath,
            tool_manager::model::HoldPolicy::Unheld,
        );
        let mut decls = std::collections::BTreeMap::new();
        decls.insert(tool_id.clone(), decl);
        store::replace_declarations(&decls).unwrap();
        let mut states = std::collections::BTreeMap::new();
        states.insert(tool_id, state);
        store::commit_state(&states).unwrap();

        let lock_guard = tool_manager::lock::LifecycleLock::acquire().unwrap();
        let exit = cmd_upgrade(
            &["upgrade".into(), "tool:manual-tool".into(), "--yes".into()],
            Some(&lock_guard),
        );
        restore_saved_env(&saved);

        assert_eq!(exit, ExitCode::UpdatesAvailable); // exit code 3
        let journal_path = ccync_foundation::paths::tool_operation_journal_path();
        assert!(
            journal_path.is_none() || !journal_path.unwrap().exists(),
            "unsupported tool must create no journal"
        );
    }

    #[test]
    fn upgrade_unsafe_tool_id_returns_usage() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());

        let lock_guard = tool_manager::lock::LifecycleLock::acquire().unwrap();
        let exit = cmd_upgrade(&["upgrade".into(), "tool:../bad".into()], Some(&lock_guard));
        restore_saved_env(&saved);

        assert_eq!(
            exit,
            ExitCode::Usage,
            "unsafe tool id must return Usage (64)"
        );
    }

    #[test]
    fn upgrade_unchanged_bare_plugin_id_and_no_target_paths() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        let ccync_dir = tmp.path().join(".ccync");
        std::fs::create_dir_all(&ccync_dir).unwrap();
        std::fs::write(
            ccync_dir.join("plugins.json"),
            r#"{"_personalPlugins":[{"pluginId":"some-plugin","installStrategy":"personal-git-clone","source":"https://example.invalid/a.git","pin":"oldsha"}]}"#,
        )
        .unwrap();

        // Bare upgrade with no target and dry-run
        let exit_bare = cmd_upgrade(&["upgrade".into(), "--dry-run".into()], None);
        assert_eq!(exit_bare, ExitCode::Success);

        // Bare plugin id
        let exit_plugin_bare = cmd_upgrade(
            &["upgrade".into(), "some-plugin".into(), "--dry-run".into()],
            None,
        );
        assert_eq!(exit_plugin_bare, ExitCode::Success);

        // Explicit plugin: id
        let exit_plugin_explicit = cmd_upgrade(
            &[
                "upgrade".into(),
                "plugin:some-plugin".into(),
                "--dry-run".into(),
            ],
            None,
        );
        assert_eq!(exit_plugin_explicit, ExitCode::Success);

        restore_saved_env(&saved);
    }

    #[test]
    fn upgrade_all_reasons_and_batch_eligibility() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        unsafe { std::env::set_var("CCYNC_ASSUME_YES", "1") };

        let (t1, d1, s1) = setup_test_tool(
            tmp.path(),
            "held-t",
            tool_manager::model::AdapterKind::Cargo,
            tool_manager::model::HoldPolicy::Held,
        );
        let (t2, d2, s2) = setup_test_tool(
            tmp.path(),
            "manual-t",
            tool_manager::model::AdapterKind::ManualPath,
            tool_manager::model::HoldPolicy::Unheld,
        );
        let mut decls = std::collections::BTreeMap::new();
        decls.insert(t1.clone(), d1.clone());
        decls.insert(t2.clone(), d2.clone());
        store::replace_declarations(&decls).unwrap();

        let mut states = std::collections::BTreeMap::new();
        states.insert(t1, s1);
        states.insert(t2, s2);
        store::commit_state(&states).unwrap();

        // Also test batch_eligibility directly to verify reasons
        let list = vec![d1, d2];
        let eligibility = tool_manager::upgrade::batch_eligibility(&list);
        assert_eq!(eligibility.len(), 2);
        let held_item = eligibility
            .iter()
            .find(|e| e.id.as_str() == "held-t")
            .unwrap();
        assert!(!held_item.eligible);
        assert!(held_item.reasons.iter().any(|r| r.contains("held")));

        let manual_item = eligibility
            .iter()
            .find(|e| e.id.as_str() == "manual-t")
            .unwrap();
        assert!(!manual_item.eligible);
        assert!(manual_item
            .reasons
            .iter()
            .any(|r| r.contains("manual-path")));

        // Now run cmd_upgrade with --all --dry-run
        let lock_guard = tool_manager::lock::LifecycleLock::acquire().unwrap();
        let exit = cmd_upgrade(
            &["upgrade".into(), "--all".into(), "--dry-run".into()],
            Some(&lock_guard),
        );
        restore_saved_env(&saved);
        assert_eq!(exit, ExitCode::Success);
    }

    #[test]
    fn upgrade_missing_yes_creates_no_journal() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        unsafe { std::env::remove_var("CCYNC_ASSUME_YES") };

        let (tool_id, decl, state) = setup_test_tool(
            tmp.path(),
            "valid-tool",
            tool_manager::model::AdapterKind::Cargo,
            tool_manager::model::HoldPolicy::Unheld,
        );
        let mut decls = std::collections::BTreeMap::new();
        decls.insert(tool_id.clone(), decl.clone());
        store::replace_declarations(&decls).unwrap();
        let mut states = std::collections::BTreeMap::new();
        states.insert(tool_id.clone(), state);
        store::commit_state(&states).unwrap();

        let mock_adapter = MockAdapter {
            kind: tool_manager::model::AdapterKind::Cargo,
            update_supported: true,
            update_available: true,
            update_argv: Some(vec!["cargo".into(), "install".into()]),
            identity: decl.identity.clone(),
        };
        let adapters: Vec<&dyn ToolAdapter> = vec![&mock_adapter];
        let runner = CountingProbeRunner::default();

        let lock_guard = tool_manager::lock::LifecycleLock::acquire().unwrap();
        // In non-interactive test environment without --yes, confirmation fails -> ExitCode::Usage
        let exit = upgrade_one_tool_internal(
            &["upgrade".into(), "tool:valid-tool".into()],
            Some(&lock_guard),
            &tool_id,
            false,
            false,
            Some(&adapters),
            &runner,
        );
        restore_saved_env(&saved);

        assert_eq!(
            exit,
            ExitCode::UpdatesAvailable,
            "without --yes, a proven update remains confirmation-required"
        );
        let journal_path = ccync_foundation::paths::tool_operation_journal_path();
        assert!(
            journal_path.is_none() || !journal_path.unwrap().exists(),
            "missing confirmation must create no journal"
        );
    }

    #[test]
    fn upgrade_rejected_retry_flag_without_matching_recovery_state() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        unsafe { std::env::set_var("CCYNC_ASSUME_YES", "1") };

        let (tool_id, decl, state) = setup_test_tool(
            tmp.path(),
            "demo-tool",
            tool_manager::model::AdapterKind::Cargo,
            tool_manager::model::HoldPolicy::Unheld,
        );
        let mut decls = std::collections::BTreeMap::new();
        decls.insert(tool_id.clone(), decl);
        store::replace_declarations(&decls).unwrap();
        let mut states = std::collections::BTreeMap::new();
        states.insert(tool_id, state);
        store::commit_state(&states).unwrap();

        let lock_guard = tool_manager::lock::LifecycleLock::acquire().unwrap();
        // Case A: No journal exists
        let exit_no_journal = cmd_upgrade(
            &[
                "upgrade".into(),
                "tool:demo-tool".into(),
                "--retry-manager-step".into(),
                "--yes".into(),
            ],
            Some(&lock_guard),
        );
        assert_eq!(
            exit_no_journal,
            ExitCode::Usage,
            "retry flag without journal must return Usage"
        );

        // Case B: Journal exists but is at Prepared (not ManagerOutcomeUnknown)
        let mut journal = tool_manager::journal::OperationJournal::new(
            ToolId::new("demo-tool").unwrap(),
            tool_manager::journal::OperationKind::Upgrade,
            tool_manager::journal::RetryIntent::Upgrade {
                tool_id: ToolId::new("demo-tool").unwrap(),
            },
            None,
            PathBuf::new(),
            "digest".into(),
        );
        journal.manager_step_state = tool_manager::journal::ManagerStepState::NotStarted;
        journal.create(&lock_guard).unwrap();

        let exit_wrong_state = cmd_upgrade(
            &[
                "upgrade".into(),
                "tool:demo-tool".into(),
                "--retry-manager-step".into(),
                "--yes".into(),
            ],
            Some(&lock_guard),
        );
        restore_saved_env(&saved);

        assert_eq!(
            exit_wrong_state,
            ExitCode::Usage,
            "retry flag without manager-outcome-unknown journal must return Usage"
        );
    }

    #[test]
    fn upgrade_unknown_manager_outcome_not_retried_automatically() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        unsafe { std::env::set_var("CCYNC_ASSUME_YES", "1") };

        let (tool_id, decl, mut state) = setup_test_tool(
            tmp.path(),
            "demo-tool",
            tool_manager::model::AdapterKind::Cargo,
            tool_manager::model::HoldPolicy::Unheld,
        );
        let mock_adapter = MockAdapter {
            kind: tool_manager::model::AdapterKind::Cargo,
            update_supported: true,
            update_available: false,
            update_argv: Some(vec!["cargo".into(), "install".into()]),
            identity: decl.identity.clone(),
        };
        let runner = CountingProbeRunner::default();
        let e0 = tool_manager::evidence::capture_evidence(&mock_adapter, &runner, &decl.identity)
            .unwrap();
        #[cfg(unix)]
        let file_id = serde_json::json!({"dev": e0.file_identity.dev, "ino": e0.file_identity.ino});
        #[cfg(windows)]
        let file_id = serde_json::json!({"volumeSerialNumber": e0.file_identity.volume_serial_number, "fileIndex": e0.file_identity.file_index});
        state.source_evidence = serde_json::json!({
            "adapterFields": e0.adapter_fields,
            "resolvedTarget": e0.resolved_target,
            "fileIdentity": file_id,
            "sha256": e0.sha256,
        });

        let mut decls = std::collections::BTreeMap::new();
        decls.insert(tool_id.clone(), decl.clone());
        store::replace_declarations(&decls).unwrap();
        let mut states = std::collections::BTreeMap::new();
        states.insert(tool_id.clone(), state);
        store::commit_state(&states).unwrap();

        let lock_guard = tool_manager::lock::LifecycleLock::acquire().unwrap();
        // Create journal with ManagerOutcomeUnknown
        let mut journal = tool_manager::journal::OperationJournal::new(
            tool_id.clone(),
            tool_manager::journal::OperationKind::Upgrade,
            tool_manager::journal::RetryIntent::Upgrade {
                tool_id: tool_id.clone(),
            },
            None,
            PathBuf::new(),
            "sha256:aeb09e0ec07e54dcba9812834ce293fb6f271657b691396ce161b9a91c4d7ecf".into(),
        );
        journal.manager_step_state = tool_manager::journal::ManagerStepState::ManagerOutcomeUnknown;
        journal.create(&lock_guard).unwrap();

        let adapters: Vec<&dyn ToolAdapter> = vec![&mock_adapter];
        // Reset call count from setup capture_evidence
        runner.calls.store(0, std::sync::atomic::Ordering::SeqCst);

        // Run WITHOUT --retry-manager-step
        let exit = upgrade_one_tool_internal(
            &["upgrade".into(), "tool:demo-tool".into(), "--yes".into()],
            Some(&lock_guard),
            &tool_id,
            false,
            false,
            Some(&adapters),
            &runner,
        );
        restore_saved_env(&saved);

        assert_eq!(
            exit,
            ExitCode::UpdatesAvailable,
            "unknown manager outcome must exit 3 (UpdatesAvailable) without claiming success"
        );
        assert_eq!(
            runner.calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "manager command must not be retried automatically"
        );
    }

    #[test]
    fn upgrade_explicit_retry_after_confirmation() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        unsafe { std::env::set_var("CCYNC_ASSUME_YES", "1") };

        let (tool_id, mut decl, mut state) = setup_test_tool(
            tmp.path(),
            "demo-tool",
            tool_manager::model::AdapterKind::Cargo,
            tool_manager::model::HoldPolicy::Unheld,
        );
        decl.delivery_id = tool_manager::model::DeliveryId::RtkPath;
        decl.identity.fields.insert("name".into(), "rtk".into());

        let mock_adapter = MockAdapter {
            kind: tool_manager::model::AdapterKind::Cargo,
            update_supported: true,
            update_available: true,
            update_argv: Some(vec!["cargo".into(), "install".into()]),
            identity: decl.identity.clone(),
        };
        let runner = CountingProbeRunner::default();
        let e0 = tool_manager::evidence::capture_evidence(&mock_adapter, &runner, &decl.identity)
            .unwrap();
        #[cfg(unix)]
        let file_id = serde_json::json!({"dev": e0.file_identity.dev, "ino": e0.file_identity.ino});
        #[cfg(windows)]
        let file_id = serde_json::json!({"volumeSerialNumber": e0.file_identity.volume_serial_number, "fileIndex": e0.file_identity.file_index});
        state.source_evidence = serde_json::json!({
            "adapterFields": e0.adapter_fields,
            "resolvedTarget": e0.resolved_target,
            "fileIdentity": file_id,
            "sha256": e0.sha256,
        });

        let mut decls = std::collections::BTreeMap::new();
        decls.insert(tool_id.clone(), decl.clone());
        store::replace_declarations(&decls).unwrap();
        let mut states = std::collections::BTreeMap::new();
        states.insert(tool_id.clone(), state);
        store::commit_state(&states).unwrap();

        let lock_guard = tool_manager::lock::LifecycleLock::acquire().unwrap();
        // Create journal with ManagerOutcomeUnknown
        let mut journal = tool_manager::journal::OperationJournal::new(
            tool_id.clone(),
            tool_manager::journal::OperationKind::Upgrade,
            tool_manager::journal::RetryIntent::Upgrade {
                tool_id: tool_id.clone(),
            },
            None,
            PathBuf::new(),
            "sha256:aeb09e0ec07e54dcba9812834ce293fb6f271657b691396ce161b9a91c4d7ecf".into(),
        );
        journal.manager_step_state = tool_manager::journal::ManagerStepState::ManagerOutcomeUnknown;
        journal.create(&lock_guard).unwrap();

        let adapters: Vec<&dyn ToolAdapter> = vec![&mock_adapter];
        // Reset runner calls after capture_evidence
        runner.calls.store(0, std::sync::atomic::Ordering::SeqCst);

        // Run WITH --retry-manager-step and --yes
        let exit = upgrade_one_tool_internal(
            &[
                "upgrade".into(),
                "tool:demo-tool".into(),
                "--retry-manager-step".into(),
                "--yes".into(),
            ],
            Some(&lock_guard),
            &tool_id,
            false,
            true, // retry = true
            Some(&adapters),
            &runner,
        );
        restore_saved_env(&saved);

        // The manager step was retried!
        assert!(
            runner.calls.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "manager step must be rerun on explicit retry after confirmation"
        );
        assert_ne!(exit, ExitCode::Usage);
    }

    #[test]
    fn committed_upgrade_guidance_contains_reload_line() {
        let agents = vec!["Claude".into()];
        let guidance = upgrade_reload_guidance(true, &agents).unwrap();
        assert!(guidance.contains("reload advised; loaded generation unknown"));
        assert!(upgrade_reload_guidance(false, &[]).is_none());
        assert!(upgrade_projection_changed(
            super::super::tool_output::ToolOutcome::Committed,
            &agents
        ));
        assert!(!upgrade_projection_changed(
            super::super::tool_output::ToolOutcome::Committed,
            &[]
        ));
        assert_eq!(
            super::super::tool_output::dry_run_value(),
            "unknown-until-executed"
        );
    }

    #[derive(Debug)]
    struct StatusMockAdapter {
        kind: tool_manager::model::AdapterKind,
        update_supported: bool,
        remote_status: Result<RemoteUpdateStatus, adapters::AdapterError>,
        update_argv: Option<Vec<String>>,
        identity: tool_manager::model::InstallationIdentity,
    }

    impl ToolAdapter for StatusMockAdapter {
        fn kind(&self) -> tool_manager::model::AdapterKind {
            self.kind
        }
        fn capabilities(&self) -> adapters::AdapterCapabilities {
            adapters::AdapterCapabilities {
                local_enumeration: adapters::CapabilityState::Supported,
                executable_mapping: adapters::CapabilityState::Supported,
                installed_version_lookup: adapters::CapabilityState::Supported,
                remote_discovery: adapters::CapabilityState::Supported,
                package_update: if self.update_supported {
                    adapters::CapabilityState::Supported
                } else {
                    adapters::CapabilityState::Gated
                },
            }
        }
        fn discover(
            &self,
            _query: &tool_manager::discovery::DiscoveryQuery,
        ) -> Result<Vec<tool_manager::discovery::DiscoveryCandidate>, adapters::AdapterError> {
            Ok(vec![tool_manager::discovery::DiscoveryCandidate::new(
                self.identity.clone(),
                tool_manager::model::EvidenceLevel::Verified,
                true,
                true,
                false,
            )])
        }
        fn reprobe(
            &self,
            _query: &tool_manager::discovery::DiscoveryQuery,
        ) -> Result<Vec<tool_manager::discovery::DiscoveryCandidate>, adapters::AdapterError> {
            Ok(vec![tool_manager::discovery::DiscoveryCandidate::new(
                self.identity.clone(),
                tool_manager::model::EvidenceLevel::Verified,
                true,
                true,
                false,
            )])
        }
        fn remote_update_status(
            &self,
            _query: &tool_manager::discovery::DiscoveryQuery,
        ) -> Result<RemoteUpdateStatus, adapters::AdapterError> {
            self.remote_status.clone()
        }
        fn manager_pin(
            &self,
        ) -> Result<tool_manager::model::ManagerPinState, adapters::AdapterError> {
            Ok(tool_manager::model::ManagerPinState::Unpinned)
        }
        fn update_argv(&self) -> Option<Vec<String>> {
            self.update_argv.clone()
        }
    }

    #[derive(Debug, Default)]
    struct MutatingProbeRunner {
        calls: std::sync::atomic::AtomicUsize,
        target_file: std::sync::Mutex<Option<PathBuf>>,
    }

    impl tool_manager::probe::ProbeRunner for MutatingProbeRunner {
        fn run(
            &self,
            _request: &tool_manager::probe::ProbeRequest,
        ) -> tool_manager::probe::ProbeOutput {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Ok(guard) = self.target_file.lock() {
                if let Some(ref path) = *guard {
                    let _ = std::fs::write(path, b"mutated-bytes-after-manager-run");
                }
            }
            tool_manager::probe::ProbeOutput {
                exit_code: Some(0),
                ..Default::default()
            }
        }
    }

    #[test]
    fn upgrade_fresh_flow_update_available_dry_run_and_committed() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        unsafe { std::env::set_var("CCYNC_ASSUME_YES", "1") };

        let (tool_id, mut decl, mut state) = setup_test_tool(
            tmp.path(),
            "demo-tool",
            tool_manager::model::AdapterKind::Cargo,
            tool_manager::model::HoldPolicy::Unheld,
        );
        decl.delivery_id = tool_manager::model::DeliveryId::RtkPath;
        decl.identity.fields.insert("name".into(), "rtk".into());

        let exe_path = PathBuf::from(decl.identity.fields.get("executable").unwrap());
        let mock_adapter = StatusMockAdapter {
            kind: tool_manager::model::AdapterKind::Cargo,
            update_supported: true,
            remote_status: Ok(RemoteUpdateStatus::UpdateAvailable),
            update_argv: Some(vec!["cargo".into(), "install".into()]),
            identity: decl.identity.clone(),
        };
        let runner = MutatingProbeRunner {
            calls: std::sync::atomic::AtomicUsize::new(0),
            target_file: std::sync::Mutex::new(None),
        };
        let e0 = tool_manager::evidence::capture_evidence(&mock_adapter, &runner, &decl.identity)
            .unwrap();
        #[cfg(unix)]
        let file_id = serde_json::json!({"dev": e0.file_identity.dev, "ino": e0.file_identity.ino});
        #[cfg(windows)]
        let file_id = serde_json::json!({"volumeSerialNumber": e0.file_identity.volume_serial_number, "fileIndex": e0.file_identity.file_index});
        state.source_evidence = serde_json::json!({
            "adapterFields": e0.adapter_fields,
            "resolvedTarget": e0.resolved_target,
            "fileIdentity": file_id,
            "sha256": e0.sha256,
        });

        let mut decls = std::collections::BTreeMap::new();
        decls.insert(tool_id.clone(), decl);
        store::replace_declarations(&decls).unwrap();
        let mut states = std::collections::BTreeMap::new();
        states.insert(tool_id.clone(), state);
        store::commit_state(&states).unwrap();

        let lock_guard = tool_manager::lock::LifecycleLock::acquire().unwrap();
        let adapters: Vec<&dyn ToolAdapter> = vec![&mock_adapter];
        runner.calls.store(0, std::sync::atomic::Ordering::SeqCst);

        // 1. Dry run - human mode
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit_dry = upgrade_one_tool_internal_to(
            &["upgrade".into(), "tool:demo-tool".into(), "--dry-run".into()],
            Some(&lock_guard),
            &tool_id,
            true,
            false,
            Some(&adapters),
            &runner,
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(exit_dry, ExitCode::Success);
        assert_eq!(runner.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let out_str = String::from_utf8(stdout).unwrap();
        assert!(out_str.contains("dry-run"));
        assert!(stderr.is_empty());

        // 2. Dry run - JSON mode
        let mut stdout_json = Vec::new();
        let mut stderr_json = Vec::new();
        let exit_dry_json = upgrade_one_tool_internal_to(
            &["upgrade".into(), "tool:demo-tool".into(), "--dry-run".into(), "--json".into()],
            Some(&lock_guard),
            &tool_id,
            true,
            false,
            Some(&adapters),
            &runner,
            &mut stdout_json,
            &mut stderr_json,
        );
        assert_eq!(exit_dry_json, ExitCode::Success);
        assert_eq!(runner.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(stderr_json.is_empty());
        let doc: serde_json::Value = serde_json::from_slice(&stdout_json).unwrap();
        assert_eq!(doc["outcome"], "dry-run");
        assert_eq!(doc["command"], "upgrade tool");
        assert_eq!(doc["schemaVersion"], 1);
        assert!(doc["updateState"].is_null());
        assert_eq!(doc["updateFreshness"], "stale");
        assert_eq!(doc["projectionChanged"], false);
        assert!(doc["reloadAdvice"].is_null());

        // Set target file so runner mutates executable when executed
        *runner.target_file.lock().unwrap() = Some(exe_path);

        // 3. Committed upgrade with --yes
        let mut stdout_commit = Vec::new();
        let mut stderr_commit = Vec::new();
        let exit_commit = upgrade_one_tool_internal_to(
            &["upgrade".into(), "tool:demo-tool".into(), "--yes".into(), "--json".into()],
            Some(&lock_guard),
            &tool_id,
            false,
            false,
            Some(&adapters),
            &runner,
            &mut stdout_commit,
            &mut stderr_commit,
        );
        restore_saved_env(&saved);

        assert_eq!(exit_commit, ExitCode::Success);
        assert!(runner.calls.load(std::sync::atomic::Ordering::SeqCst) > 0);
        assert!(stderr_commit.is_empty());
        let doc_commit: serde_json::Value = serde_json::from_slice(&stdout_commit).unwrap();
        assert_eq!(doc_commit["outcome"], "committed");
        assert!(doc_commit["updateState"].is_null());
        assert_eq!(doc_commit["updateFreshness"], "stale");
    }

    #[test]
    fn upgrade_recovery_first_ordering_bypasses_gated_package_update() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        unsafe { std::env::set_var("CCYNC_ASSUME_YES", "1") };

        let (tool_id, mut decl, mut state) = setup_test_tool(
            tmp.path(),
            "demo-tool",
            tool_manager::model::AdapterKind::Cargo,
            tool_manager::model::HoldPolicy::Unheld,
        );
        decl.delivery_id = tool_manager::model::DeliveryId::RtkPath;
        decl.identity.fields.insert("name".into(), "rtk".into());
        let exe_path = PathBuf::from(decl.identity.fields.get("executable").unwrap());

        // Adapter has update_supported = false (Gated)
        let mock_adapter = StatusMockAdapter {
            kind: tool_manager::model::AdapterKind::Cargo,
            update_supported: false,
            remote_status: Ok(RemoteUpdateStatus::UpdateAvailable),
            update_argv: Some(vec!["cargo".into(), "install".into()]),
            identity: decl.identity.clone(),
        };
        let runner = MutatingProbeRunner {
            calls: std::sync::atomic::AtomicUsize::new(0),
            target_file: std::sync::Mutex::new(None),
        };
        let e0 = tool_manager::evidence::capture_evidence(&mock_adapter, &runner, &decl.identity)
            .unwrap();
        #[cfg(unix)]
        let file_id = serde_json::json!({"dev": e0.file_identity.dev, "ino": e0.file_identity.ino});
        #[cfg(windows)]
        let file_id = serde_json::json!({"volumeSerialNumber": e0.file_identity.volume_serial_number, "fileIndex": e0.file_identity.file_index});
        state.source_evidence = serde_json::json!({
            "adapterFields": e0.adapter_fields,
            "resolvedTarget": e0.resolved_target,
            "fileIdentity": file_id,
            "sha256": e0.sha256,
        });

        let mut decls = std::collections::BTreeMap::new();
        decls.insert(tool_id.clone(), decl);
        store::replace_declarations(&decls).unwrap();
        let mut states = std::collections::BTreeMap::new();
        states.insert(tool_id.clone(), state);
        store::commit_state(&states).unwrap();

        let lock_guard = tool_manager::lock::LifecycleLock::acquire().unwrap();
        let adapters: Vec<&dyn ToolAdapter> = vec![&mock_adapter];

        // Without journal, running upgrade blocks because package_update is Gated:
        let mut stdout_blocked = Vec::new();
        let mut stderr_blocked = Vec::new();
        let exit_blocked = upgrade_one_tool_internal_to(
            &["upgrade".into(), "tool:demo-tool".into(), "--yes".into(), "--json".into()],
            Some(&lock_guard),
            &tool_id,
            false,
            false,
            Some(&adapters),
            &runner,
            &mut stdout_blocked,
            &mut stderr_blocked,
        );
        assert_eq!(exit_blocked, ExitCode::UpdatesAvailable); // 3 (Blocked)
        let doc_blocked: serde_json::Value = serde_json::from_slice(&stdout_blocked).unwrap();
        assert_eq!(doc_blocked["outcome"], "blocked");

        // Now create a valid same-tool journal in ManagerOutcomeUnknown
        let mut journal = tool_manager::journal::OperationJournal::new(
            tool_id.clone(),
            tool_manager::journal::OperationKind::Upgrade,
            tool_manager::journal::RetryIntent::Upgrade {
                tool_id: tool_id.clone(),
            },
            None,
            PathBuf::new(),
            "sha256:aeb09e0ec07e54dcba9812834ce293fb6f271657b691396ce161b9a91c4d7ecf".into(),
        );
        journal.manager_step_state = tool_manager::journal::ManagerStepState::ManagerOutcomeUnknown;
        journal.create(&lock_guard).unwrap();

        runner.calls.store(0, std::sync::atomic::Ordering::SeqCst);
        *runner.target_file.lock().unwrap() = Some(exe_path);

        // With recovery, the package_update gate is bypassed:
        let mut stdout_rec = Vec::new();
        let mut stderr_rec = Vec::new();
        let exit_rec = upgrade_one_tool_internal_to(
            &[
                "upgrade".into(),
                "tool:demo-tool".into(),
                "--retry-manager-step".into(),
                "--yes".into(),
                "--json".into(),
            ],
            Some(&lock_guard),
            &tool_id,
            false,
            true,
            Some(&adapters),
            &runner,
            &mut stdout_rec,
            &mut stderr_rec,
        );
        restore_saved_env(&saved);

        assert_eq!(exit_rec, ExitCode::Success);
        assert!(runner.calls.load(std::sync::atomic::Ordering::SeqCst) > 0);
        let doc_rec: serde_json::Value = serde_json::from_slice(&stdout_rec).unwrap();
        assert_eq!(doc_rec["outcome"], "committed");
    }

    #[test]
    fn upgrade_invalid_journals_handled() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        unsafe { std::env::set_var("CCYNC_ASSUME_YES", "1") };

        let (tool_id, decl, state) = setup_test_tool(
            tmp.path(),
            "demo-tool",
            tool_manager::model::AdapterKind::Cargo,
            tool_manager::model::HoldPolicy::Unheld,
        );
        let mut decls = std::collections::BTreeMap::new();
        decls.insert(tool_id.clone(), decl.clone());
        store::replace_declarations(&decls).unwrap();
        let mut states = std::collections::BTreeMap::new();
        states.insert(tool_id.clone(), state);
        store::commit_state(&states).unwrap();

        let lock_guard = tool_manager::lock::LifecycleLock::acquire().unwrap();
        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        std::fs::create_dir_all(journal_path.parent().unwrap()).unwrap();

        let mock_adapter = StatusMockAdapter {
            kind: tool_manager::model::AdapterKind::Cargo,
            update_supported: true,
            remote_status: Ok(RemoteUpdateStatus::UpdateAvailable),
            update_argv: Some(vec!["cargo".into(), "install".into()]),
            identity: decl.identity.clone(),
        };
        let adapters: Vec<&dyn ToolAdapter> = vec![&mock_adapter];
        let runner = CountingProbeRunner::default();

        // Case A: Malformed journal JSON -> CheckFailed (1)
        std::fs::write(&journal_path, b"{broken json").unwrap();
        let mut stdout_bad = Vec::new();
        let mut stderr_bad = Vec::new();
        let exit_bad = upgrade_one_tool_internal_to(
            &["upgrade".into(), "tool:demo-tool".into(), "--json".into()],
            Some(&lock_guard),
            &tool_id,
            false,
            false,
            Some(&adapters),
            &runner,
            &mut stdout_bad,
            &mut stderr_bad,
        );
        assert_eq!(exit_bad, ExitCode::Error); // 1
        let doc_bad: serde_json::Value = serde_json::from_slice(&stdout_bad).unwrap();
        assert_eq!(doc_bad["outcome"], "check-failed");

        // Clean up malformed journal before creating foreign journal
        std::fs::remove_file(&journal_path).ok();

        // Case B: Foreign tool journal -> RecoveryRequired (3)
        let foreign_id = ToolId::new("other-tool").unwrap();
        let mut foreign_journal = tool_manager::journal::OperationJournal::new(
            foreign_id.clone(),
            tool_manager::journal::OperationKind::Upgrade,
            tool_manager::journal::RetryIntent::Upgrade {
                tool_id: foreign_id,
            },
            None,
            PathBuf::new(),
            "sha256:foreign".into(),
        );
        foreign_journal.stage = tool_manager::journal::OperationStage::Prepared;
        foreign_journal.create(&lock_guard).unwrap();

        let mut stdout_foreign = Vec::new();
        let mut stderr_foreign = Vec::new();
        let exit_foreign = upgrade_one_tool_internal_to(
            &["upgrade".into(), "tool:demo-tool".into(), "--json".into()],
            Some(&lock_guard),
            &tool_id,
            false,
            false,
            Some(&adapters),
            &runner,
            &mut stdout_foreign,
            &mut stderr_foreign,
        );
        restore_saved_env(&saved);

        assert_eq!(exit_foreign, ExitCode::UpdatesAvailable); // 3 (RecoveryRequired)
        let doc_foreign: serde_json::Value = serde_json::from_slice(&stdout_foreign).unwrap();
        assert_eq!(doc_foreign["outcome"], "recovery-required");
    }

    #[test]
    fn upgrade_non_proof_states_prevent_manager_execution() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_env(tmp.path());
        unsafe { std::env::set_var("CCYNC_ASSUME_YES", "1") };

        let (tool_id, decl, state) = setup_test_tool(
            tmp.path(),
            "demo-tool",
            tool_manager::model::AdapterKind::Cargo,
            tool_manager::model::HoldPolicy::Unheld,
        );
        let mut decls = std::collections::BTreeMap::new();
        decls.insert(tool_id.clone(), decl.clone());
        store::replace_declarations(&decls).unwrap();
        let mut states = std::collections::BTreeMap::new();
        states.insert(tool_id.clone(), state);
        store::commit_state(&states).unwrap();

        let lock_guard = tool_manager::lock::LifecycleLock::acquire().unwrap();
        let runner = CountingProbeRunner::default();

        // 1. Current
        let mock_current = StatusMockAdapter {
            kind: tool_manager::model::AdapterKind::Cargo,
            update_supported: true,
            remote_status: Ok(RemoteUpdateStatus::Current),
            update_argv: Some(vec!["cargo".into(), "install".into()]),
            identity: decl.identity.clone(),
        };
        let adapters_current: Vec<&dyn ToolAdapter> = vec![&mock_current];
        runner.calls.store(0, std::sync::atomic::Ordering::SeqCst);
        let mut stdout_current = Vec::new();
        let mut stderr_current = Vec::new();
        let exit_current = upgrade_one_tool_internal_to(
            &["upgrade".into(), "tool:demo-tool".into(), "--yes".into(), "--json".into()],
            Some(&lock_guard),
            &tool_id,
            false,
            false,
            Some(&adapters_current),
            &runner,
            &mut stdout_current,
            &mut stderr_current,
        );
        assert_eq!(exit_current, ExitCode::Success);
        assert_eq!(runner.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let doc_curr: serde_json::Value = serde_json::from_slice(&stdout_current).unwrap();
        assert_eq!(doc_curr["outcome"], "current");
        assert_eq!(doc_curr["updateState"], "current");
        assert_eq!(doc_curr["updateFreshness"], "fresh");

        // 2. Unsupported
        let mock_unsupported = StatusMockAdapter {
            kind: tool_manager::model::AdapterKind::Cargo,
            update_supported: true,
            remote_status: Ok(RemoteUpdateStatus::Unsupported),
            update_argv: Some(vec!["cargo".into(), "install".into()]),
            identity: decl.identity.clone(),
        };
        let adapters_unsupported: Vec<&dyn ToolAdapter> = vec![&mock_unsupported];
        runner.calls.store(0, std::sync::atomic::Ordering::SeqCst);
        let mut stdout_unsupported = Vec::new();
        let mut stderr_unsupported = Vec::new();
        let exit_unsupported = upgrade_one_tool_internal_to(
            &["upgrade".into(), "tool:demo-tool".into(), "--yes".into(), "--json".into()],
            Some(&lock_guard),
            &tool_id,
            false,
            false,
            Some(&adapters_unsupported),
            &runner,
            &mut stdout_unsupported,
            &mut stderr_unsupported,
        );
        assert_eq!(exit_unsupported, ExitCode::UpdatesAvailable); // 3 (Blocked)
        assert_eq!(runner.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let doc_unsupp: serde_json::Value = serde_json::from_slice(&stdout_unsupported).unwrap();
        assert_eq!(doc_unsupp["outcome"], "blocked");
        assert_eq!(doc_unsupp["updateState"], "not-checked");
        assert_eq!(doc_unsupp["updateFreshness"], "fresh");

        // 3. NotChecked
        let mock_not_checked = StatusMockAdapter {
            kind: tool_manager::model::AdapterKind::Cargo,
            update_supported: true,
            remote_status: Ok(RemoteUpdateStatus::NotChecked),
            update_argv: Some(vec!["cargo".into(), "install".into()]),
            identity: decl.identity.clone(),
        };
        let adapters_not_checked: Vec<&dyn ToolAdapter> = vec![&mock_not_checked];
        runner.calls.store(0, std::sync::atomic::Ordering::SeqCst);
        let mut stdout_not_checked = Vec::new();
        let mut stderr_not_checked = Vec::new();
        let exit_not_checked = upgrade_one_tool_internal_to(
            &["upgrade".into(), "tool:demo-tool".into(), "--yes".into(), "--json".into()],
            Some(&lock_guard),
            &tool_id,
            false,
            false,
            Some(&adapters_not_checked),
            &runner,
            &mut stdout_not_checked,
            &mut stderr_not_checked,
        );
        assert_eq!(exit_not_checked, ExitCode::UpdatesAvailable); // 3 (Blocked)
        assert_eq!(runner.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let doc_notchk: serde_json::Value = serde_json::from_slice(&stdout_not_checked).unwrap();
        assert_eq!(doc_notchk["outcome"], "blocked");
        assert_eq!(doc_notchk["updateState"], "not-checked");
        assert_eq!(doc_notchk["updateFreshness"], "fresh");

        // 4. CheckFailed
        let mock_failed = StatusMockAdapter {
            kind: tool_manager::model::AdapterKind::Cargo,
            update_supported: true,
            remote_status: Err(adapters::AdapterError::Failed("mock probe failed".into())),
            update_argv: Some(vec!["cargo".into(), "install".into()]),
            identity: decl.identity.clone(),
        };
        let adapters_failed: Vec<&dyn ToolAdapter> = vec![&mock_failed];
        runner.calls.store(0, std::sync::atomic::Ordering::SeqCst);
        let mut stdout_failed = Vec::new();
        let mut stderr_failed = Vec::new();
        let exit_failed = upgrade_one_tool_internal_to(
            &["upgrade".into(), "tool:demo-tool".into(), "--yes".into(), "--json".into()],
            Some(&lock_guard),
            &tool_id,
            false,
            false,
            Some(&adapters_failed),
            &runner,
            &mut stdout_failed,
            &mut stderr_failed,
        );
        restore_saved_env(&saved);

        assert_eq!(exit_failed, ExitCode::Error); // 1
        assert_eq!(runner.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let doc_failed: serde_json::Value = serde_json::from_slice(&stdout_failed).unwrap();
        assert_eq!(doc_failed["outcome"], "check-failed");
        assert_eq!(doc_failed["updateState"], "check-failed");
        assert_eq!(doc_failed["updateFreshness"], "fresh");
    }

    #[test]
    fn upgrade_full_tool_outcome_matrix_streams_and_json() {
        use super::super::tool_output::{output_stream, OutputStream, ToolCommand, ToolOutcome};

        let outcomes = [
            ToolOutcome::Committed,
            ToolOutcome::Current,
            ToolOutcome::DryRun,
            ToolOutcome::Blocked,
            ToolOutcome::CheckFailed,
            ToolOutcome::RecoveryRequired,
        ];

        let agents = vec!["Claude".to_string(), "Codex".to_string()];

        for outcome in outcomes {
            assert!(ToolCommand::Upgrade.allows(outcome));

            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let exit = emit_tool_upgrade_to(
                true,
                outcome,
                Some("diagnostic/next"),
                Some("update-available"),
                Some("fresh"),
                outcome == ToolOutcome::Committed,
                &agents,
                &mut stdout,
                &mut stderr,
            );
            assert_eq!(exit, outcome.exit_code());
            assert!(stderr.is_empty(), "JSON output must leave stderr empty");

            let doc: serde_json::Value = serde_json::from_slice(&stdout).expect("must be valid JSON");
            assert_eq!(doc["schemaVersion"], 1);
            assert_eq!(doc["command"], "upgrade tool");
            assert_eq!(doc["outcome"], outcome.as_str());

            if outcome == ToolOutcome::Committed {
                assert!(doc["reloadAdvice"].is_string());
                assert_eq!(doc["projectionChanged"], true);
            } else {
                assert!(doc["reloadAdvice"].is_null());
            }

            let mut stdout_human = Vec::new();
            let mut stderr_human = Vec::new();
            let exit_human = emit_tool_upgrade_to(
                false,
                outcome,
                Some("action"),
                Some("update-available"),
                Some("fresh"),
                outcome == ToolOutcome::Committed,
                &agents,
                &mut stdout_human,
                &mut stderr_human,
            );
            assert_eq!(exit_human, outcome.exit_code());

            match output_stream(ToolCommand::Upgrade, outcome) {
                OutputStream::Stdout => {
                    assert!(!stdout_human.is_empty(), "stdout expected for {:?}", outcome);
                    assert!(stderr_human.is_empty(), "stderr should be empty for {:?}", outcome);
                }
                OutputStream::Stderr => {
                    assert!(stdout_human.is_empty(), "stdout should be empty for {:?}", outcome);
                    assert!(!stderr_human.is_empty(), "stderr expected for {:?}", outcome);
                }
            }
        }
    }
}
