//! Acceptance probes for the managed instruction block in the projection core.

use super::lifecycle::run_projection_core;
use ccync_engine::ExitCode;
use ccync_foundation::config::CcyncConfig;
use ccync_foundation::paths::user_scope_instruction_path;
use ccync_foundation::target::ProjectionTargetId;
use std::path::{Path, PathBuf};

fn isolate_home(tmp: &Path) -> Vec<(String, Option<String>)> {
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
    saved
}

fn restore_home(saved: &[(String, Option<String>)]) {
    for (key, value) in saved {
        unsafe {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn config_for(targets: &[&str]) -> CcyncConfig {
    let mut config = CcyncConfig::default();
    config.selected_projection_targets =
        targets.iter().map(|target| (*target).to_string()).collect();
    config
}

fn target_path(target: ProjectionTargetId) -> PathBuf {
    user_scope_instruction_path(target).expect("isolated test home must resolve target path")
}

#[test]
fn projection_core_writes_input_verbatim_into_selected_target_block() {
    let _lock = crate::test_io_lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let saved = isolate_home(tmp.path());

    let input = "# Team rules\nUse the repository formatter.\n\nKeep this newline.";
    let input_path = tmp.path().join(".ccync").join("AGENTS.md");
    std::fs::create_dir_all(input_path.parent().unwrap()).unwrap();
    std::fs::write(&input_path, input).unwrap();
    let target = target_path(ProjectionTargetId::Claude);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, "user-authored heading\n").unwrap();

    let _result = run_projection_core(&config_for(&["claude"]), "ccync sync");
    let projected = std::fs::read_to_string(&target).unwrap();

    restore_home(&saved);

    assert!(projected.contains("<!-- ccync:managed:begin -->"));
    assert!(projected.contains(input));
    assert!(projected.contains("<!-- ccync:managed:end -->"));
    assert!(projected.starts_with("user-authored heading\n"));
}

#[test]
fn projection_core_prunes_selected_block_when_input_is_absent() {
    let _lock = crate::test_io_lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let saved = isolate_home(tmp.path());
    let target = target_path(ProjectionTargetId::Claude);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(
        &target,
        "before\n<!-- ccync:managed:begin -->\nold rules\n<!-- ccync:managed:end -->\nafter\n",
    )
    .unwrap();

    let _result = run_projection_core(&config_for(&["claude"]), "ccync sync");
    let projected = std::fs::read_to_string(&target).unwrap();

    restore_home(&saved);

    assert_eq!(projected, "before\nafter\n");
}

#[test]
fn projection_core_does_not_resolve_or_modify_deselected_target() {
    let _lock = crate::test_io_lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let saved = isolate_home(tmp.path());
    let target = target_path(ProjectionTargetId::Codex);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    let original = "stale\n<!-- ccync:managed:begin -->\nold rules\n<!-- ccync:managed:end -->\n";
    std::fs::write(&target, original).unwrap();

    let _result = run_projection_core(&config_for(&["claude"]), "ccync sync");
    let projected = std::fs::read_to_string(&target).unwrap();

    restore_home(&saved);

    assert_eq!(projected, original);
}

/// Integration coverage: a damaged managed block on one selected target
/// is reported and left byte-identical, the run still projects the other
/// selected target's clean file, and the run's own `ExitCode` is `Error` —
/// exercised through `run_projection_core`, the production caller, rather
/// than through `write_block`'s own lower-level damaged-file unit test.
#[test]
fn projection_core_reports_damaged_target_projects_others_and_exits_error() {
    let _lock = crate::test_io_lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let saved = isolate_home(tmp.path());

    let input = "# Team rules\n";
    let input_path = tmp.path().join(".ccync").join("AGENTS.md");
    std::fs::create_dir_all(input_path.parent().unwrap()).unwrap();
    std::fs::write(&input_path, input).unwrap();

    let damaged_target = target_path(ProjectionTargetId::Claude);
    std::fs::create_dir_all(damaged_target.parent().unwrap()).unwrap();
    let damaged_bytes =
        "<!-- ccync:managed:begin -->\nfirst\n<!-- ccync:managed:begin -->\nsecond\n<!-- ccync:managed:end -->\n";
    std::fs::write(&damaged_target, damaged_bytes).unwrap();

    let clean_target = target_path(ProjectionTargetId::Codex);
    std::fs::create_dir_all(clean_target.parent().unwrap()).unwrap();
    std::fs::write(&clean_target, "user-authored heading\n").unwrap();

    let exit = run_projection_core(&config_for(&["claude", "codex"]), "ccync sync");
    let damaged_after = std::fs::read_to_string(&damaged_target).unwrap();
    let clean_after = std::fs::read_to_string(&clean_target).unwrap();

    restore_home(&saved);

    assert_eq!(
        exit,
        ExitCode::Error,
        "a run with one damaged managed-block target must exit Error"
    );
    assert_eq!(
        damaged_after, damaged_bytes,
        "a damaged target file must be left byte-identical, never repaired or overwritten"
    );
    assert!(
        clean_after.contains("<!-- ccync:managed:begin -->") && clean_after.contains(input),
        "the other selected target must still be projected despite the sibling's damage"
    );
}
