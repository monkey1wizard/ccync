//! Claude Code MCP config serializer.
//!
//! Converts the portable `.mcp.json` manifest to the `mcpServers` entries of
//! Claude Code's `~/.claude.json`. Stdio servers render as `command` + `args`;
//! on Windows, env vars are folded into a PowerShell wrapper. Remote servers
//! render as `type` (`http` or `sse`) + `url` + optional `headers`.

use super::McpHostConfig;
use ccync_foundation::mcp::{McpManifest, McpServer, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// Claude server entry.
///
/// `Stdio` keeps the exact `command` + `args` shape earlier releases wrote, so
/// already-owned stdio entries keep their fingerprints. `Remote` headers are an
/// ordered map so the written config lists keys in a stable order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ClaudeDesktopServerEntry {
    Stdio {
        command: String,
        args: Vec<String>,
    },
    Remote {
        #[serde(rename = "type")]
        transport: String,
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        headers: Option<BTreeMap<String, String>>,
    },
}

/// Claude Desktop MCP configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeDesktopMcpConfig {
    pub mcp_servers: HashMap<String, ClaudeDesktopServerEntry>,
}

impl McpHostConfig for ClaudeDesktopMcpConfig {
    fn from_manifest(manifest: &McpManifest) -> Result<Self> {
        let mut mcp_servers = HashMap::new();

        for (server_name, server_config) in &manifest.servers {
            // Servers with neither a command nor a url have nothing to render
            if let Some(entry) = convert_to_claude_entry(server_config) {
                mcp_servers.insert(server_name.clone(), entry);
            }
        }

        Ok(Self { mcp_servers })
    }

