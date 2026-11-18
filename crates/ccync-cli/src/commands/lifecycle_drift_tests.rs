//! Acceptance probes for drift comparison scope.
//!
//! Seam: the three-class comparison behind `restore --dry-run` in `cmd_restore`
//! within `crates/ccync-cli/src/commands/lifecycle.rs`.
//!
//! Drift comparison rules:
//! - Classify each payload file as converged, differs, or absent-on-one-side;
//!   absent on both sides is converged.
//! - Compare `config.json` whole minus the top-level `secrets` key, reporting a
//!   machine-path difference as differs on purpose.
//! - Compare `plugins.lock.json` only on `_adoptMaster`, `_adoptedItems`,
//!   `_mcpServers`, and `_looseSkills`, structurally, after secret normalization
//!   on both sides; exclude `_personalPlugins` and every other top-level key,
//!   or `0` becomes unreachable after the first `sync`.
//! - Compare `plugins.json` whole.
//! - Compare the INPUT file (`AGENTS.md`) byte-for-byte (CRLF vs LF is drift).

use super::lifecycle::cmd_restore;
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

/// EF-01 Acceptance Probe:
/// When `plugins.lock.json` only differs on regenerated keys (`generatedAt`,
/// `driftDetection`, `resolvedPlugins`, `_ccyncProjection`, `held`) and
/// `_personalPlugins`, but its tracked keys (`_adoptMaster`, `_adoptedItems`,
/// `_mcpServers`, `_looseSkills`) are identical, `restore --dry-run` must
/// classify `plugins.lock.json` as converged and exit 0.
///
/// Current behavior: naive byte comparison classifies `plugins.lock.json` as
/// differs, printing `ccync restore: plugins.lock.json: differs` to stdout,
/// and exits with 3 (UpdatesAvailable).
#[test]
fn restore_dry_run_ignores_regenerated_lockfile_keys_and_personal_plugins_as_converged() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let profile = TempDir::new().unwrap();

    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();

    // Identical config.json
    fs::write(
        ccync.join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();

    // Identical plugins.json
    fs::write(
        ccync.join("plugins.json"),
        r#"{"plugins":[{"id":"test-plugin"}]}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("plugins.json"),
        r#"{"plugins":[{"id":"test-plugin"}]}"#,
    )
    .unwrap();

    // Identical AGENTS.md
    fs::write(ccync.join("AGENTS.md"), "# Project Instructions\n").unwrap();
    fs::write(profile.path().join("AGENTS.md"), "# Project Instructions\n").unwrap();

    // Machine lock.json: contains tracked keys plus regenerated/excluded keys
    let machine_lock = r#"{
  "_adoptMaster": "claude",
  "_adoptedItems": [{"name": "item-1"}],
  "_mcpServers": {"server-a": {"command": "node"}},
  "_looseSkills": ["skill-a"],
  "generatedAt": "2026-09-11T12:00:00Z",
  "driftDetection": {"hash": "abc123machine"},
  "resolvedPlugins": [{"id": "test-plugin", "version": "1.0.0"}],
  "_ccyncProjection": {"version": 2},
  "_personalPlugins": [{"id": "local-only-plugin"}],
  "held": true
}"#;
    fs::write(ccync.join("build").join("lock.json"), machine_lock).unwrap();

    // Profile plugins.lock.json: contains identical tracked keys, but differing/omitted regenerated keys
    let profile_lock = r#"{
  "_adoptMaster": "claude",
  "_adoptedItems": [{"name": "item-1"}],
  "_mcpServers": {"server-a": {"command": "node"}},
  "_looseSkills": ["skill-a"],
  "generatedAt": "2026-09-01T00:00:00Z",
  "driftDetection": {"hash": "xyz789profile"},
  "resolvedPlugins": [],
  "_ccyncProjection": {"version": 1},
  "_personalPlugins": [],
  "held": false
}"#;
    fs::write(profile.path().join("plugins.lock.json"), profile_lock).unwrap();

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
        "regenerated lockfile keys and _personalPlugins must be excluded from comparison; plugins.lock.json must be converged"
    );
}

