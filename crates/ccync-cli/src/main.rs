//! `ccync` CLI entry point — cross-agent plugin / MCP / skills manager.
//!
//! Single binary entry for the 16-command public surface: init, sync, add,
//! remove, list, doctor, backup, restore, uninstall, update, upgrade, show,
//! cleanup, pin, search, repair.

mod commands;

use ccync_engine::{classify_args, Action, CommandKind, ExitCode};
use commands::cleanup::cmd_cleanup;
use commands::doctor::cmd_doctor;
use commands::init::cmd_init;
use commands::lifecycle::{cmd_backup, cmd_restore, cmd_sync, cmd_uninstall};
use commands::pin::cmd_pin;
use commands::plugin::{cmd_add, cmd_list, cmd_remove};
use commands::refresh::cmd_refresh;
use commands::repair::cmd_repair;
use commands::rollback::cmd_rollback;
use commands::search::cmd_search;
use commands::show::cmd_show;
use commands::update::cmd_update;
use commands::upgrade::cmd_upgrade;
use std::process::ExitCode as ProcessExitCode;

fn print_help() {
    println!("ccync — cross-agent plugin / MCP / skills manager");
    println!();
    println!("Usage: ccync <command> [options]");
    println!("       ccync --version");
    println!("       ccync --help");
    println!();
    println!("Commands:");
    // Explicitly list the public commands only — internal commands (refresh,
    // rollback) are dispatchable but not advertised here.
    for cmd in CommandKind::PUBLIC {
        println!("  {}", cmd.as_str());
    }
    println!();
    println!("Run `ccync <command> --help` for command-specific usage.");
    println!();
    println!("Options:");
    println!("  --yes                     Skip the first-run confirmation prompt (sync, add, remove, rollback)");
    println!("  --dry-run                 Preview without making changes (sync, doctor, upgrade)");
    println!("  --help, -h                Show usage for a command");
    println!("  --version                 Print the ccync version");
    println!();
    println!("Environment:");
    println!("  CCYNC_ASSUME_YES=1        Equivalent to passing --yes to every command");
}

fn run(args: &[String]) -> ExitCode {
    match classify_args(args) {
        Action::Version => {
            println!("ccync {}", env!("CARGO_PKG_VERSION"));
            ExitCode::Success
        }
        Action::Help => {
            print_help();
            ExitCode::Success
        }
        Action::MissingCommand => {
            // Bare `ccync` (no args) prints help to stdout and exits 0. This is
            // the standard "no subcommand → show help" CLI behavior, and it keeps
            // the winget executable smoke-test (which runs the exe with no args)
            // from seeing a non-zero exit. Unknown flags/commands still return
            // Usage (64) via the UnknownCommand / per-verb guard paths.
            print_help();
            ExitCode::Success
        }
        Action::NotWired(CommandKind::Init) => cmd_init(args),
        Action::NotWired(CommandKind::Sync) => cmd_sync(args),
        Action::NotWired(CommandKind::Add) => cmd_add(args),
        Action::NotWired(CommandKind::Remove) => cmd_remove(args),
        Action::NotWired(CommandKind::List) => cmd_list(args),
        Action::NotWired(CommandKind::Doctor) => cmd_doctor(args),
        // backup/restore: machine-local canonical state file export/import.
        Action::NotWired(CommandKind::Backup) => cmd_backup(args),
        Action::NotWired(CommandKind::Restore) => cmd_restore(args),
        Action::NotWired(CommandKind::Uninstall) => cmd_uninstall(args),
        Action::NotWired(CommandKind::Update) => cmd_update(args),
        Action::NotWired(CommandKind::Upgrade) => cmd_upgrade(args),
        Action::NotWired(CommandKind::Show) => cmd_show(args),
        Action::NotWired(CommandKind::Cleanup) => cmd_cleanup(args),
        Action::NotWired(CommandKind::Pin) => cmd_pin(args),
        Action::NotWired(CommandKind::Search) => cmd_search(args),
        Action::NotWired(CommandKind::Repair) => cmd_repair(args),
        Action::NotWired(CommandKind::Refresh) => cmd_refresh(args),
        Action::NotWired(CommandKind::Rollback) => cmd_rollback(args),
        Action::UnknownCommand(cmd) => {
            eprintln!("ccync: unknown command '{cmd}'. Run `ccync --help`.");
            ExitCode::Usage
        }
    }
}

