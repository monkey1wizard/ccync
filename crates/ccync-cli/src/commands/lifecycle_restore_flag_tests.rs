//! Acceptance probes for `ccync restore --dry-run`.
//!
//! Seam: `ccync restore --from <dir> --dry-run` in `cmd_restore`
//! within `crates/ccync-cli/src/commands/lifecycle.rs`.
//!
//! Restore dry-run acceptance contract:
//! - Takes the boolean-flag slot of `reject_unknown_flags`, which is `&[]` today.
//! - The branch must be fully short-circuited and must never enter `cmd_restore`'s
//!   per-file copy loop.
//! - It never writes on any path.
//! - Exit codes mirror the roster contract:
//!   - `0` (`ExitCode::Success`): converged.
//!   - `3` (`ExitCode::UpdatesAvailable`): drift.
//!   - `1` (`ExitCode::Error`): unreadable profile directory.
//! - A readable but empty profile directory against an initialized machine is `3`.
//! - Absent-on-both-sides (e.g. `AGENTS.md`) is converged (`0`).

use super::lifecycle::cmd_restore;
use ccync_engine::ExitCode;
use std::fs;
use tempfile::TempDir;

fn snapshot_tree(root: &std::path::Path) -> Vec<(String, String)> {
    fn visit(root: &std::path::Path, path: &std::path::Path, rows: &mut Vec<(String, String)>) {
        let mut entries = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        entries.sort();
        for entry in entries {
            let relative = entry
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let metadata = fs::symlink_metadata(&entry).unwrap();
            if metadata.file_type().is_symlink() {
                rows.push((
                    relative,
                    format!("link:{:?}", fs::read_link(&entry).unwrap()),
                ));
            } else if metadata.is_dir() {
                rows.push((relative, "directory".into()));
                visit(root, &entry, rows);
            } else {
                rows.push((relative, format!("file:{:?}", fs::read(&entry).unwrap())));
            }
        }
    }

    let mut rows = Vec::new();
    visit(root, root, &mut rows);
    rows
}

struct EnvHomeGuard {
    saved: Vec<(String, Option<String>)>,
}

impl EnvHomeGuard {
    fn set(tmp: &std::path::Path) -> Self {
        let keys = [
            "USERPROFILE",
            "HOME",
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
        ];
        let mut saved = Vec::new();
        for key in keys {
            saved.push((key.to_string(), std::env::var(key).ok()));
            unsafe {
                std::env::set_var(key, tmp);
            }
        }
        Self { saved }
    }
}

#[test]
fn portable_restore_case_folded_collisions_are_rejected() {
    assert!(super::lifecycle::restore_names_have_case_collision(&[
        "plugins.json",
        "PLUGINS.JSON"
    ]));
    assert!(!super::lifecycle::restore_names_have_case_collision(&[
        "plugins.json",
        "config.json"
    ]));
}

