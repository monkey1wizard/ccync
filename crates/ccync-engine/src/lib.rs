//! ccync-engine: shared primitives for the `ccync` CLI.
//!
//! Provides configuration parsing (Rust owns config truth), command enum,
//! exit-code classification, and path resolution.

pub mod adopt;
pub mod cache;
pub mod catalog;
pub mod delivery;
pub mod delivery_apply;
pub mod doctor;
pub mod install;
pub mod managed_view;
pub mod migrate;
pub mod plugin_native;
pub mod plugin_render;
pub mod plugin_upgrade;
pub mod roster;
pub mod self_update;
pub mod tool_manager;
pub mod truth;

// Foundation modules live in the `base` crate; re-exported here so existing
// `crate::config` / `ccync_engine::config` consumer paths keep resolving.
pub use adopt::remove_adopted_item;
pub use ccync_foundation::{config, paths};
pub use managed_view::{
    build_managed_view, build_managed_view_with_shadows, ItemKind as ManagedItemKind, ManagedItem,
    Origin as ManagedOrigin,
};
pub use plugin_upgrade::{
    apply_upgrade, apply_upgrade_rewriting_source, resolve_adopted_upgrades, resolve_upgrades,
    run_upgrade, AdoptedUpgradeOutcome, UpgradeChange, UpgradeError, UpgradeOpts, UpgradeOutcome,
    UpgradeReport,
};
pub use self_update::{detect_provenance, InstallProvenance};

