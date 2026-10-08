use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;

use ccync_foundation::mcp::McpServer;
use ccync_foundation::paths::{
    claude_config_path, claude_installed_plugins_path, claude_known_marketplaces_path,
    claude_mcp_json_path, claude_skills_path, codex_config_path, shared_agents_skills_path,
    user_home,
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
    /// Resolvable origin reference for re-fetch. Set for marketplace
    /// plugins when the marketplace's local clone catalog resolves this
    /// plugin's entry; `None` for MCP/skill items and for plugins whose
    /// source could not be resolved (unknown marketplace, missing local
    /// clone, plugin absent from the catalog) — unresolved remains absent,
    /// never fabricated.
    pub source: Option<String>,
    /// The version recorded for this plugin in the marketplace catalog at
    /// adopt time and kept as separate version evidence for future upgrade comparison, distinct
    /// from `version` (the currently-installed version). `None` when the
    /// plugin's source could not be resolved, or for MCP/skill items.
    pub baseline_version: Option<String>,
}

// ── vendor-native ownership boundary ──────────────────────────────────────────

/// Codex names that are the vendor's own native artifacts, never ccync-managed
/// even if they surface in a reader's raw listing.
pub(crate) const CODEX_VENDOR_NATIVE_NAMES: &[&str] = &["node_repl"];

/// Claude names that are the vendor's own native artifacts. Empty as of the
/// 2026-08-05 survey — Claude Code ships no vendor-native MCP server/skill/plugin
/// name that collides with the adopt readers. Kept as an explicit empty slice
/// (not omitted) so the selector's `match` stays exhaustive and this arm reads
/// as surveyed, not forgotten.
pub(crate) const CLAUDE_VENDOR_NATIVE_NAMES: &[&str] = &[];

/// Select the vendor-native name list to exclude from adopt-eligible items for
/// `agent`. Exhaustive `match`, no catch-all arm — adding a new `AgentKind`
/// variant forces this selector to be updated rather than silently defaulting.
pub(crate) fn vendor_native_names(agent: &AgentKind) -> &'static [&'static str] {
    match agent {
        AgentKind::Claude => CLAUDE_VENDOR_NATIVE_NAMES,
        AgentKind::Codex => CODEX_VENDOR_NATIVE_NAMES,
    }
}

/// Items eligible for adopt from a master agent's install surface, plus the
/// names excluded at the read boundary because they matched that agent's
/// [`vendor_native_names`] list. An excluded name never appears in `items`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AdoptableState {
    pub items: Vec<InstalledItem>,
    pub skipped_vendor_native: Vec<String>,
}

