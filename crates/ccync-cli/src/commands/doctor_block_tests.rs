//! Acceptance probes for the doctor health check registration of managed blocks.
//!
//! Seam: `cmd_doctor()` in `crates/ccync-cli/src/commands/doctor.rs`.
//!
//! Covers:
//! - `ccync doctor` output contains the managed-block finding for each deduped
//!   file and the command writes nothing.
//! - R10: doctor reports, per resolved file, whether the managed block is present,
//!   absent, or damaged. Absent is a finding only while `~/.ccync/AGENTS.md` exists.
//! - Read-only guarantee: `cmd_doctor` modifies no target or configuration files.

use super::doctor::cmd_doctor;
use ccync_engine::ExitCode;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn isolate_home_child(home: &Path) {
    #[cfg(windows)]
    unsafe {
        std::env::set_var("USERPROFILE", home);
        std::env::set_var("HOME", home);
        std::env::set_var("APPDATA", home.join("AppData").join("Roaming"));
        std::env::set_var("LOCALAPPDATA", home.join("AppData").join("Local"));
        std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
    }
    #[cfg(not(windows))]
    unsafe {
        std::env::set_var("HOME", home);
        std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
    }
}

fn run_doctor_child(test_name: &str, child_tag: &str, home: &Path) -> (ExitCode, String) {
    let exe = std::env::current_exe().expect("test binary path");
    let output = std::process::Command::new(exe)
        .arg("--exact")
        .arg(format!("commands::doctor_block_tests::{test_name}"))
        .arg("--nocapture")
        .env("CCYNC_TEST_DOCTOR_CHILD", child_tag)
        .env("CCYNC_TEST_HOME", home)
        .output()
        .expect("relaunch test binary for doctor run");

    let stdout = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let exit = if output.status.success() {
        ExitCode::Success
    } else {
        ExitCode::Error
    };
    (exit, stdout)
}

/// EF-01 Acceptance Probe:
/// When `~/.ccync/AGENTS.md` exists and a selected target's instruction file lacks
/// the managed block, `ccync doctor` must report the absent managed block.
///
/// Current behavior: `cmd_doctor` does not register the managed block health check,
/// so doctor reports nothing about managed blocks.
#[test]
fn doctor_reports_absent_managed_block_when_input_exists() {
    if std::env::var("CCYNC_TEST_DOCTOR_CHILD").as_deref() == Ok("absent_block") {
        let home = std::env::var("CCYNC_TEST_HOME").expect("child must receive fixture home");
        isolate_home_child(Path::new(&home));
        let _ = cmd_doctor(&["doctor".to_string()]);
        return;
    }

    let _lock = crate::test_io_lock();
    let home = tempdir().unwrap();
    let ccync_dir = home.path().join(".ccync");
    fs::create_dir_all(&ccync_dir).unwrap();
    fs::write(
        ccync_dir.join("config.json"),
        r#"{"selectedProjectionTargets":["codex"]}"#,
    )
    .unwrap();
    fs::write(ccync_dir.join("AGENTS.md"), "# Team instructions\n").unwrap();

    let codex_dir = home.path().join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    fs::write(codex_dir.join("AGENTS.md"), "# User instructions\n").unwrap();

    let (_exit, stdout) = run_doctor_child(
        "doctor_reports_absent_managed_block_when_input_exists",
        "absent_block",
        home.path(),
    );

    assert!(
        stdout.contains("managed block"),
        "doctor reports nothing about managed blocks: stdout={stdout}"
    );
}

/// Acceptance Probe:
/// `doctor.rs` must register the managed block health check into `cmd_doctor()`.
#[test]
fn doctor_source_registers_managed_block_health_check() {
    let doctor_source = include_str!("doctor.rs");
    assert!(
        doctor_source.contains("ManagedBlockHealthCheck")
            || doctor_source.contains("managed_block"),
        "doctor reports nothing about managed blocks: doctor.rs must register the managed block health check"
    );
}

/// Acceptance Probe:
/// When a target file holds a damaged managed block (e.g. missing end marker),
/// `ccync doctor` must report the damaged managed block finding.
#[test]
fn doctor_reports_damaged_managed_block() {
    if std::env::var("CCYNC_TEST_DOCTOR_CHILD").as_deref() == Ok("damaged_block") {
        let home = std::env::var("CCYNC_TEST_HOME").expect("child must receive fixture home");
        isolate_home_child(Path::new(&home));
        let _ = cmd_doctor(&["doctor".to_string()]);
        return;
    }

    let _lock = crate::test_io_lock();
    let home = tempdir().unwrap();
    let ccync_dir = home.path().join(".ccync");
    fs::create_dir_all(&ccync_dir).unwrap();
    fs::write(
        ccync_dir.join("config.json"),
        r#"{"selectedProjectionTargets":["codex"]}"#,
    )
    .unwrap();
    fs::write(ccync_dir.join("AGENTS.md"), "# Team instructions\n").unwrap();

    let codex_dir = home.path().join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    fs::write(
        codex_dir.join("AGENTS.md"),
        "<!-- ccync:managed:begin -->\nincomplete body\n",
    )
    .unwrap();

    let (_exit, stdout) = run_doctor_child(
        "doctor_reports_damaged_managed_block",
        "damaged_block",
        home.path(),
    );

    assert!(
        stdout.contains("managed block"),
        "doctor reports nothing about managed blocks: stdout={stdout}"
    );
}

