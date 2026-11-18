//! Manager-backed upgrades for tracked external tools.
use super::{
    adapters::{CapabilityState, ToolAdapter},
    evidence::{capture_evidence, EvidenceTuple},
    journal::{ManagerStepState, OperationJournal, OperationKind, RetryIntent},
    lock::LifecycleGuard,
    model::{AdapterKind, HoldPolicy, ManagerPinState, ToolDeclaration, ToolId, UpdateState},
    probe::{ProbeRequest, ProbeRunner},
    refresh::{self, RefreshOptions, RefreshOutcome},
    store,
};
use crate::install::ProjectionRunReport;
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

pub struct UpgradeOptions<'a> {
    pub declaration: &'a ToolDeclaration,
    pub adapter: &'a dyn ToolAdapter,
    pub package_update: UpdateState,
    pub retry_manager_step: bool,
    pub artifact_current: bool,
    pub projection_current: bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchEligibility {
    pub id: ToolId,
    pub eligible: bool,
    pub reasons: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum UpgradeError {
    #[error("tool upgrade blocked: {action}")]
    Blocked { action: String },
    #[error("manager outcome is unknown; package evidence did not prove a change. Run `{retry_command}`")]
    OutcomeUnknown { retry_command: String },
    #[error("manager update failed with exit status {exit_code:?}")]
    ManagerFailed { exit_code: Option<i32> },
    #[error("upgrade operation failed: {0}")]
    Failed(String),
}

pub fn upgrade_tool<P>(
    guard: &LifecycleGuard,
    id: &ToolId,
    runner: &dyn ProbeRunner,
    project: P,
    options: UpgradeOptions<'_>,
) -> Result<RefreshOutcome, UpgradeError>
where
    P: FnOnce() -> ProjectionRunReport,
{
    let paths = store::ToolPaths::load().map_err(|e| UpgradeError::Failed(e.to_string()))?;
    let declaration = options.declaration;
    if &declaration.id != id {
        return Err(blocked("ccync tool show"));
    }
    let argv = options
        .adapter
        .update_argv()
        .ok_or_else(|| blocked("package update is unsupported for this tool"))?;
    let recovery = load_recovery_journal(id, &argv, &paths)?;
    if let Some(journal) = recovery.as_ref() {
        if let Some(record) =
            store::committed_operation(journal).map_err(|e| UpgradeError::Failed(e.to_string()))?
        {
            if options.retry_manager_step {
                return Err(UpgradeError::Failed(
                    "manager retry requires an unknown manager outcome".into(),
                ));
            }
            if !options.dry_run {
                let mut journal = journal.clone();
                if journal.stage == super::journal::OperationStage::ProjectionSucceeded {
                    journal
                        .advance(guard, super::journal::OperationStage::Committed)
                        .map_err(|e| UpgradeError::Failed(e.to_string()))?;
                }
                journal
                    .remove(guard)
                    .map_err(|e| UpgradeError::Failed(e.to_string()))?;
            }
            return Ok(RefreshOutcome {
                status: refresh::RefreshStatus::NoOp,
                generation: record
                    .committed_generation
                    .map(refresh::FutureValue::Known)
                    .unwrap_or(refresh::FutureValue::UnknownUntilExecuted),
                artifact_fingerprint: record
                    .artifact_fingerprint
                    .map(refresh::FutureValue::Known)
                    .unwrap_or(refresh::FutureValue::UnknownUntilExecuted),
                reload_advised_agents: Vec::new(),
            });
        }
        if options.retry_manager_step
            && journal.manager_step_state != ManagerStepState::ManagerOutcomeUnknown
        {
            return Err(UpgradeError::Failed(
                "manager retry requires an unknown manager outcome".into(),
            ));
        }
    }
    let recovering = recovery.is_some();
    if options.retry_manager_step && !recovering {
        return Err(UpgradeError::Failed(
            "manager retry requires a valid recovery journal".into(),
        ));
    }
    if declaration.hold_policy == HoldPolicy::Held {
        return Err(blocked(&format!("ccync tool hold {} --off", id)));
    }
    if declaration.identity.adapter_kind == AdapterKind::ManualPath {
        return Err(blocked("track this tool with a supported package manager"));
    }
    if !recovering && options.package_update != UpdateState::UpdateAvailable {
        return Err(blocked("ccync tool outdated"));
    }
    if !recovering && options.adapter.capabilities().package_update != CapabilityState::Supported {
        return Err(blocked("package update is unsupported for this tool"));
    }
    if declaration.identity.adapter_kind == AdapterKind::WingetOnline
        && declaration
            .manager_takeover
            .as_ref()
            .and_then(|v| v.get("manager"))
            .and_then(serde_json::Value::as_str)
            != Some("winget")
    {
        return Err(blocked("accept WinGet manager takeover before upgrading"));
    }
    if !recovering {
        let pin = options
            .adapter
            .manager_pin()
            .map_err(|e| UpgradeError::Failed(format!("manager pin probe failed: {e:?}")))?;
        if pin == ManagerPinState::Pinned {
            let action = if declaration.identity.adapter_kind == AdapterKind::BrewFormula {
                format!(
                    "{} unpin {}",
                    declaration
                        .identity
                        .fields
                        .get("managerExecutable")
                        .map(String::as_str)
                        .unwrap_or("brew"),
                    declaration
                        .identity
                        .fields
                        .get("name")
                        .map(String::as_str)
                        .unwrap_or("<formula>")
                )
            } else {
                "clear the package manager pin".into()
            };
            return Err(blocked(&action));
        }
    }

    let prior = store::load_state().map_err(|e| UpgradeError::Failed(e.to_string()))?;
    let prior_record = prior
        .get(id)
        .cloned()
        .ok_or_else(|| blocked("ccync tool track"))?;
    let e0 = capture_evidence(options.adapter, runner, &declaration.identity)
        .map_err(|e| UpgradeError::Failed(e.to_string()))?;
    let e0_val = evidence_value(&e0);

    if options.dry_run {
        return Ok(RefreshOutcome {
            status: refresh::RefreshStatus::DryRun,
            generation: refresh::FutureValue::UnknownUntilExecuted,
            artifact_fingerprint: refresh::FutureValue::UnknownUntilExecuted,
            reload_advised_agents: Vec::new(),
        });
    }

    let (mut journal, is_resume) = if let Some(existing) = recovery {
        if existing.manager_step_state == ManagerStepState::Succeeded {
            let refresh_options = RefreshOptions {
                declaration,
                adapter: options.adapter,
                artifact_current: options.artifact_current,
                projection_current: options.projection_current,
                dry_run: options.dry_run,
            };
            let mut existing = existing;
            return refresh::post_package_continuation(
                guard,
                &mut existing,
                id,
                runner,
                project,
                refresh_options,
                &prior_record,
                true,
                Some(e0),
                &paths,
            )
            .map_err(|e| UpgradeError::Failed(e.to_string()));
        }
        if existing.manager_step_state == ManagerStepState::ManagerOutcomeUnknown {
            let now = capture_evidence(options.adapter, runner, &declaration.identity).ok();
            if now
                .as_ref()
                .is_some_and(|e| evidence_value(e) != prior_record.source_evidence)
            {
                let refresh_options = RefreshOptions {
                    declaration,
                    adapter: options.adapter,
                    artifact_current: options.artifact_current,
                    projection_current: options.projection_current,
                    dry_run: options.dry_run,
                };
                let mut existing = existing;
                existing.manager_step_state = ManagerStepState::Succeeded;
                if !existing
                    .irreversible_changes
                    .contains(&super::journal::IrreversibleChange::PackageInstalled)
                {
                    existing
                        .irreversible_changes
                        .push(super::journal::IrreversibleChange::PackageInstalled);
                }
                write_journal(&existing, &paths)?;
                return refresh::post_package_continuation(
                    guard,
                    &mut existing,
                    id,
                    runner,
                    project,
                    refresh_options,
                    &prior_record,
                    true,
                    now,
                    &paths,
                )
                .map_err(|e| UpgradeError::Failed(e.to_string()));
            }
            if !options.retry_manager_step {
                return Err(UpgradeError::OutcomeUnknown {
                    retry_command: format!("ccync upgrade tool:{} --retry-manager-step", id),
                });
            }
            (existing, true)
        } else {
            (existing, true)
        }
    } else {
        let mut j = OperationJournal::new(
            id.clone(),
            OperationKind::Upgrade,
            RetryIntent::Upgrade {
                tool_id: id.clone(),
            },
            None,
            PathBuf::new(),
            secret_free_digest(&argv),
        );
        j.manager_step_state = ManagerStepState::ManagerOutcomeUnknown;
        j.create(guard)
            .map_err(|e| UpgradeError::Failed(e.to_string()))?;
        (j, false)
    };

    if journal.manager_step_state != ManagerStepState::ManagerOutcomeUnknown {
        journal.manager_step_state = ManagerStepState::ManagerOutcomeUnknown;
        write_journal(&journal, &paths)?;
    }

    let mut env = BTreeMap::new();
    if declaration.identity.adapter_kind == AdapterKind::BrewFormula
        || declaration.identity.adapter_kind == AdapterKind::BrewCask
    {
        env.insert("HOMEBREW_NO_AUTO_UPDATE".into(), "1".into());
    }

    let (program, args) = argv
        .split_first()
        .ok_or_else(|| blocked("package update is unsupported for this tool"))?;
    let output = runner.run(&ProbeRequest {
        program: PathBuf::from(program),
        argv: args.to_vec(),
        env,
        working_directory: std::env::current_dir().unwrap_or_default(),
        timeout: Duration::from_secs(300),
        output_byte_cap: 64 * 1024,
    });
    if output.exit_code != Some(0) || output.timed_out || output.spawn_error.is_some() {
        journal.manager_step_state = ManagerStepState::Failed;
        write_journal(&journal, &paths)?;
        return Err(UpgradeError::ManagerFailed {
            exit_code: output.exit_code,
        });
    }
    journal.manager_step_state = ManagerStepState::Succeeded;
    journal
        .irreversible_changes
        .push(super::journal::IrreversibleChange::PackageInstalled);
    write_journal(&journal, &paths)?;

    let now = capture_evidence(options.adapter, runner, &declaration.identity)
        .map_err(|e| UpgradeError::Failed(e.to_string()))?;
    let now_val = evidence_value(&now);
    if now_val == prior_record.source_evidence || now_val == e0_val {
        journal.manager_step_state = ManagerStepState::ManagerOutcomeUnknown;
        write_journal(&journal, &paths)?;
        return Err(UpgradeError::OutcomeUnknown {
            retry_command: format!("ccync upgrade tool:{} --retry-manager-step", id),
        });
    }

    let refresh_options = RefreshOptions {
        declaration,
        adapter: options.adapter,
        artifact_current: options.artifact_current,
        projection_current: options.projection_current,
        dry_run: options.dry_run,
    };
    refresh::post_package_continuation(
        guard,
        &mut journal,
        id,
        runner,
        project,
        refresh_options,
        &prior_record,
        is_resume,
        Some(now),
        &paths,
    )
    .map_err(|e| UpgradeError::Failed(e.to_string()))
}

fn load_recovery_journal(
    id: &ToolId,
    argv: &[String],
    paths: &store::ToolPaths,
) -> Result<Option<OperationJournal>, UpgradeError> {
    let path = &paths.journal;
    if !path.exists() {
        return Ok(None);
    }

    let bytes = std::fs::read(&path).map_err(|e| UpgradeError::Failed(e.to_string()))?;
    let journal: OperationJournal = serde_json::from_slice(&bytes)
        .map_err(|e| UpgradeError::Failed(format!("malformed upgrade recovery journal: {e}")))?;
    if journal.tool_id != *id || journal.kind != OperationKind::Upgrade {
        return Err(UpgradeError::Failed(
            "recovery required for another operation".into(),
        ));
    }
    journal
        .validate_identity()
        .map_err(|e| UpgradeError::Failed(e.to_string()))?;
    let stage = journal.stage;
    let committed = store::committed_operation(&journal)
        .map_err(|e| UpgradeError::Failed(e.to_string()))?
        .is_some();
    let pending_valid = journal.pending_generation.is_some_and(|generation| {
        generation > 0
            && store::expected_snapshot_relative_path(id, generation)
                .ok()
                .as_ref()
                == Some(&journal.pending_snapshot_relative_path)
    }) && journal
        .pending_manifest_digest
        .strip_prefix("sha256:")
        .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_hexdigit()));
    let no_pending = journal.pending_generation.is_none()
        && journal
            .pending_snapshot_relative_path
            .as_os_str()
            .is_empty();
    let manager_valid = match journal.manager_step_state {
        ManagerStepState::ManagerOutcomeUnknown => {
            stage == super::journal::OperationStage::Prepared && no_pending
        }
        ManagerStepState::Succeeded => {
            matches!(
                stage,
                super::journal::OperationStage::Prepared
                    | super::journal::OperationStage::PackageVerified
            ) && no_pending
                || matches!(
                    stage,
                    super::journal::OperationStage::ArtifactValidated
                        | super::journal::OperationStage::ProjectionSucceeded
                ) && pending_valid
                || stage == super::journal::OperationStage::Committed && committed
        }
        _ => false,
    };
    if !manager_valid
        || journal.version != 1
        || journal.operation_id.is_empty()
        || !matches!(
            journal.stage,
            super::journal::OperationStage::Prepared
                | super::journal::OperationStage::PackageVerified
                | super::journal::OperationStage::ArtifactValidated
                | super::journal::OperationStage::ProjectionSucceeded
                | super::journal::OperationStage::Committed
        )
        || !matches!(
            journal.manager_step_state,
            ManagerStepState::ManagerOutcomeUnknown | ManagerStepState::Succeeded
        )
        || (journal.manager_step_state == ManagerStepState::Succeeded
            && !journal
                .irreversible_changes
                .contains(&super::journal::IrreversibleChange::PackageInstalled))
        || journal.pending_manifest_digest.is_empty()
        || journal
            .irreversible_changes
            .iter()
            .any(|change| *change != super::journal::IrreversibleChange::PackageInstalled)
        || journal.pending_stages
            != vec![
                super::journal::OperationStage::Prepared,
                super::journal::OperationStage::PackageVerified,
                super::journal::OperationStage::ArtifactValidated,
                super::journal::OperationStage::ProjectionSucceeded,
                super::journal::OperationStage::Committed,
            ]
        || journal.started_at.is_empty()
    {
        return Err(UpgradeError::Failed(
            "invalid or completed upgrade recovery journal".into(),
        ));
    }
    if journal.retry_intent
        != (RetryIntent::Upgrade {
            tool_id: id.clone(),
        })
    {
        return Err(UpgradeError::Failed(
            "upgrade recovery journal has the wrong retry intent".into(),
        ));
    }
    if no_pending && journal.pending_manifest_digest != secret_free_digest(argv) {
        return Err(UpgradeError::Failed(
            "stale upgrade recovery journal".into(),
        ));
    }
    Ok(Some(journal))
}

