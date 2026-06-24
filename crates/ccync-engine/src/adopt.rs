use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use ccync_foundation::mcp::McpServer;
use ccync_foundation::paths::{
    claude_config_path, claude_installed_plugins_path, claude_known_marketplaces_path,
    claude_skills_path, codex_config_path, shared_agents_skills_path,
};

use crate::truth::{LooseSkill, McpServerStore};

// ── adopt error ───────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum AdoptError {
    #[error("failed to read lock file: {0}")]
    Read(String),
    #[error("failed to write lock file: {0}")]
    Write(String),
}

// ── adopt namespace key ───────────────────────────────────────────────────────

const ADOPT_KEY: &str = "_adoptedItems";
const MCP_SERVERS_KEY: &str = "_mcpServers";
const LOOSE_SKILLS_KEY: &str = "_looseSkills";

// ── shared types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentKind {
    Claude,
    Codex,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemKind {
    Plugin,
    McpServer,
    Skill,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledItem {
    pub name: String,
    pub version: Option<String>,
    pub agent: AgentKind,
    pub kind: ItemKind,
    /// Resolvable origin reference for re-fetch. Set for marketplace plugins
    /// (derived from `name@marketplace` + version); `None` for MCP/skill items.
    pub source: Option<String>,
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn read_json_file(path: Option<std::path::PathBuf>) -> Option<Value> {
    let path = path?;
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

// ── installed_plugins.json v2 ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct InstalledPluginsV2 {
    #[serde(default)]
    plugins: HashMap<String, Vec<PluginRecord>>,
}

#[derive(Debug, Deserialize)]
struct PluginRecord {
    version: Option<String>,
}

fn parse_installed_plugins(raw: Value) -> Vec<InstalledItem> {
    let parsed: InstalledPluginsV2 = match serde_json::from_value(raw) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    parsed
        .plugins
        .into_iter()
        .map(|(name, records)| {
            let version = records.into_iter().find_map(|r| r.version);
            let source = Some(derive_plugin_source(&name, version.as_deref()));
            InstalledItem {
                name,
                version,
                agent: AgentKind::Claude,
                kind: ItemKind::Plugin,
                source,
            }
        })
        .collect()
}

/// Derive a resolvable source reference for an adopted Claude marketplace
/// plugin from its `name@marketplace` key plus the installed version.
///
/// Format: `"<marketplace>:<name>@<version>"`. The version suffix is dropped
/// when absent; the marketplace prefix is dropped when the key has no `@`.
fn derive_plugin_source(name_at_marketplace: &str, version: Option<&str>) -> String {
    let (name, marketplace) = match name_at_marketplace.split_once('@') {
        Some((n, m)) => (n, Some(m)),
        None => (name_at_marketplace, None),
    };
    let base = match marketplace {
        Some(m) => format!("{m}:{name}"),
        None => name.to_string(),
    };
    match version {
        Some(v) => format!("{base}@{v}"),
        None => base,
    }
}

// ── ~/.claude.json mcpServers ─────────────────────────────────────────────────

fn parse_claude_mcp_servers(raw: Value) -> Vec<InstalledItem> {
    let servers = match raw.get("mcpServers").and_then(|v| v.as_object()) {
        Some(s) => s,
        None => return vec![],
    };
    servers
        .keys()
        .map(|name| InstalledItem {
            name: name.clone(),
            version: None,
            agent: AgentKind::Claude,
            kind: ItemKind::McpServer,
            source: None,
        })
        .collect()
}

// ── full MCP definitions (snapshot into _mcpServers) ───────────────────────────

/// Capture the FULL Claude MCP server definitions (command/args/url/env/headers),
/// not just names, from `~/.claude.json` `mcpServers`. Definition-is-truth: the
/// snapshot lets ccync reproject each server without re-reading the master config.
/// Entries that fail to deserialize into [`McpServer`] are skipped.
fn parse_claude_mcp_defs(raw: &Value) -> McpServerStore {
    let mut store = McpServerStore::new();
    if let Some(map) = raw.get("mcpServers").and_then(|v| v.as_object()) {
        for (name, def) in map {
            if let Ok(server) = serde_json::from_value::<McpServer>(def.clone()) {
                store.insert(name.clone(), server);
            }
        }
    }
    store
}

#[derive(Debug, Deserialize)]
struct CodexConfigDefs {
    #[serde(default)]
    mcp_servers: BTreeMap<String, McpServer>,
}

/// Capture the FULL Codex MCP server definitions from `~/.codex/config.toml`
/// `[mcp_servers.*]`. Graceful-missing: unparseable config → empty store.
fn parse_codex_mcp_defs(text: &str) -> McpServerStore {
    match toml::from_str::<CodexConfigDefs>(text) {
        Ok(cfg) => cfg.mcp_servers,
        Err(_) => McpServerStore::new(),
    }
}

// ── public API ────────────────────────────────────────────────────────────────

/// Read Claude's existing install state from three files.
///
/// Graceful-missing: any absent or unparseable file contributes an empty set,
/// no error is returned. Unknown schema versions degrade silently.
pub fn read_claude_state() -> Vec<InstalledItem> {
    let mut items: Vec<InstalledItem> = Vec::new();

    if let Some(raw) = read_json_file(claude_installed_plugins_path()) {
        items.extend(parse_installed_plugins(raw));
    }

    if let Some(raw) = read_json_file(claude_config_path()) {
        items.extend(parse_claude_mcp_servers(raw));
    }

    if let Some(skills_dir) = claude_skills_path() {
        items.extend(enumerate_skill_dirs(&skills_dir, AgentKind::Claude));
    }

    let _ = read_json_file(claude_known_marketplaces_path());

    items
}

// ── codex config.toml ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CodexConfig {
    #[serde(default)]
    mcp_servers: HashMap<String, toml::Value>,
}

fn parse_codex_mcp_servers(text: &str) -> Vec<InstalledItem> {
    let cfg: CodexConfig = match toml::from_str(text) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    cfg.mcp_servers
        .into_keys()
        .map(|name| InstalledItem {
            name,
            version: None,
            agent: AgentKind::Codex,
            kind: ItemKind::McpServer,
            source: None,
        })
        .collect()
}

/// Read Codex's existing MCP install state from `~/.codex/config.toml`.
///
/// Only the `mcp_servers` table is read; Codex has no native plugin concept.
/// Graceful-missing: absent or unparseable file → empty set, no error.
pub fn read_codex_state() -> Vec<InstalledItem> {
    let mut items: Vec<InstalledItem> = Vec::new();

    if let Some(path) = codex_config_path() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            items.extend(parse_codex_mcp_servers(&text));
        }
    }

    // Codex consumes skills from the shared `~/.agents/skills/` directory.
    if let Some(skills_dir) = shared_agents_skills_path() {
        items.extend(enumerate_skill_dirs(&skills_dir, AgentKind::Codex));
    }

    items
}

