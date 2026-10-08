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
fn fetch_personal_plugin_at_pinned_revision_installs_pinned_commit() {
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
            "commit 1",
        ],
    );
    let sha_a = run_git(origin_path, &["rev-parse", "HEAD"]);

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
            "commit 2",
        ],
    );
    let sha_b = run_git(origin_path, &["rev-parse", "HEAD"]);

    assert_ne!(sha_a, sha_b, "fixture commits must have distinct SHAs");

    let cache_dir = tempdir().expect("tempdir for cache root");
    let cache_root = cache_dir.path();
    let plugin_id = "test-plugin";

    // Pre-populate <plugin_id>@<shaB> in cache_root.
    let cached_b_dir = cache_root.join(format!("{plugin_id}@{sha_b}"));
    std::fs::create_dir_all(&cached_b_dir).expect("create cached dir for shaB");

    let source_url = origin_path.to_str().expect("valid utf-8 source path");
    let result = fetch_personal_plugin_at(cache_root, plugin_id, source_url, Some(&sha_a));

    let expected_dir = cache_root.join(format!("{plugin_id}@{sha_a}"));
    match result {
        PersonalFetchResult::Cloned { path, sha } => {
            assert_eq!(sha, sha_a, "cloned sha must match requested shaA");
            assert_eq!(path, expected_dir, "cloned path must match target dir");
            assert!(
                expected_dir.is_dir(),
                "expected target directory {expected_dir:?} to exist"
            );
        }
        PersonalFetchResult::AlreadyPresent(p) => {
            panic!(
                "expected Cloned with sha {sha_a}, but got AlreadyPresent({p:?}) due to prefix scan"
            );
        }
        other => {
            panic!("expected Cloned for shaA ({sha_a}), but got {other:?}");
        }
    }
}
