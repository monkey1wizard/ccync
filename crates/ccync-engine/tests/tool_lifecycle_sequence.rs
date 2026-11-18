use ccync_engine::install::{ProjectionRunReport, ProjectionTargetResult, ProjectionTargetStatus};
use ccync_engine::tool_manager::{
    adapters::{
        AdapterCapabilities, AdapterError, CapabilityState, RemoteUpdateStatus, ToolAdapter,
    },
    discovery::{DiscoveryCandidate, DiscoveryQuery},
    lock::LifecycleLock,
    model::{
        AdapterKind, ArtifactState, DeliveryId, EvidenceLevel, HoldPolicy, InstallState,
        InstallationIdentity, ManagerAvailability, ManagerPinState, OperationState, PackageState,
        ProjectionState, SelectionState, ToolDeclaration, ToolFacets, ToolId, UpdateState,
    },
    probe::{ProbeOutput, ProbeRequest, ProbeRunner},
    refresh::{self, RefreshOptions, RefreshStatus},
    render,
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
    sync::{Arc, Mutex, MutexGuard},
};

static COMMITTED_LAYOUT_TEST_LOCK: Mutex<()> = Mutex::new(());

struct RestoreEnvironment(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl Drop for RestoreEnvironment {
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

struct CommittedLayoutFixture {
    home: tempfile::TempDir,
    _restore_environment: RestoreEnvironment,
    _serial: MutexGuard<'static, ()>,
    layout: ccync_foundation::paths::EffectiveLayout,
}

fn committed_layout_fixture() -> CommittedLayoutFixture {
    use ccync_foundation::{
        paths::{EffectiveLayout, StorageRoots},
        provider::{LayoutRootIdentity, StorageLayoutCommit},
    };

    let serial = COMMITTED_LAYOUT_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = tempfile::tempdir().unwrap();
    let keys = [
        "HOME",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ];
    let restore_environment =
        RestoreEnvironment(keys.map(|key| (key, std::env::var_os(key))).into());
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::set_var("USERPROFILE", home.path());
        std::env::set_var("APPDATA", home.path().join("roaming"));
        std::env::set_var("LOCALAPPDATA", home.path().join("local"));
        std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
        std::env::set_var("XDG_STATE_HOME", home.path().join("state"));
        std::env::set_var("XDG_CACHE_HOME", home.path().join("cache"));
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
    let layout = EffectiveLayout::load().unwrap();
    assert_eq!(
        layout.authority(),
        ccync_foundation::paths::EffectiveLayoutAuthority::Committed
    );
    CommittedLayoutFixture {
        home,
        _restore_environment: restore_environment,
        _serial: serial,
        layout,
    }
}

fn assert_no_legacy_tool_outputs(home: &std::path::Path) {
    assert!(!home.join(".ccync/build/tools").exists());
}

fn legacy_build_entries(home: &std::path::Path) -> Vec<PathBuf> {
    fn collect(root: &std::path::Path, current: &std::path::Path, entries: &mut Vec<PathBuf>) {
        let Ok(children) = fs::read_dir(current) else {
            return;
        };
        for child in children.map(Result::unwrap) {
            let path = child.path();
            entries.push(path.strip_prefix(root).unwrap().to_path_buf());
            if path.is_dir() {
                collect(root, &path, entries);
            }
        }
    }

    let root = home.join(".ccync/build");
    let mut entries = Vec::new();
    collect(&root, &root, &mut entries);
    entries.sort();
    entries
}

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
    let _serial = COMMITTED_LAYOUT_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = tempfile::tempdir().unwrap();
    let keys = [
        "HOME",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ];
    let _restore_environment =
        RestoreEnvironment(keys.map(|key| (key, std::env::var_os(key))).into());
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::set_var("USERPROFILE", home.path());
        std::env::set_var("APPDATA", home.path().join("roaming"));
        std::env::set_var("LOCALAPPDATA", home.path().join("local"));
        std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
        std::env::set_var("XDG_STATE_HOME", home.path().join("state"));
        std::env::set_var("XDG_CACHE_HOME", home.path().join("cache"));
    }
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

#[test]
fn tool_operation_paths_render_reads_and_stages_under_committed_layout() {
    let fixture = committed_layout_fixture();
    let id = ToolId::new("graphify").unwrap();
    let artifact = fixture
        .layout
        .tool_artifacts_root()
        .join(id.as_str())
        .join("1")
        .join(".agents/skills/graphify");
    fs::create_dir_all(&artifact).unwrap();
    fs::write(artifact.join("SKILL.md"), b"committed skill").unwrap();
    let manifest =
        ccync_engine::tool_manager::artifact_tree::scan_tree(artifact.ancestors().nth(3).unwrap())
            .unwrap();
    store::insert_declaration(ToolDeclaration {
        id: id.clone(),
        identity: InstallationIdentity {
            adapter_kind: AdapterKind::Cargo,
            fields: Default::default(),
        },
        delivery_id: DeliveryId::GraphifyAgents,
        hold_policy: HoldPolicy::Unheld,
        limited_evidence_acceptance: None,
        accepted_baseline: None,
        manager_takeover: None,
    })
    .unwrap();
    store::commit_state(
        &[(
            id.clone(),
            store::StateRecord {
                committed_generation: Some(1),
                artifact_fingerprint: Some(format!("sha256:{}", manifest.sha256)),
                source_evidence: serde_json::json!({}),
                operation_id: Some("committed-render-fixture".into()),
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
                    operation_state: OperationState::Committed,
                    last_check: None,
                },
            },
        )]
        .into(),
    )
    .unwrap();
    let staging = fixture
        .layout
        .state_base()
        .join("tools/staging/render-probe");
    fs::create_dir_all(&staging).unwrap();

    let warnings = render::materialize_tool_snapshots(&staging).unwrap();

    assert!(warnings.is_empty());
    assert_eq!(
        fs::read(staging.join("skills/graphify/SKILL.md")).unwrap(),
        b"committed skill"
    );
    assert!(artifact.starts_with(fixture.layout.tool_artifacts_root()));
    assert!(staging.starts_with(fixture.layout.state_base().join("tools")));
    assert!(fixture
        .layout
        .tools_path()
        .starts_with(fixture.layout.machine_config_root()));
    assert!(fixture
        .layout
        .tool_state_path()
        .starts_with(fixture.layout.state_base()));
    assert!(fixture
        .layout
        .tool_operation_journal_path()
        .starts_with(fixture.layout.state_base()));
    assert!(fixture
        .layout
        .tool_operation_journal_path()
        .starts_with(fixture.layout.state_base().join("tools")));
    assert!(staging.starts_with(fixture.layout.state_base().join("tools/staging")));
    assert_no_legacy_tool_outputs(fixture.home.path());
}

