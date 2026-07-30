//! `ccync upgrade [<plugin>]` — upgrade installed git-source plugins
//! (brew-upgrade semantics).
//!
//! Thin handler: parse args, load the personal-plugin set, build the engine
//! seams (`git ls-remote` / `git clone` subprocesses + `run_unified_projection`),
//! call `ccync_engine::run_upgrade`, print the report, map the exit code.

use ccync_engine::adopt::adopted_items_without_source;
use ccync_engine::{
    resolve_adopted_upgrades, run_upgrade, AdoptedUpgradeOutcome, ExitCode, UpgradeOpts,
    UpgradeReport,
};
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

    // Machine-initialization preflight: a non-dry upgrade clones/re-pins git
    // plugins and reprojects the managed set to live agent surfaces, so it
    // must not run before `ccync init` — checked here, before adopted
    // resolution or `build_opts`/`run_upgrade` (the clone/pin path), so a
    // refusal is a true zero-mutation abort. `--dry-run` is exempt, mirroring
    // `sync --dry-run`'s exemption from the same preflight.
    if !dry_run {
        if let Err(code) = super::lifecycle::require_initialized_machine() {
            return code;
        }
    }

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

    // Adopted-item classification is read-only reporting: resolved
    // unconditionally, on every invocation, from the same lockfile `Value`
    // already loaded for the personal-plugin set. Never affects the exit code.
    let adopted = resolve_adopted_upgrades(&opts.plugins);
    let unresolved = unresolved_adopted_names();

    let report = run_upgrade(&opts, git_ls_remote, git_clone, project);
    report_exit(&report, &adopted, &unresolved, dry_run)
}

