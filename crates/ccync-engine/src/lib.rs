//! ccync-engine: shared primitives for the `ccync` CLI.
//!
//! Provides configuration parsing (Rust owns config truth), command enum,
//! exit-code classification, and path resolution.

pub mod adopt;
pub mod catalog;
pub mod doctor;
pub mod install;
pub mod truth;

// Foundation modules live in the `base` crate; re-exported here so existing
// `crate::config` / `ccync_engine::config` consumer paths keep resolving.
pub use ccync_foundation::{config, ledger, paths};

/// Known subcommands of the `ccync` CLI — the 9-command surface.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum CommandKind {
    /// `ccync init [<claude|codex>]` — pick the master agent, adopt its content
    /// into the ccync truth store, and project to all agents.
    Init,
    /// `ccync sync [--dry-run]` — the unified engine: re-resolve + re-project the
    /// managed set (plugins + MCP + skills) to every agent.
    Sync,
    /// `ccync add <link> [--no-sync]` — add a plugin/MCP/skill to ccync, then auto-sync.
    Add,
    /// `ccync remove <id>` — remove a managed item, then auto-sync.
    Remove,
    /// `ccync list` — list the managed set.
    List,
    /// `ccync doctor [--dry-run]` — read-only management health check.
    Doctor,
    /// `ccync backup [--output <dir>]` — export canonical machine-local state files.
    Backup,
    /// `ccync restore --from <dir>` — restore canonical machine-local state files.
    Restore,
    /// `ccync uninstall` — remove the canonical root and runtime surfaces.
    Uninstall,
}

impl CommandKind {
    /// Every known subcommand, in help/display order.
    pub const ALL: [CommandKind; 9] = [
        CommandKind::Init,
        CommandKind::Sync,
        CommandKind::Add,
        CommandKind::Remove,
        CommandKind::List,
        CommandKind::Doctor,
        CommandKind::Backup,
        CommandKind::Restore,
        CommandKind::Uninstall,
    ];

    /// The canonical CLI spelling of this subcommand.
    pub fn as_str(self) -> &'static str {
        match self {
            CommandKind::Init => "init",
            CommandKind::Sync => "sync",
            CommandKind::Add => "add",
            CommandKind::Remove => "remove",
            CommandKind::List => "list",
            CommandKind::Doctor => "doctor",
            CommandKind::Backup => "backup",
            CommandKind::Restore => "restore",
            CommandKind::Uninstall => "uninstall",
        }
    }

    /// Parse a token into a known subcommand, or `None` if unrecognized.
    pub fn parse(input: &str) -> Option<CommandKind> {
        match input.trim().to_ascii_lowercase().as_str() {
            "init" => Some(CommandKind::Init),
            "sync" => Some(CommandKind::Sync),
            "add" => Some(CommandKind::Add),
            "remove" => Some(CommandKind::Remove),
            "list" => Some(CommandKind::List),
            "doctor" => Some(CommandKind::Doctor),
            "backup" => Some(CommandKind::Backup),
            "restore" => Some(CommandKind::Restore),
            "uninstall" => Some(CommandKind::Uninstall),
            _ => None,
        }
    }
}

/// Process exit-code classification for the CLI.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ExitCode {
    /// Completed successfully (and doctor: no errors, warnings only).
    Success = 0,
    /// Operation failed (install error, render error, uninstall error, doctor errors).
    Error = 1,
    /// Subcommand recognized but not yet wired to the legacy engine (Phase 1).
    NotWired = 2,
    /// Bad usage: unknown command or missing command.
    Usage = 64,
}

impl From<ExitCode> for std::process::ExitCode {
    fn from(code: ExitCode) -> Self {
        std::process::ExitCode::from(code as u8)
    }
}

/// The action the CLI should take for a given argv (binary name already stripped).
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Version,
    Help,
    /// A known subcommand dispatched by the CLI binary rather than handled in-engine.
    NotWired(CommandKind),
    UnknownCommand(String),
    MissingCommand,
}

/// Classify raw CLI arguments (binary name already stripped).
///
/// Pure: never touches the filesystem or machine config.
pub fn classify_args(args: &[String]) -> Action {
    let first = match args.first() {
        Some(arg) => arg.as_str(),
        None => return Action::MissingCommand,
    };

    match first {
        "--version" | "-V" | "version" => Action::Version,
        "--help" | "-h" | "help" => Action::Help,
        other => match CommandKind::parse(other) {
            Some(cmd) => Action::NotWired(cmd),
            None => Action::UnknownCommand(other.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_known_command() {
        assert_eq!(CommandKind::parse("init"), Some(CommandKind::Init));
        assert_eq!(CommandKind::parse("sync"), Some(CommandKind::Sync));
        assert_eq!(CommandKind::parse("add"), Some(CommandKind::Add));
        assert_eq!(CommandKind::parse("remove"), Some(CommandKind::Remove));
        assert_eq!(CommandKind::parse("list"), Some(CommandKind::List));
        assert_eq!(CommandKind::parse("doctor"), Some(CommandKind::Doctor));
        assert_eq!(CommandKind::parse("backup"), Some(CommandKind::Backup));
        assert_eq!(CommandKind::parse("restore"), Some(CommandKind::Restore));
        assert_eq!(
            CommandKind::parse("uninstall"),
            Some(CommandKind::Uninstall)
        );
        assert_eq!(CommandKind::parse(" DOCTOR "), Some(CommandKind::Doctor));
        // Removed commands (folded into the 9-command surface) must no longer parse.
        assert_eq!(CommandKind::parse("install"), None);
        assert_eq!(CommandKind::parse("update"), None);
        assert_eq!(CommandKind::parse("setup"), None);
        assert_eq!(CommandKind::parse("mcp"), None);
        assert_eq!(CommandKind::parse("resolve-catalog"), None);
        assert_eq!(CommandKind::parse("plugin"), None);
        // Workflow commands are gone — must no longer parse.
        assert_eq!(CommandKind::parse("pipeline"), None);
        assert_eq!(CommandKind::parse("frobnicate"), None);
        assert_eq!(CommandKind::ALL.len(), 9);
    }

    #[test]
    fn classifies_version_and_help() {
        assert_eq!(classify_args(&["--version".to_string()]), Action::Version);
        assert_eq!(classify_args(&["-V".to_string()]), Action::Version);
        assert_eq!(classify_args(&["--help".to_string()]), Action::Help);
        assert_eq!(classify_args(&[]), Action::MissingCommand);
    }

    #[test]
    fn classifies_known_and_unknown_subcommands() {
        assert_eq!(
            classify_args(&["doctor".to_string()]),
            Action::NotWired(CommandKind::Doctor)
        );
        assert_eq!(
            classify_args(&["frobnicate".to_string()]),
            Action::UnknownCommand("frobnicate".to_string())
        );
    }

    #[test]
    fn exit_codes_map_to_process_codes() {
        assert_eq!(ExitCode::Success as u8, 0);
        assert_eq!(ExitCode::NotWired as u8, 2);
        assert_eq!(ExitCode::Usage as u8, 64);
    }
}
