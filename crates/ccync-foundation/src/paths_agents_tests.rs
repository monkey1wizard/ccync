use std::path::PathBuf;

fn home_var() -> &'static str {
    if cfg!(windows) {
        "USERPROFILE"
    } else {
        "HOME"
    }
}

#[test]
fn ccync_agents_md_path_derives_from_ccync_home() {
    let _guard = crate::test_support::env_guard();
    let var = home_var();
    let prior = std::env::var_os(var);
    let fake_home = tempfile::TempDir::new().unwrap();

    std::env::set_var(var, fake_home.path());
    assert_eq!(
        crate::paths::ccync_agents_md_path(),
        Some(fake_home.path().join(".ccync").join("AGENTS.md"))
    );

    match prior {
        Some(value) => std::env::set_var(var, value),
        None => std::env::remove_var(var),
    }
}

#[test]
fn ccync_agents_md_path_is_none_without_home() {
    let _guard = crate::test_support::env_guard();
    let var = home_var();
    let prior = std::env::var_os(var);

    std::env::remove_var(var);
    assert_eq!(crate::paths::ccync_agents_md_path(), None::<PathBuf>);

    match prior {
        Some(value) => std::env::set_var(var, value),
        None => std::env::remove_var(var),
    }
}
