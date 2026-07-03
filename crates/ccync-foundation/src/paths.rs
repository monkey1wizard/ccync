//! Path location resolution for CCYNC state directories.
//!
//! All functions return `Option<PathBuf>` — `None` when home is unavailable.
//! No file I/O; callers decide whether to create directories.
//! Cross-platform: `USERPROFILE` on Windows, `HOME` on Unix.

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

/// `~/.ccync/config.json`
pub fn machine_config_path() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("config.json"))
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

// ── build / output paths ──────────────────────────────────────────────────────

/// `~/.ccync/build`
///
/// Root of all derived OUTPUT artifacts. Rebuildable by `ccync sync`.
pub fn ccync_build_root() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("build"))
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
            assert!(
                s.ends_with("lock.json"),
                "must end with lock.json: {s}"
            );
        }
    }

    #[test]
    fn all_paths_descend_from_ccync_home() {
        if let Some(home) = ccync_home() {
            for (label, path) in [
                ("plugins_root", ccync_plugins_root()),
                ("cache_root", ccync_cache_root()),
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
