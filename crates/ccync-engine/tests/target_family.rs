//! Install behavior test.
//!
//! ccync `install` resolves + creates the canonical root, projects managed
//! surfaces (best-effort), and records machine state. Acceptance is "runs to
//! completion + canonical root exists + report returned" — not ccync-core
//! render output.

use ccync_engine::config::CcyncConfig;
use ccync_engine::install::run_update_with_evidence;
use serde_json::json;
use std::fs;
use tempfile::TempDir;

#[test]
fn run_update_with_evidence_creates_canonical_root_and_returns_evidence() {
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
            "selectedProjectionTargets": ["copilot", "claude"]
        }))
        .unwrap(),
    )
    .unwrap();

    let config = CcyncConfig {
        selected_projection_targets: vec![],
        ..CcyncConfig::default()
    };

    let evidence = run_update_with_evidence(&config)
        .expect("run_update_with_evidence should succeed in isolated home");
    // Projection resolves + creates the canonical root (no ccync-core bake) —
    // resolved independently here via the same public path function the real
    // apply path uses internally, since the canonical root is not itself
    // part of the returned evidence.
    let canonical_root = ccync_foundation::paths::canonical_plugin_root()
        .expect("canonical_plugin_root should resolve in the isolated test home");
    assert!(canonical_root.exists(), "canonical root dir must exist");
    // Install ran to completion and produced evidence for at least one target.
    assert!(
        !evidence.completed_targets.is_empty(),
        "evidence must list the targets it projected to"
    );
}