/// Acceptance Probe:
/// When `~/.ccync/AGENTS.md` is absent, an absent block in a target file is NOT a
/// finding; doctor must not report absent managed blocks.
#[test]
fn doctor_skips_absent_managed_block_when_input_is_absent() {
    if std::env::var("CCYNC_TEST_DOCTOR_CHILD").as_deref() == Ok("no_input") {
        let home = std::env::var("CCYNC_TEST_HOME").expect("child must receive fixture home");
        isolate_home_child(Path::new(&home));
        let _ = cmd_doctor(&["doctor".to_string()]);
        return;
    }

    let _lock = crate::test_io_lock();
    let home = tempdir().unwrap();
    let ccync_dir = home.path().join(".ccync");
    fs::create_dir_all(&ccync_dir).unwrap();
    fs::write(
        ccync_dir.join("config.json"),
        r#"{"selectedProjectionTargets":["codex"]}"#,
    )
    .unwrap();
    // No ~/.ccync/AGENTS.md created!

    let claude_dir = home.path().join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    fs::write(claude_dir.join("CLAUDE.md"), "# User instructions\n").unwrap();

    let (_exit, stdout) = run_doctor_child(
        "doctor_skips_absent_managed_block_when_input_is_absent",
        "no_input",
        home.path(),
    );

    assert!(
        !stdout.contains("managed block is absent"),
        "absent block without input AGENTS.md must produce no finding: stdout={stdout}"
    );
}

/// Acceptance Probe:
/// `ccync doctor` must be strictly read-only and never modify or create target
/// instruction files or input configuration.
#[test]
fn doctor_preserves_target_and_config_files_without_modification() {
    if std::env::var("CCYNC_TEST_DOCTOR_CHILD").as_deref() == Ok("read_only") {
        let home = std::env::var("CCYNC_TEST_HOME").expect("child must receive fixture home");
        isolate_home_child(Path::new(&home));
        let _ = cmd_doctor(&["doctor".to_string()]);
        return;
    }

    let _lock = crate::test_io_lock();
    let home = tempdir().unwrap();
    let ccync_dir = home.path().join(".ccync");
    fs::create_dir_all(&ccync_dir).unwrap();
    let config_bytes = br#"{"selectedProjectionTargets":["codex"]}"#;
    fs::write(ccync_dir.join("config.json"), config_bytes).unwrap();
    let agents_bytes = b"# Team instructions\n";
    fs::write(ccync_dir.join("AGENTS.md"), agents_bytes).unwrap();

    let claude_dir = home.path().join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    let claude_bytes = b"# User instructions\n";
    fs::write(claude_dir.join("CLAUDE.md"), claude_bytes).unwrap();

    let _ = run_doctor_child(
        "doctor_preserves_target_and_config_files_without_modification",
        "read_only",
        home.path(),
    );

    assert_eq!(
        fs::read(ccync_dir.join("config.json")).unwrap(),
        config_bytes,
        "doctor must not modify config.json"
    );
    assert_eq!(
        fs::read(ccync_dir.join("AGENTS.md")).unwrap(),
        agents_bytes,
        "doctor must not modify AGENTS.md"
    );
    assert_eq!(
        fs::read(claude_dir.join("CLAUDE.md")).unwrap(),
        claude_bytes,
        "doctor must not modify target instruction files"
    );
}