/// Acceptance Probe:
/// `plugins.lock.json` secret fields must be normalized before comparison.
/// If a profile backup has secrets stripped to `${CCYNC_SECRET_<SERVER>_<KEY>}`
/// while the machine lock carries raw secrets, normalizing both sides allows
/// them to compare as converged.
#[test]
fn restore_dry_run_normalizes_secrets_in_lockfile_before_comparison() {
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
        profile.path().join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();

    fs::write(
        ccync.join("plugins.json"),
        r#"{"plugins":[{"id":"test-plugin"}]}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("plugins.json"),
        r#"{"plugins":[{"id":"test-plugin"}]}"#,
    )
    .unwrap();

    fs::write(ccync.join("AGENTS.md"), "# Instructions\n").unwrap();
    fs::write(profile.path().join("AGENTS.md"), "# Instructions\n").unwrap();

    // Machine lock has raw secret in env
    let machine_lock = r#"{
  "_adoptMaster": "claude",
  "_adoptedItems": [],
  "_mcpServers": {
    "github": {
      "command": "node",
      "env": {
        "API_TOKEN": "actual_machine_secret_999"
      }
    }
  },
  "_looseSkills": []
}"#;
    fs::write(ccync.join("build").join("lock.json"), machine_lock).unwrap();

    // Profile lock has stripped secret placeholder from backup --strip-secrets
    let profile_lock = r#"{
  "_adoptMaster": "claude",
  "_adoptedItems": [],
  "_mcpServers": {
    "github": {
      "command": "node",
      "env": {
        "API_TOKEN": "${CCYNC_SECRET_GITHUB_API_TOKEN}"
      }
    }
  },
  "_looseSkills": []
}"#;
    fs::write(profile.path().join("plugins.lock.json"), profile_lock).unwrap();

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
        "structural secret normalization must match raw secrets against stripped placeholders"
    );
}

/// Acceptance Probe:
/// Differing tracked key in `plugins.lock.json` (`_adoptedItems`) must report
/// differs and exit 3 (ExitCode::UpdatesAvailable).
#[test]
fn restore_dry_run_differing_adopted_items_in_lockfile_reports_differs_exits_3() {
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
        profile.path().join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();

    fs::write(
        ccync.join("plugins.json"),
        r#"{"plugins":[{"id":"test-plugin"}]}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("plugins.json"),
        r#"{"plugins":[{"id":"test-plugin"}]}"#,
    )
    .unwrap();

    fs::write(ccync.join("AGENTS.md"), "# Instructions\n").unwrap();
    fs::write(profile.path().join("AGENTS.md"), "# Instructions\n").unwrap();

    fs::write(
        ccync.join("build").join("lock.json"),
        r#"{"_adoptMaster":"claude","_adoptedItems":[{"name":"item-a"}],"_mcpServers":{},"_looseSkills":[]}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("plugins.lock.json"),
        r#"{"_adoptMaster":"claude","_adoptedItems":[{"name":"item-b"}],"_mcpServers":{},"_looseSkills":[]}"#,
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
        ExitCode::UpdatesAvailable,
        "differing _adoptedItems must report differs and exit 3"
    );
}

/// Acceptance Probe:
/// `config.json` comparison omits the top-level `secrets` key.
/// If only `secrets` differs between machine and profile `config.json`,
/// it must be treated as converged (exit 0).
#[test]
fn restore_dry_run_ignores_config_secrets_key_difference_as_converged() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let profile = TempDir::new().unwrap();

    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();

    // Machine config has secrets map
    fs::write(
        ccync.join("config.json"),
        r#"{
  "selectedProjectionTargets": ["claude"],
  "secrets": {
    "SECRET_API_KEY": "machine_local_secret_value"
  }
}"#,
    )
    .unwrap();

    // Profile config has no secrets map
    fs::write(
        profile.path().join("config.json"),
        r#"{
  "selectedProjectionTargets": ["claude"]
}"#,
    )
    .unwrap();

    fs::write(
        ccync.join("plugins.json"),
        r#"{"plugins":[{"id":"test-plugin"}]}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("plugins.json"),
        r#"{"plugins":[{"id":"test-plugin"}]}"#,
    )
    .unwrap();

    fs::write(
        ccync.join("build").join("lock.json"),
        r#"{"_adoptMaster":"claude","_adoptedItems":[],"_mcpServers":{},"_looseSkills":[]}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("plugins.lock.json"),
        r#"{"_adoptMaster":"claude","_adoptedItems":[],"_mcpServers":{},"_looseSkills":[]}"#,
    )
    .unwrap();

    fs::write(ccync.join("AGENTS.md"), "# Instructions\n").unwrap();
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
        "config.json comparison must omit top-level secrets key, resulting in converged exit 0"
    );
}

