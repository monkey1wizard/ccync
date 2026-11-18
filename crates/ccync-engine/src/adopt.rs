use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;

use ccync_foundation::mcp::McpServer;
use ccync_foundation::paths::{
    claude_config_path, claude_installed_plugins_path, claude_known_marketplaces_path,
    claude_mcp_json_path, claude_skills_path, codex_config_path, config_dir,
    shared_agents_skills_path, user_home,
};
use ccync_foundation::target::AgentId;

use crate::truth::{LooseSkill, McpServerStore};

pub mod jsonc;

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
const ADOPTION_KEY: &str = "_adoption";

/// Provenance stored in the personal plugin catalog independently of lock
/// projections. Policy fields are metadata only and must never contain values
/// from `env`, `headers`, commands, or URLs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdoptionRecord {
    pub source: String,
    pub kind: String,
    pub component_id: String,
    pub name: String,
    pub origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<Value>,
}

/// Merge records by source and component identity while preserving unknown
/// catalog fields and records belonging to other origins.
pub fn store_adoption_records(
    catalog_path: &Path,
    records: &[AdoptionRecord],
    targets: &[String],
) -> Result<(), AdoptError> {
    let mut catalog = if catalog_path.exists() {
        let text =
            std::fs::read_to_string(catalog_path).map_err(|e| AdoptError::Read(e.to_string()))?;
        serde_json::from_str::<Value>(&text).map_err(|e| AdoptError::Read(e.to_string()))?
    } else {
        json!({})
    };
    let object = catalog
        .as_object_mut()
        .ok_or_else(|| AdoptError::Read("personal catalog root must be an object".into()))?;
    match object.get("plugins") {
        None => {
            object.insert("plugins".into(), Value::Array(Vec::new()));
        }
        Some(Value::Array(_)) => {}
        Some(_) => {
            return Err(AdoptError::Read(
                "personal catalog.plugins must be an array".into(),
            ));
        }
    }
    let mut namespace = match object.get(ADOPTION_KEY) {
        None | Some(Value::Array(_)) => serde_json::Map::new(),
        Some(Value::Object(namespace)) => namespace.clone(),
        Some(_) => {
            return Err(AdoptError::Read(
                "personal catalog._adoption must be an object or legacy array".into(),
            ))
        }
    };
    let mut saved = match namespace.get("records") {
        Some(Value::Array(records)) => records.clone(),
        None => object
            .get(ADOPTION_KEY)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        Some(_) => {
            return Err(AdoptError::Read(
                "personal catalog._adoption.records must be an array".into(),
            ))
        }
    };
    for record in records {
        let mut value =
            serde_json::to_value(record).map_err(|e| AdoptError::Write(e.to_string()))?;
        if saved.iter().any(|old| {
            old.get("name").and_then(Value::as_str) == Some(record.name.as_str())
                && old.get("kind").and_then(Value::as_str) == Some(record.kind.as_str())
                && old.get("origin").is_none_or(Value::is_null)
        }) {
            return Err(AdoptError::Read(format!(
                "legacy adoption '{}' has no origin; rescan the import source before updating it",
                record.name
            )));
        }
        // Policy is metadata only. Accept only the portable boolean flags
        // understood by the adoption UI, never arbitrary source values.
        if let Some(policy) = value.get_mut("policy") {
            let safe = policy
                .as_object()
                .map(|fields| {
                    fields
                        .iter()
                        .filter(|(key, value)| {
                            matches!(key.as_str(), "enabled" | "disabled") && value.is_boolean()
                        })
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect::<serde_json::Map<_, _>>()
                })
                .unwrap_or_default();
            *policy = Value::Object(safe);
        }
        saved.retain(|old| {
            !(old.get("source") == value.get("source")
                && old.get("kind") == value.get("kind")
                && old.get("componentId") == value.get("componentId"))
        });
        saved.push(value);
    }
    namespace.insert("records".into(), Value::Array(saved));
    namespace.insert("targetSnapshot".into(), json!(targets));
    object.insert(ADOPTION_KEY.into(), Value::Object(namespace));
    let text =
        serde_json::to_string_pretty(&catalog).map_err(|e| AdoptError::Write(e.to_string()))?;
    if let Some(parent) = catalog_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| AdoptError::Write(e.to_string()))?;
    }
    ccync_foundation::platform::atomic_write_bytes(catalog_path, text.as_bytes())
        .map_err(|e| AdoptError::Write(e.to_string()))
}

/// Project stored provenance into lockfile index rows. MCP definitions are
/// excluded because credentials must be reread from verified source data.
pub fn rebuild_adoption_lock_rows(catalog_path: &Path, lock_path: &Path) -> Result<(), AdoptError> {
    let text =
        std::fs::read_to_string(catalog_path).map_err(|e| AdoptError::Read(e.to_string()))?;
    let catalog: Value =
        serde_json::from_str(&text).map_err(|e| AdoptError::Read(e.to_string()))?;
    match catalog.get("plugins") {
        None | Some(Value::Array(_)) => {}
        Some(_) => {
            return Err(AdoptError::Read(
                "personal catalog.plugins must be an array".into(),
            ));
        }
    }
    let records = catalog
        .get(ADOPTION_KEY)
        .and_then(|v| {
            v.get("records")
                .and_then(Value::as_array)
                .or_else(|| v.as_array())
        })
        .cloned()
        .unwrap_or_default();
    let adopted: Vec<Value> = records.iter().map(|r| json!({"name":r.get("name"),"sourceId":r.get("kind"),"source":r.get("origin"),"adoptedFrom":r.get("source"),"version":Value::Null})).collect();
    let loose: Vec<Value> = records
        .iter()
        .filter(|r| r.get("kind").and_then(Value::as_str) == Some("skill"))
        .map(|r| json!({"name":r.get("name"),"source":r.get("source"),"origin":r.get("origin")}))
        .collect();
    let mut mcp = serde_json::Map::new();
    let mut refused_names = HashSet::new();
    for record in records
        .iter()
        .filter(|r| r.get("kind").and_then(Value::as_str) == Some("mcp-server"))
    {
        // Disabled servers and servers carrying source-specific policy are
        // retained as adoption metadata, but are not portable lock entries.
        if record.get("enabled").and_then(Value::as_bool) == Some(false)
            || record
                .get("policy")
                .and_then(Value::as_object)
                .is_some_and(|policy| {
                    policy.get("enabled").and_then(Value::as_bool) == Some(false)
                        || policy.get("disabled").and_then(Value::as_bool) == Some(true)
                })
        {
            continue;
        }
        let (Some(name), Some(origin), Some(expected)) = (
            record.get("name").and_then(Value::as_str),
            record.get("origin").and_then(Value::as_str),
            record.get("digest").and_then(Value::as_str),
        ) else {
            continue;
        };
        let _ = (origin, expected);
        if refused_names.contains(name) {
            continue;
        }
        if let Ok(record) = serde_json::from_value::<AdoptionRecord>(record.clone()) {
            match verify_consented_mcp(&record) {
                Ok(ConsentedMcp::Portable(definition)) => {
                    mcp.insert(name.to_owned(), definition);
                }
                Ok(ConsentedMcp::SourcePreserved(_)) => {
                    mcp.remove(name);
                    refused_names.insert(name.to_owned());
                }
                Err(_) => {
                    mcp.remove(name);
                    refused_names.insert(name.to_owned());
                }
            }
        }
    }
    let mut lock = read_lock_file(lock_path)?;
    let map = lock
        .as_object_mut()
        .ok_or_else(|| AdoptError::Read("lock root must be an object".into()))?;
    map.insert(ADOPT_KEY.into(), Value::Array(adopted));
    map.insert(LOOSE_SKILLS_KEY.into(), Value::Array(loose));
    map.insert(MCP_SERVERS_KEY.into(), Value::Object(mcp));
    let text = serde_json::to_string_pretty(&lock).map_err(|e| AdoptError::Write(e.to_string()))?;
    ccync_foundation::platform::atomic_write_bytes(lock_path, text.as_bytes())
        .map_err(|e| AdoptError::Write(e.to_string()))
}

