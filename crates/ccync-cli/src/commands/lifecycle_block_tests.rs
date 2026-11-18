//! Acceptance probes for the managed instruction block in the projection core.

use super::lifecycle::{
    cmd_sync, instruction_paths, legacy_instruction_prune_paths, run_projection_core,
};
use ccync_engine::ExitCode;
use ccync_foundation::config::CcyncConfig;
use ccync_foundation::paths::{
    legacy_claude_user_instruction_path, user_scope_instruction_write_path,
};
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
    user_scope_instruction_write_path(target).expect("isolated test home must resolve target path")
}

fn legacy_path() -> PathBuf {
    legacy_claude_user_instruction_path()
        .expect("isolated test home must resolve the legacy Claude path")
}

#[test]
fn instruction_only_apply_changes_only_selected_instruction_and_preserves_unrelated_state() {
    let _lock = crate::test_io_lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let saved = isolate_home(tmp.path());
    let ccync_home = tmp.path().join(".ccync");
    std::fs::create_dir_all(&ccync_home).unwrap();
    std::fs::write(
        ccync_home.join("config.json"),
        r#"{"selectedProjectionTargets":["codex"]}"#,
    )
    .unwrap();
    let source = ccync_home.join("AGENTS.md");
    std::fs::write(&source, "# Scoped instructions\n").unwrap();
    let lockfile = ccync_home.join("plugins.lock.json");
    let lock_before = b"unrelated-lock-state";
    std::fs::write(&lockfile, lock_before).unwrap();
    let render = ccync_home.join("build").join("render").join("sentinel");
    std::fs::create_dir_all(render.parent().unwrap()).unwrap();
    std::fs::write(&render, b"unrelated-render-state").unwrap();
    let target = target_path(ProjectionTargetId::Codex);

    let exit = cmd_sync(&[
        "sync".to_string(),
        "--instructions-only".to_string(),
        "--yes".to_string(),
    ]);
    let projected = std::fs::read_to_string(&target).unwrap();
    let lock_after = std::fs::read(&lockfile).unwrap();
    let render_after = std::fs::read(&render).unwrap();

    restore_home(&saved);

    assert_eq!(exit, ExitCode::Success);
    assert!(projected.contains("# Scoped instructions"));
    assert_eq!(lock_after, lock_before);
    assert_eq!(render_after, b"unrelated-render-state");
}

#[test]
fn instruction_paths_use_canonical_order_and_dedupe_shared_antigravity_file() {
    let _lock = crate::test_io_lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let saved = isolate_home(tmp.path());
    let paths = instruction_paths(&[
        ProjectionTargetId::AgyGui,
        ProjectionTargetId::Codex,
        ProjectionTargetId::Claude,
        ProjectionTargetId::AgyCli,
        ProjectionTargetId::AgyIde,
    ]);
    restore_home(&saved);

    assert_eq!(paths.len(), 2);
    assert!(paths[0].ends_with(".codex/AGENTS.md"));
    assert!(paths[1].ends_with(".gemini/AGENTS.md"));
    let prune_paths = legacy_instruction_prune_paths(&[ProjectionTargetId::Claude]);
    assert_eq!(prune_paths.len(), 1);
    assert!(prune_paths[0].ends_with(".claude/CLAUDE.md"));
}

#[test]
fn instructions_only_renders_ordered_deduplicated_dry_run_outcomes() {
    let _lock = crate::test_io_lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let saved = isolate_home(tmp.path());
    let ccync_home = tmp.path().join(".ccync");
    std::fs::create_dir_all(&ccync_home).unwrap();
    std::fs::write(
        ccync_home.join("config.json"),
        r#"{"selectedProjectionTargets":["agy-gui","codex","claude","agy-cli"]}"#,
    )
    .unwrap();
    std::fs::write(ccync_home.join("AGENTS.md"), "# Scoped rules\n").unwrap();
    let mut output = Vec::new();

    let exit = super::lifecycle::run_instruction_only_with_output(true, false, &mut output);
    let rendered = String::from_utf8(output).unwrap();
    restore_home(&saved);

    assert_eq!(exit, ExitCode::Success);
    let paths: Vec<_> = rendered
        .lines()
        .filter(|line| line.contains(": create") || line.contains(": append"))
        .collect();
    assert_eq!(paths.len(), 2, "{rendered}");
    assert!(paths[0].contains(".codex"), "{rendered}");
    assert!(paths[1].contains(".gemini"), "{rendered}");
    assert!(
        rendered.contains("legacy instruction files (prune only)"),
        "{rendered}"
    );
    assert!(rendered.contains("CLAUDE.md: unchanged"), "{rendered}");
    assert!(rendered.contains("no files written"), "{rendered}");
}

