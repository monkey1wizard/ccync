use ccync_engine::{
    tool_manager::{
        adapters::{self, ToolAdapter},
        discovery::{
            canonical_executable_path, filter_by_executable_selector, merge_candidates,
            DiscoveryCandidate, DiscoveryQuery,
        },
        lock::LifecycleGuard,
        model::{DeliveryId, ToolId},
        probe::{ProbeRunner, SystemProbeRunner},
    },
    ExitCode,
};
use std::io::{IsTerminal, Write};

#[cfg(test)]
fn untrack_reload_guidance(projection_ran: bool) -> Option<String> {
    projection_ran.then(|| super::tool_output::reload_line(&[]))
}

fn emit_outcome_to(
    command: super::tool_output::ToolCommand,
    outcome: super::tool_output::ToolOutcome,
    next_action: Option<String>,
    stderr: bool,
    projection_changed: bool,
    stdout: &mut dyn Write,
    stderr_writer: &mut dyn Write,
) -> ExitCode {
    let mut facts = super::tool_output::ToolOutcomeFacts::new(command, outcome);
    facts.next_action = next_action;
    facts = facts.with_reload(projection_changed, &[]);
    let rendered = super::tool_output::render_outcome_human(&facts);
    if stderr {
        let _ = stderr_writer.write_all(rendered.as_bytes());
    } else {
        let _ = stdout.write_all(rendered.as_bytes());
    }
    outcome.exit_code()
}

fn candidate_executable(candidate: &DiscoveryCandidate) -> String {
    candidate
        .identity
        .fields
        .get("executable")
        .or_else(|| candidate.identity.fields.get("invokedPath"))
        .map(|value| canonical_executable_path(value))
        .unwrap_or_else(|| "<unknown>".into())
}

fn candidate_summary(candidate: &DiscoveryCandidate) -> String {
    let adapter = serde_json::to_string(&candidate.identity.adapter_kind)
        .unwrap_or_else(|_| "\"unknown\"".into())
        .trim_matches('"')
        .to_owned();
    let evidence = serde_json::to_string(&candidate.evidence_level)
        .unwrap_or_else(|_| "\"unknown\"".into())
        .trim_matches('"')
        .to_owned();
    format!(
        "{} (adapter={adapter}, executable={}, evidence={evidence})",
        candidate.candidate_id,
        candidate_executable(candidate),
    )
}

fn projection_changed(report: &ccync_engine::install::ProjectionRunReport) -> bool {
    report.targets.values().any(|target| {
        matches!(
            target.status,
            ccync_engine::install::ProjectionTargetStatus::Applied
        )
    })
}

fn adapters_for(command: &str, delivery: DeliveryId) -> Vec<Box<dyn ToolAdapter>> {
    vec![
        Box::new(adapters::cargo::CargoAdapter::default()),
        Box::new(adapters::uv::UvAdapter::default()),
        Box::new(adapters::pipx::PipxAdapter::default()),
        Box::new(adapters::pip::PipAdapter::default()),
        Box::new(adapters::npm::NpmAdapter::default()),
        Box::new(adapters::pnpm::PnpmAdapter::default()),
        Box::new(adapters::brew_formula::BrewFormulaAdapter::default()),
        Box::new(adapters::brew_cask::BrewCaskAdapter::default()),
        Box::new(adapters::winget_offline::WingetOfflineAdapter::default()),
        Box::new(adapters::winget_online::WingetOnlineAdapter::default()),
        Box::new(adapters::ccync_receipt::CcyncReceiptAdapter::new()),
        Box::new(adapters::manual_path::ManualPathAdapter::new(
            command, delivery,
        )),
    ]
}

fn assumed_yes(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "--yes")
        || std::env::var("CCYNC_ASSUME_YES").is_ok_and(|value| value == "1")
}

fn confirm_with_io<R: std::io::BufRead, W: std::io::Write>(
    args: &[String],
    limited: bool,
    is_terminal: bool,
    mut reader: R,
    mut writer: W,
) -> bool {
    if assumed_yes(args) {
        return true;
    }
    if !is_terminal {
        return false;
    }
    if limited {
        let _ = writeln!(
            writer,
            "acknowledgement: limited installation evidence will be stored with this declaration"
        );
    }
    let _ = write!(writer, "Track this tool declaration? [y/N] ");
    let _ = writer.flush();
    let mut answer = String::new();
    reader.read_line(&mut answer).is_ok() && answer.trim().eq_ignore_ascii_case("y")
}

pub(crate) fn cmd_track(args: &[String], guard: &LifecycleGuard) -> ExitCode {
    cmd_track_with_runner(args, guard, &SystemProbeRunner)
}

pub(crate) fn cmd_track_with_runner(
    args: &[String],
    guard: &LifecycleGuard,
    runner: &dyn ProbeRunner,
) -> ExitCode {
    cmd_track_with_adapters_and_runner(args, guard, None, runner)
}

pub(crate) fn cmd_track_with_adapters_and_runner(
    args: &[String],
    guard: &LifecycleGuard,
    custom_adapters: Option<&[&dyn ToolAdapter]>,
    runner: &dyn ProbeRunner,
) -> ExitCode {
    let stdin = std::io::stdin();
    let stdout_handle = std::io::stdout();
    let stderr_handle = std::io::stderr();
    let mut stdout = stdout_handle.lock();
    let mut stderr = stderr_handle.lock();
    cmd_track_with_adapters_and_runner_io(
        args,
        guard,
        custom_adapters,
        runner,
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        stdin.lock(),
        &mut stdout,
        &mut stderr,
    )
}

