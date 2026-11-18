//! MCP host config serializers.
//!
//! Converts the portable `.mcp.json` manifest into each MCP host's native
//! config format:
//! - Claude Desktop: stdio/command JSON
//! - Copilot CLI: local/http transport JSON
//! - Codex CLI: `[mcp_servers.name]` TOML sections in `~/.codex/config.toml`
//! - OpenCode: `{"mcp": {...}}` JSON in `opencode.json`
//! - agy (Antigravity): `mcpServers` JSON in `mcp_config.json`, remote servers
//!   addressed via `serverUrl`

pub mod agy;
pub mod claude_mcp;
pub mod codex;
pub mod copilot;
pub mod opencode;

use ccync_foundation::mcp::{McpManifest, Result};

/// MCP host config trait.
///
/// `to_config_string` returns the host's native format (JSON for most hosts,
/// TOML for Codex). The name is format-agnostic so all four hosts can implement
/// the same trait.
pub trait McpHostConfig {
    /// Convert portable manifest to host-specific format.
    fn from_manifest(manifest: &McpManifest) -> Result<Self>
    where
        Self: Sized;

    /// Serialize to the host's native config format (JSON or TOML).
    fn to_config_string(&self) -> Result<String>;
}
