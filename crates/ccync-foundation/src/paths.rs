//! Path location resolution for CCYNC state directories.
//!
//! All functions return `Option<PathBuf>` — `None` when home is unavailable.
//! No file I/O; callers decide whether to create directories.
//! Cross-platform: `USERPROFILE` on Windows, `HOME` on Unix.

use std::path::PathBuf;

use crate::target::ProjectionTargetId;

// ── home ─────────────────────────────────────────────────────────────────────

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// The effective user home directory used by CCYNC path resolution.
pub fn user_home() -> Option<PathBuf> {
    home_dir()
}

/// The CCYNC home directory (`~/.ccync`).
pub fn ccync_home() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".ccync"))
}

/// The effective platform config directory, env-based and semantically aligned
/// with `dirs::config_dir()`: Windows `%APPDATA%` (fallback `home/AppData/Roaming`),
/// Unix `$XDG_CONFIG_HOME` (fallback `home/.config`).
pub fn config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| home_dir().map(|h| h.join("AppData").join("Roaming")))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home_dir().map(|h| h.join(".config")))
    }
}

// ── config paths ─────────────────────────────────────────────────────────────

/// `~/.ccync/config.json`
pub fn machine_config_path() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("config.json"))
}

/// `~/.ccync/AGENTS.md`
pub fn ccync_agents_md_path() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("AGENTS.md"))
}

// ── state paths ───────────────────────────────────────────────────────────────

// ── plugin paths ──────────────────────────────────────────────────────────────

/// `~/.ccync/plugins`
pub fn ccync_plugins_root() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("plugins"))
}

/// Canonical render root for the CCYNC-managed plugin tree (`~/.ccync/build/render`).
///
/// Shared by install/sync so they resolve the same root the projection layer uses.
pub fn canonical_plugin_root() -> Option<PathBuf> {
    ccync_build_root().map(|b| b.join("render"))
}

// ── cache paths ──────────────────────────────────────────────────────────────

/// `~/.ccync/cache`
pub fn ccync_cache_root() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("cache"))
}

// ── data paths ───────────────────────────────────────────────────────────────

/// `~/.ccync/data`
///
/// plugin-owned persistent data, created by the render, never pruned by ccync.
pub fn ccync_data_root() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("data"))
}

// ── build / output paths ──────────────────────────────────────────────────────

/// `~/.ccync/build`
///
/// Root of all derived OUTPUT artifacts. Rebuildable by `ccync sync`.
pub fn ccync_build_root() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("build"))
}

/// `~/.ccync/tools.json`
pub fn tools_json_path() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("tools.json"))
}

/// `~/.ccync/build/tools/state.json`
pub fn tool_state_path() -> Option<PathBuf> {
    ccync_build_root().map(|b| b.join("tools").join("state.json"))
}

/// `~/.ccync/build/tools/artifacts`
pub fn tool_artifacts_root() -> Option<PathBuf> {
    ccync_build_root().map(|b| b.join("tools").join("artifacts"))
}

/// `~/.ccync/build/tools/operation.txn.json`
pub fn tool_operation_journal_path() -> Option<PathBuf> {
    ccync_build_root().map(|b| b.join("tools").join("operation.txn.json"))
}

/// `~/.ccync/build/lifecycle.lock`
pub fn lifecycle_lock_path() -> Option<PathBuf> {
    ccync_build_root().map(|b| b.join("lifecycle.lock"))
}

/// `~/.ccync/install-receipts/ccync.json`
pub fn install_receipt_path() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("install-receipts").join("ccync.json"))
}

/// `~/.ccync/build/active/<provider>`
///
/// Per-provider projected output root under the build tree.
pub fn ccync_active_provider_path(provider: &str) -> Option<PathBuf> {
    ccync_build_root().map(|b| b.join("active").join(provider))
}

// ── personal paths ───────────────────────────────────────────────────────────

/// `~/.ccync/plugins.json`
///
/// Personal plugin catalog (catalog-v1 entry schema). Absent when no personal
/// plugins have been added; resolver treats absence as an empty additive set.
pub fn personal_catalog_path() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("plugins.json"))
}

/// `~/.ccync/build/lock.json`
///
/// The canonical catalog lockfile written by the resolve step and consumed
/// read-only by the install/projection layers.
pub fn plugins_lock_path() -> Option<PathBuf> {
    ccync_build_root().map(|b| b.join("lock.json"))
}

// ── agent state paths ─────────────────────────────────────────────────────────

