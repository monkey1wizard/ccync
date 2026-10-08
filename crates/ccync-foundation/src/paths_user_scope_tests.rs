use std::path::PathBuf;

use crate::target::ProjectionTargetId;

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
fn user_scope_instruction_path_matches_expected_for_every_target() {
    let _guard = crate::test_support::env_guard();
    let var = home_var();
    let prior = std::env::var_os(var);
    let fake_home = tempfile::TempDir::new().unwrap();
    std::env::set_var(var, fake_home.path());

    let expected = [
        (
            ProjectionTargetId::Claude,
            fake_home.path().join(".claude/CLAUDE.md"),
        ),
        (
            ProjectionTargetId::Codex,
            fake_home.path().join(".codex/AGENTS.md"),
        ),
        (
            ProjectionTargetId::Opencode,
            fake_home.path().join(".config/opencode/AGENTS.md"),
        ),
        (
            ProjectionTargetId::Copilot,
            fake_home.path().join(".copilot/AGENTS.md"),
        ),
        (
            ProjectionTargetId::AgyCli,
            fake_home.path().join(".gemini/AGENTS.md"),
        ),
        (
            ProjectionTargetId::AgyIde,
            fake_home.path().join(".gemini/AGENTS.md"),
        ),
        (
            ProjectionTargetId::AgyGui,
            fake_home.path().join(".gemini/AGENTS.md"),
        ),
    ];

    for (target, path) in expected {
        assert_eq!(
            crate::paths::user_scope_instruction_path(target),
            Some(path)
        );
    }

    restore_var(var, prior);
}

#[test]
fn user_scope_instruction_path_is_none_without_home() {
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
            crate::paths::user_scope_instruction_path(target),
            None::<PathBuf>
        );
    }

    restore_var(var, prior);
}

#[test]
fn user_scope_instruction_path_does_not_use_config_or_vendor_home_overrides() {
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
        crate::paths::user_scope_instruction_path(ProjectionTargetId::Codex),
        Some(fake_home.path().join(".codex/AGENTS.md"))
    );
    assert_eq!(
        crate::paths::user_scope_instruction_path(ProjectionTargetId::Opencode),
        Some(fake_home.path().join(".config/opencode/AGENTS.md"))
    );

    restore_var(home_name, prior_home);
    restore_var("APPDATA", prior_appdata);
    restore_var("XDG_CONFIG_HOME", prior_xdg);
    restore_var("CODEX_HOME", prior_codex);
    restore_var("OPENCODE_CONFIG_DIR", prior_opencode);
}
