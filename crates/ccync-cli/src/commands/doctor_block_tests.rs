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
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();
    fs::write(ccync_dir.join("AGENTS.md"), "# Team instructions\n").unwrap();

    let claude_dir = home.path().join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    fs::write(claude_dir.join("CLAUDE.md"), "# User instructions\n").unwrap();

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
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();
    fs::write(ccync_dir.join("AGENTS.md"), "# Team instructions\n").unwrap();

    let claude_dir = home.path().join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    fs::write(
        claude_dir.join("CLAUDE.md"),
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
        r#"{"selectedProjectionTargets":["claude"]}"#,
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
    let config_bytes = br#"{"selectedProjectionTargets":["claude"]}"#;
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
