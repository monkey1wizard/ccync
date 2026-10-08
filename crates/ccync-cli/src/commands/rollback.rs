//! `ccync rollback` — internal/dev source repo rollback.
//!
//! Internal verb; kept out of public `--help`. Rolls back the ccync source repo to
//! the latest stable release tag (`v*`, prereleases excluded; fail-closed if
//! absent, and refuses a tag that is not an ancestor of HEAD), creates a
//! backup branch (`ccync-rollback-backup-<ts>`), then chains `refresh_with`.

use ccync_engine::ExitCode;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `ccync rollback [--yes]`
pub(crate) fn cmd_rollback(args: &[String]) -> ExitCode {
    if let Err(code) = super::args::reject_unknown_flags(
        "rollback",
        args,
        &["--help", "-h", "--yes"],
        &[],
        0,
        "Usage: ccync rollback [--yes]",
    ) {
        return code;
    }
    let args = &args[1..]; // skip "rollback" verb

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("ccync rollback [--yes]");
        println!();
        println!(
            "Internal/dev: roll back the ccync source repo to the latest stable release tag (v*),"
        );
        println!("create a backup branch, then rebuild derived outputs via refresh.");
        println!();
        println!("  --yes   Confirm destructive operation (required)");
        println!();
        println!("Prerequisites:");
        println!("  - Run from inside the ccync source repo");
        println!("  - at least one stable release tag (v*, excluding -rc) must exist");
        return ExitCode::Success;
    }

    let assume_yes = args.iter().any(|a| a == "--yes");

    // Locate the repo root — must be run from inside the ccync source repo.
    let repo_root = match find_repo_root() {
        Some(p) => p,
        None => {
            eprintln!("ccync rollback: not inside a git repository");
            return ExitCode::Usage;
        }
    };

    if !is_ccync_source_repo(&repo_root) {
        eprintln!(
            "ccync rollback: {} is not a ccync source repo \
            (expected Cargo.toml + crates/ccync-cli + plugins/catalog.json)",
            repo_root.display()
        );
        return ExitCode::Usage;
    }

    // Fail-closed resolution: latest stable release tag, ancestor-checked.
    let (marker_tag, marker_commit) = match resolve_marker(&repo_root) {
        Ok(tc) => tc,
        Err(e) => {
            eprintln!("ccync rollback: {e}");
            return ExitCode::Error;
        }
    };

    // Discard scope output.
    let (uncommitted, ahead) = discard_scope_counts(&repo_root, &marker_commit);
    println!("ccync rollback: discard scope");
    println!(
        "  target:      {} → {}",
        marker_tag,
        &marker_commit[..8.min(marker_commit.len())]
    );
    println!("  uncommitted: {} file(s)", uncommitted);
    println!("  ahead:       {} commit(s) beyond marker", ahead);

    if !assume_yes {
        eprintln!(
            "ccync rollback: pass --yes to confirm destructive rollback \
            (uncommitted changes and commits beyond the marker will be lost)"
        );
        return ExitCode::Usage;
    }

    // Backup branch (hard precondition — abort if it cannot be created).
    let backup_branch = create_backup_branch(&repo_root);
    let backup_branch = match backup_branch {
        Ok(b) => b,
        Err(e) => {
            eprintln!("ccync rollback: could not create backup branch: {e}");
            return ExitCode::Error;
        }
    };
    println!("ccync rollback: backup branch created → {backup_branch}");

    // git reset --hard <marker> + git clean -fd (A-semantics rollback).
    if let Err(e) = git_reset_hard(&repo_root, &marker_commit) {
        eprintln!("ccync rollback: git reset --hard failed: {e}");
        return ExitCode::Error;
    }
    if let Err(e) = git_clean_fd(&repo_root) {
        eprintln!("ccync rollback: git clean -fd failed: {e}");
        return ExitCode::Error;
    }
    println!(
        "ccync rollback: source rolled back to {}",
        &marker_commit[..8.min(marker_commit.len())]
    );

    // Chain refresh_with to rebuild derived layer.
    match crate::commands::refresh::refresh_with() {
        Ok(()) => {
            println!("ccync rollback: derived layer rebuilt via refresh");
            ExitCode::Success
        }
        Err(e) => {
            eprintln!("ccync rollback: source is at marker but derived rebuild failed: {e}");
            eprintln!("  Run `ccync refresh` manually to retry.");
            ExitCode::Error
        }
    }
}

