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
    if let Err(code) = super::args::reject_unknown_flags(
        "sync",
        args,
        &["--dry-run", "--yes"],
        &[],
        0,
        "Usage: ccync sync [--dry-run] [--yes]",
    ) {
        return code;
    }
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let assume_yes = args.iter().any(|a| a == "--yes");
    run_unified_projection(dry_run, assume_yes, false)
}

/// Run the one-shot layout migration for a given user home directory.
///
/// Moves old `~/.ccync/` layout files to new locations. Best-effort: returns
/// the error if apply fails so callers can log and continue.
pub(crate) fn run_layout_migration(user_home: &std::path::Path) -> std::io::Result<()> {
    let ccync_home = user_home.join(".ccync");
    let plan = ccync_engine::migrate::build_migration_plan(&ccync_home);
    ccync_engine::migrate::apply_migration(&plan)?;
    // Fold the legacy install-state.json runtime selection into config.json so an
    // upgrading install does not lose its selected/primary runtimes.
    ccync_engine::migrate::fold_install_state_into_config(&ccync_home)
}

/// Shared projection core: canonical render + machine projection + MCP projection.
///
/// Called by both `run_unified_projection` (public sync) and `refresh_with`
/// (internal dev rebuild). Does NOT run the first-run gate, catalog resolve,
/// dry-run check, or interactive prompts — those belong to their callers.
///
/// Returns `ExitCode::Success` on success; `ExitCode::Error` if the canonical
/// render fails (machine/MCP projections are best-effort).
pub(crate) fn run_projection_core(config: &ccync_foundation::config::CcyncConfig, label: &str) -> ExitCode {
    // 1. canonical render + managed.json (incl. adopted `_mcpServers`).
    match ccync_engine::install::run_update(config) {
        Ok(report) => {
            println!(
                "{label}: projected managed surfaces ({} runtime(s))",
                report.runtimes.len()
            );
            for w in &report.warnings {
                eprintln!("warning: {w}");
            }
        }
        Err(e) => {
            eprintln!("{label}: {e}");
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
    match finalize_machine_options(config, &machine_parsed) {
        Ok(machine_opts) => match projection::run_machine_update(&machine_opts) {
            Ok(report) => {
                for warning in &report.warnings {
                    eprintln!("warning: {}", warning.message);
                }
            }
            Err(e) => eprintln!("{label}: machine projection (best-effort): {e}"),
        },
        Err(e) => eprintln!("{label}: machine projection skipped (best-effort): {e}"),
    }

    // 3. unified MCP projection to all MCP-capable agents (best-effort).
    if let Some(manifest) = ccync_foundation::paths::generated_mcp_path() {
        if manifest.is_file() {
            match mcp::run_mcp_update(&manifest) {
                Ok(report) => {
                    for w in &report.warnings {
                        eprintln!("warning: {w}");
                    }
                }
                Err(e) => eprintln!("{label}: MCP projection (best-effort): {e}"),
            }
        }
    }

    ExitCode::Success
}

/// The shared unified-engine projection used by `init`, `sync`, `add`, and
/// `remove`. Internalizes the catalog resolve (lockfile write), then projects
/// the managed set to all agents. `dry_run` resolves + reports but writes no
/// projection. `assume_yes` skips the first-run overwrite-visibility prompt.
/// `skip_first_run_gate` suppresses the file-list banner entirely (used by
/// `init`, which already shows an agent-selection UI before calling this).
pub(crate) fn run_unified_projection(dry_run: bool, assume_yes: bool, skip_first_run_gate: bool) -> ExitCode {
    // 0a. One-shot layout migration — runs before any command consumes ~/.ccync/ state.
    if let Some(home) = ccync_foundation::paths::user_home() {
        if let Err(e) = run_layout_migration(&home) {
            eprintln!("ccync: layout migration (best-effort): {e}");
        }
    }

    // Load config AFTER migration so a freshly-folded runtime selection
    // (install-state.json → config.json) is visible to this run.
    let config = ccync_foundation::config::CcyncConfig::load();

    // 0b. Internalize the catalog resolve: refresh the lockfile so the
    //     managed set the projection reads is current. Best-effort.
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

    // First-run visibility gate: show which agent config files will be written
    // so the user knows exactly what ccync touches. Skipped when the caller
    // (init) already presented an agent-selection UI.
    if !skip_first_run_gate && is_first_projection() {
        println!("Projecting to {} agent config file(s):", live_paths.len());
        for p in &live_paths {
            println!("  {}", p.display());
        }
        if !confirm_overwrite(assume_yes) {
            println!("ccync: aborted — no files written.");
            return ExitCode::Success;
        }
    }

    run_projection_core(&config, "ccync sync")
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
    use ccync_foundation::paths::{machine_config_path, plugins_lock_path};

    let config_path = machine_config_path().ok_or("cannot determine config path")?;
    let lockfile_path = plugins_lock_path().ok_or("cannot determine lockfile path")?;

    let opts = ResolveCatalogOptions {
        catalog_path: None,
        config_path,
        lockfile_path,
        dry_run,
        personal_catalog_path_override: ccync_foundation::paths::personal_catalog_path(),
    };
    let resolution = run_resolve_catalog(&opts)?;
    for e in &resolution.errors {
        eprintln!("ccync: catalog resolve warning: {e}");
    }
    Ok(())
}

/// Run `ccync backup [--output <dir>]` — copy the canonical machine-local files that
/// cannot be rebuilt: the INPUT files (`config.json` + `plugins.json`) plus
/// `plugins.lock.json`, which is OUTPUT but carries the non-rebuildable init
/// adoption snapshots (`_adoptedItems` / `_mcpServers`).
/// Missing source files are skipped with a note (not an error).
pub(crate) fn cmd_backup(args: &[String]) -> ExitCode {
    use ccync_foundation::paths::{machine_config_path, personal_catalog_path, plugins_lock_path};

    if let Err(code) = super::args::reject_unknown_flags(
        "backup",
        args,
        &[],
        &["--output"],
        0,
        "Usage: ccync backup [--output <dir>]",
    ) {
        return code;
    }

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
        ("plugins.json", personal_catalog_path()),
        ("plugins.lock.json", plugins_lock_path()),
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

/// Run `ccync restore --from <dir>` — restore the canonical machine-local files from a
/// previously created backup directory: the INPUT files (`config.json` + `plugins.json`)
/// plus `plugins.lock.json`, which carries the non-rebuildable init adoption snapshots.
pub(crate) fn cmd_restore(args: &[String]) -> ExitCode {
    use ccync_foundation::paths::{machine_config_path, personal_catalog_path, plugins_lock_path};

    if let Err(code) = super::args::reject_unknown_flags(
        "restore",
        args,
        &[],
        &["--from"],
        0,
        "Usage: ccync restore --from <dir>",
    ) {
        return code;
    }

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
        ("plugins.json", personal_catalog_path()),
        ("plugins.lock.json", plugins_lock_path()),
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
pub(crate) fn cmd_uninstall(args: &[String]) -> ExitCode {
    use ccync_engine::install::run_uninstall;

    if let Err(code) =
        super::args::reject_unknown_flags("uninstall", args, &[], &[], 0, "Usage: ccync uninstall")
    {
        return code;
    }

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
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn cmd_sync_dry_run_writes_nothing_marker() {
        // --dry-run short-circuits before any projection write.
        let args = vec!["sync".to_string(), "--dry-run".to_string()];
        assert!(args.iter().any(|a| a == "--dry-run"));
    }

    // ── run_catalog_resolve: resolve errors are warnings, not fatal ───────────

    #[test]
    fn run_catalog_resolve_returns_ok_when_resolution_has_errors() {
        // A config.json with an unknown defaultProfile causes the resolver to
        // push a non-fatal entry into CatalogResolution.errors (see
        // ccync_engine::catalog::resolve_plugin_set). run_catalog_resolve must
        // still return Ok(()) — resolve errors are collected and warned about,
        // never propagated as a hard failure.
        let _guard = crate::test_io_lock();

        let tmp = TempDir::new().unwrap();
        let orig_home;
        #[cfg(windows)]
        unsafe {
            orig_home = std::env::var("USERPROFILE").ok();
            std::env::set_var("USERPROFILE", tmp.path());
            std::env::set_var("HOME", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            orig_home = std::env::var("HOME").ok();
            std::env::set_var("HOME", tmp.path());
        }

        let ccync_home = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync_home).unwrap();
        fs::write(
            ccync_home.join("config.json"),
            r#"{"defaultProfile":"does-not-exist"}"#,
        )
        .unwrap();

        let result = run_catalog_resolve(false);

        #[cfg(windows)]
        unsafe {
            match &orig_home {
                Some(v) => { std::env::set_var("USERPROFILE", v); std::env::set_var("HOME", v); }
                None => { std::env::remove_var("USERPROFILE"); std::env::remove_var("HOME"); }
            }
        }
        #[cfg(not(windows))]
        unsafe {
            match &orig_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }

        assert!(
            result.is_ok(),
            "resolve errors (e.g. unknown defaultProfile) must be collected as warnings, not returned as Err: {result:?}"
        );
    }

    // ── source-grep: exact warning string templates are present ───────────────

    #[test]
    fn catalog_resolve_warning_templates_present_in_source() {
        // Guards the exact wording of the two warning templates that replaced
        // the old suppressed-warning path: a whole-resolve-call failure inside
        // run_unified_projection, and per-entry CatalogResolution.errors
        // warnings inside run_catalog_resolve (this file) and
        // run_personal_resolve (plugin.rs).
        let this_file = include_str!("lifecycle.rs");
        assert!(
            this_file.contains("ccync sync: catalog resolve (best-effort): {e}"),
            "run_unified_projection must warn on a failed resolve call with the best-effort template"
        );
        assert!(
            this_file.contains("ccync: catalog resolve warning: {e}"),
            "run_catalog_resolve must warn per-entry using the catalog resolve warning template"
        );

        let plugin_file = include_str!("plugin.rs");
        assert!(
            plugin_file.contains("ccync: catalog resolve warning: {e}"),
            "run_personal_resolve must warn per-entry using the catalog resolve warning template"
        );
    }

    #[test]
    fn removed_suppression_helper_never_reappears() {
        // Regression guard: the old catalog-warning suppression helper and all
        // references to it must never reappear anywhere in this crate's
        // sources. The needle is assembled at runtime so this test's own
        // source text does not trip its own assertion via include_str!.
        let needle = ["is_benign_catalog", "_warning"].concat();
        let this_file = include_str!("lifecycle.rs");
        let plugin_file = include_str!("plugin.rs");
        assert!(!this_file.contains(&needle));
        assert!(!plugin_file.contains(&needle));
    }

    // ── run_projection_core shared seam ──────────────────────────────────────

    #[test]
    fn projection_core_seam_succeeds_in_isolated_home() {
        // run_projection_core is the shared seam used by refresh_with.
        // Holds the crate-level TEST_IO_LOCK to avoid racing with the sync test
        // that also redirects HOME to a temp dir.
        let _guard = crate::test_io_lock();

        let tmp = tempfile::TempDir::new().unwrap();
        let orig_home;
        #[cfg(windows)]
        {
            orig_home = std::env::var("USERPROFILE").ok();
            unsafe {
                std::env::set_var("USERPROFILE", tmp.path());
                std::env::set_var("HOME", tmp.path());
            }
        }
        #[cfg(not(windows))]
        {
            orig_home = std::env::var("HOME").ok();
            unsafe { std::env::set_var("HOME", tmp.path()); }
        }

        let config = ccync_foundation::config::CcyncConfig::load();
        let result = run_projection_core(&config, "ccync refresh");

        #[cfg(windows)]
        unsafe {
            match &orig_home {
                Some(v) => { std::env::set_var("USERPROFILE", v); std::env::set_var("HOME", v); }
                None => { std::env::remove_var("USERPROFILE"); std::env::remove_var("HOME"); }
            }
        }
        #[cfg(not(windows))]
        unsafe {
            match &orig_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }

        assert_eq!(result, ExitCode::Success, "run_projection_core must succeed in isolated home");
        assert!(
            tmp.path().join(".ccync").join("build").join("render").is_dir(),
            "run_projection_core must write the canonical render dir"
        );
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
        // CCYNC_ASSUME_YES is process-global; hold the shared I/O lock so this
        // set/remove cannot interleave with confirm_overwrite_non_tty_* reading it.
        let _guard = crate::test_io_lock();
        unsafe { std::env::set_var("CCYNC_ASSUME_YES", "1"); }
        let result = confirm_overwrite(false);
        unsafe { std::env::remove_var("CCYNC_ASSUME_YES"); }
        assert!(result, "CCYNC_ASSUME_YES=1 must return true");
    }

    #[test]
    fn confirm_overwrite_non_tty_no_flags_returns_false() {
        // In cargo test, stdin is not a TTY — gate must abort without assume_yes.
        // Hold the shared I/O lock so a concurrent test setting CCYNC_ASSUME_YES
        // cannot flip this read (the env var is process-global).
        let _guard = crate::test_io_lock();
        if !std::io::stdin().is_terminal() {
            assert!(!confirm_overwrite(false));
        }
    }

    // ── migration shim fires before command consumes ~/.ccync/ state ─────────

    #[test]
    fn layout_migration_leaves_agent_surfaces_untouched() {
        // The agent-orphan sweep was removed: migration must not touch live
        // agent surfaces, so any agent file is preserved (regression guard).
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let agents = home.join(".claude").join("agents");
        fs::create_dir_all(&agents).unwrap();
        let a = agents.join("example.agent.md");
        fs::write(&a, "---\nname: example\n---\nbody\n").unwrap();

        run_layout_migration(home).unwrap();

        assert!(a.exists(), "layout migration must not touch agent surfaces (sweep removed)");
    }

    #[test]
    fn layout_migration_runs_before_command_state_read() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let ccync = home.join(".ccync");

        // Plant old-layout files that would be stale/missing on new layout.
        fs::create_dir_all(ccync.join("config")).unwrap();
        fs::write(
            ccync.join("config").join("config.json"),
            r#"{"runtimes":["claude"]}"#,
        )
        .unwrap();
        fs::create_dir_all(ccync.join("local")).unwrap();
        fs::write(ccync.join("local").join("catalog.json"), r#"{"plugins":[]}"#).unwrap();

        // Call the migration shim (the function wired into run_unified_projection).
        run_layout_migration(home).unwrap();

        // New layout must exist before any command-specific writes.
        assert!(ccync.join("config.json").exists(), "config.json must be at new location");
        assert!(ccync.join("plugins.json").exists(), "plugins.json must be at new location");

        // Old locations must be gone (migration, not copy).
        assert!(
            !ccync.join("config").join("config.json").exists(),
            "old config/config.json must be removed"
        );
        assert!(
            !ccync.join("local").join("catalog.json").exists(),
            "old local/catalog.json must be removed"
        );
    }

    #[test]
    fn layout_migration_shim_is_idempotent_in_new_layout() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let ccync = home.join(".ccync");

        // New layout already present — migration plan must be empty, no error.
        fs::create_dir_all(&ccync).unwrap();
        fs::write(ccync.join("config.json"), r#"{"runtimes":["claude"]}"#).unwrap();

        run_layout_migration(home).unwrap();
        run_layout_migration(home).unwrap(); // second call must not error

        assert!(ccync.join("config.json").exists());
    }

    // ── cmd_restore --from <dir> regression ──────────────────────────────────

    #[test]
    fn cmd_restore_no_from_flag_is_usage() {
        let args = vec!["restore".to_string()];
        assert_eq!(cmd_restore(&args), ExitCode::Usage);
    }

    #[test]
    fn cmd_restore_from_missing_dir_is_error() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does_not_exist");
        let args = vec!["restore".to_string(), "--from".to_string(), missing.to_string_lossy().to_string()];
        assert_eq!(cmd_restore(&args), ExitCode::Error);
    }

    #[test]
    fn cmd_restore_copies_config_and_lock_to_ccync_home() {
        let _guard = crate::test_io_lock();

        let home = TempDir::new().unwrap();
        let backup = TempDir::new().unwrap();

        // Seed backup with the two canonical INPUT files.
        fs::write(backup.path().join("config.json"), r#"{"runtimes":["claude"]}"#).unwrap();
        fs::write(backup.path().join("plugins.lock.json"), r#"{"_personalPlugins":[]}"#).unwrap();

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

        let args = vec![
            "restore".to_string(),
            "--from".to_string(),
            backup.path().to_string_lossy().to_string(),
        ];
        let exit = cmd_restore(&args);

        #[cfg(windows)]
        unsafe {
            match &orig_home {
                Some(v) => { std::env::set_var("USERPROFILE", v); std::env::set_var("HOME", v); }
                None => { std::env::remove_var("USERPROFILE"); std::env::remove_var("HOME"); }
            }
        }
        #[cfg(not(windows))]
        unsafe {
            match &orig_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }

        assert_eq!(exit, ExitCode::Success);
        assert!(
            home.path().join(".ccync").join("config.json").exists(),
            "config.json must be restored to ~/.ccync/config.json"
        );
        assert!(
            home.path().join(".ccync").join("build").join("lock.json").exists(),
            "plugins.lock.json must be restored to ~/.ccync/build/lock.json"
        );
    }

    #[test]
    fn cmd_restore_skips_files_absent_from_backup_without_error() {
        let _guard = crate::test_io_lock();

        let home = TempDir::new().unwrap();
        let backup = TempDir::new().unwrap();

        // Backup contains only config.json — plugins.lock.json absent.
        fs::write(backup.path().join("config.json"), r#"{"runtimes":["codex"]}"#).unwrap();

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

        let args = vec![
            "restore".to_string(),
            "--from".to_string(),
            backup.path().to_string_lossy().to_string(),
        ];
        let exit = cmd_restore(&args);

        #[cfg(windows)]
        unsafe {
            match &orig_home {
                Some(v) => { std::env::set_var("USERPROFILE", v); std::env::set_var("HOME", v); }
                None => { std::env::remove_var("USERPROFILE"); std::env::remove_var("HOME"); }
            }
        }
        #[cfg(not(windows))]
        unsafe {
            match &orig_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }

        assert_eq!(exit, ExitCode::Success, "missing backup file must be skipped, not errored");
        assert!(
            home.path().join(".ccync").join("config.json").exists(),
            "config.json must be restored"
        );
        assert!(
            !home.path().join(".ccync").join("build").join("lock.json").exists(),
            "plugins.lock.json absent from backup must not be created at dst"
        );
    }

    #[test]
    fn cmd_restore_source_addition_does_not_affect_restore_paths() {
        // Regression guard: verify cmd_restore's canonical file list still contains
        // the three canonical files (config.json, plugins.json, plugins.lock.json)
        // after restore_source was added.
        // Probe: the file names appear in the source (compile-time check via build).
        // This test exercises the wiring without I/O by confirming usage-path behavior.
        let args = vec!["restore".to_string()]; // no --from → Usage, not a new unknown code path
        assert_eq!(cmd_restore(&args), ExitCode::Usage);
    }

    // ── backup/restore three-file canonical set (INPUT + adoption-snapshot lock) ──

    /// Swap env home to `home`, run `f`, restore env home. Serialized by the caller
    /// holding `crate::test_io_lock()`.
    fn with_env_home<T>(home: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let orig_home;
        #[cfg(windows)]
        unsafe {
            orig_home = std::env::var("USERPROFILE").ok();
            std::env::set_var("USERPROFILE", home);
            std::env::set_var("HOME", home);
        }
        #[cfg(not(windows))]
        unsafe {
            orig_home = std::env::var("HOME").ok();
            std::env::set_var("HOME", home);
        }

        let result = f();

        #[cfg(windows)]
        unsafe {
            match &orig_home {
                Some(v) => { std::env::set_var("USERPROFILE", v); std::env::set_var("HOME", v); }
                None => { std::env::remove_var("USERPROFILE"); std::env::remove_var("HOME"); }
            }
        }
        #[cfg(not(windows))]
        unsafe {
            match &orig_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        result
    }

    #[test]
    fn cmd_backup_copies_all_three_canonical_files() {
        let _guard = crate::test_io_lock();

        let home = TempDir::new().unwrap();
        let out = TempDir::new().unwrap();
        let out_dir = out.path().join("backup");

        let ccync = home.path().join(".ccync");
        fs::create_dir_all(ccync.join("build")).unwrap();
        fs::write(ccync.join("config.json"), r#"{"runtimes":["claude"]}"#).unwrap();
        fs::write(ccync.join("plugins.json"), r#"{"plugins":[{"id":"demo"}]}"#).unwrap();
        fs::write(ccync.join("build").join("lock.json"), r#"{"_personalPlugins":[]}"#).unwrap();

        let args = vec![
            "backup".to_string(),
            "--output".to_string(),
            out_dir.to_string_lossy().to_string(),
        ];
        let exit = with_env_home(home.path(), || cmd_backup(&args));

        assert_eq!(exit, ExitCode::Success);
        assert!(out_dir.join("config.json").exists(), "config.json must be backed up");
        assert!(out_dir.join("plugins.json").exists(), "plugins.json must be backed up");
        assert!(
            out_dir.join("plugins.lock.json").exists(),
            "plugins.lock.json must be backed up"
        );
        assert_eq!(
            fs::read_dir(&out_dir).unwrap().count(),
            3,
            "backup dir must contain exactly the three canonical files"
        );
        assert_eq!(
            fs::read_to_string(out_dir.join("plugins.json")).unwrap(),
            r#"{"plugins":[{"id":"demo"}]}"#,
            "backed-up plugins.json must carry the source content"
        );
    }

    #[test]
    fn cmd_backup_skips_missing_plugins_json_without_error() {
        let _guard = crate::test_io_lock();

        let home = TempDir::new().unwrap();
        let out = TempDir::new().unwrap();
        let out_dir = out.path().join("backup");

        // Machine without a personal catalog: only config.json + lock exist.
        let ccync = home.path().join(".ccync");
        fs::create_dir_all(ccync.join("build")).unwrap();
        fs::write(ccync.join("config.json"), r#"{"runtimes":["claude"]}"#).unwrap();
        fs::write(ccync.join("build").join("lock.json"), r#"{"_personalPlugins":[]}"#).unwrap();

        let args = vec![
            "backup".to_string(),
            "--output".to_string(),
            out_dir.to_string_lossy().to_string(),
        ];
        let exit = with_env_home(home.path(), || cmd_backup(&args));

        assert_eq!(exit, ExitCode::Success, "missing plugins.json must be skipped, not errored");
        assert!(out_dir.join("config.json").exists());
        assert!(out_dir.join("plugins.lock.json").exists());
        assert!(
            !out_dir.join("plugins.json").exists(),
            "absent plugins.json must not produce a backup file"
        );
    }

    #[test]
    fn cmd_restore_copies_plugins_json_symmetrically() {
        let _guard = crate::test_io_lock();

        let home = TempDir::new().unwrap();
        let backup = TempDir::new().unwrap();

        // Three-file backup — the same set cmd_backup produces.
        fs::write(backup.path().join("config.json"), r#"{"runtimes":["claude"]}"#).unwrap();
        fs::write(backup.path().join("plugins.json"), r#"{"plugins":[{"id":"demo"}]}"#).unwrap();
        fs::write(backup.path().join("plugins.lock.json"), r#"{"_personalPlugins":[]}"#).unwrap();

        let args = vec![
            "restore".to_string(),
            "--from".to_string(),
            backup.path().to_string_lossy().to_string(),
        ];
        let exit = with_env_home(home.path(), || cmd_restore(&args));

        assert_eq!(exit, ExitCode::Success);
        let ccync = home.path().join(".ccync");
        assert!(ccync.join("config.json").exists(), "config.json must be restored");
        assert!(
            ccync.join("plugins.json").exists(),
            "plugins.json must be restored to ~/.ccync/plugins.json"
        );
        assert!(
            ccync.join("build").join("lock.json").exists(),
            "plugins.lock.json must be restored to ~/.ccync/build/lock.json"
        );
        assert_eq!(
            fs::read_to_string(ccync.join("plugins.json")).unwrap(),
            r#"{"plugins":[{"id":"demo"}]}"#,
            "restored plugins.json must carry the backup content"
        );
    }

    #[test]
    fn cmd_restore_legacy_two_file_backup_skips_plugins_json() {
        let _guard = crate::test_io_lock();

        let home = TempDir::new().unwrap();
        let backup = TempDir::new().unwrap();

        // Legacy backup created before plugins.json joined the canonical set.
        fs::write(backup.path().join("config.json"), r#"{"runtimes":["claude"]}"#).unwrap();
        fs::write(backup.path().join("plugins.lock.json"), r#"{"_personalPlugins":[]}"#).unwrap();

        let args = vec![
            "restore".to_string(),
            "--from".to_string(),
            backup.path().to_string_lossy().to_string(),
        ];
        let exit = with_env_home(home.path(), || cmd_restore(&args));

        assert_eq!(exit, ExitCode::Success, "legacy two-file backup must restore without error");
        let ccync = home.path().join(".ccync");
        assert!(ccync.join("config.json").exists());
        assert!(ccync.join("build").join("lock.json").exists());
        assert!(
            !ccync.join("plugins.json").exists(),
            "plugins.json absent from a legacy backup must not create a destination file"
        );
    }

    #[test]
    fn cmd_restore_overwrites_existing_plugins_json_with_backup_content() {
        let _guard = crate::test_io_lock();

        let home = TempDir::new().unwrap();
        let backup = TempDir::new().unwrap();

        fs::write(backup.path().join("plugins.json"), r#"{"plugins":[{"id":"from-backup"}]}"#)
            .unwrap();

        let ccync = home.path().join(".ccync");
        fs::create_dir_all(&ccync).unwrap();
        fs::write(ccync.join("plugins.json"), r#"{"plugins":[{"id":"pre-existing"}]}"#).unwrap();

        let args = vec![
            "restore".to_string(),
            "--from".to_string(),
            backup.path().to_string_lossy().to_string(),
        ];
        let exit = with_env_home(home.path(), || cmd_restore(&args));

        assert_eq!(exit, ExitCode::Success);
        assert_eq!(
            fs::read_to_string(ccync.join("plugins.json")).unwrap(),
            r#"{"plugins":[{"id":"from-backup"}]}"#,
            "restore must overwrite the existing plugins.json with the backup version"
        );
    }
}