#[test]
fn instructions_only_renders_all_rejected_values_before_target_access() {
    let _lock = crate::test_io_lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let saved = isolate_home(tmp.path());
    let ccync_home = tmp.path().join(".ccync");
    std::fs::create_dir_all(&ccync_home).unwrap();
    std::fs::write(
        ccync_home.join("config.json"),
        r#"{"selectedProjectionTargets":["unknown-one","unknown-two"]}"#,
    )
    .unwrap();
    let target = target_path(ProjectionTargetId::Codex);
    let mut output = Vec::new();

    let exit = super::lifecycle::run_instruction_only_with_output(true, false, &mut output);
    let rendered = String::from_utf8(output).unwrap();
    let target_exists = target.exists();
    restore_home(&saved);

    assert_eq!(exit, ExitCode::Error);
    assert!(rendered.contains("unknown-one"), "{rendered}");
    assert!(rendered.contains("unknown-two"), "{rendered}");
    assert!(!target_exists);
}

#[test]
fn instructions_only_dry_run_renders_source_blocker_and_no_write_footer() {
    let _lock = crate::test_io_lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let saved = isolate_home(tmp.path());
    let ccync_home = tmp.path().join(".ccync");
    std::fs::create_dir_all(&ccync_home).unwrap();
    std::fs::write(
        ccync_home.join("config.json"),
        r#"{"selectedProjectionTargets":["codex"]}"#,
    )
    .unwrap();
    std::fs::create_dir_all(ccync_home.join("AGENTS.md")).unwrap();
    let mut output = Vec::new();

    let exit = super::lifecycle::run_instruction_only_with_output(true, false, &mut output);
    let rendered = String::from_utf8(output).unwrap();
    let target_exists = target_path(ProjectionTargetId::Codex).exists();
    restore_home(&saved);

    assert_eq!(exit, ExitCode::Error);
    assert!(rendered.contains("blocked: source read"), "{rendered}");
    assert!(rendered.contains("no files written"), "{rendered}");
    assert!(!target_exists);
}

#[test]
fn instructions_only_refusal_points_to_scoped_yes_and_writes_nothing() {
    let _lock = crate::test_io_lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let saved = isolate_home(tmp.path());
    let prior_consent = std::env::var("CCYNC_ASSUME_YES").ok();
    unsafe { std::env::remove_var("CCYNC_ASSUME_YES") };
    let ccync_home = tmp.path().join(".ccync");
    std::fs::create_dir_all(&ccync_home).unwrap();
    std::fs::write(
        ccync_home.join("config.json"),
        r#"{"selectedProjectionTargets":["codex"]}"#,
    )
    .unwrap();
    std::fs::write(ccync_home.join("AGENTS.md"), "# Consent test\n").unwrap();
    let target = target_path(ProjectionTargetId::Codex);
    let mut output = Vec::new();

    let exit = super::lifecycle::run_instruction_only_with_output(false, false, &mut output);
    let rendered = String::from_utf8(output).unwrap();
    let target_exists = target.exists();
    restore_home(&saved);
    unsafe {
        match prior_consent {
            Some(value) => std::env::set_var("CCYNC_ASSUME_YES", value),
            None => std::env::remove_var("CCYNC_ASSUME_YES"),
        }
    }

    assert_eq!(exit, ExitCode::Error);
    assert!(rendered.contains("--instructions-only"), "{rendered}");
    assert!(rendered.contains("--yes"), "{rendered}");
    assert!(rendered.contains("no files written"), "{rendered}");
    assert!(!target_exists);
}

#[test]
fn instructions_only_dry_run_renders_distinct_target_blockers_and_keeps_order() {
    let _lock = crate::test_io_lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let saved = isolate_home(tmp.path());
    let ccync_home = tmp.path().join(".ccync");
    std::fs::create_dir_all(&ccync_home).unwrap();
    std::fs::write(
        ccync_home.join("config.json"),
        r#"{"selectedProjectionTargets":["copilot","codex","claude"]}"#,
    )
    .unwrap();
    std::fs::write(ccync_home.join("AGENTS.md"), "# Rules\n").unwrap();
    let claude = legacy_path();
    let codex = target_path(ProjectionTargetId::Codex);
    let copilot = target_path(ProjectionTargetId::Copilot);
    for path in [&claude, &codex, &copilot] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    }
    std::fs::write(&claude, "<!-- ccync:managed:begin -->\nno end\n").unwrap();
    std::fs::write(&codex, b"target bytes").unwrap();
    std::fs::create_dir_all(&copilot).unwrap();
    let before = [
        std::fs::read(&claude).unwrap(),
        std::fs::read(&codex).unwrap(),
    ];
    let mut output = Vec::new();

    let exit = super::lifecycle::run_instruction_only_with_output(true, false, &mut output);
    let rendered = String::from_utf8(output).unwrap();
    let after = [
        std::fs::read(&claude).unwrap(),
        std::fs::read(&codex).unwrap(),
    ];
    let copilot_still_directory = copilot.is_dir();
    restore_home(&saved);

    assert_eq!(exit, ExitCode::Error);
    assert!(
        rendered.contains(&format!("{}: blocked: malformed marker", claude.display())),
        "{rendered}"
    );
    assert!(
        rendered.contains(&format!("{}: blocked: target read", copilot.display())),
        "{rendered}"
    );
    assert!(rendered.contains("no files written"), "{rendered}");
    assert_eq!(before, after);
    assert!(copilot_still_directory);
}