/// Split `items` into adopt-eligible items and the names skipped as
/// vendor-native for `agent`, per [`vendor_native_names`].
fn filter_vendor_native(items: Vec<InstalledItem>, agent: &AgentKind) -> AdoptableState {
    let vendor_names = vendor_native_names(agent);
    if vendor_names.is_empty() {
        return AdoptableState {
            items,
            skipped_vendor_native: Vec::new(),
        };
    }
    let mut kept = Vec::with_capacity(items.len());
    let mut skipped = Vec::new();
    for item in items {
        if vendor_names.contains(&item.name.as_str()) {
            skipped.push(item.name);
        } else {
            kept.push(item);
        }
    }
    AdoptableState {
        items: kept,
        skipped_vendor_native: skipped,
    }
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

/// Parse `installed_plugins.json` v2 into [`InstalledItem`]s, resolving each
/// plugin's real marketplace source and separately recorded baseline version.
///
/// Each key is `"<plugin>@<marketplace>"`. The marketplace's repo source is
/// read once from `known_marketplaces.json` (hoisted out of the per-item
/// loop), then joined with the marketplace's local clone catalog to resolve
/// this specific plugin's entry. Resolution failure (unknown marketplace,
/// missing local clone, plugin absent from the catalog) leaves `source` and
/// `baseline_version` truthfully `None` — never fabricated. When the plugin
/// resolves but the catalog entry carries no `version`, `baseline_version`
/// falls back to the currently-installed `version`; if both are
/// absent, `baseline_version` stays `None`.
fn parse_installed_plugins(raw: Value) -> Vec<InstalledItem> {
    let parsed: InstalledPluginsV2 = match serde_json::from_value(raw) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let known_marketplaces = read_known_marketplaces();
    parsed
        .plugins
        .into_iter()
        .map(|(name, records)| {
            let version = records.into_iter().find_map(|r| r.version);
            let (plugin_name, marketplace) = match name.split_once('@') {
                Some((n, m)) => (n, Some(m)),
                None => (name.as_str(), None),
            };
            let resolved = marketplace.and_then(|m| {
                let repo = known_marketplaces.get(m);
                read_marketplace_plugin_source(m, plugin_name, repo)
            });
            let (source, baseline_version) = match resolved {
                Some(r) => (r.source, r.version.or_else(|| version.clone())),
                None => (None, None),
            };
            InstalledItem {
                name,
                version,
                agent: AgentKind::Claude,
                kind: ItemKind::Plugin,
                source,
                baseline_version,
            }
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

/// Capture the FULL Codex MCP server definitions from `~/.codex/config.toml`
/// `[mcp_servers.*]`. Codex spells remote-server headers `http_headers`; they
/// are carried into the portable `headers` field. Graceful-missing: an
/// unparseable config yields an empty store, and an entry that fails to
/// deserialize into [`McpServer`] is skipped.
fn parse_codex_mcp_defs(text: &str) -> McpServerStore {
    let mut store = McpServerStore::new();
    let Ok(root) = toml::from_str::<toml::Table>(text) else {
        return store;
    };
    let Some(servers) = root.get("mcp_servers").and_then(|v| v.as_table()) else {
        return store;
    };
    for (name, def) in servers {
        let mut def = def.clone();
        if let Some(table) = def.as_table_mut() {
            if !table.contains_key("headers") {
                if let Some(http_headers) = table.remove("http_headers") {
                    table.insert("headers".to_string(), http_headers);
                }
            }
        }
        if let Ok(server) = def.try_into::<McpServer>() {
            store.insert(name.clone(), server);
        }
    }
    store
}

// ── marketplace source resolution ────────────────────────────────────────────

/// A marketplace's own repo source, as recorded in `known_marketplaces.json`.
///
/// This is the marketplace's clone URL (or local path), not any one plugin's
/// source — it is the base ccync joins with a plugin's in-repo path for the
/// embedded-path case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceRepoSource {
    /// Direct clonable reference when known (e.g. a git URL). `None` when the
    /// marketplace's own `source` shape is unrecognized/local-only.
    pub url: Option<String>,
}

/// Resolved source information for one plugin inside a marketplace catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPluginSource {
    /// The version recorded for this plugin in the marketplace catalog.
    pub version: Option<String>,
    /// Resolvable source reference:
    /// - external git source → the direct URL.
    /// - marketplace-embedded/in-repo path plugin → the marketplace repo URL
    ///   plus the plugin's subpath (when the marketplace repo URL is known).
    pub source: Option<String>,
    /// `true` when `source` points at an in-repo subpath of a local marketplace
    /// repository rather than a directly-clonable external or remote-selector
    /// reference.
    pub not_directly_clonable: bool,
}

/// Parse `known_marketplaces.json` into a marketplace-name → repo-source map.
///
/// Graceful-missing: an absent file, unreadable file, or malformed JSON yields
/// an empty map — never a panic. The real Claude Code file is flat: each
/// top-level key is a marketplace name, mapping directly to its entry object
/// (no `"marketplaces"` wrapper). Unrecognized `source` shapes (e.g.
/// `{"type":"local"}`) resolve to `url: None` rather than being dropped, so
/// callers can still see the marketplace is known. A non-object entry value
/// is skipped.
pub fn parse_known_marketplaces(raw: &Value) -> HashMap<String, MarketplaceRepoSource> {
    let mut out = HashMap::new();
    let Some(map) = raw.as_object() else {
        return out;
    };
    for (name, entry) in map {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let url = marketplace_source_url(entry.get("source"));
        out.insert(name.clone(), MarketplaceRepoSource { url });
    }
    out
}

/// Read + parse `known_marketplaces.json` from its default Claude location.
/// Graceful-missing: absent/unreadable/malformed → empty map.
pub fn read_known_marketplaces() -> HashMap<String, MarketplaceRepoSource> {
    match read_json_file(claude_known_marketplaces_path()) {
        Some(raw) => parse_known_marketplaces(&raw),
        None => HashMap::new(),
    }
}

/// Extract a directly-clonable URL from a marketplace's `source` field.
///
/// Recognized shapes:
/// - a bare string (`"source": "owner/repo"` or a full URL) — passed through
///   unchanged.
/// - an object carrying a full URL under `url`/`repo`/`git` (any value
///   containing `://`) — passed through unchanged.
/// - a GitHub-slug object, `{"source":"github","repo":"owner/repo"}` — the
///   `"source"` discriminator selects the provider and `"repo"` supplies the
///   slug, normalized to `https://github.com/<repo>`.
///
/// An unrecognized discriminator, an empty/non-string `repo` slug, or an
/// object with none of the above shapes resolves to `None` — never a
/// fabricated URL.
fn marketplace_source_url(source: Option<&Value>) -> Option<String> {
    match source? {
        Value::String(s) => Some(s.clone()),
        Value::Object(map) => {
            if let Some(direct) = map
                .get("url")
                .or_else(|| map.get("repo"))
                .or_else(|| map.get("git"))
                .and_then(Value::as_str)
            {
                if direct.contains("://") {
                    return Some(direct.to_string());
                }
            }
            match map.get("source").and_then(Value::as_str) {
                Some("github") => {
                    let slug = map.get("repo").and_then(Value::as_str)?;
                    if slug.is_empty() {
                        None
                    } else {
                        Some(format!("https://github.com/{slug}"))
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Parse a marketplace's local clone catalog (`.claude-plugin/marketplace.json`)
/// looking for one plugin's entry.
///
/// Graceful-missing/graceful-not-found: missing file, malformed JSON, or the
/// plugin not present in the catalog's `plugins` array all resolve to `None` —
/// never a panic.
///
/// Source resolution:
/// - the plugin entry's own `source` is an external git reference (string
///   containing a URL scheme, e.g. `https://...`) → used directly,
///   `not_directly_clonable: false`.
/// - the plugin entry's `source` is an in-repo relative path (`"./x"`,
///   `"../x"`, `"x/y"` with no scheme, or `{"path": "..."}`) → joined onto
///   `marketplace_repo.url` (when known), using `//` for remote URLs and `/`
///   otherwise. Any `..`/`.`/empty path segments are sanitized out first.
///   Remote selector sources are directly clonable. A non-empty path joined to
///   a local repository is marked `not_directly_clonable: true`; an empty
///   sanitized subpath is directly clonable. When the marketplace repo URL
///   itself is unknown, `source` stays `None` (never fabricate a URL).
pub fn resolve_plugin_source_in_catalog(
    catalog: &Value,
    plugin_name: &str,
    marketplace_repo: Option<&MarketplaceRepoSource>,
) -> Option<ResolvedPluginSource> {
    let plugins = catalog.get("plugins").and_then(Value::as_array)?;
    let entry = plugins
        .iter()
        .find(|p| p.get("name").and_then(Value::as_str) == Some(plugin_name))?;

    let version = entry
        .get("version")
        .and_then(Value::as_str)
        .map(str::to_string);

    let source_field = entry.get("source");
    let (subpath, is_embedded) = match source_field {
        Some(Value::String(s)) => (Some(s.clone()), looks_like_in_repo_path(s)),
        Some(Value::Object(map)) => (
            map.get("path").and_then(Value::as_str).map(str::to_string),
            true,
        ),
        _ => (None, false),
    };

    let (source, not_directly_clonable) = match (is_embedded, subpath) {
        (false, Some(direct_url)) => (Some(direct_url), false),
        (true, Some(rel_path)) => {
            let repo_url = marketplace_repo.and_then(|r| r.url.as_deref());
            match repo_url {
                Some(repo_url) => {
                    let remote = crate::install::is_remote_url(repo_url);
                    let source =
                        join_repo_subpath(repo_url, &rel_path, if remote { "//" } else { "/" });
                    let sanitized_subpath = source != repo_url.trim_end_matches('/');
                    (Some(source), !remote && sanitized_subpath)
                }
                None => (None, true),
            }
        }
        _ => (None, false),
    };

    Some(ResolvedPluginSource {
        version,
        source,
        not_directly_clonable,
    })
}

/// Heuristic: does this `source` string look like an in-repo relative path
/// (embedded plugin) rather than a directly-clonable external reference?
///
/// A string is external (not in-repo) only when it carries a URL scheme
/// (contains `://`) — e.g. `https://...`, `git://...`, `ssh://...`. Everything
/// else — `./x`, `../x`, and a bare no-scheme relative path like `plugins/beta`
/// with no `./` prefix — is treated as an in-repo relative path to be joined
/// onto the marketplace repo URL. (Note: this is distinct from
/// `marketplace_source_url`, which resolves a marketplace's own top-level
/// `source` field in `known_marketplaces.json` — a bare `owner/repo` string
/// there is a directly-clonable shorthand, not a plugin-catalog subpath.)
fn looks_like_in_repo_path(s: &str) -> bool {
    !s.contains("://")
}

/// Join a marketplace repo URL with a plugin's in-repo relative subpath.
///
/// Sanitizes `rel_path` against path traversal: splits on `/`, drops any
/// `..`, `.`, and empty segments, then rejoins. This guarantees the joined
/// result never escapes the repo URL's own path space, even if a malicious
/// or misconfigured `marketplace.json` supplies a `../`-laden subpath. If
/// sanitization empties the subpath entirely, the repo URL itself is
/// returned unchanged.
fn join_repo_subpath(repo_url: &str, rel_path: &str, separator: &str) -> String {
    let trimmed_repo = repo_url.trim_end_matches('/');
    let safe_rel: Vec<&str> = rel_path
        .split('/')
        .filter(|seg| !seg.is_empty() && *seg != "." && *seg != "..")
        .collect();
    if safe_rel.is_empty() {
        return trimmed_repo.to_string();
    }
    format!("{trimmed_repo}{separator}{}", safe_rel.join("/"))
}

/// Read a marketplace's local clone catalog
/// (`~/.claude/plugins/marketplaces/<name>/.claude-plugin/marketplace.json`)
/// and resolve one plugin's source/version entry.
///
/// Graceful-missing: absent marketplaces root, absent marketplace clone, or
/// malformed/missing catalog all resolve to `None` — never a panic.
pub fn read_marketplace_plugin_source(
    marketplace_name: &str,
    plugin_name: &str,
    marketplace_repo: Option<&MarketplaceRepoSource>,
) -> Option<ResolvedPluginSource> {
    let root = claude_marketplaces_root()?;
    let catalog_path = root
        .join(marketplace_name)
        .join(".claude-plugin")
        .join("marketplace.json");
    let raw = read_json_file(Some(catalog_path))?;
    resolve_plugin_source_in_catalog(&raw, plugin_name, marketplace_repo)
}

/// `~/.claude/plugins/marketplaces` — root of Claude's local marketplace clones.
fn claude_marketplaces_root() -> Option<std::path::PathBuf> {
    user_home().map(|h| h.join(".claude").join("plugins").join("marketplaces"))
}

// ── public API ────────────────────────────────────────────────────────────────

/// Read Claude's existing install state from three files, minus items on
/// [`CLAUDE_VENDOR_NATIVE_NAMES`].
///
/// Graceful-missing: any absent or unparseable file contributes an empty set,
/// no error is returned. Unknown schema versions degrade silently.
pub fn read_claude_adoptable_state() -> AdoptableState {
    let mut items: Vec<InstalledItem> = Vec::new();

    if let Some(raw) = read_json_file(claude_installed_plugins_path()) {
        items.extend(parse_installed_plugins(raw));
    }

    items.extend(
        read_claude_mcp_definitions()
            .into_keys()
            .map(|name| InstalledItem {
                name,
                version: None,
                agent: AgentKind::Claude,
                kind: ItemKind::McpServer,
                source: None,
                baseline_version: None,
            }),
    );

    if let Some(skills_dir) = claude_skills_path() {
        items.extend(enumerate_skill_dirs(&skills_dir, AgentKind::Claude));
    }

    filter_vendor_native(items, &AgentKind::Claude)
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
            baseline_version: None,
        })
        .collect()
}

/// Read Codex's existing MCP install state from `~/.codex/config.toml`, minus
/// items on [`CODEX_VENDOR_NATIVE_NAMES`].
///
/// Only the `mcp_servers` table is read; Codex has no native plugin concept.
/// Graceful-missing: absent or unparseable file → empty set, no error.
pub fn read_codex_adoptable_state() -> AdoptableState {
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

    filter_vendor_native(items, &AgentKind::Codex)
}

/// The one Claude MCP definition reader: read `claude_config_path()`
/// (`~/.claude.json`) first, then fill in only names missing from that store
/// using `claude_mcp_json_path()` (`~/.claude/.mcp.json`). `~/.claude.json`
/// wins any name collision. Graceful-missing: an absent/unparseable file
/// contributes nothing to the merge.
fn read_claude_mcp_definitions() -> McpServerStore {
    let mut store = read_json_file(claude_config_path())
        .map(|raw| parse_claude_mcp_defs(&raw))
        .unwrap_or_default();
    if let Some(raw) = read_json_file(claude_mcp_json_path()) {
        for (name, server) in parse_claude_mcp_defs(&raw) {
            store.entry(name).or_insert(server);
        }
    }
    store
}

/// The FULL MCP server definitions eligible for snapshotting into
/// `_mcpServers`, plus the names excluded at the read boundary because they
/// matched the agent's [`vendor_native_names`] list. An excluded name never
/// appears in `store`.
#[derive(Debug, Clone, Default)]
pub struct AdoptableMcpDefinitions {
    pub store: McpServerStore,
    pub skipped_vendor_native: Vec<String>,
}

/// Split `store` into adopt-eligible definitions and the names skipped as
/// vendor-native for `agent`, per [`vendor_native_names`].
fn filter_vendor_native_mcp_defs(
    store: McpServerStore,
    agent: &AgentKind,
) -> AdoptableMcpDefinitions {
    let vendor_names = vendor_native_names(agent);
    if vendor_names.is_empty() {
        return AdoptableMcpDefinitions {
            store,
            skipped_vendor_native: Vec::new(),
        };
    }
    let mut kept = McpServerStore::new();
    let mut skipped = Vec::new();
    for (name, def) in store {
        if vendor_names.contains(&name.as_str()) {
            skipped.push(name);
        } else {
            kept.insert(name, def);
        }
    }
    AdoptableMcpDefinitions {
        store: kept,
        skipped_vendor_native: skipped,
    }
}

/// Read the FULL MCP server definitions from a master agent for snapshotting
/// into `_mcpServers`, minus definitions on the agent's [`vendor_native_names`]
/// list. Graceful-missing: absent/unparseable config → empty store.
pub fn read_mcp_definitions(agent: &AgentKind) -> AdoptableMcpDefinitions {
    let store = match agent {
        AgentKind::Claude => read_claude_mcp_definitions(),
        AgentKind::Codex => codex_config_path()
            .and_then(|p| std::fs::read_to_string(&p).ok())
            .map(|text| parse_codex_mcp_defs(&text))
            .unwrap_or_default(),
    };
    filter_vendor_native_mcp_defs(store, agent)
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
                    baseline_version: None,
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
        if let Some(baseline) = &item.baseline_version {
            entry.insert("baselineVersion".to_string(), json!(baseline));
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

// ── remove adopted item ───────────────────────────────────────────────────────

/// Remove one adopted item by name from the lock file.
///
/// Clears the entry from `_adoptedItems`, and also from `_looseSkills`
/// (skill items) or `_mcpServers` (MCP server items) when the `sourceId`
/// field indicates the appropriate kind. All other namespaces are preserved.
///
/// Returns `Ok(true)` when an entry was found and removed, `Ok(false)` when
/// no entry with the given name exists in `_adoptedItems`.
pub fn remove_adopted_item(id: &str, lock_path: &Path) -> Result<bool, AdoptError> {
    let lock = read_lock_file(lock_path)?;

    let mut map = match lock {
        Value::Object(m) => m,
        _ => return Ok(false),
    };

    let adopted_arr = map
        .get(ADOPT_KEY)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut source_id_of_removed: Option<String> = None;
    let mut remaining: Vec<Value> = Vec::with_capacity(adopted_arr.len());
    for entry in adopted_arr {
        if source_id_of_removed.is_none() && entry.get("name").and_then(Value::as_str) == Some(id) {
            source_id_of_removed = entry
                .get("sourceId")
                .and_then(Value::as_str)
                .map(str::to_string);
        } else {
            remaining.push(entry);
        }
    }

    if source_id_of_removed.is_none() {
        return Ok(false);
    }

    let sid = source_id_of_removed.as_deref().unwrap_or("");
    let is_mcp = sid.contains("mcp");
    let is_skill = sid.contains("skill");

    map.insert(ADOPT_KEY.to_string(), Value::Array(remaining));

    if is_skill {
        let loose = map
            .get(LOOSE_SKILLS_KEY)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let pruned: Vec<Value> = loose
            .into_iter()
            .filter(|e| e.get("name").and_then(Value::as_str) != Some(id))
            .collect();
        map.insert(LOOSE_SKILLS_KEY.to_string(), Value::Array(pruned));
    }

    if is_mcp {
        if let Some(servers) = map.get_mut(MCP_SERVERS_KEY).and_then(Value::as_object_mut) {
            servers.remove(id);
        }
    }

    let updated = Value::Object(map);
    let text =
        serde_json::to_string_pretty(&updated).map_err(|e| AdoptError::Write(e.to_string()))?;
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| AdoptError::Write(e.to_string()))?;
    }
    std::fs::write(lock_path, text).map_err(|e| AdoptError::Write(e.to_string()))?;

    Ok(true)
}

// ── repair (adopted-source lookup/write seams) ─────────────────────────────────

/// Enumerate the names of adopted items with no `source` recorded — the
/// `ccync repair` no-argument scope.
pub fn adopted_items_without_source(lock_path: &Path) -> Result<Vec<String>, AdoptError> {
    let lock = read_lock_file(lock_path)?;
    let names = lock
        .get(ADOPT_KEY)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|e| {
                    e.get("source")
                        .and_then(Value::as_str)
                        .is_none_or(str::is_empty)
                })
                .filter_map(|e| e.get("name").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(names)
}

/// Look up one adopted row by name, regardless of whether it already has a
/// source — the seam behind `ccync repair <id>` explicit overwrite (may
/// overwrite any adopted source, not just an unresolved one).
pub fn find_adopted_item(name: &str, lock_path: &Path) -> Result<Option<Value>, AdoptError> {
    let lock = read_lock_file(lock_path)?;
    Ok(lock
        .get(ADOPT_KEY)
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .find(|e| e.get("name").and_then(Value::as_str) == Some(name))
        })
        .cloned())
}

/// Write a new `source` for the uniquely-named adopted row `name`.
///
/// Returns `Ok(true)` when written, `Ok(false)` when `name` matches no
/// adopted row. Fails closed (`Err`) when more than one row shares `name` —
/// never guesses which one to update. All other lock namespaces are
/// preserved via a `Value` round-trip. A no-write path (unknown name,
/// duplicate names) never opens the file for writing, so it stays
/// byte-identical. The actual write goes through
/// `ccync_foundation::platform::atomic_write_bytes`.
pub fn write_adopted_source(
    name: &str,
    source: &str,
    lock_path: &Path,
) -> Result<bool, AdoptError> {
    let lock = read_lock_file(lock_path)?;
    let mut map = match lock {
        Value::Object(m) => m,
        _ => return Ok(false),
    };
    let mut adopted_arr = map
        .get(ADOPT_KEY)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let matches: Vec<usize> = adopted_arr
        .iter()
        .enumerate()
        .filter(|(_, e)| e.get("name").and_then(Value::as_str) == Some(name))
        .map(|(i, _)| i)
        .collect();

    let idx = match matches.as_slice() {
        [] => return Ok(false),
        [idx] => *idx,
        _ => {
            return Err(AdoptError::Write(format!(
                "duplicate adopted name {name:?}: {} rows, refusing to guess which to update",
                matches.len()
            )))
        }
    };

    let Value::Object(entry) = &mut adopted_arr[idx] else {
        return Err(AdoptError::Write(format!(
            "adopted row {name:?} is not a JSON object"
        )));
    };
    entry.insert("source".to_string(), json!(source));

    map.insert(ADOPT_KEY.to_string(), Value::Array(adopted_arr));
    let updated = Value::Object(map);
    let text = serde_json::to_vec_pretty(&updated).map_err(|e| AdoptError::Write(e.to_string()))?;
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| AdoptError::Write(e.to_string()))?;
    }
    ccync_foundation::platform::atomic_write_bytes(lock_path, &text)
        .map_err(|e| AdoptError::Write(e.to_string()))?;

    Ok(true)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plugin_item_unresolved(name: &str, version: &str) -> InstalledItem {
        InstalledItem {
            name: name.to_string(),
            version: Some(version.to_string()),
            agent: AgentKind::Claude,
            kind: ItemKind::Plugin,
            source: None,
            baseline_version: None,
        }
    }

    // `parse_installed_plugins` resolves each plugin's real marketplace
    // source; with no fake home / no known_marketplaces.json / no local
    // marketplace clone present, resolution always misses and these tests
    // exercise the "unresolved remains absent" path.
    #[test]
    fn parse_installed_plugins_v2_real_schema() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
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
        assert_eq!(items[0], plugin_item_unresolved("ccync@ccync", "1.0.1"));
        assert_eq!(items[1], plugin_item_unresolved("other@mkt", "0.5.0"));
    }

    #[test]
    fn parse_installed_plugins_empty_plugins_map() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        let raw = json!({ "version": 2, "plugins": {} });
        assert_eq!(parse_installed_plugins(raw), vec![]);
    }

    #[test]
    fn parse_installed_plugins_no_version_field() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
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
    fn read_claude_adoptable_state_derives_mcp_items_from_unified_reader_keys() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        let claude_dir = tmp.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(
            tmp.path().join(".claude.json"),
            json!({ "mcpServers": { "memory": { "command": "npx" } } }).to_string(),
        )
        .unwrap();
        std::fs::write(
            claude_dir.join(".mcp.json"),
            json!({ "mcpServers": { "memory": { "command": "should-not-win" }, "codebase": { "command": "node" } } })
                .to_string(),
        )
        .unwrap();
        let state = read_claude_adoptable_state();
        let mut mcp_names: Vec<&str> = state
            .items
            .iter()
            .filter(|i| i.kind == ItemKind::McpServer)
            .map(|i| i.name.as_str())
            .collect();
        mcp_names.sort_unstable();
        assert_eq!(
            mcp_names,
            vec!["codebase", "memory"],
            "state names must equal the unified reader's store keys"
        );
    }

    #[test]
    fn read_mcp_definitions_codex_omits_vendor_native_and_reports_it_skipped() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        let codex_dir = tmp.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("config.toml"),
            r#"
