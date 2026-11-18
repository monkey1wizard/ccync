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
    std::fs::create_dir_all(origin_path.join("skills")).unwrap();
    std::fs::write(origin_path.join("skills/test.md"), "# test\n").unwrap();
    run_git(origin_path, &["-c", "user.name=t", "-c", "user.email=t@t", "add", "."]);
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

#[test]
fn add_empty_git_source_refuses_without_changing_catalog() {
    let origin_dir = tempdir().expect("tempdir for origin repo");
    let origin_path = origin_dir.path();
    run_git(origin_path, &["init"]);
    run_git(
        origin_path,
        &[
            "-c", "user.name=t", "-c", "user.email=t@t", "commit", "--allow-empty", "-m",
            "empty source",
        ],
    );
    let sha = run_git(origin_path, &["rev-parse", "HEAD"]);
    let cache = tempdir().unwrap();
    let catalog = tempdir().unwrap();
    let catalog_path = catalog.path().join("plugins.json");
    let original = b"{\"schemaVersion\":1,\"plugins\":[]}\n";
    std::fs::write(&catalog_path, original).unwrap();

    let result = add_plugin_entry(
        cache.path(),
        &catalog_path,
        "empty-plugin",
        origin_path.to_str().unwrap(),
        Some(&sha),
    );

    assert!(matches!(result, AddResult::Failed(message) if message.contains("no renderable contribution")));
    assert_eq!(std::fs::read(&catalog_path).unwrap(), original);
    assert!(cache.path().join(format!("empty-plugin@{sha}")).is_dir(), "fetched cache residue is rebuildable and may remain");
}

#[test]
fn add_corrupt_mcp_manifest_refuses_without_changing_catalog() {
    let origin_dir = tempdir().expect("tempdir for origin repo");
    let origin_path = origin_dir.path();
    run_git(origin_path, &["init"]);
    std::fs::write(origin_path.join(".mcp.json"), "{invalid").unwrap();
    run_git(
        origin_path,
        &["-c", "user.name=t", "-c", "user.email=t@t", "add", "."],
    );
    run_git(
        origin_path,
        &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-m", "corrupt manifest"],
    );
    let sha = run_git(origin_path, &["rev-parse", "HEAD"]);
    let cache = tempdir().unwrap();
    let catalog = tempdir().unwrap();
    let catalog_path = catalog.path().join("plugins.json");
    let original = b"{\"schemaVersion\":1,\"plugins\":[]}\n";
    std::fs::write(&catalog_path, original).unwrap();

    let result = add_plugin_entry(
        cache.path(),
        &catalog_path,
        "corrupt-plugin",
        origin_path.to_str().unwrap(),
        Some(&sha),
    );

    assert!(matches!(result, AddResult::Failed(message) if message.contains("inspect source contribution") && !message.contains("no renderable contribution")));
    assert_eq!(std::fs::read(&catalog_path).unwrap(), original);
}

#[test]
fn add_mcp_only_git_source_succeeds() {
    let origin_dir = tempdir().expect("tempdir for origin repo");
    let origin_path = origin_dir.path();
    run_git(origin_path, &["init"]);
    std::fs::write(
        origin_path.join(".mcp.json"),
        r#"{"mcpServers":{"remote":{"type":"http","url":"https://example.test/mcp"}}}"#,
    )
    .unwrap();
    run_git(origin_path, &["-c", "user.name=t", "-c", "user.email=t@t", "add", "."]);
    run_git(origin_path, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-m", "MCP only"]);
    let sha = run_git(origin_path, &["rev-parse", "HEAD"]);
    let cache = tempdir().unwrap();
    let catalog = tempdir().unwrap();
    let catalog_path = catalog.path().join("plugins.json");

    let result = add_plugin_entry(
        cache.path(),
        &catalog_path,
        "mcp-plugin",
        origin_path.to_str().unwrap(),
        Some(&sha),
    );

    assert!(matches!(result, AddResult::Added { .. }));
    let value = read_personal_catalog(&catalog_path).unwrap();
    assert_eq!(value["plugins"].as_array().unwrap().len(), 1);
}

#[test]
fn add_plugin_entry_rejects_invalid_or_empty_before_catalog_write() {
    add_empty_git_source_refuses_without_changing_catalog();
    add_corrupt_mcp_manifest_refuses_without_changing_catalog();
    add_mcp_only_git_source_succeeds();
}