/// Fingerprint an MCP definition for later source verification. The digest
/// contains no credential text. Credentials remain in the source.
pub fn mcp_source_fingerprint(definition: &Value) -> Result<String, AdoptError> {
    use sha2::Digest;
    let bytes = serde_json::to_vec(definition).map_err(|e| AdoptError::Write(e.to_string()))?;
    Ok(format!("sha256:{:x}", sha2::Sha256::digest(bytes)))
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConsentedMcp {
    Portable(Value),
    SourcePreserved(Value),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentRefusal {
    Changed,
    Unsafe,
    Missing,
}

fn read_mcp_source(record: &AdoptionRecord) -> Result<Value, ConsentRefusal> {
    if record.kind != "mcp-server" || record.component_id != record.name {
        return Err(ConsentRefusal::Unsafe);
    }
    let origin = record.origin.as_deref().ok_or(ConsentRefusal::Missing)?;
    let text = std::fs::read_to_string(origin).map_err(|_| ConsentRefusal::Missing)?;
    let raw = if origin.ends_with(".toml") {
        let toml: toml::Value = toml::from_str(&text).map_err(|_| ConsentRefusal::Unsafe)?;
        serde_json::to_value(toml).map_err(|_| ConsentRefusal::Unsafe)?
    } else {
        jsonc::parse(&text).map_err(|_| ConsentRefusal::Unsafe)?
    };
    let servers = match record.source.as_str() {
        "claude" | "copilot" | "agy" => raw.get("mcpServers").and_then(Value::as_object),
        "codex" => raw.get("mcp_servers").and_then(Value::as_object),
        "opencode" => raw.get("mcp").and_then(Value::as_object),
        _ => None,
    }
    .ok_or(ConsentRefusal::Missing)?;
    let mut definition = servers
        .get(&record.name)
        .cloned()
        .ok_or(ConsentRefusal::Missing)?;
    if record.source == "codex" {
        if let Some(map) = definition.as_object_mut() {
            if !map.contains_key("headers") {
                if let Some(headers) = map.remove("http_headers") {
                    map.insert("headers".into(), headers);
                }
            }
        }
    }
    if let Some(map) = definition.as_object_mut() {
        if matches!(record.source.as_str(), "agy" | "copilot") {
            if !map.contains_key("url") {
                if let Some(url) = map.remove("serverUrl") {
                    map.insert("url".into(), url);
                }
            }
            if map.get("type").and_then(Value::as_str) == Some("local") {
                map.insert("type".into(), json!("stdio"));
            }
        }
        if record.source == "opencode" {
            if let Some(Value::Array(parts)) = map.get("command").cloned() {
                if let Some((first, rest)) = parts.split_first() {
                    map.insert("command".into(), first.clone());
                    if !rest.is_empty() {
                        map.insert("args".into(), Value::Array(rest.to_vec()));
                    }
                }
            }
            if !map.contains_key("env") {
                if let Some(env) = map.remove("environment") {
                    map.insert("env".into(), env);
                }
            }
            if !map.contains_key("cwd") {
                if let Some(cwd) = map.remove("workingDirectory") {
                    map.insert("cwd".into(), cwd);
                }
            }
            if map.get("type").and_then(Value::as_str) == Some("local") {
                map.insert("type".into(), json!("stdio"));
            }
            if map.get("type").and_then(Value::as_str) == Some("remote") {
                map.remove("type");
            }
        }
    }
    Ok(definition)
}

pub fn preview_mcp_digests(
    source: AgentId,
    scan: &SourceScan,
) -> std::collections::BTreeMap<String, String> {
    scan.adoptable
        .items
        .iter()
        .filter(|item| item.kind == ItemKind::McpServer)
        .filter_map(|item| {
            let record = AdoptionRecord {
                source: source.as_str().into(),
                kind: "mcp-server".into(),
                component_id: item.name.clone(),
                name: item.name.clone(),
                origin: item.source.clone(),
                revision: None,
                digest: None,
                enabled: None,
                policy: None,
            };
            let definition = read_mcp_source(&record).ok()?;
            Some((item.name.clone(), mcp_source_fingerprint(&definition).ok()?))
        })
        .collect()
}

pub fn verify_consented_mcp(record: &AdoptionRecord) -> Result<ConsentedMcp, ConsentRefusal> {
    let expected = record.digest.as_deref().ok_or(ConsentRefusal::Missing)?;
    let definition = read_mcp_source(record)?;
    if mcp_source_fingerprint(&definition).map_err(|_| ConsentRefusal::Unsafe)? != expected {
        return Err(ConsentRefusal::Changed);
    }
    let modeled: McpServer =
        serde_json::from_value(definition.clone()).map_err(|_| ConsentRefusal::Unsafe)?;
    let literal_secret = ["env", "headers"].iter().any(|field| {
        definition
            .get(field)
            .and_then(Value::as_object)
            .is_some_and(|map| {
                map.iter().any(|(name, value)| {
                    ccync_foundation::mcp::is_secret_name(name)
                        && !value
                            .as_str()
                            .is_some_and(|v| v.starts_with("${") && v.ends_with('}'))
                })
            })
    });
    if literal_secret
        || crate::catalog::validate_mcp_output(&record.name, &definition, false).is_err()
    {
        return Ok(ConsentedMcp::SourcePreserved(definition));
    }
    let normalized = serde_json::to_value(modeled).map_err(|_| ConsentRefusal::Unsafe)?;
    Ok(ConsentedMcp::Portable(normalized))
}

pub fn adopt_consented_mcp_definitions(
    catalog_path: &Path,
    lock_path: &Path,
    source: AgentId,
) -> Result<Vec<(String, ConsentRefusal)>, AdoptError> {
    if !catalog_path.exists() {
        return Ok(Vec::new());
    }
    let records = read_adoption_records(catalog_path)?;
    let mut lock = read_lock_file(lock_path)?;
    let map = lock
        .as_object_mut()
        .ok_or_else(|| AdoptError::Read("lock root must be an object".into()))?;
    let servers = map
        .entry(MCP_SERVERS_KEY)
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| AdoptError::Read("invalid MCP servers".into()))?;
    let mut refused = Vec::new();
    for record in records
        .iter()
        .filter(|r| r.kind == "mcp-server" && r.source == source.as_str())
    {
        servers.remove(&record.name);
        match verify_consented_mcp(record) {
            Ok(ConsentedMcp::Portable(value)) => {
                servers.insert(record.name.clone(), value);
            }
            Ok(ConsentedMcp::SourcePreserved(_)) => {}
            Err(reason) => refused.push((record.name.clone(), reason)),
        }
    }
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| AdoptError::Write(e.to_string()))?;
    }
    let text = serde_json::to_string_pretty(&lock).map_err(|e| AdoptError::Write(e.to_string()))?;
    ccync_foundation::platform::atomic_write_bytes(lock_path, text.as_bytes())
        .map_err(|e| AdoptError::Write(e.to_string()))?;
    Ok(refused)
}

/// Read persisted adoption records. Legacy array-shaped `_adoption` values remain readable.
pub fn read_adoption_records(catalog_path: &Path) -> Result<Vec<AdoptionRecord>, AdoptError> {
    let text =
        std::fs::read_to_string(catalog_path).map_err(|e| AdoptError::Read(e.to_string()))?;
    let catalog: Value =
        serde_json::from_str(&text).map_err(|e| AdoptError::Read(e.to_string()))?;
    match catalog.get("plugins") {
        None | Some(Value::Array(_)) => {}
        Some(_) => {
            return Err(AdoptError::Read(
                "personal catalog.plugins must be an array".into(),
            ));
        }
    }
    let values = match catalog.get(ADOPTION_KEY) {
        None => Vec::new(),
        Some(Value::Array(records)) => records.clone(),
        Some(Value::Object(namespace)) => match namespace.get("records") {
            None => Vec::new(),
            Some(Value::Array(records)) => records.clone(),
            Some(_) => {
                return Err(AdoptError::Read(
                    "personal catalog._adoption.records must be an array".into(),
                ))
            }
        },
        Some(_) => {
            return Err(AdoptError::Read(
                "personal catalog._adoption must be an object or legacy array".into(),
            ))
        }
    };
    values
        .into_iter()
        .map(|v| serde_json::from_value(v).map_err(|e| AdoptError::Read(e.to_string())))
        .collect()
}

// ── shared types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentKind {
    Claude,
    Codex,
    Copilot,
    OpenCode,
    Agy,
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
        AgentKind::Copilot | AgentKind::OpenCode | AgentKind::Agy => &[],
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceState {
    Absent,
    Empty,
    Ready,
    Invalid,
    Unreadable,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceObservation {
    pub origin: std::path::PathBuf,
    pub state: SourceState,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceScan {
    pub agent: AgentId,
    pub state: SourceState,
    pub origins: Vec<SourceObservation>,
    pub adoptable: AdoptableState,
    pub reason: Option<String>,
    /// Fields or records observed but not imported by the source baseline.
    pub unsupported: Vec<String>,
}

/// One MCP server that a source scan blocked instead of importing. The record
/// names the server and its reason but no definition values, so credentials in
/// `env` or `headers` never reach a diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BlockedMcpServer {
    pub name: String,
    /// The config file the server was read from, when the source records one.
    pub origin: Option<std::path::PathBuf>,
    pub reason: String,
    /// True when the source also marks the server disabled.
    pub disabled: bool,
}

/// A source scan plus the MCP diagnostics that [`SourceScan`] does not carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceScanOutcome {
    pub scan: SourceScan,
    /// Servers blocked by divergent definitions or unportable tool policy, by name.
    pub blocked_mcp: Vec<BlockedMcpServer>,
    /// Servers the source disables and does not block, by name.
    pub disabled_mcp: Vec<String>,
}

