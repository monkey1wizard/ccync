use ccync_engine::install::{ProjectionRunReport, ProjectionTargetResult, ProjectionTargetStatus};
use ccync_engine::tool_manager::{
    adapters::{
        AdapterCapabilities, AdapterError, CapabilityState, RemoteUpdateStatus, ToolAdapter,
    },
    discovery::{DiscoveryCandidate, DiscoveryQuery},
    lock::LifecycleLock,
    model::{
        AdapterKind, DeliveryId, EvidenceLevel, InstallationIdentity, ManagerPinState, ToolId,
        UpdateState,
    },
    probe::{ProbeOutput, ProbeRequest, ProbeRunner},
    refresh::{self, RefreshOptions, RefreshStatus},
    status::{compute_status_with_adapter, StatusMode, UpdateFreshness},
    store,
    track::{track_tool_with_selector, TrackRequest},
    untrack::untrack_tool,
    upgrade::{upgrade_tool, UpgradeError, UpgradeOptions},
};
use ccync_foundation::target::ProjectionTargetId;
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};

struct LifecycleAdapter {
    identity: InstallationIdentity,
    remote: RemoteUpdateStatus,
}

impl ToolAdapter for LifecycleAdapter {
    fn kind(&self) -> AdapterKind {
        self.identity.adapter_kind
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            local_enumeration: CapabilityState::Supported,
            executable_mapping: CapabilityState::Supported,
            installed_version_lookup: CapabilityState::Supported,
            remote_discovery: CapabilityState::Supported,
            package_update: CapabilityState::Supported,
        }
    }

    fn discover(&self, _: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
        Ok(vec![DiscoveryCandidate::new(
            self.identity.clone(),
            EvidenceLevel::Verified,
            true,
            true,
            false,
        )])
    }

    fn reprobe(&self, _: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
        self.discover(&DiscoveryQuery { online: false })
    }

    fn remote_update_status(&self, _: &DiscoveryQuery) -> Result<RemoteUpdateStatus, AdapterError> {
        Ok(self.remote)
    }

    fn manager_pin(&self) -> Result<ManagerPinState, AdapterError> {
        Ok(ManagerPinState::Unpinned)
    }

    fn update_argv(&self) -> Option<Vec<String>> {
        Some(vec!["cargo".into(), "install".into(), "rtk".into()])
    }
}

struct LifecycleRunner {
    executable: PathBuf,
    mutate_manager_step: Arc<Mutex<bool>>,
    manager_calls: Arc<Mutex<Vec<(PathBuf, Vec<String>)>>>,
}

impl ProbeRunner for LifecycleRunner {
    fn run(&self, request: &ProbeRequest) -> ProbeOutput {
        if request.argv.first().is_some_and(|arg| arg == "install") {
            self.manager_calls
                .lock()
                .unwrap()
                .push((request.program.clone(), request.argv.clone()));
            if *self.mutate_manager_step.lock().unwrap() {
                let next_version = match fs::read(&self.executable).unwrap().as_slice() {
                    b"rtk-v1" => b"rtk-v2".as_slice(),
                    b"rtk-v2" => b"rtk-v3".as_slice(),
                    other => panic!("unexpected executable contents: {other:?}"),
                };
                fs::write(&self.executable, next_version).unwrap();
            }
        }
        ProbeOutput {
            exit_code: Some(0),
            ..Default::default()
        }
    }
}

fn projection_report() -> ProjectionRunReport {
    let target = ProjectionTargetId::Claude;
    ProjectionRunReport {
        common: Vec::new(),
        targets: [(
            target,
            ProjectionTargetResult {
                target,
                status: ProjectionTargetStatus::Applied,
                paths: Vec::new(),
                warnings: Vec::new(),
            },
        )]
        .into(),
        mode: "normal".into(),
        run_warnings: Vec::new(),
    }
}