#[test]
fn instructions_only_apply_renders_every_outcome_and_failure_summary() {
    let _lock = crate::test_io_lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let saved = isolate_home(tmp.path());
    let ccync_home = tmp.path().join(".ccync");
    std::fs::create_dir_all(&ccync_home).unwrap();
    std::fs::write(
        ccync_home.join("config.json"),
        r#"{"selectedProjectionTargets":["claude","codex","copilot"]}"#,
    )
    .unwrap();
    std::fs::write(ccync_home.join("AGENTS.md"), "# Desired output\n").unwrap();
    let failed = legacy_path();
    let unchanged = target_path(ProjectionTargetId::Codex);
    let changed = target_path(ProjectionTargetId::Copilot);
    for path in [&failed, &unchanged, &changed] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    }
    std::fs::write(&failed, b"<!-- ccync:managed:begin -->\nbroken\n").unwrap();
    std::fs::write(
        &unchanged,
        b"<!-- ccync:managed:begin -->\n# Desired output\n\n<!-- ccync:managed:end -->\n",
    )
    .unwrap();
    std::fs::write(&changed, "user bytes\n").unwrap();
    let lock = ccync_home.join("plugins.lock.json");
    let render = ccync_home.join("build/render/sentinel");
    let mcp = ccync_home.join("build/mcp/sentinel");
    std::fs::create_dir_all(render.parent().unwrap()).unwrap();
    std::fs::create_dir_all(mcp.parent().unwrap()).unwrap();
    std::fs::write(&lock, b"lock snapshot").unwrap();
    std::fs::write(&render, b"render snapshot").unwrap();
    std::fs::write(&mcp, b"mcp snapshot").unwrap();
    let mut output = Vec::new();

    let exit = super::lifecycle::run_instruction_only_with_output(false, true, &mut output);
    let rendered = String::from_utf8(output).unwrap();
    let unrelated_after = [
        std::fs::read(&lock).unwrap(),
        std::fs::read(&render).unwrap(),
        std::fs::read(&mcp).unwrap(),
    ];
    restore_home(&saved);

    assert_eq!(exit, ExitCode::Error);
    assert!(
        rendered.contains(&format!("{}: blocked: malformed marker", failed.display())),
        "{rendered}"
    );
    assert!(
        rendered.contains(&format!("{}: unchanged", unchanged.display())),
        "{rendered}"
    );
    assert!(
        rendered.contains(&format!("{}: append", changed.display())),
        "{rendered}"
    );
    let failed_at = rendered.find(&failed.display().to_string()).unwrap();
    let unchanged_at = rendered.find(&unchanged.display().to_string()).unwrap();
    let changed_at = rendered.find(&changed.display().to_string()).unwrap();
    assert!(
        unchanged_at < changed_at && changed_at < failed_at,
        "{rendered}"
    );
    assert!(
        rendered.contains("changed: 1; unchanged: 1; failed: 1"),
        "{rendered}"
    );
    assert_eq!(
        unrelated_after,
        [
            b"lock snapshot".to_vec(),
            b"render snapshot".to_vec(),
            b"mcp snapshot".to_vec()
        ]
    );
}

#[test]
fn instruction_only_rejects_mixed_selection_without_touching_targets() {
    let _lock = crate::test_io_lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let saved = isolate_home(tmp.path());
    let ccync_home = tmp.path().join(".ccync");
    std::fs::create_dir_all(&ccync_home).unwrap();
    std::fs::write(
        ccync_home.join("config.json"),
        r#"{"selectedProjectionTargets":["codex","unknown-target"]}"#,
    )
    .unwrap();
    std::fs::write(ccync_home.join("AGENTS.md"), "# Desired\n").unwrap();
    let target = target_path(ProjectionTargetId::Codex);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    let before = b"user-owned target bytes";
    std::fs::write(&target, before).unwrap();

    let exit = cmd_sync(&[
        "sync".to_string(),
        "--instructions-only".to_string(),
        "--yes".to_string(),
    ]);
    let after = std::fs::read(&target).unwrap();

    restore_home(&saved);

    assert_eq!(exit, ExitCode::Error);
    assert_eq!(after, before);
}