/// Read the FULL MCP server definitions from a master agent for snapshotting
/// into `_mcpServers`. Graceful-missing: absent/unparseable config → empty store.
pub fn read_mcp_definitions(agent: &AgentKind) -> McpServerStore {
    match agent {
        AgentKind::Claude => read_json_file(claude_config_path())
            .map(|raw| parse_claude_mcp_defs(&raw))
            .unwrap_or_default(),
        AgentKind::Codex => codex_config_path()
            .and_then(|p| std::fs::read_to_string(&p).ok())
            .map(|text| parse_codex_mcp_defs(&text))
            .unwrap_or_default(),
    }
}

// ── adopt logic ───────────────────────────────────────────────────────────────

fn source_id(item: &InstalledItem) -> &'static str {
    match (&item.agent, &item.kind) {
        (AgentKind::Claude, ItemKind::Plugin) => "claude-plugin",
        (AgentKind::Claude, ItemKind::McpServer) => "claude-mcp",
        (AgentKind::Claude, ItemKind::Skill) => "claude-skill",
        (AgentKind::Codex, ItemKind::McpServer) => "codex-mcp",
        (AgentKind::Codex, ItemKind::Plugin) => "codex-plugin",
        (AgentKind::Codex, ItemKind::Skill) => "codex-skill",
    }
}

/// Enumerate skill directories under `skills_root`.
///
/// A skill is a child directory containing a `SKILL.md`. Graceful-missing:
/// an absent or unreadable root yields an empty set. Results are name-sorted.
fn enumerate_skill_dirs(skills_root: &Path, agent: AgentKind) -> Vec<InstalledItem> {
    let mut items = Vec::new();
    let Ok(entries) = std::fs::read_dir(skills_root) else {
        return items;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && path.join("SKILL.md").is_file() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                items.push(InstalledItem {
                    name: name.to_string(),
                    version: None,
                    agent: agent.clone(),
                    kind: ItemKind::Skill,
                    source: None,
                });
            }
        }
    }
    items.sort_by(|a, b| a.name.cmp(&b.name));
    items
}

