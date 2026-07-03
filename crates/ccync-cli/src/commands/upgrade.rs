//! `ccync upgrade [<plugin>]` — upgrade installed git-source plugins
//! (brew-upgrade semantics).
//!
//! Thin handler: parse args, load the personal-plugin set, build the engine
//! seams (`git ls-remote` / `git clone` subprocesses + `run_unified_projection`),
//! call `ccync_engine::run_upgrade`, print the report, map the exit code.

use ccync_engine::{run_upgrade, ExitCode, UpgradeOpts, UpgradeReport};
use ccync_foundation::paths::{ccync_cache_root, personal_catalog_path, plugins_lock_path};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) fn cmd_upgrade(args: &[String]) -> ExitCode {
    if let Err(code) = super::args::reject_unknown_flags(
        "upgrade",
        args,
        &["--help", "-h", "--dry-run"],
        &[],
        1,
        "Usage: ccync upgrade [<plugin>] [--dry-run]",
    ) {
        return code;
    }
    let args = &args[1..]; // skip the "upgrade" verb

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("ccync upgrade [<plugin>] [--dry-run]");
        println!();
        println!("Upgrade installed git-source plugins (Homebrew `brew upgrade` semantics).");
        println!("  <plugin>    Upgrade only this plugin id (default: all git plugins).");
        println!("  --dry-run   Show what would be upgraded; clone/pin/project nothing.");
        return ExitCode::Success;
    }

    let dry_run = args.iter().any(|a| a == "--dry-run");
    let filter_id = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(|s| s.to_string());

    let opts = match build_opts(filter_id, dry_run) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("ccync upgrade: {e}");
            return ExitCode::Error;
        }
    };

    let report = run_upgrade(&opts, git_ls_remote, git_clone, project);
    report_exit(&report, dry_run)
}

/// Build [`UpgradeOpts`] from the on-disk personal-plugin state. Prefers the
/// lockfile (carries resolved pins); falls back to the personal catalog.
fn build_opts(filter_id: Option<String>, dry_run: bool) -> Result<UpgradeOpts, String> {
    let lock = plugins_lock_path();
    let catalog = personal_catalog_path();
    let cache_root = ccync_cache_root().ok_or("cannot resolve ~/.ccync/cache")?;
    let plugins = read_json(lock.as_deref())
        .or_else(|| read_json(catalog.as_deref()))
        .unwrap_or_else(|| serde_json::json!({}));
    let pin_files: Vec<PathBuf> = [lock, catalog]
        .into_iter()
        .flatten()
        .filter(|p| p.exists())
        .collect();
    Ok(UpgradeOpts {
        plugins,
        filter_id,
        dry_run,
        cache_root,
        pin_files,
    })
}

/// Read-only upgrade-available report (dry-run resolve): compares each git
/// plugin's remote HEAD against its pin, never cloning or projecting. Shared by
/// `list --upgrade-available`. On opts-build failure the report carries `error`.
pub(crate) fn upgrade_available_report() -> UpgradeReport {
    match build_opts(None, true) {
        Ok(opts) => run_upgrade(&opts, git_ls_remote, git_clone, project),
        Err(e) => UpgradeReport {
            error: Some(e),
            ..Default::default()
        },
    }
}

fn report_exit(report: &UpgradeReport, dry_run: bool) -> ExitCode {
    if let Some(err) = &report.error {
        eprintln!("ccync upgrade: {err}");
        return ExitCode::Error;
    }

    if dry_run {
        print!("{}", format_upgrade_view(report));
        return report_to_exit(report);
    }

    for id in &report.upgraded {
        println!("upgraded {id}");
    }
    for id in &report.held {
        println!("held {id} (pinned)");
    }
    for (id, reason) in &report.skipped {
        println!("skipped {id} ({reason})");
    }
    for (id, reason) in &report.failed {
        eprintln!("failed {id}: {reason}");
    }
    println!(
        "Summary: {} upgraded, {} unchanged, {} held, {} skipped, {} failed.{}",
        report.upgraded.len(),
        report.unchanged.len(),
        report.held.len(),
        report.skipped.len(),
        report.failed.len(),
        if report.projected { " Projected." } else { "" }
    );

    if report.failed.is_empty() {
        ExitCode::Success
    } else {
        ExitCode::Error
    }
}