#[test]
fn instructions_only_rejects_malformed_and_all_invalid_config_before_target_io() {
    let _lock = crate::test_io_lock();
    for (name, config) in [
        ("malformed", "{ not-json"),
        (
            "all-invalid",
            r#"{"selectedProjectionTargets":["unknown-target","also-unknown"]}"#,
        ),
    ] {
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_home(tmp.path());
        let ccync_home = tmp.path().join(".ccync");
        std::fs::create_dir_all(&ccync_home).unwrap();
        std::fs::write(ccync_home.join("config.json"), config).unwrap();
        std::fs::write(ccync_home.join("AGENTS.md"), "# Desired\n").unwrap();
        let target = target_path(ProjectionTargetId::Codex);

        let exit = cmd_sync(&[
            "sync".to_string(),
            "--instructions-only".to_string(),
            "--yes".to_string(),
        ]);

        assert_eq!(exit, ExitCode::Error, "{name} config must fail closed");
        assert!(
            !target.exists(),
            "{name} config must be rejected before target I/O"
        );
        restore_home(&saved);
    }
}

#[test]
fn instruction_only_dry_run_returns_success_without_creating_target_or_state() {
    let _lock = crate::test_io_lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let saved = isolate_home(tmp.path());
    let ccync_home = tmp.path().join(".ccync");
    std::fs::create_dir_all(&ccync_home).unwrap();
    std::fs::write(
        ccync_home.join("config.json"),
        r#"{"selectedProjectionTargets":["codex"]}"#,
    )
    .unwrap();
    std::fs::write(ccync_home.join("AGENTS.md"), "# Preview only\n").unwrap();
    let lockfile = ccync_home.join("plugins.lock.json");
    let lock_before = b"lock snapshot";
    std::fs::write(&lockfile, lock_before).unwrap();
    let target = target_path(ProjectionTargetId::Codex);

    let exit = cmd_sync(&[
        "sync".to_string(),
        "--instructions-only".to_string(),
        "--dry-run".to_string(),
    ]);
    let target_exists = target.exists();
    let lock_after = std::fs::read(&lockfile).unwrap();

    restore_home(&saved);

    assert_eq!(exit, ExitCode::Success);
    assert!(!target_exists);
    assert_eq!(lock_after, lock_before);
}

#[test]
fn instructions_only_rejects_missing_config_before_target_io() {
    let _lock = crate::test_io_lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let saved = isolate_home(tmp.path());
    std::fs::create_dir_all(tmp.path().join(".ccync")).unwrap();
    let target = target_path(ProjectionTargetId::Codex);

    let exit = cmd_sync(&[
        "sync".to_string(),
        "--instructions-only".to_string(),
        "--yes".to_string(),
    ]);

    restore_home(&saved);
    assert_eq!(exit, ExitCode::Error);
    assert!(!target.exists());
}

#[test]
fn instructions_only_rejects_unreadable_config_before_target_io() {
    let _lock = crate::test_io_lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let saved = isolate_home(tmp.path());
    let ccync_home = tmp.path().join(".ccync");
    std::fs::create_dir_all(&ccync_home).unwrap();
    // A directory at the config path produces a read error on supported platforms.
    std::fs::create_dir_all(ccync_home.join("config.json")).unwrap();
    std::fs::write(ccync_home.join("AGENTS.md"), "# Desired\n").unwrap();
    let target = target_path(ProjectionTargetId::Codex);

    let exit = cmd_sync(&[
        "sync".to_string(),
        "--instructions-only".to_string(),
        "--yes".to_string(),
    ]);

    restore_home(&saved);
    assert_eq!(exit, ExitCode::Error);
    assert!(!target.exists());
}

#[test]
fn instructions_only_apply_continues_after_malformed_target_and_preserves_failed_and_unchanged_files(
) {
    let _lock = crate::test_io_lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let saved = isolate_home(tmp.path());
    let ccync_home = tmp.path().join(".ccync");
    std::fs::create_dir_all(&ccync_home).unwrap();
    std::fs::write(
        ccync_home.join("config.json"),
        r#"{"selectedProjectionTargets":["claude","codex","copilot"]}"#,
    )
    .unwrap();
    std::fs::write(ccync_home.join("AGENTS.md"), "# Desired scoped rules\n").unwrap();

    let failed = legacy_path();
    let unchanged = target_path(ProjectionTargetId::Codex);
    let changed = target_path(ProjectionTargetId::Copilot);
    for path in [&failed, &unchanged, &changed] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    }
    let failed_bytes = b"<!-- ccync:managed:begin -->\nno end marker\n";
    let unchanged_bytes =
        b"<!-- ccync:managed:begin -->\n# Desired scoped rules\n\n<!-- ccync:managed:end -->\n";
    std::fs::write(&failed, failed_bytes).unwrap();
    std::fs::write(&unchanged, unchanged_bytes).unwrap();
    std::fs::write(&changed, "user text\n").unwrap();
    let unchanged_time = std::fs::metadata(&unchanged).unwrap().modified().unwrap();
    let lock = ccync_home.join("plugins.lock.json");
    let lock_bytes = b"unrelated plugin state";
    std::fs::write(&lock, lock_bytes).unwrap();

    let exit = cmd_sync(&[
        "sync".to_string(),
        "--instructions-only".to_string(),
        "--yes".to_string(),
    ]);
    let failed_after = std::fs::read(&failed).unwrap();
    let unchanged_after = std::fs::read(&unchanged).unwrap();
    let unchanged_time_after = std::fs::metadata(&unchanged).unwrap().modified().unwrap();
    let changed_after = std::fs::read_to_string(&changed).unwrap();
    let lock_after = std::fs::read(&lock).unwrap();
    restore_home(&saved);

    assert_eq!(exit, ExitCode::Error);
    assert_eq!(failed_after, failed_bytes);
    assert_eq!(unchanged_after, unchanged_bytes);
    assert_eq!(unchanged_time_after, unchanged_time);
    assert!(changed_after.contains("# Desired scoped rules"));
    assert_eq!(lock_after, lock_bytes);
}

