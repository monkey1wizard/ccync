#![allow(dead_code)]

//! Read-only status report for tracked tools and git plugins.
use ccync_engine::{
    tool_manager::{
        self,
        adapters::{self, AdapterError, ToolAdapter},
        model::{PackageState, UpdateState},
        probe::{ProbeRunner, SystemProbeRunner},
        status::{self, RemoteFailureDiagnostic, StatusMode, UpdateFreshness},
        store,
    },
    ExitCode,
};

struct ToolRow {
    id: String,
    facets: tool_manager::model::ToolFacets,
    update_freshness: UpdateFreshness,
}

fn aggregate_update_state(outcome: &str) -> Option<&'static str> {
    match outcome {
        "current" => Some("current"),
        "unsupported" => Some("unsupported"),
        "not-checked" => Some("not-checked"),
        "update-available" => Some("update-available"),
        "actionable-drift" | "check-failed" => None,
        _ => None,
    }
}

pub(crate) fn cmd_outdated(args: &[String]) -> ExitCode {
    cmd_outdated_with_runner(args, &SystemProbeRunner)
}

pub(crate) fn cmd_outdated_with_runner(args: &[String], runner: &dyn ProbeRunner) -> ExitCode {
    cmd_outdated_with_adapters_and_runner(args, None, runner)
}