/// `~/.claude/plugins/installed_plugins.json`
///
/// Claude installed plugin state (v2 schema: `{plugins:{"name@mkt":[...]}}`).
pub fn claude_installed_plugins_path() -> Option<PathBuf> {
    home_dir().map(|h| {
        h.join(".claude")
            .join("plugins")
            .join("installed_plugins.json")
    })
}

/// `~/.claude/plugins/known_marketplaces.json`
///
/// Claude known marketplaces list.
pub fn claude_known_marketplaces_path() -> Option<PathBuf> {
    home_dir().map(|h| {
        h.join(".claude")
            .join("plugins")
            .join("known_marketplaces.json")
    })
}

/// `~/.claude.json`
///
/// Claude top-level config (contains `mcpServers` entries).
pub fn claude_config_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".claude.json"))
}

/// `~/.claude/.mcp.json`
///
/// Claude's secondary MCP definition file. `claude_config_path()`
/// (`~/.claude.json`) is read first; this file fills in only names missing
/// from that store.
pub fn claude_mcp_json_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".claude").join(".mcp.json"))
}

/// `~/.codex/config.toml`
///
/// Codex config file; CCYNC reads the MCP section only.
pub fn codex_config_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".codex").join("config.toml"))
}

/// `~/.claude/skills`
///
/// Claude's loose-skill directory; each child dir with a `SKILL.md` is one skill.
pub fn claude_skills_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".claude").join("skills"))
}

/// `~/.agents/skills`
///
/// Shared skill directory read by Codex (and opencode/copilot); each child dir
/// with a `SKILL.md` is one skill.
pub fn shared_agents_skills_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".agents").join("skills"))
}

// ── user-scope instruction paths ─────────────────────────────────────────────

fn claude_user_scope_instruction_path() -> Option<PathBuf> {
    user_home().map(|h| h.join(".claude").join("CLAUDE.md"))
}

fn codex_user_scope_instruction_path() -> Option<PathBuf> {
    user_home().map(|h| h.join(".codex").join("AGENTS.md"))
}

fn copilot_user_scope_instruction_path() -> Option<PathBuf> {
    user_home().map(|h| h.join(".copilot").join("AGENTS.md"))
}

fn opencode_user_scope_instruction_path() -> Option<PathBuf> {
    user_home().map(|h| h.join(".config").join("opencode").join("AGENTS.md"))
}

fn antigravity_user_scope_instruction_path() -> Option<PathBuf> {
    user_home().map(|h| h.join(".gemini").join("AGENTS.md"))
}

/// Resolve the user-scope instruction file for a projection target.
pub fn user_scope_instruction_path(target: ProjectionTargetId) -> Option<PathBuf> {
    match target {
        ProjectionTargetId::Claude => claude_user_scope_instruction_path(),
        ProjectionTargetId::Codex => codex_user_scope_instruction_path(),
        ProjectionTargetId::Copilot => copilot_user_scope_instruction_path(),
        ProjectionTargetId::Opencode => opencode_user_scope_instruction_path(),
        ProjectionTargetId::AgyCli | ProjectionTargetId::AgyIde | ProjectionTargetId::AgyGui => {
            antigravity_user_scope_instruction_path()
        }
    }
}

// ── generated paths ───────────────────────────────────────────────────────────

