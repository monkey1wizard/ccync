//! `ccync` CLI entry point — cross-agent plugin / MCP / skills manager.
//!
//! Single binary entry for the 9-command surface: init, sync, add, remove,
//! list, doctor, backup, restore, uninstall.

mod commands;

use ccync_engine::{classify_args, Action, CommandKind, ExitCode};
use commands::doctor::cmd_doctor;
use commands::init::cmd_init;
use commands::lifecycle::{cmd_backup, cmd_restore, cmd_sync, cmd_uninstall};
use commands::plugin::{cmd_add, cmd_list, cmd_remove};
use std::process::ExitCode as ProcessExitCode;

fn print_help() {
    println!("ccync — cross-agent plugin / MCP / skills manager");
    println!();
    println!("Usage: ccync <command> [options]");
    println!("       ccync --version");
    println!("       ccync --help");
    println!();
    println!("Commands:");
    for cmd in CommandKind::ALL {
        println!("  {}", cmd.as_str());
    }
    println!();
    println!("Options:");
    println!(
        "  init [<claude|codex>]             Pick the master agent, adopt + project to all agents"
    );
    println!(
        "  sync [--dry-run]                  Re-resolve + re-project the managed set to all agents"
    );
    println!("  add <link> [--no-sync]            Add a plugin/MCP/skill, then auto-sync");
    println!("  remove <id>                       Remove a managed item, then auto-sync");
    println!("  list                              List the managed set");
    println!("  doctor [--dry-run]                Read-only management health check");
    println!("  backup [--output <dir>]           Export canonical machine-local state files");
    println!("  restore --from <dir>              Restore canonical machine-local state files");
    println!("  uninstall                         Remove the canonical root and runtime surfaces");
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
            print_help();
            ExitCode::Usage
        }
        Action::NotWired(CommandKind::Init) => cmd_init(args),
        Action::NotWired(CommandKind::Sync) => cmd_sync(args),
        Action::NotWired(CommandKind::Add) => cmd_add(args),
        Action::NotWired(CommandKind::Remove) => cmd_remove(args),
        Action::NotWired(CommandKind::List) => cmd_list(),
        Action::NotWired(CommandKind::Doctor) => cmd_doctor(args),
        // backup/restore: machine-local canonical state file export/import.
        Action::NotWired(CommandKind::Backup) => cmd_backup(args),
        Action::NotWired(CommandKind::Restore) => cmd_restore(args),
        Action::NotWired(CommandKind::Uninstall) => cmd_uninstall(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_flag_succeeds() {
        assert_eq!(run(&["--version".to_string()]), ExitCode::Success);
    }

    #[test]
    fn help_flag_succeeds() {
        assert_eq!(run(&["--help".to_string()]), ExitCode::Success);
    }

    #[test]
    fn no_args_is_usage() {
        assert_eq!(run(&[]), ExitCode::Usage);
    }

    #[test]
    fn unknown_command_is_usage() {
        assert_eq!(run(&["frobnicate".to_string()]), ExitCode::Usage);
    }

    // Removed commands (folded into the 9-command surface) are now unknown → Usage.
    #[test]
    fn removed_commands_are_unknown() {
        for cmd in [
            "install",
            "update",
            "setup",
            "mcp",
            "resolve-catalog",
            "plugin",
        ] {
            assert_eq!(
                run(&[cmd.to_string()]),
                ExitCode::Usage,
                "removed command '{cmd}' must be unknown (Usage)"
            );
        }
    }

    #[test]
    fn list_is_wired_not_not_wired() {
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
        // cmd_sync now runs the full unified engine (run_update + run_machine_update
        // + run_mcp_update), all of which write to home-derived paths. Isolate the
        // home to a temp dir so the suite never mutates the developer's real agent
        // configs, and assert the unified path runs end-to-end (not NotWired).
        use std::sync::{Mutex, OnceLock};
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _guard = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
            std::env::set_var("HOME", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let result = run(&["sync".to_string(), "--yes".to_string()]);
        assert_ne!(result, ExitCode::NotWired, "sync must be wired");
        assert_eq!(
            result,
            ExitCode::Success,
            "unified sync path must succeed on a clean isolated home"
        );
        // run_update rendered the canonical root under the isolated home.
        assert!(
            tmp.path()
                .join(".ccync")
                .join("plugins")
                .join("ccync")
                .is_dir(),
            "sync must render the canonical root via run_update"
        );
    }

    #[test]
    fn uninstall_is_wired_not_not_wired() {
        let result = run(&["uninstall".to_string()]);
        assert_ne!(result, ExitCode::NotWired, "uninstall must be wired");
    }

    // doctor is wired
    #[test]
    fn doctor_is_wired_not_not_wired() {
        let result = run(&["doctor".to_string()]);
        assert_ne!(result, ExitCode::NotWired, "doctor must be wired");
    }

    #[test]
    fn doctor_dry_run_is_wired() {
        let result = run(&["doctor".to_string(), "--dry-run".to_string()]);
        assert_ne!(result, ExitCode::NotWired, "doctor --dry-run must be wired");
    }

    #[test]
    fn doctor_command_remains_wired_after_domain_health_aggregation() {
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
}
