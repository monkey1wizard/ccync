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