fn main() -> ProcessExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    run(&args).into()
}

/// Serializes tests that redirect HOME to a temp dir.
/// Shared across all test modules in this crate to prevent env-var races.
#[cfg(test)]
pub(crate) static TEST_IO_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
    std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) fn test_io_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_IO_LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    // Legacy alias — delegates to the crate-level shared lock so all I/O-heavy
    // tests across modules coordinate on the same mutex.
    static IO_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn io_lock() -> std::sync::MutexGuard<'static, ()> {
        let _ = IO_LOCK.get_or_init(|| Mutex::new(())); // keep static alive
        crate::test_io_lock()
    }

    #[test]
    fn version_flag_succeeds() {
        assert_eq!(run(&["--version".to_string()]), ExitCode::Success);
    }

    #[test]
    fn help_flag_succeeds() {
        assert_eq!(run(&["--help".to_string()]), ExitCode::Success);
    }

    #[test]
    fn source_top_level_options_lists_real_flags_not_command_summaries() {
        // The "Options:" section must list actual flags/env vars, not a
        // duplicate of the "Commands:" list with per-verb argument summaries.
        let this_file = include_str!("main.rs");
        assert!(this_file.contains("--yes"), "Options must list --yes");
        assert!(
            this_file.contains("CCYNC_ASSUME_YES"),
            "Options/Environment must list CCYNC_ASSUME_YES"
        );
        assert!(
            this_file.contains("--dry-run"),
            "Options must list --dry-run"
        );
        // Split so this assertion's own source text doesn't self-match via
        // include_str!.
        let old_summary = ["Pick the master agent, ", "adopt + project to all agents"].concat();
        assert!(
            !this_file.contains(&old_summary),
            "Options must not duplicate per-command summaries (that belongs to per-verb --help)"
        );
    }

    #[test]
    fn no_args_prints_help_and_exits_success() {
        // Bare `ccync` shows help and exits 0 (standard no-subcommand behavior +
        // winget smoke-test tolerance). Unknown flags/commands still return Usage.
        assert_eq!(run(&[]), ExitCode::Success);
    }

    #[test]
    fn unknown_command_is_usage() {
        assert_eq!(run(&["frobnicate".to_string()]), ExitCode::Usage);
    }

    // Removed commands (folded into the command surface) are now unknown → Usage.
    // `update` is no longer here — it is a live public verb.
    #[test]
    fn removed_commands_are_unknown() {
        for cmd in ["install", "setup", "mcp", "resolve-catalog", "plugin"] {
            assert_eq!(
                run(&[cmd.to_string()]),
                ExitCode::Usage,
                "removed command '{cmd}' must be unknown (Usage)"
            );
        }
    }

    #[test]
    fn list_is_wired_not_not_wired() {
        let _guard = io_lock();
        let result = run(&["list".to_string()]);
        assert_ne!(result, ExitCode::NotWired, "list must be wired");
    }

    #[test]
    fn init_unknown_master_is_usage() {
        assert_eq!(
            run(&["init".to_string(), "frobnicate".to_string()]),
            ExitCode::Usage,
            "init with an unknown master must be a usage error"
        );
    }

    #[test]
    fn sync_is_wired_runs_unified_projection_in_isolated_home() {
        // cmd_sync runs the full unified engine (run_update + run_machine_update
        // + the write-ahead MCP projection), all of which write to home-derived
        // paths. Isolate the home to a temp dir and hold IO_LOCK for the entire
        // test so no concurrent I/O test reads the redirected HOME.
        let _guard = io_lock();

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
            unsafe {
                std::env::set_var("HOME", tmp.path());
            }
        }

        let ccync_home = tmp.path().join(".ccync");
        std::fs::create_dir_all(&ccync_home).unwrap();
        std::fs::write(ccync_home.join("config.json"), r#"{}"#).unwrap();

        let result = run(&["sync".to_string(), "--yes".to_string()]);

        // Restore before assertions so any panic also leaves env clean.
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

        assert_ne!(result, ExitCode::NotWired, "sync must be wired");
        assert_eq!(
            result,
            ExitCode::Success,
            "unified sync path must succeed on a clean isolated home"
        );
        assert!(
            tmp.path()
                .join(".ccync")
                .join("build")
                .join("render")
                .is_dir(),
            "sync must render the canonical root via run_update"
        );
    }

    #[test]
    fn uninstall_is_wired_not_not_wired() {
        let _guard = io_lock();
        let result = run(&["uninstall".to_string()]);
        assert_ne!(result, ExitCode::NotWired, "uninstall must be wired");
    }

    // doctor is wired
    #[test]
    fn doctor_is_wired_not_not_wired() {
        let _guard = io_lock();
        let result = run(&["doctor".to_string()]);
        assert_ne!(result, ExitCode::NotWired, "doctor must be wired");
    }

    #[test]
    fn doctor_dry_run_is_wired() {
        let _guard = io_lock();
        let result = run(&["doctor".to_string(), "--dry-run".to_string()]);
        assert_ne!(result, ExitCode::NotWired, "doctor --dry-run must be wired");
    }

    #[test]
    fn doctor_command_remains_wired_after_domain_health_aggregation() {
        let _guard = io_lock();
        let result = run(&["doctor".to_string()]);
        assert_ne!(
            result,
            ExitCode::NotWired,
            "doctor must stay wired after aggregation"
        );
    }

    #[test]
    fn exit_code_values_are_correct() {
        assert_eq!(ExitCode::Success as u8, 0);
        assert_eq!(ExitCode::Error as u8, 1);
        assert_eq!(ExitCode::NotWired as u8, 2);
        assert_eq!(ExitCode::Usage as u8, 64);
    }

    /// Point HOME/USERPROFILE at `home` for the duration of `f`, then restore.
    /// Caller must hold `io_lock()` to serialize the process-global env swap.
    fn with_env_home<T>(home: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let orig;
        #[cfg(windows)]
        unsafe {
            orig = std::env::var("USERPROFILE").ok();
            std::env::set_var("USERPROFILE", home);
            std::env::set_var("HOME", home);
        }
        #[cfg(not(windows))]
        unsafe {
            orig = std::env::var("HOME").ok();
            std::env::set_var("HOME", home);
        }
        let out = f();
        #[cfg(windows)]
        unsafe {
            match &orig {
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
            match &orig {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        out
    }

    /// Canary: every verb in `CommandKind::ALL` must reject an unknown flag with
    /// `ExitCode::Usage`. Runs in a sandboxed HOME so even a verb that (in a
    /// regression) failed to wire the guard touches no real machine state.
    /// Discrimination scope: this is a smoke-net — it is
    /// only *diagnostic* for verbs that would otherwise run normally; verbs
    /// whose bare call already returns Usage pass coincidentally. The
    /// authoritative per-verb oracles are the tests in the command modules.
    #[test]
    fn all_verbs_reject_unknown_flag_as_usage() {
        let _guard = io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        with_env_home(tmp.path(), || {
            for kind in CommandKind::ALL {
                let verb = kind.as_str();
                let result = run(&[verb.to_string(), "--frobnicate-canary".to_string()]);
                assert_eq!(
                    result,
                    ExitCode::Usage,
                    "verb '{verb}' must reject an unknown flag with Usage (64)"
                );
            }
        });
    }

    /// Legal-call zero-false-reject probes: value-flag values must
    /// not be misjudged as unknown flags or over-cap positionals.
    #[test]
    fn value_flag_legal_calls_are_not_falsely_rejected() {
        let _guard = io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let out_dir = tmp.path().join("bkp");

        with_env_home(tmp.path(), || {
            // backup --output <dir>: separated value form, must succeed.
            let backup = run(&[
                "backup".to_string(),
                "--output".to_string(),
                out_dir.to_string_lossy().to_string(),
            ]);
            assert_eq!(
                backup,
                ExitCode::Success,
                "backup --output <dir> must not be falsely rejected"
            );

            // refresh --source=<path>: refresh no longer accepts a source flag.
            let refresh = run(&[
                "refresh".to_string(),
                format!("--source={}", tmp.path().to_string_lossy()),
            ]);
            assert_eq!(
                refresh,
                ExitCode::Usage,
                "refresh --source=<path> must be rejected as a usage error"
            );
        });
    }

    // Verify that the public help surface (PUBLIC array) does not include the
    // internal developer commands, while ALL includes all 18 commands.
    #[test]
    fn help_surface_excludes_internal_commands() {
        let public_strs: Vec<&str> = CommandKind::PUBLIC.iter().map(|c| c.as_str()).collect();
        assert_eq!(
            public_strs.len(),
            16,
            "PUBLIC must list exactly 16 commands"
        );
        assert!(
            !public_strs.contains(&"refresh"),
            "refresh must not appear in PUBLIC (help surface)"
        );
        assert!(
            !public_strs.contains(&"rollback"),
            "rollback must not appear in PUBLIC (help surface)"
        );
        // All known public commands must be present, including the new verbs.
        for expected in [
            "init",
            "sync",
            "add",
            "remove",
            "list",
            "doctor",
            "backup",
            "restore",
            "uninstall",
            "update",
            "upgrade",
            "show",
            "cleanup",
            "pin",
            "search",
            "repair",
        ] {
            assert!(
                public_strs.contains(&expected),
                "public command '{expected}' missing from PUBLIC"
            );
        }
    }

    #[test]
    fn help_output_contains_update_and_upgrade() {
        let output = std::process::Command::new("cargo")
            .args(["run", "--bin", "ccync", "--", "--help"])
            .output()
            .expect("failed to execute cargo run");
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("update"),
            "help output must contain 'update'"
        );
        assert!(
            stdout.contains("upgrade"),
            "help output must contain 'upgrade'"
        );
    }

    #[test]
    fn every_public_verb_help_returns_success_and_prints_usage() {
        // Rebuilt from `CommandKind::PUBLIC` at execution time (not a
        // stale snapshot) — every public verb's `<verb> --help` must print
        // usage and exit Success. Internal refresh/rollback are excluded
        // from this loop on purpose (covered separately below; they are not
        // part of the public-help completion condition).
        let _guard = test_io_lock();
        for cmd in ccync_engine::CommandKind::PUBLIC {
            let verb = cmd.as_str();
            let exit = run(&[verb.to_string(), "--help".to_string()]);
            assert_eq!(
                exit,
                ExitCode::Success,
                "`ccync {verb} --help` must return Success, got {exit:?}"
            );
        }
    }

    #[test]
    fn dispatch_recognizes_update_and_upgrade() {
        assert_eq!(
            run(&["update".to_string(), "--help".to_string()]),
            ExitCode::Success,
            "update subcommand help dispatch must succeed"
        );
        assert_eq!(
            run(&["upgrade".to_string(), "--help".to_string()]),
            ExitCode::Success,
            "upgrade subcommand help dispatch must succeed"
        );
    }

    // Internal commands must be dispatchable (not Usage) even though absent from --help.
    // Probe via the `--help` path so the assertion is hermetic (no real-HOME
    // projection write) and independent of git tag state (rollback
    // without --yes returns Usage once a stable release tag exists).
    #[test]
    fn internal_commands_are_dispatchable_not_unknown() {
        // refresh is wired — the --help path returns Success, never Usage.
        assert_ne!(
            run(&["refresh".to_string(), "--help".to_string()]),
            ExitCode::Usage,
            "refresh must be dispatchable, not a Usage error"
        );
        // rollback is wired — the --help path returns Success, never Usage,
        // regardless of repo shape or whether a release tag exists.
        assert_ne!(
            run(&["rollback".to_string(), "--help".to_string()]),
            ExitCode::Usage,
            "rollback must be dispatchable, not a Usage error"
        );
    }
}
