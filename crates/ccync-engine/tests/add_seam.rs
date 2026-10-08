use ccync_engine::install::{add_plugin_entry, read_personal_catalog, AddResult};
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
fn add_plugin_entry_fetches_at_pin_and_appends_catalog_entry() {
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

    let cache_dir = tempdir().expect("tempdir for cache root");
    let cache_root = cache_dir.path();

    let catalog_dir = tempdir().expect("tempdir for catalog");
    let catalog_path = catalog_dir.path().join("plugins.json");

    let plugin_id = "test-plugin";
    let source_url = origin_path.to_str().expect("valid utf-8 source path");

    let result = add_plugin_entry(
        cache_root,
        &catalog_path,
        plugin_id,
        source_url,
        Some(&sha_a),
    );

    let expected_dir = cache_root.join(format!("{plugin_id}@{sha_a}"));
    match result {
        AddResult::Added { path, sha, fetched } => {
            assert!(fetched, "expected fetched=true for newly cloned plugin");
            assert_eq!(sha, sha_a, "added sha must match target commit");
            assert_eq!(path, expected_dir, "added path must match cache path");
            assert!(
                expected_dir.is_dir(),
                "expected target directory {expected_dir:?} to exist"
            );
        }
        other => {
            panic!("expected Added, but got {other:?}");
        }
    }

    let catalog =
        read_personal_catalog(&catalog_path).expect("catalog must exist and be valid json");
    assert_eq!(catalog["schemaVersion"], 1);

    let plugins = catalog["plugins"]
        .as_array()
        .expect("plugins must be a JSON array");
    assert_eq!(plugins.len(), 1);

    let entry = &plugins[0];
    assert_eq!(entry["pluginId"], plugin_id);
    assert_eq!(entry["displayName"], plugin_id);
    assert_eq!(entry["sourceType"], "curated-upstream");
    assert_eq!(entry["installStrategy"], "personal-git-clone");
    assert_eq!(entry["source"], source_url);
    assert_eq!(entry["pin"], sha_a);
}
