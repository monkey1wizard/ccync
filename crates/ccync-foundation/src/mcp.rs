//! MCP (Model Context Protocol) shared data-model types.
//!
//! These are the serde-serializable manifest/server types plus the error and
//! `Result` alias shared between the MCP generation logic (`mcp` crate) and the
//! file/skill projection layer (`projection` crate).
//!
//! They live in `base` to break the dependency cycle that would otherwise appear
//! between the projection and `mcp` crates. Pure data + error types; no CCYNC-crate deps.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

/// MCP-related errors.
#[derive(Debug, Error)]
pub enum McpError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON parse error: {0}")]
    JsonParse(#[from] serde_json::Error),

    #[error("Plugin root incomplete, missing required directory: {0}")]
    PluginRootIncomplete(String),

    #[error("Invalid variable name: {0}")]
    InvalidVariableName(String),
}

pub type Result<T> = std::result::Result<T, McpError>;

/// MCP server configuration.
///
/// Represents a single MCP server entry from a manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServer {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub server_type: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
}

/// MCP manifest structure.
///
/// Represents the portable `.mcp.json` manifest from repo source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpManifest {
    pub servers: HashMap<String, McpServer>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub inputs: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_server_cwd_round_trip() {
        let server_with_cwd: McpServer =
            serde_json::from_str(r#"{"command":"node","cwd":"/path/to/workdir"}"#).unwrap();
        assert_eq!(server_with_cwd.cwd.as_deref(), Some("/path/to/workdir"));
        let json_with_cwd = serde_json::to_string(&server_with_cwd).unwrap();
        assert!(json_with_cwd.contains(r#""cwd":"/path/to/workdir""#));

        let back: McpServer = serde_json::from_str(&json_with_cwd).unwrap();
        assert_eq!(back.cwd.as_deref(), Some("/path/to/workdir"));

        let server_without_cwd: McpServer = serde_json::from_str(r#"{"command":"node"}"#).unwrap();
        assert_eq!(server_without_cwd.cwd, None);
        let json_without_cwd = serde_json::to_string(&server_without_cwd).unwrap();
        assert!(!json_without_cwd.contains("cwd"));
    }
}
