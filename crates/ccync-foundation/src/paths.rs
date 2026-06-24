//! Path location resolution for CCYNC state directories.
//!
//! All functions return `Option<PathBuf>` — `None` when home is unavailable.
//! No file I/O; callers decide whether to create directories.
//! Cross-platform: `USERPROFILE` on Windows, `HOME` on Unix (matches
//! `Get-CcyncUserHome` / `get_ccync_user_home` in `Common.{ps1,sh}`).

use std::path::PathBuf;

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

// ── config paths ─────────────────────────────────────────────────────────────

/// `~/.ccync/config/config.json`
pub fn machine_config_path() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("config").join("config.json"))
}

/// `~/.ccync/config/executor-routing.json`
///
/// Ports the canonical path in `Read-ExecutorRouting` from Common.ps1.
pub fn executor_routing_path() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("config").join("executor-routing.json"))
}

/// `~/.ccync/config/xmachine.json`
pub fn xmachine_config_path() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("config").join("xmachine.json"))
}

// ── state paths ───────────────────────────────────────────────────────────────

/// `~/.ccync/install-state.json`
///
/// Ports `InstallStateFile` from `New-SetupContext` in Common.ps1.
pub fn install_state_path() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("install-state.json"))
}

// ── plugin paths ──────────────────────────────────────────────────────────────

/// `~/.ccync/plugins`
///
/// Ports `Get-CcyncPluginsRoot` from Common.ps1.
pub fn ccync_plugins_root() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("plugins"))
}

/// `~/.ccync/plugins/<plugin_id>`
///
/// Ports `Get-CcyncPluginRoot` from Common.ps1.
pub fn ccync_plugin_root(plugin_id: &str) -> Option<PathBuf> {
    ccync_plugins_root().map(|r| r.join(plugin_id))
}

/// Canonical plugin root for the CCYNC-managed plugin tree (`~/.ccync/plugins/ccync`).
///
/// Shared by install/sync so they resolve the same root the projection layer uses.
pub fn canonical_plugin_root() -> Option<PathBuf> {
    ccync_plugin_root("ccync")
}

// ── data / cache paths ────────────────────────────────────────────────────────

/// `~/.ccync/data`
///
/// Ports `Get-CcyncDataRoot` from Common.ps1.
pub fn ccync_data_root() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("data"))
}

/// `~/.ccync/data/<plugin_id>`
///
/// Ports `Get-CcyncPluginDataRoot` from Common.ps1.
pub fn ccync_plugin_data_root(plugin_id: &str) -> Option<PathBuf> {
    ccync_data_root().map(|r| r.join(plugin_id))
}

/// `~/.ccync/cache`
///
/// Ports `Get-CcyncCacheRoot` from Common.ps1.
pub fn ccync_cache_root() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("cache"))
}

// ── provider / active paths ───────────────────────────────────────────────────

/// `~/.ccync/active/<provider>`
///
/// Ports `Get-CcyncActiveProviderTarget` from Common.ps1.
pub fn ccync_active_provider_path(provider: &str) -> Option<PathBuf> {
    ccync_home().map(|h| h.join("active").join(provider))
}

// ── local / personal paths ────────────────────────────────────────────────

/// `~/.ccync/local`
///
/// Root of the machine-local personal content home. CCYNC reads this directory
/// as a render source and never writes or deletes content here.
pub fn ccync_local_root() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("local"))
}

/// `~/.ccync/local/skills`
///
/// Personal skill directory scanned by canonical-root render when the
/// personal layer is enabled (`personalLayer.enabled` in config.json).
pub fn ccync_local_skills_root() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("local").join("skills"))
}

/// `~/.ccync/local/mcp.json`
///
/// Personal MCP server manifest merged into the canonical `.mcp.json`
/// during render (core-wins collision policy).
pub fn ccync_local_mcp_path() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("local").join("mcp.json"))
}

/// `~/.ccync/local/catalog.json`
///
/// Machine-local personal plugin catalog (catalog-v1 entry schema). Absent
/// when no personal plugins have been installed; resolver treats absence as
/// an empty additive set (default-off byte-identical).
pub fn local_catalog_path() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("local").join("catalog.json"))
}