/// Acceptance Probe:
/// `ccync doctor` must report an unresolved secret in the managed MCP
/// projection without turning the doctor command into an error.
#[test]
fn doctor_reports_unresolved_mcp_secret_from_managed_projection() {
    if std::env::var("CCYNC_TEST_DOCTOR_CHILD").as_deref() == Ok("mcp_projection") {
        let home = std::env::var("CCYNC_TEST_HOME").expect("child must receive fixture home");
        isolate_home_child(Path::new(&home));
        unsafe {
            std::env::remove_var("CCYNC_SECRET_ALPHA_API_TOKEN");
        }
        assert_eq!(cmd_doctor(&["doctor".to_string()]), ExitCode::Success);
        return;
    }

    let _lock = crate::test_io_lock();
    let home = tempdir().unwrap();
    let mcp_dir = home.path().join(".ccync").join("build").join("mcp");
    fs::create_dir_all(&mcp_dir).unwrap();
    let render_root = home.path().join(".ccync").join("build").join("render");
    fs::create_dir_all(&render_root).unwrap();
    fs::write(
        home.path().join(".ccync").join("config.json"),
        r#"{"selectedProjectionTargets":["codex"]}"#,
    )
    .unwrap();
    let skill_parent = home.path().join(".claude").join("skills");
    fs::create_dir_all(&skill_parent).unwrap();
    let skill_link = skill_parent.join("ccync");
    if ccync_foundation::platform::create_symlink_dir(&render_root, &skill_link).is_err() {
        #[cfg(windows)]
        {
            let status = std::process::Command::new("cmd")
                .args([
                    "/C",
                    "mklink",
                    "/J",
                    &skill_link.to_string_lossy(),
                    &render_root.to_string_lossy(),
                ])
                .status()
                .expect("create sandbox Claude skill junction");
            assert!(status.success(), "create sandbox Claude skill junction");
        }
        #[cfg(not(windows))]
        panic!("create sandbox Claude skill link");
    }
    fs::write(
        mcp_dir.join("managed.json"),
        r#"{
            "servers": {
                "alpha": {
                    "command": "run",
                    "args": ["${CCYNC_SECRET_ALPHA_API_TOKEN}"]
                }
            }
        }"#,
    )
    .unwrap();

    let (exit, stdout) = run_doctor_child(
        "doctor_reports_unresolved_mcp_secret_from_managed_projection",
        "mcp_projection",
        home.path(),
    );

    assert_eq!(
        exit,
        ExitCode::Success,
        "doctor child failed: stdout={stdout}"
    );
    assert!(
        stdout.contains("WARNING") && stdout.contains("alpha"),
        "doctor must report an unresolved MCP warning for alpha: stdout={stdout}"
    );
}

/// Shared child entry: runs `cmd_doctor` inside an isolated home and prints the
/// aggregate exit so the parent can assert on output and exit together.
#[test]
fn doctor_block_scenario_child() {
    if std::env::var("CCYNC_TEST_DOCTOR_CHILD").as_deref() != Ok("scenario") {
        return;
    }
    let home = std::env::var("CCYNC_TEST_HOME").expect("child must receive fixture home");
    isolate_home_child(Path::new(&home));
    let code = cmd_doctor(&["doctor".to_string()]);
    println!(
        "DOCTOR_EXIT={}",
        if code == ExitCode::Success { 0 } else { 1 }
    );
}

#[test]
fn doctor_reports_legacy_residue_when_config_is_rejected() {
    let _lock = crate::test_io_lock();
    let cases: [&[u8]; 3] = [
        b"{not json",
        br#"{"selectedProjectionTargets":["codex"],"selectedRuntimes":["codex"]}"#,
        br#"{"selectedProjectionTargets":["bogus"]}"#,
    ];
    let recovery = "Run `ccync sync --instructions-only --yes` when Claude is selected. Otherwise remove the content between the two ccync markers, including the markers, by hand, or run `ccync uninstall`.";

    for config in cases {
        let home = tempdir().unwrap();
        let ccync_dir = home.path().join(".ccync");
        fs::create_dir_all(&ccync_dir).unwrap();
        fs::write(ccync_dir.join("config.json"), config).unwrap();
        fs::write(ccync_dir.join("AGENTS.md"), b"instructions\n").unwrap();
        let claude_dir = home.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();
        let residue = b"<!-- ccync:managed:begin -->\nlegacy\n<!-- ccync:managed:end -->\n";
        fs::write(claude_dir.join("CLAUDE.md"), residue).unwrap();

        let (_, stdout) = run_doctor_child("doctor_block_scenario_child", "scenario", home.path());

        assert_eq!(doctor_exit(&stdout), 1, "stdout={stdout}");
        assert!(
            stdout.contains("instruction config rejected"),
            "stdout={stdout}"
        );
        assert!(
            stdout.contains("[WARNING]") && stdout.contains("legacy Claude managed block"),
            "stdout={stdout}"
        );
        assert!(stdout.contains(recovery), "stdout={stdout}");
        assert_eq!(fs::read(claude_dir.join("CLAUDE.md")).unwrap(), residue);
    }
}

fn doctor_exit(stdout: &str) -> i32 {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("DOCTOR_EXIT="))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or_else(|| panic!("child printed no exit marker: {stdout}"))
}

