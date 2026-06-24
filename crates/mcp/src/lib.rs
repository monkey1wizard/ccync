//! MCP (Model Context Protocol) config orchestration crate.
//!
//! Owns variable resolution, safe CCYNC-managed merge, per-host write, and the
//! `HealthCheck` impl for MCP config state.
//!
//! Dependency law: `mcp` → `base`. No CCYNC dep cycles. MCP host config
//! serializers live in the `serializers` submodule.

pub mod serializers;

mod health;
mod merge;
mod projection;
mod resolver;

pub use ccync_foundation::mcp::{McpError, McpManifest, McpServer, Result};
pub use health::*;
pub use merge::*;
pub use projection::*;
pub use resolver::*;

use std::path::Path;

// ccync mcp backend
// ─────────────────────────────────────────────────────────────────────────────

/// Result of a `ccync mcp update` run.
pub struct McpUpdateReport {
    pub hosts_updated: Vec<String>,
    pub servers_written: usize,
    pub warnings: Vec<String>,
}

/// Run the MCP config update for all available providers.
///
/// Ports `Invoke-UpdateMcp` from `Update-Mcp.ps1`. Reads the manifest,
/// resolves `${VAR}` placeholders from the environment, `config.local.env`,
/// and machine config, then writes each host config **non-destructively**
/// (preserving user-owned entries and, for Codex, non-MCP TOML content).
///
/// Advanced cleanup is intentionally not ported (not data-loss/blocking):
/// legacy-alias removal, deprecated-key cleanup, Codex bridge-profile key
/// remapping, and the previous-projection delta guard.
pub fn run_mcp_update(
    manifest_path: &Path,
) -> std::result::Result<McpUpdateReport, McpUpdateError> {
    use crate::serializers::claude_mcp::ClaudeDesktopMcpConfig;
    use crate::serializers::codex::CodexMcpConfig;
    use crate::serializers::copilot::CopilotCliMcpConfig;
    use crate::serializers::opencode::OpenCodeMcpConfig;
    use crate::serializers::McpHostConfig;

    let manifest = load_manifest(manifest_path)?;

    let resolver = McpVariableResolver::new(build_variable_map());
    let resolved = resolver.resolve_manifest(&manifest)?;

    let servers_written = resolved.servers.len();
    let mut hosts_updated = Vec::new();
    let mut warnings = Vec::new();

    // Claude Code (`~/.claude.json` `mcpServers`) — non-destructive overlay.
    // D-11: ccync manages the Claude Code coding agent, NOT the Claude Desktop app
    // (`claude_desktop_config.json` is a separate product). The on-disk `mcpServers`
    // shape is identical, so the serializer is reused; only the target path differs.
    match ClaudeDesktopMcpConfig::from_manifest(&resolved) {
        Ok(cfg) => match serde_json::to_value(&cfg) {
            Ok(val) => {
                if let Some(dest) = ccync_foundation::paths::claude_config_path() {
                    match write_json_provider_merged(&dest, "mcpServers", &val) {
                        Ok(_) => hosts_updated.push("claude".to_string()),
                        Err(e) => warnings.push(format!("claude write error: {e}")),
                    }
                }
            }
            Err(e) => warnings.push(format!("claude serialization error: {e}")),
        },
        Err(e) => warnings.push(format!("claude manifest error: {e}")),
    }

    // Copilot CLI (mcpServers) — non-destructive overlay.
    match CopilotCliMcpConfig::from_manifest(&resolved) {
        Ok(cfg) => match serde_json::to_value(&cfg) {
            Ok(val) => {
                if let Some(dest) = copilot_cli_mcp_path() {
                    match write_json_provider_merged(&dest, "mcpServers", &val) {
                        Ok(_) => hosts_updated.push("copilot".to_string()),
                        Err(e) => warnings.push(format!("copilot write error: {e}")),
                    }
                }
            }
            Err(e) => warnings.push(format!("copilot serialization error: {e}")),
        },
        Err(e) => warnings.push(format!("copilot manifest error: {e}")),
    }

    // OpenCode (mcp) — non-destructive overlay, preserves other top-level keys.
    match OpenCodeMcpConfig::from_manifest(&resolved) {
        Ok(cfg) => match serde_json::to_value(&cfg) {
            Ok(val) => {
                if let Some(dest) = opencode_config_path() {
                    match write_json_provider_merged(&dest, "mcp", &val) {
                        Ok(_) => hosts_updated.push("opencode".to_string()),
                        Err(e) => warnings.push(format!("opencode write error: {e}")),
                    }
                }
            }
            Err(e) => warnings.push(format!("opencode serialization error: {e}")),
        },
        Err(e) => warnings.push(format!("opencode manifest error: {e}")),
    }

    // Codex (TOML) — remove managed sections then append, preserving non-MCP.
    match CodexMcpConfig::from_manifest(&resolved) {
        Ok(cfg) => match cfg.to_config_string() {
            Ok(sections) => {
                if let Some(dest) = codex_config_path() {
                    let managed_names: std::collections::HashSet<String> =
                        resolved.servers.keys().cloned().collect();
                    match write_codex_merged(&dest, &managed_names, &sections) {
                        Ok(()) => hosts_updated.push("codex".to_string()),
                        Err(e) => warnings.push(format!("codex write error: {e}")),
                    }
                }
            }
            Err(e) => warnings.push(format!("codex serialization error: {e}")),
        },
        Err(e) => warnings.push(format!("codex manifest error: {e}")),
    }

    Ok(McpUpdateReport {
        hosts_updated,
        servers_written,
        warnings,
    })
}

