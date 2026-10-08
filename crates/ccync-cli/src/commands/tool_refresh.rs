use ccync_engine::{
    tool_manager::{
        adapters::{self, ToolAdapter},
        journal::RetryIntent,
        lock::LifecycleGuard,
        model::{
            ArtifactState, EvidenceLevel, InstallState, ManagerAvailability, ManagerPinState,
            OperationState, PackageState, ProjectionState, SelectionState, ToolFacets, ToolId,
            UpdateState,
        },
        probe::{ProbeRunner, SystemProbeRunner},
        recovery::{self, RecoveryDecision},
        status::{self, StatusMode},
        store,
    },
    ExitCode,
};
use std::io::Write;

fn refresh_reload_guidance(
    status: ccync_engine::tool_manager::refresh::RefreshStatus,
    agents: &[String],
) -> Option<String> {
    (status == ccync_engine::tool_manager::refresh::RefreshStatus::Committed && !agents.is_empty())
        .then(|| super::tool_output::reload_line(agents))
}

fn missing_state_facets(hold_policy: ccync_engine::tool_manager::model::HoldPolicy) -> ToolFacets {
    ToolFacets {
        evidence_level: EvidenceLevel::Unknown,
        selection_state: SelectionState::Unique,
        manager_availability: ManagerAvailability::NotChecked,
        install_state: InstallState::NotChecked,
        package_state: PackageState::NotChecked,
        update_state: UpdateState::NotChecked,
        artifact_state: ArtifactState::NotChecked,
        projection_state: ProjectionState::NotChecked,
        hold_policy,
        manager_pin_state: ManagerPinState::Unknown,
        operation_state: OperationState::None,
        last_check: None,
    }
}

pub(crate) fn cmd_refresh(args: &[String], guard: &LifecycleGuard) -> ExitCode {
    cmd_refresh_with_runner(args, guard, &SystemProbeRunner)
}

pub(crate) fn cmd_refresh_with_runner(
    args: &[String],
    guard: &LifecycleGuard,
    runner: &dyn ProbeRunner,
) -> ExitCode {
    cmd_refresh_with_adapters_and_runner(args, guard, None, runner)
}

pub(crate) fn cmd_refresh_with_adapters_and_runner(
    args: &[String],
    guard: &LifecycleGuard,
    custom_adapters: Option<&[&dyn ToolAdapter]>,
    runner: &dyn ProbeRunner,
) -> ExitCode {
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    cmd_refresh_with_adapters_and_runner_to(
        args,
        guard,
        custom_adapters,
        runner,
        &mut stdout,
        &mut stderr,
    )
}