pub fn batch_eligibility(declarations: &[ToolDeclaration]) -> Vec<BatchEligibility> {
    let state = store::load_state().unwrap_or_default();
    declarations
        .iter()
        .map(|d| {
            let mut reasons = Vec::new();
            if d.hold_policy == HoldPolicy::Held {
                reasons.push("held".into());
            }
            if d.identity.adapter_kind == AdapterKind::ManualPath {
                reasons.push("manual-path".into());
            }
            if d.identity.adapter_kind == AdapterKind::WingetOnline
                && d.manager_takeover
                    .as_ref()
                    .and_then(|v| v.get("manager"))
                    .and_then(serde_json::Value::as_str)
                    != Some("winget")
            {
                reasons.push("WinGet takeover not accepted".into());
            }
            if super::adapters::gates::package_update_state(d.identity.adapter_kind)
                != CapabilityState::Supported
            {
                reasons.push("package update unsupported".into());
            }
            if state
                .get(&d.id)
                .is_some_and(|r| r.facets.manager_pin_state == ManagerPinState::Pinned)
            {
                reasons.push("manager pinned".into());
            }
            BatchEligibility {
                id: d.id.clone(),
                eligible: reasons.is_empty(),
                reasons,
            }
        })
        .collect()
}

fn blocked(action: &str) -> UpgradeError {
    UpgradeError::Blocked {
        action: action.to_owned(),
    }
}

fn write_journal(journal: &OperationJournal, paths: &store::ToolPaths) -> Result<(), UpgradeError> {
    let path = &paths.journal;
    let bytes =
        serde_json::to_vec_pretty(journal).map_err(|e| UpgradeError::Failed(e.to_string()))?;
    ccync_foundation::platform::atomic_write_bytes(path, &bytes)
        .map_err(|e| UpgradeError::Failed(e.to_string()))
}

