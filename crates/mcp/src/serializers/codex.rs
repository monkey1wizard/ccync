//! Codex CLI MCP configuration serializer (TOML format).
//!
//! Converts the portable `.mcp.json` manifest to Codex's `[mcp_servers.name]`
//! TOML sections written into `~/.codex/config.toml`.
//!
//! Output format (one section per server):
//! ```toml
//! [mcp_servers."server-name"]
//! command = "npx"
//! args = ["-y", "@mcp/server"]
//!
//! [mcp_servers."server-name".env]
//! KEY = "value"
//! ```
//!
//! HTTP servers (headers go in Codex's `http_headers` map of static values):
//! ```toml
//! [mcp_servers."server-name"]
//! type = "http"
//! url = "https://..."
//!
//! [mcp_servers."server-name".http_headers]
//! KEY = "value"
//! ```
//!
//! Ports `ConvertTo-CodexMcpConfig` + `ConvertTo-CodexMcpSection` from
//! `scripts/Update-Mcp.ps1`.

use super::McpHostConfig;
use ccync_foundation::mcp::{McpManifest, McpServer, Result};
use std::collections::BTreeMap;

/// A single Codex MCP server entry (for serialization purposes).
#[derive(Debug, Clone)]
pub struct CodexServerEntry {
    pub server_type: Option<String>, // "http" or None (stdio)
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
    pub url: Option<String>,
    /// HTTP servers only: rendered as the `http_headers` sub-table.
    pub http_headers: BTreeMap<String, String>,
}

/// Codex CLI MCP configuration — collection of server entries.
#[derive(Debug, Clone)]
pub struct CodexMcpConfig {
    /// Ordered (BTreeMap) so output is deterministic across test runs.
    pub servers: BTreeMap<String, CodexServerEntry>,
}

impl McpHostConfig for CodexMcpConfig {
    fn from_manifest(manifest: &McpManifest) -> Result<Self> {
        let mut servers = BTreeMap::new();
        for (name, config) in &manifest.servers {
            servers.insert(name.clone(), convert_to_codex_entry(config));
        }
        Ok(Self { servers })
    }

    /// Serialize to TOML section text.
    ///
    /// Each server becomes a `[mcp_servers."name"]` block followed by its
    /// key-value fields, and a nested `[mcp_servers."name".env]` block when
    /// env vars are present. Sections are separated by a blank line.
    fn to_config_string(&self) -> Result<String> {
        let mut sections: Vec<String> = Vec::new();
        for (name, entry) in &self.servers {
            sections.push(format_codex_section(name, entry));
        }
        Ok(sections.join("\r\n\r\n") + "\r\n")
    }
}

fn convert_to_codex_entry(server: &McpServer) -> CodexServerEntry {
    let is_http = server.server_type.as_deref() == Some("http")
        || (server.command.is_none() && server.url.is_some());

    if is_http {
        CodexServerEntry {
            server_type: Some("http".to_string()),
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            url: server.url.clone(),
            http_headers: server
                .headers
                .as_ref()
                .map(|h| h.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default(),
        }
    } else {
        let mut env = BTreeMap::new();
        if let Some(ref e) = server.env {
            for (k, v) in e {
                env.insert(k.clone(), v.clone());
            }
        }
        CodexServerEntry {
            server_type: None,
            command: server.command.clone(),
            args: server.args.clone().unwrap_or_default(),
            env,
            cwd: server.cwd.clone(),
            url: None,
            http_headers: BTreeMap::new(),
        }
    }
}

/// Format one server as TOML section(s).
///
/// Ports `ConvertTo-CodexMcpSection` → `ConvertTo-TomlTableSections`.
fn format_codex_section(name: &str, entry: &CodexServerEntry) -> String {
    let key = format_toml_key_segment(name);
    let header = format!("[mcp_servers.{}]", key);
    let mut lines = vec![header];

    if let Some(ref t) = entry.server_type {
        lines.push(format!("type = {}", toml_string(t)));
    }
    if let Some(ref url) = entry.url {
        lines.push(format!("url = {}", toml_string(url)));
    }
    if let Some(ref cmd) = entry.command {
        lines.push(format!("command = {}", toml_string(cmd)));
    }
    if !entry.args.is_empty() {
        let args_toml = entry
            .args
            .iter()
            .map(|a| toml_string(a))
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("args = [{}]", args_toml));
    }
    if let Some(ref cwd) = entry.cwd {
        lines.push(format!("cwd = {}", toml_string(cwd)));
    }

    let mut blocks = vec![lines.join("\r\n")];

    if !entry.env.is_empty() {
        let mut env_lines = vec![format!("[mcp_servers.{}.env]", key)];
        for (k, v) in &entry.env {
            env_lines.push(format!("{} = {}", k, toml_string(v)));
        }
        blocks.push(env_lines.join("\r\n"));
    }

    if !entry.http_headers.is_empty() {
        let mut header_lines = vec![format!("[mcp_servers.{}.http_headers]", key)];
        for (k, v) in &entry.http_headers {
            header_lines.push(format!(
                "{} = {}",
                format_toml_key_segment(k),
                toml_string(v)
            ));
        }
        blocks.push(header_lines.join("\r\n"));
    }

    blocks.join("\r\n\r\n")
}