/// Build a fixture home, run the scenario child, and assert nothing was written.
fn run_scenario(config: Option<&[u8]>, codex: Option<&[u8]>) -> (i32, String) {
    let _lock = crate::test_io_lock();
    let home = tempdir().unwrap();
    let ccync_dir = home.path().join(".ccync");
    fs::create_dir_all(&ccync_dir).unwrap();
    if let Some(bytes) = config {
        fs::write(ccync_dir.join("config.json"), bytes).unwrap();
    }
    let agents_bytes = b"# Team instructions\n";
    fs::write(ccync_dir.join("AGENTS.md"), agents_bytes).unwrap();
    let codex_dir = home.path().join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    if let Some(bytes) = codex {
        fs::write(codex_dir.join("AGENTS.md"), bytes).unwrap();
    }

    let (_, stdout) = run_doctor_child("doctor_block_scenario_child", "scenario", home.path());

    match config {
        Some(bytes) => assert_eq!(fs::read(ccync_dir.join("config.json")).unwrap(), bytes),
        None => assert!(
            !ccync_dir.join("config.json").exists(),
            "doctor must not create config"
        ),
    }
    assert_eq!(fs::read(ccync_dir.join("AGENTS.md")).unwrap(), agents_bytes);
    match codex {
        Some(bytes) => assert_eq!(fs::read(codex_dir.join("AGENTS.md")).unwrap(), bytes),
        None => assert!(
            !codex_dir.join("AGENTS.md").exists(),
            "doctor must not create targets"
        ),
    }

    (doctor_exit(&stdout), stdout)
}