pub(crate) fn cmd_outdated_with_adapters_and_runner(
    args: &[String],
    custom_adapters: Option<&[&dyn ToolAdapter]>,
    runner: &dyn ProbeRunner,
) -> ExitCode {
    let mut offline = false;
    let mut json = false;
    for arg in args.iter().skip(1) {
        match arg.as_str() {
            "--offline" if !offline => offline = true,
            "--json" if !json => json = true,
            _ => {
                eprintln!("Usage: ccync outdated [--offline] [--json]");
                return ExitCode::Usage;
            }
        }
    }
    let declarations = match store::load_declarations() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("ccync outdated: {e}");
            return ExitCode::Error;
        }
    };
    let states = match store::load_state() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("ccync outdated: {e}");
            return ExitCode::Error;
        }
    };
    let mut rows = Vec::new();
    let mut check_failed = false;
    for (id, declaration) in declarations {
        let state = states.get(&id);
        let (recorded_adapter, adapter_error) = match custom_adapters {
            Some(_) => (None, None),
            None => match adapters::for_tracked_declaration(&declaration) {
                Ok((adapter, _, _)) => (Some(adapter), None),
                Err(error) => (None, Some(error)),
            },
        };
        let adapter = match custom_adapters {
            Some(custom) => custom
                .iter()
                .copied()
                .find(|a| a.kind() == declaration.identity.adapter_kind),
            None => recorded_adapter.as_deref(),
        };
        let result = status::compute_status_with_adapter(
            &declaration,
            state,
            if offline { StatusMode::Offline } else { StatusMode::Online },
            runner,
            adapter,
        );
        let mut facets = result.facets;
        let mut remote_failure = result.remote_failure;
        if !offline {
            if let Some(error @ AdapterError::Failed(_)) = adapter_error {
                facets.update_state = UpdateState::CheckFailed;
                remote_failure = Some(RemoteFailureDiagnostic::from_adapter_error(&error));
            }
            if facets.update_state == UpdateState::CheckFailed {
                check_failed = true;
                eprintln!(
                    "ccync outdated {id}: {}",
                    remote_failure.unwrap_or_default().as_str()
                );
            }
        }
        rows.push(ToolRow {
            id: id.to_string(),
            facets,
            update_freshness: result.update_freshness,
        });
    }
    let plugins = if offline {
        None
    } else {
        Some(super::upgrade::upgrade_available_report())
    };
    if plugins.as_ref().is_some_and(|r| r.error.is_some()) {
        check_failed = true;
        if let Some(error) = plugins.as_ref().and_then(|r| r.error.as_ref()) {
            eprintln!("ccync outdated plugins: {error}");
        }
    }
    let action_known = rows.iter().any(|r| {
        matches!(
            r.facets.package_state,
            PackageState::ExternallyChanged | PackageState::Missing
        ) || r.facets.update_state == UpdateState::UpdateAvailable
            || matches!(
                r.facets.artifact_state,
                tool_manager::model::ArtifactState::ArtifactDrifted
                    | tool_manager::model::ArtifactState::Missing
            )
            || r.facets.projection_state == tool_manager::model::ProjectionState::Drifted
    }) || plugins.as_ref().is_some_and(|r| !r.pending.is_empty());
    let outcome = if check_failed {
        "check-failed"
    } else if rows.iter().any(|r| {
        matches!(
            r.facets.package_state,
            PackageState::ExternallyChanged | PackageState::Missing
        ) || matches!(
            r.facets.artifact_state,
            tool_manager::model::ArtifactState::ArtifactDrifted
                | tool_manager::model::ArtifactState::Missing
        ) || r.facets.projection_state == tool_manager::model::ProjectionState::Drifted
    }) {
        "actionable-drift"
    } else if rows
        .iter()
        .any(|r| r.facets.update_state == UpdateState::UpdateAvailable)
        || plugins.as_ref().is_some_and(|r| !r.pending.is_empty())
    {
        "update-available"
    } else if !offline
        && rows
            .iter()
            .all(|r| r.facets.update_state == UpdateState::Unsupported)
    {
        "unsupported"
    } else if offline
        || rows
            .iter()
            .any(|r| r.facets.update_state == UpdateState::NotChecked)
    {
        "not-checked"
    } else {
        "current"
    };
    if json {
        let tool_values = rows
            .iter()
            .map(|r| {
                let update_state = serde_json::to_value(r.facets.update_state).unwrap();
                let freshness = serde_json::to_value(r.update_freshness).unwrap();
                serde_json::json!({"id":r.id,"outcome":update_state,"facets":r.facets,"updateFreshness":freshness})
            })
            .collect::<Vec<_>>();
        let plugin_value = if offline {
            serde_json::json!({"availability":"not-checked"})
        } else {
            let r = plugins.as_ref().expect("online report");
            serde_json::json!({"availability":"checked","updatesAvailable":r.pending.iter().map(|p| serde_json::json!({"pluginId":p.plugin_id,"source":p.source,"oldSha":p.old_sha,"newSha":p.new_sha})).collect::<Vec<_>>(),"error":r.error})
        };
        println!(
            "{}",
            serde_json::json!({
                "schemaVersion": 1,
                "command": "outdated",
                "outcome": outcome,
                "selectionState": null,
                "updateState": aggregate_update_state(outcome),
                "updateFreshness": if offline { "stale" } else { "fresh" },
                "nextAction": null,
                "affectedAgents": [],
                "projectionChanged": false,
                "reloadAdvice": null,
                "tools": tool_values,
                "plugins": plugin_value
            })
        );
    } else {
        let render = || {
            println!("outcome: {outcome}");
            for row in &rows {
                println!(
                    "{}: package={:?}, update={:?}, freshness={:?}",
                    row.id, row.facets.package_state, row.facets.update_state, row.update_freshness
                );
            }
            match plugins.as_ref() {
                Some(r) => println!("plugins: {} update(s) available", r.pending.len()),
                None => println!("plugins: not-checked (offline)"),
            }
        };
        render();
    }
    if check_failed {
        ExitCode::Error
    } else if rows.iter().any(|r| {
            r.facets.operation_state == tool_manager::model::OperationState::RecoveryRequired
        })
        || action_known
    {
        ExitCode::UpdatesAvailable
    } else {
        ExitCode::Success
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccync_engine::tool_manager::{
        adapters::{AdapterCapabilities, CapabilityState, RemoteUpdateStatus},
        discovery::{DiscoveryCandidate, DiscoveryQuery},
        model::{
            AdapterKind, ArtifactState, DeliveryId, EvidenceLevel, HoldPolicy,
            InstallationIdentity, ManagerAvailability, ManagerPinState, OperationState,
            PackageState, ProjectionState, SelectionState, ToolDeclaration, ToolFacets, ToolId,
            UpdateState,
        },
        store::StateRecord,
    };
    use std::{collections::BTreeMap, fs, path::Path};

    #[test]
    fn aggregate_update_state_is_null_for_non_update_outcomes() {
        assert_eq!(aggregate_update_state("current"), Some("current"));
        assert_eq!(aggregate_update_state("unsupported"), Some("unsupported"));
        assert_eq!(aggregate_update_state("not-checked"), Some("not-checked"));
        assert_eq!(
            aggregate_update_state("update-available"),
            Some("update-available")
        );
        assert_eq!(aggregate_update_state("actionable-drift"), None);
        assert_eq!(aggregate_update_state("check-failed"), None);
    }

    #[derive(Default)]
    struct NoRequests(std::sync::Mutex<usize>);
    impl ProbeRunner for NoRequests {
        fn run(&self, _: &tool_manager::probe::ProbeRequest) -> tool_manager::probe::ProbeOutput {
            *self.0.lock().unwrap() += 1;
            Default::default()
        }
    }

    struct FakeAdapter {
        kind: AdapterKind,
        candidate: Option<DiscoveryCandidate>,
        remote_status_result: Result<RemoteUpdateStatus, AdapterError>,
    }

    impl ToolAdapter for FakeAdapter {
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
        fn discover(&self, _query: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
            Ok(self.candidate.clone().into_iter().collect())
        }
        fn reprobe(&self, _query: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
            Ok(self.candidate.clone().into_iter().collect())
        }
        fn remote_update_status(&self, _query: &DiscoveryQuery) -> Result<RemoteUpdateStatus, AdapterError> {
            self.remote_status_result.clone()
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
    fn outdated_rejects_unknown_and_duplicate_flags_with_usage_code() {
        let _guard = crate::test_io_lock();
        assert_eq!(
            cmd_outdated_with_runner(&["outdated".into(), "--bad".into()], &SystemProbeRunner),
            ExitCode::Usage
        );
        assert_eq!(
            cmd_outdated_with_runner(
                &["outdated".into(), "--offline".into(), "--offline".into()],
                &SystemProbeRunner
            ),
            ExitCode::Usage
        );
        assert_eq!(
            cmd_outdated_with_runner(
                &["outdated".into(), "--json".into(), "--json".into()],
                &SystemProbeRunner
            ),
            ExitCode::Usage
        );
    }

    #[test]
    fn outdated_offline_marks_plugins_not_checked_and_makes_no_requests() {
        let _guard = crate::test_io_lock();
        let dir = tempfile::tempdir().unwrap();
        let saved = redirect_home(dir.path());
        let fixture = NoRequests::default();

        let tool_id = ToolId::new("mytool").unwrap();
        let mut fields = BTreeMap::new();
        fields.insert("executable".into(), "dummy.exe".into());
        let decl = ToolDeclaration {
            id: tool_id.clone(),
            identity: InstallationIdentity {
                adapter_kind: AdapterKind::Cargo,
                fields,
            },
            delivery_id: DeliveryId::ObserveOnly,
            hold_policy: HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        };
        store::replace_declarations(&BTreeMap::from([(tool_id, decl)])).unwrap();

        assert_eq!(
            cmd_outdated_with_runner(
                &["outdated".into(), "--offline".into(), "--json".into()],
                &fixture
            ),
            ExitCode::Success
        );
        assert_eq!(*fixture.0.lock().unwrap(), 0);
        restore_home(saved);
    }

    #[test]
    fn outdated_clean_state_exits_success() {
        let _guard = crate::test_io_lock();
        let dir = tempfile::tempdir().unwrap();
        let saved = redirect_home(dir.path());
        let fixture = NoRequests::default();

        assert_eq!(
            cmd_outdated_with_runner(&["outdated".into(), "--offline".into()], &fixture),
            ExitCode::Success
        );
        restore_home(saved);
    }

    #[test]
    fn outdated_detects_externally_changed_tool_and_exits_updates_available() {
        let _guard = crate::test_io_lock();
        let dir = tempfile::tempdir().unwrap();
        let saved = redirect_home(dir.path());
        let fixture = NoRequests::default();

        let tool_dir = dir.path().join("bin");
        fs::create_dir_all(&tool_dir).unwrap();
        let exe_path = tool_dir.join("demo_tool.exe");
        fs::write(&exe_path, b"version 2 binary content").unwrap();

        let tool_id = ToolId::new("demo-tool").unwrap();
        let mut fields = BTreeMap::new();
        fields.insert(
            "executable".into(),
            exe_path.to_string_lossy().into_owned(),
        );
        let identity = InstallationIdentity {
            adapter_kind: AdapterKind::Cargo,
            fields: fields.clone(),
        };
        let decl = ToolDeclaration {
            id: tool_id.clone(),
            identity: identity.clone(),
            delivery_id: DeliveryId::ObserveOnly,
            hold_policy: HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        };
        store::replace_declarations(&BTreeMap::from([(tool_id.clone(), decl)])).unwrap();

        let state = StateRecord {
            committed_generation: Some(1),
            artifact_fingerprint: None,
            source_evidence: serde_json::json!({
                "adapterFields": identity,
                "resolvedTarget": exe_path.to_string_lossy().into_owned(),
                "fileIdentity": {"dev": 0, "ino": 0},
                "sha256": "different_hash_from_v1",
            }),
            operation_id: None,
            facets: default_test_facets(),
        };
        store::commit_state(&BTreeMap::from([(tool_id, state)])).unwrap();

        let candidate = DiscoveryCandidate::new(
            identity,
            EvidenceLevel::Verified,
            true,
            false,
            true,
        );
        let fake_adapter = FakeAdapter {
            kind: AdapterKind::Cargo,
            candidate: Some(candidate),
            remote_status_result: Ok(RemoteUpdateStatus::Current),
        };

        let code = cmd_outdated_with_adapters_and_runner(
            &["outdated".into()],
            Some(&[&fake_adapter]),
            &fixture,
        );
        assert_eq!(code, ExitCode::UpdatesAvailable);
        restore_home(saved);
    }

    #[test]
    fn outdated_detects_update_available_tool_and_exits_updates_available() {
        let _guard = crate::test_io_lock();
        let dir = tempfile::tempdir().unwrap();
        let saved = redirect_home(dir.path());
        let fixture = NoRequests::default();

        let tool_id = ToolId::new("updatable-tool").unwrap();
        let mut fields = BTreeMap::new();
        fields.insert("executable".into(), "dummy.exe".into());
        let decl = ToolDeclaration {
            id: tool_id.clone(),
            identity: InstallationIdentity {
                adapter_kind: AdapterKind::Cargo,
                fields,
            },
            delivery_id: DeliveryId::ObserveOnly,
            hold_policy: HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        };
        store::replace_declarations(&BTreeMap::from([(tool_id.clone(), decl)])).unwrap();

        let mut facets = default_test_facets();
        facets.update_state = UpdateState::UpdateAvailable;
        let state = StateRecord {
            committed_generation: Some(1),
            artifact_fingerprint: None,
            source_evidence: serde_json::Value::Null,
            operation_id: None,
            facets,
        };
        store::commit_state(&BTreeMap::from([(tool_id, state)])).unwrap();

        let code = cmd_outdated_with_runner(
            &["outdated".into(), "--offline".into()],
            &fixture,
        );
        assert_eq!(code, ExitCode::UpdatesAvailable);
        restore_home(saved);
    }

    #[test]
    fn outdated_check_failed_exits_error() {
        let _guard = crate::test_io_lock();
        let dir = tempfile::tempdir().unwrap();
        let saved = redirect_home(dir.path());
        let fixture = NoRequests::default();

        let tool_id = ToolId::new("failing-tool").unwrap();
        let mut fields = BTreeMap::new();
        fields.insert("executable".into(), "dummy.exe".into());
        let decl = ToolDeclaration {
            id: tool_id.clone(),
            identity: InstallationIdentity {
                adapter_kind: AdapterKind::NpmGlobal,
                fields,
            },
            delivery_id: DeliveryId::ObserveOnly,
            hold_policy: HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        };
        store::replace_declarations(&BTreeMap::from([(tool_id, decl)])).unwrap();

        let fake_adapter = FakeAdapter {
            kind: AdapterKind::NpmGlobal,
            candidate: None,
            remote_status_result: Err(AdapterError::Failed("registry unreachable".into())),
        };

        let code = cmd_outdated_with_adapters_and_runner(
            &["outdated".into()],
            Some(&[&fake_adapter]),
            &fixture,
        );
        assert_eq!(code, ExitCode::Error);
        restore_home(saved);
    }

    #[test]
    fn outdated_recovery_required_and_drift_exits_updates_available() {
        let _guard = crate::test_io_lock();
        let dir = tempfile::tempdir().unwrap();
        let saved = redirect_home(dir.path());
        let fixture = NoRequests::default();

        let tool_id = ToolId::new("recovery-tool").unwrap();
        let mut fields = BTreeMap::new();
        fields.insert("executable".into(), "dummy.exe".into());
        let decl = ToolDeclaration {
            id: tool_id.clone(),
            identity: InstallationIdentity {
                adapter_kind: AdapterKind::Cargo,
                fields,
            },
            delivery_id: DeliveryId::ObserveOnly,
            hold_policy: HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        };
        store::replace_declarations(&BTreeMap::from([(tool_id.clone(), decl)])).unwrap();

        let mut facets = default_test_facets();
        facets.operation_state = OperationState::RecoveryRequired;
        let state = StateRecord {
            committed_generation: Some(1),
            artifact_fingerprint: None,
            source_evidence: serde_json::Value::Null,
            operation_id: None,
            facets,
        };
        store::commit_state(&BTreeMap::from([(tool_id, state)])).unwrap();

        let code = cmd_outdated_with_runner(
            &["outdated".into(), "--offline".into()],
            &fixture,
        );
        assert_eq!(code, ExitCode::UpdatesAvailable);
        restore_home(saved);
    }
}

