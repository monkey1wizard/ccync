use crate::target::{ProjectionTargetId, CANONICAL_PROJECTION_TARGETS};

fn home_var() -> &'static str {
    if cfg!(windows) {
        "USERPROFILE"
    } else {
        "HOME"
    }
}

fn restore_var(name: &str, prior: Option<std::ffi::OsString>) {
    match prior {
        Some(value) => std::env::set_var(name, value),
        None => std::env::remove_var(name),
    }
}

#[test]
fn user_scope_instruction_write_paths_use_agents_filename() {
    let _guard = crate::test_support::env_guard();
    let var = home_var();
    let prior = std::env::var_os(var);
    let fake_home = tempfile::TempDir::new().unwrap();
    std::env::set_var(var, fake_home.path());

    let expected = [
        (ProjectionTargetId::Claude, None),
        (ProjectionTargetId::Codex, Some(".codex/AGENTS.md")),
        (ProjectionTargetId::Copilot, Some(".copilot/AGENTS.md")),
        (
            ProjectionTargetId::Opencode,
            Some(".config/opencode/AGENTS.md"),
        ),
        (ProjectionTargetId::AgyCli, Some(".gemini/AGENTS.md")),
        (ProjectionTargetId::AgyIde, Some(".gemini/AGENTS.md")),
        (ProjectionTargetId::AgyGui, Some(".gemini/AGENTS.md")),
    ];
    assert_eq!(expected.len(), 7, "the path matrix covers seven targets");
    assert_eq!(
        expected
            .iter()
            .map(|(target, _)| *target)
            .collect::<Vec<_>>(),
        CANONICAL_PROJECTION_TARGETS,
        "the path matrix covers every canonical target in order"
    );

    for (target, relative) in expected {
        let actual = crate::paths::user_scope_instruction_write_path(target);
        assert_eq!(actual, relative.map(|path| fake_home.path().join(path)));
        if let Some(path) = actual {
            assert_eq!(path.file_name().unwrap(), "AGENTS.md");
        }
    }
    assert_eq!(
        crate::paths::legacy_claude_user_instruction_path(),
        Some(fake_home.path().join(".claude/CLAUDE.md"))
    );
    restore_var(var, prior);
}

#[test]
fn user_scope_instruction_write_paths_are_none_without_home() {
    let _guard = crate::test_support::env_guard();
    let var = home_var();
    let prior = std::env::var_os(var);
    std::env::remove_var(var);

    for target in [
        ProjectionTargetId::Claude,
        ProjectionTargetId::Codex,
        ProjectionTargetId::Copilot,
        ProjectionTargetId::Opencode,
        ProjectionTargetId::AgyCli,
        ProjectionTargetId::AgyIde,
        ProjectionTargetId::AgyGui,
    ] {
        assert_eq!(
            crate::paths::user_scope_instruction_write_path(target),
            None
        );
    }
    assert_eq!(crate::paths::legacy_claude_user_instruction_path(), None);

    restore_var(var, prior);
}

#[test]
fn user_scope_instruction_write_paths_do_not_use_config_or_vendor_home_overrides() {
    let _guard = crate::test_support::env_guard();
    let home_name = home_var();
    let prior_home = std::env::var_os(home_name);
    let prior_appdata = std::env::var_os("APPDATA");
    let prior_xdg = std::env::var_os("XDG_CONFIG_HOME");
    let prior_codex = std::env::var_os("CODEX_HOME");
    let prior_opencode = std::env::var_os("OPENCODE_CONFIG_DIR");
    let fake_home = tempfile::TempDir::new().unwrap();
    let alternate = tempfile::TempDir::new().unwrap();

    std::env::set_var(home_name, fake_home.path());
    std::env::set_var("APPDATA", alternate.path());
    std::env::set_var("XDG_CONFIG_HOME", alternate.path());
    std::env::set_var("CODEX_HOME", alternate.path());
    std::env::set_var("OPENCODE_CONFIG_DIR", alternate.path());

    assert_eq!(
        crate::paths::user_scope_instruction_write_path(ProjectionTargetId::Codex),
        Some(fake_home.path().join(".codex/AGENTS.md"))
    );
    assert_eq!(
        crate::paths::user_scope_instruction_write_path(ProjectionTargetId::Opencode),
        Some(fake_home.path().join(".config/opencode/AGENTS.md"))
    );

    restore_var(home_name, prior_home);
    restore_var("APPDATA", prior_appdata);
    restore_var("XDG_CONFIG_HOME", prior_xdg);
    restore_var("CODEX_HOME", prior_codex);
    restore_var("OPENCODE_CONFIG_DIR", prior_opencode);
}
