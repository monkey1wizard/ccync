use ccync_engine::install::{materialize_plugin, PersonalFetchResult};
use std::collections::BTreeSet;
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

fn fixture_repo() -> (tempfile::TempDir, String) {
    let repo = tempdir().expect("tempdir for origin repo");
    let repo_path = repo.path();
    run_git(repo_path, &["init"]);

    // Fixture subtree plugins/x
    let sub = repo_path.join("plugins/x");
    std::fs::create_dir_all(sub.join("skills/s")).expect("create skills dir");
    std::fs::write(sub.join("skills/s/SKILL.md"), "skill s").expect("write skill");
    std::fs::create_dir_all(sub.join("agents")).expect("create agents dir");
    std::fs::write(sub.join("agents/a.md"), "---\nname: a\n---\n").expect("write agent");
    std::fs::create_dir_all(sub.join("commands")).expect("create commands dir");
    std::fs::write(sub.join("commands/c.md"), "---\ndescription: c\n---\n").expect("write command");
    std::fs::write(sub.join(".mcp.json"), r#"{"mcpServers":{}}"#).expect("write mcp.json");

    // Other subtree outside plugins/x
    std::fs::create_dir_all(repo_path.join("other")).expect("create other subtree");
    std::fs::write(repo_path.join("other/README.md"), "other").expect("write other file");

    run_git(
        repo_path,
        &["-c", "user.name=t", "-c", "user.email=t@t", "add", "."],
    );
    run_git(
        repo_path,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "initial",
        ],
    );
    let sha = run_git(repo_path, &["rev-parse", "HEAD"]);
    (repo, sha)
}

#[test]
fn materialize_plugin_unpinned_keeps_only_selected_subpath_and_exact_entries() {
    let (repo, sha) = fixture_repo();
    let cache = tempdir().expect("tempdir for cache root");
    let result = materialize_plugin(
        cache.path(),
        "x",
        repo.path().to_str().expect("utf-8 repo path"),
        Some("plugins/x"),
        None,
    );

    let target = cache.path().join(format!("x@{sha}"));
    match result {
        PersonalFetchResult::Cloned { path, sha: actual } => {
            assert_eq!(path, target);
            assert_eq!(actual, sha);
        }
        other => panic!("expected Cloned, got {other:?}"),
    }

    assert!(target.is_dir(), "target directory must exist");

    // cache/x@<sha> lists exactly skills, agents, commands, .mcp.json
    let entries: BTreeSet<String> = std::fs::read_dir(&target)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    let expected: BTreeSet<String> = [
        "skills".to_string(),
        "agents".to_string(),
        "commands".to_string(),
        ".mcp.json".to_string(),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        entries, expected,
        "target must list exactly the expected subtree entries"
    );
    assert!(
        !target.join("other").exists(),
        "other directory must not be present"
    );
    assert!(
        !cache.path().join(".x.staging").exists(),
        "staging dir must be cleaned up"
    );
}

#[test]
fn materialize_plugin_pinned_lands_at_pinned_sha() {
    let (repo, sha) = fixture_repo();
    let cache = tempdir().expect("tempdir for cache root");
    let result = materialize_plugin(
        cache.path(),
        "x",
        repo.path().to_str().expect("utf-8 repo path"),
        Some("plugins/x"),
        Some(&sha),
    );

    let target = cache.path().join(format!("x@{sha}"));
    match result {
        PersonalFetchResult::Cloned { path, sha: actual } => {
            assert_eq!(path, target);
            assert_eq!(actual, sha);
        }
        other => panic!("expected Cloned, got {other:?}"),
    }
    assert!(target.join(".mcp.json").exists());
}

#[test]
fn materialize_plugin_cleans_leftover_staging_first() {
    let (repo, sha) = fixture_repo();
    let cache = tempdir().expect("tempdir for cache root");
    let staging_dir = cache.path().join(".x.staging");
    std::fs::create_dir_all(&staging_dir).expect("create fake leftover staging dir");
    std::fs::write(staging_dir.join("stale.txt"), "stale").expect("write stale file");
    assert!(staging_dir.exists());

    let result = materialize_plugin(
        cache.path(),
        "x",
        repo.path().to_str().expect("utf-8 repo path"),
        Some("plugins/x"),
        Some(&sha),
    );

    let target = cache.path().join(format!("x@{sha}"));
    match result {
        PersonalFetchResult::Cloned { path, sha: actual } => {
            assert_eq!(path, target);
            assert_eq!(actual, sha);
        }
        other => panic!("expected Cloned, got {other:?}"),
    }
    assert!(!staging_dir.exists(), "staging must be cleaned up");
    assert!(
        !target.join("stale.txt").exists(),
        "stale file from previous staging must not exist"
    );
}