#[allow(clippy::too_many_arguments)]
fn cmd_track_with_adapters_and_runner_io<R>(
    args: &[String],
    guard: &LifecycleGuard,
    custom_adapters: Option<&[&dyn ToolAdapter]>,
    runner: &dyn ProbeRunner,
    is_terminal: bool,
    reader: R,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode
where
    R: std::io::BufRead,
{
    if args.iter().any(|arg| arg == "--json") {
        let _ = writeln!(stderr, "tool track does not support --json");
        return ExitCode::Usage;
    }
    if args.len() < 3 {
        let _ = writeln!(
            stderr,
            "Usage: ccync tool track <id> (--command <path> --recipe <id> | --candidate <candidate-id> --recipe <id> | --candidate <candidate-id> --delivery observe-only)"
        );
        return ExitCode::Usage;
    }
    let value = |flag: &str| {
        args.windows(2)
            .find(|pair| pair[0] == flag)
            .map(|pair| pair[1].as_str())
    };
    let id = match ToolId::new(args[1].clone()) {
        Ok(id) => id,
        Err(_) => {
            let _ = writeln!(stderr, "invalid tool id");
            return ExitCode::Usage;
        }
    };
    let command = value("--command");
    let candidate_id = value("--candidate");
    let delivery = match (value("--recipe"), value("--delivery")) {
        (Some("graphify-agents"), None) => DeliveryId::GraphifyAgents,
        (Some("rtk-path"), None) => DeliveryId::RtkPath,
        (None, Some("observe-only")) => DeliveryId::ObserveOnly,
        _ => {
            let _ = writeln!(
                stderr,
                "track requires a supported recipe or --delivery observe-only"
            );
            return ExitCode::Usage;
        }
    };
    if command.is_none() && candidate_id.is_none()
        || (command.is_some() && delivery == DeliveryId::ObserveOnly)
    {
        let _ = writeln!(stderr, "choose exactly one supported track form");
        return ExitCode::Usage;
    }
    let allowed = [
        "--command",
        "--candidate",
        "--recipe",
        "--delivery",
        "--accept-limited-evidence",
        "--online",
        "--manager-takeover",
        "--yes",
    ];
    if args[2..]
        .iter()
        .any(|arg| arg.starts_with("--") && !allowed.contains(&arg.as_str()))
    {
        let _ = writeln!(stderr, "unknown track option");
        return ExitCode::Usage;
    }
    let limited = args.iter().any(|arg| arg == "--accept-limited-evidence");
    if !assumed_yes(args) && !confirm_with_io(args, limited, is_terminal, reader, &mut *stdout) {
        return emit_outcome_to(
            super::tool_output::ToolCommand::Track,
            super::tool_output::ToolOutcome::ConfirmationRequired,
            Some("rerun with --yes".into()),
            true,
            false,
            stdout,
            stderr,
        );
    }
    let command_for_probe = command.unwrap_or("");
    let built_in_adapters;
    let adapter_refs: Vec<&dyn ToolAdapter> = match custom_adapters {
        Some(custom) => custom.to_vec(),
        None => {
            built_in_adapters = adapters_for(command_for_probe, delivery);
            built_in_adapters.iter().map(Box::as_ref).collect()
        }
    };
    let command_selector = command.map(str::to_owned);
    let selection_outcome = command_selector.as_deref().and_then(|selector| {
        let mut observations = Vec::new();
        let mut probe_failures = Vec::new();
        for adapter in &adapter_refs {
            match adapter.discover(&DiscoveryQuery { online: false }) {
                Ok(candidates) => observations.extend(candidates),
                Err(error) => probe_failures.push(format!(
                    "adapter={:?}: {:?}",
                    adapter.kind(),
                    error
                )),
            }
        }
        let candidates = merge_candidates(observations);
        let mut selected = filter_by_executable_selector(&candidates, selector);
        selected.sort_by_key(|candidate| {
            (
                serde_json::to_string(&candidate.identity.adapter_kind).unwrap_or_default(),
                candidate_executable(candidate),
                candidate.candidate_id.clone(),
            )
        });
        let selected_candidate = candidate_id.and_then(|requested| {
            selected
                .iter()
                .find(|candidate| candidate.candidate_id == requested)
        });
        let blocked_selection = candidate_id.is_none()
            && selected.len() == 1
            && (!selected[0].trackable
                || !selected[0].recipe_compatible
                || selected[0].evidence_level
                    != ccync_engine::tool_manager::model::EvidenceLevel::Verified);
        let reprobe_failure = if probe_failures.is_empty()
            && ((candidate_id.is_none() && selected.len() == 1)
                || (candidate_id.is_some() && selected_candidate.is_some()))
        {
            selected_candidate.or_else(|| selected.first()).and_then(|candidate| {
                adapter_refs
                    .iter()
                    .find(|adapter| adapter.kind() == candidate.identity.adapter_kind)
                    .and_then(|adapter| adapter.reprobe(&DiscoveryQuery { online: false }).err())
                    .map(|error| {
                        format!(
                            "adapter={:?}: re-probe incomplete: {:?}",
                            candidate.identity.adapter_kind, error
                        )
                    })
            })
        } else {
            None
        };
        let diagnostic = if !probe_failures.is_empty() {
            Some(format!(
                "probe-error: discovery incomplete; {}; retry after checking the adapter",
                probe_failures.join(", ")
            ))
        } else if let Some(failure) = reprobe_failure.as_deref() {
            Some(format!(
                "probe-error: {}; retry after checking the adapter",
                failure
            ))
        } else if blocked_selection {
            Some(format!(
                "selector={selector:?}; candidate {} cannot be selected automatically; inspect its evidence and use --candidate with explicit evidence acceptance when permitted",
                candidate_summary(selected[0])
            ))
        } else if selected.is_empty() {
            Some(format!(
                "selector={selector:?}; verify the command or path and retry"
            ))
        } else if candidate_id.is_some() && selected_candidate.is_none() {
            Some(format!(
                "selector={selector:?}; candidate {:?} was not found; retry with a matching candidate",
                candidate_id
            ))
        } else if candidate_id.is_none() && selected.len() > 1 {
            Some(format!(
                "selector={selector:?}; candidates: {}; rerun with --candidate <candidate-id>",
                selected
                    .iter()
                    .map(|candidate| candidate_summary(candidate))
                    .collect::<Vec<_>>()
                    .join("; ")
            ))
        } else {
            None
        };
        diagnostic.map(|diagnostic| {
            let outcome = if !probe_failures.is_empty() || reprobe_failure.is_some() {
                super::tool_output::ToolOutcome::ProbeError
            } else if blocked_selection {
                super::tool_output::ToolOutcome::Blocked
            } else if selected.is_empty() || selected_candidate.is_none() && candidate_id.is_some() {
                super::tool_output::ToolOutcome::NotFound
            } else {
                super::tool_output::ToolOutcome::Ambiguous
            };
            (outcome, diagnostic)
        })
    });
    if let Some((outcome, diagnostic)) = selection_outcome {
        return emit_outcome_to(
            super::tool_output::ToolCommand::Track,
            outcome,
            Some(diagnostic),
            true,
            false,
            stdout,
            stderr,
        );
    }
    match ccync_engine::tool_manager::track::track_tool_with_selector(
        guard,
        ccync_engine::tool_manager::track::TrackRequest {
            stable_id: id.to_string(),
            adapters: &adapter_refs,
            delivery,
            command_selection: candidate_id.is_none(),
            candidate_id: candidate_id.map(str::to_owned),
            accept_limited_evidence: limited,
            confirmed: assumed_yes(args) || is_terminal,
        },
        runner,
        command_selector.as_deref(),
    ) {
        Ok(outcome) => emit_outcome_to(
            super::tool_output::ToolCommand::Track,
            super::tool_output::ToolOutcome::Tracked,
            Some(if outcome.inserted {
                "declaration created".into()
            } else {
                "declaration unchanged".into()
            }),
            false,
            false,
            stdout,
            stderr,
        ),
        Err(error) => match error {
            ccync_engine::tool_manager::track::TrackError::Selection => emit_outcome_to(
                super::tool_output::ToolCommand::Track,
                if command.is_some() {
                    super::tool_output::ToolOutcome::ProbeError
                } else {
                    super::tool_output::ToolOutcome::NotFound
                },
                Some(if command.is_some() {
                    "probe/re-probe failed after selector resolution; retry after checking the adapter".into()
                } else {
                    "candidate was not found; verify the candidate ID and retry".into()
                }),
                true,
                false,
                stdout,
                stderr,
            ),
            ccync_engine::tool_manager::track::TrackError::LimitedEvidence
            | ccync_engine::tool_manager::track::TrackError::UnknownEvidence
            | ccync_engine::tool_manager::track::TrackError::Incompatible
            | ccync_engine::tool_manager::track::TrackError::Gated => emit_outcome_to(
                super::tool_output::ToolCommand::Track,
                super::tool_output::ToolOutcome::Blocked,
                Some(error.to_string()),
                true,
                false,
                stdout,
                stderr,
            ),
            ccync_engine::tool_manager::track::TrackError::InvalidId => ExitCode::Usage,
            _ => emit_outcome_to(
                super::tool_output::ToolCommand::Track,
                super::tool_output::ToolOutcome::Blocked,
                Some(error.to_string()),
                true,
                false,
                stdout,
                stderr,
            ),
        },
    }
}

pub(crate) fn cmd_untrack(args: &[String], guard: &LifecycleGuard) -> ExitCode {
    let stdin = std::io::stdin();
    let stdout_handle = std::io::stdout();
    let stderr_handle = std::io::stderr();
    let mut stdout = stdout_handle.lock();
    let mut stderr = stderr_handle.lock();
    cmd_untrack_with_projection_and_io(
        args,
        guard,
        || {
            let config = ccync_foundation::config::CcyncConfig::load();
            super::lifecycle::run_projection_with_report(&config, "ccync tool untrack").1
        },
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        stdin.lock(),
        &mut stdout,
        &mut stderr,
    )
}

fn cmd_untrack_with_projection_and_io<F, R>(
    args: &[String],
    guard: &LifecycleGuard,
    mut run_projection: F,
    is_terminal: bool,
    mut reader: R,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode
where
    F: FnMut() -> ccync_engine::install::ProjectionRunReport,
    R: std::io::BufRead,
{
    if args.iter().any(|arg| arg == "--json") {
        let _ = writeln!(stderr, "tool untrack does not support --json");
        return ExitCode::Usage;
    }
    if args.len() != 2 && !(args.len() == 3 && args[2] == "--yes") {
        let _ = writeln!(stderr, "Usage: ccync tool untrack <id> [--yes]");
        return ExitCode::Usage;
    }
    let id = match ToolId::new(args[1].clone()) {
        Ok(id) => id,
        Err(_) => {
            let _ = writeln!(stderr, "invalid tool id");
            return ExitCode::Usage;
        }
    };
    let accepted = if assumed_yes(args) {
        true
    } else if !is_terminal {
        false
    } else {
        let _ = write!(stdout, "Remove tracked tool {}? [y/N] ", id);
        let _ = stdout.flush();
        let mut answer = String::new();
        reader.read_line(&mut answer).is_ok() && answer.trim().eq_ignore_ascii_case("y")
    };
    if !accepted {
        return emit_outcome_to(
            super::tool_output::ToolCommand::Untrack,
            super::tool_output::ToolOutcome::ConfirmationRequired,
            Some("rerun with --yes".into()),
            true,
            false,
            stdout,
            stderr,
        );
    }
    let projection_report = std::cell::RefCell::new(None);
    match ccync_engine::tool_manager::untrack::untrack_tool(guard, id, || {
        let report = run_projection();
        *projection_report.borrow_mut() = Some(report.clone());
        report
    }) {
        Ok(()) => emit_outcome_to(
            super::tool_output::ToolCommand::Untrack,
            super::tool_output::ToolOutcome::Untracked,
            None,
            false,
            projection_report
                .borrow()
                .as_ref()
                .is_some_and(projection_changed),
            stdout,
            stderr,
        ),
        Err(error) => {
            let (outcome, next_action) = match error {
                ccync_engine::tool_manager::untrack::UntrackError::Missing => (
                    super::tool_output::ToolOutcome::NotFound,
                    Some("track the tool before untracking it".into()),
                ),
                ccync_engine::tool_manager::untrack::UntrackError::Ownership => (
                    super::tool_output::ToolOutcome::Blocked,
                    Some("resolve unowned tool artifacts before retrying".into()),
                ),
                ccync_engine::tool_manager::untrack::UntrackError::Projection => (
                    super::tool_output::ToolOutcome::RecoveryRequired,
                    Some("retry after projection recovery".into()),
                ),
                other => (
                    super::tool_output::ToolOutcome::Blocked,
                    Some(other.to_string()),
                ),
            };
            emit_outcome_to(
                super::tool_output::ToolCommand::Untrack,
                outcome,
                next_action,
                true,
                false,
                stdout,
                stderr,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccync_engine::tool_manager::{
        adapters::{AdapterCapabilities, AdapterError, CapabilityState},
        discovery::{DiscoveryCandidate, DiscoveryQuery},
        lock::LifecycleLock,
        model::{AdapterKind, EvidenceLevel, InstallationIdentity, ToolDeclaration},
        probe::{ProbeOutput, ProbeRequest, ProbeRunner},
        store,
    };
    use std::{collections::BTreeMap, fs, io::Cursor, path::Path};

    struct RestoredHome(Vec<(String, Option<String>)>);

    impl Drop for RestoredHome {
        fn drop(&mut self) {
            restore_home(std::mem::take(&mut self.0));
        }
    }

    struct OutcomeAdapter {
        candidates: Vec<DiscoveryCandidate>,
        fail_discovery: bool,
        fail_reprobe: bool,
    }

    impl ToolAdapter for OutcomeAdapter {
        fn kind(&self) -> AdapterKind {
            AdapterKind::Cargo
        }
        fn capabilities(&self) -> AdapterCapabilities {
            AdapterCapabilities::for_adapter(
                self.kind(),
                CapabilityState::Supported,
                CapabilityState::Supported,
                CapabilityState::Supported,
                CapabilityState::Gated,
            )
        }
        fn discover(&self, _: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
            if self.fail_discovery {
                Err(AdapterError::Failed("discovery fixture failed".into()))
            } else {
                Ok(self.candidates.clone())
            }
        }
        fn reprobe(&self, _: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
            if self.fail_reprobe {
                Err(AdapterError::Failed("reprobe fixture failed".into()))
            } else {
                Ok(self.candidates.clone())
            }
        }
    }

    fn outcome_candidate(path: &Path, level: EvidenceLevel) -> DiscoveryCandidate {
        DiscoveryCandidate::new(
            InstallationIdentity {
                adapter_kind: AdapterKind::Cargo,
                fields: BTreeMap::from([
                    ("executable".into(), path.display().to_string()),
                    ("name".into(), "rtk".into()),
                ]),
            },
            level,
            true,
            true,
            false,
        )
    }

    #[test]
    fn track_production_outcomes_capture_streams_and_evidence() {
        let _io = crate::test_io_lock();
        for case in [
            "tracked",
            "not-found",
            "ambiguous",
            "probe-error",
            "reprobe-error",
            "blocked",
            "confirmation-required",
            "json",
        ] {
            let home = tempfile::tempdir().unwrap();
            let _env = RestoredHome(redirect_home(home.path()));
            let guard = LifecycleLock::acquire().unwrap();
            let executable = home.path().join("a").join("rtk");
            fs::create_dir_all(executable.parent().unwrap()).unwrap();
            fs::write(&executable, b"fixture").unwrap();
            let level = if case == "blocked" {
                EvidenceLevel::Heuristic
            } else {
                EvidenceLevel::Verified
            };
            let candidate = outcome_candidate(&executable, level);
            let mut candidates = vec![candidate.clone()];
            if case == "not-found" {
                candidates.clear();
            }
            if case == "ambiguous" {
                candidates.insert(
                    0,
                    outcome_candidate(&home.path().join("z").join("rtk"), EvidenceLevel::Verified),
                );
            }
            let adapter = OutcomeAdapter {
                candidates,
                fail_discovery: case == "probe-error",
                fail_reprobe: case == "reprobe-error",
            };
            let adapters: [&dyn ToolAdapter; 1] = [&adapter];
            let mut args: Vec<String> =
                ["track", "rtk", "--command", "rtk", "--recipe", "rtk-path"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect();
            if case != "confirmation-required" {
                args.push("--yes".into());
            }
            if case == "json" {
                args.push("--json".into());
            }
            let mut out = Vec::new();
            let mut err = Vec::new();
            let exit = cmd_track_with_adapters_and_runner_io(
                &args,
                &guard,
                Some(&adapters),
                &FixtureProbeRunner,
                false,
                Cursor::new(b""),
                &mut out,
                &mut err,
            );
            let out = String::from_utf8(out).unwrap();
            let err = String::from_utf8(err).unwrap();
            let expected = if case == "reprobe-error" {
                "probe-error"
            } else {
                case
            };
            if case == "tracked" {
                assert_eq!(exit, ExitCode::Success);
                assert!(out.contains("outcome: tracked"));
                assert!(err.is_empty());
                assert!(store::load_declarations()
                    .unwrap()
                    .contains_key(&ToolId::new("rtk").unwrap()));
            } else {
                assert!(out.is_empty(), "{case}: unexpected stdout {out}");
                if case == "json" {
                    assert_eq!(exit, ExitCode::Usage);
                    assert!(err.contains("does not support --json"));
                } else {
                    assert!(
                        err.contains(&format!("outcome: {expected}")),
                        "{case}: {err}"
                    );
                    assert_eq!(
                        exit,
                        if expected == "probe-error" {
                            ExitCode::Error
                        } else {
                            ExitCode::UpdatesAvailable
                        }
                    );
                }
                assert!(store::load_declarations().unwrap().is_empty());
            }
            assert!(!out.contains("reload advised") && !err.contains("reload advised"));
            if expected == "probe-error" {
                assert!(
                    err.contains("Cargo") && err.contains("incomplete") && err.contains("retry"),
                    "{err}"
                );
            }
            if case == "not-found" {
                assert!(err.contains("rtk") && err.contains("retry"));
            }
            if case == "ambiguous" {
                assert!(
                    err.contains(&candidate.candidate_id)
                        && err.contains("adapter=cargo")
                        && err.contains("executable=")
                        && err.contains("evidence=verified")
                        && err.contains("--candidate"),
                    "{err}"
                );
                assert!(err.find("/a/rtk").unwrap() < err.find("/z/rtk").unwrap());
            }
        }
    }

    fn outcome_projection(
        status: ccync_engine::install::ProjectionTargetStatus,
    ) -> ccync_engine::install::ProjectionRunReport {
        use ccync_engine::install::{ProjectionRunReport, ProjectionTargetResult};
        use ccync_foundation::target::ProjectionTargetId;
        let target = ProjectionTargetId::Claude;
        ProjectionRunReport {
            common: vec![],
            targets: BTreeMap::from([(
                target,
                ProjectionTargetResult {
                    target,
                    status,
                    paths: vec![],
                    warnings: vec![],
                },
            )]),
            mode: "normal".into(),
            run_warnings: vec![],
        }
    }

    #[test]
    fn untrack_production_outcomes_capture_streams_and_projection_changes() {
        use ccync_engine::install::ProjectionTargetStatus;
        use ccync_engine::tool_manager::model::*;
        let _io = crate::test_io_lock();
        for case in [
            "untracked",
            "unchanged",
            "never-projected",
            "not-found",
            "blocked",
            "confirmation-required",
            "recovery-required",
            "json",
        ] {
            let home = tempfile::tempdir().unwrap();
            let _env = RestoredHome(redirect_home(home.path()));
            let guard = LifecycleLock::acquire().unwrap();
            let id = ToolId::new("graphify").unwrap();
            if case != "not-found" {
                store::insert_declaration(ToolDeclaration {
                    id: id.clone(),
                    identity: InstallationIdentity {
                        adapter_kind: AdapterKind::InterpreterPip,
                        fields: BTreeMap::new(),
                    },
                    delivery_id: DeliveryId::GraphifyAgents,
                    hold_policy: HoldPolicy::Unheld,
                    limited_evidence_acceptance: None,
                    accepted_baseline: None,
                    manager_takeover: None,
                })
                .unwrap();
                if case != "never-projected" {
                    store::commit_state(&BTreeMap::from([(
                        id.clone(),
                        store::StateRecord {
                            committed_generation: Some(1),
                            artifact_fingerprint: None,
                            source_evidence: serde_json::json!({}),
                            operation_id: None,
                            facets: ToolFacets {
                                evidence_level: EvidenceLevel::Verified,
                                selection_state: SelectionState::Unique,
                                manager_availability: ManagerAvailability::Available,
                                install_state: InstallState::Installed,
                                package_state: PackageState::MatchesArtifactSource,
                                update_state: UpdateState::Current,
                                artifact_state: ArtifactState::Current,
                                projection_state: ProjectionState::Current,
                                hold_policy: HoldPolicy::Unheld,
                                manager_pin_state: ManagerPinState::Unpinned,
                                operation_state: OperationState::None,
                                last_check: None,
                            },
                        },
                    )]))
                    .unwrap();
                }
            }
            if case == "blocked" {
                fs::create_dir_all(
                    ccync_foundation::paths::tool_artifacts_root()
                        .unwrap()
                        .join("graphify")
                        .join("unowned"),
                )
                .unwrap();
            }
            let mut args: Vec<String> = ["untrack", "graphify"]
                .into_iter()
                .map(str::to_owned)
                .collect();
            if case != "confirmation-required" {
                args.push("--yes".into());
            }
            if case == "json" {
                args.push("--json".into());
            }
            let calls = std::cell::Cell::new(0);
            let mut out = Vec::new();
            let mut err = Vec::new();
            let exit = cmd_untrack_with_projection_and_io(
                &args,
                &guard,
                || {
                    calls.set(calls.get() + 1);
                    outcome_projection(match case {
                        "unchanged" => ProjectionTargetStatus::Unchanged,
                        "recovery-required" => ProjectionTargetStatus::Failed {
                            path: None,
                            reason: "fixture failure".into(),
                        },
                        _ => ProjectionTargetStatus::Applied,
                    })
                },
                false,
                Cursor::new(b""),
                &mut out,
                &mut err,
            );
            let out = String::from_utf8(out).unwrap();
            let err = String::from_utf8(err).unwrap();
            let success = matches!(case, "untracked" | "unchanged" | "never-projected");
            if success {
                assert_eq!(exit, ExitCode::Success);
                assert!(out.contains("outcome: untracked"));
                assert!(err.is_empty());
                assert!(!store::load_declarations().unwrap().contains_key(&id));
                assert!(!store::load_state().unwrap().contains_key(&id));
            } else {
                assert!(out.is_empty());
                assert_eq!(
                    exit,
                    if case == "json" {
                        ExitCode::Usage
                    } else {
                        ExitCode::UpdatesAvailable
                    }
                );
                assert!(err.contains(if case == "json" {
                    "does not support --json"
                } else {
                    case
                }));
                if case != "not-found" {
                    assert!(store::load_declarations().unwrap().contains_key(&id));
                }
            }
            assert_eq!(
                out.contains("reload advised"),
                case == "untracked",
                "{case}: {out}"
            );
            assert_eq!(
                out.contains("loaded generation unknown"),
                case == "untracked"
            );
            assert!(!err.contains("reload advised"));
            assert_eq!(
                calls.get(),
                usize::from(matches!(
                    case,
                    "untracked" | "unchanged" | "recovery-required"
                )),
                "{case}"
            );
        }
    }

    #[derive(Debug, Default)]
    struct FixtureProbeRunner;

    impl FixtureProbeRunner {
        fn new(_outputs: Vec<ProbeOutput>) -> Self {
            Self
        }
    }

    impl ProbeRunner for FixtureProbeRunner {
        fn run(&self, _request: &ProbeRequest) -> ProbeOutput {
            ProbeOutput {
                exit_code: Some(0),
                ..Default::default()
            }
        }
    }

    struct MockAdapter {
        kind: AdapterKind,
        candidates: Vec<DiscoveryCandidate>,
    }

    impl ToolAdapter for MockAdapter {
        fn kind(&self) -> AdapterKind {
            self.kind
        }
        fn capabilities(&self) -> AdapterCapabilities {
            AdapterCapabilities::for_adapter(
                self.kind,
                CapabilityState::Supported,
                CapabilityState::Supported,
                CapabilityState::Supported,
                CapabilityState::Supported,
            )
        }
        fn discover(
            &self,
            _query: &DiscoveryQuery,
        ) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
            Ok(self.candidates.clone())
        }
        fn reprobe(
            &self,
            _query: &DiscoveryQuery,
        ) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
            Ok(self.candidates.clone())
        }
    }

    fn redirect_home(path: &Path) -> Vec<(String, Option<String>)> {
        [
            "USERPROFILE",
            "HOME",
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
            "CARGO_INSTALL_ROOT",
            "CCYNC_ASSUME_YES",
        ]
        .into_iter()
        .map(|key| {
            let old = std::env::var(key).ok();
            unsafe {
                if key == "CCYNC_ASSUME_YES" {
                    std::env::remove_var(key);
                } else {
                    std::env::set_var(key, path);
                }
            }
            (key.to_owned(), old)
        })
        .collect()
    }

    fn restore_home(saved: Vec<(String, Option<String>)>) {
        for (key, value) in saved {
            unsafe {
                if let Some(value) = value {
                    std::env::set_var(key, value)
                } else {
                    std::env::remove_var(key)
                }
            }
        }
    }

    #[test]
    fn tool_track_refuses_unsafe_ids_before_any_write() {
        let _guard = crate::test_io_lock();
        let home = tempfile::tempdir().unwrap();
        let saved = redirect_home(home.path());
        let lock_guard = LifecycleLock::acquire().unwrap();

        assert!(ToolId::new("../escape").is_err());
        assert!(ToolId::new("").is_err());
        assert!(ToolId::new("sub/dir").is_err());

        let track_exit = cmd_track(
            &[
                "track".into(),
                "../escape".into(),
                "--command".into(),
                "cargo".into(),
                "--recipe".into(),
                "rtk-path".into(),
                "--yes".into(),
            ],
            &lock_guard,
        );
        assert_eq!(track_exit, ExitCode::Usage);

        let untrack_exit = cmd_untrack(
            &["untrack".into(), "../escape".into(), "--yes".into()],
            &lock_guard,
        );
        assert_eq!(untrack_exit, ExitCode::Usage);

        restore_home(saved);
    }

    #[test]
    fn tool_track_rejects_missing_confirmation_when_noninteractive() {
        let _guard = crate::test_io_lock();
        let home = tempfile::tempdir().unwrap();
        let saved = redirect_home(home.path());
        unsafe { std::env::remove_var("CCYNC_ASSUME_YES") };
        let lock_guard = LifecycleLock::acquire().unwrap();

        // Non-interactive track without --yes exits 3 (UpdatesAvailable) before any write
        let track_exit = cmd_track(
            &[
                "track".into(),
                "demo".into(),
                "--command".into(),
                "cargo".into(),
                "--recipe".into(),
                "rtk-path".into(),
            ],
            &lock_guard,
        );
        assert_eq!(track_exit, ExitCode::UpdatesAvailable);

        // Non-interactive untrack without --yes exits 3 (UpdatesAvailable) before any write
        let untrack_exit = cmd_untrack(&["untrack".into(), "demo".into()], &lock_guard);
        assert_eq!(untrack_exit, ExitCode::UpdatesAvailable);

        // confirm_with_io non-interactive returns false
        assert!(!confirm_with_io(
            &[],
            false,
            false,
            std::io::empty(),
            Vec::new()
        ));

        restore_home(saved);
    }

    #[test]
    fn tool_track_assume_yes_does_not_accept_limited_evidence() {
        let _guard = crate::test_io_lock();
        let home = tempfile::tempdir().unwrap();
        let saved = redirect_home(home.path());
        unsafe { std::env::set_var("CCYNC_ASSUME_YES", "1") };
        let lock_guard = LifecycleLock::acquire().unwrap();

        // assumed_yes detects env variable
        assert!(assumed_yes(&[]));

        // Create a mock adapter with a candidate having Heuristic evidence level
        let mut fields = BTreeMap::new();
        fields.insert("name".into(), "rtk".into());
        fields.insert(
            "executable".into(),
            home.path().join("rtk").display().to_string(),
        );
        let candidate = DiscoveryCandidate::new(
            InstallationIdentity {
                adapter_kind: AdapterKind::Cargo,
                fields,
            },
            EvidenceLevel::Heuristic,
            true,
            true,
            false,
        );
        let candidate_id = candidate.candidate_id.clone();
        let mock_adapter = MockAdapter {
            kind: AdapterKind::Cargo,
            candidates: vec![candidate],
        };
        let adapters: Vec<&dyn ToolAdapter> = vec![&mock_adapter];
        let runner = FixtureProbeRunner::new(vec![]);

        // Even with CCYNC_ASSUME_YES=1, lacking --accept-limited-evidence exits UpdatesAvailable (3)
        let exit = cmd_track_with_adapters_and_runner(
            &[
                "track".into(),
                "demo".into(),
                "--candidate".into(),
                candidate_id,
                "--recipe".into(),
                "rtk-path".into(),
            ],
            &lock_guard,
            Some(&adapters),
            &runner,
        );
        assert_eq!(exit, ExitCode::UpdatesAvailable);

        restore_home(saved);
    }

    #[test]
    fn tool_track_limited_evidence_acknowledgement() {
        let _io = crate::test_io_lock();
        let home = tempfile::tempdir().unwrap();
        let _env = RestoredHome(redirect_home(home.path()));
        let mut stdout_buf = Vec::new();
        let stdin_input = Cursor::new(b"y\n");

        // When limited is true and interactive without --yes, prints acknowledgement
        let accepted = confirm_with_io(&[], true, true, stdin_input, &mut stdout_buf);
        assert!(accepted);
        let output = String::from_utf8(stdout_buf).unwrap();
        assert!(output.contains(
            "acknowledgement: limited installation evidence will be stored with this declaration"
        ));

        // When limited is false, no acknowledgement line is emitted
        let mut stdout_buf2 = Vec::new();
        let stdin_input2 = Cursor::new(b"y\n");
        let accepted2 = confirm_with_io(&[], false, true, stdin_input2, &mut stdout_buf2);
        assert!(accepted2);
        let output2 = String::from_utf8(stdout_buf2).unwrap();
        assert!(!output2.contains("acknowledgement:"));
    }

    #[test]
    fn tool_untrack_failure_keeps_declaration() {
        let _guard = crate::test_io_lock();
        let home = tempfile::tempdir().unwrap();
        let saved = redirect_home(home.path());
        let lock_guard = LifecycleLock::acquire().unwrap();

        // Seed a valid tool declaration in tools.json
        let tool_id = ToolId::new("mytool").unwrap();
        let mut fields = BTreeMap::new();
        fields.insert("name".into(), "mytool".into());
        let declaration = ToolDeclaration {
            id: tool_id.clone(),
            identity: InstallationIdentity {
                adapter_kind: AdapterKind::Cargo,
                fields,
            },
            delivery_id: DeliveryId::RtkPath,
            hold_policy: ccync_engine::tool_manager::model::HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        };
        let mut declarations = BTreeMap::new();
        declarations.insert(tool_id.clone(), declaration);
        store::replace_declarations(&declarations).unwrap();

        // Create an unowned non-generation directory in the artifact directory
        let artifacts_root = ccync_foundation::paths::tool_artifacts_root().unwrap();
        let unowned_dir = artifacts_root.join("mytool").join("unowned-corrupt-entry");
        fs::create_dir_all(&unowned_dir).unwrap();

        // Attempt untrack with --yes: ownership error occurs
        let exit = cmd_untrack(
            &["untrack".into(), "mytool".into(), "--yes".into()],
            &lock_guard,
        );
        assert_eq!(exit, ExitCode::UpdatesAvailable);

        // Verification: the declaration is KEPT in tools.json
        let remaining = ccync_engine::tool_manager::store::load_declarations().unwrap();
        assert!(
            remaining.contains_key(&tool_id),
            "untrack failure must keep the declaration"
        );

        restore_home(saved);
    }

    #[test]
    fn tool_track_covers_each_form_and_usage_errors() {
        let _guard = crate::test_io_lock();
        let home = tempfile::tempdir().unwrap();
        let saved = redirect_home(home.path());
        let lock_guard = LifecycleLock::acquire().unwrap();
        let runner = FixtureProbeRunner::new(vec![]);

        // Form 1: --command <path> --recipe <id> [--yes]
        let mut fields1 = BTreeMap::new();
        fields1.insert("name".into(), "rtk".into());
        fields1.insert(
            "executable".into(),
            home.path().join("rtk").display().to_string(),
        );
        let cand1 = DiscoveryCandidate::new(
            InstallationIdentity {
                adapter_kind: AdapterKind::Cargo,
                fields: fields1,
            },
            EvidenceLevel::Verified,
            true,
            true,
            false,
        );
        let adapter1 = MockAdapter {
            kind: AdapterKind::Cargo,
            candidates: vec![cand1.clone()],
        };
        let adapters1: Vec<&dyn ToolAdapter> = vec![&adapter1];

        let exit1 = cmd_track_with_adapters_and_runner(
            &[
                "track".into(),
                "rtk".into(),
                "--command".into(),
                "rtk".into(),
                "--recipe".into(),
                "rtk-path".into(),
                "--yes".into(),
            ],
            &lock_guard,
            Some(&adapters1),
            &runner,
        );
        assert_eq!(exit1, ExitCode::Success);

        // Form 2: --candidate <id> --recipe <id> [--accept-limited-evidence] [--yes]
        let mut fields2 = BTreeMap::new();
        fields2.insert("distribution".into(), "graphifyy".into());
        fields2.insert(
            "interpreter".into(),
            home.path().join("python").display().to_string(),
        );
        let cand2 = DiscoveryCandidate::new(
            InstallationIdentity {
                adapter_kind: AdapterKind::InterpreterPip,
                fields: fields2,
            },
            EvidenceLevel::Manageable,
            true,
            true,
            false,
        );
        let cand2_id = cand2.candidate_id.clone();
        let adapter2 = MockAdapter {
            kind: AdapterKind::InterpreterPip,
            candidates: vec![cand2],
        };
        let adapters2: Vec<&dyn ToolAdapter> = vec![&adapter2];

        let exit2 = cmd_track_with_adapters_and_runner(
            &[
                "track".into(),
                "graphify".into(),
                "--candidate".into(),
                cand2_id,
                "--recipe".into(),
                "graphify-agents".into(),
                "--accept-limited-evidence".into(),
                "--yes".into(),
            ],
            &lock_guard,
            Some(&adapters2),
            &runner,
        );
        assert_eq!(exit2, ExitCode::Success);

        // Form 3: --candidate <id> --delivery observe-only --accept-limited-evidence [--yes]
        // Note: ManualPath observe-only is gated in GATE_TABLE, so it exits UpdatesAvailable (3)
        let mut fields3 = BTreeMap::new();
        fields3.insert(
            "invokedPath".into(),
            home.path().join("obs").display().to_string(),
        );
        fields3.insert("fileIdentity".into(), "1:2".into());
        fields3.insert("linkChain".into(), "[]".into());
        let cand3 = DiscoveryCandidate::new(
            InstallationIdentity {
                adapter_kind: AdapterKind::ManualPath,
                fields: fields3,
            },
            EvidenceLevel::Heuristic,
            true,
            false,
            false,
        );
        let cand3_id = cand3.candidate_id.clone();
        let adapter3 = MockAdapter {
            kind: AdapterKind::ManualPath,
            candidates: vec![cand3],
        };
        let adapters3: Vec<&dyn ToolAdapter> = vec![&adapter3];

        let exit3 = cmd_track_with_adapters_and_runner(
            &[
                "track".into(),
                "obs".into(),
                "--candidate".into(),
                cand3_id,
                "--delivery".into(),
                "observe-only".into(),
                "--accept-limited-evidence".into(),
                "--yes".into(),
            ],
            &lock_guard,
            Some(&adapters3),
            &runner,
        );
        assert_eq!(exit3, ExitCode::UpdatesAvailable);

        // Explicit candidate disambiguation preserves the command selector.
        assert_eq!(
            cmd_track_with_adapters_and_runner(
                &[
                    "track".into(),
                    "rtk".into(),
                    "--command".into(),
                    "rtk".into(),
                    "--candidate".into(),
                    cand1.candidate_id.clone(),
                    "--recipe".into(),
                    "rtk-path".into(),
                    "--yes".into(),
                ],
                &lock_guard,
                Some(&adapters1),
                &runner,
            ),
            ExitCode::Success
        );

        // Usage errors: --command with observe-only
        assert_eq!(
            cmd_track(
                &[
                    "track".into(),
                    "demo".into(),
                    "--command".into(),
                    "cargo".into(),
                    "--delivery".into(),
                    "observe-only".into(),
                    "--yes".into(),
                ],
                &lock_guard,
            ),
            ExitCode::Usage
        );

        // Usage errors: unknown recipe
        assert_eq!(
            cmd_track(
                &[
                    "track".into(),
                    "demo".into(),
                    "--command".into(),
                    "cargo".into(),
                    "--recipe".into(),
                    "unsupported-recipe".into(),
                    "--yes".into(),
                ],
                &lock_guard,
            ),
            ExitCode::Usage
        );

        // Usage errors: unknown option
        assert_eq!(
            cmd_track(
                &[
                    "track".into(),
                    "demo".into(),
                    "--command".into(),
                    "cargo".into(),
                    "--recipe".into(),
                    "rtk-path".into(),
                    "--bogus-flag".into(),
                    "--yes".into(),
                ],
                &lock_guard,
            ),
            ExitCode::Usage
        );

        // Usage errors: insufficient args
        assert_eq!(
            cmd_track(&["track".into(), "demo".into()], &lock_guard),
            ExitCode::Usage
        );

        restore_home(saved);
    }

    #[test]
    fn untrack_guidance_contains_reload_line_without_fabricated_agents() {
        let guidance = untrack_reload_guidance(true).unwrap();
        assert_eq!(guidance, "reload advised; loaded generation unknown");
        assert!(untrack_reload_guidance(false).is_none());
    }
}