#[test]
fn tool_operation_paths_track_reads_lock_and_writes_under_committed_layout() {
    let fixture = committed_layout_fixture();
    let lock_path = fixture.layout.build_lock_path();
    fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
    let loose_skill = fixture.layout.state_base().join("tools/loose/rtk");
    fs::create_dir_all(&loose_skill).unwrap();
    let lock_content = serde_json::json!({
        "_looseSkills": [{"name": "rtk", "origin": loose_skill}]
    });
    fs::write(&lock_path, serde_json::to_vec(&lock_content).unwrap()).unwrap();
    let guard = LifecycleLock::acquire().unwrap();
    let executable = fixture.home.path().join("bin/rtk");
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
        identity,
        remote: RemoteUpdateStatus::UpdateAvailable,
    };
    let runner = LifecycleRunner {
        executable,
        mutate_manager_step: Arc::new(Mutex::new(false)),
        manager_calls: Arc::new(Mutex::new(Vec::new())),
    };
    let adapters: [&dyn ToolAdapter; 1] = [&adapter];
    let legacy_entries_before = legacy_build_entries(fixture.home.path());

    let outcome = track_tool_with_selector(
        &guard,
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

    assert!(outcome.inserted);
    assert_eq!(outcome.external_action, Some(loose_skill.clone()));
    assert!(loose_skill.starts_with(fixture.layout.state_base().join("tools")));
    assert!(lock_path.exists());
    assert!(fixture.layout.tools_path().exists());
    assert!(fixture
        .layout
        .tools_path()
        .starts_with(fixture.layout.machine_config_root()));
    assert!(lock_path.starts_with(fixture.layout.state_base()));
    assert!(fixture
        .layout
        .tool_operation_journal_path()
        .starts_with(fixture.layout.state_base().join("tools")));
    assert!(fixture
        .layout
        .tool_state_path()
        .starts_with(fixture.layout.state_base().join("tools")));
    assert!(fixture
        .layout
        .tool_artifacts_root()
        .join("rtk/1")
        .starts_with(fixture.layout.state_base().join("tools/artifacts")));
    assert!(fixture
        .layout
        .tool_operation_journal_path()
        .starts_with(fixture.layout.state_base().join("tools")));
    assert!(fixture
        .layout
        .state_base()
        .join("tools/staging")
        .starts_with(fixture.layout.state_base().join("tools")));
    assert!(lock_path.starts_with(fixture.layout.state_base()));
    assert!(fixture
        .layout
        .tool_artifacts_root()
        .starts_with(fixture.layout.state_base().join("tools")));
    assert!(fixture
        .layout
        .state_base()
        .join("tools/staging")
        .starts_with(fixture.layout.state_base().join("tools")));
    assert_eq!(
        legacy_build_entries(fixture.home.path()),
        legacy_entries_before
    );
    assert_no_legacy_tool_outputs(fixture.home.path());
}

