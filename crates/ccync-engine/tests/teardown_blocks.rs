use ccync_engine::install::run_uninstall;
use ccync_foundation::paths::user_scope_instruction_path;
use ccync_foundation::target::CANONICAL_PROJECTION_TARGETS;
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

fn seed_ccync_home(home: &Path) {
    let ccync_home = home.join(".ccync");
    fs::create_dir_all(ccync_home.join("build").join("render")).unwrap();
    fs::write(ccync_home.join("config.json"), "{}").unwrap();
    fs::write(ccync_home.join("plugins.json"), r#"{"plugins":[]}"#).unwrap();
}

fn target_instruction_paths() -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    for target in CANONICAL_PROJECTION_TARGETS {
        if let Some(path) = user_scope_instruction_path(*target) {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    paths
}

#[test]
fn uninstall_removes_managed_blocks_from_all_target_files() {
    let _guard = env_guard();
    let fake_home = TempDir::new().unwrap();
    set_home(fake_home.path());
    seed_ccync_home(fake_home.path());

    let targets = target_instruction_paths();
    assert_eq!(
        targets.len(),
        5,
        "expected exactly 5 deduped user-scope instruction files"
    );

    for path in &targets {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let content = format!(
            "# User Header for {}\n\n<!-- ccync:managed:begin -->\nmanaged block payload\n<!-- ccync:managed:end -->\n\n# User Footer\n",
            path.display()
        );
        fs::write(path, content).unwrap();
    }

    run_uninstall().expect("uninstall should succeed");

    for path in &targets {
        assert!(
            path.exists(),
            "target file should not be deleted: {}",
            path.display()
        );
        let content = fs::read_to_string(path).unwrap();
        assert!(
            !content.contains("ccync:managed:begin"),
            "uninstall leaves the managed block behind in target file {}: found {}",
            path.display(),
            content
        );
        assert!(
            !content.contains("<!-- ccync:managed:end -->"),
            "uninstall leaves end marker in target file {}: found {}",
            path.display(),
            content
        );
        assert!(
            content.contains(&format!("# User Header for {}", path.display())),
            "user header should be preserved in {}",
            path.display()
        );
        assert!(
            content.contains("# User Footer"),
            "user footer should be preserved in {}",
            path.display()
        );
    }
}

#[test]
fn uninstall_leaves_absent_target_files_absent() {
    let _guard = env_guard();
    let fake_home = TempDir::new().unwrap();
    set_home(fake_home.path());
    seed_ccync_home(fake_home.path());

    let targets = target_instruction_paths();
    run_uninstall().expect("uninstall should succeed");

    for path in &targets {
        assert!(
            !path.exists(),
            "uninstall must not create absent target file: {}",
            path.display()
        );
    }
}

#[test]
fn uninstall_preserves_target_file_when_it_only_contained_managed_block() {
    let _guard = env_guard();
    let fake_home = TempDir::new().unwrap();
    set_home(fake_home.path());
    seed_ccync_home(fake_home.path());

    let targets = target_instruction_paths();
    let test_file = &targets[0];
    if let Some(parent) = test_file.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(
        test_file,
        "<!-- ccync:managed:begin -->\nonly managed block\n<!-- ccync:managed:end -->\n",
    )
    .unwrap();

    run_uninstall().expect("uninstall should succeed");

    assert!(
        test_file.exists(),
        "target file should not be deleted even if it only had managed block: {}",
        test_file.display()
    );
    let content = fs::read_to_string(test_file).unwrap();
    assert!(
        !content.contains("ccync:managed:begin"),
        "managed block should be pruned: {}",
        content
    );
}

#[test]
fn uninstall_leaves_damaged_target_file_alone() {
    let _guard = env_guard();
    let fake_home = TempDir::new().unwrap();
    set_home(fake_home.path());
    seed_ccync_home(fake_home.path());

    let targets = target_instruction_paths();
    let damaged_file = &targets[0];
    if let Some(parent) = damaged_file.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let damaged_content = "<!-- ccync:managed:begin -->\nmissing end marker\n";
    fs::write(damaged_file, damaged_content).unwrap();

    // Uninstall should report damaged file and not corrupt or prune it
    let _ = run_uninstall();

    assert_eq!(
        fs::read_to_string(damaged_file).unwrap(),
        damaged_content,
        "damaged file must be left alone by uninstall"
    );
}
