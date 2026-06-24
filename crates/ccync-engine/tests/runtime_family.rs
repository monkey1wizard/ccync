//! Install behavior test (post-carve).
//!
//! ccync `install` no longer bakes ccync-core content; it resolves + creates the
//! canonical root, projects managed surfaces (best-effort), and records machine
//! state. Acceptance is "runs to completion + canonical root exists + report
//! returned" (plan D7) — not ccync-core render output.

use ccync_engine::config::CcyncConfig;
use ccync_engine::install::run_install;
use serde_json::json;
use std::fs;
use tempfile::TempDir;

#[test]
fn run_install_creates_canonical_root_and_returns_report() {
    let fake_home = TempDir::new().unwrap();

    #[cfg(windows)]
    unsafe {
        std::env::set_var("USERPROFILE", fake_home.path());
        std::env::set_var("HOME", fake_home.path());
    }

    #[cfg(not(windows))]
    unsafe {
        std::env::set_var("HOME", fake_home.path());
    }

    let resolved_home =
        ccync_foundation::paths::user_home().expect("home_dir should resolve in isolated test");
    assert_eq!(
        resolved_home,
        fake_home.path(),
        "ccync_foundation::paths::user_home() must point at the isolated test home"
    );

    let ccync_home = fake_home.path().join(".ccync");
    fs::create_dir_all(ccync_home.join("config")).unwrap();
    fs::write(
        ccync_home.join("install-state.json"),
        serde_json::to_string_pretty(&json!({
            "selectedRuntimes": ["copilot", "claude"],
            "primaryRuntime": "copilot"
        }))
        .unwrap(),
    )
    .unwrap();

    let config = CcyncConfig {
        dev_mode: None,
        ccync_root: None,
        install_mode: None,
    };

    let report = run_install(&config).expect("run_install should succeed in isolated home");
    // ccync install resolves + creates the canonical root (no ccync-core bake).
    assert!(
        report.canonical_root.exists(),
        "canonical root dir must exist"
    );
    // Install ran to completion and produced a report with at least one runtime.
    assert!(
        !report.runtimes.is_empty(),
        "install report must list the runtimes it projected to"
    );
}