/// Read a supported agent source without writing to it. Discovery records every
/// configured input, including absent optional roots, and keeps legacy readers intact.
pub fn scan_source(agent: AgentId) -> SourceScan {
    scan_source_outcome(agent).scan
}

/// [`scan_source`] with the MCP diagnostics it does not carry. The scan itself is
/// unchanged, so callers that need only the legacy shape keep [`scan_source`].
pub fn scan_source_outcome(agent: AgentId) -> SourceScanOutcome {
    let roots: Vec<std::path::PathBuf> = match agent {
        AgentId::Claude => [
            claude_skills_path(),
            claude_config_path(),
            claude_mcp_json_path(),
            claude_installed_plugins_path(),
        ]
        .into_iter()
        .flatten()
        .collect(),
        AgentId::Copilot => user_home()
            .map(|h| {
                vec![
                    h.join(".copilot").join("skills"),
                    shared_agents_skills_path().unwrap_or_else(|| h.join(".agents").join("skills")),
                    h.join(".copilot").join("mcp-config.json"),
                    h.join(".copilot").join("settings.json"),
                    h.join(".copilot").join("installed-plugins.json"),
                ]
            })
            .unwrap_or_default(),
        AgentId::Codex => [shared_agents_skills_path(), codex_config_path()]
            .into_iter()
            .flatten()
            .collect(),
        AgentId::Opencode => opencode_roots(),
        AgentId::Agy => agy_skill_roots(),
    };
    let adoptable = match agent {
        AgentId::Claude => read_claude_adoptable_state(),
        AgentId::Codex => read_codex_adoptable_state(),
        AgentId::Copilot => read_copilot_adoptable_state(),
        AgentId::Opencode => read_opencode_adoptable_state(),
        AgentId::Agy => read_agy_adoptable_state(),
    };
    let override_error = source_override_error(agent);
    let config_conflict = match agent {
        AgentId::Opencode => opencode_config_values().1,
        AgentId::Agy => (!adoptable.unsupported.is_empty()).then(|| {
            format!(
                "conflicting AGY skill contents: {}",
                adoptable.unsupported.join(", ")
            )
        }),
        _ => None,
    };
    let origins: Vec<_> = roots
        .into_iter()
        .map(|origin| {
            let origin = std::fs::canonicalize(&origin).unwrap_or(origin);
            let (state, reason) = inspect_origin(&origin);
            SourceObservation {
                origin,
                state,
                reason,
            }
        })
        .collect();
    let state = if override_error.is_some() {
        SourceState::Unsupported
    } else if config_conflict.is_some() {
        SourceState::Invalid
    } else if origins.iter().any(|o| o.state == SourceState::Unreadable) {
        SourceState::Unreadable
    } else if origins.iter().any(|o| o.state == SourceState::Invalid) {
        SourceState::Invalid
    } else if !adoptable.items.is_empty() {
        SourceState::Ready
    } else if origins
        .iter()
        .any(|o| o.state == SourceState::Empty || o.state == SourceState::Ready)
    {
        SourceState::Empty
    } else {
        SourceState::Absent
    };
    let unsupported = match agent {
        AgentId::Copilot => copilot_unsupported_fields(),
        AgentId::Opencode => opencode_unsupported_fields(),
        AgentId::Agy => adoptable.unsupported.clone(),
        _ => vec![],
    };
    let (blocked_mcp, disabled_mcp) = mcp_diagnostics(agent_kind(agent));
    let scan = SourceScan {
        agent,
        state,
        origins,
        adoptable,
        reason: override_error.or(config_conflict),
        unsupported,
    };
    SourceScanOutcome {
        scan,
        blocked_mcp,
        disabled_mcp,
    }
}

fn agent_kind(agent: AgentId) -> AgentKind {
    match agent {
        AgentId::Claude => AgentKind::Claude,
        AgentId::Codex => AgentKind::Codex,
        AgentId::Copilot => AgentKind::Copilot,
        AgentId::Opencode => AgentKind::OpenCode,
        AgentId::Agy => AgentKind::Agy,
    }
}

/// The blocked and disabled MCP servers of `agent`, read from the same
/// definitions the import reads. A name lands in `blocked` or in `disabled`.
fn mcp_diagnostics(agent: AgentKind) -> (Vec<BlockedMcpServer>, Vec<String>) {
    let defs = read_mcp_definitions(&agent);
    let mut blocked: Vec<BlockedMcpServer> = defs
        .blocked
        .iter()
        .map(|(name, reason)| BlockedMcpServer {
            name: name.clone(),
            origin: defs.blocked_origins.get(name).cloned(),
            reason: reason.clone(),
            disabled: policy_disabled(&defs.policy, name),
        })
        .collect();
    blocked.sort_by(|a, b| a.name.cmp(&b.name));
    let mut disabled: Vec<String> = defs
        .policy
        .keys()
        .filter(|name| policy_disabled(&defs.policy, name) && !defs.blocked.contains_key(*name))
        .cloned()
        .collect();
    disabled.sort();
    (blocked, disabled)
}

fn policy_disabled(policy: &HashMap<String, Value>, name: &str) -> bool {
    policy.get(name).and_then(|p| p.get("enabled")) == Some(&Value::Bool(false))
}

/// Resolve each AGY face's native skills surface. Plugin junction roots are
/// deliberately excluded because they do not provide a native source reader.
fn agy_skill_roots() -> Vec<std::path::PathBuf> {
    let Some(home) = user_home() else {
        return Vec::new();
    };
    let gemini = home.join(".gemini");
    AgentId::Agy
        .expand()
        .iter()
        .map(|face| {
            let root = match face {
                ccync_foundation::target::ProjectionTargetId::AgyCli => {
                    gemini.join("antigravity-cli")
                }
                ccync_foundation::target::ProjectionTargetId::AgyIde => {
                    gemini.join("antigravity-ide")
                }
                ccync_foundation::target::ProjectionTargetId::AgyGui => gemini.clone(),
                _ => unreachable!("AGY expands only to AGY faces"),
            };
            root.join("skills")
        })
        .collect()
}

/// Adoptable AGY items keyed by identity, each with its physical copies.
type AgyItemsByIdentity = HashMap<String, (InstalledItem, Vec<(std::path::PathBuf, Vec<u8>)>)>;

fn read_agy_adoptable_state() -> AdoptableState {
    let roots = agy_skill_roots();
    let mut physical_roots = HashSet::new();
    let mut items_by_identity: AgyItemsByIdentity = HashMap::new();
    let mut conflicts = Vec::new();
    let mut skipped_owned = Vec::new();
    for root in roots {
        let physical_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        if !physical_roots.insert(physical_root.clone()) {
            continue;
        }
        for item in enumerate_skill_dirs_reporting(&root, AgentKind::Agy, &mut skipped_owned) {
            let skill_root = physical_root.join(&item.name);
            let content = skill_content(&skill_root);
            let mut item = item;
            item.source = Some(skill_root.to_string_lossy().into_owned());
            match items_by_identity.get(&item.name) {
                Some((_, previous)) if previous != &content => conflicts.push(item.name.clone()),
                Some(_) => {}
                None => {
                    items_by_identity.insert(item.name.clone(), (item, content));
                }
            }
        }
    }
    conflicts.sort();
    conflicts.dedup();
    let mcp = read_agy_mcp_sources();
    let mut items: Vec<InstalledItem> = items_by_identity
        .into_values()
        .map(|(item, _)| item)
        .collect();
    let mut mcp_names: Vec<_> = mcp.store.keys().cloned().collect();
    mcp_names.sort();
    items.extend(mcp_names.into_iter().map(|name| {
        InstalledItem {
            source: mcp
                .origins
                .get(&name)
                .map(|path| path.to_string_lossy().into_owned()),
            name,
            version: None,
            agent: AgentKind::Agy,
            kind: ItemKind::McpServer,
            baseline_version: None,
        }
    }));
    AdoptableState {
        items: if conflicts.is_empty() {
            items
        } else {
            Vec::new()
        },
        unsupported: conflicts,
        policy: mcp.policy,
        skipped_owned,
        ..AdoptableState::default()
    }
}

