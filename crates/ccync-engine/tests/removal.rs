use ccync_engine::config::CcyncConfig;
use ccync_engine::install::{run_install, run_uninstall};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn make_ccync_source_tree_with_content(root: &Path) {
    fs::create_dir_all(root.join("agents")).unwrap();
    fs::create_dir_all(root.join("skills").join("doc-sync")).unwrap();
    fs::create_dir_all(root.join("commands").join("ccync")).unwrap();

    fs::write(
        root.join("agents").join("golem-steward.agent.md"),
        "---\nname: golem-steward\ndescription: doc keeper\n---\n# body\n",
    )
    .unwrap();
    fs::write(
        root.join("skills").join("doc-sync").join("SKILL.md"),
        "# doc-sync skill\n",
    )
    .unwrap();
    fs::write(
        root.join("commands").join("ccync").join("SKILL.md"),
        "# ccync command\n",
    )
    .unwrap();
}

#[test]
fn run_uninstall_removes_rust_managed_provider_outputs() {
    let fake_home = TempDir::new().unwrap();
    let source = TempDir::new().unwrap();
    make_ccync_source_tree_with_content(source.path());

    #[cfg(windows)]
    unsafe {
        std::env::set_var("USERPROFILE", fake_home.path());
        std::env::set_var("HOME", fake_home.path());
    }

    #[cfg(not(windows))]
    unsafe {
        std::env::set_var("HOME", fake_home.path());
    }

    let ccync_home = fake_home.path().join(".ccync");
    fs::create_dir_all(ccync_home.join("config")).unwrap();
    fs::write(
        ccync_home.join("install-state.json"),
        serde_json::json!({
            "selectedRuntimes": ["copilot", "antigravity", "codex", "claude"],
            "primaryRuntime": "copilot"
        })
        .to_string(),
    )
    .unwrap();

    // Simulate a pre-rename install's leftover dist tree to exercise the
    // providers→runtimes path migration on the next run (best-effort cleanup).
    let stale_dist = ccync_home.join("dist").join("providers");
    fs::create_dir_all(stale_dist.join("claude")).unwrap();
    fs::write(stale_dist.join("claude").join("managed.json"), "{}").unwrap();

    let config = CcyncConfig {
        dev_mode: Some(true),
        ccync_root: Some(source.path().display().to_string()),
        install_mode: None,
    };

    run_install(&config).unwrap();

    assert!(ccync_home.join("plugins").join("ccync").exists());
    assert!(ccync_home.join("dist").join("runtimes").exists());
    assert!(
        !stale_dist.exists(),
        "stale dist/providers must be migrated away on rerun"
    );
    assert!(fake_home
        .path()
        .join(".copilot")
        .join("installed-plugins")
        .join("ccync-copilot")
        .join("ccync")
        .exists());
    assert!(fake_home
        .path()
        .join(".claude")
        .join("skills")
        .join("ccync")
        .exists());

    run_uninstall().unwrap();

    assert!(!ccync_home.join("plugins").join("ccync").exists());
    assert!(!ccync_home.join("dist").join("runtimes").exists());
    assert!(!fake_home
        .path()
        .join(".copilot")
        .join("installed-plugins")
        .join("ccync-copilot")
        .join("ccync")
        .exists());
    assert!(!fake_home
        .path()
        .join(".claude")
        .join("skills")
        .join("ccync")
        .exists());
    assert!(ccync_home.join("ledger.json").is_file());
}