    fn to_config_string(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}

/// Convert MCP server config to a Claude entry.
///
/// A server is remote when its type is `http` or `sse`, or when it has no
/// command. Remote servers need a `url`; stdio servers need a `command`.
/// Returns `None` when the required field is missing. On Windows, stdio env
/// vars are wrapped into a PowerShell wrapper.
fn convert_to_claude_entry(server_config: &McpServer) -> Option<ClaudeDesktopServerEntry> {
    let server_type = server_config.server_type.as_deref();
    let is_remote =
        matches!(server_type, Some("http") | Some("sse")) || server_config.command.is_none();

    if is_remote {
        let url = server_config.url.clone()?;
        let transport = if server_type == Some("sse") {
            "sse"
        } else {
            "http"
        };
        let headers = server_config
            .headers
            .as_ref()
            .filter(|h| !h.is_empty())
            .map(|h| h.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
        return Some(ClaudeDesktopServerEntry::Remote {
            transport: transport.to_string(),
            url,
            headers,
        });
    }

    let command = server_config.command.clone()?;
    let args = server_config.args.clone().unwrap_or_default();

    // Check if we need PowerShell wrapper (Windows only, and env vars present)
    if cfg!(target_os = "windows") {
        if let Some(ref env) = server_config.env {
            if !env.is_empty() {
                return Some(wrap_with_powershell(&command, &args, env));
            }
        }
    }

    Some(ClaudeDesktopServerEntry::Stdio { command, args })
}

/// Wrap command with PowerShell to set environment variables
///
/// Generates:
/// ```powershell
/// powershell -NoProfile -Command "$env:VAR='value'; & 'command' 'arg1' 'arg2'"
/// ```
fn wrap_with_powershell(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> ClaudeDesktopServerEntry {
    let mut script_parts = Vec::new();

    // Add env var assignments in key order: `env` is a HashMap, and an unsorted
    // walk reorders the script between runs, which changes the rendered entry
    // and forces a needless rewrite of the live config on every sync.
    let mut sorted_env: Vec<(&String, &String)> = env.iter().collect();
    sorted_env.sort_by(|a, b| a.0.cmp(b.0));
    for (name, value) in sorted_env {
        let escaped_value = value.replace('\'', "''");
        script_parts.push(format!("$env:{} = '{}'", name, escaped_value));
    }

    // Build command invocation
    let mut command_parts = vec!["&".to_string()];
    command_parts.push(format!("'{}'", command.replace('\'', "''")));
    for arg in args {
        command_parts.push(format!("'{}'", arg.replace('\'', "''")));
    }
    script_parts.push(command_parts.join(" "));

    ClaudeDesktopServerEntry::Stdio {
        command: "powershell".to_string(),
        args: vec![
            "-NoProfile".to_string(),
            "-Command".to_string(),
            script_parts.join("; "),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio_parts(entry: &ClaudeDesktopServerEntry) -> (&str, &[String]) {
        match entry {
            ClaudeDesktopServerEntry::Stdio { command, args } => (command, args),
            other => panic!("expected a stdio entry, got {other:?}"),
        }
    }

    fn remote_server(server_type: Option<&str>, headers: Option<&[(&str, &str)]>) -> McpServer {
        McpServer {
            server_type: server_type.map(str::to_string),
            command: None,
            args: None,
            env: None,
            cwd: None,
            url: Some("https://example.com/mcp".to_string()),
            headers: headers.map(|h| {
                h.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect()
            }),
        }
    }

    fn render_one(server: McpServer) -> ClaudeDesktopMcpConfig {
        let manifest = McpManifest {
            servers: std::iter::once(("srv".to_string(), server)).collect(),
            inputs: None,
        };
        ClaudeDesktopMcpConfig::from_manifest(&manifest).unwrap()
    }

    #[test]
    fn test_convert_simple_server() {
        let mut servers = HashMap::new();
        servers.insert(
            "test-server".to_string(),
            McpServer {
                server_type: Some("stdio".to_string()),
                command: Some("node".to_string()),
                args: Some(vec!["index.js".to_string()]),
                env: None,
                cwd: None,
                url: None,
                headers: None,
            },
        );

        let manifest = McpManifest {
            servers,
            inputs: None,
        };

        let config = ClaudeDesktopMcpConfig::from_manifest(&manifest).unwrap();

        assert_eq!(config.mcp_servers.len(), 1);
        assert!(config.mcp_servers.contains_key("test-server"));

        let (command, args) = stdio_parts(&config.mcp_servers["test-server"]);
        assert_eq!(command, "node");
        assert_eq!(args, ["index.js"]);
    }

    #[test]
    fn stdio_entry_json_shape_is_unchanged() {
        // Owned stdio entries are fingerprinted from this exact JSON shape, so
        // the remote variant must not add keys to it.
        let entry = ClaudeDesktopServerEntry::Stdio {
            command: "node".to_string(),
            args: Vec::new(),
        };
        assert_eq!(
            serde_json::to_value(&entry).unwrap(),
            serde_json::json!({"command": "node", "args": []})
        );
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn test_wrap_with_env_on_windows() {
        let mut env = HashMap::new();
        env.insert("API_KEY".to_string(), "secret123".to_string());

        let mut servers = HashMap::new();
        servers.insert(
            "test-server".to_string(),
            McpServer {
                server_type: Some("stdio".to_string()),
                command: Some("node".to_string()),
                args: Some(vec!["index.js".to_string()]),
                env: Some(env),
                cwd: None,
                url: None,
                headers: None,
            },
        );

        let manifest = McpManifest {
            servers,
            inputs: None,
        };

        let config = ClaudeDesktopMcpConfig::from_manifest(&manifest).unwrap();
        let (command, args) = stdio_parts(&config.mcp_servers["test-server"]);

        assert_eq!(command, "powershell");
        assert_eq!(args[0], "-NoProfile");
        assert_eq!(args[1], "-Command");
        assert!(args[2].contains("$env:API_KEY = 'secret123'"));
        assert!(args[2].contains("& 'node' 'index.js'"));
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn wrap_with_powershell_orders_env_by_key() {
        let expected = "$env:ALPHA = 'a'; $env:MIDDLE = 'm'; $env:ZULU = 'z'; & 'node' 'index.js'";
        // Each HashMap gets its own random hash seed, so repeated construction
        // exercises different iteration orders for the same content.
        for _ in 0..16 {
            let env: HashMap<String, String> = [("ZULU", "z"), ("ALPHA", "a"), ("MIDDLE", "m")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let entry = wrap_with_powershell("node", &["index.js".to_string()], &env);
            assert_eq!(stdio_parts(&entry).1[2], expected);
        }
    }

    #[test]
    fn http_server_renders_type_url_and_headers() {
        let config = render_one(remote_server(
            Some("http"),
            Some(&[("X-Zeta", "z"), ("CONTEXT7_API_KEY", "k-1")]),
        ));
        let rendered = serde_json::to_string(&config.mcp_servers["srv"]).unwrap();
        assert_eq!(
            rendered,
            r#"{"type":"http","url":"https://example.com/mcp","headers":{"CONTEXT7_API_KEY":"k-1","X-Zeta":"z"}}"#
        );
    }

    #[test]
    fn http_server_without_headers_omits_headers_key() {
        let config = render_one(remote_server(Some("http"), None));
        assert_eq!(
            serde_json::to_value(&config.mcp_servers["srv"]).unwrap(),
            serde_json::json!({"type": "http", "url": "https://example.com/mcp"})
        );
    }

    #[test]
    fn sse_server_keeps_sse_type() {
        let config = render_one(remote_server(Some("sse"), None));
        assert_eq!(
            serde_json::to_value(&config.mcp_servers["srv"]).unwrap()["type"],
            "sse"
        );
    }

    #[test]
    fn url_only_server_renders_as_http() {
        let config = render_one(remote_server(None, None));
        assert_eq!(
            serde_json::to_value(&config.mcp_servers["srv"]).unwrap()["type"],
            "http"
        );
    }

    #[test]
    fn server_without_command_or_url_is_skipped() {
        let mut server = remote_server(Some("http"), None);
        server.url = None;
        assert!(render_one(server).mcp_servers.is_empty());
    }
}
