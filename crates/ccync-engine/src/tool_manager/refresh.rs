//! Journaled refresh orchestration for tracked external tools.
use super::{
    adapters::ToolAdapter,
    artifact_tree, delivery,
    evidence::{capture_evidence, evidence_to_json, EvidenceTuple},
    journal::{OperationJournal, OperationKind, OperationStage, RetryIntent},
    lock::LifecycleGuard,
    model::{
        ArtifactState, EvidenceLevel, InstallState, ManagerAvailability, ManagerPinState,
        OperationState, PackageState, ProjectionState, SelectionState, ToolDeclaration, ToolFacets,
        ToolId, UpdateState,
    },
    probe::ProbeRunner,
    render::PendingGenerationScope,
    store,
};
use crate::install::{ProjectionRunReport, ProjectionTargetStatus};
use std::{fs, path::PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FutureValue<T> {
    Known(T),
    UnknownUntilExecuted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshStatus {
    NoOp,
    Committed,
    DryRun,
}

#[derive(Debug, Clone)]
pub struct RefreshOutcome {
    pub status: RefreshStatus,
    pub generation: FutureValue<u64>,
    pub artifact_fingerprint: FutureValue<String>,
    pub reload_advised_agents: Vec<String>,
}

pub struct RefreshOptions<'a> {
    pub declaration: &'a ToolDeclaration,
    pub adapter: &'a dyn ToolAdapter,
    pub artifact_current: bool,
    pub projection_current: bool,
    pub dry_run: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    #[error("tracked tool declaration is missing or does not match the requested id")]
    Declaration,
    #[error("refresh operation failed at {stage:?}: {reason}; retry with `{retry_command}`")]
    Failed {
        stage: OperationStage,
        reason: String,
        retry_command: String,
    },
    #[error("refresh state failed: {0}")]
    Store(#[from] store::StoreError),
    #[error("refresh journal failed: {0}")]
    Journal(#[from] super::journal::JournalError),
}

/// Run the recipe, validate its tree, project exactly once, then atomically commit state.
pub fn refresh_tool<P>(
    guard: &LifecycleGuard,
    id: &ToolId,
    runner: &dyn ProbeRunner,
    project: P,
    options: RefreshOptions<'_>,
) -> Result<RefreshOutcome, RefreshError>
where
    P: FnOnce() -> ProjectionRunReport,
{
    if &options.declaration.id != id {
        return Err(RefreshError::Declaration);
    }
    let paths = store::ToolPaths::load()?;
    let journal_path = paths.journal.clone();
    if journal_path.exists() {
        let mut journal: OperationJournal = serde_json::from_slice(
            &fs::read(&journal_path).map_err(|e| RefreshError::Journal(e.into()))?,
        )
        .map_err(|e| RefreshError::Journal(e.into()))?;
        journal.validate_identity()?;
        if journal.tool_id != *id || journal.kind != OperationKind::Refresh {
            return Err(super::journal::JournalError::RecoveryRequired.into());
        }
        if let Some(record) = store::committed_operation(&journal)? {
            if !options.dry_run {
                if journal.stage == OperationStage::ProjectionSucceeded {
                    journal.advance(guard, OperationStage::Committed)?;
                }
                journal.remove(guard)?;
            }
            return Ok(RefreshOutcome {
                status: RefreshStatus::NoOp,
                generation: record
                    .committed_generation
                    .map(FutureValue::Known)
                    .unwrap_or(FutureValue::UnknownUntilExecuted),
                artifact_fingerprint: record
                    .artifact_fingerprint
                    .map(FutureValue::Known)
                    .unwrap_or(FutureValue::UnknownUntilExecuted),
                reload_advised_agents: Vec::new(),
            });
        }
    }
    let e0 = capture_evidence(options.adapter, runner, &options.declaration.identity)
        .map_err(|e| failure(OperationStage::Prepared, e.to_string(), "ccync refresh"))?;
    let states = store::load_state()?;
    let prior = states
        .get(id)
        .cloned()
        .unwrap_or_else(|| initial_state_record(options.declaration));
    let encoded_e0 = evidence_to_json(&e0);
    if !journal_path.exists()
        && prior.source_evidence == encoded_e0
        && options.artifact_current
        && options.projection_current
    {
        return Ok(RefreshOutcome {
            status: RefreshStatus::NoOp,
            generation: prior
                .committed_generation
                .map(FutureValue::Known)
                .unwrap_or(FutureValue::UnknownUntilExecuted),
            artifact_fingerprint: prior
                .artifact_fingerprint
                .map(FutureValue::Known)
                .unwrap_or(FutureValue::UnknownUntilExecuted),
            reload_advised_agents: Vec::new(),
        });
    }
    if options.dry_run {
        return Ok(RefreshOutcome {
            status: RefreshStatus::DryRun,
            generation: FutureValue::UnknownUntilExecuted,
            artifact_fingerprint: FutureValue::UnknownUntilExecuted,
            reload_advised_agents: Vec::new(),
        });
    }

    let journal_path = paths.journal.clone();
    let (mut journal, is_resume) = if journal_path.exists() {
        let raw = fs::read(&journal_path).map_err(|e| RefreshError::Journal(e.into()))?;
        let existing: OperationJournal =
            serde_json::from_slice(&raw).map_err(|e| RefreshError::Journal(e.into()))?;
        if &existing.tool_id != id || existing.retry_intent != RetryIntent::Refresh {
            return Err(RefreshError::Journal(
                super::journal::JournalError::RecoveryRequired,
            ));
        }
        (existing, true)
    } else {
        let kind = OperationKind::Refresh;
        let intent = RetryIntent::Refresh;
        let j = OperationJournal::new(
            id.clone(),
            kind,
            intent,
            None,
            PathBuf::new(),
            String::new(),
        );
        j.create(guard)?;
        (j, false)
    };

    post_package_continuation(
        guard,
        &mut journal,
        id,
        runner,
        project,
        options,
        &prior,
        is_resume,
        Some(e0),
        &paths,
    )
}

fn initial_state_record(declaration: &ToolDeclaration) -> store::StateRecord {
    store::StateRecord {
        committed_generation: None,
        artifact_fingerprint: None,
        source_evidence: serde_json::Value::Null,
        operation_id: None,
        facets: ToolFacets {
            evidence_level: EvidenceLevel::Unknown,
            selection_state: SelectionState::Unique,
            manager_availability: ManagerAvailability::NotChecked,
            install_state: InstallState::NotChecked,
            package_state: PackageState::NotChecked,
            update_state: UpdateState::NotChecked,
            artifact_state: ArtifactState::NotChecked,
            projection_state: ProjectionState::NotChecked,
            hold_policy: declaration.hold_policy,
            manager_pin_state: ManagerPinState::Unknown,
            operation_state: OperationState::None,
            last_check: None,
        },
    }
}

/// Post-package continuation shared by refresh and upgrade after package verification.
#[allow(clippy::too_many_arguments)]
pub(super) fn post_package_continuation<P>(
    guard: &LifecycleGuard,
    journal: &mut OperationJournal,
    id: &ToolId,
    runner: &dyn ProbeRunner,
    project: P,
    options: RefreshOptions<'_>,
    prior: &store::StateRecord,
    is_resume: bool,
    pre_captured_e0: Option<EvidenceTuple>,
    paths: &store::ToolPaths,
) -> Result<RefreshOutcome, RefreshError>
where
    P: FnOnce() -> ProjectionRunReport,
{
    journal.validate_identity()?;
    if let Some(record) = store::committed_operation(journal)? {
        if journal.stage == OperationStage::ProjectionSucceeded {
            journal.advance(guard, OperationStage::Committed)?;
        }
        journal.remove(guard)?;
        return Ok(RefreshOutcome {
            status: RefreshStatus::NoOp,
            generation: record
                .committed_generation
                .map(FutureValue::Known)
                .unwrap_or(FutureValue::UnknownUntilExecuted),
            artifact_fingerprint: record
                .artifact_fingerprint
                .map(FutureValue::Known)
                .unwrap_or(FutureValue::UnknownUntilExecuted),
            reload_advised_agents: Vec::new(),
        });
    }
    let prior_pending_generation = journal.pending_generation;
    let prior_pending_relative_path = journal.pending_snapshot_relative_path.clone();
    let prior_pending_manifest_digest = journal.pending_manifest_digest.clone();

    if is_resume {
        journal.stage = OperationStage::PackageVerified;
        write_journal(journal, paths)
            .map_err(|e| failure(journal.stage, e, &journal.retry_command()))?;
    } else {
        journal.advance(guard, OperationStage::PackageVerified)?;
    }

    let e0 = match (is_resume, pre_captured_e0) {
        (false, Some(e0)) => e0,
        _ => capture_evidence(options.adapter, runner, &options.declaration.identity)
            .map_err(|e| failure(journal.stage, e.to_string(), &journal.retry_command()))?,
    };

    let staging = super::snapshot::create_staging(&journal.operation_id)
        .map_err(|e| failure(journal.stage, e.to_string(), &journal.retry_command()))?;
    delivery::run_recipe(
        options.declaration.delivery_id,
        &options.declaration.identity,
        staging.path(),
        runner,
    )
    .map_err(|e| failure(journal.stage, e.to_string(), &journal.retry_command()))?;

    let manifest = artifact_tree::scan_tree(staging.path())
        .map_err(|e| failure(journal.stage, e.to_string(), &journal.retry_command()))?;
    let e1 = capture_evidence(options.adapter, runner, &options.declaration.identity)
        .map_err(|e| failure(journal.stage, e.to_string(), &journal.retry_command()))?;
    ensure_same(&e0, &e1, journal.stage, &journal.retry_command())?;

    let generation = prior
        .committed_generation
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| {
            failure(
                journal.stage,
                "generation overflow".into(),
                &journal.retry_command(),
            )
        })?;
    let root = paths.artifacts.clone();
    let dest = root.join(id.as_str()).join(generation.to_string());
    store::validate_snapshot_ancestors(&dest)?;
    let build = paths.snapshot_base.clone();
    let relative = dest
        .strip_prefix(&build)
        .map_err(|_| {
            failure(
                journal.stage,
                "artifact path escaped build root".into(),
                &journal.retry_command(),
            )
        })?
        .to_path_buf();

    if dest.exists() {
        let existing = artifact_tree::scan_tree(&dest)
            .map_err(|e| failure(journal.stage, e.to_string(), &journal.retry_command()))?;
        let owned = is_resume
            && prior_pending_generation == Some(generation)
            && prior_pending_relative_path == relative
            && !prior_pending_manifest_digest.is_empty()
            && format!("sha256:{}", existing.sha256) == prior_pending_manifest_digest;
        if !owned {
            return Err(failure(
                journal.stage,
                "pending generation is not owned by this journal".into(),
                &journal.retry_command(),
            ));
        }
        fs::remove_dir_all(&dest)
            .map_err(|e| failure(journal.stage, e.to_string(), &journal.retry_command()))?;
    }

    journal.pending_generation = Some(generation);
    journal.pending_snapshot_relative_path = relative;
    journal.pending_manifest_digest = format!("sha256:{}", manifest.sha256);
    write_journal(journal, paths)
        .map_err(|e| failure(journal.stage, e, &journal.retry_command()))?;

    fs::create_dir_all(&dest)
        .map_err(|e| failure(journal.stage, e.to_string(), &journal.retry_command()))?;
    artifact_tree::materialize(&manifest, staging.path(), &dest)
        .map_err(|e| failure(journal.stage, e.to_string(), &journal.retry_command()))?;

    journal.advance(guard, OperationStage::ArtifactValidated)?;

    let e2 = capture_evidence(options.adapter, runner, &options.declaration.identity)
        .map_err(|e| failure(journal.stage, e.to_string(), &journal.retry_command()))?;
    ensure_same(&e0, &e2, journal.stage, &journal.retry_command())?;

    let report = {
        let _scope = PendingGenerationScope::new(id.clone(), generation);
        project()
    };

    let targets_valid = !report.targets.is_empty()
        && !report.targets.values().any(|target| {
            matches!(
                target.status,
                ProjectionTargetStatus::Failed { .. } | ProjectionTargetStatus::NotRun
            )
        })
        && report.targets.values().all(|target| match &target.status {
            ProjectionTargetStatus::Applied | ProjectionTargetStatus::Unchanged => true,
            ProjectionTargetStatus::Skipped { reason } => !reason.trim().is_empty(),
            _ => false,
        });

    if !targets_valid || !dest.exists() {
        return Err(failure(
            journal.stage,
            "projection report or canonical generation is incomplete".into(),
            &journal.retry_command(),
        ));
    }

    journal.advance(guard, OperationStage::ProjectionSucceeded)?;

    let mut next = store::load_state()?;
    let record = next
        .entry(id.clone())
        .or_insert_with(|| initial_state_record(options.declaration));
    record.source_evidence = evidence_to_json(&e2);
    record.committed_generation = Some(generation);
    record.artifact_fingerprint = Some(journal.pending_manifest_digest.clone());
    record.operation_id = Some(journal.operation_id.clone());
    record.facets.artifact_state = ArtifactState::Current;
    record.facets.projection_state = ProjectionState::Current;
    record.facets.operation_state = OperationState::Committed;
    let verified_manifest = artifact_tree::scan_tree(&dest)
        .map_err(|e| failure(journal.stage, e.to_string(), &journal.retry_command()))?;
    let proof = store::GenerationOwnership {
        tool_id: id.clone(),
        generation,
        operation_id: journal.operation_id.clone(),
        snapshot_relative_path: journal.pending_snapshot_relative_path.clone(),
        manifest_digest: format!("sha256:{}", verified_manifest.sha256),
        manifest: verified_manifest,
        directories: store::directory_inventory(&dest)
            .map_err(|e| failure(journal.stage, e.to_string(), &journal.retry_command()))?,
    };
    store::commit_generation(&next, proof)?;

    journal.advance(guard, OperationStage::Committed)?;
    journal.remove(guard)?;

    let advised = report
        .targets
        .iter()
        .filter(|(_, result)| matches!(result.status, ProjectionTargetStatus::Applied))
        .map(|(tid, _)| format!("{tid:?}"))
        .collect();

    Ok(RefreshOutcome {
        status: RefreshStatus::Committed,
        generation: FutureValue::Known(generation),
        artifact_fingerprint: FutureValue::Known(journal.pending_manifest_digest.clone()),
        reload_advised_agents: advised,
    })
}

pub(super) fn ensure_same(
    a: &EvidenceTuple,
    b: &EvidenceTuple,
    stage: OperationStage,
    retry: &str,
) -> Result<(), RefreshError> {
    a.compare(b)
        .map_err(|e| failure(stage, e.to_string(), retry))
}
pub(super) fn failure(stage: OperationStage, reason: String, retry: &str) -> RefreshError {
    RefreshError::Failed {
        stage,
        reason,
        retry_command: retry.to_owned(),
    }
}
fn write_journal(journal: &OperationJournal, paths: &store::ToolPaths) -> Result<(), String> {
    let path = &paths.journal;
    let bytes = serde_json::to_vec_pretty(journal).map_err(|e| e.to_string())?;
    ccync_foundation::platform::atomic_write_bytes(path, &bytes).map_err(|e| e.to_string())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::ProjectionTargetResult;
    use crate::tool_manager::{
        adapters::{AdapterCapabilities, AdapterError, CapabilityState},
        discovery::{DiscoveryCandidate, DiscoveryQuery},
        file_identity::FileIdentity,
        lock::LifecycleLock,
        model::{
            AdapterKind, ArtifactState, DeliveryId, EvidenceLevel, HoldPolicy,
            InstallationIdentity, ManagerAvailability, ManagerPinState, OperationState,
            PackageState, ProjectionState, SelectionState, ToolDeclaration, ToolFacets, ToolId,
            UpdateState,
        },
        probe::FixtureProbeRunner,
        store::{self, StateRecord},
    };
    use ccync_foundation::target::ProjectionTargetId;
    use std::{
        collections::BTreeMap,
        fs,
        path::PathBuf,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    struct RestoreStorageEnv([(&'static str, Option<std::ffi::OsString>); 2]);

    impl Drop for RestoreStorageEnv {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    fn commit_test_layout(home: &crate::test_support::TestHome) -> RestoreStorageEnv {
        use ccync_foundation::{
            paths::StorageRoots,
            provider::{LayoutRootIdentity, StorageLayoutCommit},
        };
        let saved = ["APPDATA", "LOCALAPPDATA"].map(|key| (key, std::env::var_os(key)));
        let restore = RestoreStorageEnv(saved);
        unsafe {
            std::env::set_var("APPDATA", home.path().join("roaming"));
            std::env::set_var("LOCALAPPDATA", home.path().join("local"));
        }
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
        let bytes = serde_json::to_vec(&commit).unwrap();
        fs::create_dir_all(home.path().join(".ccync")).unwrap();
        fs::write(home.path().join(".ccync/migration.json"), &bytes).unwrap();
        fs::write(roots.state.join("layout.json"), bytes).unwrap();
        restore
    }

    #[test]
    fn evidence_equality_accepts_identical_probe_tuples() {
        #[cfg(unix)]
        let identity = FileIdentity { dev: 1, ino: 2 };
        #[cfg(windows)]
        let identity = FileIdentity {
            volume_serial_number: 1,
            file_index: 2,
        };
        let value = EvidenceTuple {
            adapter_fields: InstallationIdentity {
                adapter_kind: AdapterKind::ManualPath,
                fields: BTreeMap::new(),
            },
            resolved_target: PathBuf::from("tool"),
            file_identity: identity,
            sha256: "hash".into(),
        };
        assert!(ensure_same(&value, &value, OperationStage::Prepared, "ccync refresh").is_ok());
    }

    fn test_facets() -> ToolFacets {
        ToolFacets {
            evidence_level: EvidenceLevel::Verified,
            selection_state: SelectionState::Unique,
            manager_availability: ManagerAvailability::Available,
            install_state: crate::tool_manager::model::InstallState::Installed,
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

    fn test_record(generation: Option<u64>, evidence: serde_json::Value) -> StateRecord {
        StateRecord {
            committed_generation: generation,
            artifact_fingerprint: Some("sha256:test".into()),
            source_evidence: evidence,
            operation_id: Some("prev-op".into()),
            facets: test_facets(),
        }
    }

    fn test_declaration(id: &ToolId, exe_path: &PathBuf) -> ToolDeclaration {
        let mut fields = BTreeMap::new();
        fields.insert("name".into(), "rtk".into());
        fields.insert("executable".into(), exe_path.display().to_string());
        ToolDeclaration {
            id: id.clone(),
            identity: InstallationIdentity {
                adapter_kind: AdapterKind::Cargo,
                fields,
            },
            delivery_id: DeliveryId::RtkPath,
            hold_policy: HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        }
    }

    struct FakeAdapter {
        #[allow(dead_code)]
        candidate_id: String,
        identity: InstallationIdentity,
        reprobe_calls: Arc<AtomicUsize>,
        mutate_step: Option<usize>,
        mutation: Option<Arc<dyn Fn() + Send + Sync>>,
    }

    impl ToolAdapter for FakeAdapter {
        fn kind(&self) -> AdapterKind {
            self.identity.adapter_kind
        }
        fn capabilities(&self) -> AdapterCapabilities {
            AdapterCapabilities::for_adapter(
                self.identity.adapter_kind,
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
            Ok(vec![])
        }
        fn reprobe(
            &self,
            _query: &DiscoveryQuery,
        ) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
            let call = self.reprobe_calls.fetch_add(1, Ordering::SeqCst);
            if Some(call) == self.mutate_step {
                if let Some(m) = &self.mutation {
                    m();
                }
            }
            Ok(vec![DiscoveryCandidate::new(
                self.identity.clone(),
                EvidenceLevel::Verified,
                true,
                true,
                false,
            )])
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

    struct Harness {
        _home: crate::test_support::TestHome,
        guard: LifecycleGuard,
        id: ToolId,
        exe_path: PathBuf,
        declaration: ToolDeclaration,
        reprobe_calls: Arc<AtomicUsize>,
        project_calls: Arc<AtomicUsize>,
    }

    fn setup() -> Harness {
        let home = crate::test_support::TestHome::new();
        let guard = LifecycleLock::acquire().unwrap();
        let id = ToolId::new("rtk").unwrap();
        let exe_dir = home.path().join("bin");
        fs::create_dir_all(&exe_dir).unwrap();
        let exe_path = exe_dir.join("rtk.exe");
        fs::write(&exe_path, b"initial-tool-binary-bytes").unwrap();
        let declaration = test_declaration(&id, &exe_path);
        let reprobe_calls = Arc::new(AtomicUsize::new(0));
        let project_calls = Arc::new(AtomicUsize::new(0));
        Harness {
            _home: home,
            guard,
            id,
            exe_path,
            declaration,
            reprobe_calls,
            project_calls,
        }
    }

    #[test]
    fn refresh_no_op_when_evidence_and_projections_current() {
        let h = setup();
        let runner = FixtureProbeRunner::new(vec![]);
        let adapter = FakeAdapter {
            candidate_id: "rtk-candidate".into(),
            identity: h.declaration.identity.clone(),
            reprobe_calls: h.reprobe_calls.clone(),
            mutate_step: None,
            mutation: None,
        };
        let e0 = capture_evidence(&adapter, &runner, &h.declaration.identity).unwrap();
        let mut map = BTreeMap::new();
        map.insert(h.id.clone(), test_record(Some(3), evidence_to_json(&e0)));
        store::commit_state(&map).unwrap();

        let project_calls = h.project_calls.clone();
        let projector = move || {
            project_calls.fetch_add(1, Ordering::SeqCst);
            make_report(ProjectionTargetStatus::Applied)
        };

        let outcome = refresh_tool(
            &h.guard,
            &h.id,
            &runner,
            projector,
            RefreshOptions {
                declaration: &h.declaration,
                adapter: &adapter,
                artifact_current: true,
                projection_current: true,
                dry_run: false,
            },
        )
        .unwrap();

        assert_eq!(outcome.status, RefreshStatus::NoOp);
        assert_eq!(outcome.generation, FutureValue::Known(3));
        assert_eq!(h.project_calls.load(Ordering::SeqCst), 0);
        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        assert!(!journal_path.exists());
    }

    #[test]
    fn refresh_dry_run_writes_nothing_and_marks_future_values_unknown() {
        let h = setup();
        let runner = FixtureProbeRunner::new(vec![]);
        let adapter = FakeAdapter {
            candidate_id: "rtk-candidate".into(),
            identity: h.declaration.identity.clone(),
            reprobe_calls: h.reprobe_calls.clone(),
            mutate_step: None,
            mutation: None,
        };
        let mut map = BTreeMap::new();
        map.insert(h.id.clone(), test_record(Some(1), serde_json::json!({})));
        store::commit_state(&map).unwrap();
        let raw_state_before =
            fs::read(ccync_foundation::paths::tool_state_path().unwrap()).unwrap();

        let project_calls = h.project_calls.clone();
        let projector = move || {
            project_calls.fetch_add(1, Ordering::SeqCst);
            make_report(ProjectionTargetStatus::Applied)
        };

        let outcome = refresh_tool(
            &h.guard,
            &h.id,
            &runner,
            projector,
            RefreshOptions {
                declaration: &h.declaration,
                adapter: &adapter,
                artifact_current: false,
                projection_current: false,
                dry_run: true,
            },
        )
        .unwrap();

        assert_eq!(outcome.status, RefreshStatus::DryRun);
        assert_eq!(outcome.generation, FutureValue::UnknownUntilExecuted);
        assert_eq!(
            outcome.artifact_fingerprint,
            FutureValue::UnknownUntilExecuted
        );
        assert_eq!(h.project_calls.load(Ordering::SeqCst), 0);
        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        assert!(!journal_path.exists());
        let raw_state_after =
            fs::read(ccync_foundation::paths::tool_state_path().unwrap()).unwrap();
        assert_eq!(raw_state_before, raw_state_after);
    }

    #[test]
    fn pending_refresh_must_resume_even_when_prior_facets_look_current() {
        let h = setup();
        let runner = FixtureProbeRunner::new(vec![]);
        let adapter = FakeAdapter {
            candidate_id: "rtk-candidate".into(),
            identity: h.declaration.identity.clone(),
            reprobe_calls: h.reprobe_calls.clone(),
            mutate_step: None,
            mutation: None,
        };
        let initial = refresh_tool(
            &h.guard,
            &h.id,
            &runner,
            || make_report(ProjectionTargetStatus::Applied),
            RefreshOptions {
                declaration: &h.declaration,
                adapter: &adapter,
                artifact_current: false,
                projection_current: false,
                dry_run: false,
            },
        )
        .unwrap();
        assert_eq!(initial.generation, FutureValue::Known(1));
        let journal = OperationJournal::new(
            h.id.clone(),
            OperationKind::Refresh,
            RetryIntent::Refresh,
            None,
            PathBuf::new(),
            String::new(),
        );
        journal.create(&h.guard).unwrap();
        let resumed = refresh_tool(
            &h.guard,
            &h.id,
            &runner,
            || make_report(ProjectionTargetStatus::Applied),
            RefreshOptions {
                declaration: &h.declaration,
                adapter: &adapter,
                artifact_current: true,
                projection_current: true,
                dry_run: false,
            },
        )
        .unwrap();
        assert_eq!(resumed.status, RefreshStatus::Committed);
        assert_eq!(resumed.generation, FutureValue::Known(2));
        assert_eq!(
            store::load_state().unwrap()[&h.id].operation_id.as_deref(),
            Some(journal.operation_id.as_str())
        );
        assert!(!ccync_foundation::paths::tool_operation_journal_path()
            .unwrap()
            .exists());
    }

    #[test]
    fn tool_operation_paths_refresh_writes_under_effective_layout() {
        let h = setup();
        let _restore_storage_env = commit_test_layout(&h._home);
        let runner = FixtureProbeRunner::new(vec![]);
        let adapter = FakeAdapter {
            candidate_id: "rtk-candidate".into(),
            identity: h.declaration.identity.clone(),
            reprobe_calls: h.reprobe_calls.clone(),
            mutate_step: None,
            mutation: None,
        };
        let mut map = BTreeMap::new();
        map.insert(h.id.clone(), test_record(Some(1), serde_json::json!({})));
        store::commit_state(&map).unwrap();

        let project_calls = h.project_calls.clone();
        let projector = move || {
            project_calls.fetch_add(1, Ordering::SeqCst);
            make_report(ProjectionTargetStatus::Applied)
        };

        let outcome = refresh_tool(
            &h.guard,
            &h.id,
            &runner,
            projector,
            RefreshOptions {
                declaration: &h.declaration,
                adapter: &adapter,
                artifact_current: false,
                projection_current: false,
                dry_run: false,
            },
        )
        .unwrap();

        assert_eq!(outcome.status, RefreshStatus::Committed);
        assert_eq!(outcome.generation, FutureValue::Known(2));
        assert_eq!(h.project_calls.load(Ordering::SeqCst), 1);
        assert_eq!(outcome.reload_advised_agents, vec!["Claude"]);

        let layout = ccync_foundation::paths::EffectiveLayout::load_or_legacy().unwrap();
        assert_eq!(
            layout.authority(),
            ccync_foundation::paths::EffectiveLayoutAuthority::Committed
        );
        let journal_path = layout.tool_operation_journal_path();
        assert!(!journal_path.exists());
        assert!(layout.tool_state_path().exists());
        assert!(layout
            .tool_artifacts_root()
            .join(h.id.as_str())
            .join("2")
            .exists());
        assert!(layout
            .tool_artifacts_root()
            .starts_with(layout.state_base().join("tools")));

        let states = store::load_state().unwrap();
        let current = states.get(&h.id).unwrap();
        assert_eq!(current.committed_generation, Some(2));
        assert_eq!(current.facets.artifact_state, ArtifactState::Current);
        assert_eq!(current.facets.projection_state, ProjectionState::Current);
        assert_eq!(current.facets.operation_state, OperationState::Committed);
    }

    #[test]
    fn refresh_creates_first_state_record_for_tracked_tool() {
        let h = setup();
        let runner = FixtureProbeRunner::new(vec![]);
        let adapter = FakeAdapter {
            candidate_id: "rtk-candidate".into(),
            identity: h.declaration.identity.clone(),
            reprobe_calls: h.reprobe_calls.clone(),
            mutate_step: None,
            mutation: None,
        };
        let project_calls = h.project_calls.clone();
        let outcome = refresh_tool(
            &h.guard,
            &h.id,
            &runner,
            move || {
                project_calls.fetch_add(1, Ordering::SeqCst);
                make_report(ProjectionTargetStatus::Applied)
            },
            RefreshOptions {
                declaration: &h.declaration,
                adapter: &adapter,
                artifact_current: false,
                projection_current: false,
                dry_run: false,
            },
        )
        .unwrap();

        assert_eq!(outcome.status, RefreshStatus::Committed);
        assert_eq!(outcome.generation, FutureValue::Known(1));
        let state = store::load_state().unwrap();
        let record = state.get(&h.id).expect("first refresh creates state");
        assert_eq!(record.committed_generation, Some(1));
        assert_eq!(record.facets.artifact_state, ArtifactState::Current);
        assert_eq!(record.facets.projection_state, ProjectionState::Current);
        assert_eq!(record.facets.operation_state, OperationState::Committed);
    }

    #[test]
    fn refresh_stops_on_e1_mismatch_with_unchanged_state_and_journal_at_package_verified() {
        let h = setup();
        let runner = FixtureProbeRunner::new(vec![]);
        let exe_clone = h.exe_path.clone();
        let adapter = FakeAdapter {
            candidate_id: "rtk-candidate".into(),
            identity: h.declaration.identity.clone(),
            reprobe_calls: h.reprobe_calls.clone(),
            mutate_step: Some(1), // Mutate at E1
            mutation: Some(Arc::new(move || {
                fs::write(&exe_clone, b"modified-tool-bytes-at-e1").unwrap();
            })),
        };
        let mut map = BTreeMap::new();
        map.insert(h.id.clone(), test_record(Some(1), serde_json::json!({})));
        store::commit_state(&map).unwrap();
        let raw_state_before =
            fs::read(ccync_foundation::paths::tool_state_path().unwrap()).unwrap();

        let project_calls = h.project_calls.clone();
        let projector = move || {
            project_calls.fetch_add(1, Ordering::SeqCst);
            make_report(ProjectionTargetStatus::Applied)
        };

        let err = refresh_tool(
            &h.guard,
            &h.id,
            &runner,
            projector,
            RefreshOptions {
                declaration: &h.declaration,
                adapter: &adapter,
                artifact_current: false,
                projection_current: false,
                dry_run: false,
            },
        )
        .unwrap_err();

        match err {
            RefreshError::Failed {
                stage,
                retry_command,
                ..
            } => {
                assert_eq!(stage, OperationStage::PackageVerified);
                assert_eq!(retry_command, "ccync tool refresh rtk");
            }
            other => panic!("expected RefreshError::Failed, got {other:?}"),
        }

        assert_eq!(h.project_calls.load(Ordering::SeqCst), 0);
        let raw_state_after =
            fs::read(ccync_foundation::paths::tool_state_path().unwrap()).unwrap();
        assert_eq!(raw_state_before, raw_state_after);

        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        assert!(journal_path.exists());
        let journal: OperationJournal =
            serde_json::from_slice(&fs::read(&journal_path).unwrap()).unwrap();
        assert_eq!(journal.stage, OperationStage::PackageVerified);
    }

    #[test]
    fn refresh_stops_on_e2_mismatch_with_unchanged_state_and_journal_at_artifact_validated() {
        let h = setup();
        let runner = FixtureProbeRunner::new(vec![]);
        let exe_clone = h.exe_path.clone();
        let adapter = FakeAdapter {
            candidate_id: "rtk-candidate".into(),
            identity: h.declaration.identity.clone(),
            reprobe_calls: h.reprobe_calls.clone(),
            mutate_step: Some(2), // Mutate at E2
            mutation: Some(Arc::new(move || {
                fs::write(&exe_clone, b"modified-tool-bytes-at-e2").unwrap();
            })),
        };
        let mut map = BTreeMap::new();
        map.insert(h.id.clone(), test_record(Some(1), serde_json::json!({})));
        store::commit_state(&map).unwrap();
        let raw_state_before =
            fs::read(ccync_foundation::paths::tool_state_path().unwrap()).unwrap();

        let project_calls = h.project_calls.clone();
        let projector = move || {
            project_calls.fetch_add(1, Ordering::SeqCst);
            make_report(ProjectionTargetStatus::Applied)
        };

        let err = refresh_tool(
            &h.guard,
            &h.id,
            &runner,
            projector,
            RefreshOptions {
                declaration: &h.declaration,
                adapter: &adapter,
                artifact_current: false,
                projection_current: false,
                dry_run: false,
            },
        )
        .unwrap_err();

        match err {
            RefreshError::Failed {
                stage,
                retry_command,
                ..
            } => {
                assert_eq!(stage, OperationStage::ArtifactValidated);
                assert_eq!(retry_command, "ccync tool refresh rtk");
            }
            other => panic!("expected RefreshError::Failed, got {other:?}"),
        }

        assert_eq!(h.project_calls.load(Ordering::SeqCst), 0);
        let raw_state_after =
            fs::read(ccync_foundation::paths::tool_state_path().unwrap()).unwrap();
        assert_eq!(raw_state_before, raw_state_after);

        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        assert!(journal_path.exists());
        let journal: OperationJournal =
            serde_json::from_slice(&fs::read(&journal_path).unwrap()).unwrap();
        assert_eq!(journal.stage, OperationStage::ArtifactValidated);
    }

    #[test]
    fn refresh_stops_on_recipe_failure_with_unchanged_state_and_journal_at_package_verified() {
        let h = setup();
        let runner = FixtureProbeRunner::new(vec![]);
        let adapter = FakeAdapter {
            candidate_id: "rtk-candidate".into(),
            identity: h.declaration.identity.clone(),
            reprobe_calls: h.reprobe_calls.clone(),
            mutate_step: None,
            mutation: None,
        };
        let mut bad_decl = h.declaration.clone();
        bad_decl.delivery_id = DeliveryId::ObserveOnly; // Incompatible with run_recipe!

        let mut map = BTreeMap::new();
        map.insert(h.id.clone(), test_record(Some(1), serde_json::json!({})));
        store::commit_state(&map).unwrap();
        let raw_state_before =
            fs::read(ccync_foundation::paths::tool_state_path().unwrap()).unwrap();

        let project_calls = h.project_calls.clone();
        let projector = move || {
            project_calls.fetch_add(1, Ordering::SeqCst);
            make_report(ProjectionTargetStatus::Applied)
        };

        let err = refresh_tool(
            &h.guard,
            &h.id,
            &runner,
            projector,
            RefreshOptions {
                declaration: &bad_decl,
                adapter: &adapter,
                artifact_current: false,
                projection_current: false,
                dry_run: false,
            },
        )
        .unwrap_err();

        match err {
            RefreshError::Failed {
                stage,
                retry_command,
                ..
            } => {
                assert_eq!(stage, OperationStage::PackageVerified);
                assert_eq!(retry_command, "ccync tool refresh rtk");
            }
            other => panic!("expected RefreshError::Failed, got {other:?}"),
        }

        assert_eq!(h.project_calls.load(Ordering::SeqCst), 0);
        let raw_state_after =
            fs::read(ccync_foundation::paths::tool_state_path().unwrap()).unwrap();
        assert_eq!(raw_state_before, raw_state_after);

        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        assert!(journal_path.exists());
        let journal: OperationJournal =
            serde_json::from_slice(&fs::read(&journal_path).unwrap()).unwrap();
        assert_eq!(journal.stage, OperationStage::PackageVerified);
    }

    #[test]
    fn refresh_stops_on_projection_failure_with_unchanged_state_and_journal_at_artifact_validated()
    {
        let h = setup();
        let runner = FixtureProbeRunner::new(vec![]);
        let adapter = FakeAdapter {
            candidate_id: "rtk-candidate".into(),
            identity: h.declaration.identity.clone(),
            reprobe_calls: h.reprobe_calls.clone(),
            mutate_step: None,
            mutation: None,
        };
        let mut map = BTreeMap::new();
        map.insert(h.id.clone(), test_record(Some(1), serde_json::json!({})));
        store::commit_state(&map).unwrap();
        let raw_state_before =
            fs::read(ccync_foundation::paths::tool_state_path().unwrap()).unwrap();

        let project_calls = h.project_calls.clone();
        let projector = move || {
            project_calls.fetch_add(1, Ordering::SeqCst);
            make_report(ProjectionTargetStatus::Failed {
                path: None,
                reason: "disk permission error".into(),
            })
        };

        let err = refresh_tool(
            &h.guard,
            &h.id,
            &runner,
            projector,
            RefreshOptions {
                declaration: &h.declaration,
                adapter: &adapter,
                artifact_current: false,
                projection_current: false,
                dry_run: false,
            },
        )
        .unwrap_err();

        match err {
            RefreshError::Failed {
                stage,
                retry_command,
                ..
            } => {
                assert_eq!(stage, OperationStage::ArtifactValidated);
                assert_eq!(retry_command, "ccync tool refresh rtk");
            }
            other => panic!("expected RefreshError::Failed, got {other:?}"),
        }

        assert_eq!(h.project_calls.load(Ordering::SeqCst), 1);
        let raw_state_after =
            fs::read(ccync_foundation::paths::tool_state_path().unwrap()).unwrap();
        assert_eq!(raw_state_before, raw_state_after);

        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        assert!(journal_path.exists());
        let journal: OperationJournal =
            serde_json::from_slice(&fs::read(&journal_path).unwrap()).unwrap();
        assert_eq!(journal.stage, OperationStage::ArtifactValidated);
    }

    #[test]
    fn refresh_stops_on_not_run_target_with_unchanged_state_and_journal_at_artifact_validated() {
        let h = setup();
        let runner = FixtureProbeRunner::new(vec![]);
        let adapter = FakeAdapter {
            candidate_id: "rtk-candidate".into(),
            identity: h.declaration.identity.clone(),
            reprobe_calls: h.reprobe_calls.clone(),
            mutate_step: None,
            mutation: None,
        };
        let mut map = BTreeMap::new();
        map.insert(h.id.clone(), test_record(Some(1), serde_json::json!({})));
        store::commit_state(&map).unwrap();
        let raw_state_before =
            fs::read(ccync_foundation::paths::tool_state_path().unwrap()).unwrap();

        let project_calls = h.project_calls.clone();
        let projector = move || {
            project_calls.fetch_add(1, Ordering::SeqCst);
            make_report(ProjectionTargetStatus::NotRun)
        };

        let err = refresh_tool(
            &h.guard,
            &h.id,
            &runner,
            projector,
            RefreshOptions {
                declaration: &h.declaration,
                adapter: &adapter,
                artifact_current: false,
                projection_current: false,
                dry_run: false,
            },
        )
        .unwrap_err();

        match err {
            RefreshError::Failed {
                stage,
                retry_command,
                ..
            } => {
                assert_eq!(stage, OperationStage::ArtifactValidated);
                assert_eq!(retry_command, "ccync tool refresh rtk");
            }
            other => panic!("expected RefreshError::Failed, got {other:?}"),
        }

        assert_eq!(h.project_calls.load(Ordering::SeqCst), 1);
        let raw_state_after =
            fs::read(ccync_foundation::paths::tool_state_path().unwrap()).unwrap();
        assert_eq!(raw_state_before, raw_state_after);

        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        assert!(journal_path.exists());
        let journal: OperationJournal =
            serde_json::from_slice(&fs::read(&journal_path).unwrap()).unwrap();
        assert_eq!(journal.stage, OperationStage::ArtifactValidated);
    }

    #[test]
    fn refresh_resume_from_recorded_stages_restarts_at_package_verification_and_replaces_journal_owned_generation(
    ) {
        let recorded_stages = [
            OperationStage::Prepared,
            OperationStage::PackageVerified,
            OperationStage::ArtifactValidated,
            OperationStage::ProjectionSucceeded,
        ];

        for stage in recorded_stages {
            let h = setup();
            let runner = FixtureProbeRunner::new(vec![]);
            let adapter = FakeAdapter {
                candidate_id: "rtk-candidate".into(),
                identity: h.declaration.identity.clone(),
                reprobe_calls: h.reprobe_calls.clone(),
                mutate_step: None,
                mutation: None,
            };

            let mut map = BTreeMap::new();
            map.insert(h.id.clone(), test_record(Some(0), serde_json::json!({})));
            store::commit_state(&map).unwrap();

            let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
            let dest_dir = ccync_foundation::paths::tool_artifacts_root()
                .unwrap()
                .join(h.id.as_str())
                .join("1");
            let build = ccync_foundation::paths::ccync_build_root().unwrap();
            let relative = dest_dir.strip_prefix(&build).unwrap().to_path_buf();

            let mut journal = OperationJournal::new(
                h.id.clone(),
                OperationKind::Refresh,
                RetryIntent::Refresh,
                None,
                PathBuf::new(),
                String::new(),
            );
            journal.stage = stage;

            if stage == OperationStage::ArtifactValidated
                || stage == OperationStage::ProjectionSucceeded
            {
                fs::create_dir_all(&dest_dir).unwrap();
                let manifest = artifact_tree::scan_tree(&dest_dir).unwrap();
                journal.pending_generation = Some(1);
                journal.pending_snapshot_relative_path = relative;
                journal.pending_manifest_digest = format!("sha256:{}", manifest.sha256);
            }

            fs::create_dir_all(journal_path.parent().unwrap()).unwrap();
            fs::write(&journal_path, serde_json::to_vec_pretty(&journal).unwrap()).unwrap();

            let project_calls = h.project_calls.clone();
            let projector = move || {
                project_calls.fetch_add(1, Ordering::SeqCst);
                make_report(ProjectionTargetStatus::Applied)
            };

            let outcome = refresh_tool(
                &h.guard,
                &h.id,
                &runner,
                projector,
                RefreshOptions {
                    declaration: &h.declaration,
                    adapter: &adapter,
                    artifact_current: false,
                    projection_current: false,
                    dry_run: false,
                },
            )
            .unwrap_or_else(|e| panic!("stage {stage:?} resume failed: {e:?}"));

            assert_eq!(outcome.status, RefreshStatus::Committed);
            assert_eq!(outcome.generation, FutureValue::Known(1));
            assert_eq!(h.project_calls.load(Ordering::SeqCst), 1);
            assert!(!journal_path.exists());

            let states = store::load_state().unwrap();
            assert_eq!(states.get(&h.id).unwrap().committed_generation, Some(1));
        }
    }

    #[test]
    fn refresh_resume_refuses_to_replace_unowned_generation() {
        let h = setup();
        let runner = FixtureProbeRunner::new(vec![]);
        let adapter = FakeAdapter {
            candidate_id: "rtk-candidate".into(),
            identity: h.declaration.identity.clone(),
            reprobe_calls: h.reprobe_calls.clone(),
            mutate_step: None,
            mutation: None,
        };

        let mut map = BTreeMap::new();
        map.insert(h.id.clone(), test_record(Some(0), serde_json::json!({})));
        store::commit_state(&map).unwrap();
        let raw_state_before =
            fs::read(ccync_foundation::paths::tool_state_path().unwrap()).unwrap();

        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        let dest_dir = ccync_foundation::paths::tool_artifacts_root()
            .unwrap()
            .join(h.id.as_str())
            .join("1");
        fs::create_dir_all(&dest_dir).unwrap();
        fs::write(dest_dir.join("foreign.txt"), b"unowned-content").unwrap();

        // Journal does NOT name this digest or generation
        let mut journal = OperationJournal::new(
            h.id.clone(),
            OperationKind::Refresh,
            RetryIntent::Refresh,
            None,
            PathBuf::new(),
            String::new(),
        );
        journal.stage = OperationStage::PackageVerified;
        fs::create_dir_all(journal_path.parent().unwrap()).unwrap();
        fs::write(&journal_path, serde_json::to_vec_pretty(&journal).unwrap()).unwrap();

        let project_calls = h.project_calls.clone();
        let projector = move || {
            project_calls.fetch_add(1, Ordering::SeqCst);
            make_report(ProjectionTargetStatus::Applied)
        };

        let err = refresh_tool(
            &h.guard,
            &h.id,
            &runner,
            projector,
            RefreshOptions {
                declaration: &h.declaration,
                adapter: &adapter,
                artifact_current: false,
                projection_current: false,
                dry_run: false,
            },
        )
        .unwrap_err();

        match err {
            RefreshError::Failed {
                stage,
                reason,
                retry_command,
            } => {
                assert_eq!(stage, OperationStage::PackageVerified);
                assert!(reason.contains("pending generation is not owned by this journal"));
                assert_eq!(retry_command, "ccync tool refresh rtk");
            }
            other => panic!("expected RefreshError::Failed, got {other:?}"),
        }

        assert_eq!(h.project_calls.load(Ordering::SeqCst), 0);
        let raw_state_after =
            fs::read(ccync_foundation::paths::tool_state_path().unwrap()).unwrap();
        assert_eq!(raw_state_before, raw_state_after);

        let journal_after: OperationJournal =
            serde_json::from_slice(&fs::read(&journal_path).unwrap()).unwrap();
        assert_eq!(journal_after.stage, OperationStage::PackageVerified);
    }
}
