use ccync_engine::install::{fetch_personal_plugin_at, PersonalFetchResult};
use std::process::Command;
use tempfile::tempdir;

fn run_git(cwd: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("git command must succeed to set up test fixture");
    assert!(
        out.status.success(),
        "git {:?} in {:?} failed: stderr={}",
        args,
        cwd,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn fetch_personal_plugin_at_ignores_fetching_residue() {
    let origin_dir = tempdir().expect("tempdir for origin repo");
    let origin_path = origin_dir.path();
    run_git(origin_path, &["init"]);
    run_git(
        origin_path,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "initial commit",
        ],
    );

    let cache_dir = tempdir().expect("tempdir for cache root");
    let cache_root = cache_dir.path();
    let plugin_id = "test-plugin";
    let residue = cache_root.join(format!("{plugin_id}@__fetching__"));
    std::fs::create_dir_all(&residue).expect("create fetching residue");

    let source_url = origin_path.to_str().expect("valid utf-8 source path");
    let result = fetch_personal_plugin_at(cache_root, plugin_id, source_url, None);

    match result {
        PersonalFetchResult::Cloned { path, .. } => {
            assert!(path.is_dir(), "cloned path {path:?} must exist");
            assert_ne!(path, residue, "fetching residue must not be accepted");
        }
        PersonalFetchResult::AlreadyPresent(path) => {
            panic!("fetching residue was accepted as cached: {path:?}");
        }
        other => panic!("expected Cloned after ignoring residue, got {other:?}"),
    }
}

#[test]
fn fetch_personal_plugin_at_accepts_valid_cache_alongside_residue() {
    let cache_dir = tempdir().expect("tempdir for cache root");
    let cache_root = cache_dir.path();
    let plugin_id = "test-plugin";
    let residue = cache_root.join(format!("{plugin_id}@__fetching__"));
    std::fs::create_dir_all(&residue).expect("create fetching residue");

    let valid_cache = cache_root.join(format!("{plugin_id}@1234567890abcdef"));
    std::fs::create_dir_all(&valid_cache).expect("create valid cached dir");

    let result =
        fetch_personal_plugin_at(cache_root, plugin_id, "https://example.com/repo.git", None);

    match result {
        PersonalFetchResult::AlreadyPresent(path) => {
            assert_eq!(
                path, valid_cache,
                "valid cache must be returned, not residue"
            );
            assert_ne!(path, residue, "residue must not be returned");
        }
        other => panic!("expected AlreadyPresent for valid cache, got {other:?}"),
    }
}