/// Returns true when `path` looks like a ccync source repo.
/// Fingerprint: `Cargo.toml` AND `crates/ccync-cli` AND `plugins/catalog.json`.
pub(crate) fn is_ccync_source_repo(path: &Path) -> bool {
    path.join("Cargo.toml").is_file()
        && path.join("crates").join("ccync-cli").is_dir()
        && path.join("plugins").join("catalog.json").is_file()
}

/// Walk up from the current directory to find the git repo root (`.git` dir).
fn find_repo_root() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Fail-closed release-tag resolution. Finds the latest stable release tag
/// (`v*`, highest version first, prereleases like `-rc` excluded), verifies it
/// is an ancestor of HEAD (refusing a sideways move), and returns `(tag, commit)`.
/// The ancestor gate runs before discard-scope so `<commit>..HEAD` is meaningful.
fn resolve_marker(repo: &Path) -> Result<(String, String), String> {
    let list = Command::new("git")
        .args(["tag", "-l", "v*", "--sort=-v:refname"])
        .current_dir(repo)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !list.status.success() {
        return Err("cannot list git tags".to_string());
    }
    // Highest stable v* tag: prereleases contain '-' (e.g. v0.1.0-rc4) — exclude them.
    let tag = String::from_utf8_lossy(&list.stdout)
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty() && !l.contains('-'))
        .map(|s| s.to_string())
        .ok_or_else(|| {
            "no stable release tag (v*) found — create one with `git tag vX.Y.Z` \
            at a known-good commit"
                .to_string()
        })?;

    let verify = Command::new("git")
        .args(["rev-parse", "--verify", &format!("{tag}^{{commit}}")])
        .current_dir(repo)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !verify.status.success() {
        return Err(format!("cannot resolve release tag '{tag}'"));
    }
    let commit = String::from_utf8_lossy(&verify.stdout).trim().to_string();

    // Ancestor gate: refuse a tag that is not behind HEAD (avoids `git reset
    // --hard` moving sideways to an unrelated branch's release).
    let anc = Command::new("git")
        .args(["merge-base", "--is-ancestor", &commit, "HEAD"])
        .current_dir(repo)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !anc.status.success() {
        return Err(format!(
            "release tag '{tag}' ({}) is not an ancestor of HEAD — refusing to move \
            sideways; check out the release line before restoring",
            &commit[..8.min(commit.len())]
        ));
    }

    Ok((tag, commit))
}

/// Count uncommitted changes and commits ahead of the resolved target commit.
fn discard_scope_counts(repo: &Path, target: &str) -> (usize, usize) {
    let uncommitted = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().count())
        .unwrap_or(0);

    let ahead = Command::new("git")
        .args(["rev-list", "--count", &format!("{target}..HEAD")])
        .current_dir(repo)
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<usize>()
                .unwrap_or(0)
        })
        .unwrap_or(0);

    (uncommitted, ahead)
}

/// Create a backup branch pointing at the current HEAD.
/// Branch name: `ccync-restore-backup-<unix-seconds>`.
fn create_backup_branch(repo: &Path) -> Result<String, String> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let branch = format!("ccync-rollback-backup-{secs}");

    let out = Command::new("git")
        .args(["branch", &branch])
        .current_dir(repo)
        .output()
        .map_err(|e| e.to_string())?;

    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(branch)
}

/// `git reset --hard <commit>` in the repo root.
fn git_reset_hard(repo: &Path, commit: &str) -> Result<(), String> {
    let out = Command::new("git")
        .args(["reset", "--hard", commit])
        .current_dir(repo)
        .output()
        .map_err(|e| e.to_string())?;

    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(())
}

