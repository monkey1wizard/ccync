use ccync_engine::ExitCode;
use std::{fs, path::Path};

fn redirect_home(path: &Path) -> Vec<(String, Option<String>)> {
    [
        "USERPROFILE",
        "HOME",
        "APPDATA",
        "LOCALAPPDATA",
        "XDG_CONFIG_HOME",
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

fn seed_tool_state(home: &Path) {
    let ccync = home.join(".ccync");
    let tools = ccync.join("build/tools");
    fs::create_dir_all(&tools).unwrap();
    fs::write(ccync.join("config.json"), "{}").unwrap();
    fs::create_dir_all(ccync.join("install-receipts")).unwrap();
    fs::write(ccync.join("tools.json"), r#"{"version":1,"records":{"tracked-tool":{"id":"tracked-tool","identity":{"adapterKind":"manual-path","fields":{}},"deliveryId":"observe-only","holdPolicy":"unheld"}}}"#).unwrap();
    fs::write(tools.join("state.json"), r#"{"version":1,"records":{}}"#).unwrap();
    fs::write(
        tools.join("operation.txn.json"),
        r#"{"receipt":"tool-journal-secret"}"#,
    )
    .unwrap();
    fs::write(
        ccync.join("install-receipts/ccync.json"),
        r#"{"receipt":"tool-install-receipt-secret"}"#,
    )
    .unwrap();
    fs::create_dir_all(tools.join("artifacts/tracked-tool/1")).unwrap();
    fs::write(
        tools.join("artifacts/tracked-tool/1/payload"),
        "tool-snapshot-secret",
    )
    .unwrap();
}

#[test]
fn lifecycle_backup_tool_excludes_all_tool_state_and_roster_entries() {
    let _guard = crate::test_io_lock();
    let home = tempfile::tempdir().unwrap();
    let saved = redirect_home(home.path());
    seed_tool_state(home.path());

    let backup = home.path().join("backup");
    assert_eq!(
        super::lifecycle::cmd_backup(&[
            "backup".into(),
            "--output".into(),
            backup.display().to_string()
        ]),
        ExitCode::Success
    );
    let backup_entries = fs::read_dir(&backup)
        .unwrap()
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    let backup_names = backup_entries
        .iter()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    for excluded in [
        "tools.json",
        "state.json",
        "artifacts",
        "operation.txn.json",
        "ccync.json",
    ] {
        assert!(
            !backup_names.iter().any(|name| name == excluded),
            "backup included {excluded}"
        );
    }
    let backup_text = backup_entries
        .into_iter()
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .collect::<Vec<_>>()
        .join("\n");
    for secret in [
        "tracked-tool",
        "tool-snapshot-secret",
        "tool-journal-secret",
        "tool-install-receipt-secret",
    ] {
        assert!(!backup_text.contains(secret), "backup leaked {secret}");
    }

    let roster = home.path().join("roster.json");
    assert_eq!(
        super::roster::cmd_roster(&[
            "roster".into(),
            "dump".into(),
            "--out".into(),
            roster.display().to_string()
        ]),
        ExitCode::Success
    );
    let roster_text = fs::read_to_string(roster).unwrap();
    assert!(!roster_text.contains("tracked-tool"));

    restore_home(saved);
}

#[test]
fn snapshot_preflight_rejects_drift_and_accepts_matching_fingerprint() {
    let _guard = crate::test_io_lock();
    let home = tempfile::tempdir().unwrap();
    let saved = redirect_home(home.path());
    let artifacts = home.path().join(".ccync/build/tools/artifacts/tool/1");
    fs::create_dir_all(&artifacts).unwrap();
    fs::write(artifacts.join("payload"), b"committed").unwrap();
    let manifest = ccync_engine::tool_manager::artifact_tree::scan_tree(&artifacts).unwrap();
    fs::create_dir_all(home.path().join(".ccync/build/tools")).unwrap();
    fs::write(home.path().join(".ccync/tools.json"), r#"{"version":1,"records":{"tool":{"id":"tool","identity":{"adapterKind":"interpreter-pip","fields":{"distribution":"graphifyy"}},"deliveryId":"graphify-agents","holdPolicy":"unheld"}}}"#).unwrap();
    let state = serde_json::json!({"version":1,"records":{"tool":{
        "committedGeneration":1,"artifactFingerprint":format!("sha256:{}", manifest.sha256),
        "sourceEvidence":{},"operationId":"op",
        "facets":{"evidenceLevel":"verified","selectionState":"unique","managerAvailability":"available","installState":"installed","packageState":"matches-artifact-source","updateState":"not-checked","artifactState":"current","projectionState":"current","holdPolicy":"unheld","managerPinState":"unknown","operationState":"committed","lastCheck":null}
    }}});
    let state_path = home.path().join(".ccync/build/tools/state.json");
    fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    assert!(super::lifecycle::verify_committed_tool_snapshots()
        .unwrap()
        .is_ok());
    let ccync = home.path().join(".ccync");
    fs::create_dir_all(&ccync).unwrap();
    fs::write(ccync.join("config.json"), "{}").unwrap();
    let lock = ccync.join("build/lock.json");
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    fs::write(&lock, "valid-snapshot-seed").unwrap();
    let _ = super::lifecycle::run_unified_projection(
        false,
        true,
        false,
        Some(&super::lifecycle::SYNC_GATE_ABORT_CONTEXT),
    );
    assert_ne!(
        fs::read(&lock).unwrap(),
        b"valid-snapshot-seed",
        "valid snapshot allows sync to reach catalog/render work"
    );
    fs::write(&lock, "preflight-sentinel").unwrap();
    let host_config = home.path().join(".claude.json");
    let host_before = fs::read(&host_config).unwrap();
    let lock_before = fs::read(&lock).unwrap();
    fs::write(artifacts.join("payload"), b"drifted").unwrap();
    let findings = super::lifecycle::verify_committed_tool_snapshots()
        .unwrap()
        .unwrap_err();
    assert_eq!(findings, vec![("tool".to_owned(), true)]);
    assert_eq!(
        super::lifecycle::tool_snapshot_recovery_command("tool", true),
        "ccync tool refresh tool"
    );
    assert_eq!(
        super::lifecycle::tool_snapshot_recovery_command("tool", false),
        "ccync tool untrack tool"
    );
    let dry_run_result = super::lifecycle::run_unified_projection(
        true,
        true,
        false,
        Some(&super::lifecycle::SYNC_GATE_ABORT_CONTEXT),
    );
    assert_eq!(dry_run_result, ExitCode::Error);
    assert_eq!(
        fs::read(&host_config).unwrap(),
        host_before,
        "drift abort leaves live host files unchanged under dry-run"
    );
    assert_eq!(
        fs::read(&lock).unwrap(),
        lock_before,
        "drift abort precedes catalog writes under dry-run"
    );

    let result = super::lifecycle::run_unified_projection(
        false,
        true,
        false,
        Some(&super::lifecycle::SYNC_GATE_ABORT_CONTEXT),
    );
    assert_eq!(result, ExitCode::Error);
    assert_eq!(
        fs::read(&host_config).unwrap(),
        host_before,
        "drift abort leaves live host files unchanged"
    );
    assert_eq!(
        fs::read(&lock).unwrap(),
        lock_before,
        "drift abort precedes catalog writes"
    );
    restore_home(saved);
}

#[test]
fn run_projection_with_report_matches_core_exit_and_reports_canonical_targets() {
    let _guard = crate::test_io_lock();
    let home = tempfile::tempdir().unwrap();
    let saved = redirect_home(home.path());
    let config = ccync_foundation::config::CcyncConfig::load();
    let core_code = super::lifecycle::run_projection_core(&config, "test projection");
    let (reported_code, report) =
        super::lifecycle::run_projection_with_report(&config, "test projection");
    assert_eq!(reported_code, core_code);
    assert_eq!(report.mode, "normal");
    assert!(
        !report.targets.is_empty(),
        "selected targets appear in finished report"
    );
    assert!(
        report
            .targets
            .contains_key(&ccync_foundation::target::ProjectionTargetId::Claude),
        "canonical phase target evidence is present in the report"
    );
    restore_home(saved);
}
