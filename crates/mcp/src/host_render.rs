//! In-memory rendering of a portable manifest into each host's *exact*
//! host-native server entries.
//!
//! Reuses the four existing serializers (Claude / Copilot / OpenCode / Codex) to
//! produce, per host, a `server-name -> host-native entry` map — the same shape
//! each host would actually write, including that host's own filtering (e.g.
//! Claude drops servers with neither a command nor a url, both JSON hosts drop
//! unresolved secrets). No live file is read or written: this is purely the desired/prior
//! rendering seam consumed by the ownership planner and the crash-recovery
//! bootstrap. Each rendered entry can be fingerprinted (`entry.rs`) so ownership
//! can be proven without persisting the entry's resolved content.

use crate::entry::{fingerprint_codex_toml, fingerprint_json, EntryFingerprint};
use crate::serializers::agy::AgyMcpConfig;
use crate::serializers::claude_mcp::ClaudeDesktopMcpConfig;
use crate::serializers::codex::CodexMcpConfig;
use crate::serializers::copilot::CopilotCliMcpConfig;
use crate::serializers::opencode::OpenCodeMcpConfig;
use crate::serializers::McpHostConfig;
use crate::McpUpdateError;
use ccync_foundation::mcp::McpManifest;
use std::collections::BTreeMap;

/// The five fixed MCP hosts ccync projects onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Host {
    Claude,
    Copilot,
    OpenCode,
    Codex,
    Agy,
}

impl Host {
    /// All five hosts, in a stable order.
    pub(crate) const ALL: [Host; 5] = [
        Host::Claude,
        Host::Copilot,
        Host::OpenCode,
        Host::Codex,
        Host::Agy,
    ];

    /// Stable identifier used as a key in the ownership/transaction state files.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Host::Claude => "claude",
            Host::Copilot => "copilot",
            Host::OpenCode => "opencode",
            Host::Codex => "codex",
            Host::Agy => "agy",
        }
    }
}

/// A single host-native server entry, exactly as it appears in that host's
/// config. JSON hosts carry a `serde_json::Value`; Codex carries the TOML text
/// of the entry's section(s). Both can be fingerprinted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostEntry {
    Json(serde_json::Value),
    CodexToml(String),
}

impl HostEntry {
    /// Fingerprint this rendered entry (see `entry.rs`).
    pub(crate) fn fingerprint(&self) -> Result<EntryFingerprint, McpUpdateError> {
        match self {
            HostEntry::Json(v) => Ok(fingerprint_json(v)),
            HostEntry::CodexToml(t) => fingerprint_codex_toml(t),
        }
    }
}