fn skill_content(root: &Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    fn collect(root: &Path, current: &Path, files: &mut Vec<(std::path::PathBuf, Vec<u8>)>) {
        let Ok(entries) = std::fs::read_dir(current) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                collect(root, &path, files);
            } else if metadata.is_file() {
                if let (Ok(relative), Ok(bytes)) = (path.strip_prefix(root), std::fs::read(&path)) {
                    files.push((relative.to_path_buf(), bytes));
                }
            }
        }
    }
    let mut files = Vec::new();
    collect(root, root, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

fn source_override_error(agent: AgentId) -> Option<String> {
    let (key, default) = match agent {
        AgentId::Claude => ("CLAUDE_CONFIG_DIR", user_home()?.join(".claude")),
        AgentId::Codex => ("CODEX_HOME", user_home()?.join(".codex")),
        AgentId::Copilot => ("COPILOT_HOME", user_home()?.join(".copilot")),
        AgentId::Opencode => {
            user_home()?;
            let default_dir = config_dir()?.join("opencode");
            for (key, default) in [
                ("OPENCODE_CONFIG_DIR", default_dir.clone()),
                ("OPENCODE_CONFIG", default_dir.join("opencode.json")),
            ] {
                if let Some(value) = std::env::var_os(key) {
                    if std::path::Path::new(&value) != default {
                        return Some(format!("{key} is a non-default source override"));
                    }
                }
            }
            if std::env::var_os("OPENCODE_CONFIG_CONTENT").is_some_and(|v| !v.is_empty()) {
                return Some("OPENCODE_CONFIG_CONTENT is a non-default source override".into());
            }
            return None;
        }
        _ => return None,
    };
    let value = std::env::var_os(key)?;
    (std::path::Path::new(&value) != default)
        .then(|| format!("{key} points outside its home-derived default"))
}

fn inspect_origin(path: &Path) -> (SourceState, Option<String>) {
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (SourceState::Absent, None),
        Err(e) => (SourceState::Unreadable, Some(e.to_string())),
        Ok(meta) if meta.is_dir() && path.extension().is_some() => (
            SourceState::Unreadable,
            Some("expected a file but found a directory".into()),
        ),
        Ok(meta) if meta.is_dir() => match std::fs::read_dir(path) {
            Err(e) => (SourceState::Unreadable, Some(e.to_string())),
            Ok(entries) => {
                let mut any = false;
                for entry in entries {
                    match entry {
                        Ok(_) => any = true,
                        Err(e) => return (SourceState::Unreadable, Some(e.to_string())),
                    }
                }
                (
                    if any {
                        SourceState::Ready
                    } else {
                        SourceState::Empty
                    },
                    None,
                )
            }
        },
        Ok(_) => match std::fs::read_to_string(path) {
            Err(e) => (SourceState::Unreadable, Some(e.to_string())),
            Ok(text) if text.trim().is_empty() => (SourceState::Empty, None),
            Ok(text) => {
                let result: Result<(), String> = if path.extension().is_some_and(|e| e == "toml") {
                    toml::from_str::<toml::Value>(&text)
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                } else {
                    jsonc::parse(&text).map(|_| ()).map_err(|e| e.to_string())
                };
                match result {
                    Ok(()) => (SourceState::Ready, None),
                    Err(e) => (SourceState::Invalid, Some(e)),
                }
            }
        },
    }
}

/// Items eligible for adopt from a master agent's install surface, plus the
/// names excluded at the read boundary because they matched that agent's
/// [`vendor_native_names`] list. An excluded name never appears in `items`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AdoptableState {
    pub items: Vec<InstalledItem>,
    pub skipped_vendor_native: Vec<String>,
    pub unsupported: Vec<String>,
    pub policy: HashMap<String, Value>,
    /// Skill directories left out of `items` because ccync's ownership record
    /// proves ccync published them. Each entry is `<name> (<path>)`.
    pub skipped_owned: Vec<String>,
}

fn copilot_root() -> Option<std::path::PathBuf> {
    user_home().map(|h| h.join(".copilot"))
}

fn opencode_skill_roots() -> Vec<std::path::PathBuf> {
    let Some(home) = user_home() else {
        return Vec::new();
    };
    let config = config_dir()
        .unwrap_or_else(|| home.join(".config"))
        .join("opencode");
    let mut roots = vec![config.join("skills"), home.join(".claude/skills")];
    if let Some(shared) = shared_agents_skills_path() {
        roots.push(shared);
    }
    let mut seen = HashSet::new();
    roots
        .into_iter()
        .filter(|path| {
            let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            seen.insert(key)
        })
        .collect()
}

fn opencode_roots() -> Vec<std::path::PathBuf> {
    let Some(home) = user_home() else {
        return Vec::new();
    };
    let config = config_dir()
        .unwrap_or_else(|| home.join(".config"))
        .join("opencode");
    let mut roots = opencode_skill_roots();
    roots.extend([config.join("opencode.json"), config.join("opencode.jsonc")]);
    let mut seen = HashSet::new();
    roots
        .into_iter()
        .filter(|path| {
            let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            seen.insert(key)
        })
        .collect()
}

fn opencode_config_values() -> (Vec<(std::path::PathBuf, Value)>, Option<String>) {
    let Some(config) = config_dir()
        .or_else(|| user_home().map(|h| h.join(".config")))
        .map(|p| p.join("opencode"))
    else {
        return (Vec::new(), None);
    };
    let paths = [config.join("opencode.json"), config.join("opencode.jsonc")];
    let mut values = Vec::new();
    for path in paths {
        if let Ok(text) = std::fs::read_to_string(&path) {
            match jsonc::parse(&text) {
                Ok(value) => values.push((path, value)),
                Err(error) => return (values, Some(format!("invalid OpenCode config: {error}"))),
            }
        }
    }
    if values.len() == 2 && values[0].1 != values[1].1 {
        return (
            values,
            Some("conflicting opencode.json and opencode.jsonc configurations".into()),
        );
    }
    (values, None)
}

fn opencode_server(value: &Value) -> Option<McpServer> {
    let server_type = value.get("type").and_then(Value::as_str);
    let command_value = value.get("command");
    let (command, args) = match command_value {
        Some(Value::Array(parts)) => {
            let mut strings = parts
                .iter()
                .map(Value::as_str)
                .collect::<Option<Vec<_>>>()?;
            if strings.is_empty() {
                return None;
            }
            let command = strings.remove(0).to_string();
            (
                Some(command),
                Some(strings.into_iter().map(str::to_string).collect()),
            )
        }
        Some(Value::String(command)) => (Some(command.clone()), None),
        _ => (None, None),
    };
    let env = value
        .get("environment")
        .or_else(|| value.get("env"))
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        });
    Some(McpServer {
        server_type: server_type.map(str::to_string),
        command,
        args,
        env,
        cwd: value
            .get("workingDirectory")
            .or_else(|| value.get("cwd"))
            .and_then(Value::as_str)
            .map(str::to_string),
        url: value.get("url").and_then(Value::as_str).map(str::to_string),
        headers: value.get("headers").and_then(Value::as_object).map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        }),
    })
}

