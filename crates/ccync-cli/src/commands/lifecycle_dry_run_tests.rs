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
        scope_builder.contains("instruction_paths")
            && scope_builder.contains("legacy_instruction_prune_paths"),
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

mod full_sync_instruction_source {
    use super::super::lifecycle::{dry_run_report_lines, run_dry_run_report};
    use ccync_engine::ExitCode;
    use ccync_foundation::config::CcyncConfig;
    use ccync_foundation::paths::{
        legacy_claude_user_instruction_path, user_scope_instruction_write_path,
    };
    use ccync_foundation::target::ProjectionTargetId;
    use std::path::Path;

    fn legacy_path() -> std::path::PathBuf {
        legacy_claude_user_instruction_path().unwrap()
    }

    fn isolate(tmp: &Path) -> Vec<(&'static str, Option<String>)> {
        let keys = [
            "USERPROFILE",
            "HOME",
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
        ];
        let saved = keys.map(|key| (key, std::env::var(key).ok())).to_vec();
        for key in keys {
            unsafe { std::env::set_var(key, tmp) };
        }
        saved
    }

    fn restore(saved: &[(&'static str, Option<String>)]) {
        for (key, value) in saved {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn config(targets: &[&str]) -> CcyncConfig {
        let mut config = CcyncConfig::default();
        config.selected_projection_targets = targets.iter().map(|t| (*t).to_string()).collect();
        config
    }

    #[test]
    fn dry_run_reports_typed_source_blocker_on_every_path_and_writes_nothing() {
        let _lock = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate(tmp.path());
        let ccync_home = tmp.path().join(".ccync");
        std::fs::create_dir_all(&ccync_home).unwrap();
        std::fs::write(ccync_home.join("AGENTS.md"), [0xff, 0xfe]).unwrap();
        let claude = legacy_path();
        std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
        let managed = b"keep\n<!-- ccync:managed:begin -->\nold\n<!-- ccync:managed:end -->\n";
        std::fs::write(&claude, managed).unwrap();
        let codex = user_scope_instruction_write_path(ProjectionTargetId::Codex).unwrap();

        let lines = dry_run_report_lines(&config(&["claude", "codex"])).join("\n");
        let claude_after = std::fs::read(&claude).unwrap();
        let codex_created = codex.exists();
        restore(&saved);

        assert!(lines.contains(&format!("{}: blocked: source encoding", claude.display())));
        assert!(lines.contains(&format!("{}: blocked: source encoding", codex.display())));
        assert!(
            !lines.contains(": prune"),
            "source failure must not preview a prune"
        );
        assert!(
            lines.contains("MCP host files"),
            "other preview output must remain"
        );
        assert_eq!(claude_after, managed);
        assert!(!codex_created);
    }

    #[test]
    fn dry_run_reports_typed_target_blockers_without_lossy_labels() {
        let _lock = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate(tmp.path());
        let ccync_home = tmp.path().join(".ccync");
        std::fs::create_dir_all(&ccync_home).unwrap();
        std::fs::write(ccync_home.join("AGENTS.md"), "# Rules\n").unwrap();
        let claude = legacy_path();
        std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
        std::fs::write(
            &claude,
            "<!-- ccync:managed:begin -->\na\n<!-- ccync:managed:begin -->\nb\n<!-- ccync:managed:end -->\n",
        )
        .unwrap();
        let codex = user_scope_instruction_write_path(ProjectionTargetId::Codex).unwrap();
        std::fs::create_dir_all(&codex).unwrap();

        let lines = dry_run_report_lines(&config(&["claude", "codex"])).join("\n");
        restore(&saved);

        assert!(lines.contains(&format!("{}: blocked: malformed marker", claude.display())));
        assert!(lines.contains(&format!("{}: blocked: target read", codex.display())));
        assert!(!lines.contains("damaged-skip") && !lines.contains("TargetRead"));
    }

    #[test]
    fn dry_run_treats_missing_source_as_prune_not_blocker() {
        let _lock = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate(tmp.path());
        let claude = legacy_path();
        std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
        std::fs::write(
            &claude,
            "<!-- ccync:managed:begin -->\nold\n<!-- ccync:managed:end -->\n",
        )
        .unwrap();

        let lines = dry_run_report_lines(&config(&["claude"])).join("\n");
        restore(&saved);

        assert!(lines.contains(&format!("{}: prune", claude.display())));
        assert!(!lines.contains("blocked: source"));
    }

    #[cfg(unix)]
    #[test]
    fn dry_run_reports_dangling_source_blocker() {
        use std::os::unix::fs::symlink;

        let _lock = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate(tmp.path());
        let ccync_home = tmp.path().join(".ccync");
        std::fs::create_dir_all(&ccync_home).unwrap();
        let missing = ccync_home.join("missing-source");
        symlink(missing, ccync_home.join("AGENTS.md")).unwrap();
        let claude = legacy_path();
        std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
        let managed = b"keep\n<!-- ccync:managed:begin -->\nold\n<!-- ccync:managed:end -->\n";
        std::fs::write(&claude, managed).unwrap();

        let lines = dry_run_report_lines(&config(&["claude"])).join("\n");
        let claude_after = std::fs::read(&claude).unwrap();
        restore(&saved);

        assert!(lines.contains(&format!(
            "{}: blocked: dangling source link",
            claude.display()
        )));
        assert!(!lines.contains(": prune"));
        assert_eq!(claude_after, managed);
    }

    #[cfg(unix)]
    #[test]
    fn dry_run_reports_dangling_target_link_blocker() {
        use std::os::unix::fs::symlink;

        let _lock = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate(tmp.path());
        let ccync_home = tmp.path().join(".ccync");
        std::fs::create_dir_all(&ccync_home).unwrap();
        std::fs::write(ccync_home.join("AGENTS.md"), "# Rules\n").unwrap();
        let claude = legacy_path();
        std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
        let missing = claude.parent().unwrap().join("missing-claude");
        symlink(missing, &claude).unwrap();

        let lines = dry_run_report_lines(&config(&["claude"])).join("\n");
        restore(&saved);

        assert!(lines.contains(&format!("{}: blocked: dangling link", claude.display())));
    }

    #[test]
    fn dry_run_exits_error_on_invalid_utf8_source_and_writes_nothing() {
        let _lock = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate(tmp.path());
        let ccync_home = tmp.path().join(".ccync");
        std::fs::create_dir_all(&ccync_home).unwrap();
        std::fs::write(ccync_home.join("AGENTS.md"), [0xff, 0xfe]).unwrap();
        let codex = user_scope_instruction_write_path(ProjectionTargetId::Codex).unwrap();

        let config = config(&["claude", "codex"]);
        let lines = dry_run_report_lines(&config).join(
            "
",
        );
        let code = run_dry_run_report(&config);
        let codex_created = codex.exists();
        restore(&saved);

        assert!(lines.contains("blocked: source encoding"));
        assert_eq!(code, ExitCode::Error);
        assert!(!codex_created);
    }

    #[test]
    fn dry_run_exits_error_on_malformed_marker_target() {
        let _lock = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate(tmp.path());
        let ccync_home = tmp.path().join(".ccync");
        std::fs::create_dir_all(&ccync_home).unwrap();
        std::fs::write(
            ccync_home.join("AGENTS.md"),
            "# Rules
",
        )
        .unwrap();
        let claude = legacy_path();
        std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
        std::fs::write(
            &claude,
            "<!-- ccync:managed:begin -->
a
<!-- ccync:managed:begin -->
b
<!-- ccync:managed:end -->
",
        )
        .unwrap();

        let config = config(&["claude"]);
        let lines = dry_run_report_lines(&config).join(
            "
",
        );
        let code = run_dry_run_report(&config);
        restore(&saved);

        assert!(lines.contains("blocked: malformed marker"));
        assert_eq!(code, ExitCode::Error);
    }

    #[test]
    fn dry_run_exits_success_on_clean_preview() {
        let _lock = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate(tmp.path());
        let ccync_home = tmp.path().join(".ccync");
        std::fs::create_dir_all(&ccync_home).unwrap();
        std::fs::write(
            ccync_home.join("AGENTS.md"),
            "# Rules
",
        )
        .unwrap();

        let config = config(&["claude", "codex"]);
        let lines = dry_run_report_lines(&config).join(
            "
",
        );
        let code = run_dry_run_report(&config);
        restore(&saved);

        assert!(!lines.contains("blocked:"));
        assert_eq!(code, ExitCode::Success);
    }
}
