//! Orthogonal local and remote status facets for tracked tools.
use super::{
    adapters::{CapabilityState, RemoteUpdateStatus},
    artifact_tree,
    model::ToolDeclaration,
    model::{
        ArtifactState, InstallState, ManagerAvailability, ManagerPinState, OperationState,
        PackageState, ProjectionState, ToolFacets, UpdateState,
    },
    probe::ProbeRunner,
    store::StateRecord,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusMode {
    Offline,
    Local,
    Online,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateFreshness {
    Fresh,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RemoteFailureDiagnostic {
    TimedOut,
    OutputLimit,
    CouldNotStart,
    UnsuccessfulExit,
    InvalidResponse,
    #[default]
    RemoteCheckFailed,
}

impl RemoteFailureDiagnostic {
    pub fn from_adapter_error(error: &super::adapters::AdapterError) -> Self {
        let super::adapters::AdapterError::Failed(message) = error else {
            return Self::RemoteCheckFailed;
        };
        if message == "interpreter pip probe timed out" {
            Self::TimedOut
        } else if message == "interpreter pip probe output exceeded limit" {
            Self::OutputLimit
        } else if message == "interpreter pip probe could not start" {
            Self::CouldNotStart
        } else if message.starts_with("interpreter pip probe exited unsuccessfully ") {
            Self::UnsuccessfulExit
        } else if message == "interpreter pip returned invalid JSON" {
            Self::InvalidResponse
        } else {
            Self::RemoteCheckFailed
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::TimedOut => "remote check timed out",
            Self::OutputLimit => "remote check output exceeded limit",
            Self::CouldNotStart => "remote check could not start",
            Self::UnsuccessfulExit => "remote check exited unsuccessfully",
            Self::InvalidResponse => "remote check returned an invalid response",
            Self::RemoteCheckFailed => "remote check failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusResult {
    pub facets: ToolFacets,
    pub update_freshness: UpdateFreshness,
    #[serde(skip)]
    pub remote_failure: Option<RemoteFailureDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextAction {
    pub command: Option<String>,
}

pub fn compute_facets(
    declaration: &ToolDeclaration,
    state: Option<&StateRecord>,
    mode: StatusMode,
    runner: &dyn ProbeRunner,
) -> ToolFacets {
    compute_status(declaration, state, mode, runner).facets
}

pub fn compute_status(
    declaration: &ToolDeclaration,
    state: Option<&StateRecord>,
    mode: StatusMode,
    runner: &dyn ProbeRunner,
) -> StatusResult {
    compute_status_with_adapter(declaration, state, mode, runner, None)
}

pub fn compute_facets_with_adapter(
    declaration: &ToolDeclaration,
    state: Option<&StateRecord>,
    mode: StatusMode,
    runner: &dyn ProbeRunner,
    adapter: Option<&dyn super::adapters::ToolAdapter>,
) -> ToolFacets {
    compute_status_with_adapter(declaration, state, mode, runner, adapter).facets
}

pub fn compute_status_with_adapter(
    declaration: &ToolDeclaration,
    state: Option<&StateRecord>,
    mode: StatusMode,
    runner: &dyn ProbeRunner,
    adapter: Option<&dyn super::adapters::ToolAdapter>,
) -> StatusResult {
    let _ = runner;
    let persisted = state.map(|s| &s.facets);
    let mut facets = persisted.cloned().unwrap_or(ToolFacets {
        evidence_level: super::model::EvidenceLevel::Unknown,
        selection_state: super::model::SelectionState::Unique,
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
    });
    facets.hold_policy = declaration.hold_policy;
    let local_mode = matches!(mode, StatusMode::Local | StatusMode::Online);
    if local_mode {
        if let Some(adapter) = adapter {
            match super::evidence::capture_evidence(adapter, runner, &declaration.identity) {
                Ok(evidence) => {
                    facets.install_state = InstallState::Installed;
                    let encoded = super::evidence::evidence_to_json(&evidence);
                    if state.is_some_and(|s| s.source_evidence != encoded) {
                        facets.package_state = PackageState::ExternallyChanged;
                    }
                }
                Err(super::evidence::CaptureError::IdentityNotFound) => {
                    facets.install_state = InstallState::Missing;
                    facets.package_state = PackageState::Missing;
                }
                Err(_) => facets.package_state = PackageState::VersionUnresolved,
            }
        }
    }
    if mode == StatusMode::Offline {
        if let Some(generation) = state.and_then(|s| s.committed_generation) {
            let path = super::store::ToolPaths::load().ok().map(|paths| {
                paths
                    .artifacts
                    .join(declaration.id.as_str())
                    .join(generation.to_string())
            });
            facets.artifact_state = match path.map(artifact_tree::scan_tree) {
                Some(Ok(manifest))
                    if state
                        .and_then(|s| s.artifact_fingerprint.as_deref())
                        .is_some_and(|f| f.ends_with(&manifest.sha256)) =>
                {
                    ArtifactState::Current
                }
                Some(Ok(_)) => ArtifactState::ArtifactDrifted,
                Some(Err(artifact_tree::TreeError::Io(e)))
                    if e.kind() == std::io::ErrorKind::NotFound =>
                {
                    ArtifactState::Missing
                }
                Some(Err(_)) => ArtifactState::Unreadable,
                None => ArtifactState::NotChecked,
            };
        }
        if let Some(root) = ccync_foundation::paths::canonical_plugin_root() {
            facets.projection_state = if root.is_dir() {
                ProjectionState::Current
            } else {
                ProjectionState::Missing
            };
        }
        return StatusResult {
            facets,
            update_freshness: UpdateFreshness::Stale,
            remote_failure: None,
        };
    } else {
        facets.manager_availability =
            match super::adapters::gates::package_update_state(declaration.identity.adapter_kind) {
                CapabilityState::Supported | CapabilityState::Gated => {
                    ManagerAvailability::Available
                }
                CapabilityState::Unsupported => ManagerAvailability::NotApplicable,
            };
        if mode == StatusMode::Online {
            let mut remote_failure = None;
            facets.update_state = match adapter {
                Some(adapter) => match adapter
                    .remote_update_status(&super::discovery::DiscoveryQuery { online: true })
                {
                    Ok(RemoteUpdateStatus::Current) => UpdateState::Current,
                    Ok(RemoteUpdateStatus::UpdateAvailable) => UpdateState::UpdateAvailable,
                    Ok(RemoteUpdateStatus::Unsupported)
                    | Err(super::adapters::AdapterError::Unsupported) => UpdateState::Unsupported,
                    Ok(RemoteUpdateStatus::NotChecked)
                    | Err(super::adapters::AdapterError::NotChecked) => UpdateState::NotChecked,
                    Err(error @ super::adapters::AdapterError::Failed(_)) => {
                        remote_failure = Some(RemoteFailureDiagnostic::from_adapter_error(&error));
                        UpdateState::CheckFailed
                    }
                },
                None => UpdateState::Unsupported,
            };
            facets.last_check = Some(chrono::Utc::now().to_rfc3339());
            return StatusResult {
                facets,
                update_freshness: UpdateFreshness::Fresh,
                remote_failure,
            };
        }
        return StatusResult {
            facets,
            update_freshness: UpdateFreshness::Stale,
            remote_failure: None,
        };
    }
}

pub fn next_action(
    declaration: &ToolDeclaration,
    facets: &ToolFacets,
    snapshot_valid: bool,
    refresh_supported: bool,
    update_supported: bool,
) -> NextAction {
    use super::model::{PackageState::ExternallyChanged, UpdateState::UpdateAvailable};
    let command = if facets.install_state == InstallState::Installed {
        Some("ccync outdated".into())
    } else if facets.package_state == ExternallyChanged && refresh_supported {
        Some(format!("ccync tool refresh {}", declaration.id))
    } else if facets.update_state == UpdateAvailable && update_supported {
        Some(format!("ccync upgrade tool:{}", declaration.id))
    } else if facets.projection_state == ProjectionState::Drifted && snapshot_valid {
        Some("ccync sync".into())
    } else {
        None
    };
    NextAction { command }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_manager::{
        adapters::{AdapterCapabilities, AdapterError, CapabilityState, ToolAdapter},
        discovery::{DiscoveryCandidate, DiscoveryQuery},
        model::{AdapterKind, DeliveryId, EvidenceLevel, HoldPolicy, InstallationIdentity, ToolId},
        probe::FixtureProbeRunner,
    };
    use std::collections::BTreeMap;
    fn declaration() -> ToolDeclaration {
        ToolDeclaration {
            id: ToolId::new("demo").unwrap(),
            identity: InstallationIdentity {
                adapter_kind: AdapterKind::ManualPath,
                fields: BTreeMap::new(),
            },
            delivery_id: DeliveryId::ObserveOnly,
            hold_policy: HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        }
    }
    #[test]
    fn offline_defaults_are_not_checked_and_use_no_runner() {
        let runner = FixtureProbeRunner::new([]);
        let f = compute_facets(&declaration(), None, StatusMode::Offline, &runner);
        assert_eq!(f.package_state, PackageState::NotChecked);
        assert_eq!(f.update_state, UpdateState::NotChecked);
        assert!(runner.requests().is_empty());
    }
    #[test]
    fn offline_preserves_persisted_pending_package_and_update_evidence() {
        let runner = FixtureProbeRunner::new([]);
        let d = declaration();
        let mut facets = compute_facets(&d, None, StatusMode::Offline, &runner);
        facets.package_state = PackageState::ExternallyChanged;
        facets.update_state = UpdateState::UpdateAvailable;
        let state = StateRecord {
            committed_generation: None,
            artifact_fingerprint: None,
            source_evidence: serde_json::Value::Null,
            operation_id: None,
            facets,
        };
        let result = compute_facets(&d, Some(&state), StatusMode::Offline, &runner);
        assert_eq!(result.package_state, PackageState::ExternallyChanged);
        assert_eq!(result.update_state, UpdateState::UpdateAvailable);
        assert!(runner.requests().is_empty());
    }
    struct ReprobeFixture(DiscoveryCandidate);
    impl ToolAdapter for ReprobeFixture {
        fn kind(&self) -> AdapterKind {
            self.0.identity.adapter_kind
        }
        fn capabilities(&self) -> AdapterCapabilities {
            AdapterCapabilities::for_adapter(
                self.kind(),
                CapabilityState::Supported,
                CapabilityState::Supported,
                CapabilityState::Supported,
                CapabilityState::Unsupported,
            )
        }
        fn discover(&self, _: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
            Ok(vec![self.0.clone()])
        }
        fn reprobe(&self, _: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
            Ok(vec![self.0.clone()])
        }
    }

    #[test]
    fn local_reprobes_and_marks_changed_package_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        std::fs::write(&path, b"fixture tool").unwrap();
        let mut d = declaration();
        d.identity
            .fields
            .insert("executable".into(), path.to_string_lossy().into_owned());
        let candidate = DiscoveryCandidate::new(
            d.identity.clone(),
            EvidenceLevel::Manageable,
            true,
            false,
            true,
        );
        let adapter = ReprobeFixture(candidate);
        let mut facets =
            compute_facets(&d, None, StatusMode::Offline, &FixtureProbeRunner::new([]));
        facets.package_state = PackageState::MatchesArtifactSource;
        let state = StateRecord {
            committed_generation: None,
            artifact_fingerprint: None,
            source_evidence: serde_json::json!({"old": true}),
            operation_id: None,
            facets,
        };
        let result = compute_facets_with_adapter(
            &d,
            Some(&state),
            StatusMode::Local,
            &FixtureProbeRunner::new([]),
            Some(&adapter),
        );
        assert_eq!(result.install_state, InstallState::Installed);
        assert_eq!(result.package_state, PackageState::ExternallyChanged);
    }

    struct RemoteStatusFixture {
        calls: std::sync::atomic::AtomicUsize,
        result: Result<RemoteUpdateStatus, AdapterError>,
        package_update: CapabilityState,
        events: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
    }
    impl ToolAdapter for RemoteStatusFixture {
        fn kind(&self) -> AdapterKind {
            AdapterKind::ManualPath
        }
        fn capabilities(&self) -> AdapterCapabilities {
            AdapterCapabilities {
                local_enumeration: CapabilityState::Supported,
                executable_mapping: CapabilityState::Supported,
                installed_version_lookup: CapabilityState::Supported,
                remote_discovery: CapabilityState::Unsupported,
                package_update: self.package_update,
            }
        }
        fn discover(&self, _: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
            Ok(vec![])
        }
        fn reprobe(&self, _: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
            self.events.lock().unwrap().push("local");
            Ok(vec![])
        }
        fn remote_update_status(
            &self,
            query: &DiscoveryQuery,
        ) -> Result<RemoteUpdateStatus, AdapterError> {
            assert!(query.online);
            self.events.lock().unwrap().push("remote");
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.result.clone()
        }
    }

    fn remote_fixture(
        result: Result<RemoteUpdateStatus, AdapterError>,
        package_update: CapabilityState,
    ) -> RemoteStatusFixture {
        RemoteStatusFixture {
            calls: std::sync::atomic::AtomicUsize::new(0),
            result,
            package_update,
            events: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    fn persisted_update_state(update_state: UpdateState) -> StateRecord {
        let mut facets = compute_facets(
            &declaration(),
            None,
            StatusMode::Offline,
            &FixtureProbeRunner::new([]),
        );
        facets.update_state = update_state;
        StateRecord {
            committed_generation: None,
            artifact_fingerprint: None,
            source_evidence: serde_json::Value::Null,
            operation_id: None,
            facets,
        }
    }

    #[test]
    fn online_remote_status_results_replace_stale_state_with_typed_result() {
        let cases = [
            (
                Ok(RemoteUpdateStatus::Current),
                CapabilityState::Supported,
                UpdateState::UpdateAvailable,
                UpdateState::Current,
            ),
            (
                Ok(RemoteUpdateStatus::Current),
                CapabilityState::Unsupported,
                UpdateState::UpdateAvailable,
                UpdateState::Current,
            ),
            (
                Ok(RemoteUpdateStatus::UpdateAvailable),
                CapabilityState::Supported,
                UpdateState::UpdateAvailable,
                UpdateState::UpdateAvailable,
            ),
            (
                Err(AdapterError::Failed("fixture failure".into())),
                CapabilityState::Supported,
                UpdateState::UpdateAvailable,
                UpdateState::CheckFailed,
            ),
            (
                Err(AdapterError::Unsupported),
                CapabilityState::Supported,
                UpdateState::UpdateAvailable,
                UpdateState::Unsupported,
            ),
            (
                Err(AdapterError::NotChecked),
                CapabilityState::Supported,
                UpdateState::UpdateAvailable,
                UpdateState::NotChecked,
            ),
        ];
        for (remote_result, capability, initial, expected) in cases {
            let adapter = remote_fixture(remote_result, capability);
            let state = persisted_update_state(initial);
            let result = compute_facets_with_adapter(
                &declaration(),
                Some(&state),
                StatusMode::Online,
                &FixtureProbeRunner::new([]),
                Some(&adapter),
            );
            assert_eq!(adapter.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(result.update_state, expected);
        }
    }

    #[test]
    fn online_reprobes_locally_before_remote_check() {
        let adapter = remote_fixture(Ok(RemoteUpdateStatus::Current), CapabilityState::Supported);
        let events = adapter.events.clone();

        let _ = compute_status_with_adapter(
            &declaration(),
            None,
            StatusMode::Online,
            &FixtureProbeRunner::new([]),
            Some(&adapter),
        );

        assert_eq!(*events.lock().unwrap(), vec!["local", "remote"]);
    }

    #[test]
    fn offline_and_local_are_stale_and_preserve_remote_timestamp() {
        let d = declaration();
        let mut facets =
            compute_facets(&d, None, StatusMode::Offline, &FixtureProbeRunner::new([]));
        facets.update_state = UpdateState::UpdateAvailable;
        facets.last_check = Some("2026-09-27T15:00:00Z".into());
        let state = StateRecord {
            committed_generation: None,
            artifact_fingerprint: None,
            source_evidence: serde_json::Value::Null,
            operation_id: None,
            facets: facets.clone(),
        };
        let offline = compute_status(
            &d,
            Some(&state),
            StatusMode::Offline,
            &FixtureProbeRunner::new([]),
        );
        assert_eq!(offline.update_freshness, UpdateFreshness::Stale);
        assert_eq!(offline.facets.update_state, UpdateState::UpdateAvailable);
        assert_eq!(offline.facets.last_check, facets.last_check);
        let local = compute_status(
            &d,
            Some(&state),
            StatusMode::Local,
            &FixtureProbeRunner::new([]),
        );
        assert_eq!(local.update_freshness, UpdateFreshness::Stale);
        assert_eq!(local.facets.update_state, UpdateState::UpdateAvailable);
        assert_eq!(local.facets.last_check, facets.last_check);
    }

    #[test]
    fn online_marks_completed_attempt_fresh_and_updates_timestamp() {
        let d = declaration();
        let state = persisted_update_state(UpdateState::UpdateAvailable);
        let adapter = remote_fixture(Ok(RemoteUpdateStatus::Current), CapabilityState::Supported);
        let result = compute_status_with_adapter(
            &d,
            Some(&state),
            StatusMode::Online,
            &FixtureProbeRunner::new([]),
            Some(&adapter),
        );
        assert_eq!(result.update_freshness, UpdateFreshness::Fresh);
        assert_eq!(result.facets.update_state, UpdateState::Current);
        assert!(result.facets.last_check.is_some());
    }

    #[test]
    fn online_failed_attempt_is_fresh_and_updates_timestamp() {
        let d = declaration();
        let previous_check = "2026-09-27T15:00:00Z";
        let mut state = persisted_update_state(UpdateState::UpdateAvailable);
        state.facets.last_check = Some(previous_check.into());
        let adapter = remote_fixture(
            Err(AdapterError::Failed("fixture failure".into())),
            CapabilityState::Supported,
        );

        let result = compute_status_with_adapter(
            &d,
            Some(&state),
            StatusMode::Online,
            &FixtureProbeRunner::new([]),
            Some(&adapter),
        );

        assert_eq!(result.update_freshness, UpdateFreshness::Fresh);
        assert_eq!(result.facets.update_state, UpdateState::CheckFailed);
        assert_ne!(result.facets.last_check.as_deref(), Some(previous_check));
        assert_eq!(
            result.remote_failure,
            Some(RemoteFailureDiagnostic::RemoteCheckFailed)
        );
    }

    #[test]
    fn online_failure_diagnostics_are_bounded_and_not_serialized() {
        let d = declaration();
        for (message, expected) in [
            (
                "interpreter pip probe timed out",
                RemoteFailureDiagnostic::TimedOut,
            ),
            (
                "interpreter pip probe output exceeded limit",
                RemoteFailureDiagnostic::OutputLimit,
            ),
            (
                "interpreter pip probe could not start",
                RemoteFailureDiagnostic::CouldNotStart,
            ),
            (
                "interpreter pip probe exited unsuccessfully (exit=Some(7))",
                RemoteFailureDiagnostic::UnsuccessfulExit,
            ),
            (
                "interpreter pip returned invalid JSON",
                RemoteFailureDiagnostic::InvalidResponse,
            ),
            (
                "secret https://user:password@example.test",
                RemoteFailureDiagnostic::RemoteCheckFailed,
            ),
        ] {
            let adapter = remote_fixture(
                Err(AdapterError::Failed(message.into())),
                CapabilityState::Supported,
            );
            let result = compute_status_with_adapter(
                &d,
                None,
                StatusMode::Online,
                &FixtureProbeRunner::new([]),
                Some(&adapter),
            );
            assert_eq!(result.remote_failure, Some(expected));
            let encoded = serde_json::to_string(&result).unwrap();
            assert!(!encoded.contains("remote_failure"));
            assert!(!encoded.contains("password"));
        }
    }
    #[test]
    fn online_without_adapter_returns_unsupported_update_state() {
        let runner = FixtureProbeRunner::new([]);
        let d = declaration();
        let state = persisted_update_state(UpdateState::UpdateAvailable);
        let result = compute_facets(&d, Some(&state), StatusMode::Online, &runner);
        assert_eq!(result.update_state, UpdateState::Unsupported);
    }
    #[test]
    fn next_action_rules_cover_supported_and_unsupported_paths() {
        let d = declaration();
        let mut f = compute_facets(&d, None, StatusMode::Offline, &FixtureProbeRunner::new([]));
        f.install_state = InstallState::Installed;
        assert_eq!(
            next_action(&d, &f, true, true, true).command.as_deref(),
            Some("ccync outdated")
        );
        f.install_state = InstallState::Missing;
        f.package_state = PackageState::ExternallyChanged;
        assert!(next_action(&d, &f, true, false, true).command.is_none());
        assert_eq!(
            next_action(&d, &f, true, true, true).command.as_deref(),
            Some("ccync tool refresh demo")
        );
        f.package_state = PackageState::NotChecked;
        f.update_state = UpdateState::UpdateAvailable;
        assert!(next_action(&d, &f, true, true, false).command.is_none());
        assert_eq!(
            next_action(&d, &f, true, true, true).command.as_deref(),
            Some("ccync upgrade tool:demo")
        );
        f.update_state = UpdateState::NotChecked;
        f.projection_state = ProjectionState::Drifted;
        assert_eq!(
            next_action(&d, &f, true, true, true).command.as_deref(),
            Some("ccync sync")
        );
        assert!(next_action(&d, &f, false, true, true).command.is_none());
    }
}