pub(crate) fn ensure_parent(path: &Path) -> std::result::Result<(), McpUpdateError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn copilot_cli_mcp_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".copilot").join("mcp-config.json"))
}

fn codex_config_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex").join("config.toml"))
}

fn opencode_config_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|c| c.join("opencode").join("opencode.json"))
}

/// Inner helper so tests can inject a fake home without fighting `dirs` on Windows.
fn live_mcp_target_paths_inner(
    home: Option<std::path::PathBuf>,
    config_dir: Option<std::path::PathBuf>,
) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::with_capacity(4);
    if let Some(ref h) = home {
        paths.push(h.join(".claude.json"));
        paths.push(h.join(".codex").join("config.toml"));
        paths.push(h.join(".copilot").join("mcp-config.json"));
    }
    if let Some(ref c) = config_dir {
        paths.push(c.join("opencode").join("opencode.json"));
    }
    paths
}

/// Returns the live agent-config paths that `run_mcp_update` writes to.
///
/// Order: claude `~/.claude.json`, codex `~/.codex/config.toml`,
/// copilot `~/.copilot/mcp-config.json`, opencode config dir `opencode.json`.
/// Entries are absent when the platform cannot resolve home/config directory.
/// Pure: no file I/O. Used by the first-run overwrite-visibility gate.
pub fn live_mcp_target_paths() -> Vec<std::path::PathBuf> {
    live_mcp_target_paths_inner(dirs::home_dir(), dirs::config_dir())
}