#[test]
fn instructions_only_source_encoding_failure_preserves_target_and_unrelated_state() {
    let _lock = crate::test_io_lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let saved = isolate_home(tmp.path());
    let ccync_home = tmp.path().join(".ccync");
    std::fs::create_dir_all(&ccync_home).unwrap();
    std::fs::write(
        ccync_home.join("config.json"),
        r#"{"selectedProjectionTargets":["codex"]}"#,
    )
    .unwrap();
    std::fs::write(ccync_home.join("AGENTS.md"), [0xff, 0xfe]).unwrap();
    let target = target_path(ProjectionTargetId::Codex);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    let original = b"target snapshot";
    std::fs::write(&target, original).unwrap();
    let lock = ccync_home.join("plugins.lock.json");
    std::fs::write(&lock, b"lock snapshot").unwrap();

    let exit = cmd_sync(&[
        "sync".to_string(),
        "--instructions-only".to_string(),
        "--yes".to_string(),
    ]);
    let after = std::fs::read(&target).unwrap();
    let lock_after = std::fs::read(&lock).unwrap();
    restore_home(&saved);

    assert_eq!(exit, ExitCode::Error);
    assert_eq!(after, original);
    assert_eq!(lock_after, b"lock snapshot");
}

#[test]
fn instructions_only_reports_source_and_target_read_blockers_and_dry_run_writes_nothing() {
    let _lock = crate::test_io_lock();
    for source_is_directory in [true, false] {
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_home(tmp.path());
        let ccync_home = tmp.path().join(".ccync");
        std::fs::create_dir_all(&ccync_home).unwrap();
        std::fs::write(
            ccync_home.join("config.json"),
            r#"{"selectedProjectionTargets":["codex"]}"#,
        )
        .unwrap();
        let source = ccync_home.join("AGENTS.md");
        if source_is_directory {
            std::fs::create_dir_all(&source).unwrap();
        } else {
            std::fs::write(&source, "# Valid source\n").unwrap();
        }
        let target = target_path(ProjectionTargetId::Codex);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        if !source_is_directory {
            std::fs::create_dir_all(&target).unwrap();
        }
        let lock = ccync_home.join("plugins.lock.json");
        std::fs::write(&lock, b"unrelated").unwrap();

        let exit = cmd_sync(&[
            "sync".to_string(),
            "--instructions-only".to_string(),
            "--dry-run".to_string(),
        ]);
        let lock_after = std::fs::read(&lock).unwrap();
        let target_still_directory = target.is_dir();
        restore_home(&saved);

        assert_eq!(exit, ExitCode::Error);
        assert_eq!(lock_after, b"unrelated");
        if !source_is_directory {
            assert!(target_still_directory);
        }
    }
}

#[cfg(unix)]
#[test]
fn instructions_only_rejects_dangling_source_and_target_links_without_writing() {
    use std::os::unix::fs::symlink;

    let _lock = crate::test_io_lock();
    for source_is_dangling_link in [true, false] {
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_home(tmp.path());
        let ccync_home = tmp.path().join(".ccync");
        std::fs::create_dir_all(&ccync_home).unwrap();
        std::fs::write(
            ccync_home.join("config.json"),
            r#"{"selectedProjectionTargets":["codex"]}"#,
        )
        .unwrap();
        let source = ccync_home.join("AGENTS.md");
        let target = target_path(ProjectionTargetId::Codex);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        if source_is_dangling_link {
            symlink(ccync_home.join("missing-source"), &source).unwrap();
            std::fs::write(&target, b"target bytes").unwrap();
        } else {
            std::fs::write(&source, "# Source\n").unwrap();
            symlink(ccync_home.join("missing-target"), &target).unwrap();
        }
        let lock = ccync_home.join("plugins.lock.json");
        std::fs::write(&lock, b"lock bytes").unwrap();

        let exit = cmd_sync(&[
            "sync".to_string(),
            "--instructions-only".to_string(),
            "--yes".to_string(),
        ]);
        let lock_after = std::fs::read(&lock).unwrap();
        let source_link_remains = source.is_symlink();
        let target_link_remains = target.is_symlink();
        let target_bytes = if target_link_remains {
            None
        } else {
            Some(std::fs::read(&target).unwrap())
        };
        restore_home(&saved);

        assert_eq!(exit, ExitCode::Error);
        assert_eq!(lock_after, b"lock bytes");
        assert_eq!(source_link_remains, source_is_dangling_link);
        assert_eq!(target_link_remains, !source_is_dangling_link);
        if let Some(bytes) = target_bytes {
            assert_eq!(bytes, b"target bytes");
        }
    }
}