fn read_opencode_adoptable_state() -> AdoptableState {
    let roots = opencode_skill_roots();
    let mut items = Vec::new();
    let mut skipped_owned = Vec::new();
    for root in &roots {
        items.extend(enumerate_skill_dirs_reporting(
            root,
            AgentKind::OpenCode,
            &mut skipped_owned,
        ));
    }
    let (configs, conflict) = opencode_config_values();
    let mut policy = HashMap::new();
    if conflict.is_none() {
        if let Some((config_path, raw)) = configs.first() {
            if let Some(servers) = raw.get("mcp").and_then(Value::as_object) {
                for (name, value) in servers {
                    let enabled = value
                        .get("enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(true);
                    policy.insert(name.clone(), json!({"enabled": enabled}));
                    if enabled && opencode_server(value).is_some() {
                        items.push(InstalledItem {
                            name: name.clone(),
                            version: None,
                            agent: AgentKind::OpenCode,
                            kind: ItemKind::McpServer,
                            source: Some(config_path.to_string_lossy().into_owned()),
                            baseline_version: None,
                        });
                    }
                }
            }
        }
    }
    AdoptableState {
        items,
        policy,
        skipped_owned,
        ..AdoptableState::default()
    }
}

fn opencode_unsupported_fields() -> Vec<String> {
    let mut fields = HashSet::new();
    let (configs, _) = opencode_config_values();
    for (_, config) in configs {
        if let Some(object) = config.as_object() {
            fields.extend(
                object
                    .keys()
                    .filter(|key| !["mcp", "$schema", "$schema", "version"].contains(&key.as_str()))
                    .cloned(),
            );
        }
        if let Some(servers) = config.get("mcp").and_then(Value::as_object) {
            for (name, value) in servers {
                if let Some(object) = value.as_object() {
                    for key in object.keys().filter(|key| {
                        ![
                            "type",
                            "command",
                            "environment",
                            "env",
                            "url",
                            "headers",
                            "enabled",
                            "workingDirectory",
                            "cwd",
                        ]
                        .contains(&key.as_str())
                    }) {
                        fields.insert(format!("mcp.{name}.{key}"));
                    }
                }
            }
        }
    }
    let mut fields: Vec<_> = fields.into_iter().collect();
    fields.sort();
    fields
}

fn copilot_settings() -> Option<Value> {
    read_json_file(copilot_root().map(|p| p.join("settings.json")))
}

fn copilot_unsupported_fields() -> Vec<String> {
    let mut fields = Vec::new();
    for path in [
        copilot_root().map(|p| p.join("mcp-config.json")),
        copilot_root().map(|p| p.join("settings.json")),
        copilot_root().map(|p| p.join("installed-plugins.json")),
    ]
    .into_iter()
    .flatten()
    {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(value) = jsonc::parse(&text) else {
            continue;
        };
        if let Some(map) = value.as_object() {
            for key in map.keys() {
                if !matches!(
                    key.as_str(),
                    "mcpServers" | "disabledMcpServers" | "disabledSkills" | "plugins"
                ) {
                    fields.push(key.clone());
                }
            }
        }
    }
    fields.sort();
    fields.dedup();
    fields
}

fn read_copilot_adoptable_state() -> AdoptableState {
    let mut skipped_owned = Vec::new();
    let mut items = copilot_root()
        .map(|p| {
            enumerate_skill_dirs_reporting(
                &p.join("skills"),
                AgentKind::Copilot,
                &mut skipped_owned,
            )
        })
        .unwrap_or_default();
    let mcp_config_path = copilot_root().map(|p| p.join("mcp-config.json"));
    let mut policy = HashMap::new();
    // The shared ~/.agents/skills root is observed for scan state only. Codex
    // owns its import identity, so importing it again as Copilot would duplicate it.
    let mut mcp = McpServerStore::new();
    if let Some(raw) = read_json_file(mcp_config_path.clone()) {
        if let Some(servers) = raw.get("mcpServers").and_then(Value::as_object) {
            for (name, def) in servers {
                if let Ok(server) = serde_json::from_value::<McpServer>(def.clone()) {
                    mcp.insert(name.clone(), server);
                }
            }
        }
    }
    let settings = copilot_settings().unwrap_or(Value::Null);
    let disabled: HashSet<&str> = settings
        .get("disabledMcpServers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    for name in mcp.keys() {
        policy.insert(
            name.clone(),
            json!({"enabled": !disabled.contains(name.as_str())}),
        );
        if !disabled.contains(name.as_str()) {
            items.push(InstalledItem {
                name: name.clone(),
                version: None,
                agent: AgentKind::Copilot,
                kind: ItemKind::McpServer,
                source: mcp_config_path
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                baseline_version: None,
            });
        }
    }
    if !settings.is_null() {
        let raw = settings;
        if let Some(disabled_skills) = raw.get("disabledSkills").and_then(Value::as_array) {
            for item in &items {
                if item.kind == ItemKind::Skill {
                    policy.insert(item.name.clone(), json!({"enabled": !disabled_skills.iter().any(|v| v.as_str() == Some(&item.name))}));
                }
            }
        }
    }
    let plugins = copilot_root()
        .and_then(|p| read_json_file(Some(p.join("installed-plugins.json"))))
        .unwrap_or(Value::Null);
    if let Some(plugins) = plugins.get("plugins").and_then(Value::as_object) {
        for (name, record) in plugins {
            let enabled = record
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let origin = record
                .get("installPath")
                .or_else(|| record.get("path"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            items.push(InstalledItem {
                name: name.clone(),
                version: record
                    .get("version")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                agent: AgentKind::Copilot,
                kind: ItemKind::Plugin,
                source: origin,
                baseline_version: None,
            });
            policy.insert(name.clone(), json!({"enabled": enabled}));
        }
    }
    AdoptableState {
        items,
        skipped_vendor_native: vec![],
        unsupported: copilot_unsupported_fields(),
        policy,
        skipped_owned,
    }
}

/// Split `items` into adopt-eligible items and the names skipped as
/// vendor-native for `agent`, per [`vendor_native_names`].
fn filter_vendor_native(items: Vec<InstalledItem>, agent: &AgentKind) -> AdoptableState {
    let vendor_names = vendor_native_names(agent);
    if vendor_names.is_empty() {
        return AdoptableState {
            items,
            ..AdoptableState::default()
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
        ..AdoptableState::default()
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

fn parse_opencode_mcp_defs(raw: &Value) -> McpServerStore {
    raw.get("mcp")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(_, value)| value.get("enabled").and_then(Value::as_bool) != Some(false))
        .filter_map(|(name, value)| opencode_server(value).map(|server| (name.clone(), server)))
        .collect()
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
                source: claude_mcp_origin(&name),
                name,
                version: None,
                agent: AgentKind::Claude,
                kind: ItemKind::McpServer,
                baseline_version: None,
            }),
    );

    let mut skipped_owned = Vec::new();
    if let Some(skills_dir) = claude_skills_path() {
        items.extend(enumerate_skill_dirs_reporting(
            &skills_dir,
            AgentKind::Claude,
            &mut skipped_owned,
        ));
    }

    let mut state = filter_vendor_native(items, &AgentKind::Claude);
    state.skipped_owned = skipped_owned;
    state
}

/// The Claude config file that declares MCP server `name`. `~/.claude.json`
/// wins over `~/.claude/.mcp.json`, matching [`read_claude_mcp_definitions`].
fn claude_mcp_origin(name: &str) -> Option<String> {
    [claude_config_path(), claude_mcp_json_path()]
        .into_iter()
        .flatten()
        .find(|path| {
            read_json_file(Some(path.clone())).is_some_and(|raw| {
                raw.get("mcpServers")
                    .and_then(Value::as_object)
                    .is_some_and(|servers| servers.contains_key(name))
            })
        })
        .map(|path| path.to_string_lossy().into_owned())
}

// ── codex config.toml ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CodexConfig {
    #[serde(default)]
    mcp_servers: HashMap<String, toml::Value>,
}

/// MCP server items declared in Codex `config.toml` text. `origin` is the path
/// of the file the text came from, recorded as each item's source.
fn parse_codex_mcp_servers(text: &str, origin: Option<&Path>) -> Vec<InstalledItem> {
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
            source: origin.map(|path| path.to_string_lossy().into_owned()),
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
            items.extend(parse_codex_mcp_servers(&text, Some(&path)));
        }
    }

    // Codex consumes skills from the shared `~/.agents/skills/` directory.
    let mut skipped_owned = Vec::new();
    if let Some(skills_dir) = shared_agents_skills_path() {
        items.extend(enumerate_skill_dirs_reporting(
            &skills_dir,
            AgentKind::Codex,
            &mut skipped_owned,
        ));
    }

    let mut state = filter_vendor_native(items, &AgentKind::Codex);
    state.skipped_owned = skipped_owned;
    state
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
    /// Source policy retained alongside portable definitions.
    pub policy: HashMap<String, Value>,
    /// Policy that cannot be represented safely in the portable server model.
    pub blocked: HashMap<String, String>,
    /// The config file each blocked name was read from, when the source records one.
    pub blocked_origins: HashMap<String, std::path::PathBuf>,
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
            policy: HashMap::new(),
            blocked: HashMap::new(),
            blocked_origins: HashMap::new(),
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
        policy: HashMap::new(),
        blocked: HashMap::new(),
        blocked_origins: HashMap::new(),
    }
}