#[test]
fn materialize_plugin_returns_already_present_if_target_exists() {
    let (repo, sha) = fixture_repo();
    let cache = tempdir().expect("tempdir for cache root");
    let target = cache.path().join(format!("x@{sha}"));
    std::fs::create_dir_all(&target).expect("pre-create target");
    std::fs::write(target.join("existing.txt"), "existing").expect("write marker");

    let result = materialize_plugin(
        cache.path(),
        "x",
        repo.path().to_str().expect("utf-8 repo path"),
        Some("plugins/x"),
        Some(&sha),
    );

    match result {
        PersonalFetchResult::AlreadyPresent(path) => {
            assert_eq!(path, target);
        }
        other => panic!("expected AlreadyPresent, got {other:?}"),
    }
    assert!(
        !cache.path().join(".x.staging").exists(),
        "staging must be cleaned up"
    );
}

#[test]
fn materialize_plugin_rejects_lexically_invalid_subpath_before_writing() {
    let cache = tempdir().expect("tempdir for cache root");
    let result = materialize_plugin(
        cache.path(),
        "x",
        "not-a-real-repository",
        Some("../outside"),
        None,
    );

    match result {
        PersonalFetchResult::CloneFailed(message) => {
            assert!(
                message.contains("../outside"),
                "error must name subpath: {message}"
            );
        }
        other => panic!("expected CloneFailed for ../outside, got {other:?}"),
    }
    assert!(!cache.path().join(".x.staging").exists());
    let x_entries: Vec<_> = std::fs::read_dir(cache.path())
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().to_string_lossy().to_string();
            if name.starts_with("x@") || name == ".x.staging" {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    assert!(
        x_entries.is_empty(),
        "cache must hold no x@* and no .x.staging"
    );
}

#[test]
fn materialize_plugin_rejects_missing_subpath_in_checkout() {
    let (repo, _) = fixture_repo();
    let cache = tempdir().expect("tempdir for cache root");
    let result = materialize_plugin(
        cache.path(),
        "x",
        repo.path().to_str().expect("utf-8 repo path"),
        Some("plugins/missing"),
        None,
    );

    match result {
        PersonalFetchResult::CloneFailed(message) => {
            assert!(
                message.contains("plugins/missing"),
                "error must name subpath: {message}"
            );
            assert!(
                message.contains("x"),
                "error must name plugin id: {message}"
            );
        }
        other => panic!("expected CloneFailed for missing subpath, got {other:?}"),
    }
    assert!(
        !cache.path().join(".x.staging").exists(),
        "staging must be removed"
    );
    let x_entries: Vec<_> = std::fs::read_dir(cache.path())
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().to_string_lossy().to_string();
            if name.starts_with("x@") || name == ".x.staging" {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    assert!(
        x_entries.is_empty(),
        "cache must hold no x@* and no .x.staging"
    );
}

#[test]
fn materialize_plugin_subpath_none_works_as_whole_repo_clone() {
    let (repo, sha) = fixture_repo();
    let cache = tempdir().expect("tempdir for cache root");
    let result = materialize_plugin(
        cache.path(),
        "x",
        repo.path().to_str().expect("utf-8 repo path"),
        None,
        Some(&sha),
    );

    let target = cache.path().join(format!("x@{sha}"));
    match result {
        PersonalFetchResult::Cloned { path, sha: actual } => {
            assert_eq!(path, target);
            assert_eq!(actual, sha);
        }
        other => panic!("expected Cloned for subpath None, got {other:?}"),
    }
    assert!(target.join("plugins/x/.mcp.json").exists());
    assert!(target.join("other/README.md").exists());
}

#[cfg(unix)]
#[test]
fn materialize_plugin_rejects_symlink_escape_and_cleans_staging() {
    let (repo, sha) = fixture_repo();
    let sub = repo.path().join("plugins/x");
    std::os::unix::fs::symlink("../../other", sub.join("skills_link"))
        .expect("create escaping symlink");
    run_git(
        repo.path(),
        &["-c", "user.name=t", "-c", "user.email=t@t", "add", "."],
    );
    run_git(
        repo.path(),
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "escape",
        ],
    );
    let new_sha = run_git(repo.path(), &["rev-parse", "HEAD"]);
    assert_ne!(sha, new_sha);

    let cache = tempdir().expect("tempdir for cache root");
    let result = materialize_plugin(
        cache.path(),
        "x",
        repo.path().to_str().expect("utf-8 repo path"),
        Some("plugins/x"),
        Some(&new_sha),
    );

    match result {
        PersonalFetchResult::CloneFailed(message) => {
            assert!(
                message.contains("skills_link"),
                "error must name link: {message}"
            );
            assert!(
                message.contains("inside"),
                "error must name containment rule: {message}"
            );
        }
        other => panic!("expected symlink rejection, got {other:?}"),
    }
    assert!(!cache.path().join(format!("x@{new_sha}")).exists());
    assert!(!cache.path().join(".x.staging").exists());
}

#[cfg(unix)]
#[test]
fn materialize_plugin_rejects_subpath_symlink_pointing_outside() {
    let (repo, sha) = fixture_repo();
    std::fs::create_dir_all(repo.path().join("external_dir")).expect("create external dir");
    std::fs::write(repo.path().join("external_dir/file.txt"), "ext").expect("write file");
    std::os::unix::fs::symlink("../external_dir", repo.path().join("plugins/symlink_sub"))
        .expect("create subpath symlink");
    run_git(
        repo.path(),
        &["-c", "user.name=t", "-c", "user.email=t@t", "add", "."],
    );
    run_git(
        repo.path(),
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "symlink subpath",
        ],
    );
    let new_sha = run_git(repo.path(), &["rev-parse", "HEAD"]);
    assert_ne!(sha, new_sha);

    let cache = tempdir().expect("tempdir for cache root");
    let result = materialize_plugin(
        cache.path(),
        "x",
        repo.path().to_str().expect("utf-8 repo path"),
        Some("plugins/symlink_sub"),
        Some(&new_sha),
    );

    match result {
        PersonalFetchResult::CloneFailed(message) => {
            assert!(
                message.contains("symlink_sub") || message.contains("plugins"),
                "error must name subpath/link: {message}"
            );
        }
        other => panic!("expected symlink subpath rejection, got {other:?}"),
    }
    assert!(!cache.path().join(format!("x@{new_sha}")).exists());
    assert!(!cache.path().join(".x.staging").exists());
}