fn agent_label(agent: &AgentKind) -> &'static str {
    match agent {
        AgentKind::Claude => "claude",
        AgentKind::Codex => "codex",
    }
}

fn read_lock_file(lock_path: &Path) -> Result<Value, AdoptError> {
    if !lock_path.exists() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    let text = std::fs::read_to_string(lock_path).map_err(|e| AdoptError::Read(e.to_string()))?;
    Ok(serde_json::from_str::<Value>(&text)
        .unwrap_or_else(|_| Value::Object(serde_json::Map::new())))
}

/// Write `_adoptedItems` into the lock file, preserving all other namespaces.
fn splice_adopt_key(existing: Value, adopted: Vec<Value>) -> Value {
    let adopted_value = Value::Array(adopted);
    match existing {
        Value::Object(mut map) => {
            map.insert(ADOPT_KEY.to_string(), adopted_value);
            Value::Object(map)
        }
        _ => json!({ ADOPT_KEY: adopted_value }),
    }
}

/// Merge full MCP server definitions into the lock file `_mcpServers` namespace,
/// preserving all other namespaces and any previously-snapshotted servers.
/// Same-name servers are refreshed (snapshot semantics: latest master wins).
fn splice_mcp_servers(existing: Value, store: &McpServerStore) -> Value {
    let mut map = match existing {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    let mut servers = map
        .get(MCP_SERVERS_KEY)
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    for (name, def) in store {
        if let Ok(v) = serde_json::to_value(def) {
            servers.insert(name.clone(), v);
        }
    }
    map.insert(MCP_SERVERS_KEY.to_string(), Value::Object(servers));
    Value::Object(map)
}

/// Snapshot the FULL MCP server definitions into the lock file `_mcpServers`
/// namespace (definition-is-truth). No-op when the store is empty (idempotent).
pub fn adopt_mcp_definitions(store: McpServerStore, lock_path: &Path) -> Result<(), AdoptError> {
    if store.is_empty() {
        return Ok(());
    }
    let existing = read_lock_file(lock_path)?;
    let updated = splice_mcp_servers(existing, &store);
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| AdoptError::Write(e.to_string()))?;
    }
    let text =
        serde_json::to_string_pretty(&updated).map_err(|e| AdoptError::Write(e.to_string()))?;
    std::fs::write(lock_path, text).map_err(|e| AdoptError::Write(e.to_string()))?;
    Ok(())
}

/// Select the loose skills (no owning plugin) from a skill-item set.
///
/// A skill whose name is in `plugin_owned` is bundled by a plugin and is
/// reprojected through that plugin's decomposition — it must NOT be double-marked
/// for canonical-root materialization. Non-skill items are ignored. Result is
/// name-deduplicated and sorted (deterministic).
fn mark_loose_skills(items: &[InstalledItem], plugin_owned: &HashSet<String>) -> Vec<LooseSkill> {
    let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for item in items {
        if item.kind == ItemKind::Skill && !plugin_owned.contains(&item.name) {
            names.insert(item.name.clone());
        }
    }
    names.into_iter().map(|name| LooseSkill { name }).collect()
}

/// Merge loose-skill markers into the lock file `_looseSkills` namespace,
/// preserving all other namespaces.
fn splice_loose_skills(existing: Value, skills: &[LooseSkill]) -> Value {
    let arr: Vec<Value> = skills
        .iter()
        .filter_map(|s| serde_json::to_value(s).ok())
        .collect();
    match existing {
        Value::Object(mut map) => {
            map.insert(LOOSE_SKILLS_KEY.to_string(), Value::Array(arr));
            Value::Object(map)
        }
        _ => json!({ LOOSE_SKILLS_KEY: Value::Array(arr) }),
    }
}

/// Mark loose skills (skills with no owning plugin) for materialization into the
/// canonical root, writing them to the lock file `_looseSkills` namespace.
/// No-op when there is nothing to mark (idempotent).
pub fn adopt_loose_skills(
    items: &[InstalledItem],
    plugin_owned: &HashSet<String>,
    lock_path: &Path,
) -> Result<Vec<LooseSkill>, AdoptError> {
    let loose = mark_loose_skills(items, plugin_owned);
    if loose.is_empty() {
        return Ok(loose);
    }
    let existing = read_lock_file(lock_path)?;
    let updated = splice_loose_skills(existing, &loose);
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| AdoptError::Write(e.to_string()))?;
    }
    let text =
        serde_json::to_string_pretty(&updated).map_err(|e| AdoptError::Write(e.to_string()))?;
    std::fs::write(lock_path, text).map_err(|e| AdoptError::Write(e.to_string()))?;
    Ok(loose)
}