/// Lock-derived names of adopted items with no recorded source, read
/// via `ccync_engine::adopt::adopted_items_without_source`. Covers every
/// adopted kind (plugin, MCP, skill) uniformly — unlike the removed
/// plugin-only `unresolved_repair_hint`. Graceful-missing: an unresolvable
/// lock path or unreadable/malformed lock file yields an empty vec, never a
/// panic or a false tip.
fn unresolved_adopted_names() -> Vec<String> {
    plugins_lock_path()
        .and_then(|p| adopted_items_without_source(&p).ok())
        .unwrap_or_default()
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

/// Read-only adopted-item classification (detect + report only) sharing
/// the same lockfile `Value` as [`upgrade_available_report`]. Returns an empty
/// list when the lockfile/catalog cannot be loaded (mirrors the `error`
/// carried by `upgrade_available_report` in that same case).
pub(crate) fn upgrade_available_adopted() -> Vec<AdoptedUpgradeOutcome> {
    match build_opts(None, true) {
        Ok(opts) => resolve_adopted_upgrades(&opts.plugins),
        Err(_) => Vec::new(),
    }
}

/// Lock-derived unresolved adopted-item names for `list --upgrade-available`,
/// sharing [`unresolved_adopted_names`]'s graceful-missing semantics.
pub(crate) fn upgrade_available_unresolved() -> Vec<String> {
    unresolved_adopted_names()
}

fn report_exit(
    report: &UpgradeReport,
    adopted: &[AdoptedUpgradeOutcome],
    unresolved: &[String],
    dry_run: bool,
) -> ExitCode {
    if let Some(err) = &report.error {
        eprintln!("ccync upgrade: {err}");
        return ExitCode::Error;
    }

    if dry_run {
        print!("{}", format_upgrade_view(report, adopted, unresolved));
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

    print!("{}", format_adopted_section(adopted));
    let mut tip = String::new();
    push_repair_tip(&mut tip, unresolved);
    print!("{tip}");

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
/// entries, plus the adopted-item section. Pure (returns the text) so it is
/// testable and reusable by `list --upgrade-available`.
pub(crate) fn format_upgrade_view(
    report: &UpgradeReport,
    adopted: &[AdoptedUpgradeOutcome],
    unresolved: &[String],
) -> String {
    let mut out = String::new();
    let tracked_count = report.unchanged.len()
        + report.held.len()
        + report.skipped.len()
        + report.failed.len()
        + adopted.len();
    if report.pending.is_empty() {
        if tracked_count == 0 {
            out.push_str("No plugins to check for updates.\n");
        } else {
            out.push_str("Everything up to date.\n");
        }
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
        out.push_str("  run `ccync upgrade` to apply\n");
    }
    for id in &report.held {
        out.push_str(&format!(
            "  held {id} (pinned — run `ccync pin --remove {id}`)\n"
        ));
    }
    for (id, reason) in &report.skipped {
        out.push_str(&format!("  skipped {id} ({reason})\n"));
    }
    out.push_str(&format_adopted_section(adopted));
    push_repair_tip(&mut out, unresolved);
    out
}

/// Render the adopted-item report-only section (detect + report, never
/// auto-apply). `Changed` prints `current -> latest` plus the two activation
/// paths — Claude's own `/plugin` UI, or `ccync add <source>` to take over
/// management. `Unchanged` is omitted entirely, mirroring how the personal-
/// plugin view above never prints unchanged plugins either. `Skipped` mirrors
/// the personal `skipped {id} ({reason})` line style. Pure and testable;
/// never influences `ExitCode` (adopted items have no "pending" concept in
/// [`UpgradeReport`]).
///
/// This no longer prints a per-item repair hint (removed with
/// `AdoptedUpgradeOutcome::unresolved_repair_hint`, which was plugin-only);
/// [`push_repair_tip`] appends a single lock-derived tip line covering every
/// kind after this section instead.
pub(crate) fn format_adopted_section(adopted: &[AdoptedUpgradeOutcome]) -> String {
    let mut out = String::new();
    for outcome in adopted {
        match outcome {
            AdoptedUpgradeOutcome::Changed {
                name,
                source,
                current_version,
                latest_version,
                takeover_supported,
            } => {
                let current = current_version.as_deref().unwrap_or("(unknown)");
                out.push_str(&format!(
                    "  adopted {name}  {current} -> {latest_version}  {source}\n"
                ));
                if *takeover_supported {
                    out.push_str(
                        "    update via Claude Code's own /plugin, or take over management: ",
                    );
                    out.push_str(&format!("ccync add {source}\n"));
                } else {
                    out.push_str(
                        "    update via Claude Code's own /plugin (plugin lives inside the marketplace repo — `ccync add` cannot fetch it directly)\n",
                    );
                }
            }
            AdoptedUpgradeOutcome::Unchanged { .. } => {}
            AdoptedUpgradeOutcome::VersionUnresolved { name, source } => {
                out.push_str(&format!(
                    "  adopted {name} version-unresolved (source: {source})\n"
                ));
            }
            AdoptedUpgradeOutcome::Skipped { name, reason } => {
                out.push_str(&format!("  adopted {name} skipped ({reason})\n"));
            }
        }
    }
    out
}

/// Append a single `Tip: run \`ccync repair\`` line when `unresolved` (from
/// `ccync_engine::adopt::adopted_items_without_source`, lock-derived and
/// covering plugin/MCP/skill uniformly) is non-empty. Printed once per
/// invocation regardless of how many rows are unresolved — never per-item,
/// unlike the removed plugin-only hint.
pub(crate) fn push_repair_tip(out: &mut String, unresolved: &[String]) {
    if !unresolved.is_empty() {
        out.push_str("Tip: run `ccync repair` to resolve missing adopted-item sources.\n");
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
    match crate::commands::lifecycle::run_unified_projection(
        false,
        true,
        false,
        Some(&crate::commands::lifecycle::SYNC_GATE_ABORT_CONTEXT),
    ) {
        ExitCode::Success => Ok(()),
        other => Err(format!("projection returned {other:?}")),
    }
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
        let out = format_upgrade_view(&report, &[], &[]);
        assert!(out.contains('→'), "view must render current → latest arrow");
        assert!(out.contains("oldsha111") && out.contains("newsha222"));
        assert!(out.contains('a'));
    }

    // ── adopted classifications + activation commands ─────────────────────────

    #[test]
    fn adopted_changed_prints_both_activation_commands_and_versions() {
        let adopted = vec![AdoptedUpgradeOutcome::Changed {
            name: "tool@market".into(),
            source: "https://github.com/example/tool-plugin.git".into(),
            current_version: Some("2.0.0".into()),
            latest_version: "2.1.0".into(),
            takeover_supported: true,
        }];
        let out = format_adopted_section(&adopted);
        assert!(out.contains("tool@market"), "must name the adopted item");
        assert!(
            out.contains("2.0.0") && out.contains("2.1.0"),
            "must report current -> latest"
        );
        assert!(
            out.contains("/plugin"),
            "must mention Claude Code's own /plugin activation path"
        );
        assert!(
            out.contains("ccync add https://github.com/example/tool-plugin.git"),
            "must print the take-over-management activation command with the source url"
        );
    }

    /// An embedded (in-repo-subpath) plugin's catalog-resolved source is a
    /// browse path, not a git remote — `cmd_add` would fail at clone. The
    /// view must not hand the user that command.
    #[test]
    fn adopted_changed_without_takeover_support_suppresses_the_add_command() {
        let adopted = vec![AdoptedUpgradeOutcome::Changed {
            name: "tool@market".into(),
            source: "https://github.com/example/market/plugins/tool".into(),
            current_version: Some("2.0.0".into()),
            latest_version: "2.1.0".into(),
            takeover_supported: false,
        }];
        let out = format_adopted_section(&adopted);
        // The explanation names the verb on purpose; what must never appear
        // is a runnable `ccync add <url>` the user could copy and have fail.
        assert!(
            !out.contains("ccync add https://"),
            "a non-clonable source must never be advertised as a runnable add: {out:?}"
        );
        assert!(
            out.contains("/plugin"),
            "Claude's own update path stays available: {out:?}"
        );
        assert!(
            out.contains("cannot fetch it directly"),
            "must say why take-over is unavailable: {out:?}"
        );
        assert!(
            out.contains("2.0.0") && out.contains("2.1.0"),
            "the upgrade itself is still reported: {out:?}"
        );
    }

    #[test]
    fn adopted_changed_with_unknown_current_version_renders_placeholder() {
        let adopted = vec![AdoptedUpgradeOutcome::Changed {
            name: "tool@market".into(),
            source: "https://github.com/example/tool-plugin.git".into(),
            current_version: None,
            latest_version: "3.0.0".into(),
            takeover_supported: true,
        }];
        let out = format_adopted_section(&adopted);
        assert!(
            out.contains("(unknown)"),
            "missing current_version must render a placeholder"
        );
    }

    #[test]
    fn adopted_skipped_prints_reason() {
        let adopted = vec![AdoptedUpgradeOutcome::Skipped {
            name: "memory".into(),
            reason: "adopted MCP server — config entry, not versioned".into(),
        }];
        let out = format_adopted_section(&adopted);
        assert!(out.contains("memory"));
        assert!(out.contains("adopted MCP server — config entry, not versioned"));
    }

    #[test]
    fn adopted_version_unresolved_prints_name_and_source_not_skipped_wording() {
        let adopted = vec![AdoptedUpgradeOutcome::VersionUnresolved {
            name: "tool@market".into(),
            source: "https://github.com/example/tool.git".into(),
        }];
        let out = format_adopted_section(&adopted);
        assert!(out.contains("tool@market"));
        assert!(out.contains("version-unresolved"));
        assert!(out.contains("https://github.com/example/tool.git"));
        assert!(
            !out.contains("skipped"),
            "a source-bearing item must never render as skipped: {out:?}"
        );
    }

    // ── lock-derived repair tip (replaces the removed plugin-only hint) ────────

    #[test]
    fn push_repair_tip_prints_once_when_unresolved_names_present() {
        let mut out = String::new();
        push_repair_tip(&mut out, &["standalone-tool".to_string()]);
        let tip_lines = out.lines().filter(|l| l.contains("ccync repair")).count();
        assert_eq!(
            tip_lines, 1,
            "must print exactly one repair-tip line: {out:?}"
        );
    }

    #[test]
    fn push_repair_tip_absent_when_no_unresolved_names() {
        let mut out = String::new();
        push_repair_tip(&mut out, &[]);
        assert!(
            out.is_empty(),
            "no unresolved rows must print no tip: {out:?}"
        );
    }

    #[test]
    fn push_repair_tip_still_one_line_for_multiple_unresolved_names() {
        // The tip is a single generic line regardless of count — never one
        // line per item, unlike the removed plugin-only hint.
        let mut out = String::new();
        push_repair_tip(
            &mut out,
            &["a".to_string(), "b".to_string(), "c".to_string()],
        );
        assert_eq!(out.lines().count(), 1, "must stay a single line: {out:?}");
    }

    #[test]
    fn format_adopted_section_no_longer_prints_any_repair_hint() {
        // format_adopted_section itself never prints a tip anymore — that is
        // now push_repair_tip's job, driven by lock-derived unresolved
        // names, not per-item Skipped-reason detection.
        let adopted = vec![AdoptedUpgradeOutcome::Skipped {
            name: "standalone-tool".into(),
            reason: "adopted plugin — source-unresolved (no clonable source recorded)".into(),
        }];
        let out = format_adopted_section(&adopted);
        assert!(
            !out.contains("Tip:"),
            "format_adopted_section must not print a tip: {out:?}"
        );
    }

    #[test]
    fn format_upgrade_view_appends_repair_tip_when_unresolved_present() {
        let report = UpgradeReport::default();
        let out = format_upgrade_view(&report, &[], &["standalone-tool".to_string()]);
        assert!(
            out.contains("ccync repair"),
            "must append the repair tip: {out:?}"
        );
    }

    #[test]
    fn format_upgrade_view_omits_repair_tip_when_all_resolved() {
        let report = UpgradeReport::default();
        let out = format_upgrade_view(&report, &[], &[]);
        assert!(
            !out.contains("ccync repair"),
            "must not print a tip when all resolved: {out:?}"
        );
    }

    #[test]
    fn adopted_unchanged_is_not_printed() {
        let adopted = vec![AdoptedUpgradeOutcome::Unchanged {
            name: "tool@market".into(),
        }];
        let out = format_adopted_section(&adopted);
        assert!(
            out.is_empty(),
            "unchanged adopted items are omitted, matching personal-plugin view"
        );
    }

    #[test]
    fn format_upgrade_view_zero_plugins_is_neutral_not_up_to_date() {
        let report = UpgradeReport::default();
        let out = format_upgrade_view(&report, &[], &[]);
        assert!(
            !out.contains("Everything up to date."),
            "zero tracked plugins must not claim everything is up to date"
        );
        assert!(out.contains("No plugins to check for updates."));
    }

    #[test]
    fn format_upgrade_view_pending_includes_apply_hint() {
        let report = UpgradeReport {
            pending: vec![change("a", "oldsha111", "newsha222")],
            ..Default::default()
        };
        let out = format_upgrade_view(&report, &[], &[]);
        assert!(out.contains("run `ccync upgrade` to apply"));
    }

    #[test]
    fn format_upgrade_view_includes_adopted_section() {
        let report = UpgradeReport::default();
        let adopted = vec![AdoptedUpgradeOutcome::Changed {
            name: "tool@market".into(),
            source: "https://github.com/example/tool-plugin.git".into(),
            current_version: Some("1.0.0".into()),
            latest_version: "1.1.0".into(),
            takeover_supported: true,
        }];
        let out = format_upgrade_view(&report, &adopted, &[]);
        assert!(
            out.contains("Everything up to date."),
            "personal section unaffected"
        );
        assert!(out.contains("tool@market"), "adopted section appended");
    }

    /// Combined-rendering case: a real personal `pending` change AND an
    /// adopted `Changed` outcome present simultaneously (the realistic
    /// dry-run scenario when both a git plugin and an adopted plugin have
    /// updates). Both sections must render fully and independently — neither
    /// truncates or clobbers the other's lines.
    #[test]
    fn format_upgrade_view_renders_personal_pending_and_adopted_changed_together() {
        let report = UpgradeReport {
            pending: vec![change("a", "oldsha111", "newsha222")],
            skipped: vec![("b".into(), "non-git source".into())],
            ..Default::default()
        };
        let adopted = vec![
            AdoptedUpgradeOutcome::Changed {
                name: "tool@market".into(),
                source: "https://github.com/example/tool-plugin.git".into(),
                current_version: Some("2.0.0".into()),
                latest_version: "2.1.0".into(),
                takeover_supported: true,
            },
            AdoptedUpgradeOutcome::Skipped {
                name: "memory".into(),
                reason: "adopted MCP server — config entry, not versioned".into(),
            },
        ];
        let out = format_upgrade_view(&report, &adopted, &[]);

        // Personal section: pending change + skipped entry both intact.
        assert!(out.contains("Updates available:"));
        assert!(out.contains('a') && out.contains("oldsha111") && out.contains("newsha222"));
        assert!(out.contains("skipped b (non-git source)"));

        // Adopted section: Changed (with both activation commands) + Skipped
        // both intact, not clobbered by the personal section above it.
        assert!(out.contains(
            "adopted tool@market  2.0.0 -> 2.1.0  https://github.com/example/tool-plugin.git"
        ));
        assert!(out.contains("/plugin"));
        assert!(out.contains("ccync add https://github.com/example/tool-plugin.git"));
        assert!(out
            .contains("adopted memory skipped (adopted MCP server — config entry, not versioned)"));

        // Ordering sanity: personal section precedes adopted section.
        let personal_pos = out.find("Updates available:").unwrap();
        let adopted_pos = out.find("adopted tool@market").unwrap();
        assert!(
            personal_pos < adopted_pos,
            "personal section must precede adopted section"
        );
    }

    /// Adopted `Changed` items are report-only and never affect the exit code.
    /// Zero personal `pending` + a `Changed` adopted item still must exit
    /// `Success` (0), proving the two are decided independently.
    #[test]
    fn adopted_changed_present_does_not_affect_exit_code() {
        let report = UpgradeReport::default(); // no personal pending changes
        let adopted = vec![AdoptedUpgradeOutcome::Changed {
            name: "tool@market".into(),
            source: "https://github.com/example/tool-plugin.git".into(),
            current_version: Some("2.0.0".into()),
            latest_version: "2.1.0".into(),
            takeover_supported: true,
        }];
        // report_to_exit takes only the personal report — adopted has no
        // pathway into it at all.
        assert_eq!(report_to_exit(&report), ExitCode::Success);

        // report_exit (dry-run branch) must also still return Success even
        // though the printed view carries the adopted Changed section.
        assert_eq!(report_exit(&report, &adopted, &[], true), ExitCode::Success);
    }

    /// Exit-code invariant, non-dry-run branch: personal `failed` still drives
    /// `Error` (1) regardless of what adopted outcomes are present.
    #[test]
    fn adopted_state_does_not_affect_non_dry_run_error_exit() {
        let report = UpgradeReport {
            failed: vec![("a".into(), "boom".into())],
            ..Default::default()
        };
        let adopted = vec![AdoptedUpgradeOutcome::Changed {
            name: "tool@market".into(),
            source: "https://github.com/example/tool-plugin.git".into(),
            current_version: Some("2.0.0".into()),
            latest_version: "2.1.0".into(),
            takeover_supported: true,
        }];
        assert_eq!(report_exit(&report, &adopted, &[], false), ExitCode::Error);
    }

    /// Exit-code invariant, non-dry-run success branch: no personal failures
    /// + an adopted Changed item still exits Success (0).
    #[test]
    fn adopted_state_does_not_affect_non_dry_run_success_exit() {
        let report = UpgradeReport {
            upgraded: vec!["a".into()],
            ..Default::default()
        };
        let adopted = vec![AdoptedUpgradeOutcome::Changed {
            name: "tool@market".into(),
            source: "https://github.com/example/tool-plugin.git".into(),
            current_version: Some("2.0.0".into()),
            latest_version: "2.1.0".into(),
            takeover_supported: true,
        }];
        assert_eq!(
            report_exit(&report, &adopted, &[], false),
            ExitCode::Success
        );
    }

    /// exit code 3 (`UpdatesAvailable`) with adopted state present: personal
    /// `pending` still drives the exit code, independent of adopted outcomes.
    #[test]
    fn adopted_state_does_not_affect_updates_available_exit() {
        let report = UpgradeReport {
            pending: vec![change("a", "oldsha", "newsha")],
            ..Default::default()
        };
        let adopted = vec![AdoptedUpgradeOutcome::Skipped {
            name: "memory".into(),
            reason: "adopted MCP server — config entry, not versioned".into(),
        }];
        assert_eq!(
            report_exit(&report, &adopted, &[], true),
            ExitCode::UpdatesAvailable
        );
    }

    // ── pre-init upgrade guard (machine-initialization preflight) ─────────────
    //
    // Mirrors lifecycle.rs's pre-init sync guard tests: a non-dry `upgrade`
    // clones/re-pins git plugins and reprojects, so it must refuse before ANY
    // domain mutation on an uninitialized machine, while `upgrade --dry-run`
    // (the exception, same shape as `sync --dry-run`) stays available
    // pre-init.
    mod pre_init_upgrade_guard {
        use super::*;

        fn isolate_env(tmp: &std::path::Path) -> Vec<(String, Option<String>)> {
            let keys = [
                "USERPROFILE",
                "HOME",
                "APPDATA",
                "LOCALAPPDATA",
                "XDG_CONFIG_HOME",
            ];
            let mut saved = Vec::new();
            for k in keys {
                saved.push((k.to_string(), std::env::var(k).ok()));
                unsafe {
                    std::env::set_var(k, tmp);
                }
            }
            saved
        }

        fn restore_saved_env(saved: &[(String, Option<String>)]) {
            for (k, v) in saved {
                unsafe {
                    match v {
                        Some(val) => std::env::set_var(k, val),
                        None => std::env::remove_var(k),
                    }
                }
            }
        }

        /// Recursive snapshot of every file under `dir`: (relative path, byte
        /// length, modified time). Used to prove a refused pre-init upgrade
        /// leaves the entire home tree byte- and mtime-identical — zero
        /// cache/pin/live-surface writes, i.e. the clone/pin/project seams
        /// were never reached.
        fn snapshot_tree(dir: &std::path::Path) -> Vec<(String, u64, std::time::SystemTime)> {
            fn walk(
                base: &std::path::Path,
                dir: &std::path::Path,
                out: &mut Vec<(String, u64, std::time::SystemTime)>,
            ) {
                let Ok(read) = std::fs::read_dir(dir) else {
                    return;
                };
                for entry in read.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        walk(base, &path, out);
                    } else if let Ok(meta) = path.metadata() {
                        let rel = path
                            .strip_prefix(base)
                            .unwrap_or(&path)
                            .to_string_lossy()
                            .into_owned();
                        out.push((
                            rel,
                            meta.len(),
                            meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                        ));
                    }
                }
            }
            let mut out = Vec::new();
            walk(dir, dir, &mut out);
            out.sort();
            out
        }

        /// Seed a personal catalog with one git plugin entry so, absent the
        /// preflight, `cmd_upgrade` would reach `resolve_adopted_upgrades` /
        /// `run_upgrade` and attempt to clone/pin it. The source URL is
        /// deliberately unreachable — the guard must refuse before any
        /// git subprocess (`ls-remote`/`clone`) is ever spawned.
        fn seed_personal_git_plugin(home: &std::path::Path) {
            let ccync = home.join(".ccync");
            std::fs::create_dir_all(&ccync).unwrap();
            std::fs::write(
                ccync.join("plugins.json"),
                r#"{"_personalPlugins":[{"pluginId":"a","installStrategy":"personal-git-clone","source":"https://example.invalid/a.git","pin":"oldsha"}]}"#,
            )
            .unwrap();
        }

        #[test]
        fn pre_init_upgrade_guard_refuses_non_dry_with_zero_mutation() {
            let _env_guard = crate::test_io_lock();
            let tmp = tempfile::TempDir::new().unwrap();
            let saved = isolate_env(tmp.path());

            seed_personal_git_plugin(tmp.path());
            let before = snapshot_tree(tmp.path());

            let args = vec!["upgrade".to_string()];
            let exit = cmd_upgrade(&args);

            let after = snapshot_tree(tmp.path());
            restore_saved_env(&saved);

            assert_eq!(
                exit,
                ExitCode::Error,
                "a non-dry upgrade on an uninitialized machine must refuse with Error/1"
            );
            assert!(
                !before.is_empty(),
                "sanity: the seeded personal catalog must exist before the refused upgrade"
            );
            assert_eq!(
                before, after,
                "a refused pre-init upgrade must leave every file byte- and mtime-identical — \
                 zero clone/pin/project mutation (the cache root, pin files, and live agent \
                 surfaces are all untouched)"
            );
            assert!(
                !tmp.path().join(".ccync").join("cache").exists(),
                "the clone seam must never run: no cache dir created"
            );
            assert!(
                !tmp.path().join(".ccync").join("build").exists(),
                "the project seam must never run: no build/render output created"
            );
        }

        #[test]
        fn pre_init_upgrade_guard_dry_run_still_succeeds_pre_init() {
            let _env_guard = crate::test_io_lock();
            let tmp = tempfile::TempDir::new().unwrap();
            let saved = isolate_env(tmp.path());

            // Deliberately unseeded — a fresh, uninitialized machine with no
            // personal catalog at all. `--dry-run` must still reach its
            // existing read-only outcome instead of being refused.
            assert!(
                !crate::commands::lifecycle::machine_is_initialized(),
                "sanity: fresh redirected home must be uninitialized"
            );

            let args = vec!["upgrade".to_string(), "--dry-run".to_string()];
            let exit = cmd_upgrade(&args);

            restore_saved_env(&saved);

            assert_eq!(
                exit,
                ExitCode::Success,
                "upgrade --dry-run must remain available pre-init, still reaching its \
                 existing read-only outcome (no plugins tracked ⇒ Success)"
            );
        }
    }
}