fn short(sha: &str) -> String {
    sha.chars().take(12).collect()
}

/// Read-only exit semantics shared by `upgrade --dry-run` and
/// `list --upgrade-available`: a real error → `Error`; pending upgrades →
/// `UpdatesAvailable` (3); otherwise → `Success`. Lets a script tell
/// "updates available" apart from an operational failure.
pub(crate) fn report_to_exit(report: &UpgradeReport) -> ExitCode {
    if report.error.is_some() {
        ExitCode::Error
    } else if !report.pending.is_empty() {
        ExitCode::UpdatesAvailable
    } else {
        ExitCode::Success
    }
}

/// Render the upgrade-available view: one `id  current → latest  source` line
/// per pending change (current = pinned sha or `(unpinned)`), plus skipped
/// entries. Pure (returns the text) so it is testable and reusable by
/// `list --upgrade-available`.
pub(crate) fn format_upgrade_view(report: &UpgradeReport) -> String {
    let mut out = String::new();
    if report.pending.is_empty() {
        out.push_str("Everything up to date.\n");
    } else {
        out.push_str("Updates available:\n");
        for c in &report.pending {
            let current = c
                .old_sha
                .as_deref()
                .map(short)
                .unwrap_or_else(|| "(unpinned)".to_string());
            out.push_str(&format!(
                "  {}  {} → {}  {}\n",
                c.plugin_id,
                current,
                short(&c.new_sha),
                c.source
            ));
        }
    }
    for id in &report.held {
        out.push_str(&format!("  held {id} (pinned — run `ccync pin --remove {id}`)\n"));
    }
    for (id, reason) in &report.skipped {
        out.push_str(&format!("  skipped {id} ({reason})\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccync_engine::UpgradeChange;

    fn change(id: &str, old: &str, new: &str) -> UpgradeChange {
        UpgradeChange {
            plugin_id: id.into(),
            source: format!("https://github.com/u/{id}.git"),
            old_sha: Some(old.into()),
            new_sha: new.into(),
        }
    }

    #[test]
    fn report_to_exit_three_states() {
        assert_eq!(report_to_exit(&UpgradeReport::default()), ExitCode::Success);
        let pending = UpgradeReport {
            pending: vec![change("a", "oldsha111", "newsha222")],
            ..Default::default()
        };
        assert_eq!(report_to_exit(&pending), ExitCode::UpdatesAvailable);
        let err = UpgradeReport {
            error: Some("boom".into()),
            ..Default::default()
        };
        assert_eq!(report_to_exit(&err), ExitCode::Error);
    }

    #[test]
    fn format_upgrade_view_renders_current_to_latest() {
        let report = UpgradeReport {
            pending: vec![change("a", "oldsha111", "newsha222")],
            ..Default::default()
        };
        let out = format_upgrade_view(&report);
        assert!(out.contains('→'), "view must render current → latest arrow");
        assert!(out.contains("oldsha111") && out.contains("newsha222"));
        assert!(out.contains('a'));
    }
}

fn read_json(path: Option<&Path>) -> Option<Value> {
    let p = path?;
    let text = std::fs::read_to_string(p).ok()?;
    serde_json::from_str(&text).ok()
}

/// `git ls-remote <url> <ref>` → the remote object sha (first column).
fn git_ls_remote(url: &str, git_ref: &str) -> Result<String, String> {
    let out = Command::new("git")
        .args(["ls-remote", url, git_ref])
        .output()
        .map_err(|e| format!("spawn git ls-remote: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().next())
        .map(str::to_string)
        .ok_or_else(|| format!("git ls-remote {url} {git_ref}: empty result"))
}

/// `git clone --depth 1 <url> <dest>`.
fn git_clone(url: &str, dest: &Path) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let out = Command::new("git")
        .args([
            "clone",
            "--depth",
            "1",
            url,
            dest.to_str().ok_or("non-utf8 dest path")?,
        ])
        .output()
        .map_err(|e| format!("spawn git clone: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Reproject the managed set after pins change (reuses the unified engine).
fn project() -> Result<(), String> {
    match crate::commands::lifecycle::run_unified_projection(false, true, false) {
        ExitCode::Success => Ok(()),
        other => Err(format!("projection returned {other:?}")),
    }
}