fn assert_config_rejected(config: Option<&[u8]>, needle: &str) {
    let (exit, stdout) = run_scenario(config, Some(b"# User instructions\n"));
    assert_eq!(exit, 1, "rejected config must exit with error: {stdout}");
    assert!(stdout.contains("[ERROR]"), "stdout={stdout}");
    assert!(
        stdout.contains("instruction config rejected"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains(needle),
        "missing `{needle}` in stdout={stdout}"
    );
    assert!(
        !stdout.contains("managed block"),
        "health must not be evaluated after config rejection: {stdout}"
    );
}

#[test]
fn doctor_block_rejects_missing_config() {
    assert_config_rejected(None, "could not read config");
}

#[test]
fn doctor_block_rejects_malformed_config() {
    assert_config_rejected(Some(b"{not json"), "invalid config JSON");
}

#[test]
fn doctor_block_rejects_unreadable_config() {
    // A directory at the config path cannot be read as a file.
    let _lock = crate::test_io_lock();
    let home = tempdir().unwrap();
    let ccync_dir = home.path().join(".ccync");
    fs::create_dir_all(ccync_dir.join("config.json")).unwrap();
    fs::write(ccync_dir.join("AGENTS.md"), "# Team instructions\n").unwrap();
    let (_, stdout) = run_doctor_child("doctor_block_scenario_child", "scenario", home.path());
    assert_eq!(doctor_exit(&stdout), 1, "stdout={stdout}");
    assert!(stdout.contains("could not read config"), "stdout={stdout}");
    assert!(!stdout.contains("managed block"), "stdout={stdout}");
    assert!(ccync_dir.join("config.json").is_dir());
}

#[test]
fn doctor_block_rejects_all_invalid_selection() {
    assert_config_rejected(
        Some(br#"{"selectedProjectionTargets":["nope","bogus"]}"#),
        "nope, bogus",
    );
}

#[test]
fn doctor_block_rejects_mixed_valid_and_invalid_selection() {
    assert_config_rejected(
        Some(br#"{"selectedProjectionTargets":["codex","nope"]}"#),
        "nope",
    );
}

#[test]
fn doctor_block_rejects_both_selection_keys() {
    assert_config_rejected(
        Some(br#"{"selectedProjectionTargets":["codex"],"selectedRuntimes":["codex"]}"#),
        "both selectedRuntimes and selectedProjectionTargets",
    );
}

#[test]
fn doctor_block_error_health_exits_nonzero_with_recovery() {
    let (exit, stdout) = run_scenario(
        Some(br#"{"selectedProjectionTargets":["codex"]}"#),
        Some(b"<!-- ccync:managed:begin -->\nincomplete body\n"),
    );
    assert_eq!(exit, 1, "stdout={stdout}");
    assert!(
        stdout.contains("[ERROR]") && stdout.contains("managed block"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains(" \u{2192} "),
        "recovery hint missing: {stdout}"
    );
}

#[test]
fn doctor_block_valid_config_reports_absent_block() {
    let (_exit, stdout) = run_scenario(
        Some(br#"{"selectedProjectionTargets":["codex"]}"#),
        Some(b"# User instructions\n"),
    );
    assert!(
        !stdout.contains("instruction config rejected"),
        "stdout={stdout}"
    );
    assert!(stdout.contains("managed block"), "stdout={stdout}");
}

#[test]
fn doctor_block_drift_exits_nonzero_with_recovery() {
    let drifted_content =
        b"<!-- ccync:managed:begin -->\n# Drifted instructions\n<!-- ccync:managed:end -->\n";
    let (exit, stdout) = run_scenario(
        Some(br#"{"selectedProjectionTargets":["codex"]}"#),
        Some(drifted_content),
    );
    assert_eq!(exit, 1, "drifted block must exit with error: {stdout}");
    assert!(
        stdout.contains("[ERROR]") && stdout.contains("managed block"),
        "stdout={stdout}"
    );
    assert!(stdout.contains("differs"), "stdout={stdout}");
    assert!(
        stdout.contains(" \u{2192} ") && stdout.contains("ccync sync"),
        "recovery hint missing: {stdout}"
    );
}

#[test]
fn doctor_block_residual_block_warns_with_recovery() {
    let _lock = crate::test_io_lock();
    let home = tempdir().unwrap();
    setup_healthy_base(home.path());
    let ccync_dir = home.path().join(".ccync");
    fs::create_dir_all(&ccync_dir).unwrap();
    let config_bytes = br#"{"selectedProjectionTargets":["claude"]}"#;
    fs::write(ccync_dir.join("config.json"), config_bytes).unwrap();
    fs::write(ccync_dir.join("AGENTS.md"), b"").unwrap();

    let claude_dir = home.path().join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    let claude_bytes =
        b"<!-- ccync:managed:begin -->\n# Residual block\n<!-- ccync:managed:end -->\n";
    fs::write(claude_dir.join("CLAUDE.md"), claude_bytes).unwrap();

    let (_, stdout) = run_doctor_child("doctor_block_scenario_child", "scenario", home.path());

    assert_eq!(
        fs::read(ccync_dir.join("config.json")).unwrap(),
        config_bytes
    );
    assert_eq!(fs::read(ccync_dir.join("AGENTS.md")).unwrap(), b"");
    assert_eq!(
        fs::read(claude_dir.join("CLAUDE.md")).unwrap(),
        claude_bytes
    );

    let exit = doctor_exit(&stdout);
    assert_eq!(
        exit, 0,
        "residual block warning must exit successfully: {stdout}"
    );
    assert!(
        stdout.contains("[WARNING]") && stdout.contains("legacy Claude managed block"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains(" \u{2192} ")
            && stdout.contains("ccync sync --instructions-only --yes")
            && stdout.contains("ccync uninstall"),
        "recovery hint missing: {stdout}"
    );
    assert!(
        !stdout.contains("delete"),
        "recovery must not suggest deletion: {stdout}"
    );
}

#[test]
fn doctor_block_malformed_legacy_block_errors_without_writing() {
    let _lock = crate::test_io_lock();
    let home = tempdir().unwrap();
    let ccync_dir = home.path().join(".ccync");
    fs::create_dir_all(&ccync_dir).unwrap();
    fs::write(
        ccync_dir.join("config.json"),
        br#"{"selectedProjectionTargets":["codex"]}"#,
    )
    .unwrap();
    let claude_dir = home.path().join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    let legacy_path = claude_dir.join("CLAUDE.md");
    let malformed = b"<!-- ccync:managed:begin -->\nincomplete\n";
    fs::write(&legacy_path, malformed).unwrap();

    let (_, stdout) = run_doctor_child("doctor_block_scenario_child", "scenario", home.path());

    assert_eq!(doctor_exit(&stdout), 1, "stdout={stdout}");
    assert!(
        stdout.contains("[ERROR]") && stdout.contains("legacy Claude managed block is malformed"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains(&legacy_path.display().to_string()),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("repair or remove the ccync markers"),
        "stdout={stdout}"
    );
    assert_eq!(fs::read(legacy_path).unwrap(), malformed);
}

#[cfg(unix)]
#[test]
fn doctor_block_dangling_legacy_link_preserves_link_and_recovers_access() {
    let _lock = crate::test_io_lock();
    let home = tempdir().unwrap();
    setup_healthy_base(home.path());
    let ccync_dir = home.path().join(".ccync");
    fs::create_dir_all(&ccync_dir).unwrap();
    fs::write(
        ccync_dir.join("config.json"),
        br#"{"selectedProjectionTargets":["codex"]}"#,
    )
    .unwrap();
    let claude_dir = home.path().join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    let legacy_path = claude_dir.join("CLAUDE.md");
    let missing_target = home.path().join("missing_legacy_target");
    std::os::unix::fs::symlink(&missing_target, &legacy_path).unwrap();

    let (_, stdout) = run_doctor_child("doctor_block_scenario_child", "scenario", home.path());

    assert_eq!(doctor_exit(&stdout), 1, "stdout={stdout}");
    assert!(
        stdout.contains("[ERROR]") && stdout.contains("dangling symbolic link"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("preserve the existing symbolic link")
            && stdout.contains("manually inspect its missing target and access")
            && stdout.contains("restore access, then rerun `ccync doctor`"),
        "recovery hint missing: {stdout}"
    );
    assert!(fs::symlink_metadata(&legacy_path)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_link(&legacy_path).unwrap(), missing_target);
}

fn setup_healthy_base(home: &Path) {
    let render_root = home.join(".ccync").join("build").join("render");
    fs::create_dir_all(&render_root).unwrap();
    let skill_parent = home.join(".claude").join("skills");
    fs::create_dir_all(&skill_parent).unwrap();
    let skill_link = skill_parent.join("ccync");
    if ccync_foundation::platform::create_symlink_dir(&render_root, &skill_link).is_err() {
        #[cfg(windows)]
        {
            let status = std::process::Command::new("cmd")
                .args([
                    "/C",
                    "mklink",
                    "/J",
                    &skill_link.to_string_lossy(),
                    &render_root.to_string_lossy(),
                ])
                .status()
                .expect("create sandbox Claude skill junction");
            assert!(status.success(), "create sandbox Claude skill junction");
        }
        #[cfg(not(windows))]
        panic!("create sandbox Claude skill link");
    }
}

#[test]
fn doctor_block_stale_deselected_warning_alone_exits_zero() {
    let _lock = crate::test_io_lock();
    let home = tempdir().unwrap();
    setup_healthy_base(home.path());
    let ccync_dir = home.path().join(".ccync");
    fs::create_dir_all(&ccync_dir).unwrap();
    let config_bytes = br#"{"selectedProjectionTargets":["claude"]}"#;
    fs::write(ccync_dir.join("config.json"), config_bytes).unwrap();
    let agents_bytes = b"instructions";
    fs::write(ccync_dir.join("AGENTS.md"), agents_bytes).unwrap();

    let claude_dir = home.path().join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    let claude_bytes = b"<!-- ccync:managed:begin -->\ninstructions\n<!-- ccync:managed:end -->\n";
    fs::write(claude_dir.join("CLAUDE.md"), claude_bytes).unwrap();

    let codex_dir = home.path().join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    let codex_bytes = b"<!-- ccync:managed:begin -->\nstale\n<!-- ccync:managed:end -->\n";
    fs::write(codex_dir.join("AGENTS.md"), codex_bytes).unwrap();

    let (_, stdout) = run_doctor_child("doctor_block_scenario_child", "scenario", home.path());

    assert_eq!(
        fs::read(ccync_dir.join("config.json")).unwrap(),
        config_bytes
    );
    assert_eq!(fs::read(ccync_dir.join("AGENTS.md")).unwrap(), agents_bytes);
    assert_eq!(
        fs::read(claude_dir.join("CLAUDE.md")).unwrap(),
        claude_bytes
    );
    assert_eq!(fs::read(codex_dir.join("AGENTS.md")).unwrap(), codex_bytes);

    let exit = doctor_exit(&stdout);
    assert_eq!(exit, 0, "warning alone must exit with success: {stdout}");
    assert!(stdout.contains("[WARNING]"), "stdout={stdout}");
    assert!(stdout.contains("stale"), "stdout={stdout}");
    assert!(!stdout.contains("[ERROR]"), "stdout={stdout}");
}

#[test]
fn doctor_block_codex_override_warning_alone_exits_zero() {
    let _lock = crate::test_io_lock();
    let home = tempdir().unwrap();
    setup_healthy_base(home.path());
    let ccync_dir = home.path().join(".ccync");
    fs::create_dir_all(&ccync_dir).unwrap();
    let config_bytes = br#"{"selectedProjectionTargets":["claude","codex"]}"#;
    fs::write(ccync_dir.join("config.json"), config_bytes).unwrap();
    let agents_bytes = b"instructions";
    fs::write(ccync_dir.join("AGENTS.md"), agents_bytes).unwrap();

    let claude_dir = home.path().join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    let claude_bytes = b"<!-- ccync:managed:begin -->\ninstructions\n<!-- ccync:managed:end -->\n";
    fs::write(claude_dir.join("CLAUDE.md"), claude_bytes).unwrap();

    let codex_dir = home.path().join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    let codex_bytes = b"<!-- ccync:managed:begin -->\ninstructions\n<!-- ccync:managed:end -->\n";
    fs::write(codex_dir.join("AGENTS.md"), codex_bytes).unwrap();

    let override_bytes = b"# User local override\n";
    fs::write(codex_dir.join("AGENTS.override.md"), override_bytes).unwrap();

    let (_, stdout) = run_doctor_child("doctor_block_scenario_child", "scenario", home.path());

    assert_eq!(
        fs::read(ccync_dir.join("config.json")).unwrap(),
        config_bytes
    );
    assert_eq!(fs::read(ccync_dir.join("AGENTS.md")).unwrap(), agents_bytes);
    assert_eq!(
        fs::read(claude_dir.join("CLAUDE.md")).unwrap(),
        claude_bytes
    );
    assert_eq!(fs::read(codex_dir.join("AGENTS.md")).unwrap(), codex_bytes);
    assert_eq!(
        fs::read(codex_dir.join("AGENTS.override.md")).unwrap(),
        override_bytes
    );

    let exit = doctor_exit(&stdout);
    assert_eq!(exit, 0, "warning alone must exit with success: {stdout}");
    assert!(stdout.contains("[WARNING]"), "stdout={stdout}");
    assert!(stdout.contains("shadowed"), "stdout={stdout}");
    assert!(!stdout.contains("[ERROR]"), "stdout={stdout}");
}

#[test]
fn doctor_block_unreadable_source_exits_nonzero_with_recovery() {
    let _lock = crate::test_io_lock();
    let home = tempdir().unwrap();
    let ccync_dir = home.path().join(".ccync");
    fs::create_dir_all(&ccync_dir).unwrap();
    let config_bytes = br#"{"selectedProjectionTargets":["codex"]}"#;
    fs::write(ccync_dir.join("config.json"), config_bytes).unwrap();
    fs::create_dir_all(ccync_dir.join("AGENTS.md")).unwrap();

    let codex_dir = home.path().join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    let codex_bytes = b"# User instructions\n";
    fs::write(codex_dir.join("AGENTS.md"), codex_bytes).unwrap();

    let (_, stdout) = run_doctor_child("doctor_block_scenario_child", "scenario", home.path());

    assert_eq!(
        fs::read(ccync_dir.join("config.json")).unwrap(),
        config_bytes
    );
    assert!(ccync_dir.join("AGENTS.md").is_dir());
    assert_eq!(fs::read(codex_dir.join("AGENTS.md")).unwrap(), codex_bytes);

    let exit = doctor_exit(&stdout);
    assert_eq!(exit, 1, "unreadable source must exit with error: {stdout}");
    assert!(
        stdout.contains("[ERROR]") && stdout.contains("instruction source cannot be read"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains(" \u{2192} ")
            && stdout.contains("check the instruction source file permissions"),
        "recovery hint missing: {stdout}"
    );
}

#[test]
fn doctor_block_invalid_utf8_source_exits_nonzero_with_recovery() {
    let _lock = crate::test_io_lock();
    let home = tempdir().unwrap();
    let ccync_dir = home.path().join(".ccync");
    fs::create_dir_all(&ccync_dir).unwrap();
    let config_bytes = br#"{"selectedProjectionTargets":["codex"]}"#;
    fs::write(ccync_dir.join("config.json"), config_bytes).unwrap();
    let invalid_utf8 = [0xff, 0xfe, 0xfd];
    fs::write(ccync_dir.join("AGENTS.md"), invalid_utf8).unwrap();

    let codex_dir = home.path().join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    let codex_bytes = b"# User instructions\n";
    fs::write(codex_dir.join("AGENTS.md"), codex_bytes).unwrap();

    let (_, stdout) = run_doctor_child("doctor_block_scenario_child", "scenario", home.path());

    assert_eq!(
        fs::read(ccync_dir.join("config.json")).unwrap(),
        config_bytes
    );
    assert_eq!(
        fs::read(ccync_dir.join("AGENTS.md")).unwrap(),
        &invalid_utf8
    );
    assert_eq!(fs::read(codex_dir.join("AGENTS.md")).unwrap(), codex_bytes);

    let exit = doctor_exit(&stdout);
    assert_eq!(
        exit, 1,
        "invalid UTF-8 source must exit with error: {stdout}"
    );
    assert!(
        stdout.contains("[ERROR]") && stdout.contains("UTF-8"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains(" \u{2192} ") && stdout.contains("re-save the instruction source as UTF-8"),
        "recovery hint missing: {stdout}"
    );
}

#[test]
fn doctor_block_unreadable_target_exits_nonzero_with_recovery() {
    let _lock = crate::test_io_lock();
    let home = tempdir().unwrap();
    let ccync_dir = home.path().join(".ccync");
    fs::create_dir_all(&ccync_dir).unwrap();
    let config_bytes = br#"{"selectedProjectionTargets":["codex"]}"#;
    fs::write(ccync_dir.join("config.json"), config_bytes).unwrap();
    let agents_bytes = b"# Team instructions\n";
    fs::write(ccync_dir.join("AGENTS.md"), agents_bytes).unwrap();

    let codex_target = home.path().join(".codex").join("AGENTS.md");
    fs::create_dir_all(&codex_target).unwrap();

    let (_, stdout) = run_doctor_child("doctor_block_scenario_child", "scenario", home.path());

    assert_eq!(
        fs::read(ccync_dir.join("config.json")).unwrap(),
        config_bytes
    );
    assert_eq!(fs::read(ccync_dir.join("AGENTS.md")).unwrap(), agents_bytes);
    assert!(codex_target.is_dir());

    let exit = doctor_exit(&stdout);
    assert_eq!(exit, 1, "unreadable target must exit with error: {stdout}");
    assert!(
        stdout.contains("[ERROR]") && stdout.contains("cannot be read"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains(" \u{2192} ") && stdout.contains("check the target file permissions"),
        "recovery hint missing: {stdout}"
    );
}

#[test]
fn doctor_block_unreadable_codex_override_exits_nonzero_with_recovery() {
    let _lock = crate::test_io_lock();
    let home = tempdir().unwrap();
    let ccync_dir = home.path().join(".ccync");
    fs::create_dir_all(&ccync_dir).unwrap();
    let config_bytes = br#"{"selectedProjectionTargets":["codex"]}"#;
    fs::write(ccync_dir.join("config.json"), config_bytes).unwrap();
    let agents_bytes = b"# Team instructions\n";
    fs::write(ccync_dir.join("AGENTS.md"), agents_bytes).unwrap();

    let codex_dir = home.path().join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    let codex_bytes =
        b"<!-- ccync:managed:begin -->\n# Team instructions\n<!-- ccync:managed:end -->\n";
    fs::write(codex_dir.join("AGENTS.md"), codex_bytes).unwrap();

    let override_path = codex_dir.join("AGENTS.override.md");
    fs::create_dir_all(&override_path).unwrap();

    let (_, stdout) = run_doctor_child("doctor_block_scenario_child", "scenario", home.path());

    assert_eq!(
        fs::read(ccync_dir.join("config.json")).unwrap(),
        config_bytes
    );
    assert_eq!(fs::read(ccync_dir.join("AGENTS.md")).unwrap(), agents_bytes);
    assert_eq!(fs::read(codex_dir.join("AGENTS.md")).unwrap(), codex_bytes);
    assert!(override_path.is_dir());

    let exit = doctor_exit(&stdout);
    assert_eq!(
        exit, 1,
        "unreadable override must exit with error: {stdout}"
    );
    assert!(
        stdout.contains("[ERROR]") && stdout.contains("override cannot be read"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains(" \u{2192} ")
            && stdout.contains("check the override file permissions or remove it"),
        "recovery hint missing: {stdout}"
    );
}

#[cfg(unix)]
#[test]
fn doctor_block_dangling_source_link_exits_nonzero_with_recovery() {
    let _lock = crate::test_io_lock();
    let home = tempdir().unwrap();
    let ccync_dir = home.path().join(".ccync");
    fs::create_dir_all(&ccync_dir).unwrap();
    let config_bytes = br#"{"selectedProjectionTargets":["codex"]}"#;
    fs::write(ccync_dir.join("config.json"), config_bytes).unwrap();
    std::os::unix::fs::symlink(
        home.path().join("nonexistent_source"),
        ccync_dir.join("AGENTS.md"),
    )
    .unwrap();

    let codex_dir = home.path().join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    let codex_bytes = b"# User instructions\n";
    fs::write(codex_dir.join("AGENTS.md"), codex_bytes).unwrap();

    let (_, stdout) = run_doctor_child("doctor_block_scenario_child", "scenario", home.path());
    let exit = doctor_exit(&stdout);
    assert_eq!(
        exit, 1,
        "dangling source link must exit with error: {stdout}"
    );
    assert!(
        stdout.contains("[ERROR]") && stdout.contains("dangling symbolic link"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains(" \u{2192} ") && stdout.contains("remove or repair the symbolic link"),
        "recovery hint missing: {stdout}"
    );
}

#[cfg(unix)]
#[test]
fn doctor_block_dangling_target_link_exits_nonzero_with_recovery() {
    let _lock = crate::test_io_lock();
    let home = tempdir().unwrap();
    let ccync_dir = home.path().join(".ccync");
    fs::create_dir_all(&ccync_dir).unwrap();
    let config_bytes = br#"{"selectedProjectionTargets":["codex"]}"#;
    fs::write(ccync_dir.join("config.json"), config_bytes).unwrap();
    let agents_bytes = b"instructions";
    fs::write(ccync_dir.join("AGENTS.md"), agents_bytes).unwrap();

    let codex_dir = home.path().join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    std::os::unix::fs::symlink(
        home.path().join("nonexistent_target"),
        codex_dir.join("AGENTS.md"),
    )
    .unwrap();

    let (_, stdout) = run_doctor_child("doctor_block_scenario_child", "scenario", home.path());
    let exit = doctor_exit(&stdout);
    assert_eq!(
        exit, 1,
        "dangling target link must exit with error: {stdout}"
    );
    assert!(
        stdout.contains("[ERROR]") && stdout.contains("dangling symbolic link"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains(" \u{2192} ") && stdout.contains("remove or repair the symbolic link"),
        "recovery hint missing: {stdout}"
    );
}