fn secret_free_digest(argv: &[String]) -> String {
    use sha2::{Digest, Sha256};
    let normalized = argv
        .iter()
        .map(|a| {
            let key = a.to_ascii_lowercase();
            if ["token", "password", "secret", "auth"]
                .iter()
                .any(|x| key.contains(x))
            {
                "<redacted>"
            } else {
                a.as_str()
            }
        })
        .collect::<Vec<_>>()
        .join("\0");
    format!(
        "sha256:{}",
        hex::encode(Sha256::digest(normalized.as_bytes()))
    )
}

fn evidence_value(value: &EvidenceTuple) -> serde_json::Value {
    #[cfg(unix)]
    let identity =
        serde_json::json!({"dev": value.file_identity.dev, "ino": value.file_identity.ino});
    #[cfg(windows)]
    let identity = serde_json::json!({"volumeSerialNumber": value.file_identity.volume_serial_number, "fileIndex": value.file_identity.file_index});
    serde_json::json!({"adapterFields": value.adapter_fields, "resolvedTarget": value.resolved_target, "fileIdentity": identity, "sha256": value.sha256})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        install::{ProjectionRunReport, ProjectionTargetResult, ProjectionTargetStatus},
        tool_manager::{
            adapters::{AdapterCapabilities, AdapterError},
            discovery::{DiscoveryCandidate, DiscoveryQuery},
            lock::LifecycleLock,
            model::{
                ArtifactState, DeliveryId, EvidenceLevel, InstallationIdentity,
                ManagerAvailability, OperationState, PackageState, ProjectionState, SelectionState,
                ToolFacets,
            },
            probe::ProbeOutput,
            store::StateRecord,
        },
    };
    use ccync_foundation::target::ProjectionTargetId;
    use std::{
        fs,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex, MutexGuard,
        },
    };

    struct TestAdapter {
        kind: AdapterKind,
        package_update_cap: CapabilityState,
        manager_pin: ManagerPinState,
        update_argv: Option<Vec<String>>,
        identity: InstallationIdentity,
    }

    impl ToolAdapter for TestAdapter {
        fn kind(&self) -> AdapterKind {
            self.kind
        }
        fn capabilities(&self) -> AdapterCapabilities {
            AdapterCapabilities {
                local_enumeration: CapabilityState::Supported,
                executable_mapping: CapabilityState::Supported,
                installed_version_lookup: CapabilityState::Supported,
                remote_discovery: CapabilityState::Unsupported,
                package_update: self.package_update_cap,
            }
        }
        fn discover(
            &self,
            _query: &DiscoveryQuery,
        ) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
            Ok(vec![])
        }
        fn reprobe(
            &self,
            _query: &DiscoveryQuery,
        ) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
            Ok(vec![DiscoveryCandidate::new(
                self.identity.clone(),
                EvidenceLevel::Verified,
                true,
                true,
                false,
            )])
        }
        fn manager_pin(&self) -> Result<ManagerPinState, AdapterError> {
            Ok(self.manager_pin)
        }
        fn update_argv(&self) -> Option<Vec<String>> {
            self.update_argv.clone()
        }
    }

    struct CallbackRunner {
        calls: Arc<AtomicUsize>,
        output: ProbeOutput,
        on_run: Option<Arc<dyn Fn(&ProbeRequest) + Send + Sync>>,
    }

    impl ProbeRunner for CallbackRunner {
        fn run(&self, request: &ProbeRequest) -> ProbeOutput {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(ref hook) = self.on_run {
                hook(request);
            }
            self.output.clone()
        }
    }

    struct TestHarness {
        _env: MutexGuard<'static, ()>,
        _home: tempfile::TempDir,
        guard: LifecycleGuard,
        id: ToolId,
        exe_path: PathBuf,
        declaration: ToolDeclaration,
        runner_calls: Arc<AtomicUsize>,
        project_calls: Arc<AtomicUsize>,
    }

    fn test_facets() -> ToolFacets {
        ToolFacets {
            evidence_level: EvidenceLevel::Verified,
            selection_state: SelectionState::Unique,
            manager_availability: ManagerAvailability::Available,
            install_state: crate::tool_manager::model::InstallState::Installed,
            package_state: PackageState::MatchesArtifactSource,
            update_state: UpdateState::UpdateAvailable,
            artifact_state: ArtifactState::Current,
            projection_state: ProjectionState::Current,
            hold_policy: HoldPolicy::Unheld,
            manager_pin_state: ManagerPinState::Unpinned,
            operation_state: OperationState::Committed,
            last_check: None,
        }
    }

    fn make_report(status: ProjectionTargetStatus) -> ProjectionRunReport {
        let mut targets = BTreeMap::new();
        targets.insert(
            ProjectionTargetId::Claude,
            ProjectionTargetResult {
                target: ProjectionTargetId::Claude,
                status,
                paths: Vec::new(),
                warnings: Vec::new(),
            },
        );
        ProjectionRunReport {
            common: Vec::new(),
            targets,
            mode: "normal".into(),
            run_warnings: Vec::new(),
        }
    }

    fn setup(kind: AdapterKind, delivery: DeliveryId) -> TestHarness {
        let env = crate::test_support::env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("USERPROFILE", home.path());
        std::env::set_var("HOME", home.path());
        let guard = LifecycleLock::acquire().unwrap();
        let id = ToolId::new("demo").unwrap();
        let exe_dir = home.path().join("bin");
        fs::create_dir_all(&exe_dir).unwrap();
        let exe_path = exe_dir.join(if cfg!(windows) { "demo.exe" } else { "demo" });
        fs::write(&exe_path, b"initial-bytes-v1").unwrap();

        let mut fields = BTreeMap::new();
        fields.insert("executable".into(), exe_path.display().to_string());
        if kind == AdapterKind::Cargo {
            fields.insert("name".into(), "rtk".into());
        }
        if kind == AdapterKind::BrewFormula {
            fields.insert("name".into(), "wget".into());
            fields.insert("managerExecutable".into(), "brew".into());
        }

        let declaration = ToolDeclaration {
            id: id.clone(),
            identity: InstallationIdentity {
                adapter_kind: kind,
                fields,
            },
            delivery_id: delivery,
            hold_policy: HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        };

        TestHarness {
            _env: env,
            _home: home,
            guard,
            id,
            exe_path,
            declaration,
            runner_calls: Arc::new(AtomicUsize::new(0)),
            project_calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn init_tool_state(h: &TestHarness, adapter: &TestAdapter) {
        let runner = CallbackRunner {
            calls: Arc::new(AtomicUsize::new(0)),
            output: ProbeOutput::default(),
            on_run: None,
        };
        let e0 = capture_evidence(adapter, &runner, &h.declaration.identity).unwrap();
        let mut states = BTreeMap::new();
        states.insert(
            h.id.clone(),
            StateRecord {
                committed_generation: Some(1),
                artifact_fingerprint: Some("sha256:init".into()),
                source_evidence: evidence_value(&e0),
                operation_id: Some("init-op".into()),
                facets: test_facets(),
            },
        );
        store::commit_state(&states).unwrap();
    }

    #[test]
    fn pre_journal_blocks_create_no_journal_for_held_tool() {
        let mut h = setup(AdapterKind::Cargo, DeliveryId::RtkPath);
        h.declaration.hold_policy = HoldPolicy::Held;
        let adapter = TestAdapter {
            kind: AdapterKind::Cargo,
            package_update_cap: CapabilityState::Supported,
            manager_pin: ManagerPinState::Unpinned,
            update_argv: Some(vec!["cargo".into(), "install".into(), "demo".into()]),
            identity: h.declaration.identity.clone(),
        };
        init_tool_state(&h, &adapter);
        let runner = CallbackRunner {
            calls: h.runner_calls.clone(),
            output: ProbeOutput::default(),
            on_run: None,
        };
        let project = || make_report(ProjectionTargetStatus::Applied);
        let options = UpgradeOptions {
            declaration: &h.declaration,
            adapter: &adapter,
            package_update: UpdateState::UpdateAvailable,
            retry_manager_step: false,
            artifact_current: true,
            projection_current: true,
            dry_run: false,
        };
        let result = upgrade_tool(&h.guard, &h.id, &runner, project, options);
        match result {
            Err(UpgradeError::Blocked { action }) => {
                assert_eq!(action, "ccync tool hold demo --off");
            }
            other => panic!("expected Blocked error, got {other:?}"),
        }
        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        assert!(
            !journal_path.exists(),
            "no journal file should be created when held"
        );
    }

    #[test]
    fn pre_journal_blocks_create_no_journal_for_unsupported_capability() {
        let h = setup(AdapterKind::Cargo, DeliveryId::RtkPath);
        let adapter = TestAdapter {
            kind: AdapterKind::Cargo,
            package_update_cap: CapabilityState::Unsupported,
            manager_pin: ManagerPinState::Unpinned,
            update_argv: Some(vec!["cargo".into(), "install".into(), "demo".into()]),
            identity: h.declaration.identity.clone(),
        };
        init_tool_state(&h, &adapter);
        let runner = CallbackRunner {
            calls: h.runner_calls.clone(),
            output: ProbeOutput::default(),
            on_run: None,
        };
        let project = || make_report(ProjectionTargetStatus::Applied);
        let options = UpgradeOptions {
            declaration: &h.declaration,
            adapter: &adapter,
            package_update: UpdateState::UpdateAvailable,
            retry_manager_step: false,
            artifact_current: true,
            projection_current: true,
            dry_run: false,
        };
        let result = upgrade_tool(&h.guard, &h.id, &runner, project, options);
        match result {
            Err(UpgradeError::Blocked { action }) => {
                assert_eq!(action, "package update is unsupported for this tool");
            }
            other => panic!("expected Blocked error, got {other:?}"),
        }
        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        assert!(
            !journal_path.exists(),
            "no journal file should be created when unsupported"
        );
    }

    #[test]
    fn pre_journal_blocks_create_no_journal_for_winget_without_takeover() {
        let h = setup(AdapterKind::WingetOnline, DeliveryId::ObserveOnly);
        let adapter = TestAdapter {
            kind: AdapterKind::WingetOnline,
            package_update_cap: CapabilityState::Supported,
            manager_pin: ManagerPinState::Unpinned,
            update_argv: Some(vec!["winget".into(), "upgrade".into(), "demo".into()]),
            identity: h.declaration.identity.clone(),
        };
        init_tool_state(&h, &adapter);
        let runner = CallbackRunner {
            calls: h.runner_calls.clone(),
            output: ProbeOutput::default(),
            on_run: None,
        };
        let project = || make_report(ProjectionTargetStatus::Applied);
        let options = UpgradeOptions {
            declaration: &h.declaration,
            adapter: &adapter,
            package_update: UpdateState::UpdateAvailable,
            retry_manager_step: false,
            artifact_current: true,
            projection_current: true,
            dry_run: false,
        };
        let result = upgrade_tool(&h.guard, &h.id, &runner, project, options);
        match result {
            Err(UpgradeError::Blocked { action }) => {
                assert_eq!(action, "accept WinGet manager takeover before upgrading");
            }
            other => panic!("expected Blocked error, got {other:?}"),
        }
        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        assert!(
            !journal_path.exists(),
            "no journal file should be created without takeover"
        );
    }

    #[test]
    fn pre_journal_blocks_create_no_journal_for_manual_path() {
        let h = setup(AdapterKind::ManualPath, DeliveryId::ObserveOnly);
        let adapter = TestAdapter {
            kind: AdapterKind::ManualPath,
            package_update_cap: CapabilityState::Supported,
            manager_pin: ManagerPinState::Unpinned,
            update_argv: Some(vec!["manual".into()]),
            identity: h.declaration.identity.clone(),
        };
        init_tool_state(&h, &adapter);
        let runner = CallbackRunner {
            calls: h.runner_calls.clone(),
            output: ProbeOutput::default(),
            on_run: None,
        };
        let project = || make_report(ProjectionTargetStatus::Applied);
        let options = UpgradeOptions {
            declaration: &h.declaration,
            adapter: &adapter,
            package_update: UpdateState::UpdateAvailable,
            retry_manager_step: false,
            artifact_current: true,
            projection_current: true,
            dry_run: false,
        };
        let result = upgrade_tool(&h.guard, &h.id, &runner, project, options);
        match result {
            Err(UpgradeError::Blocked { action }) => {
                assert_eq!(action, "track this tool with a supported package manager");
            }
            other => panic!("expected Blocked error, got {other:?}"),
        }
        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        assert!(
            !journal_path.exists(),
            "no journal file should be created for manual path"
        );
    }

    #[test]
    fn pre_journal_blocks_create_no_journal_for_manager_pins() {
        // Brew formula pin
        let h_brew = setup(AdapterKind::BrewFormula, DeliveryId::ObserveOnly);
        let adapter_brew = TestAdapter {
            kind: AdapterKind::BrewFormula,
            package_update_cap: CapabilityState::Supported,
            manager_pin: ManagerPinState::Pinned,
            update_argv: Some(vec!["brew".into(), "upgrade".into(), "wget".into()]),
            identity: h_brew.declaration.identity.clone(),
        };
        init_tool_state(&h_brew, &adapter_brew);
        let runner = CallbackRunner {
            calls: h_brew.runner_calls.clone(),
            output: ProbeOutput::default(),
            on_run: None,
        };
        let project = || make_report(ProjectionTargetStatus::Applied);
        let options_brew = UpgradeOptions {
            declaration: &h_brew.declaration,
            adapter: &adapter_brew,
            package_update: UpdateState::UpdateAvailable,
            retry_manager_step: false,
            artifact_current: true,
            projection_current: true,
            dry_run: false,
        };
        let res_brew = upgrade_tool(&h_brew.guard, &h_brew.id, &runner, project, options_brew);
        match res_brew {
            Err(UpgradeError::Blocked { action }) => {
                assert_eq!(action, "brew unpin wget");
            }
            other => panic!("expected Blocked error, got {other:?}"),
        }
        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        assert!(
            !journal_path.exists(),
            "no journal file should be created for pinned brew formula"
        );
        drop(h_brew);

        // Generic adapter pin
        let h_cargo = setup(AdapterKind::Cargo, DeliveryId::RtkPath);
        let adapter_cargo = TestAdapter {
            kind: AdapterKind::Cargo,
            package_update_cap: CapabilityState::Supported,
            manager_pin: ManagerPinState::Pinned,
            update_argv: Some(vec!["cargo".into(), "install".into(), "demo".into()]),
            identity: h_cargo.declaration.identity.clone(),
        };
        init_tool_state(&h_cargo, &adapter_cargo);
        let options_cargo = UpgradeOptions {
            declaration: &h_cargo.declaration,
            adapter: &adapter_cargo,
            package_update: UpdateState::UpdateAvailable,
            retry_manager_step: false,
            artifact_current: true,
            projection_current: true,
            dry_run: false,
        };
        let res_cargo = upgrade_tool(&h_cargo.guard, &h_cargo.id, &runner, project, options_cargo);
        match res_cargo {
            Err(UpgradeError::Blocked { action }) => {
                assert_eq!(action, "clear the package manager pin");
            }
            other => panic!("expected Blocked error, got {other:?}"),
        }
        let journal_path_cargo = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        assert!(
            !journal_path_cargo.exists(),
            "no journal file should be created for generic pinned tool"
        );
    }

    #[test]
    fn manager_failure_runs_no_recipe_and_records_failed_state() {
        let h = setup(AdapterKind::Cargo, DeliveryId::RtkPath);
        let adapter = TestAdapter {
            kind: AdapterKind::Cargo,
            package_update_cap: CapabilityState::Supported,
            manager_pin: ManagerPinState::Unpinned,
            update_argv: Some(vec!["cargo".into(), "install".into(), "demo".into()]),
            identity: h.declaration.identity.clone(),
        };
        init_tool_state(&h, &adapter);
        let runner = CallbackRunner {
            calls: h.runner_calls.clone(),
            output: ProbeOutput {
                stdout: b"error: update failed".to_vec(),
                stderr: Vec::new(),
                exit_code: Some(1),
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
                spawn_error: None,
            },
            on_run: None,
        };
        let proj_calls = h.project_calls.clone();
        let project = move || {
            proj_calls.fetch_add(1, Ordering::SeqCst);
            make_report(ProjectionTargetStatus::Applied)
        };
        let options = UpgradeOptions {
            declaration: &h.declaration,
            adapter: &adapter,
            package_update: UpdateState::UpdateAvailable,
            retry_manager_step: false,
            artifact_current: true,
            projection_current: true,
            dry_run: false,
        };
        let result = upgrade_tool(&h.guard, &h.id, &runner, project, options);
        match result {
            Err(UpgradeError::ManagerFailed { exit_code }) => {
                assert_eq!(exit_code, Some(1));
            }
            other => panic!("expected ManagerFailed error, got {other:?}"),
        }
        assert_eq!(
            h.project_calls.load(Ordering::SeqCst),
            0,
            "recipe must not run on manager failure"
        );
        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        assert!(
            journal_path.exists(),
            "journal file must exist after manager failure"
        );
        let bytes = fs::read(&journal_path).unwrap();
        let journal: OperationJournal = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(journal.manager_step_state, ManagerStepState::Failed);
    }

    #[test]
    fn journal_is_outcome_unknown_before_spawn_and_contains_no_raw_manager_argv() {
        let h = setup(AdapterKind::Cargo, DeliveryId::RtkPath);
        let adapter = TestAdapter {
            kind: AdapterKind::Cargo,
            package_update_cap: CapabilityState::Supported,
            manager_pin: ManagerPinState::Unpinned,
            update_argv: Some(vec![
                "cargo".into(),
                "install".into(),
                "--token".into(),
                "supersecrettoken123".into(),
                "demo".into(),
            ]),
            identity: h.declaration.identity.clone(),
        };
        init_tool_state(&h, &adapter);

        let hook_saw_unknown = Arc::new(Mutex::new(false));
        let hook_saw_no_raw_argv = Arc::new(Mutex::new(false));
        let unknown_flag = hook_saw_unknown.clone();
        let no_raw_argv_flag = hook_saw_no_raw_argv.clone();

        let runner = CallbackRunner {
            calls: h.runner_calls.clone(),
            output: ProbeOutput {
                stdout: b"done".to_vec(),
                stderr: Vec::new(),
                exit_code: Some(0),
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
                spawn_error: None,
            },
            on_run: Some(Arc::new(move |_req| {
                let jpath = ccync_foundation::paths::tool_operation_journal_path().unwrap();
                if jpath.exists() {
                    let bytes = fs::read(&jpath).unwrap();
                    let raw_text = String::from_utf8_lossy(&bytes);
                    if !raw_text.contains("supersecrettoken123") && !raw_text.contains("--token") {
                        *no_raw_argv_flag.lock().unwrap() = true;
                    }
                    if let Ok(j) = serde_json::from_slice::<OperationJournal>(&bytes) {
                        if j.manager_step_state == ManagerStepState::ManagerOutcomeUnknown {
                            *unknown_flag.lock().unwrap() = true;
                        }
                    }
                }
            })),
        };

        let project = || make_report(ProjectionTargetStatus::Applied);
        let options = UpgradeOptions {
            declaration: &h.declaration,
            adapter: &adapter,
            package_update: UpdateState::UpdateAvailable,
            retry_manager_step: false,
            artifact_current: true,
            projection_current: true,
            dry_run: false,
        };
        let _ = upgrade_tool(&h.guard, &h.id, &runner, project, options);
        assert!(
            *hook_saw_unknown.lock().unwrap(),
            "journal must be manager-outcome-unknown before spawn"
        );
        assert!(
            *hook_saw_no_raw_argv.lock().unwrap(),
            "journal must not contain raw manager argv"
        );
    }

    #[test]
    fn successful_upgrade_projects_once() {
        let h = setup(AdapterKind::Cargo, DeliveryId::RtkPath);
        let adapter = TestAdapter {
            kind: AdapterKind::Cargo,
            package_update_cap: CapabilityState::Supported,
            manager_pin: ManagerPinState::Unpinned,
            update_argv: Some(vec!["cargo".into(), "install".into(), "demo".into()]),
            identity: h.declaration.identity.clone(),
        };
        init_tool_state(&h, &adapter);

        let exe_path_clone = h.exe_path.clone();
        let runner = CallbackRunner {
            calls: h.runner_calls.clone(),
            output: ProbeOutput {
                stdout: b"updated".to_vec(),
                stderr: Vec::new(),
                exit_code: Some(0),
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
                spawn_error: None,
            },
            on_run: Some(Arc::new(move |_req| {
                fs::write(&exe_path_clone, b"updated-binary-bytes-v2").unwrap();
            })),
        };

        let proj_calls = h.project_calls.clone();
        let project = move || {
            proj_calls.fetch_add(1, Ordering::SeqCst);
            make_report(ProjectionTargetStatus::Applied)
        };
        let options = UpgradeOptions {
            declaration: &h.declaration,
            adapter: &adapter,
            package_update: UpdateState::UpdateAvailable,
            retry_manager_step: false,
            artifact_current: true,
            projection_current: true,
            dry_run: false,
        };
        let result = upgrade_tool(&h.guard, &h.id, &runner, project, options);
        assert!(result.is_ok(), "upgrade must succeed when evidence changes");
        assert_eq!(
            h.project_calls.load(Ordering::SeqCst),
            1,
            "successful upgrade must project exactly once"
        );
        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        assert!(
            !journal_path.exists(),
            "journal must be removed after successful commit"
        );
        let states = store::load_state().unwrap();
        let record = states.get(&h.id).unwrap();
        assert_eq!(record.committed_generation, Some(2));
    }

    #[test]
    fn known_manager_success_and_committed_windows_never_repeat_manager() {
        use super::super::journal::{IrreversibleChange, OperationStage};
        for (stage, state_committed) in [
            (OperationStage::Prepared, false),
            (OperationStage::PackageVerified, false),
            (OperationStage::ArtifactValidated, false),
            (OperationStage::ProjectionSucceeded, false),
            (OperationStage::ProjectionSucceeded, true),
            (OperationStage::Committed, true),
        ] {
            let h = setup(AdapterKind::Cargo, DeliveryId::RtkPath);
            let adapter = TestAdapter {
                kind: AdapterKind::Cargo,
                package_update_cap: CapabilityState::Gated,
                manager_pin: ManagerPinState::Unpinned,
                update_argv: Some(vec!["cargo".into(), "install".into(), "demo".into()]),
                identity: h.declaration.identity.clone(),
            };
            init_tool_state(&h, &adapter);
            fs::write(&h.exe_path, b"manager-already-updated").unwrap();
            let mut journal = OperationJournal::new(
                h.id.clone(),
                OperationKind::Upgrade,
                RetryIntent::Upgrade {
                    tool_id: h.id.clone(),
                },
                None,
                PathBuf::new(),
                secret_free_digest(&adapter.update_argv.clone().unwrap()),
            );
            journal.manager_step_state = ManagerStepState::Succeeded;
            journal
                .irreversible_changes
                .push(IrreversibleChange::PackageInstalled);
            journal.stage = stage;
            if matches!(
                stage,
                OperationStage::ArtifactValidated
                    | OperationStage::ProjectionSucceeded
                    | OperationStage::Committed
            ) {
                let relative = store::expected_snapshot_relative_path(&h.id, 2).unwrap();
                let path = ccync_foundation::paths::ccync_build_root()
                    .unwrap()
                    .join(&relative);
                fs::create_dir_all(&path).unwrap();
                fs::write(path.join("tool"), b"manager-already-updated").unwrap();
                let manifest = crate::tool_manager::artifact_tree::scan_tree(&path).unwrap();
                journal.pending_generation = Some(2);
                journal.pending_snapshot_relative_path = relative.clone();
                journal.pending_manifest_digest = format!("sha256:{}", manifest.sha256);
                if state_committed {
                    let mut states = store::load_state().unwrap();
                    let record = states.get_mut(&h.id).unwrap();
                    record.committed_generation = Some(2);
                    record.operation_id = Some(journal.operation_id.clone());
                    record.artifact_fingerprint = Some(journal.pending_manifest_digest.clone());
                    store::commit_generation(
                        &states,
                        store::GenerationOwnership {
                            tool_id: h.id.clone(),
                            generation: 2,
                            operation_id: journal.operation_id.clone(),
                            snapshot_relative_path: relative,
                            manifest_digest: journal.pending_manifest_digest.clone(),
                            manifest,
                            directories: Vec::new(),
                        },
                    )
                    .unwrap();
                }
            }
            journal.create(&h.guard).unwrap();
            let runner = CallbackRunner {
                calls: h.runner_calls.clone(),
                output: ProbeOutput::default(),
                on_run: None,
            };
            let result = upgrade_tool(
                &h.guard,
                &h.id,
                &runner,
                || {
                    assert!(
                        !state_committed,
                        "committed operation must not repeat projection"
                    );
                    make_report(ProjectionTargetStatus::Applied)
                },
                UpgradeOptions {
                    declaration: &h.declaration,
                    adapter: &adapter,
                    package_update: UpdateState::Current,
                    retry_manager_step: false,
                    artifact_current: false,
                    projection_current: false,
                    dry_run: false,
                },
            )
            .unwrap();
            assert_eq!(h.runner_calls.load(Ordering::SeqCst), 0, "{stage:?}");
            assert_eq!(result.generation, refresh::FutureValue::Known(2));
            assert_eq!(
                store::load_state().unwrap()[&h.id].operation_id.as_deref(),
                Some(journal.operation_id.as_str())
            );
            assert!(!ccync_foundation::paths::tool_operation_journal_path()
                .unwrap()
                .exists());
        }
    }

    #[test]
    fn refresh_continuation_when_new_evidence_proves_package_change() {
        let h = setup(AdapterKind::Cargo, DeliveryId::RtkPath);
        let adapter = TestAdapter {
            kind: AdapterKind::Cargo,
            package_update_cap: CapabilityState::Supported,
            manager_pin: ManagerPinState::Unpinned,
            update_argv: Some(vec!["cargo".into(), "install".into(), "demo".into()]),
            identity: h.declaration.identity.clone(),
        };
        init_tool_state(&h, &adapter);

        // Pre-create journal at manager-outcome-unknown
        let mut journal = OperationJournal::new(
            h.id.clone(),
            OperationKind::Upgrade,
            RetryIntent::Upgrade {
                tool_id: h.id.clone(),
            },
            None,
            PathBuf::new(),
            secret_free_digest(&["cargo".into(), "install".into(), "demo".into()]),
        );
        journal.manager_step_state = ManagerStepState::ManagerOutcomeUnknown;
        journal.create(&h.guard).unwrap();

        // Mutate target binary before calling upgrade_tool to simulate package changed externally or before crash
        fs::write(&h.exe_path, b"already-updated-bytes-v3").unwrap();

        let runner = CallbackRunner {
            calls: h.runner_calls.clone(),
            output: ProbeOutput::default(),
            on_run: None,
        };
        let proj_calls = h.project_calls.clone();
        let project = move || {
            proj_calls.fetch_add(1, Ordering::SeqCst);
            make_report(ProjectionTargetStatus::Applied)
        };
        let options = UpgradeOptions {
            declaration: &h.declaration,
            adapter: &adapter,
            package_update: UpdateState::UpdateAvailable,
            retry_manager_step: false,
            artifact_current: true,
            projection_current: true,
            dry_run: false,
        };

        let result = upgrade_tool(&h.guard, &h.id, &runner, project, options);
        assert!(
            result.is_ok(),
            "recovery must continue through refresh when new evidence proves package change"
        );
        assert_eq!(
            h.runner_calls.load(Ordering::SeqCst),
            0,
            "manager step must not be rerun when evidence already changed"
        );
        assert_eq!(
            h.project_calls.load(Ordering::SeqCst),
            1,
            "refresh continuation must project once"
        );
        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        assert!(
            !journal_path.exists(),
            "journal must be removed after successful commit"
        );
    }

    #[test]
    fn exit_3_with_retry_flag_when_evidence_unchanged() {
        let h = setup(AdapterKind::Cargo, DeliveryId::RtkPath);
        let adapter = TestAdapter {
            kind: AdapterKind::Cargo,
            package_update_cap: CapabilityState::Supported,
            manager_pin: ManagerPinState::Unpinned,
            update_argv: Some(vec!["cargo".into(), "install".into(), "demo".into()]),
            identity: h.declaration.identity.clone(),
        };
        init_tool_state(&h, &adapter);

        // Manager succeeds, but binary on disk was NOT changed
        let runner = CallbackRunner {
            calls: h.runner_calls.clone(),
            output: ProbeOutput {
                stdout: b"already up to date".to_vec(),
                stderr: Vec::new(),
                exit_code: Some(0),
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
                spawn_error: None,
            },
            on_run: None,
        };
        let proj_calls = h.project_calls.clone();
        let project = move || {
            proj_calls.fetch_add(1, Ordering::SeqCst);
            make_report(ProjectionTargetStatus::Applied)
        };
        let options = UpgradeOptions {
            declaration: &h.declaration,
            adapter: &adapter,
            package_update: UpdateState::UpdateAvailable,
            retry_manager_step: false,
            artifact_current: true,
            projection_current: true,
            dry_run: false,
        };

        let result = upgrade_tool(&h.guard, &h.id, &runner, project, options);
        match result {
            Err(UpgradeError::OutcomeUnknown { retry_command }) => {
                assert_eq!(
                    retry_command,
                    "ccync upgrade tool:demo --retry-manager-step"
                );
            }
            other => panic!("expected OutcomeUnknown, got {other:?}"),
        }
        assert_eq!(
            h.project_calls.load(Ordering::SeqCst),
            0,
            "no recipe must run when evidence is unchanged"
        );
        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        assert!(
            journal_path.exists(),
            "journal must exist at manager-outcome-unknown"
        );
        let bytes = fs::read(&journal_path).unwrap();
        let journal: OperationJournal = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            journal.manager_step_state,
            ManagerStepState::ManagerOutcomeUnknown
        );
    }

    #[test]
    fn rerun_only_with_explicit_retry_manager_step_flag() {
        let h = setup(AdapterKind::Cargo, DeliveryId::RtkPath);
        let adapter = TestAdapter {
            kind: AdapterKind::Cargo,
            package_update_cap: CapabilityState::Supported,
            manager_pin: ManagerPinState::Unpinned,
            update_argv: Some(vec!["cargo".into(), "install".into(), "demo".into()]),
            identity: h.declaration.identity.clone(),
        };
        init_tool_state(&h, &adapter);

        // Pre-create journal at manager-outcome-unknown with unchanged evidence
        let mut journal = OperationJournal::new(
            h.id.clone(),
            OperationKind::Upgrade,
            RetryIntent::Upgrade {
                tool_id: h.id.clone(),
            },
            None,
            PathBuf::new(),
            secret_free_digest(&["cargo".into(), "install".into(), "demo".into()]),
        );
        journal.manager_step_state = ManagerStepState::ManagerOutcomeUnknown;
        journal.create(&h.guard).unwrap();

        let runner = CallbackRunner {
            calls: h.runner_calls.clone(),
            output: ProbeOutput {
                stdout: b"retried".to_vec(),
                stderr: Vec::new(),
                exit_code: Some(0),
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
                spawn_error: None,
            },
            on_run: None,
        };
        let project = || make_report(ProjectionTargetStatus::Applied);

        // 1. Without retry_manager_step: rejected, runner not called
        let options_no_retry = UpgradeOptions {
            declaration: &h.declaration,
            adapter: &adapter,
            package_update: UpdateState::UpdateAvailable,
            retry_manager_step: false,
            artifact_current: true,
            projection_current: true,
            dry_run: false,
        };
        let res_no_retry = upgrade_tool(&h.guard, &h.id, &runner, project, options_no_retry);
        match res_no_retry {
            Err(UpgradeError::OutcomeUnknown { retry_command }) => {
                assert!(retry_command.contains("--retry-manager-step"));
            }
            other => panic!("expected OutcomeUnknown, got {other:?}"),
        }
        assert_eq!(
            h.runner_calls.load(Ordering::SeqCst),
            0,
            "runner must not be called without retry flag"
        );

        // 2. With retry_manager_step: runner IS called
        let exe_clone = h.exe_path.clone();
        let runner_retry = CallbackRunner {
            calls: h.runner_calls.clone(),
            output: ProbeOutput {
                stdout: b"retried".to_vec(),
                stderr: Vec::new(),
                exit_code: Some(0),
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
                spawn_error: None,
            },
            on_run: Some(Arc::new(move |_req| {
                fs::write(&exe_clone, b"now-updated-on-retry").unwrap();
            })),
        };
        let options_retry = UpgradeOptions {
            declaration: &h.declaration,
            adapter: &adapter,
            package_update: UpdateState::UpdateAvailable,
            retry_manager_step: true,
            artifact_current: true,
            projection_current: true,
            dry_run: false,
        };
        let res_retry = upgrade_tool(&h.guard, &h.id, &runner_retry, project, options_retry);
        assert!(
            res_retry.is_ok(),
            "upgrade must succeed when retried with flag and evidence updates"
        );
        assert_eq!(
            h.runner_calls.load(Ordering::SeqCst),
            1,
            "runner must be called with retry flag"
        );
    }

    fn test_batch_declaration(
        id_str: &str,
        kind: AdapterKind,
        hold: HoldPolicy,
        takeover: Option<&str>,
    ) -> ToolDeclaration {
        let mut fields = BTreeMap::new();
        fields.insert("executable".into(), "/usr/bin/tool".into());
        let mut takeover_val = None;
        if let Some(mgr) = takeover {
            let mut map = serde_json::Map::new();
            map.insert("manager".into(), serde_json::Value::String(mgr.to_string()));
            takeover_val = Some(serde_json::Value::Object(map));
        }
        ToolDeclaration {
            id: ToolId::new(id_str).unwrap(),
            identity: InstallationIdentity {
                adapter_kind: kind,
                fields,
            },
            delivery_id: DeliveryId::ObserveOnly,
            hold_policy: hold,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: takeover_val,
        }
    }

    #[test]
    fn batch_eligibility_returns_all_applicable_exclusion_reasons() {
        let _env = crate::test_support::env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("USERPROFILE", home.path());
        std::env::set_var("HOME", home.path());

        // Setup state where tool "pinned-tool" has manager_pin_state = Pinned
        let mut states = BTreeMap::new();
        let mut facets_pinned = test_facets();
        facets_pinned.manager_pin_state = ManagerPinState::Pinned;
        states.insert(
            ToolId::new("pinned-tool").unwrap(),
            StateRecord {
                committed_generation: Some(1),
                artifact_fingerprint: Some("sha256:init".into()),
                source_evidence: serde_json::json!({}),
                operation_id: Some("op-1".into()),
                facets: facets_pinned,
            },
        );
        store::commit_state(&states).unwrap();

        let values = [
            test_batch_declaration("held-tool", AdapterKind::Cargo, HoldPolicy::Held, None),
            test_batch_declaration(
                "manual-tool",
                AdapterKind::ManualPath,
                HoldPolicy::Unheld,
                None,
            ),
            test_batch_declaration(
                "winget-no-takeover",
                AdapterKind::WingetOnline,
                HoldPolicy::Unheld,
                None,
            ),
            test_batch_declaration("pinned-tool", AdapterKind::Cargo, HoldPolicy::Unheld, None),
            test_batch_declaration(
                "multi-reason",
                AdapterKind::ManualPath,
                HoldPolicy::Held,
                None,
            ),
        ];
        let result = batch_eligibility(&values);
        assert_eq!(result.len(), 5);
        let cargo_gate =
            crate::tool_manager::adapters::gates::package_update_state(AdapterKind::Cargo);
        let cargo_unsupported = cargo_gate != CapabilityState::Supported;

        // held-tool
        assert_eq!(result[0].id.as_str(), "held-tool");
        assert!(!result[0].eligible);
        assert!(result[0].reasons.contains(&"held".into()));
        assert_eq!(
            result[0]
                .reasons
                .contains(&"package update unsupported".into()),
            cargo_unsupported
        );

        // manual-tool
        assert_eq!(result[1].id.as_str(), "manual-tool");
        assert!(!result[1].eligible);
        assert!(result[1].reasons.contains(&"manual-path".into()));
        assert!(result[1]
            .reasons
            .contains(&"package update unsupported".into()));

        // winget-no-takeover
        assert_eq!(result[2].id.as_str(), "winget-no-takeover");
        assert!(!result[2].eligible);
        assert!(result[2]
            .reasons
            .contains(&"WinGet takeover not accepted".into()));
        assert!(result[2]
            .reasons
            .contains(&"package update unsupported".into()));

        // pinned-tool
        assert_eq!(result[3].id.as_str(), "pinned-tool");
        assert!(!result[3].eligible);
        assert!(result[3].reasons.contains(&"manager pinned".into()));
        assert_eq!(
            result[3]
                .reasons
                .contains(&"package update unsupported".into()),
            cargo_unsupported
        );

        // multi-reason: held + manual-path + package update unsupported
        assert_eq!(result[4].id.as_str(), "multi-reason");
        assert!(!result[4].eligible);
        assert!(result[4].reasons.contains(&"held".into()));
        assert!(result[4].reasons.contains(&"manual-path".into()));
        assert!(result[4]
            .reasons
            .contains(&"package update unsupported".into()));
    }

    #[test]
    fn fresh_eligibility_requires_update_available() {
        let h = setup(AdapterKind::Cargo, DeliveryId::RtkPath);
        let adapter = TestAdapter {
            kind: AdapterKind::Cargo,
            package_update_cap: CapabilityState::Supported,
            manager_pin: ManagerPinState::Unpinned,
            update_argv: Some(vec!["cargo".into(), "install".into(), "demo".into()]),
            identity: h.declaration.identity.clone(),
        };
        init_tool_state(&h, &adapter);
        let runner = CallbackRunner {
            calls: h.runner_calls.clone(),
            output: ProbeOutput::default(),
            on_run: None,
        };
        let project = || make_report(ProjectionTargetStatus::Applied);

        let non_eligible_states = [
            UpdateState::Current,
            UpdateState::Unsupported,
            UpdateState::NotChecked,
            UpdateState::CheckFailed,
        ];

        for state in non_eligible_states {
            let options = UpgradeOptions {
                declaration: &h.declaration,
                adapter: &adapter,
                package_update: state,
                retry_manager_step: false,
                artifact_current: true,
                projection_current: true,
                dry_run: false,
            };
            let result = upgrade_tool(&h.guard, &h.id, &runner, project, options);
            match result {
                Err(UpgradeError::Blocked { action }) => {
                    assert_eq!(
                        action, "ccync tool outdated",
                        "non-UpdateAvailable state {state:?} must block with 'ccync tool outdated'"
                    );
                }
                other => panic!("expected Blocked for state {state:?}, got {other:?}"),
            }
        }
        assert_eq!(
            h.runner_calls.load(Ordering::SeqCst),
            0,
            "manager runner must not be called when blocked by eligibility"
        );
        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        assert!(
            !journal_path.exists(),
            "journal must not be created when update is not available"
        );
    }

    #[test]
    fn invalid_journal_missing_recovery_rejected() {
        let h = setup(AdapterKind::Cargo, DeliveryId::RtkPath);
        let adapter = TestAdapter {
            kind: AdapterKind::Cargo,
            package_update_cap: CapabilityState::Supported,
            manager_pin: ManagerPinState::Unpinned,
            update_argv: Some(vec!["cargo".into(), "install".into(), "demo".into()]),
            identity: h.declaration.identity.clone(),
        };
        init_tool_state(&h, &adapter);
        let runner = CallbackRunner {
            calls: h.runner_calls.clone(),
            output: ProbeOutput::default(),
            on_run: None,
        };
        let project = || make_report(ProjectionTargetStatus::Applied);
        let options = UpgradeOptions {
            declaration: &h.declaration,
            adapter: &adapter,
            package_update: UpdateState::UpdateAvailable,
            retry_manager_step: true,
            artifact_current: true,
            projection_current: true,
            dry_run: false,
        };
        let result = upgrade_tool(&h.guard, &h.id, &runner, project, options);
        match result {
            Err(UpgradeError::Failed(msg)) => {
                assert!(
                    msg.contains("manager retry requires a valid recovery journal"),
                    "unexpected error message: {msg}"
                );
            }
            other => panic!("expected Failed error for missing journal, got {other:?}"),
        }
        assert_eq!(h.runner_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn invalid_journal_foreign_tool_rejected() {
        let h = setup(AdapterKind::Cargo, DeliveryId::RtkPath);
        let adapter = TestAdapter {
            kind: AdapterKind::Cargo,
            package_update_cap: CapabilityState::Supported,
            manager_pin: ManagerPinState::Unpinned,
            update_argv: Some(vec!["cargo".into(), "install".into(), "demo".into()]),
            identity: h.declaration.identity.clone(),
        };
        init_tool_state(&h, &adapter);

        let foreign_id = ToolId::new("foreign").unwrap();
        let mut journal = OperationJournal::new(
            foreign_id.clone(),
            OperationKind::Upgrade,
            RetryIntent::Upgrade {
                tool_id: foreign_id,
            },
            None,
            PathBuf::new(),
            secret_free_digest(&["cargo".into(), "install".into(), "demo".into()]),
        );
        journal.manager_step_state = ManagerStepState::ManagerOutcomeUnknown;
        journal.create(&h.guard).unwrap();

        let runner = CallbackRunner {
            calls: h.runner_calls.clone(),
            output: ProbeOutput::default(),
            on_run: None,
        };
        let project = || make_report(ProjectionTargetStatus::Applied);
        let options = UpgradeOptions {
            declaration: &h.declaration,
            adapter: &adapter,
            package_update: UpdateState::UpdateAvailable,
            retry_manager_step: true,
            artifact_current: true,
            projection_current: true,
            dry_run: false,
        };
        let result = upgrade_tool(&h.guard, &h.id, &runner, project, options);
        match result {
            Err(UpgradeError::Failed(msg)) => {
                assert!(
                    msg.contains("recovery required for another operation"),
                    "unexpected error message: {msg}"
                );
            }
            other => panic!("expected Failed error for foreign journal, got {other:?}"),
        }
        assert_eq!(h.runner_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn invalid_journal_completed_state_rejected() {
        let h = setup(AdapterKind::Cargo, DeliveryId::RtkPath);
        let adapter = TestAdapter {
            kind: AdapterKind::Cargo,
            package_update_cap: CapabilityState::Supported,
            manager_pin: ManagerPinState::Unpinned,
            update_argv: Some(vec!["cargo".into(), "install".into(), "demo".into()]),
            identity: h.declaration.identity.clone(),
        };
        init_tool_state(&h, &adapter);

        let mut journal = OperationJournal::new(
            h.id.clone(),
            OperationKind::Upgrade,
            RetryIntent::Upgrade {
                tool_id: h.id.clone(),
            },
            None,
            PathBuf::new(),
            secret_free_digest(&["cargo".into(), "install".into(), "demo".into()]),
        );
        journal.manager_step_state = ManagerStepState::Succeeded;
        journal.create(&h.guard).unwrap();

        let runner = CallbackRunner {
            calls: h.runner_calls.clone(),
            output: ProbeOutput::default(),
            on_run: None,
        };
        let project = || make_report(ProjectionTargetStatus::Applied);
        let options = UpgradeOptions {
            declaration: &h.declaration,
            adapter: &adapter,
            package_update: UpdateState::UpdateAvailable,
            retry_manager_step: true,
            artifact_current: true,
            projection_current: true,
            dry_run: false,
        };
        let result = upgrade_tool(&h.guard, &h.id, &runner, project, options);
        match result {
            Err(UpgradeError::Failed(msg)) => {
                assert!(
                    msg.contains("invalid or completed upgrade recovery journal"),
                    "unexpected error message: {msg}"
                );
            }
            other => panic!("expected Failed error for completed journal, got {other:?}"),
        }
        assert_eq!(h.runner_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn invalid_journal_stale_digest_rejected() {
        let h = setup(AdapterKind::Cargo, DeliveryId::RtkPath);
        let adapter = TestAdapter {
            kind: AdapterKind::Cargo,
            package_update_cap: CapabilityState::Supported,
            manager_pin: ManagerPinState::Unpinned,
            update_argv: Some(vec!["cargo".into(), "install".into(), "demo".into()]),
            identity: h.declaration.identity.clone(),
        };
        init_tool_state(&h, &adapter);

        let mut journal = OperationJournal::new(
            h.id.clone(),
            OperationKind::Upgrade,
            RetryIntent::Upgrade {
                tool_id: h.id.clone(),
            },
            None,
            PathBuf::new(),
            "sha256:staledigestdifferentfromargv".into(),
        );
        journal.manager_step_state = ManagerStepState::ManagerOutcomeUnknown;
        journal.create(&h.guard).unwrap();

        let runner = CallbackRunner {
            calls: h.runner_calls.clone(),
            output: ProbeOutput::default(),
            on_run: None,
        };
        let project = || make_report(ProjectionTargetStatus::Applied);
        let options = UpgradeOptions {
            declaration: &h.declaration,
            adapter: &adapter,
            package_update: UpdateState::UpdateAvailable,
            retry_manager_step: true,
            artifact_current: true,
            projection_current: true,
            dry_run: false,
        };
        let result = upgrade_tool(&h.guard, &h.id, &runner, project, options);
        match result {
            Err(UpgradeError::Failed(msg)) => {
                assert!(
                    msg.contains("stale upgrade recovery journal"),
                    "unexpected error message: {msg}"
                );
            }
            other => panic!("expected Failed error for stale journal, got {other:?}"),
        }
        assert_eq!(h.runner_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn invalid_journal_malformed_json_rejected() {
        let h = setup(AdapterKind::Cargo, DeliveryId::RtkPath);
        let adapter = TestAdapter {
            kind: AdapterKind::Cargo,
            package_update_cap: CapabilityState::Supported,
            manager_pin: ManagerPinState::Unpinned,
            update_argv: Some(vec!["cargo".into(), "install".into(), "demo".into()]),
            identity: h.declaration.identity.clone(),
        };
        init_tool_state(&h, &adapter);

        let jpath = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        if let Some(parent) = jpath.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&jpath, b"{ not valid json").unwrap();

        let runner = CallbackRunner {
            calls: h.runner_calls.clone(),
            output: ProbeOutput::default(),
            on_run: None,
        };
        let project = || make_report(ProjectionTargetStatus::Applied);
        let options = UpgradeOptions {
            declaration: &h.declaration,
            adapter: &adapter,
            package_update: UpdateState::UpdateAvailable,
            retry_manager_step: true,
            artifact_current: true,
            projection_current: true,
            dry_run: false,
        };
        let result = upgrade_tool(&h.guard, &h.id, &runner, project, options);
        match result {
            Err(UpgradeError::Failed(msg)) => {
                assert!(
                    msg.contains("malformed upgrade recovery journal"),
                    "unexpected error message: {msg}"
                );
            }
            other => panic!("expected Failed error for malformed journal, got {other:?}"),
        }
        assert_eq!(h.runner_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn valid_recovery_bypasses_normal_gates() {
        let h = setup(AdapterKind::Cargo, DeliveryId::RtkPath);
        let adapter = TestAdapter {
            kind: AdapterKind::Cargo,
            package_update_cap: CapabilityState::Unsupported,
            manager_pin: ManagerPinState::Pinned,
            update_argv: Some(vec!["cargo".into(), "install".into(), "demo".into()]),
            identity: h.declaration.identity.clone(),
        };
        init_tool_state(&h, &adapter);

        let mut journal = OperationJournal::new(
            h.id.clone(),
            OperationKind::Upgrade,
            RetryIntent::Upgrade {
                tool_id: h.id.clone(),
            },
            None,
            PathBuf::new(),
            secret_free_digest(&["cargo".into(), "install".into(), "demo".into()]),
        );
        journal.manager_step_state = ManagerStepState::ManagerOutcomeUnknown;
        journal.create(&h.guard).unwrap();

        fs::write(&h.exe_path, b"already-updated-bytes-bypassing-gates").unwrap();

        let runner = CallbackRunner {
            calls: h.runner_calls.clone(),
            output: ProbeOutput::default(),
            on_run: None,
        };
        let proj_calls = h.project_calls.clone();
        let project = move || {
            proj_calls.fetch_add(1, Ordering::SeqCst);
            make_report(ProjectionTargetStatus::Applied)
        };
        let options = UpgradeOptions {
            declaration: &h.declaration,
            adapter: &adapter,
            package_update: UpdateState::Current,
            retry_manager_step: false,
            artifact_current: true,
            projection_current: true,
            dry_run: false,
        };

        let result = upgrade_tool(&h.guard, &h.id, &runner, project, options);
        assert!(
            result.is_ok(),
            "valid recovery journal must bypass package_update and capability gates"
        );
        assert_eq!(
            h.runner_calls.load(Ordering::SeqCst),
            0,
            "manager step must not be rerun when evidence changed"
        );
        assert_eq!(
            h.project_calls.load(Ordering::SeqCst),
            1,
            "refresh continuation must project once"
        );
        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        assert!(
            !journal_path.exists(),
            "journal must be removed after successful continuation"
        );
    }
}