/// Read the FULL MCP server definitions from a master agent for snapshotting
/// into `_mcpServers`, minus definitions on the agent's [`vendor_native_names`]
/// list. Graceful-missing: absent/unparseable config → empty store.
pub fn read_mcp_definitions(agent: &AgentKind) -> AdoptableMcpDefinitions {
    let mut store = match agent {
        AgentKind::Claude => read_claude_mcp_definitions(),
        AgentKind::Codex => codex_config_path()
            .and_then(|p| std::fs::read_to_string(&p).ok())
            .map(|text| parse_codex_mcp_defs(&text))
            .unwrap_or_default(),
        AgentKind::OpenCode => {
            let (configs, conflict) = opencode_config_values();
            if conflict.is_none() {
                configs
                    .first()
                    .map(|(_, raw)| parse_opencode_mcp_defs(raw))
                    .unwrap_or_default()
            } else {
                McpServerStore::new()
            }
        }
        AgentKind::Copilot => {
            let sources = read_copilot_mcp_sources();
            let mut filtered = filter_vendor_native_mcp_defs(sources.store, agent);
            filtered.policy = sources.policy;
            filtered.blocked = sources.blocked;
            filtered.blocked_origins = sources.blocked_origins;
            return filtered;
        }
        AgentKind::Agy => {
            let sources = read_agy_mcp_sources();
            let mut filtered = filter_vendor_native_mcp_defs(sources.store, agent);
            filtered.policy = sources.policy;
            filtered.blocked = sources.blocked;
            filtered.blocked_origins = sources.blocked_origins;
            return filtered;
        }
    };
    let mut policy = HashMap::new();
    let mut blocked = HashMap::new();
    let mut blocked_origins = HashMap::new();
    if *agent == AgentKind::Claude {
        for name in store.keys() {
            policy.insert(name.clone(), json!({"enabled": true}));
        }
        // Preserve the same precedence as the definition merge: top-level
        // ~/.claude.json wins, while ~/.claude/.mcp.json supplies missing names.
        let mut definitions = HashMap::<String, (std::path::PathBuf, Value)>::new();
        for path in [claude_mcp_json_path(), claude_config_path()]
            .into_iter()
            .flatten()
        {
            if let Some(raw) = read_json_file(Some(path.clone())) {
                if let Some(servers) = raw.get("mcpServers").and_then(Value::as_object) {
                    for (name, definition) in servers {
                        definitions.insert(name.clone(), (path.clone(), definition.clone()));
                    }
                }
                if let Some(disabled) = raw.get("disabledMcpServers").and_then(Value::as_array) {
                    for name in disabled.iter().filter_map(Value::as_str) {
                        policy.insert(name.to_string(), json!({"enabled": false}));
                    }
                }
            }
        }
        for (name, (origin, definition)) in definitions {
            if has_unportable_tool_policy(&definition) {
                blocked.insert(
                    name.clone(),
                    "source tool policy cannot be represented by the portable MCP server"
                        .to_string(),
                );
                blocked_origins.insert(name.clone(), origin);
                store.remove(&name);
            }
        }
        for (name, observation) in &policy {
            if observation.get("enabled") == Some(&Value::Bool(false)) {
                store.remove(name);
            }
        }
    } else if *agent == AgentKind::Codex {
        let codex_path = codex_config_path();
        if let Some(text) = codex_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
        {
            if let Ok(root) = toml::from_str::<toml::Table>(&text) {
                if let Some(servers) = root.get("mcp_servers").and_then(toml::Value::as_table) {
                    for (name, definition) in servers {
                        let enabled = definition
                            .get("enabled")
                            .and_then(toml::Value::as_bool)
                            .unwrap_or(true);
                        policy.insert(name.clone(), json!({"enabled": enabled}));
                        if !enabled {
                            store.remove(name);
                        }
                        if has_unportable_tool_policy(&toml_to_json(definition)) {
                            if let Some(origin) = &codex_path {
                                blocked_origins.insert(name.clone(), origin.clone());
                            }
                            blocked.insert(name.clone(), "source tool policy cannot be represented by the portable MCP server".to_string());
                            store.remove(name);
                        }
                    }
                }
            }
        }
    } else if *agent == AgentKind::OpenCode {
        let (configs, conflict) = opencode_config_values();
        if conflict.is_none() {
            if let Some((path, raw)) = configs.first() {
                if let Some(servers) = raw.get("mcp").and_then(Value::as_object) {
                    for (name, definition) in servers {
                        let enabled = definition
                            .get("enabled")
                            .and_then(Value::as_bool)
                            .unwrap_or(true);
                        policy.insert(name.clone(), json!({"enabled": enabled}));
                        if !enabled {
                            store.remove(name);
                        }
                        if has_unportable_tool_policy(definition) {
                            blocked_origins.insert(name.clone(), path.clone());
                            blocked.insert(
                            name.clone(),
                            "source tool policy cannot be represented by the portable MCP server"
                                .into(),
                        );
                            store.remove(name);
                        }
                    }
                }
            }
        }
    }
    let mut filtered = filter_vendor_native_mcp_defs(store, agent);
    filtered.policy = policy;
    filtered.blocked = blocked;
    filtered.blocked_origins = blocked_origins;
    filtered
}

fn has_unportable_tool_policy(definition: &Value) -> bool {
    [
        "allowedTools",
        "deniedTools",
        "toolFilter",
        "allowed_tools",
        "denied_tools",
        "tool_filter",
    ]
    .iter()
    .any(|key| definition.get(key).is_some())
}

const UNPORTABLE_TOOL_POLICY: &str =
    "source tool policy cannot be represented by the portable MCP server";

/// MCP servers read from one JSON `mcpServers` source, with the source policy
/// and the names that cannot be imported. A name in `blocked` or disabled in
/// `policy` never appears in `store`.
#[derive(Debug, Default)]
struct JsonMcpSources {
    store: McpServerStore,
    policy: HashMap<String, Value>,
    blocked: HashMap<String, String>,
    /// The config file each imported name was read from.
    origins: HashMap<String, std::path::PathBuf>,
    /// The config file each blocked name was read from.
    blocked_origins: HashMap<String, std::path::PathBuf>,
}

/// Convert one Copilot or AGY `mcpServers` entry into the portable model.
/// AGY spells the remote address `serverUrl` and Copilot names stdio servers
/// `local`; both are carried into the portable `url` and `stdio` spellings.
fn json_mcp_server(definition: &Value) -> Option<McpServer> {
    let mut definition = definition.clone();
    let map = definition.as_object_mut()?;
    if !map.contains_key("url") {
        if let Some(url) = map.remove("serverUrl") {
            map.insert("url".into(), url);
        }
    }
    if map.get("type").and_then(Value::as_str) == Some("local") {
        map.insert("type".into(), Value::String("stdio".into()));
    }
    serde_json::from_value(definition).ok()
}

/// A Copilot `tools` list other than the unrestricted `["*"]` limits which
/// tools the server exposes, which the portable server cannot express.
fn copilot_restricts_tools(definition: &Value) -> bool {
    match definition.get("tools") {
        None | Some(Value::Null) => false,
        Some(Value::Array(tools)) => !(tools.len() == 1 && tools[0].as_str() == Some("*")),
        Some(_) => true,
    }
}

/// AGY records per-server tool exclusions in `disabledTools`.
fn agy_restricts_tools(definition: &Value) -> bool {
    match definition.get("disabledTools") {
        None | Some(Value::Null) => false,
        Some(Value::Array(tools)) => !tools.is_empty(),
        Some(_) => true,
    }
}

/// Read Copilot's `~/.copilot/mcp-config.json` servers. A name listed in
/// `disabledMcpServers` of `~/.copilot/settings.json` stays disabled and is not
/// imported; a restricted tool list blocks the server.
fn read_copilot_mcp_sources() -> JsonMcpSources {
    let mut sources = JsonMcpSources::default();
    let Some(path) = copilot_root().map(|p| p.join("mcp-config.json")) else {
        return sources;
    };
    let Some(raw) = read_json_file(Some(path.clone())) else {
        return sources;
    };
    let settings = copilot_settings().unwrap_or(Value::Null);
    let disabled: HashSet<&str> = settings
        .get("disabledMcpServers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let Some(servers) = raw.get("mcpServers").and_then(Value::as_object) else {
        return sources;
    };
    for (name, definition) in servers {
        let enabled = !disabled.contains(name.as_str());
        sources
            .policy
            .insert(name.clone(), json!({"enabled": enabled}));
        if has_unportable_tool_policy(definition) || copilot_restricts_tools(definition) {
            sources.blocked_origins.insert(name.clone(), path.clone());
            sources
                .blocked
                .insert(name.clone(), UNPORTABLE_TOOL_POLICY.to_string());
            continue;
        }
        if !enabled {
            continue;
        }
        if let Some(server) = json_mcp_server(definition) {
            sources.store.insert(name.clone(), server);
            sources.origins.insert(name.clone(), path.clone());
        }
    }
    sources
}