/// Acceptance Probe:
/// `config.json` machine-path difference (e.g. `tempDir`) must report differs
/// on purpose and exit 3 (ExitCode::UpdatesAvailable).
#[test]
fn restore_dry_run_config_machine_path_difference_reports_differs_exits_3() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let profile = TempDir::new().unwrap();

    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();

    fs::write(
        ccync.join("config.json"),
        r#"{"selectedProjectionTargets":["claude"],"tempDir":"/tmp/machine_path"}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("config.json"),
        r#"{"selectedProjectionTargets":["claude"],"tempDir":"/tmp/profile_path"}"#,
    )
    .unwrap();

    fs::write(
        ccync.join("plugins.json"),
        r#"{"plugins":[{"id":"test-plugin"}]}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("plugins.json"),
        r#"{"plugins":[{"id":"test-plugin"}]}"#,
    )
    .unwrap();

    fs::write(
        ccync.join("build").join("lock.json"),
        r#"{"_adoptMaster":"claude","_adoptedItems":[],"_mcpServers":{},"_looseSkills":[]}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("plugins.lock.json"),
        r#"{"_adoptMaster":"claude","_adoptedItems":[],"_mcpServers":{},"_looseSkills":[]}"#,
    )
    .unwrap();

    fs::write(ccync.join("AGENTS.md"), "# Instructions\n").unwrap();
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
        ExitCode::UpdatesAvailable,
        "config.json machine-path difference (tempDir) must report differs on purpose and exit 3"
    );
}

/// Acceptance Probe:
/// `plugins.json` compared whole: differences must report differs and exit 3.
#[test]
fn restore_dry_run_plugins_json_difference_reports_differs_exits_3() {
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
        profile.path().join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();

    fs::write(
        ccync.join("plugins.json"),
        r#"{"plugins":[{"id":"plugin-machine"}]}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("plugins.json"),
        r#"{"plugins":[{"id":"plugin-profile"}]}"#,
    )
    .unwrap();

    fs::write(
        ccync.join("build").join("lock.json"),
        r#"{"_adoptMaster":"claude","_adoptedItems":[],"_mcpServers":{},"_looseSkills":[]}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("plugins.lock.json"),
        r#"{"_adoptMaster":"claude","_adoptedItems":[],"_mcpServers":{},"_looseSkills":[]}"#,
    )
    .unwrap();

    fs::write(ccync.join("AGENTS.md"), "# Instructions\n").unwrap();
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
        ExitCode::UpdatesAvailable,
        "plugins.json difference must report differs and exit 3"
    );
}

/// Acceptance Probe:
/// INPUT file (`AGENTS.md`) is compared byte-for-byte:
/// A CRLF-only difference between machine and profile must be treated as drift (exits 3).
#[test]
fn restore_dry_run_agents_md_crlf_vs_lf_is_treated_as_drift_exits_3() {
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
        profile.path().join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();

    fs::write(
        ccync.join("plugins.json"),
        r#"{"plugins":[{"id":"test-plugin"}]}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("plugins.json"),
        r#"{"plugins":[{"id":"test-plugin"}]}"#,
    )
    .unwrap();

    fs::write(
        ccync.join("build").join("lock.json"),
        r#"{"_adoptMaster":"claude","_adoptedItems":[],"_mcpServers":{},"_looseSkills":[]}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("plugins.lock.json"),
        r#"{"_adoptMaster":"claude","_adoptedItems":[],"_mcpServers":{},"_looseSkills":[]}"#,
    )
    .unwrap();

    // Machine uses LF, profile uses CRLF
    fs::write(ccync.join("AGENTS.md"), "Line 1\nLine 2\n").unwrap();
    fs::write(profile.path().join("AGENTS.md"), "Line 1\r\nLine 2\r\n").unwrap();

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
        "INPUT file (AGENTS.md) is compared byte-for-byte; CRLF vs LF difference is drift (exit 3)"
    );
}