#[test]
fn tool_operation_paths_upgrade_keeps_state_and_artifacts_under_committed_layout() {
    let fixture = committed_layout_fixture();
    let lock_path = fixture.layout.build_lock_path();
    fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
    fs::write(&lock_path, br#"{"_looseSkills":[]}"#).unwrap();
    let guard = LifecycleLock::acquire().unwrap();
    let executable = fixture.home.path().join("bin/rtk");
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
        identity,
        remote: RemoteUpdateStatus::UpdateAvailable,
    };
    let runner = LifecycleRunner {
        executable,
        mutate_manager_step: Arc::new(Mutex::new(true)),
        manager_calls: Arc::new(Mutex::new(Vec::new())),
    };
    let adapters: [&dyn ToolAdapter; 1] = [&adapter];
    let id = ToolId::new("rtk").unwrap();
    track_tool_with_selector(
        &guard,
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
    let declaration = store::load_declarations().unwrap().remove(&id).unwrap();
    let refreshed = refresh::refresh_tool(
        &guard,
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
    let legacy_entries_before = legacy_build_entries(fixture.home.path());

    let upgraded = upgrade_tool(
        &guard,
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
    )
    .unwrap();

    assert_eq!(upgraded.status, RefreshStatus::Committed);
    assert!(fixture.layout.tools_path().exists());
    assert!(fixture.layout.tool_state_path().exists());
    assert!(fixture
        .layout
        .tool_artifacts_root()
        .starts_with(fixture.layout.state_base().join("tools")));
    assert!(!fixture.layout.tool_operation_journal_path().exists());
    assert!(lock_path.starts_with(fixture.layout.state_base()));
    assert!(fixture
        .layout
        .tool_operation_journal_path()
        .starts_with(fixture.layout.state_base().join("tools")));
    assert!(fixture
        .layout
        .tool_artifacts_root()
        .join(id.as_str())
        .starts_with(fixture.layout.state_base().join("tools")));
    assert!(fixture
        .layout
        .state_base()
        .join("tools/staging")
        .starts_with(fixture.layout.state_base().join("tools")));
    assert_eq!(
        legacy_build_entries(fixture.home.path()),
        legacy_entries_before
    );
    assert_no_legacy_tool_outputs(fixture.home.path());
}