/// The MCP config each AGY face reads. The CLI and IDE faces share
/// `~/.gemini/config/mcp_config.json`; a face-local `mcp_config.json` beside a
/// face's skills root is read as well when present. Each physical file is read
/// once.
fn agy_mcp_config_paths() -> Vec<std::path::PathBuf> {
    let Some(home) = user_home() else {
        return Vec::new();
    };
    let gemini = home.join(".gemini");
    let mut paths = vec![gemini.join("config").join("mcp_config.json")];
    for root in agy_skill_roots() {
        if let Some(face) = root.parent() {
            if face != gemini {
                paths.push(face.join("mcp_config.json"));
            }
        }
    }
    paths
}

/// Read the AGY faces' MCP servers. Identical definitions across faces import
/// once; one name with divergent definitions across faces is a conflict that
/// imports nothing for that name. `disabled: true` keeps a server disabled and
/// a non-empty `disabledTools` blocks it.
fn read_agy_mcp_sources() -> JsonMcpSources {
    let mut sources = JsonMcpSources::default();
    let mut physical = HashSet::new();
    let mut seen: HashMap<String, (Value, std::path::PathBuf)> = HashMap::new();
    let mut conflicts = HashSet::new();
    for path in agy_mcp_config_paths() {
        let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if !physical.insert(key.clone()) {
            continue;
        }
        let Some(raw) = read_json_file(Some(path.clone())) else {
            continue;
        };
        let Some(servers) = raw.get("mcpServers").and_then(Value::as_object) else {
            continue;
        };
        for (name, definition) in servers {
            match seen.get(name) {
                Some((previous, _)) if previous != definition => {
                    conflicts.insert(name.clone());
                    continue;
                }
                Some(_) => continue,
                None => {
                    seen.insert(name.clone(), (definition.clone(), key.clone()));
                }
            }
            let enabled = definition.get("disabled").and_then(Value::as_bool) != Some(true);
            sources
                .policy
                .insert(name.clone(), json!({"enabled": enabled}));
            if has_unportable_tool_policy(definition) || agy_restricts_tools(definition) {
                sources.blocked_origins.insert(name.clone(), key.clone());
                sources
                    .blocked
                    .insert(name.clone(), UNPORTABLE_TOOL_POLICY.to_string());
                continue;
            }
            if !enabled {
                continue;
            }
            if let Some(server) = json_mcp_server(definition) {
                sources.store.insert(name.clone(), server);
                sources.origins.insert(name.clone(), key.clone());
            }
        }
    }
    for name in conflicts {
        sources.store.remove(&name);
        sources.origins.remove(&name);
        sources.policy.remove(&name);
        if let Some((_, origin)) = seen.get(&name) {
            sources.blocked_origins.insert(name.clone(), origin.clone());
        }
        sources.blocked.insert(
            name,
            "divergent definitions for one server name across AGY faces".to_string(),
        );
    }
    sources
}

fn toml_to_json(value: &toml::Value) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
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
        (AgentKind::Copilot, ItemKind::Plugin) => "copilot-plugin",
        (AgentKind::Copilot, ItemKind::McpServer) => "copilot-mcp",
        (AgentKind::Copilot, ItemKind::Skill) => "copilot-skill",
        (AgentKind::OpenCode, ItemKind::Plugin) => "opencode-plugin",
        (AgentKind::OpenCode, ItemKind::McpServer) => "opencode-mcp",
        (AgentKind::OpenCode, ItemKind::Skill) => "opencode-skill",
        (AgentKind::Agy, ItemKind::Plugin) => "agy-plugin",
        (AgentKind::Agy, ItemKind::McpServer) => "agy-mcp",
        (AgentKind::Agy, ItemKind::Skill) => "agy-skill",
    }
}

/// Enumerate skill directories under `skills_root`.
///
/// A skill is a child directory containing a `SKILL.md`. Each item records its
/// exact source directory in `source`. Graceful-missing: an absent or
/// unreadable root yields an empty set. Results are name-sorted.
#[cfg(test)]
fn enumerate_skill_dirs(skills_root: &Path, agent: AgentKind) -> Vec<InstalledItem> {
    enumerate_skill_dirs_reporting(skills_root, agent, &mut Vec::new())
}

/// [`enumerate_skill_dirs`], also leaving out every skill directory that the
/// projection ownership record (`_ccyncProjection` in the lockfile) proves
/// ccync published, and every directory that resolves into ccync's own
/// output. A direct copy that ccync wrote into a native root is never user
/// content, so a re-init must not import it as a loose skill. Each excluded
/// directory is appended to `owned_skipped` as `<name> (<path>)`.
fn enumerate_skill_dirs_reporting(
    skills_root: &Path,
    agent: AgentKind,
    owned_skipped: &mut Vec<String>,
) -> Vec<InstalledItem> {
    let mut items = Vec::new();
    let root = std::fs::canonicalize(skills_root).unwrap_or_else(|_| skills_root.to_path_buf());
    let Ok(layout) = ccync_foundation::paths::EffectiveLayout::load_or_legacy() else {
        return items;
    };
    let mut output_roots = vec![
        layout.machine_config_root(),
        layout.machine_state_root(),
        layout.machine_cache_root(),
        layout.state_base(),
        layout.dist_path(),
    ];
    if let Some(legacy_root) = ccync_foundation::paths::ccync_home() {
        output_roots.extend([
            legacy_root.join("plugins"),
            legacy_root.join("source"),
            legacy_root.join("build/render"),
            legacy_root.join("build/active"),
            legacy_root.join("data"),
            legacy_root.join("install-receipts"),
        ]);
    }
    let Ok(entries) = std::fs::read_dir(&root) else {
        return items;
    };
    let lock_path = layout.state_base().join("lock.json");
    let ownership = Some(projection::ManagedDeprojection::from_lockfile(&lock_path));
    for entry in entries.flatten() {
        let path = entry.path();
        let resolved = std::fs::canonicalize(&path).unwrap_or(path.clone());
        let is_generated = output_roots.iter().any(|generated| {
            let generated = std::fs::canonicalize(generated).unwrap_or_else(|_| generated.clone());
            resolved.starts_with(generated)
        });
        if !path.is_dir() || !resolved.join("SKILL.md").is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // The ownership record stores the path ccync wrote, which is the
        // home-derived form rather than the canonical one, so every spelling
        // of this directory is checked. A link into ccync's own output is
        // ccync's as well, whether or not a record names it.
        let owned = is_generated
            || ownership.as_ref().is_some_and(|ownership| {
                [skills_root.join(name), path.clone(), resolved.clone()]
                    .iter()
                    .flat_map(|candidate| [candidate.clone(), without_verbatim_prefix(candidate)])
                    .any(|candidate| ownership.is_authorized_for_removal(&candidate))
            });
        if owned {
            owned_skipped.push(format!("{name} ({})", skills_root.join(name).display()));
            continue;
        }
        items.push(InstalledItem {
            name: name.to_string(),
            version: None,
            agent: agent.clone(),
            kind: ItemKind::Skill,
            source: Some(path.to_string_lossy().into_owned()),
            baseline_version: None,
        });
    }
    items.sort_by(|a, b| a.name.cmp(&b.name));
    items
}

/// Drop the Windows verbatim prefix (`\\?\`) that `canonicalize` adds, so a
/// canonical path compares equal to the plain form recorded elsewhere.
fn without_verbatim_prefix(path: &Path) -> std::path::PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        std::path::PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        std::path::PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

fn agent_label(agent: &AgentKind) -> &'static str {
    match agent {
        AgentKind::Claude => "claude",
        AgentKind::Codex => "codex",
        AgentKind::Copilot => "copilot",
        AgentKind::OpenCode => "opencode",
        AgentKind::Agy => "agy",
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
    let _guard = crate::tool_manager::lock::LifecycleLock::acquire_if_needed()
        .map_err(|error| AdoptError::Write(error.to_string()))?;
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
    ccync_foundation::platform::atomic_write_bytes(lock_path, text.as_bytes())
        .map_err(|e| AdoptError::Write(e.to_string()))?;
    Ok(())
}

/// Select the loose skills (no owning plugin) from a skill-item set.
///
/// A skill whose name is in `plugin_owned` is bundled by a plugin and is
/// reprojected through that plugin's decomposition — it must NOT be double-marked
/// for canonical-root materialization. Non-skill items are ignored. Result is
/// name-deduplicated and sorted (deterministic).
fn mark_loose_skills(items: &[InstalledItem], plugin_owned: &HashSet<String>) -> Vec<LooseSkill> {
    // Identity is (name, exact source directory). Same-name items from different
    // origins stay separate rows so the materializer can block the whole group
    // instead of letting the first adopted origin win. The first item seen keeps
    // its source label for an identity that repeats.
    let mut skills: std::collections::BTreeMap<(String, Option<std::path::PathBuf>), &'static str> =
        std::collections::BTreeMap::new();
    for item in items {
        if item.kind == ItemKind::Skill && !plugin_owned.contains(&item.name) {
            let origin = item.source.as_ref().map(std::path::PathBuf::from);
            skills
                .entry((item.name.clone(), origin))
                .or_insert_with(|| agent_label(&item.agent));
        }
    }
    skills
        .into_iter()
        .map(|((name, origin), source)| LooseSkill {
            name,
            source: Some(source.to_string()),
            origin,
        })
        .collect()
}