#[test]
fn pending_restore_blocks_backup_and_restore_preview() {
    use ccync_engine::tool_manager::lock::LifecycleLock;

    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let backup = TempDir::new().unwrap();
    let archive = backup.path().join("plugins.json");
    fs::write(&archive, br#"{"plugins":[]}"#).unwrap();
    let guard = LifecycleLock::acquire_for_restore(&archive, &["plugins.json"]).unwrap();
    guard
        .record_marker(&archive, &[("plugins.json", None, Some(b"after"))])
        .unwrap();
    drop(guard);

    let output = home.path().join("backup-output");
    let backup_args = vec![
        "backup".into(),
        "--output".into(),
        output.to_string_lossy().into_owned(),
    ];
    assert_eq!(super::lifecycle::cmd_backup(&backup_args), ExitCode::Error);
    assert!(!output.exists());
    let restore_args = vec![
        "restore".into(),
        "--from".into(),
        backup.path().to_string_lossy().into_owned(),
        "--dry-run".into(),
    ];
    assert_eq!(cmd_restore(&restore_args), ExitCode::Error);
}

#[test]
fn restore_mcp_conflicts_refuse_before_writing() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let backup = TempDir::new().unwrap();
    fs::create_dir_all(home.path().join(".agents")).unwrap();
    let personal = home.path().join(".agents/mcp.json");
    let original = br#"{"ccyncMcpServers":{"one":{"command":"node","hosts":["claude"]}}}"#;
    fs::write(&personal, original).unwrap();
    fs::write(backup.path().join("plugins.json"), br#"{"plugins":[]}"#).unwrap();
    fs::write(
        backup.path().join("mcp.json"),
        br#"{"ccyncMcpServers":{"one":{"command":"node","hosts":["codex"]}}}"#,
    )
    .unwrap();
    let args = vec![
        "restore".into(),
        "--from".into(),
        backup.path().to_string_lossy().into_owned(),
    ];
    assert_eq!(cmd_restore(&args), ExitCode::Error);
    assert_eq!(fs::read(&personal).unwrap(), original);
    assert!(!home.path().join(".ccync/plugins.json").exists());
    assert!(matches!(
        ccync_engine::tool_manager::lock::read_restore_marker().unwrap(),
        ccync_engine::tool_manager::lock::RestoreMarkerState::Absent
    ));
}

#[test]
fn restore_without_mcp_does_not_read_personal_candidate() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let backup = TempDir::new().unwrap();
    fs::create_dir_all(home.path().join(".agents/mcp.json")).unwrap();
    fs::write(backup.path().join("plugins.json"), br#"{"plugins":[]}"#).unwrap();
    let args = vec![
        "restore".into(),
        "--from".into(),
        backup.path().to_string_lossy().into_owned(),
    ];
    assert_eq!(cmd_restore(&args), ExitCode::Success);
    assert!(home.path().join(".agents/mcp.json").is_dir());
    assert!(home.path().join(".ccync/plugins.json").is_file());
}

#[test]
fn restore_skips_foreign_list_without_mcp_but_refuses_active_legacy_input() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let backup = TempDir::new().unwrap();
    fs::write(backup.path().join("plugins.json"), br#"{"plugins":[]}"#).unwrap();
    let personal = home.path().join(".agents/mcp.json");
    fs::create_dir_all(personal.parent().unwrap()).unwrap();
    let foreign = br#"{"mcpServers":{"other":{"command":"node"}}}"#;
    fs::write(&personal, foreign).unwrap();
    let args = vec![
        "restore".into(),
        "--from".into(),
        backup.path().to_string_lossy().into_owned(),
    ];
    assert_eq!(cmd_restore(&args), ExitCode::Success);
    assert_eq!(fs::read(&personal).unwrap(), foreign);
    fs::remove_file(personal).unwrap();
    let legacy = home.path().join(".ccync/mcp.json");
    fs::write(&legacy, foreign).unwrap();
    fs::write(backup.path().join("mcp.json"), br#"{"ccyncMcpServers":{}}"#).unwrap();
    let catalog = home.path().join(".ccync/plugins.json");
    fs::write(&catalog, br#"{"plugins":[{"pluginId":"keep"}]}"#).unwrap();
    assert_eq!(cmd_restore(&args), ExitCode::Error);
    assert_eq!(fs::read(&legacy).unwrap(), foreign);
    assert_eq!(
        fs::read(&catalog).unwrap(),
        br#"{"plugins":[{"pluginId":"keep"}]}"#
    );
    assert!(matches!(
        ccync_engine::tool_manager::lock::read_restore_marker().unwrap(),
        ccync_engine::tool_manager::lock::RestoreMarkerState::Absent
    ));
}

#[test]
fn restore_legacy_conversion_is_stable_on_rerun() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let backup = TempDir::new().unwrap();
    fs::write(backup.path().join("plugins.json"), br#"{"plugins":[]}"#).unwrap();
    fs::write(
        backup.path().join("mcp.json"),
        br#"{"mcpServers":{"one":{"command":"node","hosts":["claude"]}}}"#,
    )
    .unwrap();
    let args = vec![
        "restore".into(),
        "--from".into(),
        backup.path().to_string_lossy().into_owned(),
    ];
    assert_eq!(cmd_restore(&args), ExitCode::Success);
    let path = home.path().join(".agents/mcp.json");
    let before = fs::read(&path).unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    assert_eq!(cmd_restore(&args), ExitCode::Success);
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
}

#[test]
fn restore_personal_bytes_and_refuse_foreign_inputs() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let backup = TempDir::new().unwrap();
    fs::write(backup.path().join("plugins.json"), br#"{"plugins":[]}"#).unwrap();
    let bytes = b"{\r\n \"ccyncMcpServers\": {\"one\": {\"command\": \"node\"}}\r\n}\r\n";
    fs::write(backup.path().join("mcp.json"), bytes).unwrap();
    let args = vec![
        "restore".into(),
        "--from".into(),
        backup.path().to_string_lossy().into_owned(),
    ];
    let personal = home.path().join(".agents/mcp.json");
    fs::create_dir_all(personal.parent().unwrap()).unwrap();
    let foreign = br#"{"mcpServers":{"other":{"command":"node"}}}"#;
    fs::write(&personal, foreign).unwrap();
    assert_eq!(cmd_restore(&args), ExitCode::Error);
    assert_eq!(fs::read(&personal).unwrap(), foreign);
    assert!(!home.path().join(".ccync/plugins.json").exists());
    fs::remove_file(&personal).unwrap();
    assert_eq!(cmd_restore(&args), ExitCode::Success);
    assert_eq!(fs::read(personal).unwrap(), bytes);
}

#[test]
fn restore_imports_route_intent_without_importing_ownership() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let backup = TempDir::new().unwrap();
    fs::write(backup.path().join("plugins.json"), br#"{"plugins":[]}"#).unwrap();
    fs::write(backup.path().join("config.json"), br#"{"_ccyncBackupMcpRoutes":{"version":1,"routes":[{"source":"personal","server":"example","hosts":["claude"]}]}}"#).unwrap();
    let args = vec![
        "restore".into(),
        "--from".into(),
        backup.path().to_string_lossy().into_owned(),
    ];
    assert_eq!(cmd_restore(&args), ExitCode::Success);
    let layout = ccync_foundation::paths::EffectiveLayout::load_or_legacy().unwrap();
    let managed = layout.generated_mcp_path();
    let routes = mcp::read_migrated_routes(&managed).unwrap();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].identity().source, "personal");
    assert_eq!(routes[0].identity().server, "example");
    assert_eq!(routes[0].hosts(), &["claude"]);
    let preview = mcp::preview_restored_routes(&managed, &routes).unwrap();
    assert_eq!(preview.before(), Some(preview.after()));
    assert!(!managed.exists());
    let config: serde_json::Value =
        serde_json::from_slice(&fs::read(layout.machine_config_path()).unwrap()).unwrap();
    assert!(config.get("_ccyncBackupMcpRoutes").is_none());
}

#[test]
fn portable_restore_case_folded_collision_rejects_backup_before_home_changes() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    fs::create_dir_all(home.path().join(".ccync/build")).unwrap();
    fs::write(home.path().join(".ccync/config.json"), "foreign-config").unwrap();
    fs::write(home.path().join(".ccync/build/lock.json"), "foreign-lock").unwrap();
    let before = snapshot_tree(home.path());
    let colliding_names = vec!["plugins.json".to_string(), "PLUGINS.JSON".to_string()];
    let error = super::lifecycle::validate_restore_backup_names(&colliding_names).unwrap_err();
    assert!(error.contains("case-folded filename collision"));
    let after = snapshot_tree(home.path());
    assert_eq!(before, after);
}

#[test]
fn portable_restore_rejects_junction_before_writing() {
    use std::process::Command;

    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let backup = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();
    fs::write(target.path().join("foreign.txt"), "must remain unchanged").unwrap();
    let home_before = snapshot_tree(home.path());
    let target_before = snapshot_tree(target.path());
    fs::write(backup.path().join("plugins.json"), r#"{"plugins":[]}"#).unwrap();
    let junction = backup.path().join("portable");
    let output = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&junction)
        .arg(target.path())
        .output();
    let Ok(output) = output else { return };
    if !output.status.success() {
        return;
    }
    let args = vec![
        "restore".into(),
        "--from".into(),
        backup.path().to_string_lossy().to_string(),
    ];
    assert_eq!(cmd_restore(&args), ExitCode::Error);
    assert_eq!(snapshot_tree(home.path()), home_before);
    assert_eq!(snapshot_tree(target.path()), target_before);
}

#[test]
fn portable_restore_missing_plugins_json_is_rejected_without_writes() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    fs::create_dir_all(home.path().join(".ccync/build")).unwrap();
    fs::write(home.path().join(".ccync/config.json"), "foreign-config").unwrap();
    fs::write(home.path().join(".ccync/build/lock.json"), "foreign-lock").unwrap();
    let home_before = snapshot_tree(home.path());
    let backup = TempDir::new().unwrap();
    fs::write(backup.path().join("config.json"), "{}").unwrap();
    let args = vec![
        "restore".to_string(),
        "--from".to_string(),
        backup.path().to_string_lossy().to_string(),
    ];
    assert_eq!(cmd_restore(&args), ExitCode::Error);
    assert_eq!(snapshot_tree(home.path()), home_before);
}