#[test]
fn instructions_only_requires_consent_on_each_pre_full_projection_apply() {
    let _lock = crate::test_io_lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let saved = isolate_home(tmp.path());
    let prior_consent = std::env::var("CCYNC_ASSUME_YES").ok();
    unsafe { std::env::remove_var("CCYNC_ASSUME_YES") };
    let ccync_home = tmp.path().join(".ccync");
    std::fs::create_dir_all(&ccync_home).unwrap();
    std::fs::write(
        ccync_home.join("config.json"),
        r#"{"selectedProjectionTargets":["codex"]}"#,
    )
    .unwrap();
    std::fs::write(ccync_home.join("AGENTS.md"), "# Consent\n").unwrap();
    let target = target_path(ProjectionTargetId::Codex);

    let first = cmd_sync(&[
        "sync".to_string(),
        "--instructions-only".to_string(),
        "--yes".to_string(),
    ]);
    let first_bytes = std::fs::read(&target).unwrap();
    let second = cmd_sync(&["sync".to_string(), "--instructions-only".to_string()]);
    let second_bytes = std::fs::read(&target).unwrap();

    restore_home(&saved);
    unsafe {
        match prior_consent {
            Some(value) => std::env::set_var("CCYNC_ASSUME_YES", value),
            None => std::env::remove_var("CCYNC_ASSUME_YES"),
        }
    }
    assert_eq!(first, ExitCode::Success);
    assert_eq!(second, ExitCode::Error);
    assert_eq!(second_bytes, first_bytes);
}

/// Byte snapshot of the codex target plus the unrelated `~/.ccync/` state.
struct ConsentFixture {
    target: PathBuf,
    unrelated: [PathBuf; 3],
}

impl ConsentFixture {
    fn new(ccync_home: &Path, config: &str) -> Self {
        std::fs::create_dir_all(ccync_home).unwrap();
        std::fs::write(ccync_home.join("config.json"), config).unwrap();
        std::fs::write(ccync_home.join("AGENTS.md"), "# Consent matrix\n").unwrap();
        let unrelated = [
            ccync_home.join("plugins.lock.json"),
            ccync_home.join("build/render/sentinel"),
            ccync_home.join("build/mcp/sentinel"),
        ];
        for path in &unrelated {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!("unrelated:{}", path.display())).unwrap();
        }
        let target = target_path(ProjectionTargetId::Codex);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"user-owned target bytes").unwrap();
        Self { target, unrelated }
    }

    fn snapshot(&self) -> Vec<Vec<u8>> {
        std::iter::once(&self.target)
            .chain(self.unrelated.iter())
            .map(|path| std::fs::read(path).unwrap())
            .collect()
    }

    fn target_bytes(&self) -> Vec<u8> {
        std::fs::read(&self.target).unwrap()
    }
}

#[test]
fn instructions_only_mixed_valid_and_invalid_selection_lists_every_rejected_value() {
    let _lock = crate::test_io_lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let saved = isolate_home(tmp.path());
    let fixture = ConsentFixture::new(
        &tmp.path().join(".ccync"),
        r#"{"selectedProjectionTargets":["codex","bogus-one","claude","bogus-two"]}"#,
    );
    let before = fixture.snapshot();
    let mut output = Vec::new();

    let exit =
        super::lifecycle::run_instruction_only_with_consent(false, &mut || true, &mut output);
    let rendered = String::from_utf8(output).unwrap();
    let after = fixture.snapshot();
    let claude_exists = legacy_path().exists();
    restore_home(&saved);

    assert_eq!(exit, ExitCode::Error);
    assert!(rendered.contains("bogus-one"), "{rendered}");
    assert!(rendered.contains("bogus-two"), "{rendered}");
    assert!(!rendered.contains("changed:"), "{rendered}");
    assert_eq!(before, after);
    assert!(!claude_exists);
}