[mcp_servers.node_repl]
command = "node"

[mcp_servers.fetch]
command = "uvx"
args = ["mcp-server-fetch"]
"#,
        )
        .unwrap();
        let defs = read_mcp_definitions(&AgentKind::Codex);
        assert_eq!(
            defs.store.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["fetch"],
            "vendor-native server must be absent from the store"
        );
        assert_eq!(defs.skipped_vendor_native, vec!["node_repl".to_string()]);
    }

    #[test]
    fn read_mcp_definitions_claude_keeps_every_server_empty_vendor_native_list() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        std::fs::write(
            tmp.path().join(".claude.json"),
            json!({ "mcpServers": { "memory": { "command": "npx" } } }).to_string(),
        )
        .unwrap();
        let defs = read_mcp_definitions(&AgentKind::Claude);
        assert_eq!(
            defs.store.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["memory"]
        );
        assert!(defs.skipped_vendor_native.is_empty());
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
            baseline_version: None,
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
            baseline_version: None,
        }
    }

    fn make_claude_mcp(name: &str) -> InstalledItem {
        InstalledItem {
            name: name.to_string(),
            version: None,
            agent: AgentKind::Claude,
            kind: ItemKind::McpServer,
            source: None,
            baseline_version: None,
        }
    }

    fn make_codex_mcp(name: &str) -> InstalledItem {
        InstalledItem {
            name: name.to_string(),
            version: None,
            agent: AgentKind::Codex,
            kind: ItemKind::McpServer,
            source: None,
            baseline_version: None,
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
        let lock = dir.path().join("build").join("lock.json");
        let adopted = adopt_items(vec![make_claude_plugin("x@mkt")], &lock).unwrap();
        assert_eq!(adopted.len(), 1);
    }

    #[test]
    fn adopt_items_installer_evidence_row_lands_only_in_adopted_items() {
        // Locks class-2 (installer-evidence: version and source both present)
        // behavior: the row reaches `_adoptedItems` only, carrying all four
        // provenance fields, and nothing else is materialized to disk.
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        let item = InstalledItem {
            name: "mkt-plugin@mkt".to_string(),
            version: Some("2.3.4".to_string()),
            agent: AgentKind::Claude,
            kind: ItemKind::Plugin,
            source: Some("https://github.com/owner/repo".to_string()),
            baseline_version: Some("2.0.0".to_string()),
        };
        let adopted = adopt_items(vec![item], &lock).unwrap();
        assert_eq!(adopted.len(), 1);

        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();

        // Reaches `_adoptedItems` with all four provenance fields intact.
        let arr = content["_adoptedItems"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let entry = &arr[0];
        assert_eq!(entry["name"], "mkt-plugin@mkt");
        assert_eq!(entry["adoptedFrom"], "claude");
        assert_eq!(entry["sourceId"], "claude-plugin");
        assert_eq!(entry["source"], "https://github.com/owner/repo");
        assert_eq!(entry["baselineVersion"], "2.0.0");
        assert_eq!(entry["version"], "2.3.4");

        // Reaches no other namespace: the lock file's top-level object holds
        // only `_adoptedItems` — `adopt_items` never touches `_mcpServers`,
        // `_looseSkills`, `resolvedPlugins`, or any other key.
        let map = content.as_object().unwrap();
        assert_eq!(
            map.keys().collect::<Vec<_>>(),
            vec!["_adoptedItems"],
            "adopt_items must write only the _adoptedItems namespace"
        );

        // Nothing materialized: no other file was written under the lock's
        // parent directory beyond the lock file itself.
        let siblings: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            siblings,
            vec![lock.file_name().unwrap().to_os_string()],
            "adopt_items must not materialize any file beyond the lock file"
        );
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
                baseline_version: None,
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
            baseline_version: None,
        };
        let codex_skill = InstalledItem {
            name: "s".into(),
            version: None,
            agent: AgentKind::Codex,
            kind: ItemKind::Skill,
            source: None,
            baseline_version: None,
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

    // ── resolved marketplace source + baseline version ─────────────────────────

    /// Write a `known_marketplaces.json` + a local marketplace clone catalog
    /// under `home` so `parse_installed_plugins` can resolve real sources.
    fn write_marketplace_fixture(
        home: &Path,
        marketplace: &str,
        repo_url: &str,
        plugin_name: &str,
        plugin_version: &str,
        plugin_source: &str,
    ) {
        let plugins_dir = home.join(".claude").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("known_marketplaces.json"),
            json!({ marketplace: { "source": repo_url } }).to_string(),
        )
        .unwrap();
        let catalog_dir = plugins_dir
            .join("marketplaces")
            .join(marketplace)
            .join(".claude-plugin");
        std::fs::create_dir_all(&catalog_dir).unwrap();
        std::fs::write(
            catalog_dir.join("marketplace.json"),
            json!({
                "plugins": [
                    { "name": plugin_name, "version": plugin_version, "source": plugin_source }
                ]
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn parsed_plugin_resolves_real_source_and_baseline_version() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        write_marketplace_fixture(
            tmp.path(),
            "market",
            "https://github.com/example/market.git",
            "tool",
            "2.1.0",
            "https://github.com/example/tool-plugin.git",
        );
        let raw = json!({
            "version": 2,
            "plugins": { "tool@market": [{ "version": "2.0.0" }] }
        });
        let items = parse_installed_plugins(raw);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].version.as_deref(),
            Some("2.0.0"),
            "installed version"
        );
        assert_eq!(
            items[0].source.as_deref(),
            Some("https://github.com/example/tool-plugin.git"),
            "real resolved marketplace source, not a fabricated label"
        );
        assert_eq!(
            items[0].baseline_version.as_deref(),
            Some("2.1.0"),
            "baseline version comes from the catalog entry, may differ from installed version"
        );
    }

    #[test]
    fn parsed_plugin_missing_catalog_version_falls_back_to_installed_version() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        let plugins_dir = tmp.path().join(".claude").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("known_marketplaces.json"),
            json!({ "mkt": { "source": "https://github.com/example/mkt.git" } }).to_string(),
        )
        .unwrap();
        let catalog_dir = plugins_dir
            .join("marketplaces")
            .join("mkt")
            .join(".claude-plugin");
        std::fs::create_dir_all(&catalog_dir).unwrap();
        // Catalog entry resolves but carries no `version` field.
        std::fs::write(
            catalog_dir.join("marketplace.json"),
            json!({
                "plugins": [
                    { "name": "tool", "source": "https://github.com/example/tool.git" }
                ]
            })
            .to_string(),
        )
        .unwrap();
        let raw = json!({
            "version": 2,
            "plugins": { "tool@mkt": [{ "version": "1.5.0" }] }
        });
        let items = parse_installed_plugins(raw);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].baseline_version.as_deref(),
            Some("1.5.0"),
            "catalog has no version: baseline falls back to installed version"
        );
    }

    #[test]
    fn parsed_plugin_missing_catalog_and_installed_version_baseline_absent() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        let plugins_dir = tmp.path().join(".claude").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("known_marketplaces.json"),
            json!({ "mkt": { "source": "https://github.com/example/mkt.git" } }).to_string(),
        )
        .unwrap();
        let catalog_dir = plugins_dir
            .join("marketplaces")
            .join("mkt")
            .join(".claude-plugin");
        std::fs::create_dir_all(&catalog_dir).unwrap();
        // Catalog entry resolves but carries no `version` field.
        std::fs::write(
            catalog_dir.join("marketplace.json"),
            json!({
                "plugins": [
                    { "name": "tool", "source": "https://github.com/example/tool.git" }
                ]
            })
            .to_string(),
        )
        .unwrap();
        // Installed record also carries no `version`.
        let raw = json!({
            "version": 2,
            "plugins": { "tool@mkt": [{ "version": null }] }
        });
        let items = parse_installed_plugins(raw);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].baseline_version, None,
            "both catalog and installed version absent: baseline stays None, never fabricated"
        );
    }

    #[test]
    fn parsed_plugin_unknown_marketplace_source_absent() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        // No known_marketplaces.json / no local clone at all.
        let raw = json!({
            "version": 2,
            "plugins": { "tool@nope": [{ "version": "1.0.0" }] }
        });
        let items = parse_installed_plugins(raw);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].source, None,
            "unresolved marketplace: source absent"
        );
        assert_eq!(items[0].baseline_version, None);
    }

    #[test]
    fn parsed_plugin_not_in_catalog_source_absent() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        write_marketplace_fixture(
            tmp.path(),
            "market",
            "https://github.com/example/market.git",
            "other-plugin",
            "9.9.9",
            "https://github.com/example/other.git",
        );
        let raw = json!({
            "version": 2,
            "plugins": { "tool@market": [{ "version": "1.0.0" }] }
        });
        let items = parse_installed_plugins(raw);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].source, None,
            "plugin absent from catalog: source absent, never fabricated"
        );
        assert_eq!(items[0].baseline_version, None);
    }

    #[test]
    fn parsed_plugin_bare_name_no_marketplace_source_absent() {
        // A key with no `@` at all (bare plugin name, no marketplace) must
        // not panic on split_once and must resolve to source/baseline absent,
        // since there is no marketplace to consult.
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        let raw = json!({
            "version": 2,
            "plugins": { "standalone-tool": [{ "version": "1.0.0" }] }
        });
        let items = parse_installed_plugins(raw);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "standalone-tool");
        assert_eq!(
            items[0].source, None,
            "bare plugin name with no marketplace: source absent"
        );
        assert_eq!(items[0].baseline_version, None);
    }

    #[test]
    fn parsed_plugin_catalog_entry_missing_source_field_still_sets_baseline_version() {
        // Catalog entry has a `version` but no (or malformed) `source` field.
        // `baseline_version` and `source` do NOT always travel together:
        // `ResolvedPluginSource` carries them as independent fields, and
        // `parse_installed_plugins` destructures both from the same `Some(r)`
        // arm — so a resolved catalog hit with a missing source must still
        // populate baseline_version while leaving source absent.
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        let plugins_dir = tmp.path().join(".claude").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("known_marketplaces.json"),
            json!({ "market": { "source": "https://github.com/example/market.git" } }).to_string(),
        )
        .unwrap();
        let catalog_dir = plugins_dir
            .join("marketplaces")
            .join("market")
            .join(".claude-plugin");
        std::fs::create_dir_all(&catalog_dir).unwrap();
        // Note: no "source" field on the plugin entry at all.
        std::fs::write(
            catalog_dir.join("marketplace.json"),
            json!({
                "plugins": [
                    { "name": "tool", "version": "3.0.0" }
                ]
            })
            .to_string(),
        )
        .unwrap();
        let raw = json!({
            "version": 2,
            "plugins": { "tool@market": [{ "version": "2.5.0" }] }
        });
        let items = parse_installed_plugins(raw);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].source, None,
            "malformed/missing catalog source field must stay absent"
        );
        assert_eq!(
            items[0].baseline_version.as_deref(),
            Some("3.0.0"),
            "baseline_version resolves independently of source when the catalog hit \
             has a version but no usable source field"
        );
    }

    #[test]
    fn adopted_entry_baseline_version_present_without_source_key() {
        // Verify the independence survives all the way into the written lock
        // entry: `baselineVersion` key present, `source` key absent.
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        let plugins_dir = tmp.path().join(".claude").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("known_marketplaces.json"),
            json!({ "market": { "source": "https://github.com/example/market.git" } }).to_string(),
        )
        .unwrap();
        let catalog_dir = plugins_dir
            .join("marketplaces")
            .join("market")
            .join(".claude-plugin");
        std::fs::create_dir_all(&catalog_dir).unwrap();
        std::fs::write(
            catalog_dir.join("marketplace.json"),
            json!({ "plugins": [ { "name": "tool", "version": "3.0.0" } ] }).to_string(),
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        let raw = json!({
            "version": 2,
            "plugins": { "tool@market": [{ "version": "2.5.0" }] }
        });
        let items = parse_installed_plugins(raw);
        adopt_items(items, &lock).unwrap();
        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
        let arr = content["_adoptedItems"].as_array().unwrap();
        let entry = arr.iter().find(|e| e["name"] == "tool@market").unwrap();
        assert!(
            entry.get("source").is_none(),
            "source must stay absent when the catalog entry has no usable source field"
        );
        assert_eq!(
            entry["baselineVersion"], "3.0.0",
            "baselineVersion must still be written even though source is absent"
        );
    }

    #[test]
    fn adopted_plugin_entry_carries_resolved_source_and_baseline_version() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        write_marketplace_fixture(
            tmp.path(),
            "market",
            "https://github.com/example/market.git",
            "tool",
            "2.1.0",
            "https://github.com/example/tool-plugin.git",
        );
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        let raw = json!({
            "version": 2,
            "plugins": { "tool@market": [{ "version": "2.0.0" }] }
        });
        let items = parse_installed_plugins(raw);
        adopt_items(items, &lock).unwrap();
        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
        let arr = content["_adoptedItems"].as_array().unwrap();
        let entry = arr.iter().find(|e| e["name"] == "tool@market").unwrap();
        assert_eq!(
            entry["source"],
            "https://github.com/example/tool-plugin.git"
        );
        assert_eq!(entry["version"], "2.0.0", "currently-installed version");
        assert_eq!(
            entry["baselineVersion"], "2.1.0",
            "catalog-reported version recorded as baseline for later upgrade comparison"
        );
    }

    #[test]
    fn adopted_plugin_entry_unresolved_omits_source_and_baseline_version() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        // No known_marketplaces.json / no local clone.
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        let raw = json!({
            "version": 2,
            "plugins": { "tool@nope": [{ "version": "1.0.0" }] }
        });
        let items = parse_installed_plugins(raw);
        adopt_items(items, &lock).unwrap();
        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
        let arr = content["_adoptedItems"].as_array().unwrap();
        let entry = arr.iter().find(|e| e["name"] == "tool@nope").unwrap();
        assert!(
            entry.get("source").is_none(),
            "unresolved source must be absent, not null/fake"
        );
        assert!(
            entry.get("baselineVersion").is_none(),
            "unresolved baseline version must be absent, not null/fake"
        );
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
        assert!(
            entry.get("baselineVersion").is_none(),
            "MCP entry must not carry a baseline version"
        );
    }

    #[test]
    fn adopted_skill_entry_has_no_source_or_baseline_version() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        let skill = InstalledItem {
            name: "doc-sync".to_string(),
            version: None,
            agent: AgentKind::Claude,
            kind: ItemKind::Skill,
            source: None,
            baseline_version: None,
        };
        adopt_items(vec![skill], &lock).unwrap();
        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
        let arr = content["_adoptedItems"].as_array().unwrap();
        let entry = arr.iter().find(|e| e["name"] == "doc-sync").unwrap();
        assert!(entry.get("source").is_none());
        assert!(entry.get("baselineVersion").is_none());
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
    fn adopt_codex_cwd_captured() {
        let toml = r#"
[mcp_servers.tool]
command = "node"
cwd = "/path/to/workdir"
"#;
        let store = parse_codex_mcp_defs(toml);
        let server = store.get("tool").unwrap();
        assert_eq!(server.cwd.as_deref(), Some("/path/to/workdir"));
    }

    #[test]
    fn parse_codex_mcp_defs_maps_http_headers_to_headers() {
        let toml = r#"
[mcp_servers.context7]
url = "https://mcp.context7.com/mcp"

[mcp_servers.context7.http_headers]
CONTEXT7_API_KEY = "${CONTEXT7_API_KEY}"
"#;
        let store = parse_codex_mcp_defs(toml);
        let server = store.get("context7").unwrap();
        assert_eq!(server.url.as_deref(), Some("https://mcp.context7.com/mcp"));
        assert_eq!(
            server.headers.as_ref().unwrap()["CONTEXT7_API_KEY"],
            "${CONTEXT7_API_KEY}"
        );
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
                cwd: None,
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
            baseline_version: None,
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

    // ── marketplace source resolution ───────────────────────────────────────────

    fn marketplace_catalog_fixture(entries: Vec<Value>) -> Value {
        json!({ "plugins": entries })
    }

    #[test]
    fn parse_known_marketplaces_resolves_string_and_object_sources() {
        let raw = json!({
            "official": { "source": "https://github.com/example/official.git" },
            "team": { "source": { "type": "git", "url": "https://github.com/example/team.git" } },
            "local-only": { "source": { "type": "local" } }
        });
        let mkts = parse_known_marketplaces(&raw);
        assert_eq!(mkts.len(), 3);
        assert_eq!(
            mkts["official"].url.as_deref(),
            Some("https://github.com/example/official.git")
        );
        assert_eq!(
            mkts["team"].url.as_deref(),
            Some("https://github.com/example/team.git")
        );
        assert_eq!(
            mkts["local-only"].url, None,
            "unrecognized source shape resolves to None, not dropped"
        );
    }

    #[test]
    fn parse_known_marketplaces_non_object_entry_skipped() {
        let raw = json!({ "weird": "not-an-object" });
        assert!(parse_known_marketplaces(&raw).is_empty());
    }

    #[test]
    fn parse_known_marketplaces_malformed_json_shape_returns_empty_no_panic() {
        let raw = json!("not an object at all");
        assert!(parse_known_marketplaces(&raw).is_empty());
    }

    #[test]
    fn parse_known_marketplaces_empty_object_returns_empty() {
        let raw = json!({});
        assert!(parse_known_marketplaces(&raw).is_empty());
    }

    #[test]
    fn resolve_plugin_source_external_git_uses_direct_url() {
        let catalog = marketplace_catalog_fixture(vec![json!({
            "name": "alpha",
            "version": "1.2.0",
            "source": "https://github.com/example/alpha-plugin.git"
        })]);
        let resolved = resolve_plugin_source_in_catalog(&catalog, "alpha", None)
            .expect("plugin entry must resolve");
        assert_eq!(resolved.version.as_deref(), Some("1.2.0"));
        assert_eq!(
            resolved.source.as_deref(),
            Some("https://github.com/example/alpha-plugin.git")
        );
        assert!(
            !resolved.not_directly_clonable,
            "external git source is directly clonable"
        );
    }

    #[test]
    fn resolve_plugin_source_embedded_path_joins_marketplace_repo_url() {
        let catalog = marketplace_catalog_fixture(vec![json!({
            "name": "beta",
            "version": "0.3.0",
            "source": "./plugins/beta"
        })]);
        let repo = MarketplaceRepoSource {
            url: Some("https://github.com/example/marketplace".to_string()),
        };
        let resolved = resolve_plugin_source_in_catalog(&catalog, "beta", Some(&repo))
            .expect("plugin entry must resolve");
        assert_eq!(resolved.version.as_deref(), Some("0.3.0"));
        assert_eq!(
            resolved.source.as_deref(),
            Some("https://github.com/example/marketplace//plugins/beta")
        );
        assert!(
            !resolved.not_directly_clonable,
            "remote embedded plugin uses a directly clonable selector source"
        );
    }

    #[test]
    fn resolve_plugin_source_embedded_path_object_shape_joins_repo_url() {
        let catalog = marketplace_catalog_fixture(vec![json!({
            "name": "gamma",
            "version": "2.0.0",
            "source": { "path": "gamma-plugin" }
        })]);
        let repo = MarketplaceRepoSource {
            url: Some("https://github.com/example/marketplace".to_string()),
        };
        let resolved = resolve_plugin_source_in_catalog(&catalog, "gamma", Some(&repo))
            .expect("plugin entry must resolve");
        assert_eq!(
            resolved.source.as_deref(),
            Some("https://github.com/example/marketplace//gamma-plugin")
        );
        assert!(!resolved.not_directly_clonable);
    }

    #[test]
    fn resolve_plugin_source_embedded_path_unknown_repo_url_stays_none() {
        let catalog = marketplace_catalog_fixture(vec![json!({
            "name": "delta",
            "version": "0.1.0",
            "source": "./delta"
        })]);
        // marketplace repo unknown (e.g. known_marketplaces.json missing/local-only)
        let resolved = resolve_plugin_source_in_catalog(&catalog, "delta", None)
            .expect("plugin entry must resolve even without a repo url");
        assert_eq!(resolved.version.as_deref(), Some("0.1.0"));
        assert_eq!(
            resolved.source, None,
            "must never fabricate a URL when the marketplace repo url is unknown"
        );
        assert!(resolved.not_directly_clonable);
    }

    #[test]
    fn resolve_plugin_source_embedded_path_local_repo_url_is_not_directly_clonable() {
        let catalog = marketplace_catalog_fixture(vec![json!({
            "name": "beta",
            "version": "0.3.0",
            "source": "./plugins/beta"
        })]);
        let repo = MarketplaceRepoSource {
            url: Some("/local/path/marketplace".to_string()),
        };
        let resolved = resolve_plugin_source_in_catalog(&catalog, "beta", Some(&repo))
            .expect("plugin entry must resolve");
        assert_eq!(
            resolved.source.as_deref(),
            Some("/local/path/marketplace/plugins/beta")
        );
        assert!(
            resolved.not_directly_clonable,
            "local marketplace embedded path is not directly clonable"
        );
    }

    #[test]
    fn resolve_plugin_source_embedded_root_path_remote_repo_url_resolves_to_repo_url() {
        let catalog = marketplace_catalog_fixture(vec![json!({
            "name": "root-plugin",
            "version": "1.0.0",
            "source": "./"
        })]);
        let repo = MarketplaceRepoSource {
            url: Some("https://github.com/example/marketplace".to_string()),
        };
        let resolved = resolve_plugin_source_in_catalog(&catalog, "root-plugin", Some(&repo))
            .expect("plugin entry must resolve");
        assert_eq!(
            resolved.source.as_deref(),
            Some("https://github.com/example/marketplace")
        );
        assert!(
            !resolved.not_directly_clonable,
            "remote repo root is directly clonable"
        );
    }

    #[test]
    fn resolve_plugin_source_bare_relative_path_no_dot_prefix_classifies_as_embedded() {
        // No `./` prefix, no scheme — must still classify as in-repo,
        // not fall through to "external direct URL".
        let catalog = marketplace_catalog_fixture(vec![json!({
            "name": "epsilon",
            "version": "0.5.0",
            "source": "plugins/beta"
        })]);
        let repo = MarketplaceRepoSource {
            url: Some("https://github.com/example/marketplace".to_string()),
        };
        let resolved = resolve_plugin_source_in_catalog(&catalog, "epsilon", Some(&repo))
            .expect("plugin entry must resolve");
        assert_eq!(
            resolved.source.as_deref(),
            Some("https://github.com/example/marketplace//plugins/beta")
        );
        assert!(
            !resolved.not_directly_clonable,
            "remote embedded plugin uses a directly clonable selector source"
        );
    }

    #[test]
    fn resolve_plugin_source_path_traversal_is_sanitized() {
        // A malicious/misconfigured `../`-laden subpath must never escape the
        // repo URL's own path space.
        let catalog = marketplace_catalog_fixture(vec![json!({
            "name": "zeta",
            "version": "0.1.0",
            "source": "../../etc/passwd"
        })]);
        let repo = MarketplaceRepoSource {
            url: Some("https://github.com/example/marketplace".to_string()),
        };
        let resolved = resolve_plugin_source_in_catalog(&catalog, "zeta", Some(&repo))
            .expect("plugin entry must resolve");
        assert_eq!(
            resolved.source.as_deref(),
            Some("https://github.com/example/marketplace//etc/passwd"),
            "'..' segments must be stripped, never letting the subpath escape the repo url"
        );
        assert!(!resolved.not_directly_clonable);
    }

    #[test]
    fn join_repo_subpath_all_traversal_segments_yields_bare_repo_url() {
        // If sanitization empties the subpath entirely, fall back to the repo
        // URL itself rather than fabricating a trailing slash or panicking.
        assert_eq!(
            join_repo_subpath("https://github.com/example/marketplace", "../..", "/"),
            "https://github.com/example/marketplace"
        );
        assert_eq!(
            join_repo_subpath("https://github.com/example/marketplace/", "./.", "/"),
            "https://github.com/example/marketplace"
        );
    }

    #[test]
    fn resolve_plugin_source_plugin_not_found_returns_none() {
        let catalog = marketplace_catalog_fixture(vec![json!({
            "name": "alpha",
            "version": "1.0.0",
            "source": "https://github.com/example/alpha.git"
        })]);
        assert!(resolve_plugin_source_in_catalog(&catalog, "missing-plugin", None).is_none());
    }

    #[test]
    fn resolve_plugin_source_missing_plugins_array_returns_none_no_panic() {
        let catalog = json!({ "unexpected": "shape" });
        assert!(resolve_plugin_source_in_catalog(&catalog, "alpha", None).is_none());
    }

    #[test]
    fn resolve_plugin_source_malformed_catalog_shape_returns_none_no_panic() {
        let catalog = json!("not an object");
        assert!(resolve_plugin_source_in_catalog(&catalog, "alpha", None).is_none());
    }

    #[test]
    fn resolve_plugin_source_entries_missing_name_field_are_skipped_no_panic() {
        // Entries lacking a `name` field must never match by accident (e.g. via
        // a default/None comparison) and must not panic while scanning.
        let catalog = marketplace_catalog_fixture(vec![
            json!({ "version": "9.9.9", "source": "https://github.com/example/noname.git" }),
            json!({ "name": "alpha", "version": "1.0.0", "source": "https://github.com/example/alpha.git" }),
        ]);
        let resolved = resolve_plugin_source_in_catalog(&catalog, "alpha", None)
            .expect("named entry after an unnamed one must still resolve");
        assert_eq!(resolved.version.as_deref(), Some("1.0.0"));

        // And a lookup that would only match the unnamed entry must be None.
        assert!(resolve_plugin_source_in_catalog(&catalog, "", None).is_none());
    }

    #[test]
    fn resolve_plugin_source_plugins_key_not_array_returns_none_no_panic() {
        let catalog = json!({ "plugins": "not-an-array" });
        assert!(resolve_plugin_source_in_catalog(&catalog, "alpha", None).is_none());

        let catalog = json!({ "plugins": { "alpha": {} } });
        assert!(resolve_plugin_source_in_catalog(&catalog, "alpha", None).is_none());
    }

    #[test]
    fn resolve_plugin_source_empty_plugins_array_returns_none() {
        let catalog = marketplace_catalog_fixture(vec![]);
        assert!(resolve_plugin_source_in_catalog(&catalog, "alpha", None).is_none());
    }

    #[test]
    fn marketplace_source_object_without_recognized_keys_resolves_to_none() {
        let raw = json!({
            "weird": { "source": { "type": "git", "branch": "main" } }
        });
        let mkts = parse_known_marketplaces(&raw);
        assert_eq!(
            mkts["weird"].url, None,
            "object source without url/repo/git keys must resolve to None, not panic or fabricate"
        );
    }

    #[test]
    fn marketplace_source_github_slug_normalizes_to_https() {
        let raw = json!({
            "mkt": { "source": { "source": "github", "repo": "example/tool" } }
        });
        let mkts = parse_known_marketplaces(&raw);
        assert_eq!(
            mkts["mkt"].url.as_deref(),
            Some("https://github.com/example/tool")
        );
    }

    #[test]
    fn marketplace_source_url_containing_scheme_passes_through_unchanged() {
        let raw = json!({
            "mkt": { "source": { "source": "github", "url": "https://example.com/already/full" } }
        });
        let mkts = parse_known_marketplaces(&raw);
        assert_eq!(
            mkts["mkt"].url.as_deref(),
            Some("https://example.com/already/full"),
            "already-full URL must pass through unchanged, not be re-normalized"
        );
    }

    #[test]
    fn marketplace_source_unknown_discriminator_resolves_to_none() {
        let raw = json!({
            "mkt": { "source": { "source": "gitlab", "repo": "example/tool" } }
        });
        let mkts = parse_known_marketplaces(&raw);
        assert_eq!(
            mkts["mkt"].url, None,
            "unknown discriminator must never be fabricated into a URL"
        );
    }

    #[test]
    fn marketplace_source_github_empty_slug_resolves_to_none() {
        let raw = json!({
            "mkt": { "source": { "source": "github", "repo": "" } }
        });
        let mkts = parse_known_marketplaces(&raw);
        assert_eq!(mkts["mkt"].url, None);
    }

    #[test]
    fn marketplace_source_github_non_string_repo_resolves_to_none() {
        let raw = json!({
            "mkt": { "source": { "source": "github", "repo": 42 } }
        });
        let mkts = parse_known_marketplaces(&raw);
        assert_eq!(mkts["mkt"].url, None);
    }

    #[test]
    fn resolve_plugin_source_no_version_field() {
        let catalog = marketplace_catalog_fixture(vec![json!({
            "name": "alpha",
            "source": "https://github.com/example/alpha.git"
        })]);
        let resolved = resolve_plugin_source_in_catalog(&catalog, "alpha", None).unwrap();
        assert_eq!(resolved.version, None);
    }

    /// Serialize tests that mutate process-global env vars (`USERPROFILE`/`HOME`).
    /// Shared crate-wide lock — see `crate::test_support::env_guard` for the
    /// full rationale (must be shared, not a per-module copy, since env
    /// mutation is process-wide and `cargo test` runs multithreaded).
    use crate::test_support::env_guard;

    fn set_home(path: &Path) {
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", path);
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", path);
        }
    }

    #[test]
    fn read_known_marketplaces_missing_file_returns_empty_no_panic() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        // No known_marketplaces.json written under this fake home at all.
        assert!(read_known_marketplaces().is_empty());
    }

    #[test]
    fn read_known_marketplaces_malformed_json_returns_empty_no_panic() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        let path = tmp.path().join(".claude").join("plugins");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("known_marketplaces.json"), "not valid json {{{").unwrap();
        assert!(read_known_marketplaces().is_empty());
    }

    #[test]
    fn read_known_marketplaces_real_fixture_resolves_url() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        let path = tmp.path().join(".claude").join("plugins");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            path.join("known_marketplaces.json"),
            json!({
                "official": { "source": "https://github.com/example/official.git" }
            })
            .to_string(),
        )
        .unwrap();
        let mkts = read_known_marketplaces();
        assert_eq!(
            mkts["official"].url.as_deref(),
            Some("https://github.com/example/official.git")
        );
    }

    #[test]
    fn read_marketplace_plugin_source_missing_marketplace_dir_returns_none() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        // No ~/.claude/plugins/marketplaces/<name> clone exists under this fake home.
        let result =
            read_marketplace_plugin_source("definitely-nonexistent-marketplace-xyz", "alpha", None);
        assert!(result.is_none());
    }

    #[test]
    fn read_marketplace_plugin_source_malformed_catalog_returns_none_no_panic() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        let catalog_dir = tmp
            .path()
            .join(".claude")
            .join("plugins")
            .join("marketplaces")
            .join("official")
            .join(".claude-plugin");
        std::fs::create_dir_all(&catalog_dir).unwrap();
        std::fs::write(catalog_dir.join("marketplace.json"), "{ not json").unwrap();
        let result = read_marketplace_plugin_source("official", "alpha", None);
        assert!(result.is_none());
    }

    #[test]
    fn read_marketplace_plugin_source_real_fixture_resolves_external_git() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        let catalog_dir = tmp
            .path()
            .join(".claude")
            .join("plugins")
            .join("marketplaces")
            .join("official")
            .join(".claude-plugin");
        std::fs::create_dir_all(&catalog_dir).unwrap();
        std::fs::write(
            catalog_dir.join("marketplace.json"),
            json!({
                "plugins": [
                    {
                        "name": "alpha",
                        "version": "1.2.0",
                        "source": "https://github.com/example/alpha-plugin.git"
                    }
                ]
            })
            .to_string(),
        )
        .unwrap();
        let resolved = read_marketplace_plugin_source("official", "alpha", None)
            .expect("fixture plugin must resolve");
        assert_eq!(resolved.version.as_deref(), Some("1.2.0"));
        assert_eq!(
            resolved.source.as_deref(),
            Some("https://github.com/example/alpha-plugin.git")
        );
        assert!(!resolved.not_directly_clonable);
    }

    #[test]
    fn read_marketplace_plugin_source_real_fixture_resolves_embedded_path() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        let catalog_dir = tmp
            .path()
            .join(".claude")
            .join("plugins")
            .join("marketplaces")
            .join("official")
            .join(".claude-plugin");
        std::fs::create_dir_all(&catalog_dir).unwrap();
        std::fs::write(
            catalog_dir.join("marketplace.json"),
            json!({
                "plugins": [
                    { "name": "beta", "version": "0.3.0", "source": "./plugins/beta" }
                ]
            })
            .to_string(),
        )
        .unwrap();
        let repo = MarketplaceRepoSource {
            url: Some("https://github.com/example/marketplace".to_string()),
        };
        let resolved = read_marketplace_plugin_source("official", "beta", Some(&repo))
            .expect("fixture plugin must resolve");
        assert_eq!(
            resolved.source.as_deref(),
            Some("https://github.com/example/marketplace//plugins/beta")
        );
        assert!(!resolved.not_directly_clonable);
    }

    #[test]
    fn read_marketplace_plugin_source_plugin_not_in_fixture_returns_none() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        let catalog_dir = tmp
            .path()
            .join(".claude")
            .join("plugins")
            .join("marketplaces")
            .join("official")
            .join(".claude-plugin");
        std::fs::create_dir_all(&catalog_dir).unwrap();
        std::fs::write(
            catalog_dir.join("marketplace.json"),
            json!({
                "plugins": [
                    { "name": "alpha", "version": "1.0.0", "source": "https://github.com/example/alpha.git" }
                ]
            })
            .to_string(),
        )
        .unwrap();
        let result = read_marketplace_plugin_source("official", "not-there", None);
        assert!(result.is_none());
    }

    // ── remove_adopted_item ───────────────────────────────────────────────────

    fn write_lock(dir: &std::path::Path, val: &Value) -> std::path::PathBuf {
        let p = dir.join("lock.json");
        std::fs::write(&p, serde_json::to_string_pretty(val).unwrap()).unwrap();
        p
    }

    #[test]
    fn remove_adopted_plugin_clears_adopted_items() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = write_lock(
            tmp.path(),
            &json!({
                "_adoptedItems": [
                    { "name": "tool-a", "sourceId": "claude-plugin", "adoptedFrom": "claude" },
                    { "name": "tool-b", "sourceId": "claude-plugin", "adoptedFrom": "claude" }
                ],
                "_personalPlugins": []
            }),
        );
        let removed = remove_adopted_item("tool-a", &lock_path).unwrap();
        assert!(removed);
        let updated: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock_path).unwrap()).unwrap();
        let arr = updated["_adoptedItems"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "tool-b");
        // Other namespaces untouched.
        assert_eq!(updated["_personalPlugins"], json!([]));
    }

    #[test]
    fn remove_adopted_skill_clears_loose_skills_too() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = write_lock(
            tmp.path(),
            &json!({
                "_adoptedItems": [
                    { "name": "my-skill", "sourceId": "claude-skill", "adoptedFrom": "claude" }
                ],
                "_looseSkills": [
                    { "name": "my-skill" },
                    { "name": "other-skill" }
                ]
            }),
        );
        let removed = remove_adopted_item("my-skill", &lock_path).unwrap();
        assert!(removed);
        let updated: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock_path).unwrap()).unwrap();
        assert_eq!(updated["_adoptedItems"].as_array().unwrap().len(), 0);
        let loose = updated["_looseSkills"].as_array().unwrap();
        assert_eq!(loose.len(), 1);
        assert_eq!(loose[0]["name"], "other-skill");
    }

    #[test]
    fn remove_adopted_mcp_clears_mcp_servers_too() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = write_lock(
            tmp.path(),
            &json!({
                "_adoptedItems": [
                    { "name": "my-mcp", "sourceId": "claude-mcp", "adoptedFrom": "claude" }
                ],
                "_mcpServers": {
                    "my-mcp": { "type": "stdio", "command": "npx" },
                    "other-mcp": { "type": "stdio", "command": "uvx" }
                }
            }),
        );
        let removed = remove_adopted_item("my-mcp", &lock_path).unwrap();
        assert!(removed);
        let updated: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock_path).unwrap()).unwrap();
        assert_eq!(updated["_adoptedItems"].as_array().unwrap().len(), 0);
        let servers = updated["_mcpServers"].as_object().unwrap();
        assert!(!servers.contains_key("my-mcp"));
        assert!(servers.contains_key("other-mcp"));
    }

    #[test]
    fn remove_adopted_not_found_returns_false() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = write_lock(
            tmp.path(),
            &json!({
                "_adoptedItems": [
                    { "name": "existing", "sourceId": "claude-plugin", "adoptedFrom": "claude" }
                ]
            }),
        );
        let removed = remove_adopted_item("ghost", &lock_path).unwrap();
        assert!(!removed);
        // File not rewritten — existing entry still present.
        let updated: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock_path).unwrap()).unwrap();
        assert_eq!(updated["_adoptedItems"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn remove_adopted_preserves_unrelated_namespaces() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = write_lock(
            tmp.path(),
            &json!({
                "_adoptedItems": [
                    { "name": "to-remove", "sourceId": "claude-plugin", "adoptedFrom": "claude" }
                ],
                "_personalPlugins": [{ "pluginId": "my-plugin", "pin": "abc123" }],
                "schemaVersion": 1
            }),
        );
        remove_adopted_item("to-remove", &lock_path).unwrap();
        let updated: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock_path).unwrap()).unwrap();
        assert_eq!(updated["_personalPlugins"].as_array().unwrap().len(), 1);
        assert_eq!(updated["schemaVersion"], 1);
    }

    // ── repair seams ─────────────────────────────────────────────────────────

    #[test]
    fn adopted_items_without_source_enumerates_missing_and_empty_source() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = write_lock(
            tmp.path(),
            &json!({
                "_adoptedItems": [
                    { "name": "no-field", "sourceId": "claude-mcp" },
                    { "name": "empty-string", "sourceId": "claude-mcp", "source": "" },
                    { "name": "has-source", "sourceId": "claude-plugin", "source": "https://github.com/o/r" }
                ]
            }),
        );
        let mut names = adopted_items_without_source(&lock_path).unwrap();
        names.sort();
        assert_eq!(names, vec!["empty-string", "no-field"]);
    }

    #[test]
    fn adopted_items_without_source_missing_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join("lock.json");
        assert!(adopted_items_without_source(&lock_path).unwrap().is_empty());
    }

    #[test]
    fn find_adopted_item_returns_row_even_when_source_already_present() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = write_lock(
            tmp.path(),
            &json!({
                "_adoptedItems": [
                    { "name": "already-sourced", "sourceId": "claude-plugin", "source": "https://github.com/o/r" }
                ]
            }),
        );
        let found = find_adopted_item("already-sourced", &lock_path).unwrap();
        assert!(
            found.is_some(),
            "explicit lookup must find any row, not just unresolved ones"
        );
        assert_eq!(found.unwrap()["source"], "https://github.com/o/r");
    }

    #[test]
    fn find_adopted_item_unknown_name_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = write_lock(tmp.path(), &json!({ "_adoptedItems": [] }));
        assert!(find_adopted_item("ghost", &lock_path).unwrap().is_none());
    }

    #[test]
    fn write_adopted_source_updates_uniquely_matched_row() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = write_lock(
            tmp.path(),
            &json!({
                "_adoptedItems": [
                    { "name": "codebase-memory-mcp", "sourceId": "claude-mcp" }
                ]
            }),
        );
        let wrote = write_adopted_source(
            "codebase-memory-mcp",
            "https://github.com/DeusData/codebase-memory-mcp",
            &lock_path,
        )
        .unwrap();
        assert!(wrote);
        let updated: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock_path).unwrap()).unwrap();
        assert_eq!(
            updated["_adoptedItems"][0]["source"],
            "https://github.com/DeusData/codebase-memory-mcp"
        );
    }

    #[test]
    fn write_adopted_source_can_overwrite_existing_source() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = write_lock(
            tmp.path(),
            &json!({
                "_adoptedItems": [
                    { "name": "mkt-plugin", "sourceId": "claude-plugin", "source": "https://github.com/old/repo" }
                ]
            }),
        );
        write_adopted_source("mkt-plugin", "https://github.com/new/repo", &lock_path).unwrap();
        let updated: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock_path).unwrap()).unwrap();
        assert_eq!(
            updated["_adoptedItems"][0]["source"],
            "https://github.com/new/repo"
        );
    }

    #[test]
    fn write_adopted_source_unknown_name_returns_false_no_write() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = write_lock(
            tmp.path(),
            &json!({
                "_adoptedItems": [
                    { "name": "existing", "sourceId": "claude-mcp" }
                ]
            }),
        );
        let before = std::fs::read(&lock_path).unwrap();
        let wrote = write_adopted_source("ghost", "https://github.com/o/r", &lock_path).unwrap();
        assert!(!wrote);
        let after = std::fs::read(&lock_path).unwrap();
        assert_eq!(
            before, after,
            "unknown-name no-write path must be byte-identical"
        );
    }

    #[test]
    fn write_adopted_source_duplicate_name_fails_closed_no_write() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = write_lock(
            tmp.path(),
            &json!({
                "_adoptedItems": [
                    { "name": "dup", "sourceId": "claude-mcp" },
                    { "name": "dup", "sourceId": "claude-skill" }
                ]
            }),
        );
        let before = std::fs::read(&lock_path).unwrap();
        let result = write_adopted_source("dup", "https://github.com/o/r", &lock_path);
        assert!(
            result.is_err(),
            "duplicate name must fail closed, never guess"
        );
        let after = std::fs::read(&lock_path).unwrap();
        assert_eq!(
            before, after,
            "duplicate-name no-write path must be byte-identical"
        );
    }

    #[test]
    fn write_adopted_source_preserves_unrelated_namespaces() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = write_lock(
            tmp.path(),
            &json!({
                "_adoptedItems": [
                    { "name": "codebase-memory-mcp", "sourceId": "claude-mcp" }
                ],
                "_personalPlugins": [{ "pluginId": "my-plugin", "pin": "abc123" }],
                "schemaVersion": 1
            }),
        );
        write_adopted_source(
            "codebase-memory-mcp",
            "https://github.com/DeusData/codebase-memory-mcp",
            &lock_path,
        )
        .unwrap();
        let updated: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock_path).unwrap()).unwrap();
        assert_eq!(updated["_personalPlugins"].as_array().unwrap().len(), 1);
        assert_eq!(updated["schemaVersion"], 1);
    }

    // ── vendor_native_names selector ────────────────────────────────────────

    #[test]
    fn vendor_native_names_codex_lists_node_repl() {
        assert_eq!(vendor_native_names(&AgentKind::Codex), &["node_repl"]);
    }

    #[test]
    fn vendor_native_names_claude_is_empty() {
        assert_eq!(
            vendor_native_names(&AgentKind::Claude),
            &[] as &[&str],
            "empty as of the 2026-08-05 survey, not an oversight"
        );
    }

    // ── filter_vendor_native — absent-name no-op contract ──────────────────────

    #[test]
    fn filter_vendor_native_is_no_op_when_listed_name_absent_from_surface() {
        let items = vec![codex_mcp_item("fetch"), codex_mcp_item("puppeteer")];
        let state = filter_vendor_native(items.clone(), &AgentKind::Codex);
        assert_eq!(
            state.items, items,
            "no listed name present, items pass through unchanged"
        );
        assert_eq!(state.skipped_vendor_native, Vec::<String>::new());
    }

    #[test]
    fn filter_vendor_native_excludes_listed_name_and_reports_it_skipped() {
        let items = vec![codex_mcp_item("node_repl"), codex_mcp_item("fetch")];
        let state = filter_vendor_native(items, &AgentKind::Codex);
        assert_eq!(state.items, vec![codex_mcp_item("fetch")]);
        assert_eq!(state.skipped_vendor_native, vec!["node_repl".to_string()]);
    }

    #[test]
    fn filter_vendor_native_claude_empty_list_is_always_a_no_op() {
        let items = vec![plugin_item_unresolved("ccync@ccync", "1.0.1")];
        let state = filter_vendor_native(items.clone(), &AgentKind::Claude);
        assert_eq!(
            state.items, items,
            "Claude's vendor-native list is empty, so nothing is ever excluded"
        );
        assert_eq!(state.skipped_vendor_native, Vec::<String>::new());
    }

    // ── read_codex_adoptable_state / read_claude_adoptable_state filtering ──

    #[test]
    fn read_codex_adoptable_state_excludes_node_repl_and_reports_it_skipped() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        let codex_dir = tmp.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("config.toml"),
            r#"
[mcp_servers.node_repl]
command = "node"

[mcp_servers.fetch]
command = "uvx"
args = ["mcp-server-fetch"]
"#,
        )
        .unwrap();

        let state = read_codex_adoptable_state();
        let names: Vec<&str> = state.items.iter().map(|i| i.name.as_str()).collect();
        assert!(
            !names.contains(&"node_repl"),
            "node_repl must not appear in the adoptable item vector"
        );
        assert!(
            names.contains(&"fetch"),
            "the user's own server still adopts"
        );
        assert_eq!(state.skipped_vendor_native, vec!["node_repl".to_string()]);
    }

    #[test]
    fn read_codex_adoptable_state_no_vendor_native_present_reports_nothing_skipped() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        let codex_dir = tmp.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("config.toml"),
            r#"
[mcp_servers.fetch]
command = "uvx"
args = ["mcp-server-fetch"]
"#,
        )
        .unwrap();

        let state = read_codex_adoptable_state();
        assert_eq!(state.skipped_vendor_native, Vec::<String>::new());
    }
}