#[test]
fn portable_restore_second_home_preserves_catalog_and_adoption_identities() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let backup = TempDir::new().unwrap();
    let catalog =
        r#"{"plugins":[{"pluginId":"personal-one","source":"https://example.test/one"}]}"#;
    let lock = r#"{"_adoptMaster":"claude","_adoptedItems":[{"id":"adopted-one"}],"_personalPlugins":[{"pluginId":"personal-one"}]}"#;
    fs::write(backup.path().join("plugins.json"), catalog).unwrap();
    fs::write(backup.path().join("plugins.lock.json"), lock).unwrap();
    fs::write(backup.path().join("config.json"), "{}").unwrap();
    let args = vec![
        "restore".to_string(),
        "--from".to_string(),
        backup.path().to_string_lossy().to_string(),
    ];
    assert_eq!(cmd_restore(&args), ExitCode::Success);
    assert_eq!(
        fs::read_to_string(home.path().join(".ccync/plugins.json")).unwrap(),
        catalog
    );
    assert_eq!(
        fs::read_to_string(home.path().join(".ccync/build/lock.json")).unwrap(),
        lock
    );
}

#[test]
fn portable_restore_missing_local_source_reports_needs_rebind_and_sync_follow_up() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let backup = TempDir::new().unwrap();
    let missing = home.path().join("missing-source");
    let catalog = serde_json::json!({
        "plugins": [{
            "pluginId": "local-one",
            "sourceType": "local",
            "source": missing.to_string_lossy()
        }]
    });
    fs::write(
        backup.path().join("plugins.json"),
        serde_json::to_vec(&catalog).unwrap(),
    )
    .unwrap();
    let notices = super::lifecycle::restore_follow_up_notices(1, &catalog);
    assert!(notices.iter().any(|notice| {
        notice.contains("local-one: needs-rebind (local source path does not exist)")
    }));
    assert!(notices
        .iter()
        .any(|notice| notice.contains("next step: run `ccync sync`")));
    assert!(notices
        .iter()
        .any(|notice| notice.contains("remote sources are fetched again")));
    let args = vec![
        "restore".into(),
        "--from".into(),
        backup.path().to_string_lossy().to_string(),
    ];
    assert_eq!(cmd_restore(&args), ExitCode::Success);
    assert!(home.path().join(".ccync/plugins.json").is_file());
    assert!(notices.iter().any(|notice| notice.contains("needs-rebind")));
}