/// Shared test-only helpers used across this crate's `#[cfg(test)]` modules.
///
/// Several modules (`adopt`, `install`, `plugin_upgrade`) have unit tests that
/// mutate process-wide home and storage-root env vars. `cargo test` runs
/// multithreaded by default, so those tests must serialize against a SINGLE
/// shared lock — not one private lock per module — or two tests in different
/// modules can race on the same env var concurrently.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    pub(crate) struct TestHome {
        _env: MutexGuard<'static, ()>,
        home: tempfile::TempDir,
        saved: [(&'static str, Option<std::ffi::OsString>); 7],
    }

    impl TestHome {
        pub(crate) fn new() -> Self {
            let env = env_guard();
            let home = tempfile::tempdir().unwrap();
            let saved = [
                "HOME",
                "USERPROFILE",
                "APPDATA",
                "LOCALAPPDATA",
                "XDG_CONFIG_HOME",
                "XDG_STATE_HOME",
                "XDG_CACHE_HOME",
            ]
            .map(|key| {
                let value = std::env::var_os(key);
                let target = match key {
                    "APPDATA" | "XDG_CONFIG_HOME" => home.path().join("C"),
                    "LOCALAPPDATA" | "XDG_STATE_HOME" => home.path().join("S"),
                    "XDG_CACHE_HOME" => home.path().join("K"),
                    _ => home.path().to_path_buf(),
                };
                unsafe { std::env::set_var(key, target) };
                (key, value)
            });
            Self {
                _env: env,
                home,
                saved,
            }
        }

        pub(crate) fn path(&self) -> &Path {
            self.home.path()
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    /// Serialize tests that mutate process-global env vars (`USERPROFILE`/`HOME`)
    /// across every module in this crate. Poison-tolerant: a panicking test (a
    /// failed assertion) must not cascade-fail every other env test by
    /// poisoning the lock.
    pub(crate) fn env_guard() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod subpath_selector_tests;

#[cfg(test)]
mod plugin_mcp_field_tests;

/// Known subcommands of the `ccync` CLI.
///
/// Public (advertised in `--help`): Init, Sync, Add, Remove, List, Doctor,
/// Backup, Restore, Uninstall, Update, Upgrade, Show, Cleanup, Pin, Search,
/// Repair, Roster, Tool, Outdated — 19 commands.
/// Internal (dispatchable but not advertised): Refresh, Rollback — 2 commands.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum CommandKind {
    /// `ccync init [<claude|codex|copilot|opencode|agy>]` — pick one of five
    /// sources, adopt its content into the ccync truth store, and choose the
    /// projection targets.
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
    /// `ccync update [--check]` — self-update the `ccync` binary (brew-update
    /// semantics); refuses + redirects under a package manager.
    Update,
    /// `ccync upgrade [<plugin>...]` — upgrade installed git-source plugins
    /// (brew-upgrade semantics); default all, optional single id.
    Upgrade,
    /// `ccync show <id>` — print full detail of one installed plugin
    /// (brew-info / winget-show semantics).
    Show,
    /// `ccync cleanup [--dry-run]` — prune orphaned `~/.ccync/cache/<id>@<sha>`
    /// dirs no longer referenced by the lockfile (brew-cleanup semantics).
    Cleanup,
    /// `ccync pin <id>` / `--remove <id>` / `--list` — hold a plugin against
    /// `upgrade` (winget-pin semantics; state in the `held` field).
    Pin,
    /// `ccync search <term> [--limit N] [--no-add]` — discover public GitHub/GitLab
    /// repository clone URLs from a name and hand off to `ccync add` after confirmation.
    Search,
    /// `ccync repair [<id>]` — resolve a knowable clonable source for an
    /// adopted item that has none (or explicitly overwrite one), via
    /// user-confirmed search. Pre-init, non-projecting.
    Repair,
    /// `ccync roster dump|install` — manage shared team rosters.
    Roster,
    /// `ccync refresh` — internal/dev: rebuild derived layer from source
    /// without running the full public `sync` lifecycle.
    Refresh,
    /// `ccync rollback [--yes]` — internal/dev: roll back the ccync source repo
    /// to the latest stable release tag, then chain `refresh`.
    Rollback,
    /// `ccync tool <discover|track|untrack|refresh>` — manage external tools.
    Tool,
    /// `ccync outdated` — inspect available updates.
    Outdated,
}

impl CommandKind {
    /// All known subcommands (public + internal), in definition order.
    /// Public surface is 19 commands; internal adds `Refresh` and `Rollback`.
    pub const ALL: [CommandKind; 21] = [
        CommandKind::Init,
        CommandKind::Sync,
        CommandKind::Add,
        CommandKind::Remove,
        CommandKind::List,
        CommandKind::Doctor,
        CommandKind::Backup,
        CommandKind::Restore,
        CommandKind::Uninstall,
        CommandKind::Update,
        CommandKind::Upgrade,
        CommandKind::Show,
        CommandKind::Cleanup,
        CommandKind::Pin,
        CommandKind::Search,
        CommandKind::Repair,
        CommandKind::Roster,
        CommandKind::Refresh,
        CommandKind::Rollback,
        CommandKind::Tool,
        CommandKind::Outdated,
    ];

    /// The public 19 commands listed in `--help`.
    pub const PUBLIC: [CommandKind; 19] = [
        CommandKind::Init,
        CommandKind::Sync,
        CommandKind::Add,
        CommandKind::Remove,
        CommandKind::List,
        CommandKind::Doctor,
        CommandKind::Backup,
        CommandKind::Restore,
        CommandKind::Uninstall,
        CommandKind::Update,
        CommandKind::Upgrade,
        CommandKind::Show,
        CommandKind::Cleanup,
        CommandKind::Pin,
        CommandKind::Search,
        CommandKind::Repair,
        CommandKind::Roster,
        CommandKind::Tool,
        CommandKind::Outdated,
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
            CommandKind::Update => "update",
            CommandKind::Upgrade => "upgrade",
            CommandKind::Show => "show",
            CommandKind::Cleanup => "cleanup",
            CommandKind::Pin => "pin",
            CommandKind::Search => "search",
            CommandKind::Repair => "repair",
            CommandKind::Roster => "roster",
            CommandKind::Refresh => "refresh",
            CommandKind::Rollback => "rollback",
            CommandKind::Tool => "tool",
            CommandKind::Outdated => "outdated",
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
            "update" => Some(CommandKind::Update),
            "upgrade" => Some(CommandKind::Upgrade),
            "show" => Some(CommandKind::Show),
            "cleanup" => Some(CommandKind::Cleanup),
            "pin" => Some(CommandKind::Pin),
            "search" => Some(CommandKind::Search),
            "repair" => Some(CommandKind::Repair),
            "roster" => Some(CommandKind::Roster),
            "refresh" => Some(CommandKind::Refresh),
            "rollback" => Some(CommandKind::Rollback),
            "tool" => Some(CommandKind::Tool),
            "outdated" => Some(CommandKind::Outdated),
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
    /// Read-only "is anything outdated" query found pending upgrades. Distinct
    /// from `Error` so a script can tell "updates available" from a real failure
    /// (used by `list --upgrade-available`, `upgrade --dry-run`,
    /// `roster install --dry-run`, and `restore --dry-run`; numeric contract:
    /// `UpdatesAvailable = 3`).
    UpdatesAvailable = 3,
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
        // Public command surface (14 verbs; the first nine asserted here, the
        // rest below).
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
        // New public verbs (brew-update / brew-upgrade semantics).
        assert_eq!(CommandKind::parse("update"), Some(CommandKind::Update));
        assert_eq!(CommandKind::parse("upgrade"), Some(CommandKind::Upgrade));
        // Internal commands (dispatchable, not in --help).
        assert_eq!(CommandKind::parse("refresh"), Some(CommandKind::Refresh));
        assert_eq!(CommandKind::parse("rollback"), Some(CommandKind::Rollback));
        assert_eq!(CommandKind::parse("tool"), Some(CommandKind::Tool));
        assert_eq!(CommandKind::parse("outdated"), Some(CommandKind::Outdated));
        // Removed commands (folded into the command surface) must no longer parse.
        assert_eq!(CommandKind::parse("install"), None);
        assert_eq!(CommandKind::parse("setup"), None);
        assert_eq!(CommandKind::parse("mcp"), None);
        assert_eq!(CommandKind::parse("resolve-catalog"), None);
        assert_eq!(CommandKind::parse("plugin"), None);
        // Workflow commands are gone — must no longer parse.
        assert_eq!(CommandKind::parse("pipeline"), None);
        assert_eq!(CommandKind::parse("frobnicate"), None);
        // ALL includes public (19) + internal (2) = 21; PUBLIC is exactly 19.
        assert_eq!(CommandKind::ALL.len(), 21);
        assert_eq!(CommandKind::PUBLIC.len(), 19);
        assert!(CommandKind::PUBLIC.contains(&CommandKind::Tool));
        assert!(CommandKind::PUBLIC.contains(&CommandKind::Outdated));
        // The new verbs are part of the advertised public surface.
        assert!(CommandKind::PUBLIC.contains(&CommandKind::Update));
        assert!(CommandKind::PUBLIC.contains(&CommandKind::Upgrade));
        assert!(CommandKind::PUBLIC.contains(&CommandKind::Show));
        assert!(CommandKind::PUBLIC.contains(&CommandKind::Cleanup));
        assert!(CommandKind::PUBLIC.contains(&CommandKind::Pin));
        assert!(CommandKind::PUBLIC.contains(&CommandKind::Search));
        assert!(CommandKind::PUBLIC.contains(&CommandKind::Repair));
        // as_str / parse roundtrip for the new verbs.
        assert_eq!(
            CommandKind::parse(CommandKind::Update.as_str()),
            Some(CommandKind::Update)
        );
        assert_eq!(
            CommandKind::parse(CommandKind::Upgrade.as_str()),
            Some(CommandKind::Upgrade)
        );
        assert_eq!(CommandKind::parse("show"), Some(CommandKind::Show));
        assert_eq!(
            CommandKind::parse(CommandKind::Show.as_str()),
            Some(CommandKind::Show)
        );
        assert_eq!(CommandKind::parse("cleanup"), Some(CommandKind::Cleanup));
        assert_eq!(
            CommandKind::parse(CommandKind::Cleanup.as_str()),
            Some(CommandKind::Cleanup)
        );
        assert_eq!(CommandKind::parse("pin"), Some(CommandKind::Pin));
        assert_eq!(
            CommandKind::parse(CommandKind::Pin.as_str()),
            Some(CommandKind::Pin)
        );
        assert_eq!(CommandKind::parse("search"), Some(CommandKind::Search));
        assert_eq!(
            CommandKind::parse(CommandKind::Search.as_str()),
            Some(CommandKind::Search)
        );
        assert_eq!(CommandKind::parse("repair"), Some(CommandKind::Repair));
        assert_eq!(
            CommandKind::parse(CommandKind::Repair.as_str()),
            Some(CommandKind::Repair)
        );
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
        assert_eq!(ExitCode::UpdatesAvailable as u8, 3);
        assert_eq!(ExitCode::Usage as u8, 64);
    }

    #[test]
    fn roster_verb_wired_into_command_kind() {
        assert_eq!(CommandKind::ALL.len(), 21);
        assert_eq!(CommandKind::PUBLIC.len(), 19);
        assert_eq!(
            CommandKind::parse("roster").map(CommandKind::as_str),
            Some("roster")
        );
        assert!(CommandKind::PUBLIC.contains(&CommandKind::parse("roster").unwrap()));
    }
}
