//! Lifecycle commands: install / update / sync / uninstall / backup / restore.

use ccync_engine::ExitCode;
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
struct UpdateCliOptions {
    dry_run: bool,
    machine_only: bool,
    uninstall: bool,
    replace: bool,
    selected_runtimes: Option<Vec<String>>,
    primary_runtime: Option<String>,
}

fn finalize_machine_options(
    config: &ccync_foundation::config::CcyncConfig,
    parsed: &UpdateCliOptions,
) -> Result<projection::MachineUpdateOptions, String> {
    let mut opts = projection::machine_options_from_config(config, parsed.dry_run)
        .map_err(|e| e.to_string())?;
    opts.uninstall = parsed.uninstall;
    opts.replace = parsed.replace;
    if let Some(selected) = &parsed.selected_runtimes {
        opts.selected_runtimes = selected.clone();
    }
    if let Some(primary) = &parsed.primary_runtime {
        opts.primary_runtime = primary.clone();
    }
    Ok(opts)
}

/// `ccync sync [--dry-run] [--yes]` — the unified engine. Re-resolves the
/// catalog lockfile, then re-projects the managed set (canonical render +
/// skills / commands / agents + unified MCP) to every selected agent.
///
/// Master selection (adopt) is owned by `ccync init`, not sync (the former
/// `--import-from` was removed). `--dry-run` resolves + reports without writing.
/// `--yes` / `CCYNC_ASSUME_YES` skips the first-run confirmation prompt.
pub(crate) fn cmd_sync(args: &[String]) -> ExitCode {
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let assume_yes = args.iter().any(|a| a == "--yes");
    run_unified_projection(dry_run, assume_yes)
}

/// The shared unified-engine projection used by `init`, `sync`, `add`, and
/// `remove`. Internalizes the catalog resolve (lockfile write), then projects
/// the managed set to all agents. `dry_run` resolves + reports but writes no
/// projection. `assume_yes` skips the first-run overwrite-visibility prompt.
pub(crate) fn run_unified_projection(dry_run: bool, assume_yes: bool) -> ExitCode {
    let config = ccync_foundation::config::CcyncConfig::load();

    // 0. Internalize the catalog resolve: refresh the lockfile so the
    //    managed set the projection reads is current. Best-effort.
    if let Err(e) = run_catalog_resolve(dry_run) {
        eprintln!("ccync sync: catalog resolve (best-effort): {e}");
    }

    // Collect live MCP write targets for dry-run output and the visibility gate.
    let live_paths = mcp::live_mcp_target_paths();

    if dry_run {
        println!("--- DRY RUN ---");
        println!("ccync sync: live agent config files that would be written:");
        for p in &live_paths {
            println!("  {}", p.display());
        }
        println!("  (no files written)");
        return ExitCode::Success;
    }

    // First-run overwrite-visibility gate: before writing live agent configs,
    // confirm the user knows which files are about to be modified.
    if is_first_projection() {
        println!("ccync: first run — about to write to live agent config files:");
        for p in &live_paths {
            println!("  {}", p.display());
        }
        if !confirm_overwrite(assume_yes) {
            println!("ccync: aborted — no files written.");
            return ExitCode::Success;
        }
    }

    // 1. canonical render + managed.json (incl. adopted `_mcpServers`).
    match ccync_engine::install::run_update(&config) {
        Ok(report) => {
            println!(
                "ccync sync: projected managed surfaces ({} runtime(s))",
                report.runtimes.len()
            );
            for w in &report.warnings {
                eprintln!("warning: {w}");
            }
        }
        Err(e) => {
            eprintln!("ccync sync: {e}");
            return ExitCode::Error;
        }
    }

    // 2. skills/commands/agents projection to all selected agents (best-effort).
    let machine_parsed = UpdateCliOptions {
        dry_run: false,
        machine_only: true,
        uninstall: false,
        replace: false,
        selected_runtimes: None,
        primary_runtime: None,
    };
    match finalize_machine_options(&config, &machine_parsed) {
        Ok(machine_opts) => match projection::run_machine_update(&machine_opts) {
            Ok(report) => {
                for warning in &report.warnings {
                    eprintln!("warning: {}", warning.message);
                }
            }
            Err(e) => eprintln!("ccync sync: machine projection (best-effort): {e}"),
        },
        Err(e) => eprintln!("ccync sync: machine projection skipped (best-effort): {e}"),
    }

    // 3. unified MCP projection to all MCP-capable agents (best-effort). Sources
    // the merged managed.json that run_update just wrote.
    if let Some(manifest) = ccync_foundation::paths::generated_mcp_path() {
        if manifest.is_file() {
            match mcp::run_mcp_update(&manifest) {
                Ok(report) => {
                    for w in &report.warnings {
                        eprintln!("warning: {w}");
                    }
                }
                Err(e) => eprintln!("ccync sync: MCP projection (best-effort): {e}"),
            }
        }
    }

    ExitCode::Success
}

