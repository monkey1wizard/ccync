//! agy (Antigravity) MCP configuration serializer
//!
//! Converts portable `.mcp.json` manifest to Antigravity's `mcp_config.json`
//! format. Remote servers are addressed with `serverUrl` — the unsupported
//! legacy `url`/`httpUrl` spellings are never emitted.
//!
//! Output format:
//! ```json
//! {
//!   "mcpServers": {
//!     "server-name": {
//!       "command": "...",
//!       "args": ["..."],
//!       "env": {"...": "..."},
//!       "cwd": "...",
//!       "serverUrl": "...",
//!       "headers": {"...": "..."}
//!     }
//!   }
//! }
//! ```

use super::McpHostConfig;
use ccync_foundation::mcp::{McpManifest, McpServer, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// agy server entry.
///
/// `env` and `headers` are ordered maps so the written config lists keys in a
/// stable order instead of the manifest HashMap's per-run iteration order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgyServerEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,

    #[serde(rename = "serverUrl", skip_serializing_if = "Option::is_none")]
    pub server_url: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
}

/// agy MCP configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgyMcpConfig {
    pub mcp_servers: HashMap<String, AgyServerEntry>,
}

impl McpHostConfig for AgyMcpConfig {
    fn from_manifest(manifest: &McpManifest) -> Result<Self> {
        let mut mcp_servers = HashMap::new();

        for (server_name, server_config) in &manifest.servers {
            let entry = convert_to_agy_entry(server_config);
            mcp_servers.insert(server_name.clone(), entry);
        }

        Ok(Self { mcp_servers })
    }

    fn to_config_string(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}

/// Convert MCP server config to an agy entry. Remote servers (a `url` in the
/// portable manifest) are re-addressed as `serverUrl`; stdio servers carry
/// `command`/`args`/`env`.
fn convert_to_agy_entry(server_config: &McpServer) -> AgyServerEntry {
    if let Some(ref url) = server_config.url {
        return AgyServerEntry {
            command: None,
            args: None,
            env: None,
            cwd: None,
            server_url: Some(url.clone()),
            headers: server_config.headers.clone().map(BTreeMap::from_iter),
        };
    }

    AgyServerEntry {
        command: server_config.command.clone(),
        args: server_config.args.clone(),
        env: server_config.env.clone().map(BTreeMap::from_iter),
        cwd: server_config.cwd.clone(),
        server_url: None,
        headers: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_stdio_server() {
        let mut servers = HashMap::new();
        servers.insert(
            "stdio-server".to_string(),
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
            provenance: Default::default(),
            servers,
            inputs: None,
        };

        let config = AgyMcpConfig::from_manifest(&manifest).unwrap();
        let entry = &config.mcp_servers["stdio-server"];

        assert_eq!(entry.command.as_ref().unwrap(), "node");
        assert_eq!(entry.args.as_ref().unwrap(), &vec!["index.js".to_string()]);
        assert!(entry.server_url.is_none());

        let rendered = config.to_config_string().unwrap();
        assert!(rendered.contains("\"mcpServers\""));
        assert!(rendered.contains("\"command\""));
        assert!(!rendered.contains("\"cwd\""));
    }

    #[test]
    fn renders_stdio_server_with_cwd() {
        let mut servers = HashMap::new();
        servers.insert(
            "stdio-with-cwd".to_string(),
            McpServer {
                server_type: Some("stdio".to_string()),
                command: Some("node".to_string()),
                args: Some(vec!["index.js".to_string()]),
                env: None,
                cwd: Some("/path/to/project".to_string()),
                url: None,
                headers: None,
            },
        );

        let manifest = McpManifest {
            provenance: Default::default(),
            servers,
            inputs: None,
        };

        let config = AgyMcpConfig::from_manifest(&manifest).unwrap();
        let entry = &config.mcp_servers["stdio-with-cwd"];

        assert_eq!(entry.command.as_deref(), Some("node"));
        assert_eq!(entry.cwd.as_deref(), Some("/path/to/project"));
        assert!(entry.server_url.is_none());

        let rendered = config.to_config_string().unwrap();
        assert!(rendered.contains("\"cwd\": \"/path/to/project\""));
    }

    #[test]
    fn renders_remote_server_as_server_url_without_legacy_spellings() {
        let mut servers = HashMap::new();
        servers.insert(
            "remote-server".to_string(),
            McpServer {
                server_type: Some("http".to_string()),
                command: None,
                args: None,
                env: None,
                cwd: None,
                url: Some("https://example.com/mcp".to_string()),
                headers: None,
            },
        );

        let manifest = McpManifest {
            provenance: Default::default(),
            servers,
            inputs: None,
        };

        let config = AgyMcpConfig::from_manifest(&manifest).unwrap();
        let entry = &config.mcp_servers["remote-server"];

        assert_eq!(
            entry.server_url.as_ref().unwrap(),
            "https://example.com/mcp"
        );
        assert!(entry.command.is_none());

        let rendered = config.to_config_string().unwrap();
        assert!(rendered.contains("\"serverUrl\""));
        assert!(!rendered.contains("\"url\""));
        assert!(!rendered.contains("\"httpUrl\""));
    }

    #[test]
    fn renders_remote_server_headers() {
        let mut headers = HashMap::new();
        headers.insert("X-Api-Version".to_string(), "2024-01".to_string());

        let mut servers = HashMap::new();
        servers.insert(
            "remote-with-headers".to_string(),
            McpServer {
                server_type: Some("http".to_string()),
                command: None,
                args: None,
                env: None,
                cwd: None,
                url: Some("https://example.com/mcp".to_string()),
                headers: Some(headers),
            },
        );

        let manifest = McpManifest {
            provenance: Default::default(),
            servers,
            inputs: None,
        };

        let config = AgyMcpConfig::from_manifest(&manifest).unwrap();
        let entry = &config.mcp_servers["remote-with-headers"];

        assert_eq!(
            entry.headers.as_ref().unwrap().get("X-Api-Version"),
            Some(&"2024-01".to_string())
        );

        let rendered = config.to_config_string().unwrap();
        assert!(rendered.contains("\"headers\""));
        assert!(!rendered.contains("\"httpUrl\""));
        assert!(!rendered.contains("\"url\""));
    }
}