fn cmd_refresh_with_adapters_and_runner_to(
    args: &[String],
    guard: &LifecycleGuard,
    custom_adapters: Option<&[&dyn ToolAdapter]>,
    runner: &dyn ProbeRunner,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    if args.len() < 2
        || args.len() > 4
        || args[2..]
            .iter()
            .any(|arg| arg != "--dry-run" && arg != "--json")
        || args[2..].iter().filter(|arg| *arg == "--dry-run").count() > 1
        || args[2..].iter().filter(|arg| *arg == "--json").count() > 1
    {
        let _ = writeln!(
            stderr,
            "Usage: ccync tool refresh <id> [--dry-run] [--json]"
        );
        return ExitCode::Usage;
    }
    let id = match ToolId::new(args[1].clone()) {
        Ok(id) => id,
        Err(_) => {
            let _ = writeln!(stderr, "invalid tool id");
            return ExitCode::Usage;
        }
    };
    let retry = RetryIntent::Refresh;
    let json = args.iter().any(|arg| arg == "--json");
    let mut emit = |outcome: super::tool_output::ToolOutcome,
                    next_action: Option<String>,
                    freshness: Option<&str>,
                    changed: bool,
                    agents: &[String],
                    status: Option<&ToolFacets>| {
        let mut facts = super::tool_output::ToolOutcomeFacts::new(
            super::tool_output::ToolCommand::Refresh,
            outcome,
        );
        facts.next_action = next_action;
        facts.update_freshness = freshness.map(str::to_owned);
        if let Some(status) = status {
            facts.selection_state = Some(
                serde_json::to_value(status.selection_state)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .into(),
            );
            facts.update_state = Some(
                serde_json::to_value(status.update_state)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .into(),
            );
        }
        let reload_advice = if changed {
            refresh_reload_guidance(
                ccync_engine::tool_manager::refresh::RefreshStatus::Committed,
                agents,
            )
        } else {
            None
        };
        facts = facts.with_reload(reload_advice.is_some(), agents);
        if json {
            let _ = writeln!(
                stdout,
                "{}",
                super::tool_output::render_outcome_json(&facts).unwrap_or_else(|_| "{}".into())
            );
        } else if matches!(
            super::tool_output::output_stream(super::tool_output::ToolCommand::Refresh, outcome),
            super::tool_output::OutputStream::Stdout
        ) {
            let _ = write!(
                stdout,
                "{}",
                super::tool_output::render_outcome_human(&facts)
            );
        } else {
            let _ = write!(
                stderr,
                "{}",
                super::tool_output::render_outcome_human(&facts)
            );
        }
        outcome.exit_code()
    };
    match recovery::recover(guard, &retry) {
        Ok(RecoveryDecision::RecoveryRequired(required)) => {
            return emit(
                super::tool_output::ToolOutcome::RecoveryRequired,
                Some(format!("retry with `{}`", required.retry_command)),
                Some("stale"),
                false,
                &[],
                None,
            );
        }
        Ok(
            RecoveryDecision::Resume | RecoveryDecision::None | RecoveryDecision::CommittedCleanup,
        ) => {}
        Err(error) => {
            return emit(
                super::tool_output::ToolOutcome::CheckFailed,
                Some(error.to_string()),
                Some("stale"),
                false,
                &[],
                None,
            );
        }
    }
    let declarations = match store::load_declarations() {
        Ok(value) => value,
        Err(error) => {
            return emit(
                super::tool_output::ToolOutcome::CheckFailed,
                Some(format!("ccync tool refresh: {error}")),
                Some("stale"),
                false,
                &[],
                None,
            );
        }
    };
    let Some(declaration) = declarations.get(&id) else {
        return emit(
            super::tool_output::ToolOutcome::Blocked,
            Some(format!("track tool `{id}` first")),
            Some("stale"),
            false,
            &[],
            None,
        );
    };
    if declaration.hold_policy == ccync_engine::tool_manager::model::HoldPolicy::Held {
        return emit(
            super::tool_output::ToolOutcome::Blocked,
            Some(format!("ccync tool hold {id} --off")),
            Some("stale"),
            false,
            &[],
            None,
        );
    }
    let recorded_adapter;
    let adapter = match custom_adapters {
        Some(custom) => custom
            .iter()
            .copied()
            .find(|adapter| adapter.kind() == declaration.identity.adapter_kind),
        None => match adapters::for_tracked_declaration(declaration) {
            Ok((adapter, _, _)) => {
                recorded_adapter = adapter;
                Some(recorded_adapter.as_ref())
            }
            Err(error) => {
                return emit(
                    super::tool_output::ToolOutcome::Blocked,
                    Some(format!("{error:?}")),
                    Some("stale"),
                    false,
                    &[],
                    None,
                )
            }
        },
    };
    let Some(adapter) = adapter else {
        return emit(
            super::tool_output::ToolOutcome::Blocked,
            Some("unsupported adapter".into()),
            Some("stale"),
            false,
            &[],
            None,
        );
    };
    let states = match store::load_state() {
        Ok(value) => value,
        Err(error) => {
            return emit(
                super::tool_output::ToolOutcome::CheckFailed,
                Some(format!("ccync tool refresh: {error}")),
                Some("stale"),
                false,
                &[],
                None,
            );
        }
    };
    let state = states.get(&id);
    let dry_run = args.iter().any(|arg| arg == "--dry-run");
    let local_status = status::compute_status_with_adapter(
        declaration,
        state,
        StatusMode::Local,
        runner,
        Some(adapter),
    );
    let initial_facets = missing_state_facets(declaration.hold_policy);
    let outcome = ccync_engine::tool_manager::refresh::refresh_tool(
        guard,
        &id,
        runner,
        || {
            let config = ccync_foundation::config::CcyncConfig::load();
            super::lifecycle::run_projection_with_report(&config, "ccync tool refresh").1
        },
        ccync_engine::tool_manager::refresh::RefreshOptions {
            declaration,
            adapter,
            artifact_current: local_status.facets.artifact_state == ArtifactState::Current,
            projection_current: local_status.facets.projection_state == ProjectionState::Current,
            dry_run,
        },
    );
    match outcome {
        Ok(result) => {
            let facets = if dry_run {
                local_status.facets.clone()
            } else {
                store::load_state()
                    .ok()
                    .and_then(|values| values.get(&id).map(|state| state.facets.clone()))
                    .unwrap_or_else(|| {
                        state
                            .map(|state| state.facets.clone())
                            .unwrap_or_else(|| initial_facets.clone())
                    })
            };
            let outcome = match result.status {
                ccync_engine::tool_manager::refresh::RefreshStatus::NoOp => {
                    super::tool_output::ToolOutcome::NoOp
                }
                ccync_engine::tool_manager::refresh::RefreshStatus::Committed => {
                    super::tool_output::ToolOutcome::Committed
                }
                ccync_engine::tool_manager::refresh::RefreshStatus::DryRun => {
                    super::tool_output::ToolOutcome::DryRun
                }
            };
            let next_action = match outcome {
                super::tool_output::ToolOutcome::Committed => Some("ccync outdated".into()),
                super::tool_output::ToolOutcome::NoOp => Some("no change".into()),
                super::tool_output::ToolOutcome::DryRun => Some("unknown-until-executed".into()),
                _ => None,
            };
            emit(
                outcome,
                next_action,
                Some("stale"),
                outcome == super::tool_output::ToolOutcome::Committed
                    && !result.reload_advised_agents.is_empty(),
                &result.reload_advised_agents,
                Some(&facets),
            )
        }
        Err(error) => {
            let text = error.to_string();
            if text.contains("unsupported") || text.contains("declaration") {
                emit(
                    super::tool_output::ToolOutcome::Blocked,
                    Some(text),
                    Some("stale"),
                    false,
                    &[],
                    None,
                )
            } else {
                emit(
                    super::tool_output::ToolOutcome::CheckFailed,
                    Some(text),
                    Some("stale"),
                    false,
                    &[],
                    None,
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccync_engine::tool_manager::{
        evidence::{capture_evidence, evidence_to_json},
        file_identity::hash_same_handle,
        journal::{OperationJournal, OperationKind, OperationStage},
        lock::LifecycleLock,
        model::{
            AdapterKind, ArtifactState, DeliveryId, EvidenceLevel, HoldPolicy,
            InstallationIdentity, ManagerAvailability, ManagerPinState, OperationState,
            PackageState, ProjectionState, SelectionState, ToolDeclaration, ToolFacets, ToolId,
            UpdateState,
        },
        probe::{ProbeOutput, ProbeRequest, ProbeRunner},
        store::{self, StateRecord},
    };
    use std::{collections::BTreeMap, fs, path::Path};

    #[derive(Debug, Default)]
    struct FixtureProbeRunner;

    impl ProbeRunner for FixtureProbeRunner {
        fn run(&self, _request: &ProbeRequest) -> ProbeOutput {
            ProbeOutput {
                exit_code: Some(0),
                ..Default::default()
            }
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
        ]
        .into_iter()
        .map(|key| {
            let old = std::env::var(key).ok();
            unsafe { std::env::set_var(key, path) };
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

    fn default_test_facets() -> ToolFacets {
        ToolFacets {
            evidence_level: EvidenceLevel::Verified,
            selection_state: SelectionState::Unique,
            manager_availability: ManagerAvailability::Available,
            install_state: ccync_engine::tool_manager::model::InstallState::Installed,
            package_state: PackageState::MatchesArtifactSource,
            update_state: UpdateState::Current,
            artifact_state: ArtifactState::Current,
            projection_state: ProjectionState::Current,
            hold_policy: HoldPolicy::Unheld,
            manager_pin_state: ManagerPinState::Unpinned,
            operation_state: OperationState::Committed,
            last_check: None,
        }
    }

    #[test]
    fn tool_refresh_rejects_unsafe_id_and_usage() {
        let _guard = crate::test_io_lock();
        let home = tempfile::tempdir().unwrap();
        let saved = redirect_home(home.path());
        let lock_guard = LifecycleLock::acquire().unwrap();

        assert!(ToolId::new("../escape").is_err());
        assert_eq!(
            cmd_refresh(&["refresh".into(), "../escape".into()], &lock_guard),
            ExitCode::Usage
        );
        assert_eq!(
            cmd_refresh(&["refresh".into()], &lock_guard),
            ExitCode::Usage
        );
        assert_eq!(
            cmd_refresh(
                &["refresh".into(), "rtk".into(), "--invalid".into()],
                &lock_guard
            ),
            ExitCode::Usage
        );
        assert_eq!(
            cmd_refresh(
                &[
                    "refresh".into(),
                    "rtk".into(),
                    "--dry-run".into(),
                    "--dry-run".into()
                ],
                &lock_guard
            ),
            ExitCode::Usage
        );
        assert_eq!(
            cmd_refresh(
                &[
                    "refresh".into(),
                    "rtk".into(),
                    "--json".into(),
                    "--json".into()
                ],
                &lock_guard
            ),
            ExitCode::Usage
        );

        restore_home(saved);
    }

    #[test]
    fn tool_refresh_requires_declaration() {
        let _guard = crate::test_io_lock();
        let home = tempfile::tempdir().unwrap();
        let saved = redirect_home(home.path());
        let lock_guard = LifecycleLock::acquire().unwrap();

        // Missing declaration exits 3 (UpdatesAvailable)
        let exit_no_decl = cmd_refresh(&["refresh".into(), "rtk".into()], &lock_guard);
        assert_eq!(exit_no_decl, ExitCode::UpdatesAvailable);

        restore_home(saved);
    }

    #[test]
    fn tool_refresh_json_reports_declaration_read_failure_as_one_document() {
        let _guard = crate::test_io_lock();
        let home = tempfile::tempdir().unwrap();
        let saved = redirect_home(home.path());
        let lock_guard = LifecycleLock::acquire().unwrap();
        let declarations_path = ccync_foundation::paths::tools_json_path().unwrap();
        fs::create_dir_all(declarations_path.parent().unwrap()).unwrap();
        fs::write(&declarations_path, b"not-json").unwrap();

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = cmd_refresh_with_adapters_and_runner_to(
            &["refresh".into(), "rtk".into(), "--json".into()],
            &lock_guard,
            None,
            &FixtureProbeRunner,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(exit, ExitCode::Error);
        assert!(stderr.is_empty());
        let documents: Vec<_> = String::from_utf8(stdout)
            .unwrap()
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0]["outcome"], "check-failed");
        assert_eq!(documents[0]["updateFreshness"], "stale");
        assert_eq!(documents[0]["projectionChanged"], false);

        restore_home(saved);
    }

    #[test]
    fn tool_refresh_recovery_required_in_temporary_home() {
        let _guard = crate::test_io_lock();
        let home = tempfile::tempdir().unwrap();
        let saved = redirect_home(home.path());
        let lock_guard = LifecycleLock::acquire().unwrap();
        let runner = FixtureProbeRunner;

        // Write an incomplete operation journal with different retry intent (Upgrade vs Refresh)
        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        fs::create_dir_all(journal_path.parent().unwrap()).unwrap();
        let journal = OperationJournal {
            version: 1,
            operation_id: "op-12345".into(),
            tool_id: ToolId::new("rtk").unwrap(),
            kind: OperationKind::Upgrade,
            retry_intent: RetryIntent::Upgrade {
                tool_id: ToolId::new("rtk").unwrap(),
            },
            stage: OperationStage::Prepared,
            manager_step_state: ccync_engine::tool_manager::journal::ManagerStepState::NotStarted,
            pending_generation: None,
            pending_snapshot_relative_path: std::path::PathBuf::new(),
            pending_manifest_digest: String::new(),
            irreversible_changes: Vec::new(),
            pending_stages: vec![
                OperationStage::Prepared,
                OperationStage::PackageVerified,
                OperationStage::ArtifactValidated,
                OperationStage::ProjectionSucceeded,
                OperationStage::Committed,
            ],
            started_at: "2026-09-28T00:00:00Z".into(),
            cleanup_plan: Vec::new(),
        };
        fs::write(
            &journal_path,
            serde_json::to_string_pretty(&journal).unwrap(),
        )
        .unwrap();

        // cmd_refresh detects recovery is required, outputs retry instruction and exits 3
        let exit = cmd_refresh_with_runner(&["refresh".into(), "rtk".into()], &lock_guard, &runner);
        assert_eq!(exit, ExitCode::UpdatesAvailable);

        restore_home(saved);
    }

    #[test]
    fn tool_refresh_no_op_in_temporary_home() {
        let _guard = crate::test_io_lock();
        let home = tempfile::tempdir().unwrap();
        let saved = redirect_home(home.path());

        // Setup cargo install root and rtk executable
        let cargo_root = home.path().join("cargo_root");
        let bin_dir = cargo_root.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        #[cfg(windows)]
        let exe_path = bin_dir.join("rtk.exe");
        #[cfg(not(windows))]
        let exe_path = bin_dir.join("rtk");
        fs::write(&exe_path, b"test-rtk-binary-bytes").unwrap();
        let exe_hash = hash_same_handle(&exe_path).unwrap();

        let crates2_json = serde_json::json!({
            "v1": {
                "installs": {
                    "rtk 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)": {
                        "version": "0.1.0",
                        "bins": ["rtk"],
                        "source": "registry+https://github.com/rust-lang/crates.io-index"
                    }
                }
            }
        });
        fs::write(
            cargo_root.join(".crates2.json"),
            serde_json::to_string(&crates2_json).unwrap(),
        )
        .unwrap();

        unsafe { std::env::set_var("CARGO_INSTALL_ROOT", &cargo_root) };

        let lock_guard = LifecycleLock::acquire().unwrap();
        let runner = FixtureProbeRunner;

        let tool_id = ToolId::new("rtk").unwrap();
        let mut fields = BTreeMap::new();
        fields.insert("name".into(), "rtk".into());
        fields.insert("version".into(), "0.1.0".into());
        fields.insert(
            "source".into(),
            "registry+https://github.com/rust-lang/crates.io-index".into(),
        );
        fields.insert("root".into(), cargo_root.to_string_lossy().into_owned());
        fields.insert("executable".into(), exe_path.to_string_lossy().into_owned());
        fields.insert("hash".into(), exe_hash);
        fields.insert("trackedBin".into(), "rtk".into());

        let identity = InstallationIdentity {
            adapter_kind: AdapterKind::Cargo,
            fields,
        };
        let decl = ToolDeclaration {
            id: tool_id.clone(),
            identity: identity.clone(),
            delivery_id: DeliveryId::RtkPath,
            hold_policy: HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        };
        store::replace_declarations(&BTreeMap::from([(tool_id.clone(), decl)])).unwrap();

        // Capture evidence E0 from Cargo adapter
        let cargo_adapter = adapters::cargo::CargoAdapter::with_root(&cargo_root, "cargo");
        let e0 = capture_evidence(&cargo_adapter, &runner, &identity).unwrap();

        // Seed state matching current evidence E0, with current artifact and projection facets
        let state_record = StateRecord {
            committed_generation: Some(1),
            artifact_fingerprint: Some("sha256:rtk-fingerprint".into()),
            source_evidence: evidence_to_json(&e0),
            operation_id: Some("op-init".into()),
            facets: default_test_facets(),
        };
        store::commit_state(&BTreeMap::from([(tool_id.clone(), state_record)])).unwrap();

        // Run refresh: should be a NoOp!
        let exit = cmd_refresh_with_runner(&["refresh".into(), "rtk".into()], &lock_guard, &runner);
        assert_eq!(exit, ExitCode::Success);

        // Verify no journal was left behind
        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        assert!(!journal_path.exists());

        restore_home(saved);
    }

    #[test]
    fn tool_refresh_dry_run_with_no_write_in_temporary_home() {
        let _guard = crate::test_io_lock();
        let home = tempfile::tempdir().unwrap();
        let saved = redirect_home(home.path());

        // Setup cargo root and rtk executable
        let cargo_root = home.path().join("cargo_root");
        let bin_dir = cargo_root.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        #[cfg(windows)]
        let exe_path = bin_dir.join("rtk.exe");
        #[cfg(not(windows))]
        let exe_path = bin_dir.join("rtk");
        fs::write(&exe_path, b"test-rtk-binary-bytes").unwrap();
        let exe_hash = hash_same_handle(&exe_path).unwrap();

        let crates2_json = serde_json::json!({
            "v1": {
                "installs": {
                    "rtk 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)": {
                        "version": "0.1.0",
                        "bins": ["rtk"],
                        "source": "registry+https://github.com/rust-lang/crates.io-index"
                    }
                }
            }
        });
        fs::write(
            cargo_root.join(".crates2.json"),
            serde_json::to_string(&crates2_json).unwrap(),
        )
        .unwrap();

        unsafe { std::env::set_var("CARGO_INSTALL_ROOT", &cargo_root) };

        let lock_guard = LifecycleLock::acquire().unwrap();
        let runner = FixtureProbeRunner;

        let tool_id = ToolId::new("rtk").unwrap();
        let mut fields = BTreeMap::new();
        fields.insert("name".into(), "rtk".into());
        fields.insert("version".into(), "0.1.0".into());
        fields.insert(
            "source".into(),
            "registry+https://github.com/rust-lang/crates.io-index".into(),
        );
        fields.insert("root".into(), cargo_root.to_string_lossy().into_owned());
        fields.insert("executable".into(), exe_path.to_string_lossy().into_owned());
        fields.insert("hash".into(), exe_hash);
        fields.insert("trackedBin".into(), "rtk".into());

        let identity = InstallationIdentity {
            adapter_kind: AdapterKind::Cargo,
            fields,
        };
        let decl = ToolDeclaration {
            id: tool_id.clone(),
            identity,
            delivery_id: DeliveryId::RtkPath,
            hold_policy: HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        };
        store::replace_declarations(&BTreeMap::from([(tool_id.clone(), decl)])).unwrap();

        // Seed state with ArtifactDrifted so refresh would normally perform an update
        let mut facets = default_test_facets();
        facets.artifact_state = ArtifactState::ArtifactDrifted;
        let state_record = StateRecord {
            committed_generation: Some(1),
            artifact_fingerprint: Some("sha256:old".into()),
            source_evidence: serde_json::json!({ "sha256": "older-hash" }),
            operation_id: Some("op-old".into()),
            facets,
        };
        store::commit_state(&BTreeMap::from([(tool_id.clone(), state_record)])).unwrap();

        // Run dry-run refresh
        let exit = cmd_refresh_with_runner(
            &["refresh".into(), "rtk".into(), "--dry-run".into()],
            &lock_guard,
            &runner,
        );
        assert_eq!(exit, ExitCode::Success);

        // Assert: NO journal created, NO snapshot directory created under artifacts/rtk/
        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        assert!(!journal_path.exists());
        let artifacts_root = ccync_foundation::paths::tool_artifacts_root().unwrap();
        assert!(!artifacts_root.join("rtk").exists());

        // State remains unchanged
        let after_state = store::load_state().unwrap();
        assert_eq!(
            after_state.get(&tool_id).unwrap().facets.artifact_state,
            ArtifactState::ArtifactDrifted
        );

        restore_home(saved);
    }

    #[test]
    fn tool_refresh_success_in_temporary_home() {
        let _guard = crate::test_io_lock();
        let home = tempfile::tempdir().unwrap();
        let saved = redirect_home(home.path());

        // Setup cargo root and rtk executable
        let cargo_root = home.path().join("cargo_root");
        let bin_dir = cargo_root.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        #[cfg(windows)]
        let exe_path = bin_dir.join("rtk.exe");
        #[cfg(not(windows))]
        let exe_path = bin_dir.join("rtk");
        fs::write(&exe_path, b"test-rtk-binary-bytes").unwrap();
        let exe_hash = hash_same_handle(&exe_path).unwrap();

        let crates2_json = serde_json::json!({
            "v1": {
                "installs": {
                    "rtk 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)": {
                        "version": "0.1.0",
                        "bins": ["rtk"],
                        "source": "registry+https://github.com/rust-lang/crates.io-index"
                    }
                }
            }
        });
        fs::write(
            cargo_root.join(".crates2.json"),
            serde_json::to_string(&crates2_json).unwrap(),
        )
        .unwrap();

        unsafe { std::env::set_var("CARGO_INSTALL_ROOT", &cargo_root) };

        let lock_guard = LifecycleLock::acquire().unwrap();
        let runner = FixtureProbeRunner;

        let tool_id = ToolId::new("rtk").unwrap();
        let mut fields = BTreeMap::new();
        fields.insert("name".into(), "rtk".into());
        fields.insert("version".into(), "0.1.0".into());
        fields.insert(
            "source".into(),
            "registry+https://github.com/rust-lang/crates.io-index".into(),
        );
        fields.insert("root".into(), cargo_root.to_string_lossy().into_owned());
        fields.insert("executable".into(), exe_path.to_string_lossy().into_owned());
        fields.insert("hash".into(), exe_hash);
        fields.insert("trackedBin".into(), "rtk".into());

        let identity = InstallationIdentity {
            adapter_kind: AdapterKind::Cargo,
            fields,
        };
        let decl = ToolDeclaration {
            id: tool_id.clone(),
            identity,
            delivery_id: DeliveryId::RtkPath,
            hold_policy: HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        };
        store::replace_declarations(&BTreeMap::from([(tool_id.clone(), decl)])).unwrap();

        // Seed state with ArtifactDrifted to trigger refresh execution
        let mut facets = default_test_facets();
        facets.artifact_state = ArtifactState::ArtifactDrifted;
        let state_record = StateRecord {
            committed_generation: None,
            artifact_fingerprint: None,
            source_evidence: serde_json::json!({ "sha256": "older-hash" }),
            operation_id: Some("op-old".into()),
            facets,
        };
        store::commit_state(&BTreeMap::from([(tool_id.clone(), state_record)])).unwrap();

        // Run refresh with --json flag
        let exit = cmd_refresh_with_runner(
            &["refresh".into(), "rtk".into(), "--json".into()],
            &lock_guard,
            &runner,
        );
        assert_eq!(exit, ExitCode::Success);

        // Verification:
        // 1. Generation snapshot was committed under artifacts/rtk/1/
        let artifacts_root = ccync_foundation::paths::tool_artifacts_root().unwrap();
        assert!(artifacts_root.join("rtk").join("1").is_dir());

        // 2. State was updated with committed generation Some(1), ArtifactState::Current, ProjectionState::Current
        let after_state = store::load_state().unwrap();
        let rtk_state = after_state.get(&tool_id).unwrap();
        assert_eq!(rtk_state.committed_generation, Some(1));
        assert_eq!(rtk_state.facets.artifact_state, ArtifactState::Current);
        assert_eq!(rtk_state.facets.projection_state, ProjectionState::Current);

        // 3. Journal was cleanly deleted after completion
        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        assert!(!journal_path.exists());

        restore_home(saved);
    }

    #[test]
    fn tool_refresh_first_refresh_and_dry_run_without_prior_state() {
        let _guard = crate::test_io_lock();
        let home = tempfile::tempdir().unwrap();
        let saved = redirect_home(home.path());

        // Setup cargo root and rtk executable
        let cargo_root = home.path().join("cargo_root");
        let bin_dir = cargo_root.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        #[cfg(windows)]
        let exe_path = bin_dir.join("rtk.exe");
        #[cfg(not(windows))]
        let exe_path = bin_dir.join("rtk");
        fs::write(&exe_path, b"test-rtk-binary-bytes").unwrap();
        let exe_hash = hash_same_handle(&exe_path).unwrap();

        let crates2_json = serde_json::json!({
            "v1": {
                "installs": {
                    "rtk 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)": {
                        "version": "0.1.0",
                        "bins": ["rtk"],
                        "source": "registry+https://github.com/rust-lang/crates.io-index"
                    }
                }
            }
        });
        fs::write(
            cargo_root.join(".crates2.json"),
            serde_json::to_string(&crates2_json).unwrap(),
        )
        .unwrap();

        unsafe { std::env::set_var("CARGO_INSTALL_ROOT", &cargo_root) };

        let lock_guard = LifecycleLock::acquire().unwrap();
        let runner = FixtureProbeRunner;

        let tool_id = ToolId::new("rtk").unwrap();
        let mut fields = BTreeMap::new();
        fields.insert("name".into(), "rtk".into());
        fields.insert("version".into(), "0.1.0".into());
        fields.insert(
            "source".into(),
            "registry+https://github.com/rust-lang/crates.io-index".into(),
        );
        fields.insert("root".into(), cargo_root.to_string_lossy().into_owned());
        fields.insert("executable".into(), exe_path.to_string_lossy().into_owned());
        fields.insert("hash".into(), exe_hash);
        fields.insert("trackedBin".into(), "rtk".into());

        let identity = InstallationIdentity {
            adapter_kind: AdapterKind::Cargo,
            fields,
        };
        let decl = ToolDeclaration {
            id: tool_id.clone(),
            identity,
            delivery_id: DeliveryId::RtkPath,
            hold_policy: HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        };
        store::replace_declarations(&BTreeMap::from([(tool_id.clone(), decl)])).unwrap();

        assert!(store::load_state().unwrap().get(&tool_id).is_none());

        let dry_run_exit = cmd_refresh_with_runner(
            &["refresh".into(), "rtk".into(), "--dry-run".into()],
            &lock_guard,
            &runner,
        );
        assert_eq!(dry_run_exit, ExitCode::Success);
        assert!(store::load_state().unwrap().get(&tool_id).is_none());

        // Run refresh with --json flag
        let exit = cmd_refresh_with_runner(
            &["refresh".into(), "rtk".into(), "--json".into()],
            &lock_guard,
            &runner,
        );
        assert_eq!(exit, ExitCode::Success);

        // Verification:
        // 1. Generation snapshot was committed under artifacts/rtk/1/
        let artifacts_root = ccync_foundation::paths::tool_artifacts_root().unwrap();
        assert!(artifacts_root.join("rtk").join("1").is_dir());

        // 2. State was updated with committed generation Some(1), ArtifactState::Current, ProjectionState::Current
        let after_state = store::load_state().unwrap();
        let rtk_state = after_state.get(&tool_id).unwrap();
        assert_eq!(rtk_state.committed_generation, Some(1));
        assert_eq!(rtk_state.facets.artifact_state, ArtifactState::Current);
        assert_eq!(rtk_state.facets.projection_state, ProjectionState::Current);

        // 3. Journal was cleanly deleted after completion
        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        assert!(!journal_path.exists());

        restore_home(saved);
    }

    #[test]
    fn committed_refresh_guidance_contains_reload_line() {
        let guidance = refresh_reload_guidance(
            ccync_engine::tool_manager::refresh::RefreshStatus::Committed,
            &["Claude".into()],
        )
        .unwrap();
        assert!(guidance.contains("reload advised; loaded generation unknown"));
        assert!(refresh_reload_guidance(
            ccync_engine::tool_manager::refresh::RefreshStatus::Committed,
            &[]
        )
        .is_none());
        assert!(refresh_reload_guidance(
            ccync_engine::tool_manager::refresh::RefreshStatus::NoOp,
            &["Claude".into()]
        )
        .is_none());
        assert_eq!(
            super::super::tool_output::dry_run_value(),
            "unknown-until-executed"
        );
    }
}