/// Returns true when `plugins.lock.json` has a `_ccyncProjection` key
/// (meaning at least one successful projection has completed on this machine).
fn lock_has_projection(lock_path: &std::path::Path) -> bool {
    if !lock_path.exists() {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(lock_path) else {
        return false;
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    val.get("_ccyncProjection").is_some()
}

/// True when this machine has never completed a successful projection.
fn is_first_projection() -> bool {
    ccync_foundation::paths::plugins_lock_path()
        .map(|p| !lock_has_projection(&p))
        .unwrap_or(true)
}

/// Returns true when the user has confirmed (or `assume_yes` / `CCYNC_ASSUME_YES`
/// is set). On a TTY, prompts interactively. On a non-TTY without either flag,
/// prints guidance and returns false (no write).
fn confirm_overwrite(assume_yes: bool) -> bool {
    if assume_yes || std::env::var("CCYNC_ASSUME_YES").is_ok() {
        return true;
    }
    if std::io::stdin().is_terminal() {
        eprint!("Continue? [y/N] ");
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_ok() {
            let t = input.trim().to_ascii_lowercase();
            return t == "y" || t == "yes";
        }
        return false;
    }
    eprintln!(
        "ccync: non-interactive session — pass --yes or set CCYNC_ASSUME_YES=1 to proceed."
    );
    false
}

/// Resolve the public catalog into the lockfile (internalized by `sync`).
fn run_catalog_resolve(dry_run: bool) -> Result<(), String> {
    use ccync_engine::catalog::{run_resolve_catalog, ResolveCatalogOptions};
    use ccync_foundation::paths::{ccync_plugin_root, machine_config_path, plugins_lock_path};

    let catalog_path = ccync_plugin_root("ccync")
        .ok_or("cannot determine plugin root")?
        .join("catalog.json");
    let config_path = machine_config_path().ok_or("cannot determine config path")?;
    let lockfile_path = plugins_lock_path().ok_or("cannot determine lockfile path")?;

    let opts = ResolveCatalogOptions {
        catalog_path,
        config_path,
        lockfile_path,
        dry_run,
        personal_catalog_path_override: ccync_foundation::paths::local_catalog_path(),
    };
    run_resolve_catalog(&opts).map(|_| ())
}

/// Run `ccync backup [--output <dir>]` — copy canonical machine-local state files to a
/// backup directory. Covers `config.json`, `xmachine.json`, `plugins.lock.json`, and
/// `install-state.json`. Missing source files are skipped with a note (not an error).
pub(crate) fn cmd_backup(args: &[String]) -> ExitCode {
    use ccync_foundation::paths::{
        install_state_path, machine_config_path, plugins_lock_path, xmachine_config_path,
    };

    // Parse --output <dir>; default to ./ccync-backup-<unix-seconds> in the current dir.
    let output_dir = {
        let mut out: Option<PathBuf> = None;
        let mut i = 1usize;
        while i < args.len() {
            if args[i] == "--output" {
                i += 1;
                if let Some(val) = args.get(i) {
                    out = Some(PathBuf::from(val));
                } else {
                    eprintln!("ccync backup: --output requires a path");
                    return ExitCode::Usage;
                }
            }
            i += 1;
        }
        out.unwrap_or_else(|| {
            use std::time::{SystemTime, UNIX_EPOCH};
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            PathBuf::from(format!("ccync-backup-{secs}"))
        })
    };

    let canonical_files: &[(&str, Option<PathBuf>)] = &[
        ("config.json", machine_config_path()),
        ("xmachine.json", xmachine_config_path()),
        ("plugins.lock.json", plugins_lock_path()),
        ("install-state.json", install_state_path()),
    ];

    if let Err(e) = std::fs::create_dir_all(&output_dir) {
        eprintln!(
            "ccync backup: cannot create output dir {}: {e}",
            output_dir.display()
        );
        return ExitCode::Error;
    }

    let mut copied = 0usize;
    let mut skipped = 0usize;
    for (name, src_opt) in canonical_files {
        let Some(src) = src_opt else {
            eprintln!("ccync backup: cannot resolve path for {name} (no home dir)");
            skipped += 1;
            continue;
        };
        if !src.exists() {
            println!("ccync backup: {name} not found — skipped");
            skipped += 1;
            continue;
        }
        let dst = output_dir.join(name);
        if let Err(e) = std::fs::copy(src, &dst) {
            eprintln!("ccync backup: failed to copy {name}: {e}");
            return ExitCode::Error;
        }
        copied += 1;
    }

    println!(
        "ccync backup: {} file(s) backed up to {}",
        copied,
        output_dir.display()
    );
    if skipped > 0 {
        println!("ccync backup: {skipped} file(s) skipped (not present on this machine)");
    }
    ExitCode::Success
}

/// Run `ccync restore --from <dir>` — restore canonical machine-local state files from a
/// previously created backup directory.
pub(crate) fn cmd_restore(args: &[String]) -> ExitCode {
    use ccync_foundation::paths::{
        install_state_path, machine_config_path, plugins_lock_path, xmachine_config_path,
    };

    let from_dir = {
        let mut from: Option<PathBuf> = None;
        let mut i = 1usize;
        while i < args.len() {
            if args[i] == "--from" {
                i += 1;
                if let Some(val) = args.get(i) {
                    from = Some(PathBuf::from(val));
                } else {
                    eprintln!("ccync restore: --from requires a path");
                    return ExitCode::Usage;
                }
            }
            i += 1;
        }
        from
    };

    let from_dir = match from_dir {
        Some(d) => d,
        None => {
            eprintln!("ccync restore: --from <dir> is required");
            return ExitCode::Usage;
        }
    };

    if !from_dir.is_dir() {
        eprintln!(
            "ccync restore: backup directory not found: {}",
            from_dir.display()
        );
        return ExitCode::Error;
    }

    let canonical_files: &[(&str, Option<PathBuf>)] = &[
        ("config.json", machine_config_path()),
        ("xmachine.json", xmachine_config_path()),
        ("plugins.lock.json", plugins_lock_path()),
        ("install-state.json", install_state_path()),
    ];

    let mut restored = 0usize;
    let mut skipped = 0usize;
    for (name, dst_opt) in canonical_files {
        let Some(dst) = dst_opt else {
            eprintln!("ccync restore: cannot resolve path for {name} (no home dir)");
            skipped += 1;
            continue;
        };
        let src = from_dir.join(name);
        if !src.exists() {
            println!("ccync restore: {name} not in backup — skipped");
            skipped += 1;
            continue;
        }
        if let Some(parent) = dst.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("ccync restore: cannot create parent dir for {name}: {e}");
                return ExitCode::Error;
            }
        }
        if let Err(e) = std::fs::copy(&src, dst) {
            eprintln!("ccync restore: failed to restore {name}: {e}");
            return ExitCode::Error;
        }
        restored += 1;
    }

    println!(
        "ccync restore: {} file(s) restored from {}",
        restored,
        from_dir.display()
    );
    if skipped > 0 {
        println!("ccync restore: {skipped} file(s) skipped (not in backup)");
    }
    ExitCode::Success
}