/// `git clean -fd` in the repo root.
fn git_clean_fd(repo: &Path) -> Result<(), String> {
    let out = Command::new("git")
        .args(["clean", "-fd"])
        .current_dir(repo)
        .output()
        .map_err(|e| e.to_string())?;

    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Create a minimal temp dir that looks like a ccync source repo.
    fn make_fake_ccync_repo() -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().unwrap();
        let r = tmp.path();
        fs::write(r.join("Cargo.toml"), "[workspace]").unwrap();
        fs::create_dir_all(r.join("crates").join("ccync-cli")).unwrap();
        fs::create_dir_all(r.join("plugins")).unwrap();
        fs::write(r.join("plugins").join("catalog.json"), "{}").unwrap();
        tmp
    }

    #[test]
    fn repo_shape_accepted_when_fingerprint_matches() {
        let tmp = make_fake_ccync_repo();
        assert!(is_ccync_source_repo(tmp.path()));
    }

    #[test]
    fn repo_shape_rejected_missing_cargo_toml() {
        let tmp = tempfile::TempDir::new().unwrap();
        let r = tmp.path();
        fs::create_dir_all(r.join("crates").join("ccync-cli")).unwrap();
        fs::create_dir_all(r.join("plugins")).unwrap();
        fs::write(r.join("plugins").join("catalog.json"), "{}").unwrap();
        assert!(!is_ccync_source_repo(r));
    }

    #[test]
    fn repo_shape_rejected_missing_ccync_cli_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let r = tmp.path();
        fs::write(r.join("Cargo.toml"), "[workspace]").unwrap();
        fs::create_dir_all(r.join("plugins")).unwrap();
        fs::write(r.join("plugins").join("catalog.json"), "{}").unwrap();
        assert!(!is_ccync_source_repo(r));
    }

    #[test]
    fn repo_shape_rejected_missing_catalog_json() {
        let tmp = tempfile::TempDir::new().unwrap();
        let r = tmp.path();
        fs::write(r.join("Cargo.toml"), "[workspace]").unwrap();
        fs::create_dir_all(r.join("crates").join("ccync-cli")).unwrap();
        fs::create_dir_all(r.join("plugins")).unwrap();
        assert!(!is_ccync_source_repo(r));
    }

    #[test]
    fn marker_fail_closed_on_missing_tag() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Init a real git repo without the marker tag.
        Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        let result = resolve_marker(tmp.path());
        assert!(
            result.is_err(),
            "resolution must fail when no stable release tag exists"
        );
        let msg = result.unwrap_err();
        assert!(
            msg.contains("release tag"),
            "error must mention the missing release tag; got: {msg}"
        );
    }

    #[test]
    fn no_yes_flag_returns_usage_and_zero_mutation() {
        let _guard = crate::test_io_lock(); // serialize process-cwd mutation
                                            // Run cmd_rollback without --yes in a non-repo dir; expects Usage.
                                            // (Will hit the is_ccync_source_repo guard before any git mutation.)
        let tmp = tempfile::TempDir::new().unwrap();
        let orig_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        let result = cmd_rollback(&["rollback".to_string()]);

        std::env::set_current_dir(&orig_dir).unwrap();
        // Not a ccync source repo → Usage (guard fires before --yes check)
        assert_eq!(
            result,
            ExitCode::Usage,
            "non-repo without --yes must be Usage"
        );
    }

    #[test]
    fn no_yes_flag_in_ccync_repo_without_marker_is_error() {
        let _guard = crate::test_io_lock(); // serialize process-cwd mutation
                                            // Set up a fake ccync repo with a real git history but no marker tag.
                                            // Marker resolution happens before the --yes guard (to compute discard scope),
                                            // so missing marker → Error regardless of --yes.
        let tmp = tempfile::TempDir::new().unwrap();
        let r = tmp.path();

        // Plant fingerprint files.
        fs::write(r.join("Cargo.toml"), "[workspace]").unwrap();
        fs::create_dir_all(r.join("crates").join("ccync-cli")).unwrap();
        fs::create_dir_all(r.join("plugins")).unwrap();
        fs::write(r.join("plugins").join("catalog.json"), "{}").unwrap();

        // Init git repo.
        Command::new("git")
            .args(["init"])
            .current_dir(r)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test"])
            .current_dir(r)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(r)
            .output()
            .unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(r)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(r)
            .output()
            .unwrap();

        let orig_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(r).unwrap();

        let result = cmd_rollback(&["rollback".to_string()]);

        std::env::set_current_dir(&orig_dir).unwrap();
        // Marker absent → Error (fail-closed; no git mutation occurred).
        assert_eq!(
            result,
            ExitCode::Error,
            "missing marker must return Error (fail-closed)"
        );
    }

    #[test]
    fn no_yes_flag_with_valid_marker_returns_usage_zero_mutation() {
        let _guard = crate::test_io_lock(); // serialize process-cwd mutation
                                            // Set up a fake ccync repo with the marker tag present but no --yes.
                                            // With marker present, discard scope is computed, then --yes guard fires.
        let tmp = tempfile::TempDir::new().unwrap();
        let r = tmp.path();

        fs::write(r.join("Cargo.toml"), "[workspace]").unwrap();
        fs::create_dir_all(r.join("crates").join("ccync-cli")).unwrap();
        fs::create_dir_all(r.join("plugins")).unwrap();
        fs::write(r.join("plugins").join("catalog.json"), "{}").unwrap();

        Command::new("git")
            .args(["init"])
            .current_dir(r)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test"])
            .current_dir(r)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(r)
            .output()
            .unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(r)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(r)
            .output()
            .unwrap();
        Command::new("git")
            .args(["tag", "v0.1.0"])
            .current_dir(r)
            .output()
            .unwrap();

        // Capture HEAD before the command runs.
        let head_before = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(r)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();

        let orig_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(r).unwrap();

        let result = cmd_rollback(&["rollback".to_string()]);

        let head_after = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(r)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();

        std::env::set_current_dir(&orig_dir).unwrap();

        assert_eq!(result, ExitCode::Usage, "--yes absent must return Usage");
        assert_eq!(
            head_before, head_after,
            "HEAD must not change without --yes (zero mutation)"
        );
    }

    #[test]
    fn rollback_with_yes_produces_a_semantics() {
        // Full destructive happy-path: --yes WITH a valid marker must
        //   (1) create a backup branch pointing at the original (pre-rollback) HEAD,
        //   (2) reset the worktree to the marker commit,
        //   (3) remove untracked files via `git clean -fd`,
        //   (4) chain refresh to rebuild the derived layer,
        // and return Success. Holds the crate-level lock because refresh_with
        // redirects HOME to write the canonical render.
        let _guard = crate::test_io_lock();

        let repo = tempfile::TempDir::new().unwrap();
        let r = repo.path();

        // ccync source-repo fingerprint.
        fs::write(r.join("Cargo.toml"), "[workspace]").unwrap();
        fs::create_dir_all(r.join("crates").join("ccync-cli")).unwrap();
        fs::create_dir_all(r.join("plugins")).unwrap();
        fs::write(r.join("plugins").join("catalog.json"), "{}").unwrap();

        let git = |a: &[&str]| Command::new("git").args(a).current_dir(r).output().unwrap();
        git(&["init"]);
        git(&["config", "user.email", "test@test"]);
        git(&["config", "user.name", "test"]);
        git(&["add", "."]);
        git(&["commit", "-m", "known-good"]);
        git(&["tag", "v0.1.0"]);

        let marker_commit = String::from_utf8_lossy(
            &Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(r)
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();

        // Advance HEAD beyond the marker (a committed change to discard).
        fs::write(r.join("ahead.txt"), "beyond marker").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "ahead of marker"]);
        let ahead_head = String::from_utf8_lossy(
            &Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(r)
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();

        // Untracked file that `git clean -fd` must remove.
        fs::write(r.join("untracked.txt"), "dirty").unwrap();

        // Redirect HOME so refresh_with writes the render into a temp dir.
        let home = tempfile::TempDir::new().unwrap();
        let orig_home;
        #[cfg(windows)]
        unsafe {
            orig_home = std::env::var("USERPROFILE").ok();
            std::env::set_var("USERPROFILE", home.path());
            std::env::set_var("HOME", home.path());
        }
        #[cfg(not(windows))]
        unsafe {
            orig_home = std::env::var("HOME").ok();
            std::env::set_var("HOME", home.path());
        }

        let orig_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(r).unwrap();

        let result = cmd_rollback(&["rollback".to_string(), "--yes".to_string()]);

        let head_after = String::from_utf8_lossy(
            &Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(r)
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        let backup_branch = String::from_utf8_lossy(
            &Command::new("git")
                .args([
                    "for-each-ref",
                    "--format=%(refname:short)",
                    "refs/heads/ccync-rollback-backup-*",
                ])
                .current_dir(r)
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        let backup_sha = String::from_utf8_lossy(
            &Command::new("git")
                .args(["rev-parse", &backup_branch])
                .current_dir(r)
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();

        std::env::set_current_dir(&orig_dir).unwrap();
        #[cfg(windows)]
        unsafe {
            match &orig_home {
                Some(v) => {
                    std::env::set_var("USERPROFILE", v);
                    std::env::set_var("HOME", v);
                }
                None => {
                    std::env::remove_var("USERPROFILE");
                    std::env::remove_var("HOME");
                }
            }
        }
        #[cfg(not(windows))]
        unsafe {
            match &orig_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }

        assert_eq!(
            result,
            ExitCode::Success,
            "--yes with valid marker must succeed"
        );
        assert_eq!(
            head_after, marker_commit,
            "worktree must be reset to the marker commit"
        );
        assert!(!backup_branch.is_empty(), "a backup branch must be created");
        assert_eq!(
            backup_sha, ahead_head,
            "backup branch must point at the original pre-rollback HEAD"
        );
        assert!(
            !r.join("untracked.txt").exists(),
            "git clean -fd must remove untracked files"
        );
        assert!(
            home.path()
                .join(".ccync")
                .join("build")
                .join("render")
                .is_dir(),
            "refresh chain must rebuild the derived render layer"
        );
    }

    #[test]
    fn rc_only_tags_are_fail_closed() {
        // Only a prerelease tag exists → no stable release → fail-closed.
        let tmp = tempfile::TempDir::new().unwrap();
        let r = tmp.path();
        let git = |a: &[&str]| Command::new("git").args(a).current_dir(r).output().unwrap();
        git(&["init"]);
        git(&["config", "user.email", "test@test"]);
        git(&["config", "user.name", "test"]);
        git(&["commit", "--allow-empty", "-m", "base"]);
        git(&["tag", "v0.1.0-rc1"]);

        let result = resolve_marker(r);
        assert!(
            result.is_err(),
            "prerelease-only tags must not resolve as a stable release"
        );
        assert!(
            result.unwrap_err().contains("release tag"),
            "error must mention the missing stable release tag"
        );
    }

    #[test]
    fn non_ancestor_release_tag_is_refused() {
        // A release tag on a divergent branch, not reachable from HEAD → refuse.
        let tmp = tempfile::TempDir::new().unwrap();
        let r = tmp.path();
        let git = |a: &[&str]| Command::new("git").args(a).current_dir(r).output().unwrap();
        git(&["init"]);
        git(&["config", "user.email", "test@test"]);
        git(&["config", "user.name", "test"]);
        git(&["commit", "--allow-empty", "-m", "base"]);
        git(&["checkout", "-b", "sidebranch"]);
        git(&["commit", "--allow-empty", "-m", "side"]);
        git(&["tag", "v9.9.9"]);
        git(&["checkout", "-"]); // back to base branch; v9.9.9 is not an ancestor of HEAD

        let result = resolve_marker(r);
        assert!(
            result.is_err(),
            "a non-ancestor release tag must be refused"
        );
        assert!(
            result.unwrap_err().contains("ancestor"),
            "error must explain the tag is not an ancestor of HEAD"
        );
    }
}
