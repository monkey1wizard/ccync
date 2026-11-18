//! Acceptance probes for the backup and restore payload including `AGENTS.md`.
//!
//! Seam: The two `canonical_files` payload lists in `cmd_backup` and `cmd_restore`
//! within `crates/ccync-cli/src/commands/lifecycle.rs`.

use super::lifecycle::{cmd_backup, cmd_restore};
use ccync_engine::ExitCode;
use std::fs;
use tempfile::TempDir;

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
            let value = match key {
                "APPDATA" | "XDG_CONFIG_HOME" => tmp.join("config"),
                "LOCALAPPDATA" | "XDG_STATE_HOME" => tmp.join("state"),
                "XDG_CACHE_HOME" => tmp.join("cache"),
                _ => tmp.to_path_buf(),
            };
            unsafe {
                std::env::set_var(key, value);
            }
        }
        Self { saved }
    }
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
fn backup_payload_includes_agents_md() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let out = TempDir::new().unwrap();
    let out_dir = out.path().join("backup");

    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    fs::write(
        ccync.join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();
    fs::write(ccync.join("plugins.json"), r#"{"plugins":[{"id":"demo"}]}"#).unwrap();
    fs::write(
        ccync.join("build").join("lock.json"),
        r#"{"_personalPlugins":[]}"#,
    )
    .unwrap();
    fs::write(ccync.join("AGENTS.md"), "# Team Rules\n").unwrap();

    let args = vec![
        "backup".to_string(),
        "--output".to_string(),
        out_dir.to_string_lossy().to_string(),
    ];
    let exit = cmd_backup(&args);

    assert_eq!(exit, ExitCode::Success);
    assert!(
        out_dir.join("AGENTS.md").exists(),
        "backup directory does not yet contain the INPUT file: AGENTS.md"
    );
    assert_eq!(
        fs::read_to_string(out_dir.join("AGENTS.md")).unwrap(),
        "# Team Rules\n",
        "backed-up AGENTS.md must carry the source content"
    );
}

#[test]
fn restore_payload_restores_agents_md_to_ccync_home() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let backup = TempDir::new().unwrap();

    fs::write(
        backup.path().join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();
    fs::write(
        backup.path().join("plugins.json"),
        r#"{"plugins":[{"id":"demo"}]}"#,
    )
    .unwrap();
    fs::write(
        backup.path().join("plugins.lock.json"),
        r#"{"_personalPlugins":[]}"#,
    )
    .unwrap();
    fs::write(backup.path().join("AGENTS.md"), "# Backup Instructions\n").unwrap();

    let args = vec![
        "restore".to_string(),
        "--from".to_string(),
        backup.path().to_string_lossy().to_string(),
    ];
    let exit = cmd_restore(&args);

    assert_eq!(exit, ExitCode::Success);
    let ccync = home.path().join(".ccync");
    assert!(
        ccync.join("AGENTS.md").exists(),
        "restore must copy AGENTS.md to ~/.ccync/AGENTS.md"
    );
    assert_eq!(
        fs::read_to_string(ccync.join("AGENTS.md")).unwrap(),
        "# Backup Instructions\n",
        "restored AGENTS.md must carry the backup content"
    );
}

#[test]
fn backup_skips_missing_agents_md_without_error() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let out = TempDir::new().unwrap();
    let out_dir = out.path().join("backup");

    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    fs::write(
        ccync.join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();
    fs::write(ccync.join("plugins.json"), r#"{"plugins":[{"id":"demo"}]}"#).unwrap();
    fs::write(
        ccync.join("build").join("lock.json"),
        r#"{"_personalPlugins":[]}"#,
    )
    .unwrap();
    // AGENTS.md is absent

    let args = vec![
        "backup".to_string(),
        "--output".to_string(),
        out_dir.to_string_lossy().to_string(),
    ];
    let exit = cmd_backup(&args);

    assert_eq!(
        exit,
        ExitCode::Success,
        "missing AGENTS.md must be skipped, not errored"
    );
    assert!(out_dir.join("config.json").exists());
    assert!(out_dir.join("plugins.json").exists());
    assert!(out_dir.join("plugins.lock.json").exists());
    assert!(
        !out_dir.join("AGENTS.md").exists(),
        "absent AGENTS.md must not be created in backup directory"
    );
}

#[test]
fn restore_legacy_backup_without_agents_md_succeeds() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let backup = TempDir::new().unwrap();

    fs::write(
        backup.path().join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();
    fs::write(
        backup.path().join("plugins.json"),
        r#"{"plugins":[{"id":"demo"}]}"#,
    )
    .unwrap();
    fs::write(
        backup.path().join("plugins.lock.json"),
        r#"{"_personalPlugins":[]}"#,
    )
    .unwrap();

    let args = vec![
        "restore".to_string(),
        "--from".to_string(),
        backup.path().to_string_lossy().to_string(),
    ];
    let exit = cmd_restore(&args);

    assert_eq!(exit, ExitCode::Success);
    let ccync = home.path().join(".ccync");
    assert!(
        ccync.join("config.json").exists(),
        "config.json must be restored"
    );
    assert!(
        !ccync.join("AGENTS.md").exists(),
        "AGENTS.md must not exist when restoring legacy backup lacking it"
    );
}

#[test]
fn lifecycle_seam_both_backup_and_restore_include_agents_md_in_canonical_files() {
    let lifecycle_source = include_str!("lifecycle.rs");

    let (_, backup_fn) = lifecycle_source
        .split_once("pub(crate) fn cmd_backup(")
        .expect("lifecycle.rs must define cmd_backup");
    let (backup_body, restore_fn) = backup_fn
        .split_once("pub(crate) fn cmd_restore(")
        .expect("lifecycle.rs must define cmd_restore after cmd_backup");
    let (restore_body, _) = restore_fn
        .split_once("pub(crate) fn cmd_uninstall(")
        .expect("lifecycle.rs must define cmd_uninstall after cmd_restore");

    assert!(
        backup_body.contains("canonical_files") && backup_body.contains("AGENTS.md"),
        "cmd_backup must include AGENTS.md in canonical_files payload list"
    );
    assert!(
        restore_body.contains("canonical_files") && restore_body.contains("AGENTS.md"),
        "cmd_restore must include AGENTS.md in canonical_files payload list"
    );
}

#[test]
fn backup_and_restore_preserve_mcp_json_bytes_and_legacy_backup_succeeds() {
    mcp_input_lifecycle_backup_and_restore_preserves_mcp_json_exact_bytes();
    mcp_input_lifecycle_restore_legacy_backup_without_mcp_json_succeeds();
}

#[test]
fn mcp_input_lifecycle_backup_and_restore_preserves_mcp_json_exact_bytes() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let out = TempDir::new().unwrap();
    let backup = out.path().join("backup");
    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    fs::write(
        ccync.join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();
    fs::write(ccync.join("plugins.json"), r#"{"plugins":[]}"#).unwrap();
    fs::write(
        ccync.join("build").join("lock.json"),
        r#"{"_personalPlugins":[]}"#,
    )
    .unwrap();
    let mcp_bytes = b"{\r\n  \"mcpServers\": {\r\n    \"custom\": {\r\n      \"command\": \"node\"\r\n    }\r\n  }\r\n}\r\n";
    fs::write(ccync.join("mcp.json"), mcp_bytes).unwrap();

    assert_eq!(
        cmd_backup(&[
            "backup".into(),
            "--output".into(),
            backup.to_string_lossy().into()
        ]),
        ExitCode::Success
    );
    assert_eq!(fs::read(backup.join("mcp.json")).unwrap(), mcp_bytes);
    fs::remove_file(ccync.join("mcp.json")).unwrap();
    assert_eq!(
        cmd_restore(&[
            "restore".into(),
            "--from".into(),
            backup.to_string_lossy().into()
        ]),
        ExitCode::Success
    );
    let restored: serde_json::Value = serde_json::from_slice(
        &fs::read(home.path().join(".agents/mcp.json")).unwrap(),
    ).unwrap();
    assert_eq!(restored["ccyncMcpServers"]["custom"]["command"], "node");
    assert!(restored.get("mcpServers").is_none());
    assert!(!ccync.join("mcp.json").exists());
}

#[test]
fn mcp_input_lifecycle_restore_legacy_backup_without_mcp_json_succeeds() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let legacy = TempDir::new().unwrap();
    fs::write(
        legacy.path().join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();
    fs::write(legacy.path().join("plugins.json"), r#"{"plugins":[]}"#).unwrap();
    assert_eq!(
        cmd_restore(&[
            "restore".into(),
            "--from".into(),
            legacy.path().to_string_lossy().into()
        ]),
        ExitCode::Success
    );
    assert!(
        !home.path().join(".agents/mcp.json").exists(),
        "mcp.json must not exist when restoring legacy backup lacking it"
    );
}

#[test]
fn mcp_input_lifecycle_backup_skips_missing_mcp_json_without_error() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let out = TempDir::new().unwrap();
    let backup = out.path().join("backup");
    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    fs::write(
        ccync.join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();
    fs::write(ccync.join("plugins.json"), r#"{"plugins":[]}"#).unwrap();
    fs::write(
        ccync.join("build").join("lock.json"),
        r#"{"_personalPlugins":[]}"#,
    )
    .unwrap();

    assert_eq!(
        cmd_backup(&[
            "backup".into(),
            "--output".into(),
            backup.to_string_lossy().into()
        ]),
        ExitCode::Success
    );
    assert!(
        !backup.join("mcp.json").exists(),
        "absent mcp.json must not be created in backup directory"
    );
}

#[test]
fn mcp_input_lifecycle_seam_both_backup_and_restore_include_mcp_json_in_canonical_files() {
    let lifecycle_source = include_str!("lifecycle.rs");

    let (_, backup_fn) = lifecycle_source
        .split_once("pub(crate) fn cmd_backup(")
        .expect("lifecycle.rs must define cmd_backup");
    let (backup_body, restore_fn) = backup_fn
        .split_once("pub(crate) fn cmd_restore(")
        .expect("lifecycle.rs must define cmd_restore after cmd_backup");
    let (restore_body, _) = restore_fn
        .split_once("pub(crate) fn cmd_uninstall(")
        .expect("lifecycle.rs must define cmd_uninstall after cmd_restore");

    assert!(
        backup_body.contains("canonical_files") && backup_body.contains("mcp.json"),
        "cmd_backup must include mcp.json in canonical_files payload list"
    );
    assert!(
        restore_body.contains("canonical_files") && restore_body.contains("mcp.json"),
        "cmd_restore must include mcp.json in canonical_files payload list"
    );
}