#[test]
fn instructions_only_interactive_y_and_yes_authorize_the_write() {
    let _lock = crate::test_io_lock();
    for answer in ["y\n", "yes\n", " YES \n"] {
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_home(tmp.path());
        let fixture = ConsentFixture::new(
            &tmp.path().join(".ccync"),
            r#"{"selectedProjectionTargets":["codex"]}"#,
        );
        let unrelated_before = fixture.snapshot()[1..].to_vec();
        let mut output = Vec::new();
        let mut input = std::io::Cursor::new(answer.as_bytes().to_vec());

        let exit = super::lifecycle::run_instruction_only_with_consent(
            false,
            &mut || super::lifecycle::confirm_overwrite_with(false, None, true, &mut input),
            &mut output,
        );
        let rendered = String::from_utf8(output).unwrap();
        let target_after = String::from_utf8(fixture.target_bytes()).unwrap();
        let unrelated_after = fixture.snapshot()[1..].to_vec();
        restore_home(&saved);

        assert_eq!(exit, ExitCode::Success, "{answer:?}: {rendered}");
        assert!(target_after.contains("# Consent matrix"), "{answer:?}");
        assert!(
            target_after.contains("user-owned target bytes"),
            "{answer:?}"
        );
        assert!(
            rendered.contains("changed: 1; unchanged: 0; failed: 0"),
            "{rendered}"
        );
        assert_eq!(unrelated_before, unrelated_after, "{answer:?}");
    }
}

#[test]
fn instructions_only_interactive_refusal_writes_nothing_and_points_to_scoped_yes() {
    let _lock = crate::test_io_lock();
    for answer in ["n\n", "no\n", "\n", "yes please\n", ""] {
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_home(tmp.path());
        let fixture = ConsentFixture::new(
            &tmp.path().join(".ccync"),
            r#"{"selectedProjectionTargets":["codex"]}"#,
        );
        let before = fixture.snapshot();
        let mut output = Vec::new();
        let mut input = std::io::Cursor::new(answer.as_bytes().to_vec());

        let exit = super::lifecycle::run_instruction_only_with_consent(
            false,
            &mut || super::lifecycle::confirm_overwrite_with(false, None, true, &mut input),
            &mut output,
        );
        let rendered = String::from_utf8(output).unwrap();
        let after = fixture.snapshot();
        restore_home(&saved);

        assert_eq!(exit, ExitCode::Error, "{answer:?}: {rendered}");
        assert!(
            rendered.contains("--instructions-only"),
            "{answer:?}: {rendered}"
        );
        assert!(rendered.contains("--yes"), "{answer:?}: {rendered}");
        assert!(
            rendered.contains("no files written"),
            "{answer:?}: {rendered}"
        );
        assert!(!rendered.contains("changed:"), "{answer:?}: {rendered}");
        assert_eq!(before, after, "{answer:?}");
    }
}

#[test]
fn instructions_only_noninteractive_consent_requires_exact_one_in_environment() {
    let _lock = crate::test_io_lock();
    let cases: [(Option<&str>, bool); 5] = [
        (None, false),
        (Some("0"), false),
        (Some(""), false),
        (Some("true"), false),
        (Some("1"), true),
    ];
    for (env_value, writes) in cases {
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_home(tmp.path());
        let fixture = ConsentFixture::new(
            &tmp.path().join(".ccync"),
            r#"{"selectedProjectionTargets":["codex"]}"#,
        );
        let before = fixture.snapshot();
        let mut output = Vec::new();
        let mut input = std::io::Cursor::new(Vec::new());

        let exit = super::lifecycle::run_instruction_only_with_consent(
            false,
            &mut || super::lifecycle::confirm_overwrite_with(false, env_value, false, &mut input),
            &mut output,
        );
        let rendered = String::from_utf8(output).unwrap();
        let after = fixture.snapshot();
        restore_home(&saved);

        if writes {
            assert_eq!(exit, ExitCode::Success, "{env_value:?}: {rendered}");
            assert!(
                String::from_utf8(after[0].clone())
                    .unwrap()
                    .contains("# Consent matrix"),
                "{env_value:?}"
            );
            assert_eq!(before[1..], after[1..], "{env_value:?}");
        } else {
            assert_eq!(exit, ExitCode::Error, "{env_value:?}: {rendered}");
            assert!(rendered.contains("--yes"), "{env_value:?}: {rendered}");
            assert!(
                rendered.contains("no files written"),
                "{env_value:?}: {rendered}"
            );
            assert_eq!(before, after, "{env_value:?}");
        }
    }
}

#[test]
fn overwrite_environment_consent_accepts_only_exact_one() {
    assert!(super::lifecycle::overwrite_env_authorized(Some("1")));
    for value in [None, Some(""), Some("0"), Some("true"), Some("01")] {
        assert!(!super::lifecycle::overwrite_env_authorized(value));
    }
}