/// Errors from the `ccync mcp` backend that are not covered by `McpError`.
#[derive(Debug, thiserror::Error)]
pub enum McpUpdateError {
    #[error("manifest error: {0}")]
    Manifest(#[from] McpError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests (moved from ccync-engine::mcp)
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ccync_foundation::health::HealthCheck;
    use std::collections::HashMap;
    use std::fs;
    use tempfile::TempDir;

    // ── McpVariableResolver ──────────────────────────────────────────────────

    #[test]
    fn resolver_substitutes_known_var() {
        let mut vars = HashMap::new();
        vars.insert("HOME".to_string(), "/home/user".to_string());
        let r = McpVariableResolver::new(vars);
        assert_eq!(
            r.resolve_string("${HOME}/vault").unwrap(),
            "/home/user/vault"
        );
    }

    #[test]
    fn resolver_substitutes_multiple_vars() {
        let mut vars = HashMap::new();
        vars.insert("HOME".to_string(), "/home/user".to_string());
        vars.insert("PROJECT".to_string(), "myproject".to_string());
        let r = McpVariableResolver::new(vars);
        assert_eq!(
            r.resolve_string("${HOME}/projects/${PROJECT}").unwrap(),
            "/home/user/projects/myproject"
        );
    }

    #[test]
    fn resolver_rejects_unresolved_secret() {
        let r = McpVariableResolver::empty();
        assert!(matches!(
            r.resolve_string("${API_KEY}"),
            Err(McpError::UnresolvedSecret(_))
        ));
    }

    #[test]
    fn resolver_leaves_unresolved_non_secret_as_is() {
        let r = McpVariableResolver::empty();
        assert_eq!(r.resolve_string("${SOME_VAR}").unwrap(), "${SOME_VAR}");
    }

    // ── McpMerger ────────────────────────────────────────────────────────────

    #[test]
    fn merger_replaces_ccync_managed() {
        let merger = McpMerger::new(vec!["github".to_string()]);
        let mut ccync_servers = HashMap::new();
        ccync_servers.insert(
            "github".to_string(),
            McpServer {
                server_type: None,
                url: Some("https://new.url".to_string()),
                command: None,
                args: None,
                env: None,
                headers: None,
            },
        );
        let ccync = McpManifest {
            servers: ccync_servers,
            inputs: None,
        };
        let mut ex_servers = HashMap::new();
        ex_servers.insert(
            "github".to_string(),
            McpServer {
                server_type: None,
                url: Some("https://old.url".to_string()),
                command: None,
                args: None,
                env: None,
                headers: None,
            },
        );
        let existing = McpManifest {
            servers: ex_servers,
            inputs: None,
        };
        let merged = merger.merge(&ccync, Some(&existing));
        assert_eq!(
            merged.servers["github"].url.as_deref(),
            Some("https://new.url")
        );
    }

    #[test]
    fn merger_preserves_user_owned() {
        let merger = McpMerger::new(vec!["github".to_string()]);
        let mut ccync_servers = HashMap::new();
        ccync_servers.insert(
            "github".to_string(),
            McpServer {
                server_type: None,
                url: Some("https://new.url".to_string()),
                command: None,
                args: None,
                env: None,
                headers: None,
            },
        );
        let ccync = McpManifest {
            servers: ccync_servers,
            inputs: None,
        };
        let mut ex_servers = HashMap::new();
        ex_servers.insert(
            "my-tool".to_string(),
            McpServer {
                server_type: None,
                command: Some("my-tool".to_string()),
                args: None,
                env: None,
                url: None,
                headers: None,
            },
        );
        let existing = McpManifest {
            servers: ex_servers,
            inputs: None,
        };
        let merged = merger.merge(&ccync, Some(&existing));
        assert_eq!(merged.servers.len(), 2);
        assert!(merged.servers.contains_key("my-tool"));
    }

    #[test]
    fn merger_idempotent() {
        let merger = McpMerger::new(vec!["github".to_string()]);
        let mut ccync_servers = HashMap::new();
        ccync_servers.insert(
            "github".to_string(),
            McpServer {
                server_type: None,
                url: Some("https://url".to_string()),
                command: None,
                args: None,
                env: None,
                headers: None,
            },
        );
        let ccync = McpManifest {
            servers: ccync_servers,
            inputs: None,
        };
        let m1 = merger.merge(&ccync, None);
        let m2 = merger.merge(&ccync, Some(&m1));
        assert_eq!(m1.servers.len(), m2.servers.len());
    }

    // ── check_plugin_root_complete ───────────────────────────────────────────

    #[test]
    fn plugin_root_missing_commands_fails() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("agents")).unwrap();
        fs::create_dir(tmp.path().join("skills")).unwrap();
        assert!(matches!(
            check_plugin_root_complete(tmp.path()),
            Err(McpError::PluginRootIncomplete(_))
        ));
    }

    #[test]
    fn plugin_root_complete_ok() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("commands")).unwrap();
        fs::create_dir(tmp.path().join("agents")).unwrap();
        fs::create_dir(tmp.path().join("skills")).unwrap();
        assert!(check_plugin_root_complete(tmp.path()).is_ok());
    }

    #[test]
    fn save_projection_fails_on_incomplete_root() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("agents")).unwrap();
        fs::create_dir(tmp.path().join("skills")).unwrap();
        let out = tmp.path().join("out.json");
        let proj = McpProjection {
            schema_version: 1,
            mcp_servers: HashMap::new(),
            inputs: None,
            metadata: McpProjectionMetadata {
                ownership: "ccync-managed".into(),
                generated_at: "2026-06-09T00:00:00Z".into(),
                generated_by: "test".into(),
                secret_bearing: false,
                source_files: HashMap::new(),
                preservation: "test".into(),
            },
        };
        assert!(save_projection(&out, &proj, tmp.path()).is_err());
        assert!(!out.exists());
    }

    #[test]
    fn save_projection_ok_on_complete_root() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("commands")).unwrap();
        fs::create_dir(tmp.path().join("agents")).unwrap();
        fs::create_dir(tmp.path().join("skills")).unwrap();
        let out = tmp.path().join("out.json");
        let proj = McpProjection {
            schema_version: 1,
            mcp_servers: HashMap::new(),
            inputs: None,
            metadata: McpProjectionMetadata {
                ownership: "ccync-managed".into(),
                generated_at: "2026-06-09T00:00:00Z".into(),
                generated_by: "test".into(),
                secret_bearing: false,
                source_files: HashMap::new(),
                preservation: "test".into(),
            },
        };
        assert!(save_projection(&out, &proj, tmp.path()).is_ok());
        assert!(out.exists());
    }

    // ── McpProjectionHealthCheck ─────────────────────────────────────────────

    #[test]
    fn health_check_warns_when_projection_missing() {
        use ccync_foundation::health::Severity;
        let tmp = TempDir::new().unwrap();
        let check = McpProjectionHealthCheck::with_path(tmp.path().join("nonexistent.json"));
        let findings = check.check();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Warning);
    }

    #[test]
    fn health_check_ok_when_valid_json() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("managed.json");
        fs::write(
            &path,
            r#"{"schemaVersion":1,"mcpServers":{},"_metadata":{}}"#,
        )
        .unwrap();
        let check = McpProjectionHealthCheck::with_path(path);
        assert!(check.check().is_empty());
    }

    #[test]
    fn health_check_error_on_invalid_json() {
        use ccync_foundation::health::Severity;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("managed.json");
        fs::write(&path, "not json").unwrap();
        let check = McpProjectionHealthCheck::with_path(path);
        let findings = check.check();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Error);
    }

    // ── run_mcp_update orchestration: variable resolution ─────────────────────

    // ── live_mcp_target_paths ─────────────────────────────────────────────────

    #[test]
    fn live_mcp_target_paths_returns_four_known_files() {
        use std::path::PathBuf;
        // Use inner helper so the test is hermetic: dirs::home_dir() on Windows
        // reads SHGetKnownFolderPath (not USERPROFILE env var), so env override
        // doesn't work cross-platform. The inner fn accepts explicit home/config.
        let fake_home = PathBuf::from("/fakehome");
        let fake_config = PathBuf::from("/fakeconfig");
        let paths = live_mcp_target_paths_inner(Some(fake_home), Some(fake_config));
        let s: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(
            s.iter().any(|p| p.ends_with(".claude.json")),
            "claude path missing: {s:?}"
        );
        assert!(
            s.iter().any(|p| p.contains(".codex") && p.ends_with("config.toml")),
            "codex path missing: {s:?}"
        );
        assert!(
            s.iter().any(|p| p.ends_with("mcp-config.json")),
            "copilot path missing: {s:?}"
        );
        assert!(
            s.iter().any(|p| p.ends_with("opencode.json")),
            "opencode path missing: {s:?}"
        );
        assert_eq!(paths.len(), 4, "expected exactly 4 paths, got: {s:?}");
    }

    #[test]
    fn build_variable_map_resolves_secret_from_env() {
        // Defect-1 regression: an empty resolver hard-failed on secret-named
        // placeholders. The map must pick up a secret from the environment so
        // resolve_manifest succeeds instead of returning UnresolvedSecret.
        let key = "CCYNC_TEST_MCP_API_KEY";
        std::env::set_var(key, "sekret");
        let map = build_variable_map_from(None, None);
        let r = McpVariableResolver::new(map);
        assert_eq!(
            r.resolve_string("${CCYNC_TEST_MCP_API_KEY}").unwrap(),
            "sekret"
        );
        std::env::remove_var(key);
    }

    #[test]
    fn build_variable_map_reads_env_file_as_legacy_fallback() {
        let tmp = TempDir::new().unwrap();
        let env_file = tmp.path().join("config.local.env");
        fs::write(&env_file, "MY_TOKEN=from_file\n").unwrap();

        let map = build_variable_map_from(Some(&env_file), None);
        assert_eq!(map.get("MY_TOKEN").map(String::as_str), Some("from_file"));
    }

    #[test]
    fn build_variable_map_reads_config_json_path_via_registry() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.json");
        fs::write(
            &cfg,
            r#"{"obsidian":{"vault":"/Users/test/Vault"},"research":{"localSearchProject":"C:/Code/Search"}}"#,
        )
        .unwrap();

        let map = build_variable_map_from(None, Some(&cfg));
        assert_eq!(
            map.get("OBSIDIAN_VAULT").map(String::as_str),
            Some("/Users/test/Vault")
        );
        assert_eq!(
            map.get("LOCAL_SEARCH_PROJECT").map(String::as_str),
            Some("C:/Code/Search")
        );
    }

    #[test]
    fn build_variable_map_reads_research_keys() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.json");
        fs::write(
            &cfg,
            r#"{"research":{"localSearchProject":"C:/Code/Search","privateResearchDir":"/vault/private","researchDefaultDest":"private"}}"#,
        )
        .unwrap();

        let map = build_variable_map_from(None, Some(&cfg));
        assert_eq!(
            map.get("LOCAL_SEARCH_PROJECT").map(String::as_str),
            Some("C:/Code/Search")
        );
        assert_eq!(
            map.get("OBSIDIAN_PRIVATE_RESEARCH_DIR").map(String::as_str),
            Some("/vault/private")
        );
    }

    #[test]
    fn build_variable_map_reads_secrets_verbatim() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.json");
        fs::write(&cfg, r#"{"secrets":{"CONTEXT7_API_KEY":"ctx-123"}}"#).unwrap();

        let map = build_variable_map_from(None, Some(&cfg));
        assert_eq!(
            map.get("CONTEXT7_API_KEY").map(String::as_str),
            Some("ctx-123")
        );
    }

    #[test]
    fn build_variable_map_config_json_wins_over_legacy_env() {
        let tmp = TempDir::new().unwrap();
        let env_file = tmp.path().join("config.local.env");
        fs::write(&env_file, "OBSIDIAN_VAULT=/legacy/vault\n").unwrap();
        let cfg = tmp.path().join("config.json");
        fs::write(&cfg, r#"{"obsidian":{"vault":"/new/vault"}}"#).unwrap();

        let map = build_variable_map_from(Some(&env_file), Some(&cfg));
        assert_eq!(
            map.get("OBSIDIAN_VAULT").map(String::as_str),
            Some("/new/vault")
        );
    }

    #[test]
    fn build_variable_map_legacy_env_supplies_missing_keys() {
        let tmp = TempDir::new().unwrap();
        let env_file = tmp.path().join("config.local.env");
        fs::write(&env_file, "CCYNC_SKILLS=/some/skills\n").unwrap();
        // config.json has no CCYNC_SKILLS entry
        let cfg = tmp.path().join("config.json");
        fs::write(&cfg, r#"{"obsidian":{"vault":"/vault"}}"#).unwrap();

        let map = build_variable_map_from(Some(&env_file), Some(&cfg));
        assert_eq!(
            map.get("CCYNC_SKILLS").map(String::as_str),
            Some("/some/skills")
        );
    }

    // ── write_json_provider_merged (non-destructive) ──────────────────────────

    #[test]
    fn json_merge_preserves_user_entries_and_other_keys() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("claude_desktop_config.json");
        // Pre-existing config: a user MCP server + an unrelated top-level key.
        fs::write(
            &dest,
            r#"{"mcpServers":{"user-server":{"command":"mine"}},"theme":"dark"}"#,
        )
        .unwrap();

        let generated = serde_json::json!({
            "mcpServers": { "ccync-managed": { "command": "npx", "args": [] } }
        });
        let n = write_json_provider_merged(&dest, "mcpServers", &generated).unwrap();
        assert_eq!(n, 1);

        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&dest).unwrap()).unwrap();
        // User server preserved.
        assert_eq!(written["mcpServers"]["user-server"]["command"], "mine");
        // Managed server added.
        assert_eq!(written["mcpServers"]["ccync-managed"]["command"], "npx");
        // Unrelated top-level key preserved.
        assert_eq!(written["theme"], "dark");
    }

    #[test]
    fn json_merge_creates_file_when_absent() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("sub").join("mcp-config.json");
        let generated = serde_json::json!({ "mcp": { "s": { "type": "local" } } });
        let n = write_json_provider_merged(&dest, "mcp", &generated).unwrap();
        assert_eq!(n, 1);
        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&dest).unwrap()).unwrap();
        assert_eq!(written["mcp"]["s"]["type"], "local");
    }

    #[test]
    fn json_merge_refuses_to_overwrite_non_object() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("bad.json");
        fs::write(&dest, "[1,2,3]").unwrap();
        let generated = serde_json::json!({ "mcpServers": { "x": {} } });
        let err = write_json_provider_merged(&dest, "mcpServers", &generated);
        assert!(err.is_err(), "must not clobber a non-object config");
        // Original content untouched.
        assert_eq!(fs::read_to_string(&dest).unwrap(), "[1,2,3]");
    }

    // ── Codex TOML section parsing + non-destructive merge ────────────────────

    #[test]
    fn codex_table_server_name_parses_bare_and_quoted() {
        assert_eq!(
            codex_table_server_name("[mcp_servers.memory]").as_deref(),
            Some("memory")
        );
        assert_eq!(
            codex_table_server_name("[mcp_servers.memory.env]").as_deref(),
            Some("memory")
        );
        assert_eq!(
            codex_table_server_name(r#"[mcp_servers."upstash/context7"]"#).as_deref(),
            Some("upstash/context7")
        );
        assert_eq!(codex_table_server_name("[model]"), None);
        assert_eq!(codex_table_server_name("command = \"npx\""), None);
    }

    #[test]
    fn codex_remove_preserves_non_mcp_and_user_servers() {
        let raw = "[model]\nname = \"gpt\"\n\n[mcp_servers.ccync_managed]\ncommand = \"npx\"\n\n[mcp_servers.user_kept]\ncommand = \"mine\"\n";
        let managed: std::collections::HashSet<String> =
            ["ccync_managed".to_string()].into_iter().collect();
        let out = remove_codex_managed_sections(raw, &managed);
        assert!(out.contains("[model]"), "non-MCP [model] must survive");
        assert!(out.contains("name = \"gpt\""));
        assert!(
            out.contains("[mcp_servers.user_kept]"),
            "user MCP server must survive"
        );
        assert!(
            !out.contains("ccync_managed"),
            "managed section must be removed"
        );
    }

    #[test]
    fn codex_write_merge_appends_and_preserves() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("config.toml");
        fs::write(&dest, "[model]\nname = \"gpt\"\n").unwrap();
        let managed: std::collections::HashSet<String> =
            ["memory".to_string()].into_iter().collect();
        let sections = "[mcp_servers.memory]\r\ncommand = \"npx\"\r\n";
        write_codex_merged(&dest, &managed, sections).unwrap();
        let written = fs::read_to_string(&dest).unwrap();
        assert!(written.contains("[model]"), "non-MCP content preserved");
        assert!(
            written.contains("[mcp_servers.memory]"),
            "managed section appended"
        );
    }

    #[test]
    fn run_mcp_update_does_not_treat_agy_as_an_mcp_provider() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let appdata = tmp.path().join("appdata");
        let xdg = tmp.path().join("xdg-config");
        let ccync_home = home.join(".ccync");
        let manifest_path = ccync_home
            .join("generated")
            .join("mcp")
            .join("managed.json");

        fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::create_dir_all(home.join(".copilot")).unwrap();
        fs::create_dir_all(appdata.join("Claude")).unwrap();
        fs::create_dir_all(xdg.join("opencode")).unwrap();
        fs::create_dir_all(ccync_home.join("config")).unwrap();

        fs::write(
            &manifest_path,
            r#"{
  "servers": {
    "memory": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "@mcp/server-memory"]
    }
  }
}"#,
        )
        .unwrap();

        // Pre-populate ~/.claude.json with an unrelated user server + non-MCP key,
        // to prove the unified projection is non-destructive (conflict-safe).
        fs::write(
            home.join(".claude.json"),
            r#"{"mcpServers":{"user-server":{"command":"mine","args":[]}},"theme":"dark"}"#,
        )
        .unwrap();

        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", &home);
            std::env::set_var("HOME", &home);
            std::env::set_var("APPDATA", &appdata);
            std::env::remove_var("XDG_CONFIG_HOME");
        }

        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("XDG_CONFIG_HOME", &xdg);
            std::env::remove_var("APPDATA");
        }

        let report = run_mcp_update(&manifest_path).unwrap();

        assert!(report.hosts_updated.contains(&"codex".to_string()));
        assert!(!report.hosts_updated.contains(&"agy".to_string()));
        assert!(report.warnings.iter().all(|w| !w.contains("agy")));

        // The unified MCP projection targets Claude = Claude Code (`~/.claude.json`),
        // NOT Claude Desktop, and never gemini-cli.
        // (1) Claude Code target written, server present.
        assert!(report.hosts_updated.contains(&"claude".to_string()));
        let claude_json = home.join(".claude.json");
        assert!(
            claude_json.exists(),
            "Claude Code ~/.claude.json must be written"
        );
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&claude_json).unwrap()).unwrap();
        assert!(
            v["mcpServers"]["memory"].is_object(),
            "server projected into ~/.claude.json mcpServers"
        );
        // Non-destructive: pre-existing unrelated server + non-MCP key preserved.
        assert!(
            v["mcpServers"]["user-server"].is_object(),
            "unrelated user MCP server must be preserved"
        );
        assert_eq!(
            v["theme"], "dark",
            "non-MCP top-level key must be preserved"
        );

        // (2) Claude Desktop config must NOT be written (D-11).
        assert!(
            !appdata
                .join("Claude")
                .join("claude_desktop_config.json")
                .exists(),
            "Claude Desktop config must not be written"
        );

        // (3) gemini-cli is not an MCP target.
        assert!(!report.hosts_updated.contains(&"gemini".to_string()));
        assert!(!report.hosts_updated.contains(&"gemini-cli".to_string()));
    }
}