/// `~/.ccync/build/mcp/managed.json`
///
/// MCP managed manifest written by `ccync sync` and consumed by MCP projection.
pub fn generated_mcp_path() -> Option<PathBuf> {
    ccync_build_root().map(|b| b.join("mcp").join("managed.json"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ccync_home_ends_with_dot_ccync() {
        if let Some(h) = ccync_home() {
            assert_eq!(h.file_name().and_then(|n| n.to_str()), Some(".ccync"));
        }
    }

    #[test]
    fn machine_config_path_ends_with_config_json() {
        if let (Some(p), Some(home)) = (machine_config_path(), ccync_home()) {
            assert_eq!(
                p.parent(),
                Some(home.as_path()),
                "config.json must be directly under ccync_home, no config/ subdir"
            );
            assert_eq!(p.file_name().and_then(|n| n.to_str()), Some("config.json"));
        }
    }

    #[test]
    fn ccync_plugins_root_leaf_is_plugins() {
        if let Some(p) = ccync_plugins_root() {
            assert_eq!(p.file_name().and_then(|n| n.to_str()), Some("plugins"));
        }
    }

    #[test]
    fn ccync_cache_root_leaf_is_cache() {
        if let Some(p) = ccync_cache_root() {
            assert_eq!(p.file_name().and_then(|n| n.to_str()), Some("cache"));
        }
    }

    #[test]
    fn ccync_data_root_leaf_is_data() {
        if let Some(p) = ccync_data_root() {
            assert_eq!(p.file_name().and_then(|n| n.to_str()), Some("data"));
        }
    }

    #[test]
    fn ccync_data_root_resolves_under_set_home_and_none_without() {
        let _guard = crate::test_support::env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);

        let fake_home = tempfile::TempDir::new().unwrap();
        std::env::set_var(var, fake_home.path());
        assert_eq!(
            ccync_data_root(),
            Some(fake_home.path().join(".ccync").join("data"))
        );

        std::env::remove_var(var);
        assert_eq!(ccync_data_root(), None);

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }
    }

    #[test]
    fn canonical_plugin_root_under_build_render() {
        if let Some(p) = canonical_plugin_root() {
            let s = p.to_string_lossy();
            assert!(s.contains("build"), "must be under build: {s}");
            assert!(
                s.ends_with("render") || s.ends_with("render\\"),
                "must end with render: {s}"
            );
        }
    }

    #[test]
    fn ccync_build_root_leaf_is_build() {
        if let Some(p) = ccync_build_root() {
            assert_eq!(p.file_name().and_then(|n| n.to_str()), Some("build"));
        }
    }

    #[test]
    fn tool_and_lifecycle_paths_resolve_under_temporary_home_and_none_without_home() {
        let _guard = crate::test_support::env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        let fake_home = tempfile::TempDir::new().unwrap();
        std::env::set_var(var, fake_home.path());

        assert_eq!(
            tools_json_path(),
            Some(fake_home.path().join(".ccync/tools.json"))
        );
        assert_eq!(
            tool_state_path(),
            Some(fake_home.path().join(".ccync/build/tools/state.json"))
        );
        assert_eq!(
            tool_artifacts_root(),
            Some(fake_home.path().join(".ccync/build/tools/artifacts"))
        );
        assert_eq!(
            tool_operation_journal_path(),
            Some(
                fake_home
                    .path()
                    .join(".ccync/build/tools/operation.txn.json")
            )
        );
        assert_eq!(
            lifecycle_lock_path(),
            Some(fake_home.path().join(".ccync/build/lifecycle.lock"))
        );
        assert_eq!(
            install_receipt_path(),
            Some(fake_home.path().join(".ccync/install-receipts/ccync.json"))
        );

        std::env::remove_var(var);
        assert_eq!(tools_json_path(), None);
        assert_eq!(tool_state_path(), None);
        assert_eq!(tool_artifacts_root(), None);
        assert_eq!(tool_operation_journal_path(), None);
        assert_eq!(lifecycle_lock_path(), None);
        assert_eq!(install_receipt_path(), None);

        match prior {
            Some(value) => std::env::set_var(var, value),
            None => std::env::remove_var(var),
        }
    }

    #[test]
    fn ccync_active_provider_path_under_build_active() {
        if let Some(p) = ccync_active_provider_path("claude") {
            let s = p.to_string_lossy();
            assert!(s.contains("build"), "must be under build: {s}");
            assert!(s.contains("active"), "must contain active: {s}");
            assert_eq!(p.file_name().and_then(|n| n.to_str()), Some("claude"));
        }
    }

    #[test]
    fn claude_installed_plugins_path_under_claude_plugins() {
        if let Some(p) = claude_installed_plugins_path() {
            let s = p.to_string_lossy();
            assert!(s.contains(".claude"), "must be under .claude: {s}");
            assert!(
                s.ends_with("installed_plugins.json"),
                "must end with installed_plugins.json: {s}"
            );
        }
    }

    #[test]
    fn claude_known_marketplaces_path_under_claude_plugins() {
        if let Some(p) = claude_known_marketplaces_path() {
            let s = p.to_string_lossy();
            assert!(s.contains(".claude"), "must be under .claude: {s}");
            assert!(
                s.ends_with("known_marketplaces.json"),
                "must end with known_marketplaces.json: {s}"
            );
        }
    }

    #[test]
    fn claude_config_path_ends_with_claude_json() {
        if let Some(p) = claude_config_path() {
            let s = p.to_string_lossy();
            assert!(
                s.ends_with(".claude.json"),
                "must end with .claude.json: {s}"
            );
        }
    }

    #[test]
    fn claude_mcp_json_path_ends_with_claude_mcp_json() {
        if let Some(p) = claude_mcp_json_path() {
            let s = p.to_string_lossy();
            assert!(
                s.ends_with(".claude/.mcp.json") || s.ends_with(".claude\\.mcp.json"),
                "must end with .claude/.mcp.json: {s}"
            );
        }
    }

    #[test]
    fn codex_config_path_under_codex_dir() {
        if let Some(p) = codex_config_path() {
            let s = p.to_string_lossy();
            assert!(s.contains(".codex"), "must be under .codex: {s}");
            assert!(s.ends_with("config.toml"), "must end with config.toml: {s}");
        }
    }

    #[test]
    fn all_agent_state_paths_under_home() {
        if let Some(home) = home_dir() {
            for (label, path) in [
                ("claude_installed_plugins", claude_installed_plugins_path()),
                (
                    "claude_known_marketplaces",
                    claude_known_marketplaces_path(),
                ),
                ("claude_config", claude_config_path()),
                ("claude_mcp_json", claude_mcp_json_path()),
                ("codex_config", codex_config_path()),
            ] {
                if let Some(p) = path {
                    assert!(
                        p.starts_with(&home),
                        "{label} ({}) must be under home ({})",
                        p.display(),
                        home.display()
                    );
                }
            }
        }
    }

    #[test]
    fn generated_mcp_path_under_build_mcp() {
        if let Some(p) = generated_mcp_path() {
            let s = p.to_string_lossy();
            assert!(s.contains("build"), "must be under build: {s}");
            assert!(s.contains("mcp"), "must contain mcp: {s}");
            assert!(
                s.ends_with("managed.json"),
                "must end with managed.json: {s}"
            );
        }
    }

    #[test]
    fn personal_catalog_path_directly_under_ccync_home() {
        if let (Some(p), Some(home)) = (personal_catalog_path(), ccync_home()) {
            assert_eq!(
                p.parent(),
                Some(home.as_path()),
                "plugins.json must be directly under ccync_home"
            );
            assert_eq!(p.file_name().and_then(|n| n.to_str()), Some("plugins.json"));
        }
    }

    #[test]
    fn plugins_lock_path_under_build() {
        if let Some(p) = plugins_lock_path() {
            let s = p.to_string_lossy();
            assert!(s.contains("build"), "must be under build: {s}");
            assert!(s.ends_with("lock.json"), "must end with lock.json: {s}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn config_dir_prefers_appdata_env() {
        let _guard = crate::test_support::env_guard();
        let prior = std::env::var_os("APPDATA");
        std::env::set_var("APPDATA", r"C:\injected\AppData\Roaming");
        let result = config_dir();
        match prior {
            Some(v) => std::env::set_var("APPDATA", v),
            None => std::env::remove_var("APPDATA"),
        }
        assert_eq!(result, Some(PathBuf::from(r"C:\injected\AppData\Roaming")));
    }

    #[cfg(windows)]
    #[test]
    fn config_dir_falls_back_to_home_appdata_roaming_without_appdata_env() {
        let _guard = crate::test_support::env_guard();
        let prior = std::env::var_os("APPDATA");
        std::env::remove_var("APPDATA");
        let result = config_dir();
        if let Some(v) = prior {
            std::env::set_var("APPDATA", v);
        }
        if let Some(home) = home_dir() {
            assert_eq!(result, Some(home.join("AppData").join("Roaming")));
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn config_dir_prefers_xdg_config_home_env() {
        let _guard = crate::test_support::env_guard();
        let prior = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", "/injected/config");
        let result = config_dir();
        match prior {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        assert_eq!(result, Some(PathBuf::from("/injected/config")));
    }

    #[cfg(not(windows))]
    #[test]
    fn config_dir_falls_back_to_home_dot_config_without_xdg_env() {
        let _guard = crate::test_support::env_guard();
        let prior = std::env::var_os("XDG_CONFIG_HOME");
        std::env::remove_var("XDG_CONFIG_HOME");
        let result = config_dir();
        if let Some(v) = prior {
            std::env::set_var("XDG_CONFIG_HOME", v);
        }
        if let Some(home) = home_dir() {
            assert_eq!(result, Some(home.join(".config")));
        }
    }

    #[test]
    fn all_paths_descend_from_ccync_home() {
        if let Some(home) = ccync_home() {
            for (label, path) in [
                ("plugins_root", ccync_plugins_root()),
                ("cache_root", ccync_cache_root()),
                ("data_root", ccync_data_root()),
                ("build_root", ccync_build_root()),
                ("machine_config", machine_config_path()),
                ("generated_mcp", generated_mcp_path()),
                ("plugins_lock_path", plugins_lock_path()),
                ("canonical_plugin_root", canonical_plugin_root()),
            ] {
                if let Some(p) = path {
                    assert!(
                        p.starts_with(&home),
                        "{label} ({}) must be under ccync_home ({})",
                        p.display(),
                        home.display()
                    );
                }
            }
        }
    }
}