#[test]
fn overwrite_prompt_consent_accepts_only_y_or_yes() {
    use super::lifecycle::overwrite_prompt_authorized;
    assert!(overwrite_prompt_authorized("y\n"));
    assert!(overwrite_prompt_authorized(" YES \n"));
    for value in ["", "n", "no", "true", "yes please"] {
        assert!(!overwrite_prompt_authorized(value));
    }
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
    let target = target_path(ProjectionTargetId::Codex);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, "user-authored heading\n").unwrap();

    let _result = run_projection_core(&config_for(&["codex"]), "ccync sync");
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
    let target = target_path(ProjectionTargetId::Codex);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(
        &target,
        "before\n<!-- ccync:managed:begin -->\nold rules\n<!-- ccync:managed:end -->\nafter\n",
    )
    .unwrap();

    let _result = run_projection_core(&config_for(&["codex"]), "ccync sync");
    let projected = std::fs::read_to_string(&target).unwrap();

    restore_home(&saved);

    assert_eq!(projected, "before\n\nafter\n");
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

    let damaged_target = legacy_path();
    std::fs::create_dir_all(damaged_target.parent().unwrap()).unwrap();
    let damaged_bytes = "<!-- ccync:managed:begin -->\nfirst\n<!-- ccync:managed:begin -->\nsecond\n<!-- ccync:managed:end -->\n";
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

mod full_sync_instruction_source {
    use super::*;

    const MANAGED: &[u8] =
        b"keep\n<!-- ccync:managed:begin -->\nold rules\n<!-- ccync:managed:end -->\nafter\n";

    fn run_with_source(make_source: impl FnOnce(&Path)) -> (ExitCode, Vec<u8>, Vec<u8>) {
        let _lock = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_home(tmp.path());
        let ccync_home = tmp.path().join(".ccync");
        std::fs::create_dir_all(&ccync_home).unwrap();
        make_source(&ccync_home.join("AGENTS.md"));
        let claude = legacy_path();
        std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
        std::fs::write(&claude, MANAGED).unwrap();
        let codex = target_path(ProjectionTargetId::Codex);
        std::fs::create_dir_all(codex.parent().unwrap()).unwrap();
        std::fs::write(&codex, b"codex snapshot").unwrap();

        let exit = run_projection_core(&config_for(&["claude", "codex"]), "ccync sync");
        let claude_after = std::fs::read(&claude).unwrap();
        let codex_after = std::fs::read(&codex).unwrap();
        restore_home(&saved);
        (exit, claude_after, codex_after)
    }

    #[test]
    fn invalid_utf8_source_skips_every_instruction_write_and_exits_error() {
        let (exit, claude, codex) =
            run_with_source(|source| std::fs::write(source, [0xff, 0xfe]).unwrap());

        assert_eq!(exit, ExitCode::Error);
        assert_eq!(claude, MANAGED, "source failure must not prune a block");
        assert_eq!(codex, b"codex snapshot");
    }

    #[test]
    fn unreadable_source_skips_every_instruction_write_and_exits_error() {
        let (exit, claude, codex) =
            run_with_source(|source| std::fs::create_dir_all(source).unwrap());

        assert_eq!(exit, ExitCode::Error);
        assert_eq!(claude, MANAGED);
        assert_eq!(codex, b"codex snapshot");
    }

    #[test]
    fn source_failure_retains_unrelated_completed_phases() {
        let _lock = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_home(tmp.path());
        let ccync_home = tmp.path().join(".ccync");
        std::fs::create_dir_all(&ccync_home).unwrap();
        std::fs::write(ccync_home.join("AGENTS.md"), [0xff, 0xfe]).unwrap();
        let codex = target_path(ProjectionTargetId::Codex);
        std::fs::create_dir_all(codex.parent().unwrap()).unwrap();
        std::fs::write(&codex, MANAGED).unwrap();
        let lock = ccync_home.join("plugins.lock.json");
        std::fs::write(&lock, b"unrelated").unwrap();

        let exit = run_projection_core(&config_for(&["codex"]), "ccync sync");
        let lock_after = std::fs::read(&lock).unwrap();
        let codex_after = std::fs::read(&codex).unwrap();
        let render_retained = ccync_home.join("build").exists();
        restore_home(&saved);

        assert_eq!(exit, ExitCode::Error);
        assert_eq!(lock_after, b"unrelated");
        assert_eq!(codex_after, MANAGED);
        assert!(
            render_retained,
            "completed canonical render must be retained"
        );
    }

    #[test]
    fn zero_byte_source_still_prunes() {
        let (_exit, claude, _codex) =
            run_with_source(|source| std::fs::write(source, b"").unwrap());

        assert_eq!(claude, b"keep\n\nafter\n");
    }

    #[cfg(unix)]
    #[test]
    fn dangling_source_link_skips_every_instruction_write_and_exits_error() {
        use std::os::unix::fs::symlink;

        let (exit, claude, codex) = run_with_source(|source| {
            let missing = source.parent().unwrap().join("missing-source");
            symlink(missing, source).unwrap();
        });

        assert_eq!(exit, ExitCode::Error);
        assert_eq!(claude, MANAGED, "dangling source link must not prune");
        assert_eq!(codex, b"codex snapshot");
    }
}
