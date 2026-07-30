//! Truth-store data model: the three ccync content types.
//!
//! ccync's managed content is type-split (reference-first; see the plan's engine
//! data model + D-13):
//! - **MCP server** — definition-is-truth. The full server definition (command/
//!   args/url/env/headers) is stored verbatim under the lockfile `_mcpServers`
//!   namespace (snapshot semantics; captured at `init`/`add`). Reuses
//!   [`ccync_foundation::mcp::McpServer`] rather than redefining the shape.
//! - **Plugin** — reference-is-truth. Only the source + pin sha is stored
//!   (`resolvedPlugins` / `_personalPlugins`); content is fetched→cached→decomposed.
//! - **Loose skill** — no plugin owner → materialized into the canonical root.
//!   Recorded by name only.
//!
//! These are the serde types the adopt/projection layers build on.
//! No `serde(alias)` (zero-alias policy); `camelCase` only.

use ccync_foundation::mcp::McpServer;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Lockfile `_mcpServers` namespace: server name → full MCP definition.
///
/// Definition-is-truth: the value carries command/args/url/env/headers verbatim,
/// so ccync can reproject the server to any MCP-capable agent without re-reading
/// the original master config. `BTreeMap` for deterministic serialization.
pub type McpServerStore = BTreeMap<String, McpServer>;

/// A plugin reference entry (reference-is-truth) for `resolvedPlugins` /
/// `_personalPlugins`. `pin` is the resolved commit sha once fetched (absent =
/// not yet pinned).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginRef {
    pub plugin_id: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
}

/// A loose skill (no owning plugin) marked for materialization into the canonical
/// root. Has no upstream, so only its name is recorded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LooseSkill {
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mcp_server_store_round_trips() {
        // McpServer has no PartialEq; compare via serde_json::Value (which is Eq).
        let mut store: McpServerStore = BTreeMap::new();
        store.insert(
            "memory".to_string(),
            McpServer {
                server_type: Some("stdio".to_string()),
                command: Some("npx".to_string()),
                args: Some(vec!["-y".to_string(), "@mcp/server-memory".to_string()]),
                env: Some(
                    [("API_KEY".to_string(), "${MEMORY_KEY}".to_string())]
                        .into_iter()
                        .collect(),
                ),
                url: None,
                headers: None,
            },
        );
        let text = serde_json::to_string(&store).unwrap();
        let back: McpServerStore = serde_json::from_str(&text).unwrap();
        // Round-trip is lossless at the JSON-value level.
        assert_eq!(
            serde_json::to_value(&store).unwrap(),
            serde_json::to_value(&back).unwrap()
        );
        // Definition fields survive (camelCase `type`, command/args/env).
        let v = serde_json::to_value(&store).unwrap();
        assert_eq!(v["memory"]["type"], "stdio");
        assert_eq!(v["memory"]["command"], "npx");
        assert_eq!(v["memory"]["env"]["API_KEY"], "${MEMORY_KEY}");
    }

    #[test]
    fn plugin_ref_round_trips_camel_case() {
        let p = PluginRef {
            plugin_id: "my-plugin".to_string(),
            source: "https://github.com/user/my-plugin.git".to_string(),
            pin: Some("abc1234".to_string()),
        };
        let text = serde_json::to_string(&p).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            json!({"pluginId":"my-plugin","source":"https://github.com/user/my-plugin.git","pin":"abc1234"})
        );
        assert_eq!(serde_json::from_str::<PluginRef>(&text).unwrap(), p);
    }

    #[test]
    fn plugin_ref_unpinned_omits_pin() {
        let p = PluginRef {
            plugin_id: "x".to_string(),
            source: "/local/x".to_string(),
            pin: None,
        };
        let text = serde_json::to_string(&p).unwrap();
        assert!(
            !text.contains("pin"),
            "unpinned plugin must omit the pin field"
        );
        assert_eq!(serde_json::from_str::<PluginRef>(&text).unwrap(), p);
    }

    #[test]
    fn loose_skill_round_trips() {
        let s = LooseSkill {
            name: "my-skill".to_string(),
        };
        let text = serde_json::to_string(&s).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            json!({"name":"my-skill"})
        );
        assert_eq!(serde_json::from_str::<LooseSkill>(&text).unwrap(), s);
    }
}