#[test]
fn portable_restore_rejects_portable_schema_and_duplicate_key_without_writes() {
    for portable_json in [
        "[]",
        r#"{"schemaVersion":1,"schemaVersion":1,"sources":[]}"#,
    ] {
        let _lock = crate::test_io_lock();
        let home = TempDir::new().unwrap();
        let _env = EnvHomeGuard::set(home.path());
        fs::create_dir_all(home.path().join(".ccync/build")).unwrap();
        fs::write(home.path().join(".ccync/config.json"), "foreign-config").unwrap();
        let home_before = snapshot_tree(home.path());
        let backup = TempDir::new().unwrap();
        fs::write(backup.path().join("plugins.json"), r#"{"plugins":[]}"#).unwrap();
        fs::create_dir(backup.path().join("portable")).unwrap();
        fs::write(backup.path().join("portable/sources.json"), portable_json).unwrap();
        let args = vec![
            "restore".into(),
            "--from".into(),
            backup.path().to_string_lossy().to_string(),
        ];
        assert_eq!(cmd_restore(&args), ExitCode::Error);
        assert_eq!(snapshot_tree(home.path()), home_before);
    }
}

#[test]
fn portable_restore_rejects_unsafe_portable_relative_path_without_writes() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    fs::create_dir_all(home.path().join(".ccync/build")).unwrap();
    fs::write(home.path().join(".ccync/config.json"), "{}").unwrap();
    let home_before = snapshot_tree(home.path());
    let backup = TempDir::new().unwrap();
    fs::write(backup.path().join("plugins.json"), r#"{"plugins":[]}"#).unwrap();
    fs::create_dir(backup.path().join("portable")).unwrap();
    fs::write(
        backup.path().join("portable/sources.json"),
        r#"{"schemaVersion":1,"sources":[{"source":"https://example.test/../outside","reference":"https://example.test/repo"}]}"#,
    )
    .unwrap();
    let args = vec![
        "restore".into(),
        "--from".into(),
        backup.path().to_string_lossy().to_string(),
    ];
    assert_eq!(cmd_restore(&args), ExitCode::Error);
    assert_eq!(snapshot_tree(home.path()), home_before);
}

#[test]
fn portable_restore_dry_run_writes_nothing() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    fs::create_dir_all(home.path().join(".ccync/build")).unwrap();
    fs::write(home.path().join(".ccync/config.json"), "{}").unwrap();
    fs::write(home.path().join(".ccync/build/lock.json"), "foreign-lock").unwrap();
    let home_before = snapshot_tree(home.path());
    let backup = TempDir::new().unwrap();
    fs::write(backup.path().join("plugins.json"), r#"{"plugins":[]}"#).unwrap();
    fs::write(backup.path().join("config.json"), "{}").unwrap();
    let args = vec![
        "restore".to_string(),
        "--from".to_string(),
        backup.path().to_string_lossy().to_string(),
        "--dry-run".to_string(),
    ];
    assert_eq!(cmd_restore(&args), ExitCode::UpdatesAvailable);
    assert_eq!(snapshot_tree(home.path()), home_before);
}

impl Drop for EnvHomeGuard {
    fn drop(&mut self) {
        for (key, val) in &self.saved {
            unsafe {
                match val {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

#[test]
fn lifecycle_seam_restore_registers_dry_run_flag() {
    let lifecycle_source = include_str!("lifecycle.rs");

    let (_, restore_fn) = lifecycle_source
        .split_once("pub(crate) fn cmd_restore(")
        .expect("lifecycle.rs must define cmd_restore");

    let restore_body = match restore_fn.split_once("pub(crate) fn ") {
        Some((body, _)) => body,
        None => restore_fn,
    };

    assert!(
        restore_body.contains(r#""--dry-run""#),
        "--dry-run is rejected as unknown today: cmd_restore must include --dry-run in allowed_flags"
    );
}

#[test]
fn restore_dry_run_flag_accepted_by_cli_seam() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let profile = TempDir::new().unwrap();

    let args = vec![
        "restore".to_string(),
        "--from".to_string(),
        profile.path().to_string_lossy().to_string(),
        "--dry-run".to_string(),
    ];

    let exit = cmd_restore(&args);
    assert_ne!(
        exit,
        ExitCode::Usage,
        "--dry-run is rejected as unknown on the restore verb today"
    );
}

#[test]
fn restore_dry_run_is_short_circuited_branch_in_source() {
    let lifecycle_source = include_str!("lifecycle.rs");

    let (_, restore_fn) = lifecycle_source
        .split_once("pub(crate) fn cmd_restore(")
        .expect("lifecycle.rs must define cmd_restore");

    let dry_run_pos = restore_fn
        .find("dry_run")
        .expect("cmd_restore must handle dry_run");

    let copy_loop_pos = restore_fn
        .find("exists — will be overwritten")
        .or_else(|| restore_fn.find("file(s) restored from"))
        .expect("cmd_restore must retain the restore copy loop");

    assert!(
        dry_run_pos < copy_loop_pos,
        "dry_run branch must short-circuit before entering the per-file copy loop"
    );
}

#[test]
fn restore_dry_run_identical_profile_converged_exits_0() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let profile = TempDir::new().unwrap();

    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    fs::write(
        ccync.join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();
    fs::write(
        ccync.join("plugins.json"),
        r#"{"plugins":[{"id":"test-plugin"}]}"#,
    )
    .unwrap();
    fs::write(
        ccync.join("build").join("lock.json"),
        r#"{"_personalPlugins":[]}"#,
    )
    .unwrap();
    fs::write(ccync.join("AGENTS.md"), "# Instructions\n").unwrap();

    fs::write(
        profile.path().join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("plugins.json"),
        r#"{"plugins":[{"id":"test-plugin"}]}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("plugins.lock.json"),
        r#"{"_personalPlugins":[]}"#,
    )
    .unwrap();
    fs::write(profile.path().join("AGENTS.md"), "# Instructions\n").unwrap();

    let args = vec![
        "restore".to_string(),
        "--from".to_string(),
        profile.path().to_string_lossy().to_string(),
        "--dry-run".to_string(),
    ];

    let exit = cmd_restore(&args);
    assert_eq!(
        exit,
        ExitCode::Success,
        "an identical profile must exit 0 (converged)"
    );
}

#[test]
fn restore_dry_run_differing_profile_drift_exits_3() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let profile = TempDir::new().unwrap();

    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    fs::write(
        ccync.join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();
    fs::write(
        ccync.join("plugins.json"),
        r#"{"plugins":[{"id":"plugin-a"}]}"#,
    )
    .unwrap();
    fs::write(
        ccync.join("build").join("lock.json"),
        r#"{"_personalPlugins":[]}"#,
    )
    .unwrap();
    fs::write(ccync.join("AGENTS.md"), "# Instructions A\n").unwrap();

    fs::write(
        profile.path().join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("plugins.json"),
        r#"{"plugins":[{"id":"plugin-b"}]}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("plugins.lock.json"),
        r#"{"_personalPlugins":[]}"#,
    )
    .unwrap();
    fs::write(profile.path().join("AGENTS.md"), "# Instructions B\n").unwrap();

    let args = vec![
        "restore".to_string(),
        "--from".to_string(),
        profile.path().to_string_lossy().to_string(),
        "--dry-run".to_string(),
    ];

    let exit = cmd_restore(&args);
    assert_eq!(
        exit,
        ExitCode::UpdatesAvailable,
        "a differing profile must exit 3 (drift reusing ExitCode::UpdatesAvailable)"
    );
}

#[test]
fn restore_dry_run_missing_profile_dir_exits_1() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let tmp = TempDir::new().unwrap();
    let missing_dir = tmp.path().join("missing_profile");

    let args = vec![
        "restore".to_string(),
        "--from".to_string(),
        missing_dir.to_string_lossy().to_string(),
        "--dry-run".to_string(),
    ];

    let exit = cmd_restore(&args);
    assert_eq!(
        exit,
        ExitCode::Error,
        "an unreadable or missing profile directory must exit 1 (ExitCode::Error)"
    );
}

#[test]
fn restore_dry_run_empty_profile_dir_against_initialized_machine_exits_3() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let profile = TempDir::new().unwrap(); // readable but empty

    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    fs::write(
        ccync.join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();
    fs::write(
        ccync.join("plugins.json"),
        r#"{"plugins":[{"id":"test-plugin"}]}"#,
    )
    .unwrap();
    fs::write(
        ccync.join("build").join("lock.json"),
        r#"{"_personalPlugins":[]}"#,
    )
    .unwrap();
    fs::write(ccync.join("AGENTS.md"), "# Instructions\n").unwrap();

    let args = vec![
        "restore".to_string(),
        "--from".to_string(),
        profile.path().to_string_lossy().to_string(),
        "--dry-run".to_string(),
    ];

    let exit = cmd_restore(&args);
    assert_eq!(
        exit,
        ExitCode::UpdatesAvailable,
        "a readable but empty profile directory against an initialized machine must exit 3"
    );
}

#[test]
fn restore_dry_run_never_writes_on_any_path() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let profile = TempDir::new().unwrap();

    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    let initial_config = r#"{"selectedProjectionTargets":["claude"]}"#;
    let initial_plugins = r#"{"plugins":[{"id":"local-only"}]}"#;
    let initial_lock = r#"{"_personalPlugins":[],"version":1}"#;
    let initial_agents = "# Local Rules\n";

    fs::write(ccync.join("config.json"), initial_config).unwrap();
    fs::write(ccync.join("plugins.json"), initial_plugins).unwrap();
    fs::write(ccync.join("build").join("lock.json"), initial_lock).unwrap();
    fs::write(ccync.join("AGENTS.md"), initial_agents).unwrap();

    let profile_config = r#"{"selectedProjectionTargets":["codex"]}"#;
    let profile_plugins = r#"{"plugins":[{"id":"profile-plugin"}]}"#;
    let profile_lock = r#"{"_personalPlugins":[],"version":2}"#;
    let profile_agents = "# Remote Rules\n";

    fs::write(profile.path().join("config.json"), profile_config).unwrap();
    fs::write(profile.path().join("plugins.json"), profile_plugins).unwrap();
    fs::write(profile.path().join("plugins.lock.json"), profile_lock).unwrap();
    fs::write(profile.path().join("AGENTS.md"), profile_agents).unwrap();

    let args = vec![
        "restore".to_string(),
        "--from".to_string(),
        profile.path().to_string_lossy().to_string(),
        "--dry-run".to_string(),
    ];

    let exit = cmd_restore(&args);
    assert_ne!(
        exit,
        ExitCode::Usage,
        "--dry-run must not be rejected as an unknown flag"
    );

    // Verify machine files are untouched
    assert_eq!(
        fs::read_to_string(ccync.join("config.json")).unwrap(),
        initial_config,
        "dry-run must never modify machine config.json"
    );
    assert_eq!(
        fs::read_to_string(ccync.join("plugins.json")).unwrap(),
        initial_plugins,
        "dry-run must never modify machine plugins.json"
    );
    assert_eq!(
        fs::read_to_string(ccync.join("build").join("lock.json")).unwrap(),
        initial_lock,
        "dry-run must never modify machine lock.json"
    );
    assert_eq!(
        fs::read_to_string(ccync.join("AGENTS.md")).unwrap(),
        initial_agents,
        "dry-run must never modify machine AGENTS.md"
    );

    // Verify profile files are untouched
    assert_eq!(
        fs::read_to_string(profile.path().join("config.json")).unwrap(),
        profile_config,
        "dry-run must never modify profile config.json"
    );
    assert_eq!(
        fs::read_to_string(profile.path().join("plugins.json")).unwrap(),
        profile_plugins,
        "dry-run must never modify profile plugins.json"
    );
    assert_eq!(
        fs::read_to_string(profile.path().join("plugins.lock.json")).unwrap(),
        profile_lock,
        "dry-run must never modify profile plugins.lock.json"
    );
    assert_eq!(
        fs::read_to_string(profile.path().join("AGENTS.md")).unwrap(),
        profile_agents,
        "dry-run must never modify profile AGENTS.md"
    );
}

#[test]
fn restore_dry_run_absent_on_both_sides_agents_md_converged_exits_0() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let profile = TempDir::new().unwrap();

    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    fs::write(
        ccync.join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();
    fs::write(
        ccync.join("plugins.json"),
        r#"{"plugins":[{"id":"test-plugin"}]}"#,
    )
    .unwrap();
    fs::write(
        ccync.join("build").join("lock.json"),
        r#"{"_personalPlugins":[]}"#,
    )
    .unwrap();
    // Neither machine nor profile has AGENTS.md

    fs::write(
        profile.path().join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("plugins.json"),
        r#"{"plugins":[{"id":"test-plugin"}]}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("plugins.lock.json"),
        r#"{"_personalPlugins":[]}"#,
    )
    .unwrap();

    let args = vec![
        "restore".to_string(),
        "--from".to_string(),
        profile.path().to_string_lossy().to_string(),
        "--dry-run".to_string(),
    ];

    let exit = cmd_restore(&args);
    assert_eq!(
        exit,
        ExitCode::Success,
        "absent-on-both-sides for AGENTS.md must be treated as converged (exits 0)"
    );
}

#[test]
fn restore_resumes_pending_destinations_and_rejects_changes() {
    use ccync_engine::tool_manager::lock::{
        read_restore_marker, LifecycleLock, RestoreMarkerState,
    };
    let _lock = crate::test_io_lock();
    for completed in 0..=2 {
        for third in [false, true] {
            let home = TempDir::new().unwrap();
            let _env = EnvHomeGuard::set(home.path());
            let backup = TempDir::new().unwrap();
            let config = br#"{}"#;
            let plugins = br#"{"plugins":[]}"#;
            fs::write(backup.path().join("config.json"), config).unwrap();
            fs::write(backup.path().join("plugins.json"), plugins).unwrap();
            let root = home.path().join(".ccync");
            fs::create_dir_all(&root).unwrap();
            let old_config = br#"{"old":true}"#;
            let old_plugins = br#"{"plugins":[{"id":"old"}]}"#;
            let sources: [(&str, Option<&[u8]>); 7] = [
                ("config.json", Some(config)),
                ("plugins.json", Some(plugins)),
                ("plugins.lock.json", None),
                ("AGENTS.md", None),
                ("mcp.json", None),
                ("portable/sources.json", None),
                ("portable/lock.json", None),
            ];
            let guard = LifecycleLock::acquire_for_restore_sources(
                &sources,
                &["config.json", "plugins.json"],
            )
            .unwrap();
            guard
                .record_destinations(&[
                    ("config.json", Some(old_config), Some(config)),
                    ("plugins.json", Some(old_plugins), Some(plugins)),
                ])
                .unwrap();
            drop(guard);
            let current_config: &[u8] = if completed > 0 { config } else { old_config };
            let current_plugins: &[u8] = if third {
                b"third"
            } else if completed > 1 {
                plugins
            } else {
                old_plugins
            };
            fs::write(root.join("config.json"), current_config).unwrap();
            fs::write(root.join("plugins.json"), current_plugins).unwrap();
            let args = vec![
                "restore".into(),
                "--from".into(),
                backup.path().to_string_lossy().into_owned(),
            ];
            assert_eq!(crate::run(&["list".into()]), ExitCode::Error);
            let other = TempDir::new().unwrap();
            fs::write(other.path().join("config.json"), br#"{"other":true}"#).unwrap();
            fs::write(other.path().join("plugins.json"), plugins).unwrap();
            let other_args = vec![
                "restore".into(),
                "--from".into(),
                other.path().to_string_lossy().into_owned(),
            ];
            assert_eq!(cmd_restore(&other_args), ExitCode::Error);
            assert_eq!(fs::read(root.join("config.json")).unwrap(), current_config);
            assert_eq!(
                fs::read(root.join("plugins.json")).unwrap(),
                current_plugins
            );
            assert!(matches!(
                read_restore_marker().unwrap(),
                RestoreMarkerState::Pending(_)
            ));
            if third {
                assert_eq!(cmd_restore(&args), ExitCode::Error);
                assert_eq!(fs::read(root.join("config.json")).unwrap(), current_config);
                assert_eq!(
                    fs::read(root.join("plugins.json")).unwrap(),
                    current_plugins
                );
                assert!(matches!(
                    read_restore_marker().unwrap(),
                    RestoreMarkerState::Pending(_)
                ));
            } else {
                assert_eq!(cmd_restore(&args), ExitCode::Success);
                assert_eq!(fs::read(root.join("config.json")).unwrap(), config);
                assert_eq!(fs::read(root.join("plugins.json")).unwrap(), plugins);
                assert_eq!(read_restore_marker().unwrap(), RestoreMarkerState::Absent);
            }
        }
    }
}