/// Acceptance Probe:
/// Absent on one side must report differs and exit 3.
#[test]
fn restore_dry_run_absent_on_one_side_reports_differs_exits_3() {
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
        profile.path().join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();

    fs::write(
        ccync.join("plugins.json"),
        r#"{"plugins":[{"id":"test-plugin"}]}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("plugins.json"),
        r#"{"plugins":[{"id":"test-plugin"}]}"#,
    )
    .unwrap();

    fs::write(
        ccync.join("build").join("lock.json"),
        r#"{"_adoptMaster":"claude","_adoptedItems":[],"_mcpServers":{},"_looseSkills":[]}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("plugins.lock.json"),
        r#"{"_adoptMaster":"claude","_adoptedItems":[],"_mcpServers":{},"_looseSkills":[]}"#,
    )
    .unwrap();

    // Machine has AGENTS.md, profile does not
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
        "absent-on-one-side must report differs and exit 3"
    );
}

/// Acceptance Probe:
/// Absent on both sides is converged.
#[test]
fn restore_dry_run_absent_on_both_sides_is_converged_exits_0() {
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
        profile.path().join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();

    fs::write(
        ccync.join("plugins.json"),
        r#"{"plugins":[{"id":"test-plugin"}]}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("plugins.json"),
        r#"{"plugins":[{"id":"test-plugin"}]}"#,
    )
    .unwrap();

    fs::write(
        ccync.join("build").join("lock.json"),
        r#"{"_adoptMaster":"claude","_adoptedItems":[],"_mcpServers":{},"_looseSkills":[]}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("plugins.lock.json"),
        r#"{"_adoptMaster":"claude","_adoptedItems":[],"_mcpServers":{},"_looseSkills":[]}"#,
    )
    .unwrap();

    // Neither machine nor profile has AGENTS.md

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
        "absent-on-both-sides must be treated as converged and exit 0"
    );
}

#[test]
fn restore_dry_run_reports_mcp_json_byte_drift() {
    mcp_input_lifecycle_restore_dry_run_reports_mcp_json_byte_drift();
}