/// Render a portable manifest into the exact host-native entry map ccync would
/// write to `host`. Server names a host skips are absent from that host's map,
/// mirroring the writer. Reuses each serializer verbatim by rendering one server
/// at a time (the serializers process servers independently). No live I/O.
pub(crate) fn render_host_entries(
    host: Host,
    manifest: &McpManifest,
) -> Result<BTreeMap<String, HostEntry>, McpUpdateError> {
    let mut out = BTreeMap::new();
    for (name, server) in &manifest.servers {
        let single = McpManifest {
            servers: std::iter::once((name.clone(), server.clone())).collect(),
            inputs: None,
        };
        match host {
            Host::Claude => {
                let cfg = ClaudeDesktopMcpConfig::from_manifest(&single)?;
                if let Some(entry) = cfg.mcp_servers.get(name) {
                    out.insert(name.clone(), HostEntry::Json(serde_json::to_value(entry)?));
                }
            }
            Host::Copilot => {
                let cfg = CopilotCliMcpConfig::from_manifest(&single)?;
                if let Some(entry) = cfg.mcp_servers.get(name) {
                    out.insert(name.clone(), HostEntry::Json(serde_json::to_value(entry)?));
                }
            }
            Host::OpenCode => {
                let cfg = OpenCodeMcpConfig::from_manifest(&single)?;
                if let Some(entry) = cfg.mcp.get(name) {
                    out.insert(name.clone(), HostEntry::Json(serde_json::to_value(entry)?));
                }
            }
            Host::Codex => {
                let cfg = CodexMcpConfig::from_manifest(&single)?;
                if cfg.servers.contains_key(name) {
                    out.insert(name.clone(), HostEntry::CodexToml(cfg.to_config_string()?));
                }
            }
            Host::Agy => {
                let cfg = AgyMcpConfig::from_manifest(&single)?;
                if let Some(entry) = cfg.mcp_servers.get(name) {
                    out.insert(name.clone(), HostEntry::Json(serde_json::to_value(entry)?));
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccync_foundation::mcp::McpServer;
    use std::collections::HashMap;

    fn stdio(command: &str, args: &[&str]) -> McpServer {
        McpServer {
            server_type: Some("stdio".to_string()),
            command: Some(command.to_string()),
            args: Some(args.iter().map(|s| s.to_string()).collect()),
            env: None,
            cwd: None,
            url: None,
            headers: None,
        }
    }

    fn manifest_of(entries: &[(&str, McpServer)]) -> McpManifest {
        let mut servers = HashMap::new();
        for (name, server) in entries {
            servers.insert(name.to_string(), server.clone());
        }
        McpManifest {
            servers,
            inputs: None,
        }
    }

    #[test]
    fn host_rendered_entries_same_name_set_four_shapes() {
        let manifest = manifest_of(&[
            ("alpha", stdio("node", &["a.js"])),
            ("beta", stdio("python", &["-m", "b"])),
        ]);

        let claude = render_host_entries(Host::Claude, &manifest).unwrap();
        let copilot = render_host_entries(Host::Copilot, &manifest).unwrap();
        let opencode = render_host_entries(Host::OpenCode, &manifest).unwrap();
        let codex = render_host_entries(Host::Codex, &manifest).unwrap();

        // Identical sorted server-name set across all four hosts (BTreeMap keys).
        let names: Vec<String> = claude.keys().cloned().collect();
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
        for map in [&copilot, &opencode, &codex] {
            assert_eq!(map.keys().cloned().collect::<Vec<_>>(), names);
        }

        // Four distinct provider-native shapes for the same server.
        match &claude["alpha"] {
            HostEntry::Json(v) => {
                assert_eq!(v["command"], "node");
                assert!(v.get("tools").is_none(), "Claude entry has no tools field");
            }
            _ => panic!("Claude entry must be JSON"),
        }
        match &copilot["alpha"] {
            HostEntry::Json(v) => {
                assert_eq!(v["type"], "local");
                assert_eq!(v["tools"][0], "*");
            }
            _ => panic!("Copilot entry must be JSON"),
        }
        match &opencode["alpha"] {
            HostEntry::Json(v) => {
                assert_eq!(v["type"], "local");
                assert!(v["command"].is_array(), "OpenCode command is a flat array");
                assert_eq!(v["enabled"], true);
            }
            _ => panic!("OpenCode entry must be JSON"),
        }
        match &codex["alpha"] {
            HostEntry::CodexToml(t) => {
                assert!(t.contains("[mcp_servers.alpha]"));
                assert!(t.contains("command = \"node\""));
            }
            _ => panic!("Codex entry must be TOML"),
        }
    }

    #[test]
    fn host_rendered_entries_respect_per_host_filtering() {
        // An HTTP (command-less) server: every host keeps it, Claude included.
        let http = McpServer {
            server_type: Some("http".to_string()),
            command: None,
            args: None,
            env: None,
            cwd: None,
            url: Some("https://example.com/mcp/".to_string()),
            headers: None,
        };
        let manifest = manifest_of(&[("remote", http.clone())]);
        for host in Host::ALL {
            assert!(
                render_host_entries(host, &manifest)
                    .unwrap()
                    .contains_key("remote"),
                "{} must keep the HTTP server",
                host.as_str()
            );
        }

        // A server with neither a command nor a url: Claude drops it.
        let empty = McpServer { url: None, ..http };
        assert!(
            render_host_entries(Host::Claude, &manifest_of(&[("empty", empty)]))
                .unwrap()
                .is_empty(),
            "Claude skips servers with neither a command nor a url"
        );
    }

    #[test]
    fn host_all_has_five_hosts_including_agy() {
        assert_eq!(Host::ALL.len(), 5);
        assert!(Host::ALL.contains(&Host::Agy));
        assert_eq!(Host::Agy.as_str(), "agy");
    }

    #[test]
    fn render_host_entries_agy_non_empty_for_one_server() {
        let manifest = manifest_of(&[("alpha", stdio("node", &["a.js"]))]);
        let agy = render_host_entries(Host::Agy, &manifest).unwrap();
        assert!(!agy.is_empty());
        match &agy["alpha"] {
            HostEntry::Json(v) => assert_eq!(v["command"], "node"),
            _ => panic!("agy entry must be JSON"),
        }
    }

    #[test]
    fn host_rendered_entry_fingerprints_are_order_stable() {
        let manifest = manifest_of(&[("alpha", stdio("node", &["a.js"]))]);
        let a = render_host_entries(Host::Claude, &manifest).unwrap();
        let b = render_host_entries(Host::Claude, &manifest).unwrap();
        assert_eq!(
            a["alpha"].fingerprint().unwrap(),
            b["alpha"].fingerprint().unwrap()
        );
    }
}
