use ccync_engine::ExitCode;
use std::{fs, path::Path};

struct EnvHomeGuard {
    saved: Vec<(String, Option<String>)>,
}

fn redirect_home(path: &Path) -> EnvHomeGuard {
    let saved = [
        "USERPROFILE",
        "HOME",
        "APPDATA",
        "LOCALAPPDATA",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ]
    .into_iter()
    .map(|key| {
        let old = std::env::var(key).ok();
        let value = match key {
            "APPDATA" | "XDG_CONFIG_HOME" => path.join("config"),
            "LOCALAPPDATA" | "XDG_STATE_HOME" => path.join("state"),
            "XDG_CACHE_HOME" => path.join("cache"),
            _ => path.to_path_buf(),
        };
        unsafe { std::env::set_var(key, value) };
        (key.to_owned(), old)
    })
    .collect();
    EnvHomeGuard { saved }
}

impl Drop for EnvHomeGuard {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            unsafe {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
        }
    }
}

#[test]
fn portable_backup_uses_committed_sources_and_excludes_private_catalog_fields() {
    let _guard = crate::test_io_lock();
    let home = tempfile::tempdir().unwrap();
    let _env = redirect_home(home.path());
    let resolved = ccync_foundation::paths::StorageRoots::resolve().unwrap();
    let roots = [
        ("portable", resolved.portable),
        ("config", resolved.config),
        ("state", resolved.state),
        ("cache", resolved.cache),
    ];
    let commit = ccync_foundation::provider::StorageLayoutCommit {
        layout_version: 1,
        roots: roots
            .iter()
            .map(
                |(name, path)| ccync_foundation::provider::LayoutRootIdentity {
                    name: (*name).to_string(),
                    path: path.clone(),
                },
            )
            .collect(),
    };
    let commit = serde_json::to_vec(&commit).unwrap();
    let marker = home.path().join(".ccync/migration.json");
    let state_layout = roots[2].1.join("layout.json");
    fs::create_dir_all(marker.parent().unwrap()).unwrap();
    fs::create_dir_all(state_layout.parent().unwrap()).unwrap();
    fs::write(marker, &commit).unwrap();
    fs::write(state_layout, &commit).unwrap();

    let catalog = serde_json::json!({"schemaVersion":1,"plugins":[{
        "pluginId":"portable-demo",
        "source":"https://example.invalid/demo.git",
        "installStrategy":"personal-git-clone",
        "pin":"abc123",
        "privatePath":"private-catalog-value"
    }]});
    let private_lock = serde_json::json!({
        "schemaVersion":1,
        "generatedAt":"2026-10-28T00:00:00Z",
        "resolverVersion":"1",
        "profileName":"default",
        "resolvedPlugins":[],
        "_personalPlugins":["private-lock-value"],
        "_ccyncProjection":{"private":"projection-value"}
    });
    let config_path = roots[1].1.join("config.json");
    let catalog_path = roots[1].1.join("plugins.json");
    let agents_path = roots[1].1.join("AGENTS.md");
    let mcp_path = roots[1].1.join("mcp.json");
    let private_lock_path = roots[2].1.join("build/lock.json");
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    fs::create_dir_all(private_lock_path.parent().unwrap()).unwrap();
    fs::write(
        &config_path,
        b"{\n  \"selectedProjectionTargets\": [\n    \"claude\"\n  ]\n}",
    )
    .unwrap();
    fs::write(&catalog_path, serde_json::to_vec(&catalog).unwrap()).unwrap();
    fs::write(&agents_path, b"# Committed rules\n").unwrap();
    fs::write(&mcp_path, br#"{"mcpServers":{}}"#).unwrap();
    fs::write(
        &private_lock_path,
        serde_json::to_vec_pretty(&private_lock).unwrap(),
    )
    .unwrap();
    fs::create_dir_all(&roots[0].1).unwrap();
    fs::write(roots[0].1.join("sources.json"), b"stale-private-source").unwrap();
    fs::write(roots[0].1.join("lock.json"), b"stale-private-lock").unwrap();

    let backup = home.path().join("backup");
    assert_eq!(
        super::lifecycle::cmd_backup(&[
            "backup".into(),
            "--output".into(),
            backup.display().to_string()
        ]),
        ExitCode::Success
    );
    for (name, source) in [
        ("config.json", config_path),
        ("plugins.json", catalog_path.clone()),
        ("plugins.lock.json", private_lock_path.clone()),
        ("AGENTS.md", agents_path),
        ("mcp.json", mcp_path),
    ] {
        assert_eq!(
            fs::read(backup.join(name)).unwrap(),
            fs::read(source).unwrap()
        );
    }
    assert_eq!(
        fs::read(backup.join("portable/sources.json")).unwrap(),
        ccync_engine::catalog::portable_sources_export_bytes(&catalog).unwrap()
    );
    assert_eq!(
        fs::read(backup.join("portable/lock.json")).unwrap(),
        ccync_engine::catalog::portable_lock_export_bytes(&private_lock).unwrap()
    );
    let portable_text = fs::read_to_string(backup.join("portable/sources.json")).unwrap()
        + &fs::read_to_string(backup.join("portable/lock.json")).unwrap();
    for private_value in [
        "private-catalog-value",
        "private-lock-value",
        "projection-value",
        "stale-private-source",
        "stale-private-lock",
    ] {
        assert!(!portable_text.contains(private_value));
    }
}

#[test]
fn portable_backup_legacy_preserves_the_five_file_payload() {
    let _guard = crate::test_io_lock();
    let home = tempfile::tempdir().unwrap();
    let _env = redirect_home(home.path());
    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    let files = [
        (
            "config.json",
            ccync.join("config.json"),
            b"{\n  \"selectedProjectionTargets\": [\n    \"claude\"\n  ]\n}".as_slice(),
        ),
        (
            "plugins.json",
            ccync.join("plugins.json"),
            br#"{"plugins":[]}"#.as_slice(),
        ),
        (
            "plugins.lock.json",
            ccync.join("build/lock.json"),
            b"{\n  \"schemaVersion\": 1\n}".as_slice(),
        ),
        (
            "AGENTS.md",
            ccync.join("AGENTS.md"),
            b"# Legacy agents\n".as_slice(),
        ),
        (
            "mcp.json",
            ccync.join("mcp.json"),
            br#"{"mcpServers":{}}"#.as_slice(),
        ),
    ];
    for (_, source, bytes) in &files {
        fs::write(source, bytes).unwrap();
    }

    let backup = home.path().join("backup");
    assert_eq!(
        super::lifecycle::cmd_backup(&[
            "backup".into(),
            "--output".into(),
            backup.display().to_string()
        ]),
        ExitCode::Success
    );
    for (name, _, expected) in files {
        assert_eq!(fs::read(backup.join(name)).unwrap(), expected);
    }
    assert!(!backup.join("portable").exists());
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
    let _env = redirect_home(home.path());
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
}

#[test]
fn snapshot_preflight_rejects_drift_and_accepts_matching_fingerprint() {
    let _guard = crate::test_io_lock();
    let home = tempfile::tempdir().unwrap();
    let _env = redirect_home(home.path());
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
    let host_config = home.path().join(".claude.json");
    fs::write(&host_config, br#"{"mcpServers":{}}"#).unwrap();
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
}

#[test]
fn run_projection_with_report_matches_core_exit_and_reports_canonical_targets() {
    let _guard = crate::test_io_lock();
    let home = tempfile::tempdir().unwrap();
    let _env = redirect_home(home.path());
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
}