#[cfg(unix)]
#[test]
fn materialize_plugin_rejects_subpath_symlink_pointing_inside() {
    let (repo, sha) = fixture_repo();
    let selected_target = repo.path().join("plugins/real_sub");
    std::fs::create_dir_all(&selected_target).expect("create selected symlink target");
    std::fs::write(selected_target.join("README.md"), "real").expect("write selected target");
    std::os::unix::fs::symlink("real_sub", repo.path().join("plugins/symlink_sub"))
        .expect("create in-tree subpath symlink");
    run_git(
        repo.path(),
        &["-c", "user.name=t", "-c", "user.email=t@t", "add", "."],
    );
    run_git(
        repo.path(),
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "in-tree symlink subpath",
        ],
    );
    let new_sha = run_git(repo.path(), &["rev-parse", "HEAD"]);
    assert_ne!(sha, new_sha);

    let cache = tempdir().expect("tempdir for cache root");
    let result = materialize_plugin(
        cache.path(),
        "x",
        repo.path().to_str().expect("utf-8 repo path"),
        Some("plugins/symlink_sub"),
        Some(&new_sha),
    );

    match result {
        PersonalFetchResult::CloneFailed(message) => {
            assert!(
                message.contains("symlink_sub"),
                "error must name subpath: {message}"
            );
            assert!(
                message.contains("symlink"),
                "error must name symlink rule: {message}"
            );
        }
        other => panic!("expected in-tree symlink subpath rejection, got {other:?}"),
    }
    assert!(!cache.path().join(format!("x@{new_sha}")).exists());
    assert!(!cache.path().join(".x.staging").exists());
}

#[cfg(unix)]
#[test]
fn materialize_plugin_rejects_dangling_symlink() {
    let (repo, _) = fixture_repo();
    std::os::unix::fs::symlink("missing", repo.path().join("plugins/x/dangling"))
        .expect("create dangling symlink");
    run_git(
        repo.path(),
        &["-c", "user.name=t", "-c", "user.email=t@t", "add", "."],
    );
    run_git(
        repo.path(),
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "dangling",
        ],
    );
    let sha = run_git(repo.path(), &["rev-parse", "HEAD"]);
    let cache = tempdir().expect("tempdir for cache root");
    let result = materialize_plugin(
        cache.path(),
        "x",
        repo.path().to_str().expect("utf-8 repo path"),
        Some("plugins/x"),
        Some(&sha),
    );

    match result {
        PersonalFetchResult::CloneFailed(message) => {
            assert!(
                message.contains("dangling"),
                "error must name link: {message}"
            );
            assert!(
                message.contains("canonical"),
                "error must name canonical failure: {message}"
            );
        }
        other => panic!("expected dangling-link rejection, got {other:?}"),
    }
    assert!(!cache.path().join(".x.staging").exists());
}