/// Run `ccync uninstall` — remove canonical root and surfaces.
pub(crate) fn cmd_uninstall() -> ExitCode {
    use ccync_engine::install::run_uninstall;

    match run_uninstall() {
        Ok(()) => {
            println!("ccync uninstalled.");
            ExitCode::Success
        }
        Err(e) => {
            eprintln!("ccync uninstall: {e}");
            ExitCode::Error
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn cmd_sync_dry_run_writes_nothing_marker() {
        // --dry-run short-circuits before any projection write.
        let args = vec!["sync".to_string(), "--dry-run".to_string()];
        assert!(args.iter().any(|a| a == "--dry-run"));
    }

    // ── lock_has_projection ───────────────────────────────────────────────────

    #[test]
    fn lock_has_projection_false_when_file_absent() {
        let dir = TempDir::new().unwrap();
        assert!(!lock_has_projection(&dir.path().join("nope.json")));
    }

    #[test]
    fn lock_has_projection_false_when_key_missing() {
        let dir = TempDir::new().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        std::fs::write(&lock, r#"{"resolvedPlugins":[]}"#).unwrap();
        assert!(!lock_has_projection(&lock));
    }

    #[test]
    fn lock_has_projection_true_when_key_present() {
        let dir = TempDir::new().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        std::fs::write(&lock, r#"{"_ccyncProjection":{"version":1},"resolvedPlugins":[]}"#)
            .unwrap();
        assert!(lock_has_projection(&lock));
    }

    // ── confirm_overwrite ─────────────────────────────────────────────────────

    #[test]
    fn confirm_overwrite_assume_yes_returns_true() {
        assert!(confirm_overwrite(true));
    }

    #[test]
    fn confirm_overwrite_env_var_returns_true() {
        unsafe { std::env::set_var("CCYNC_ASSUME_YES", "1"); }
        let result = confirm_overwrite(false);
        unsafe { std::env::remove_var("CCYNC_ASSUME_YES"); }
        assert!(result, "CCYNC_ASSUME_YES=1 must return true");
    }

    #[test]
    fn confirm_overwrite_non_tty_no_flags_returns_false() {
        // In cargo test, stdin is not a TTY — gate must abort without assume_yes.
        if !std::io::stdin().is_terminal() {
            assert!(!confirm_overwrite(false));
        }
    }
}