/// `~/.ccync/local/cache/`
///
/// Clone cache for personal plugins fetched by `ccync plugin add`. Each entry
/// lives at `<id>@<sha>/` and preserves the full CC-plugin directory structure.
/// CCYNC writes here; user content (`local/skills`, `local/mcp.json`,
/// `local/catalog.json`) remains read-only.
pub fn local_plugin_cache_dir() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("local").join("cache"))
}

// ── state paths (explicit) ────────────────────────────────────────────────────

/// `~/.ccync/state`
pub fn ccync_state_root() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("state"))
}

/// `~/.ccync/state/plugins.lock.json`
///
/// The canonical catalog lockfile written by `ccync resolve-catalog` and consumed
/// read-only by the install/projection layers.
pub fn plugins_lock_path() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("state").join("plugins.lock.json"))
}

/// `~/.ccync/state/ccync-self.json`
///
/// Registry for CCYNC's own managed artifacts (empty-shell foundation; not yet
/// read or written by any production path).
pub fn ccync_self_registry_path() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("state").join("ccync-self.json"))
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

// ── generated paths ───────────────────────────────────────────────────────────

/// `~/.ccync/generated/mcp/managed.json`
///
/// Ports `CcyncGeneratedMcpFile` from `New-SetupContext` in Common.ps1.
pub fn generated_mcp_path() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("generated").join("mcp").join("managed.json"))
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
        if let Some(p) = machine_config_path() {
            let s = p.to_string_lossy();
            assert!(s.contains(".ccync"), "must be under .ccync: {s}");
            assert!(s.ends_with("config.json"), "must end with config.json: {s}");
        }
    }

    #[test]
    fn executor_routing_path_ends_with_executor_routing_json() {
        if let Some(p) = executor_routing_path() {
            let s = p.to_string_lossy();
            assert!(
                s.ends_with("executor-routing.json"),
                "must end with executor-routing.json: {s}"
            );
        }
    }

    #[test]
    fn xmachine_config_path_under_ccync_home_config() {
        if let Some(p) = xmachine_config_path() {
            let s = p.to_string_lossy();
            assert!(s.contains(".ccync"), "must be under .ccync: {s}");
            assert!(
                s.ends_with("xmachine.json"),
                "must end with xmachine.json: {s}"
            );
        }
    }

    #[test]
    fn install_state_path_ends_with_install_state_json() {
        if let Some(p) = install_state_path() {
            let s = p.to_string_lossy();
            assert!(
                s.ends_with("install-state.json"),
                "must end with install-state.json: {s}"
            );
        }
    }

    #[test]
    fn ccync_plugins_root_leaf_is_plugins() {
        if let Some(p) = ccync_plugins_root() {
            assert_eq!(p.file_name().and_then(|n| n.to_str()), Some("plugins"));
        }
    }

    #[test]
    fn ccync_plugin_root_appends_plugin_id() {
        if let Some(p) = ccync_plugin_root("ccync-core") {
            let s = p.to_string_lossy();
            assert!(
                s.ends_with("ccync-core") || s.ends_with("ccync-core\\"),
                "must end with plugin id: {s}"
            );
        }
    }

    #[test]
    fn ccync_data_root_leaf_is_data() {
        if let Some(p) = ccync_data_root() {
            assert_eq!(p.file_name().and_then(|n| n.to_str()), Some("data"));
        }
    }

    #[test]
    fn ccync_plugin_data_root_contains_data_and_plugin_id() {
        if let Some(p) = ccync_plugin_data_root("ccync-core") {
            let s = p.to_string_lossy();
            assert!(s.contains("data"), "must contain 'data': {s}");
            assert!(
                s.ends_with("ccync-core") || s.ends_with("ccync-core\\"),
                "must end with plugin id: {s}"
            );
        }
    }

    #[test]
    fn ccync_cache_root_leaf_is_cache() {
        if let Some(p) = ccync_cache_root() {
            assert_eq!(p.file_name().and_then(|n| n.to_str()), Some("cache"));
        }
    }

    #[test]
    fn ccync_active_provider_path_appends_provider() {
        if let Some(p) = ccync_active_provider_path("claude") {
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
    fn generated_mcp_path_ends_with_managed_json() {
        if let Some(p) = generated_mcp_path() {
            let s = p.to_string_lossy();
            assert!(s.contains("generated"), "must contain 'generated': {s}");
            assert!(
                s.ends_with("managed.json"),
                "must end with managed.json: {s}"
            );
        }
    }

    #[test]
    fn ccync_local_root_leaf_is_local() {
        if let Some(p) = ccync_local_root() {
            assert_eq!(p.file_name().and_then(|n| n.to_str()), Some("local"));
        }
    }

    #[test]
    fn ccync_local_skills_root_ends_with_skills_under_local() {
        if let Some(p) = ccync_local_skills_root() {
            let s = p.to_string_lossy();
            assert!(s.contains("local"), "must contain 'local': {s}");
            assert!(
                s.ends_with("skills") || s.ends_with("skills\\"),
                "must end with 'skills': {s}"
            );
        }
    }

    #[test]
    fn ccync_local_mcp_path_ends_with_mcp_json_under_local() {
        if let Some(p) = ccync_local_mcp_path() {
            let s = p.to_string_lossy();
            assert!(s.contains("local"), "must be under local: {s}");
            assert!(s.ends_with("mcp.json"), "must end with mcp.json: {s}");
        }
    }

    #[test]
    fn local_catalog_path_ends_with_catalog_json_under_local() {
        if let Some(p) = local_catalog_path() {
            let s = p.to_string_lossy();
            assert!(s.contains("local"), "must be under local: {s}");
            assert!(
                s.ends_with("catalog.json"),
                "must end with catalog.json: {s}"
            );
        }
    }

    #[test]
    fn local_plugin_cache_dir_ends_with_cache_under_local() {
        if let Some(p) = local_plugin_cache_dir() {
            let s = p.to_string_lossy();
            assert!(s.contains("local"), "must be under local: {s}");
            assert!(
                s.ends_with("cache") || s.ends_with("cache\\"),
                "must end with 'cache': {s}"
            );
        }
    }

    #[test]
    fn ccync_state_root_leaf_is_state() {
        if let Some(p) = ccync_state_root() {
            assert_eq!(p.file_name().and_then(|n| n.to_str()), Some("state"));
        }
    }

    #[test]
    fn plugins_lock_path_ends_with_plugins_lock_json() {
        if let Some(p) = plugins_lock_path() {
            let s = p.to_string_lossy();
            assert!(s.contains("state"), "must be under state: {s}");
            assert!(
                s.ends_with("plugins.lock.json"),
                "must end with plugins.lock.json: {s}"
            );
        }
    }

    #[test]
    fn ccync_self_registry_path_ends_with_ccync_self_json_under_state() {
        if let Some(p) = ccync_self_registry_path() {
            let s = p.to_string_lossy();
            assert!(s.contains("state"), "must be under state: {s}");
            assert!(
                s.ends_with("ccync-self.json"),
                "must end with ccync-self.json: {s}"
            );
        }
    }

    #[test]
    fn all_paths_descend_from_ccync_home() {
        if let Some(home) = ccync_home() {
            for (label, path) in [
                ("plugins_root", ccync_plugins_root()),
                ("data_root", ccync_data_root()),
                ("cache_root", ccync_cache_root()),
                ("install_state", install_state_path()),
                ("executor_routing", executor_routing_path()),
                ("machine_config", machine_config_path()),
                ("generated_mcp", generated_mcp_path()),
                ("xmachine_config", xmachine_config_path()),
                ("local_root", ccync_local_root()),
                ("local_skills_root", ccync_local_skills_root()),
                ("local_mcp_path", ccync_local_mcp_path()),
                ("ccync_state_root", ccync_state_root()),
                ("plugins_lock_path", plugins_lock_path()),
                ("local_catalog_path", local_catalog_path()),
                ("local_plugin_cache_dir", local_plugin_cache_dir()),
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