/// Collect already-managed item names from the lock file:
/// - `resolvedPlugins[].pluginId` → CCYNC catalog-installed
/// - `_adoptedItems[].name` → previously adopted
fn managed_names(lock: &Value) -> HashSet<String> {
    let mut names = HashSet::new();
    if let Some(arr) = lock.get("resolvedPlugins").and_then(|v| v.as_array()) {
        for entry in arr {
            if let Some(id) = entry.get("pluginId").and_then(|v| v.as_str()) {
                names.insert(id.to_string());
            }
        }
    }
    if let Some(arr) = lock.get(ADOPT_KEY).and_then(|v| v.as_array()) {
        for entry in arr {
            if let Some(n) = entry.get("name").and_then(|v| v.as_str()) {
                names.insert(n.to_string());
            }
        }
    }
    names
}

/// Write non-CCYNC-managed items into the lock file under `_adoptedItems`,
/// tagged with `adoptedFrom` + original version + sourceId provenance.
/// Items whose names are already in `resolvedPlugins` or `_adoptedItems`
/// are silently skipped — no duplicates, no binary download.
///
/// Returns the newly adopted items (not already present before this call).
pub fn adopt_items(
    items: Vec<InstalledItem>,
    lock_path: &Path,
) -> Result<Vec<InstalledItem>, AdoptError> {
    let existing = read_lock_file(lock_path)?;
    let already_managed = managed_names(&existing);

    // Preserve the already-adopted entries as-is.
    let mut adopted_arr: Vec<Value> = existing
        .get(ADOPT_KEY)
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut newly_adopted: Vec<InstalledItem> = Vec::new();
    for item in items {
        if already_managed.contains(&item.name) {
            continue;
        }
        let mut entry = serde_json::Map::new();
        entry.insert("name".to_string(), json!(item.name));
        entry.insert("adoptedFrom".to_string(), json!(agent_label(&item.agent)));
        entry.insert("version".to_string(), json!(item.version));
        entry.insert("sourceId".to_string(), json!(source_id(&item)));
        if let Some(src) = &item.source {
            entry.insert("source".to_string(), json!(src));
        }
        adopted_arr.push(Value::Object(entry));
        newly_adopted.push(item);
    }

    // Only write if something changed.
    if !newly_adopted.is_empty() {
        let updated = splice_adopt_key(existing, adopted_arr);
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AdoptError::Write(e.to_string()))?;
        }
        let text =
            serde_json::to_string_pretty(&updated).map_err(|e| AdoptError::Write(e.to_string()))?;
        std::fs::write(lock_path, text).map_err(|e| AdoptError::Write(e.to_string()))?;
    }

    Ok(newly_adopted)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plugin_item(name: &str, version: &str) -> InstalledItem {
        InstalledItem {
            name: name.to_string(),
            version: Some(version.to_string()),
            agent: AgentKind::Claude,
            kind: ItemKind::Plugin,
            source: Some(derive_plugin_source(name, Some(version))),
        }
    }

    fn mcp_item(name: &str) -> InstalledItem {
        InstalledItem {
            name: name.to_string(),
            version: None,
            agent: AgentKind::Claude,
            kind: ItemKind::McpServer,
            source: None,
        }
    }

    #[test]
    fn parse_installed_plugins_v2_real_schema() {
        let raw = json!({
            "version": 2,
            "plugins": {
                "ccync@ccync": [{ "version": "1.0.1", "installPath": "/some/path", "installedAt": "2026-01-01T00:00:00Z" }],
                "other@mkt": [{ "version": "0.5.0" }]
            }
        });
        let mut items = parse_installed_plugins(raw);
        items.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], plugin_item("ccync@ccync", "1.0.1"));
        assert_eq!(items[1], plugin_item("other@mkt", "0.5.0"));
    }

    #[test]
    fn parse_installed_plugins_empty_plugins_map() {
        let raw = json!({ "version": 2, "plugins": {} });
        assert_eq!(parse_installed_plugins(raw), vec![]);
    }

    #[test]
    fn parse_installed_plugins_no_version_field() {
        let raw = json!({
            "version": 2,
            "plugins": { "tool@mkt": [{ "installPath": "/p" }] }
        });
        let items = parse_installed_plugins(raw);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].version, None);
    }

    #[test]
    fn parse_installed_plugins_bad_schema_returns_empty() {
        let raw = json!({ "unexpected": "shape" });
        assert_eq!(parse_installed_plugins(raw), vec![]);
    }

    #[test]
    fn parse_claude_mcp_servers_extracts_server_names() {
        let raw = json!({
            "mcpServers": {
                "memory":   { "type": "stdio", "command": "npx" },
                "codebase": { "type": "stdio", "command": "node" }
            }
        });
        let mut items = parse_claude_mcp_servers(raw);
        items.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], mcp_item("codebase"));
        assert_eq!(items[1], mcp_item("memory"));
    }

    #[test]
    fn parse_claude_mcp_servers_absent_key_returns_empty() {
        let raw = json!({ "other": {} });
        assert_eq!(parse_claude_mcp_servers(raw), vec![]);
    }

    #[test]
    fn read_json_file_absent_path_returns_none() {
        assert!(read_json_file(None).is_none());
    }

    #[test]
    fn read_json_file_nonexistent_file_returns_none() {
        let p = std::path::PathBuf::from("/nonexistent/path/that/cannot/exist.json");
        assert!(read_json_file(Some(p)).is_none());
    }

    fn codex_mcp_item(name: &str) -> InstalledItem {
        InstalledItem {
            name: name.to_string(),
            version: None,
            agent: AgentKind::Codex,
            kind: ItemKind::McpServer,
            source: None,
        }
    }

    #[test]
    fn parse_codex_mcp_servers_stdio_entries() {
        let toml = r#"
model = "gpt-5.4"

[mcp_servers.fetch]
command = "uvx"
args = ["mcp-server-fetch"]

[mcp_servers.puppeteer]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-puppeteer"]
"#;
        let mut items = parse_codex_mcp_servers(toml);
        items.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], codex_mcp_item("fetch"));
        assert_eq!(items[1], codex_mcp_item("puppeteer"));
    }

    #[test]
    fn parse_codex_mcp_servers_http_entry() {
        let toml = r#"
[mcp_servers.microsoftdocs]
url = "https://learn.microsoft.com/api/mcp"
"#;
        let items = parse_codex_mcp_servers(toml);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0], codex_mcp_item("microsoftdocs"));
    }

    #[test]
    fn parse_codex_mcp_servers_no_mcp_section_returns_empty() {
        let toml = r#"
model = "gpt-5.4"
[projects.'C:\Users\user']
trust_level = "trusted"
"#;
        assert_eq!(parse_codex_mcp_servers(toml), vec![]);
    }

    #[test]
    fn parse_codex_mcp_servers_bad_toml_returns_empty() {
        assert_eq!(parse_codex_mcp_servers("not valid { toml [["), vec![]);
    }

    // ── adopt_items tests ─────────────────────────────────────────────────────

    fn make_claude_plugin(name: &str) -> InstalledItem {
        InstalledItem {
            name: name.to_string(),
            version: Some("1.0.0".to_string()),
            agent: AgentKind::Claude,
            kind: ItemKind::Plugin,
            source: None,
        }
    }

    fn make_claude_mcp(name: &str) -> InstalledItem {
        InstalledItem {
            name: name.to_string(),
            version: None,
            agent: AgentKind::Claude,
            kind: ItemKind::McpServer,
            source: None,
        }
    }

    fn make_codex_mcp(name: &str) -> InstalledItem {
        InstalledItem {
            name: name.to_string(),
            version: None,
            agent: AgentKind::Codex,
            kind: ItemKind::McpServer,
            source: None,
        }
    }

    #[test]
    fn adopt_items_writes_new_entries_to_empty_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        let items = vec![
            make_claude_plugin("my-plugin@mkt"),
            make_claude_mcp("memory"),
        ];
        let adopted = adopt_items(items, &lock).unwrap();
        assert_eq!(adopted.len(), 2);
        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
        let arr = content
            .get("_adoptedItems")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn adopt_items_skips_already_in_resolved_plugins() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        std::fs::write(&lock, r#"{"resolvedPlugins":[{"pluginId":"ccync@ccync"}]}"#).unwrap();
        let items = vec![
            make_claude_plugin("ccync@ccync"),
            make_claude_plugin("other@mkt"),
        ];
        let adopted = adopt_items(items, &lock).unwrap();
        assert_eq!(adopted.len(), 1);
        assert_eq!(adopted[0].name, "other@mkt");
        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
        let arr = content
            .get("_adoptedItems")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "other@mkt");
    }

    #[test]
    fn adopt_items_skips_already_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        std::fs::write(&lock, r#"{"_adoptedItems":[{"name":"memory","adoptedFrom":"claude","version":null,"sourceId":"claude-mcp"}]}"#).unwrap();
        let items = vec![make_claude_mcp("memory"), make_claude_mcp("new-server")];
        let adopted = adopt_items(items, &lock).unwrap();
        assert_eq!(adopted.len(), 1);
        assert_eq!(adopted[0].name, "new-server");
        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
        let arr = content
            .get("_adoptedItems")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(arr.len(), 2); // old + new
    }

    #[test]
    fn adopt_items_preserves_existing_lock_namespaces() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        std::fs::write(
            &lock,
            r#"{"_ccyncProjection":{"foo":"bar"},"resolvedPlugins":[]}"#,
        )
        .unwrap();
        let items = vec![make_codex_mcp("fetch")];
        adopt_items(items, &lock).unwrap();
        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
        assert!(
            content.get("_ccyncProjection").is_some(),
            "_ccyncProjection must be preserved"
        );
        assert!(
            content.get("resolvedPlugins").is_some(),
            "resolvedPlugins must be preserved"
        );
        assert!(
            content.get("_adoptedItems").is_some(),
            "_adoptedItems must be written"
        );
    }

    #[test]
    fn adopt_items_tags_source_id_and_agent_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        let items = vec![
            make_claude_plugin("plug@mkt"),
            make_claude_mcp("srv"),
            make_codex_mcp("cdx-srv"),
        ];
        adopt_items(items, &lock).unwrap();
        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
        let arr = content["_adoptedItems"].as_array().unwrap();
        let plug = arr.iter().find(|e| e["name"] == "plug@mkt").unwrap();
        assert_eq!(plug["adoptedFrom"], "claude");
        assert_eq!(plug["sourceId"], "claude-plugin");
        assert_eq!(plug["version"], "1.0.0");
        let srv = arr.iter().find(|e| e["name"] == "srv").unwrap();
        assert_eq!(srv["sourceId"], "claude-mcp");
        let cdx = arr.iter().find(|e| e["name"] == "cdx-srv").unwrap();
        assert_eq!(cdx["adoptedFrom"], "codex");
        assert_eq!(cdx["sourceId"], "codex-mcp");
    }

    #[test]
    fn adopt_items_no_write_when_all_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        std::fs::write(&lock, r#"{"_adoptedItems":[{"name":"memory","adoptedFrom":"claude","version":null,"sourceId":"claude-mcp"}]}"#).unwrap();
        let mtime_before = std::fs::metadata(&lock).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        adopt_items(vec![make_claude_mcp("memory")], &lock).unwrap();
        let mtime_after = std::fs::metadata(&lock).unwrap().modified().unwrap();
        assert_eq!(
            mtime_before, mtime_after,
            "lock must not be rewritten when nothing new is adopted"
        );
    }

    #[test]
    fn adopt_items_absent_lock_no_error() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("state").join("plugins.lock.json");
        let adopted = adopt_items(vec![make_claude_plugin("x@mkt")], &lock).unwrap();
        assert_eq!(adopted.len(), 1);
    }

    #[test]
    fn enumerate_skill_dirs_returns_skill_items() {
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path().join("skills");
        // two real skills (dir + SKILL.md) and one decoy (no SKILL.md)
        for name in ["doc-sync", "git-commits"] {
            let s = skills.join(name);
            std::fs::create_dir_all(&s).unwrap();
            std::fs::write(s.join("SKILL.md"), format!("# {name}")).unwrap();
        }
        std::fs::create_dir_all(skills.join("incomplete")).unwrap(); // no SKILL.md
        let items = enumerate_skill_dirs(&skills, AgentKind::Claude);
        assert_eq!(items.len(), 2, "only dirs with SKILL.md count");
        assert_eq!(
            items[0],
            InstalledItem {
                name: "doc-sync".to_string(),
                version: None,
                agent: AgentKind::Claude,
                kind: ItemKind::Skill,
                source: None,
            }
        );
        assert_eq!(items[1].name, "git-commits");
        assert!(items.iter().all(|i| i.kind == ItemKind::Skill));
    }

    #[test]
    fn enumerate_skill_dirs_absent_root_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(enumerate_skill_dirs(&dir.path().join("nope"), AgentKind::Codex).is_empty());
    }

    #[test]
    fn skill_source_id_tags() {
        let claude_skill = InstalledItem {
            name: "s".into(),
            version: None,
            agent: AgentKind::Claude,
            kind: ItemKind::Skill,
            source: None,
        };
        let codex_skill = InstalledItem {
            name: "s".into(),
            version: None,
            agent: AgentKind::Codex,
            kind: ItemKind::Skill,
            source: None,
        };
        assert_eq!(source_id(&claude_skill), "claude-skill");
        assert_eq!(source_id(&codex_skill), "codex-skill");
    }

    #[test]
    fn codex_reader_does_not_read_plugins_section() {
        let toml = r#"
[plugins."documents@openai-primary-runtime"]
enabled = true

[mcp_servers.fetch]
command = "uvx"
args = ["mcp-server-fetch"]
"#;
        let items = parse_codex_mcp_servers(toml);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0], codex_mcp_item("fetch"));
    }

    // ── source-reference derivation ───────────────────────────────────────────

    #[test]
    fn derive_plugin_source_marketplace_and_version() {
        assert_eq!(
            derive_plugin_source("tool@market", Some("2.1.0")),
            "market:tool@2.1.0"
        );
        assert_eq!(
            derive_plugin_source("ccync@ccync", Some("1.0.1")),
            "ccync:ccync@1.0.1"
        );
    }

    #[test]
    fn derive_plugin_source_no_version() {
        assert_eq!(derive_plugin_source("tool@market", None), "market:tool");
    }

    #[test]
    fn derive_plugin_source_no_marketplace() {
        assert_eq!(derive_plugin_source("bare", Some("0.1.0")), "bare@0.1.0");
        assert_eq!(derive_plugin_source("bare", None), "bare");
    }

    #[test]
    fn parsed_plugin_carries_derived_source() {
        let raw = json!({
            "version": 2,
            "plugins": { "tool@market": [{ "version": "2.1.0" }] }
        });
        let items = parse_installed_plugins(raw);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source.as_deref(), Some("market:tool@2.1.0"));
    }

    #[test]
    fn adopted_plugin_entry_carries_source_ref() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        let raw = json!({
            "version": 2,
            "plugins": { "tool@market": [{ "version": "2.1.0" }] }
        });
        let items = parse_installed_plugins(raw);
        adopt_items(items, &lock).unwrap();
        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
        let arr = content["_adoptedItems"].as_array().unwrap();
        let entry = arr.iter().find(|e| e["name"] == "tool@market").unwrap();
        assert_eq!(entry["source"], "market:tool@2.1.0");
    }

    #[test]
    fn adopted_mcp_entry_has_no_source_key() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        adopt_items(vec![make_claude_mcp("memory")], &lock).unwrap();
        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
        let arr = content["_adoptedItems"].as_array().unwrap();
        let entry = arr.iter().find(|e| e["name"] == "memory").unwrap();
        assert!(
            entry.get("source").is_none(),
            "MCP entry must not carry a source ref"
        );
    }

    // ── full MCP definition snapshot (_mcpServers) ─────────────────────────────

    #[test]
    fn parse_claude_mcp_defs_captures_full_definition() {
        let raw = json!({
            "mcpServers": {
                "memory": {
                    "type": "stdio",
                    "command": "npx",
                    "args": ["-y", "@mcp/server-memory"],
                    "env": { "API_KEY": "${MEMORY_KEY}" }
                }
            }
        });
        let store = parse_claude_mcp_defs(&raw);
        let v = serde_json::to_value(&store).unwrap();
        assert_eq!(v["memory"]["command"], "npx");
        assert_eq!(v["memory"]["args"][1], "@mcp/server-memory");
        assert_eq!(v["memory"]["env"]["API_KEY"], "${MEMORY_KEY}");
    }

    #[test]
    fn parse_codex_mcp_defs_captures_full_definition() {
        let toml = r#"
[mcp_servers.fetch]
command = "uvx"
args = ["mcp-server-fetch"]

[mcp_servers.fetch.env]
TOKEN = "${FETCH_TOKEN}"
"#;
        let store = parse_codex_mcp_defs(toml);
        let v = serde_json::to_value(&store).unwrap();
        assert_eq!(v["fetch"]["command"], "uvx");
        assert_eq!(v["fetch"]["args"][0], "mcp-server-fetch");
        assert_eq!(v["fetch"]["env"]["TOKEN"], "${FETCH_TOKEN}");
    }

    #[test]
    fn adopt_mcp_definitions_writes_full_def_to_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        let raw = json!({
            "mcpServers": {
                "memory": { "command": "npx", "args": ["-y", "srv"], "env": { "K": "v" } }
            }
        });
        let store = parse_claude_mcp_defs(&raw);
        adopt_mcp_definitions(store, &lock).unwrap();
        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
        let def = &content["_mcpServers"]["memory"];
        assert_eq!(def["command"], "npx");
        assert_eq!(def["args"][1], "srv");
        assert_eq!(def["env"]["K"], "v");
    }

    #[test]
    fn adopt_mcp_definitions_preserves_other_namespaces() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        std::fs::write(
            &lock,
            r#"{"_ccyncProjection":{"x":1},"resolvedPlugins":[]}"#,
        )
        .unwrap();
        let mut store = McpServerStore::new();
        store.insert(
            "memory".to_string(),
            McpServer {
                server_type: None,
                command: Some("npx".into()),
                args: None,
                env: None,
                url: None,
                headers: None,
            },
        );
        adopt_mcp_definitions(store, &lock).unwrap();
        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
        assert!(content.get("_ccyncProjection").is_some());
        assert!(content.get("resolvedPlugins").is_some());
        assert_eq!(content["_mcpServers"]["memory"]["command"], "npx");
    }

    #[test]
    fn adopt_mcp_definitions_empty_store_no_write() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        adopt_mcp_definitions(McpServerStore::new(), &lock).unwrap();
        assert!(!lock.exists(), "empty store must not create the lock file");
    }

    // ── loose-skill marking (_looseSkills) ─────────────────────────────────────

    fn skill_item(name: &str, agent: AgentKind) -> InstalledItem {
        InstalledItem {
            name: name.to_string(),
            version: None,
            agent,
            kind: ItemKind::Skill,
            source: None,
        }
    }

    #[test]
    fn mark_loose_skills_excludes_plugin_owned() {
        let items = vec![
            skill_item("doc-sync", AgentKind::Claude),
            skill_item("bundled-skill", AgentKind::Claude),
            make_claude_mcp("memory"), // non-skill ignored
        ];
        let owned: HashSet<String> = ["bundled-skill".to_string()].into_iter().collect();
        let loose = mark_loose_skills(&items, &owned);
        assert_eq!(loose.len(), 1);
        assert_eq!(loose[0].name, "doc-sync");
    }

    #[test]
    fn mark_loose_skills_dedups_across_agents() {
        let items = vec![
            skill_item("doc-sync", AgentKind::Claude),
            skill_item("doc-sync", AgentKind::Codex),
        ];
        let loose = mark_loose_skills(&items, &HashSet::new());
        assert_eq!(
            loose.len(),
            1,
            "same skill name from two agents marked once"
        );
    }

    #[test]
    fn adopt_loose_skills_writes_marker_and_preserves_namespaces() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        std::fs::write(&lock, r#"{"_ccyncProjection":{"x":1}}"#).unwrap();
        let items = vec![
            skill_item("doc-sync", AgentKind::Claude),
            skill_item("owned", AgentKind::Claude),
        ];
        let owned: HashSet<String> = ["owned".to_string()].into_iter().collect();
        let loose = adopt_loose_skills(&items, &owned, &lock).unwrap();
        assert_eq!(loose.len(), 1);
        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
        assert!(
            content.get("_ccyncProjection").is_some(),
            "namespace preserved"
        );
        let arr = content["_looseSkills"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "doc-sync");
    }

    #[test]
    fn adopt_loose_skills_nothing_to_mark_no_write() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        let owned: HashSet<String> = ["only-skill".to_string()].into_iter().collect();
        let loose = adopt_loose_skills(
            &[skill_item("only-skill", AgentKind::Claude)],
            &owned,
            &lock,
        )
        .unwrap();
        assert!(loose.is_empty());
        assert!(!lock.exists(), "no loose skills → no lock write");
    }
}
