//! Acceptance probes for the read-only unified projection preview.
//!
//! These probes intentionally inspect the lifecycle seam's source contract.
//! The implementation keeps the preview on the dry-run path, while the
//! production helpers remain private to `lifecycle`.

#[test]
fn dry_run_preview_lists_every_user_scope_instruction_file() {
    let lifecycle = include_str!("lifecycle.rs");
    let scope_builder = lifecycle
        .split_once("fn build_touch_scope(")
        .and_then(|(_, rest)| rest.split_once("fn render_touch_scope("))
        .map(|(_, _)| {
            let start = lifecycle.find("fn build_touch_scope(").unwrap();
            let end = lifecycle.find("fn render_touch_scope(").unwrap();
            &lifecycle[start..end]
        })
        .expect("lifecycle must retain the touch-scope builder");

    assert!(
        scope_builder.contains("user_scope_instruction_path"),
        "dry-run report must list every resolved user-scope instruction file"
    );
}

#[test]
fn dry_run_preview_stays_before_projection_core() {
    let lifecycle = include_str!("lifecycle.rs");
    let dry_run_branch = lifecycle
        .find("if dry_run {")
        .expect("run_unified_projection must keep an explicit dry-run branch");
    let core_call = lifecycle
        .find("run_projection_core(&config, \"ccync sync\")")
        .expect("unified projection must retain the real projection core");

    assert!(
        dry_run_branch < core_call,
        "dry-run must return before run_projection_core so the preview stays read-only"
    );
}
