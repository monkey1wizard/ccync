use ccync_engine::install::run_uninstall;
use std::fs;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};
use tempfile::TempDir;

/// Serialize tests that mutate process-global `HOME`/`USERPROFILE`.
fn env_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Point `ccync_home()` / `user_home()` at `home` for the duration of a test.
fn set_home(home: &Path) {
    #[cfg(windows)]
    unsafe {
        std::env::set_var("USERPROFILE", home);
        std::env::set_var("HOME", home);
    }
    #[cfg(not(windows))]
    unsafe {
        std::env::set_var("HOME", home);
    }
}

#[test]
fn uninstall_sweep_preserves_agents_md_input_file() {
    let _guard = env_guard();
    let fake_home = TempDir::new().unwrap();
    set_home(fake_home.path());

    let ccync_home = fake_home.path().join(".ccync");
    fs::create_dir_all(ccync_home.join("build").join("render")).unwrap();
    fs::create_dir_all(ccync_home.join("cache")).unwrap();
    fs::write(ccync_home.join("config.json"), "{}").unwrap();
    fs::write(ccync_home.join("plugins.json"), r#"{"plugins":[]}"#).unwrap();

    let agents_md = ccync_home.join("AGENTS.md");
    let agents_content = "# User Instruction Content\nKeep this file across uninstall.\n";
    fs::write(&agents_md, agents_content).unwrap();

    run_uninstall().expect("uninstall should succeed");

    assert!(
        agents_md.exists(),
        "uninstall sweep must preserve ~/.ccync/AGENTS.md"
    );
    assert_eq!(
        fs::read_to_string(&agents_md).unwrap(),
        agents_content,
        "AGENTS.md content must be preserved unchanged"
    );
    assert!(
        ccync_home.join("config.json").exists(),
        "config.json must be preserved"
    );
    assert!(
        ccync_home.join("plugins.json").exists(),
        "plugins.json must be preserved"
    );
    let build_left: Vec<_> = fs::read_dir(ccync_home.join("build"))
        .map(|d| d.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(
        build_left.iter().all(|n| n == "lifecycle.lock"),
        "build must keep only the lifecycle lock, found {build_left:?}"
    );
    assert!(
        !ccync_home.join("cache").exists(),
        "cache directory must be removed"
    );
}

#[test]
fn uninstall_sweep_preserves_agents_md_when_other_inputs_absent() {
    let _guard = env_guard();
    let fake_home = TempDir::new().unwrap();
    set_home(fake_home.path());

    let ccync_home = fake_home.path().join(".ccync");
    fs::create_dir_all(ccync_home.join("build")).unwrap();

    let agents_md = ccync_home.join("AGENTS.md");
    let agents_content = "# Standalone AGENTS.md\n";
    fs::write(&agents_md, agents_content).unwrap();

    run_uninstall().expect("uninstall should succeed");

    assert!(
        agents_md.exists(),
        "uninstall sweep must preserve ~/.ccync/AGENTS.md when config/plugins absent"
    );
    assert_eq!(
        fs::read_to_string(&agents_md).unwrap(),
        agents_content,
        "AGENTS.md content must be preserved unchanged"
    );
}