/// Record loose skills with exact source directories while keeping the legacy
/// `InstalledItem` and `adopt_loose_skills` APIs stable.
pub fn adopt_loose_skills_with_origins(
    skills: &[LooseSkill],
    lock_path: &Path,
) -> Result<Vec<LooseSkill>, AdoptError> {
    if skills.is_empty() {
        return Ok(Vec::new());
    }
    let existing = read_lock_file(lock_path)?;
    let mut merged = existing
        .get(LOOSE_SKILLS_KEY)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for skill in skills {
        if let Ok(value) = serde_json::to_value(skill) {
            merged.retain(|entry| {
                !(entry.get("name").and_then(Value::as_str) == Some(&skill.name)
                    && entry.get("origin") == value.get("origin"))
            });
            merged.push(value);
        }
    }
    let updated = match existing {
        Value::Object(mut map) => {
            map.insert(LOOSE_SKILLS_KEY.to_string(), Value::Array(merged));
            Value::Object(map)
        }
        _ => json!({ LOOSE_SKILLS_KEY: Value::Array(merged) }),
    };
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| AdoptError::Write(e.to_string()))?;
    }
    let text =
        serde_json::to_string_pretty(&updated).map_err(|e| AdoptError::Write(e.to_string()))?;
    ccync_foundation::platform::atomic_write_bytes(lock_path, text.as_bytes())
        .map_err(|e| AdoptError::Write(e.to_string()))?;
    Ok(skills.to_vec())
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
    ccync_foundation::platform::atomic_write_bytes(lock_path, text.as_bytes())
        .map_err(|e| AdoptError::Write(e.to_string()))?;
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
        ccync_foundation::platform::atomic_write_bytes(lock_path, text.as_bytes())
            .map_err(|e| AdoptError::Write(e.to_string()))?;
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
    let mut origin_of_removed: Option<String> = None;
    let mut remaining: Vec<Value> = Vec::with_capacity(adopted_arr.len());
    for entry in adopted_arr {
        if source_id_of_removed.is_none() && entry.get("name").and_then(Value::as_str) == Some(id) {
            source_id_of_removed = entry
                .get("sourceId")
                .and_then(Value::as_str)
                .map(str::to_string);
            origin_of_removed = entry
                .get("source")
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
        // Remove only the row for the adopted origin. A same-name row from another
        // origin is a separate identity and stays. Legacy rows without an origin
        // are kept for rescan.
        let pruned: Vec<Value> = loose
            .into_iter()
            .filter(|e| {
                let same_name = e.get("name").and_then(Value::as_str) == Some(id);
                let same_origin =
                    e.get("origin").and_then(Value::as_str) == origin_of_removed.as_deref();
                !(same_name && same_origin)
            })
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
    ccync_foundation::platform::atomic_write_bytes(lock_path, text.as_bytes())
        .map_err(|e| AdoptError::Write(e.to_string()))?;

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
            )));
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

    #[test]
    fn adoption_store_normalizes_missing_plugins_and_preserves_private_extras() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("plugins.json");
        std::fs::write(&path, json!({"privateExtra":{"marker":"keep"}}).to_string()).unwrap();
        let record = AdoptionRecord {
            source: "claude".into(),
            kind: "skill".into(),
            component_id: "skill:hello".into(),
            name: "hello".into(),
            origin: Some("/private/origin".into()),
            revision: None,
            digest: None,
            enabled: Some(true),
            policy: None,
        };
        store_adoption_records(&path, &[record.clone()], &[]).unwrap();
        let stored: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored["plugins"], json!([]));
        assert_eq!(stored["privateExtra"]["marker"], "keep");
        assert_eq!(read_adoption_records(&path).unwrap(), vec![record]);
    }

    #[test]
    fn adoption_store_rejects_wrong_plugins_type_without_rewriting() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("plugins.json");
        let original = br#"{"plugins":{},"keep":true}"#;
        std::fs::write(&path, original).unwrap();
        let result = store_adoption_records(&path, &[], &[]);
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

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
        let mut items = parse_codex_mcp_servers(toml, None);
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
        let items = parse_codex_mcp_servers(toml, None);
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
        assert_eq!(parse_codex_mcp_servers(toml, None), vec![]);
    }

    #[test]
    fn parse_codex_mcp_servers_bad_toml_returns_empty() {
        assert_eq!(parse_codex_mcp_servers("not valid { toml [[", None), vec![]);
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
        let _env = env_guard();
        let dir = tempfile::tempdir().unwrap();
        set_home(dir.path());
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
                source: Some(
                    std::fs::canonicalize(&skills)
                        .unwrap()
                        .join("doc-sync")
                        .to_string_lossy()
                        .into_owned()
                ),
                baseline_version: None,
            }
        );
        assert_eq!(items[1].name, "git-commits");
        assert!(items.iter().all(|i| i.kind == ItemKind::Skill));
    }

    #[test]
    fn enumerate_skill_dirs_absent_root_is_empty() {
        let _env = env_guard();
        let dir = tempfile::tempdir().unwrap();
        set_home(dir.path());
        assert!(enumerate_skill_dirs(&dir.path().join("nope"), AgentKind::Codex).is_empty());
    }

    #[cfg(not(windows))]
    #[test]
    fn adopt_legacy_key_skips_managed_copy_and_preserves_user_file() {
        use sha2::{Digest, Sha256};
        let _env = env_guard();
        let dir = tempfile::tempdir().unwrap();
        set_home(dir.path());
        let skills = dir.path().join(".claude/skills");
        let managed = skills.join("published");
        let user = skills.join("mine/SKILL.md");
        std::fs::create_dir_all(&managed).unwrap();
        std::fs::write(managed.join("SKILL.md"), "managed").unwrap();
        std::fs::create_dir_all(user.parent().unwrap()).unwrap();
        std::fs::write(&user, "user content").unwrap();
        let before = Sha256::digest(std::fs::read(&user).unwrap());
        let lock = ccync_foundation::paths::plugins_lock_path().unwrap();
        std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
        std::fs::write(&lock, json!({"_ccyncProjection": {"skillProjectionPaths": [managed.to_string_lossy().replace('/', "\\")]}}).to_string()).unwrap();
        let mut skipped = Vec::new();
        let items = enumerate_skill_dirs_reporting(&skills, AgentKind::Claude, &mut skipped);
        assert_eq!(items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), vec!["mine"]);
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].starts_with("published ("));
        assert!(managed.join("SKILL.md").exists());
        assert_eq!(before, Sha256::digest(std::fs::read(&user).unwrap()));
    }

    #[test]
    fn enumerate_skill_dirs_excludes_and_reports_ccync_owned_copies() {
        let _env = env_guard();
        let dir = tempfile::tempdir().unwrap();
        set_home(dir.path());
        let skills = dir.path().join(".claude").join("skills");
        for name in ["mine", "published"] {
            std::fs::create_dir_all(skills.join(name)).unwrap();
            std::fs::write(skills.join(name).join("SKILL.md"), "# s\n").unwrap();
        }
        // The ownership record names the home-derived path that ccync wrote,
        // in the registry's normalized spelling.
        let recorded = projection::normalize_managed_artifact_path(&skills.join("published"));
        let lock = ccync_foundation::paths::plugins_lock_path().unwrap();
        std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
        std::fs::write(
            &lock,
            json!({"_ccyncProjection": {"skillProjectionPaths": [recorded]}}).to_string(),
        )
        .unwrap();

        let mut skipped = Vec::new();
        let items = enumerate_skill_dirs_reporting(&skills, AgentKind::Claude, &mut skipped);
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["mine"], "a ccync-owned copy is never adopted");
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert!(skipped[0].starts_with("published ("), "{skipped:?}");
    }

    #[test]
    fn mark_loose_skills_records_each_skill_origin() {
        let mut item = skill_item("doc-sync", AgentKind::Claude);
        item.source = Some("/home/u/.claude/skills/doc-sync".into());
        let loose = mark_loose_skills(&[item], &HashSet::new());
        assert_eq!(
            loose[0].origin.as_deref(),
            Some(Path::new("/home/u/.claude/skills/doc-sync"))
        );
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
        let items = parse_codex_mcp_servers(toml, None);
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