#[test]
fn no_preseed_engine_lifecycle_sequence() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("USERPROFILE", home.path());
    std::env::set_var("HOME", home.path());
    let _lock = LifecycleLock::acquire().unwrap();

    let executable = home.path().join("bin").join("rtk");
    fs::create_dir_all(executable.parent().unwrap()).unwrap();
    fs::write(&executable, b"rtk-v1").unwrap();
    let identity = InstallationIdentity {
        adapter_kind: AdapterKind::Cargo,
        fields: [
            ("name".into(), "rtk".into()),
            ("executable".into(), executable.display().to_string()),
        ]
        .into(),
    };
    let adapter = LifecycleAdapter {
        identity: identity.clone(),
        remote: RemoteUpdateStatus::UpdateAvailable,
    };
    let runner = LifecycleRunner {
        executable: executable.clone(),
        mutate_manager_step: Arc::new(Mutex::new(true)),
        manager_calls: Arc::new(Mutex::new(Vec::new())),
    };
    let id = ToolId::new("rtk").unwrap();
    let adapters: [&dyn ToolAdapter; 1] = [&adapter];

    assert!(store::load_declarations().unwrap().is_empty());
    assert!(store::load_state().unwrap().is_empty());
    eprintln!("checkpoint: no-preseed empty declarations and state");

    let candidates = adapter.discover(&DiscoveryQuery { online: false }).unwrap();
    assert_eq!(candidates.len(), 1);
    let candidate_id = candidates[0].candidate_id.clone();
    eprintln!("checkpoint: discover selected candidate {candidate_id}");

    let track = track_tool_with_selector(
        &(_lock),
        TrackRequest {
            stable_id: "rtk".into(),
            adapters: &adapters,
            delivery: DeliveryId::RtkPath,
            command_selection: true,
            candidate_id: None,
            accept_limited_evidence: false,
            confirmed: false,
        },
        &runner,
        Some("rtk"),
    )
    .unwrap();
    assert!(track.inserted);
    assert!(store::load_state().unwrap().is_empty());
    assert!(store::load_declarations().unwrap().contains_key(&id));
    eprintln!("checkpoint: track declaration created without a StateRecord");

    let declaration = store::load_declarations().unwrap().remove(&id).unwrap();
    let local = compute_status_with_adapter(
        &declaration,
        None,
        StatusMode::Local,
        &runner,
        Some(&adapter),
    );
    assert_eq!(
        local.facets.install_state,
        ccync_engine::tool_manager::model::InstallState::Installed
    );
    assert_eq!(local.update_freshness, UpdateFreshness::Stale);
    eprintln!("checkpoint: Local refresh evidence captured with stale update freshness");

    let refreshed = refresh::refresh_tool(
        &_lock,
        &id,
        &runner,
        projection_report,
        RefreshOptions {
            declaration: &declaration,
            adapter: &adapter,
            artifact_current: false,
            projection_current: false,
            dry_run: false,
        },
    )
    .unwrap();
    assert_eq!(refreshed.status, RefreshStatus::Committed);
    let state = store::load_state().unwrap();
    assert!(state[&id].committed_generation.is_some());
    assert!(state[&id].source_evidence.get("sha256").is_some());
    assert!(refreshed
        .reload_advised_agents
        .contains(&"Claude".to_owned()));
    assert!(!ccync_foundation::paths::tool_operation_journal_path()
        .unwrap()
        .exists());
    eprintln!("checkpoint: Local refresh committed evidence, journal, and projection");

    let online = compute_status_with_adapter(
        &declaration,
        state.get(&id),
        StatusMode::Online,
        &runner,
        Some(&adapter),
    );
    assert_eq!(online.facets.update_state, UpdateState::UpdateAvailable);
    assert_eq!(online.update_freshness, UpdateFreshness::Fresh);
    eprintln!("checkpoint: Online update detection reported update-available");

    let dry_run = upgrade_tool(
        &_lock,
        &id,
        &runner,
        projection_report,
        UpgradeOptions {
            declaration: &declaration,
            adapter: &adapter,
            package_update: UpdateState::UpdateAvailable,
            retry_manager_step: false,
            artifact_current: true,
            projection_current: true,
            dry_run: true,
        },
    )
    .unwrap();
    assert_eq!(dry_run.status, RefreshStatus::DryRun);
    assert!(!ccync_foundation::paths::tool_operation_journal_path()
        .unwrap()
        .exists());
    eprintln!("checkpoint: dry-run upgrade left declaration and journal unchanged");

    let committed = upgrade_tool(
        &_lock,
        &id,
        &runner,
        projection_report,
        UpgradeOptions {
            declaration: &declaration,
            adapter: &adapter,
            package_update: UpdateState::UpdateAvailable,
            retry_manager_step: false,
            artifact_current: true,
            projection_current: true,
            dry_run: false,
        },
    );
    let committed = committed.unwrap();
    assert_eq!(committed.status, RefreshStatus::Committed);
    assert_eq!(
        store::load_state().unwrap()[&id].committed_generation,
        Some(2)
    );
    assert!(!ccync_foundation::paths::tool_operation_journal_path()
        .unwrap()
        .exists());
    eprintln!(
        "checkpoint: committed upgrade recorded manager call, evidence, journal, and projection"
    );

    *runner.mutate_manager_step.lock().unwrap() = false;
    let unknown = upgrade_tool(
        &_lock,
        &id,
        &runner,
        projection_report,
        UpgradeOptions {
            declaration: &declaration,
            adapter: &adapter,
            package_update: UpdateState::UpdateAvailable,
            retry_manager_step: false,
            artifact_current: true,
            projection_current: true,
            dry_run: false,
        },
    );
    assert!(matches!(unknown, Err(UpgradeError::OutcomeUnknown { .. })));
    let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
    assert!(journal_path.exists());
    let journal = fs::read_to_string(&journal_path).unwrap();
    assert!(journal.contains("manager-outcome-unknown"));
    eprintln!("checkpoint: later manager-outcome-unknown operation recorded journal and evidence boundary");

    *runner.mutate_manager_step.lock().unwrap() = true;
    let recovered = upgrade_tool(
        &_lock,
        &id,
        &runner,
        projection_report,
        UpgradeOptions {
            declaration: &declaration,
            adapter: &adapter,
            package_update: UpdateState::UpdateAvailable,
            retry_manager_step: true,
            artifact_current: true,
            projection_current: true,
            dry_run: false,
        },
    )
    .unwrap();
    assert_eq!(recovered.status, RefreshStatus::Committed);
    assert!(!journal_path.exists());
    assert_eq!(
        store::load_state().unwrap()[&id].committed_generation,
        Some(3)
    );
    assert!(recovered
        .reload_advised_agents
        .contains(&"Claude".to_owned()));
    eprintln!("checkpoint: explicit recovery committed new evidence, journal, and projection");

    let manager_calls = runner.manager_calls.lock().unwrap();
    assert_eq!(manager_calls.len(), 3);
    assert_eq!(manager_calls[0].0, PathBuf::from("cargo"));
    assert_eq!(manager_calls[0].1, ["install", "rtk"]);
    assert_eq!(manager_calls[1], manager_calls[0]);
    assert_eq!(manager_calls[2], manager_calls[0]);
    drop(manager_calls);
    eprintln!("checkpoint: manager calls and argv recorded for committed, unknown, and recovery operations");

    let untrack = untrack_tool(&_lock, id.clone(), projection_report);
    untrack.expect("the complete lifecycle must untrack without manual snapshot deletion");
    assert!(!store::load_declarations().unwrap().contains_key(&id));
    assert!(!store::load_state().unwrap().contains_key(&id));
    assert!(!journal_path.exists());
    assert!(!ccync_foundation::paths::tool_artifacts_root()
        .unwrap()
        .join(id.as_str())
        .exists());
    eprintln!("checkpoint: cleanup removed declaration, state, journal, and owned generations");
}