#[test]
fn mcp_input_lifecycle_restore_dry_run_reports_mcp_json_byte_drift() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let profile = TempDir::new().unwrap();
    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    for (path, bytes) in [
        (
            ccync.join("config.json"),
            r#"{"selectedProjectionTargets":["claude"]}"#,
        ),
        (
            profile.path().join("config.json"),
            r#"{"selectedProjectionTargets":["claude"]}"#,
        ),
        (ccync.join("plugins.json"), r#"{"plugins":[]}"#),
        (profile.path().join("plugins.json"), r#"{"plugins":[]}"#),
        (
            ccync.join("build").join("lock.json"),
            r#"{"_adoptMaster":"claude","_adoptedItems":[],"_mcpServers":{},"_looseSkills":[]}"#,
        ),
        (
            profile.path().join("plugins.lock.json"),
            r#"{"_adoptMaster":"claude","_adoptedItems":[],"_mcpServers":{},"_looseSkills":[]}"#,
        ),
    ] {
        fs::write(path, bytes).unwrap();
    }
    fs::create_dir_all(home.path().join(".agents")).unwrap();
    fs::write(home.path().join(".agents/mcp.json"), b"{\"ccyncMcpServers\":{}}\n").unwrap();
    fs::write(profile.path().join("mcp.json"), b"{\"ccyncMcpServers\":{}}\r\n").unwrap();

    let exit = cmd_restore(&[
        "restore".into(),
        "--from".into(),
        profile.path().to_string_lossy().into(),
        "--dry-run".into(),
    ]);
    assert_eq!(exit, ExitCode::UpdatesAvailable);
}

#[test]
fn mcp_input_lifecycle_restore_dry_run_reports_mcp_json_absent_on_one_side() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let profile = TempDir::new().unwrap();
    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    for (path, bytes) in [
        (
            ccync.join("config.json"),
            r#"{"selectedProjectionTargets":["claude"]}"#,
        ),
        (
            profile.path().join("config.json"),
            r#"{"selectedProjectionTargets":["claude"]}"#,
        ),
        (ccync.join("plugins.json"), r#"{"plugins":[]}"#),
        (profile.path().join("plugins.json"), r#"{"plugins":[]}"#),
        (
            ccync.join("build").join("lock.json"),
            r#"{"_adoptMaster":"claude","_adoptedItems":[],"_mcpServers":{},"_looseSkills":[]}"#,
        ),
        (
            profile.path().join("plugins.lock.json"),
            r#"{"_adoptMaster":"claude","_adoptedItems":[],"_mcpServers":{},"_looseSkills":[]}"#,
        ),
    ] {
        fs::write(path, bytes).unwrap();
    }
    // Machine has mcp.json, profile does not
    fs::write(ccync.join("mcp.json"), b"{\"mcpServers\":{}}\n").unwrap();

    let exit = cmd_restore(&[
        "restore".into(),
        "--from".into(),
        profile.path().to_string_lossy().into(),
        "--dry-run".into(),
    ]);
    assert_eq!(exit, ExitCode::Success);
}

#[test]
fn mcp_input_lifecycle_restore_dry_run_converged_when_mcp_json_identical() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let profile = TempDir::new().unwrap();
    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    for (path, bytes) in [
        (
            ccync.join("config.json"),
            r#"{"selectedProjectionTargets":["claude"]}"#,
        ),
        (
            profile.path().join("config.json"),
            r#"{"selectedProjectionTargets":["claude"]}"#,
        ),
        (ccync.join("plugins.json"), r#"{"plugins":[]}"#),
        (profile.path().join("plugins.json"), r#"{"plugins":[]}"#),
        (
            ccync.join("build").join("lock.json"),
            r#"{"_adoptMaster":"claude","_adoptedItems":[],"_mcpServers":{},"_looseSkills":[]}"#,
        ),
        (
            profile.path().join("plugins.lock.json"),
            r#"{"_adoptMaster":"claude","_adoptedItems":[],"_mcpServers":{},"_looseSkills":[]}"#,
        ),
    ] {
        fs::write(path, bytes).unwrap();
    }
    let identical_bytes = b"{\"ccyncMcpServers\":{\"s1\":{\"command\":\"npx\"}}}\n";
    fs::create_dir_all(home.path().join(".agents")).unwrap();
    fs::write(home.path().join(".agents/mcp.json"), identical_bytes).unwrap();
    fs::write(profile.path().join("mcp.json"), identical_bytes).unwrap();

    let exit = cmd_restore(&[
        "restore".into(),
        "--from".into(),
        profile.path().to_string_lossy().into(),
        "--dry-run".into(),
    ]);
    assert_eq!(exit, ExitCode::Success);
}

#[test]
fn mcp_input_lifecycle_restore_dry_run_converged_when_mcp_json_absent_on_both_sides() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let profile = TempDir::new().unwrap();
    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    for (path, bytes) in [
        (
            ccync.join("config.json"),
            r#"{"selectedProjectionTargets":["claude"]}"#,
        ),
        (
            profile.path().join("config.json"),
            r#"{"selectedProjectionTargets":["claude"]}"#,
        ),
        (ccync.join("plugins.json"), r#"{"plugins":[]}"#),
        (profile.path().join("plugins.json"), r#"{"plugins":[]}"#),
        (
            ccync.join("build").join("lock.json"),
            r#"{"_adoptMaster":"claude","_adoptedItems":[],"_mcpServers":{},"_looseSkills":[]}"#,
        ),
        (
            profile.path().join("plugins.lock.json"),
            r#"{"_adoptMaster":"claude","_adoptedItems":[],"_mcpServers":{},"_looseSkills":[]}"#,
        ),
    ] {
        fs::write(path, bytes).unwrap();
    }
    // Neither has mcp.json

    let exit = cmd_restore(&[
        "restore".into(),
        "--from".into(),
        profile.path().to_string_lossy().into(),
        "--dry-run".into(),
    ]);
    assert_eq!(exit, ExitCode::Success);
}