/// Escape and quote a TOML string value: `"text"`.
fn toml_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{}\"", escaped)
}

/// Format a TOML key segment — bare if alphanumeric/hyphen/underscore, else quoted.
fn format_toml_key_segment(key: &str) -> String {
    if key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        key.to_string()
    } else {
        toml_string(key)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_manifest(servers: HashMap<String, McpServer>) -> McpManifest {
        McpManifest {
            provenance: Default::default(),
            servers,
            inputs: None,
        }
    }

    #[test]
    fn stdio_server_produces_command_and_args() {
        let mut servers = HashMap::new();
        servers.insert(
            "memory".to_string(),
            McpServer {
                server_type: Some("stdio".to_string()),
                command: Some("npx".to_string()),
                args: Some(vec!["-y".to_string(), "@mcp/server-memory".to_string()]),
                env: None,
                cwd: None,
                url: None,
                headers: None,
            },
        );
        let cfg = CodexMcpConfig::from_manifest(&make_manifest(servers)).unwrap();
        let toml = cfg.to_config_string().unwrap();
        assert!(
            toml.contains("[mcp_servers.memory]"),
            "section header missing"
        );
        assert!(toml.contains("command = \"npx\""), "command missing");
        assert!(
            toml.contains("args = [\"-y\", \"@mcp/server-memory\"]"),
            "args missing"
        );
    }

    #[test]
    fn http_server_produces_type_and_url() {
        let mut servers = HashMap::new();
        servers.insert(
            "remote".to_string(),
            McpServer {
                server_type: Some("http".to_string()),
                command: None,
                args: None,
                env: None,
                cwd: None,
                url: Some("https://api.example.com/mcp/".to_string()),
                headers: None,
            },
        );
        let cfg = CodexMcpConfig::from_manifest(&make_manifest(servers)).unwrap();
        let toml = cfg.to_config_string().unwrap();
        assert!(
            toml.contains("[mcp_servers.remote]"),
            "section header missing"
        );
        assert!(toml.contains("type = \"http\""), "type missing");
        assert!(
            toml.contains("url = \"https://api.example.com/mcp/\""),
            "url missing"
        );
    }

    #[test]
    fn http_server_headers_produce_http_headers_table() {
        let headers: HashMap<String, String> = [("CONTEXT7_API_KEY", "k-1"), ("X.Region", "us")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut servers = HashMap::new();
        servers.insert(
            "remote".to_string(),
            McpServer {
                server_type: Some("http".to_string()),
                command: None,
                args: None,
                env: None,
                cwd: None,
                url: Some("https://api.example.com/mcp/".to_string()),
                headers: Some(headers),
            },
        );
        let cfg = CodexMcpConfig::from_manifest(&make_manifest(servers)).unwrap();
        let toml = cfg.to_config_string().unwrap();
        assert!(
            toml.contains("[mcp_servers.remote.http_headers]"),
            "http_headers table missing: {toml}"
        );

        let parsed: toml::Value = toml::from_str(&toml).unwrap();
        let table = &parsed["mcp_servers"]["remote"]["http_headers"];
        assert_eq!(table["CONTEXT7_API_KEY"].as_str(), Some("k-1"));
        assert_eq!(table["X.Region"].as_str(), Some("us"));

        // The managed-section remover must strip the sub-table with its server.
        let managed: std::collections::HashSet<String> =
            ["remote".to_string()].into_iter().collect();
        let stripped = crate::merge::remove_codex_managed_sections(&toml, &managed);
        assert!(
            !stripped.contains("http_headers") && !stripped.contains("k-1"),
            "remover left header content behind: {stripped}"
        );
    }

    #[test]
    fn env_vars_produce_nested_env_section() {
        let mut env = HashMap::new();
        env.insert(
            "MEMORY_FILE_PATH".to_string(),
            "/home/user/mem.json".to_string(),
        );
        let mut servers = HashMap::new();
        servers.insert(
            "memory".to_string(),
            McpServer {
                server_type: Some("stdio".to_string()),
                command: Some("npx".to_string()),
                args: Some(vec!["-y".to_string(), "@mcp/server-memory".to_string()]),
                env: Some(env),
                cwd: None,
                url: None,
                headers: None,
            },
        );
        let cfg = CodexMcpConfig::from_manifest(&make_manifest(servers)).unwrap();
        let toml = cfg.to_config_string().unwrap();
        assert!(
            toml.contains("[mcp_servers.memory.env]"),
            "env section header missing"
        );
        assert!(
            toml.contains("MEMORY_FILE_PATH = \"/home/user/mem.json\""),
            "env key missing"
        );
    }

    #[test]
    fn server_name_with_special_chars_is_quoted() {
        let mut servers = HashMap::new();
        servers.insert(
            "upstash/context7".to_string(),
            McpServer {
                server_type: Some("stdio".to_string()),
                command: Some("npx".to_string()),
                args: Some(vec!["-y".to_string(), "@upstash/context7".to_string()]),
                env: None,
                cwd: None,
                url: None,
                headers: None,
            },
        );
        let cfg = CodexMcpConfig::from_manifest(&make_manifest(servers)).unwrap();
        let toml = cfg.to_config_string().unwrap();
        // Name with '/' must be quoted in TOML key
        assert!(
            toml.contains("[mcp_servers.\"upstash/context7\"]"),
            "quoted key missing: {toml}"
        );
    }

    #[test]
    fn empty_manifest_produces_single_newline() {
        let cfg = CodexMcpConfig::from_manifest(&McpManifest {
            provenance: Default::default(),
            servers: HashMap::new(),
            inputs: None,
        })
        .unwrap();
        let toml = cfg.to_config_string().unwrap();
        assert_eq!(toml, "\r\n");
    }

    #[test]
    fn url_only_server_treated_as_http() {
        let mut servers = HashMap::new();
        servers.insert(
            "remote".to_string(),
            McpServer {
                server_type: None, // no explicit type
                command: None,
                args: None,
                env: None,
                cwd: None,
                url: Some("https://example.com/mcp/".to_string()),
                headers: None,
            },
        );
        let cfg = CodexMcpConfig::from_manifest(&make_manifest(servers)).unwrap();
        let entry = cfg.servers.get("remote").unwrap();
        assert_eq!(entry.server_type.as_deref(), Some("http"));
    }

    #[test]
    fn codex_stdio_cwd_line() {
        let mut servers = HashMap::new();
        servers.insert(
            "with-cwd".to_string(),
            McpServer {
                server_type: Some("stdio".to_string()),
                command: Some("npx".to_string()),
                args: Some(vec!["-y".to_string(), "tool".to_string()]),
                env: None,
                cwd: Some("/path/to/workdir".to_string()),
                url: None,
                headers: None,
            },
        );
        servers.insert(
            "without-cwd".to_string(),
            McpServer {
                server_type: Some("stdio".to_string()),
                command: Some("npx".to_string()),
                args: Some(vec!["-y".to_string(), "tool".to_string()]),
                env: None,
                cwd: None,
                url: None,
                headers: None,
            },
        );

        let cfg = CodexMcpConfig::from_manifest(&make_manifest(servers)).unwrap();
        let toml = cfg.to_config_string().unwrap();

        // Section with cwd has cwd = "<value>" after args
        assert!(toml.contains("cwd = \"/path/to/workdir\""));
        let with_cwd_section = toml
            .split("\r\n\r\n")
            .find(|s| s.contains("[mcp_servers.with-cwd]"))
            .expect("with-cwd section present");
        let args_pos = with_cwd_section.find("args = [\"-y\", \"tool\"]").unwrap();
        let cwd_pos = with_cwd_section.find("cwd = \"/path/to/workdir\"").unwrap();
        assert!(cwd_pos > args_pos, "cwd line must come after args");

        // Section without cwd omits the cwd line
        let without_cwd_section = toml
            .split("\r\n\r\n")
            .find(|s| s.contains("[mcp_servers.without-cwd]"))
            .expect("without-cwd section present");
        assert!(
            !without_cwd_section.lines().any(|l| l.starts_with("cwd =")),
            "section without cwd must not contain cwd line: {without_cwd_section}"
        );
    }
}
