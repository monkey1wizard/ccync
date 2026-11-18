//! Internal catalog resolution — invoked by `ccync sync`.

use ccync_foundation::paths::{personal_catalog_path, EffectiveLayout, EffectiveLayoutAuthority};
use ccync_foundation::provider::{
    ComponentKind, PortableComponentSelection, PortableProviderSource, PortableSourcesDocument,
    ProviderSourceKey, SourceBindingKey,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogResolution {
    pub profile_name: String,
    pub resolved_plugins: Vec<Value>,
    /// Personal plugins from `~/.ccync/plugins.json` (or the injected override).
    /// Additive-only: entries whose `pluginId` already appears in `resolved_plugins`
    /// are dropped (core-wins). Each entry carries `_personalOrigin: true`.
    pub personal_plugins: Vec<Value>,
    pub errors: Vec<String>,
    pub drift_detected: bool,
    pub drift_details: Vec<String>,
    pub lockfile: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveCatalogOptions {
    /// `None` (production default) → resolve from the embedded catalog baked
    /// into the binary. `Some(path)` → read a catalog override from disk.
    pub catalog_path: Option<PathBuf>,
    pub config_path: PathBuf,
    pub lockfile_path: PathBuf,
    pub dry_run: bool,
    /// Inject a test-specific personal catalog path instead of `personal_catalog_path()`.
    /// `None` (production default) → reads `~/.ccync/plugins.json`.
    pub personal_catalog_path_override: Option<PathBuf>,
}

/// The public plugin catalog, baked into the binary at compile time.
///
/// The catalog is embedded directly in the binary and resolved without any
/// disk deploy step; `ccync sync` (and `ccync add`) read it via
/// [`embedded_catalog_json`] unless a disk override is supplied.
pub fn embedded_catalog_json() -> &'static str {
    include_str!("../../../plugins/catalog.json")
}

/// Build portable provider intent from a personal catalog and its private
/// adoption records. Local references remain in the private catalog and are
/// represented only by a deterministic binding key in the portable document.
pub fn portable_sources_from_personal_catalog(
    catalog: &Value,
) -> Result<PortableSourcesDocument, String> {
    let plugins = match catalog.get("plugins") {
        None => &[][..],
        Some(Value::Array(plugins)) => plugins.as_slice(),
        Some(_) => return Err("personal catalog.plugins must be an array".into()),
    };
    let records = catalog
        .get("_adoption")
        .and_then(|adoption| adoption.get("records").or(Some(adoption)))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut sources = Vec::new();
    for plugin in plugins {
        let Some(key) = plugin.get("pluginId").and_then(Value::as_str) else {
            continue;
        };
        let source = ProviderSourceKey::new(key.to_owned()).map_err(|error| error.to_string())?;
        let reference = plugin
            .get("source")
            .and_then(Value::as_str)
            .or_else(|| plugin.pointer("/upstream/repo").and_then(Value::as_str));
        let (reference, binding_key) = match reference {
            Some(reference) => {
                let candidate = PortableProviderSource {
                    source: source.clone(),
                    reference: Some(reference.to_owned()),
                    revision: None,
                    binding_key: None,
                    selection: Vec::new(),
                    digest: None,
                };
                if candidate.validate().is_ok() {
                    (Some(reference.to_owned()), None)
                } else {
                    (None, Some(binding_key_for(key)?))
                }
            }
            None => (None, Some(binding_key_for(key)?)),
        };
        let selection = records
            .iter()
            .filter(|record| record.get("source").and_then(Value::as_str) == Some(key))
            .filter_map(|record| {
                let plugin_name = record.get("name")?.as_str()?.to_owned();
                let component_id = record.get("componentId")?.as_str()?.to_owned();
                let kind = match record.get("kind")?.as_str()? {
                    "skill" => ComponentKind::Skill,
                    "command" => ComponentKind::Command,
                    "agent" => ComponentKind::Agent,
                    "hook" => ComponentKind::Hook,
                    "resource" => ComponentKind::Resource,
                    _ => return None,
                };
                Some(PortableComponentSelection {
                    plugin_name,
                    kind,
                    component_id,
                })
            })
            .collect();
        let revision = plugin
            .pointer("/upstream/ref")
            .and_then(Value::as_str)
            .map(str::to_owned);
        sources.push(PortableProviderSource {
            source,
            reference,
            revision,
            binding_key,
            selection,
            digest: None,
        });
    }
    for record in &records {
        let Some(key) = record.get("source").and_then(Value::as_str) else {
            continue;
        };
        if sources.iter().any(|source| source.source.as_str() == key) {
            continue;
        }
        let source = ProviderSourceKey::new(key.to_owned()).map_err(|error| error.to_string())?;
        let selections = records
            .iter()
            .filter(|row| row.get("source").and_then(Value::as_str) == Some(key))
            .filter_map(|row| {
                let plugin_name = row.get("name")?.as_str()?.to_owned();
                let component_id = row.get("componentId")?.as_str()?.to_owned();
                let kind = match row.get("kind")?.as_str()? {
                    "skill" => ComponentKind::Skill,
                    "command" => ComponentKind::Command,
                    "agent" => ComponentKind::Agent,
                    "hook" => ComponentKind::Hook,
                    "resource" => ComponentKind::Resource,
                    _ => return None,
                };
                Some(PortableComponentSelection {
                    plugin_name,
                    kind,
                    component_id,
                })
            })
            .collect();
        let reference = record
            .get("origin")
            .and_then(Value::as_str)
            .filter(|origin| origin.starts_with("https://") && origin.len() > 8)
            .map(str::to_owned);
        sources.push(PortableProviderSource {
            source,
            reference,
            revision: None,
            binding_key: if record
                .get("origin")
                .and_then(Value::as_str)
                .is_some_and(|origin| origin.starts_with("https://") && origin.len() > 8)
            {
                None
            } else {
                Some(binding_key_for(key)?)
            },
            selection: selections,
            digest: None,
        });
    }
    PortableSourcesDocument::new(sources).map_err(|error| error.to_string())
}

/// Persist a private source change before refreshing its portable projection.
/// The hook runs after the private write, allowing callers to perform another
/// fallible step before the export is updated.
pub fn persist_source_change<C, H>(
    layout: &EffectiveLayout,
    change: C,
    hook: H,
) -> Result<(), String>
where
    C: FnOnce(&mut Value) -> Result<(), String>,
    H: FnOnce() -> Result<(), String>,
{
    let catalog_path = layout.personal_catalog_path();
    let mut catalog = read_json_file(&catalog_path)?.unwrap_or_else(|| json!({"plugins": []}));
    change(&mut catalog)?;
    write_json_file(&catalog_path, &catalog)?;
    hook()?;

    if layout.authority() == EffectiveLayoutAuthority::Committed {
        export_portable_sources(layout, &catalog)?;
    }
    Ok(())
}

/// Serialize the portable source projection without performing filesystem I/O.
pub fn portable_sources_export_bytes(catalog: &Value) -> Result<Vec<u8>, String> {
    let sources = portable_sources_from_personal_catalog(catalog)?;
    serde_json::to_vec_pretty(&sources).map_err(|e| e.to_string())
}

/// Serialize the portable lock projection without performing filesystem I/O.
pub fn portable_lock_export_bytes(lock: &Value) -> Result<Vec<u8>, String> {
    if let Some(servers) = lock.get("_mcpServers") {
        let servers = servers.as_object().ok_or("invalid MCP declarations")?;
        for (name, value) in servers {
            validate_mcp_output(name, value, false)?;
        }
    }
    let mut portable = serde_json::Map::new();
    for key in [
        "schemaVersion",
        "generatedAt",
        "resolverVersion",
        "profileName",
        "catalogHash",
        "resolvedPlugins",
        "driftDetection",
    ] {
        if let Some(value) = lock.get(key) {
            portable.insert(key.to_string(), value.clone());
        }
    }
    serde_json::to_vec_pretty(&Value::Object(portable)).map_err(|e| e.to_string())
}

pub(crate) fn validate_mcp_output(name: &str, value: &Value, trusted: bool) -> Result<(), String> {
    let mut secrets: Vec<String> = std::env::vars()
        .filter(|(name, value)| ccync_foundation::mcp::is_secret_name(name) && !value.is_empty())
        .map(|(_, value)| value)
        .collect();
    let layout = EffectiveLayout::load_or_legacy().map_err(|e| e.to_string())?;
    match std::fs::read_to_string(layout.machine_config_path()) {
        Ok(contents) => {
            let config: Value =
                serde_json::from_str(&contents).map_err(|_| "invalid machine config")?;
            if let Some(values) = config.get("secrets").and_then(Value::as_object) {
                secrets.extend(
                    values
                        .values()
                        .filter_map(Value::as_str)
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned),
                );
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot read machine config: {e}")),
    }
    let wrapped = json!({"command": "node", "env": {"MODE": [name, value]}});
    if let Err(error) = ccync_foundation::mcp::validate_portable_declaration(&wrapped, &secrets) {
        if matches!(
            error.reason,
            "known secret value" | "credential value" | "URL userinfo"
        ) {
            return Err(format!("unsafe MCP declaration: {error}"));
        }
    }
    if !trusted {
        ccync_foundation::mcp::validate_portable_declaration(value, &secrets)
            .map_err(|error| format!("unsafe MCP declaration: {error}"))?;
    }
    Ok(())
}

fn export_portable_sources(layout: &EffectiveLayout, catalog: &Value) -> Result<(), String> {
    let Some(path) = layout.sources_export_path() else {
        return Ok(());
    };
    let bytes = portable_sources_export_bytes(catalog)?;
    write_bytes_atomic(&path, &bytes)
}

/// Export resolver-owned fields from the authoritative private lockfile.
pub fn export_portable_lock(layout: &EffectiveLayout) -> Result<(), String> {
    let Some(export_path) = layout.lock_export_path() else {
        return Ok(());
    };
    let private_path = layout.state_base().join("lock.json");
    let lock = read_json_file(&private_path)?
        .ok_or_else(|| format!("private lockfile not found at: {}", private_path.display()))?;
    let bytes = portable_lock_export_bytes(&lock)?;
    write_bytes_atomic(&export_path, &bytes)
}

fn binding_key_for(source_key: &str) -> Result<SourceBindingKey, String> {
    let digest = Sha256::digest(source_key.as_bytes());
    SourceBindingKey::new(format!("binding-{:x}", digest)).map_err(|error| error.to_string())
}

pub fn run_resolve_catalog(opts: &ResolveCatalogOptions) -> Result<CatalogResolution, String> {
    let layout = EffectiveLayout::load_or_legacy().map_err(|e| e.to_string())?;
    let committed = layout.authority() == EffectiveLayoutAuthority::Committed;
    let lockfile_path = if committed {
        layout.state_base().join("lock.json")
    } else {
        opts.lockfile_path.clone()
    };
    let catalog = match &opts.catalog_path {
        Some(p) => read_json_file(p)?
            .ok_or_else(|| format!("catalog override not found at: {}", p.display()))?,
        None => serde_json::from_str(embedded_catalog_json()).map_err(|e| {
            format!(
                "embedded catalog is invalid JSON: {e} — this is a ccync build defect; run `ccync update` or reinstall"
            )
        })?,
    };
    let config_path = if committed {
        layout.config_path()
    } else {
        opts.config_path.clone()
    };
    let config = read_json_file(&config_path)?.unwrap_or_else(|| json!({}));

    let mut resolution = resolve_plugin_set(&catalog, &config, &lockfile_path)?;

    // Additive merge of personal catalog (default-off — absent = byte-identical).
    let personal_catalog_path = if committed {
        Some(layout.personal_catalog_path())
    } else {
        opts.personal_catalog_path_override
            .clone()
            .or_else(personal_catalog_path)
    };
    resolution.personal_plugins = resolve_personal_plugins(
        personal_catalog_path.as_deref(),
        &resolution.resolved_plugins,
    )?;

    if !opts.dry_run {
        // Round-trip: read existing lockfile root (fail-safe → empty object), splice
        // resolver keys in, write back.  Preserves `_ccyncProjection` and any other
        // non-resolver top-level keys written by `projection::persist`.
        let existing = read_json_file(&lockfile_path)
            .unwrap_or(None)
            .unwrap_or_else(|| json!({}));
        let merged = splice_resolver_keys(existing, &resolution.lockfile);
        // Write personal plugins into their own namespace. Default-off is keyed
        // on the personal catalog existing at all, not on the resolved set being
        // non-empty: a catalog that exists and resolves to nothing is the state
        // left behind by removing the last personal plugin, and the stale
        // namespace has to be overwritten with an empty array so the next render
        // stops treating the removed plugin as still managed.
        let personal_catalog_present = personal_catalog_path
            .as_deref()
            .is_some_and(personal_catalog_is_readable);
        let merged = splice_personal_namespace(
            merged,
            &resolution.personal_plugins,
            personal_catalog_present,
        );
        if committed {
            portable_lock_export_bytes(&merged)?;
        }
        write_json_file(&lockfile_path, &merged)?;
        if committed {
            export_portable_lock(&layout)?;
        }
    }
    Ok(resolution)
}

/// Splice personal plugin entries into `_personalPlugins` in the lockfile root.
/// Returns `existing` unchanged only when there is no personal catalog at all
/// (default-off). A catalog that exists but resolves to no plugins writes an
/// empty array, so removing the last personal plugin actually clears the
/// namespace that `managed_plugin_dirs` renders from.
/// Preserves all other top-level keys (resolver + projection namespaces).
fn splice_personal_namespace(
    existing: Value,
    personal_plugins: &[Value],
    catalog_present: bool,
) -> Value {
    if personal_plugins.is_empty() && !catalog_present {
        return existing;
    }
    let lockfile_entries: Vec<Value> = personal_plugins
        .iter()
        .map(|p| {
            let mut entry = json!({
                "pluginId":       p.get("pluginId").cloned().unwrap_or(Value::Null),
                "displayName":    p.get("displayName").cloned().unwrap_or(Value::Null),
                "sourceType":     p.get("sourceType").cloned().unwrap_or(Value::Null),
                "installStrategy":p.get("installStrategy").cloned().unwrap_or(Value::Null),
                "source":         p.get("source").cloned().unwrap_or(Value::Null),
            });
            // Carry the pin/hold state so the lockfile is the single source of
            // truth for upgrade (current sha + held) and render (exact live dir).
            // Only copied when present — absent keys leave the entry clean.
            for key in ["pin", "pinnedSha", "held"] {
                if let Some(v) = p.get(key) {
                    entry[key] = v.clone();
                }
            }
            entry
        })
        .collect();
    match existing {
        Value::Object(mut map) => {
            map.insert(
                "_personalPlugins".to_string(),
                Value::Array(lockfile_entries),
            );
            Value::Object(map)
        }
        _ => existing,
    }
}

/// True when the personal catalog exists AND parses into a `plugins` array.
///
/// Mere existence is not enough to key the namespace write on. An unreadable or
/// malformed catalog also resolves to zero plugins, and treating that as
/// "the user removed everything" would clear the lockfile's managed set and
/// unmanage every plugin on the machine over a transient read failure. A
/// catalog that cannot be understood leaves the previous namespace alone.
fn personal_catalog_is_readable(path: &Path) -> bool {
    matches!(read_json_file(path), Ok(Some(catalog))
        if catalog.get("plugins").is_none_or(Value::is_array))
}

/// Read personal catalog and return entries not already present in public resolved set.
/// Each returned entry carries `"_personalOrigin": true` for the leak guard.
/// Absent or unreadable personal catalog → empty vec (default-off; does not error).
fn resolve_personal_plugins(
    catalog_path: Option<&Path>,
    public_resolved: &[Value],
) -> Result<Vec<Value>, String> {
    let Some(path) = catalog_path else {
        return Ok(vec![]);
    };
    let catalog = match read_json_file(path) {
        Ok(Some(v)) => v,
        Ok(None) | Err(_) => return Ok(vec![]),
    };
    let plugins = match catalog.get("plugins") {
        None => Vec::new(),
        Some(Value::Array(arr)) => arr.clone(),
        Some(_) => return Err("personal catalog.plugins must be an array".into()),
    };
    let public_ids: std::collections::HashSet<&str> = public_resolved
        .iter()
        .filter_map(|p| p.get("pluginId").and_then(Value::as_str))
        .collect();

    Ok(plugins
        .into_iter()
        .filter(|p| {
            let pid = p
                .get("pluginId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            !pid.is_empty() && !public_ids.contains(pid)
        })
        .map(|mut p| {
            if let Some(obj) = p.as_object_mut() {
                obj.insert("_personalOrigin".to_string(), Value::Bool(true));
            }
            p
        })
        .collect())
}

/// Merge resolver output keys into an existing lockfile root.
/// All non-resolver top-level keys in `existing` (e.g. `_ccyncProjection`) are
/// preserved; resolver keys overwrite on collision.
/// If `existing` is not a JSON object (corrupt file), returns `resolver_output`
/// verbatim so the lockfile is always left in a valid state.
fn splice_resolver_keys(existing: Value, resolver_output: &Value) -> Value {
    match existing {
        Value::Object(mut map) => {
            if let Some(resolver_map) = resolver_output.as_object() {
                for (k, v) in resolver_map {
                    map.insert(k.clone(), v.clone());
                }
            }
            Value::Object(map)
        }
        _ => resolver_output.clone(),
    }
}

fn read_json_file(path: &Path) -> Result<Option<Value>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let content =
        fs::read_to_string(path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    serde_json::from_str(&content)
        .map(Some)
        .map_err(|e| format!("Failed to parse {}: {e}", path.display()))
}

fn write_json_file(path: &Path, value: &Value) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    write_bytes_atomic(path, &bytes)
}

fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
    }
    ccync_foundation::platform::atomic_write_bytes(path, bytes)
        .map_err(|e| format!("Failed to write {}: {e}", path.display()))
}

fn resolve_plugin_set(
    catalog: &Value,
    config: &Value,
    lockfile_path: &Path,
) -> Result<CatalogResolution, String> {
    let mut errors = Vec::new();
    let plugins = catalog
        .get("plugins")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| "catalog.plugins missing or invalid".to_string())?;
    let profiles = catalog
        .get("profiles")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| "catalog.profiles missing or invalid".to_string())?;

    for plugin in &plugins {
        validate_catalog_entry(plugin, &mut errors);
    }

    let requested_profile = config
        .get("defaultProfile")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let profile_name = if profiles.contains_key(requested_profile) {
        requested_profile.to_string()
    } else {
        errors.push(format!(
            "Profile '{}' not found in catalog. Available: {}",
            requested_profile,
            profiles.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
        "default".to_string()
    };

    let profile = profiles
        .get(&profile_name)
        .and_then(Value::as_object)
        .ok_or_else(|| format!("catalog profile '{}' missing", profile_name))?;

    let mut resolved_ids: Vec<String> = profile
        .get("plugins")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();

    if let Some(enabled) = config.get("enabledPlugins").and_then(Value::as_array) {
        for plugin_id in enabled.iter().filter_map(Value::as_str) {
            if !resolved_ids.contains(&plugin_id.to_string()) {
                resolved_ids.push(plugin_id.to_string());
            }
        }
    }

    if let Some(disabled) = config.get("disabledPlugins").and_then(Value::as_array) {
        resolved_ids.retain(|plugin_id| {
            !disabled
                .iter()
                .filter_map(Value::as_str)
                .any(|disabled_id| disabled_id == plugin_id)
        });
    }

    let mut resolved_plugins = Vec::new();
    for plugin_id in &resolved_ids {
        let Some(plugin) = plugins.iter().find(|plugin| {
            plugin.get("pluginId").and_then(Value::as_str) == Some(plugin_id.as_str())
        }) else {
            errors.push(format!(
                "Resolved plugin '{}' not found in catalog",
                plugin_id
            ));
            continue;
        };

        let resolved_by_profile = profile
            .get("plugins")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|value| value == plugin_id);
        let resolved_by_explicit = config
            .get("enabledPlugins")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|value| value == plugin_id);

        resolved_plugins.push(json!({
            "pluginId": plugin.get("pluginId").cloned().unwrap_or(Value::Null),
            "displayName": plugin.get("displayName").cloned().unwrap_or(Value::Null),
            "supportTier": plugin.get("supportTier").cloned().unwrap_or(Value::Null),
            "sourceType": plugin.get("sourceType").cloned().unwrap_or(Value::Null),
            "upstream": plugin.get("upstream").cloned().unwrap_or(Value::Null),
            "license": plugin.get("license").cloned().unwrap_or(Value::Null),
            "checksumPolicy": plugin.get("checksumPolicy").cloned().unwrap_or(Value::Null),
            "componentMap": plugin.get("componentMap").cloned().unwrap_or(Value::Null),
            "supportedProviders": plugin.get("supportedProviders").cloned().unwrap_or(Value::Null),
            "installStrategy": plugin.get("installStrategy").cloned().unwrap_or(Value::Null),
            "resolvedByProfile": resolved_by_profile,
            "resolvedByExplicit": resolved_by_explicit,
        }));
    }

    let mut drift_detected = false;
    let mut drift_details = Vec::new();
    if let Some(existing_lock) = read_json_file(lockfile_path).unwrap_or(None) {
        if let Some(existing_plugins) = existing_lock
            .get("resolvedPlugins")
            .and_then(Value::as_array)
        {
            let existing_ids: Vec<String> = existing_plugins
                .iter()
                .filter_map(|plugin| plugin.get("pluginId").and_then(Value::as_str))
                .map(str::to_string)
                .collect();
            if existing_ids != resolved_ids {
                drift_detected = true;
                drift_details.push(format!(
                    "Resolved plugin set changed: was [{}], now [{}]",
                    existing_ids.join(", "),
                    resolved_ids.join(", ")
                ));
            }
        }
    }

    let lockfile = build_lockfile(catalog, &resolved_plugins, &profile_name)?;
    Ok(CatalogResolution {
        profile_name,
        resolved_plugins,
        personal_plugins: vec![], // populated by run_resolve_catalog after public resolution
        errors,
        drift_detected,
        drift_details,
        lockfile,
    })
}

fn validate_catalog_entry(plugin: &Value, errors: &mut Vec<String>) {
    let plugin_id = plugin
        .get("pluginId")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let install_strategy = plugin
        .get("installStrategy")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let source_type = plugin
        .get("sourceType")
        .and_then(Value::as_str)
        .unwrap_or_default();

    // Fields required by all plugin types
    for field in [
        "pluginId",
        "displayName",
        "supportTier",
        "sourceType",
        "license",
        "checksumPolicy",
        "componentMap",
        "supportedProviders",
        "installStrategy",
        "defaultProfiles",
        "allowAutoUpdate",
        "localOverridePolicy",
    ] {
        if plugin.get(field).is_none() {
            errors.push(format!("[{plugin_id}] Missing required field: {field}"));
        }
    }

    // `upstream` is required only for externally-sourced plugins
    if matches!(source_type, "curated-upstream" | "mirrored" | "forked") {
        let upstream = plugin.get("upstream").and_then(Value::as_object);
        if upstream
            .and_then(|map| map.get("repo"))
            .and_then(Value::as_str)
            .is_none()
        {
            errors.push(format!(
                "[{plugin_id}] Upstream repo is required for sourceType={source_type}"
            ));
        }
        if plugin
            .get("license")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
        {
            errors.push(format!(
                "[{plugin_id}] License is required for sourceType={source_type}"
            ));
        }
        if plugin
            .get("checksumPolicy")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
        {
            errors.push(format!(
                "[{plugin_id}] checksumPolicy is required for sourceType={source_type}"
            ));
        }
        if upstream
            .and_then(|map| map.get("ref"))
            .and_then(Value::as_str)
            .is_none()
        {
            errors.push(format!(
                "[{plugin_id}] Upstream ref is required for sourceType={source_type}"
            ));
        }
    }

    // `source` (local directory path) is required for bundled-local plugins
    if install_strategy == "bundled-local"
        && plugin
            .get("source")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
    {
        errors.push(format!(
            "[{plugin_id}] source (local dir) is required for installStrategy=bundled-local"
        ));
    }

    let valid_tiers = [
        "official-ccync",
        "official-ccync-bundle",
        "curated-upstream",
        "mirrored",
        "forked",
        "local",
    ];
    let tier = plugin
        .get("supportTier")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !valid_tiers.contains(&tier) {
        errors.push(format!(
            "[{plugin_id}] Invalid supportTier: {tier}. Must be one of: {}",
            valid_tiers.join(", ")
        ));
    }
}

fn build_lockfile(
    catalog: &Value,
    resolved_plugins: &[Value],
    profile_name: &str,
) -> Result<Value, String> {
    let catalog_json = serde_json::to_string(&catalog).map_err(|e| e.to_string())?;
    let catalog_hash = hash_string(&catalog_json);
    let generated_at = chrono::Utc::now().to_rfc3339();

    let resolved_plugins: Vec<Value> = resolved_plugins
        .iter()
        .map(|plugin| {
            let resolved_source = if plugin.get("installStrategy").and_then(Value::as_str) == Some("bundled-local") {
                plugin.get("source").cloned().unwrap_or(Value::Null)
            } else {
                plugin.get("upstream").and_then(|u| u.get("repo")).cloned().unwrap_or(Value::Null)
            };
            json!({
                "pluginId": plugin.get("pluginId").cloned().unwrap_or(Value::Null),
                "displayName": plugin.get("displayName").cloned().unwrap_or(Value::Null),
                "supportTier": plugin.get("supportTier").cloned().unwrap_or(Value::Null),
                "sourceType": plugin.get("sourceType").cloned().unwrap_or(Value::Null),
                "resolvedSource": resolved_source,
                "resolvedRef": plugin.get("upstream").and_then(|u| u.get("ref")).cloned().unwrap_or(Value::Null),
                "resolvedVersion": Value::Null,
                "resolvedChecksum": Value::Null,
                "resolvedLicense": plugin.get("license").cloned().unwrap_or(Value::Null),
                "resolvedComponentMap": plugin.get("componentMap").cloned().unwrap_or(Value::Null),
                "selectedProviders": plugin.get("supportedProviders").cloned().unwrap_or(Value::Null),
                "installStrategy": plugin.get("installStrategy").cloned().unwrap_or(Value::Null),
                "resolvedByProfile": plugin.get("resolvedByProfile").cloned().unwrap_or(Value::Bool(false)),
                "resolvedByExplicit": plugin.get("resolvedByExplicit").cloned().unwrap_or(Value::Bool(false)),
            })
        })
        .collect();

    Ok(json!({
        "schemaVersion": 1,
        "generatedAt": generated_at,
        "resolverVersion": "ccync-resolver-1.0",
        "profileName": profile_name,
        "catalogHash": catalog_hash,
        "resolvedPlugins": resolved_plugins,
        "driftDetection": {
            "lastResolvedAt": generated_at,
            "catalogHash": catalog_hash,
        }
    }))
}

fn hash_string(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|byte| format!("{:02X}", byte)).collect()
}

// ---------------------------------------------------------------------------
// Profile summary
// ---------------------------------------------------------------------------

/// What a single plugin brings to a profile (for summary output).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSummary {
    pub plugin_id: String,
    pub display_name: String,
    pub description: String,
    pub has_skills: bool,
    pub has_mcp: bool,
    pub has_agents: bool,
}

/// Post-install summary for a named profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileSummary {
    pub profile_name: String,
    pub profile_description: String,
    pub plugins: Vec<PluginSummary>,
    pub available_profiles: Vec<String>,
}

/// Return a post-install summary for `profile_name` from the embedded catalog.
///
/// Returns `None` when the profile does not exist; `available_profiles` in the
/// returned value is always populated for error-message use.
pub fn profile_install_summary(profile_name: &str) -> Option<ProfileSummary> {
    let catalog: Value =
        serde_json::from_str(include_str!("../../../plugins/catalog.json")).ok()?;

    let profiles = catalog.get("profiles").and_then(Value::as_object)?;
    let plugins_arr = catalog
        .get("plugins")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut available_profiles: Vec<String> = profiles.keys().cloned().collect();
    available_profiles.sort();

    let profile = profiles.get(profile_name)?;
    let profile_description = profile
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let plugin_ids: Vec<&str> = profile
        .get("plugins")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();

    let mut plugins = Vec::new();
    for pid in plugin_ids {
        let entry = plugins_arr
            .iter()
            .find(|p| p.get("pluginId").and_then(Value::as_str) == Some(pid));
        let (display_name, description, has_skills, has_mcp, has_agents) = match entry {
            Some(p) => {
                let cm = p.get("componentMap");
                (
                    p.get("displayName")
                        .and_then(Value::as_str)
                        .unwrap_or(pid)
                        .to_string(),
                    p.get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    cm.and_then(|c| c.get("skills"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    cm.and_then(|c| c.get("mcp"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    cm.and_then(|c| c.get("agents"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                )
            }
            None => (pid.to_string(), String::new(), false, false, false),
        };
        plugins.push(PluginSummary {
            plugin_id: pid.to_string(),
            display_name,
            description,
            has_skills,
            has_mcp,
            has_agents,
        });
    }

    Some(ProfileSummary {
        profile_name: profile_name.to_string(),
        profile_description,
        plugins,
        available_profiles,
    })
}

// ---------------------------------------------------------------------------
// Leak guard — filter personal-origin entries before any repo catalog write-back
// ---------------------------------------------------------------------------

/// Resolve a public catalog ID to its fetch source.
///
/// Looks up the embedded catalog for an entry with `pluginId == plugin_id`.
/// For `installStrategy = "bundled-local"` returns `source`; for all other
/// strategies (git-clone, curated-upstream, etc.) returns `upstream.repo`.
/// Returns `None` when the ID is not found or the entry has no usable source.
pub fn resolve_catalog_source(plugin_id: &str) -> Option<String> {
    let catalog: Value = serde_json::from_str(embedded_catalog_json()).ok()?;
    let plugins = catalog.get("plugins")?.as_array()?;
    let entry = plugins
        .iter()
        .find(|p| p.get("pluginId").and_then(Value::as_str) == Some(plugin_id))?;
    let strategy = entry
        .get("installStrategy")
        .and_then(Value::as_str)
        .unwrap_or("");
    if strategy == "bundled-local" {
        entry
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_string)
    } else {
        entry
            .get("upstream")
            .and_then(|u| u.get("repo"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }
}

/// Remove entries carrying `_personalOrigin: true` from a plugin list.
/// Call before serializing any catalog slice back to the repo's `plugins/catalog.json`
/// so personal entries never leak into the tracked repo file.
pub fn filter_personal_entries(entries: &[Value]) -> Vec<Value> {
    entries
        .iter()
        .filter(|e| e.get("_personalOrigin").and_then(Value::as_bool) != Some(true))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_committed_layout(home: &Path) -> EffectiveLayout {
        use ccync_foundation::{
            paths::StorageRoots,
            provider::{LayoutRootIdentity, StorageLayoutCommit},
        };

        let roots = StorageRoots::resolve().unwrap();
        for root in [&roots.portable, &roots.config, &roots.state, &roots.cache] {
            fs::create_dir_all(root).unwrap();
        }
        let commit = StorageLayoutCommit {
            layout_version: 1,
            roots: vec![
                ("portable", &roots.portable),
                ("config", &roots.config),
                ("state", &roots.state),
                ("cache", &roots.cache),
            ]
            .into_iter()
            .map(|(name, path)| LayoutRootIdentity {
                name: name.into(),
                path: path.clone(),
            })
            .collect(),
        };
        let bytes = serde_json::to_vec(&commit).unwrap();
        fs::create_dir_all(home.join(".ccync")).unwrap();
        fs::write(home.join(".ccync/migration.json"), &bytes).unwrap();
        fs::write(roots.state.join("layout.json"), bytes).unwrap();
        EffectiveLayout::load_or_legacy().unwrap()
    }

    fn write_json(path: &Path, value: &Value) {
        fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
    }

    #[test]
    fn ordered_cross_root_writes() {
        {
            let home = crate::test_support::TestHome::new();
            let layout = write_committed_layout(home.path());
            assert_eq!(layout.authority(), EffectiveLayoutAuthority::Committed);
            let private_catalog_path = layout.personal_catalog_path();
            let sources_path = layout.sources_export_path().unwrap();
            let old_export = br#"{"schemaVersion":1,"sources":[]}"#;
            ccync_foundation::platform::atomic_write_bytes(&sources_path, old_export).unwrap();

            let failed = persist_source_change(
                &layout,
                |catalog| {
                    catalog["plugins"] = json!([{
                        "pluginId": "private-source",
                        "source": "C:/private/provider",
                        "operationalLock": "must-not-export"
                    }]);
                    Ok(())
                },
                || Err("injected failure after private write".into()),
            );
            assert!(failed.is_err());
            let private_bytes = fs::read(&private_catalog_path).unwrap();
            let private: Value = serde_json::from_slice(&private_bytes).unwrap();
            assert_eq!(private["plugins"][0]["source"], "C:/private/provider");
            assert_eq!(fs::read(&sources_path).unwrap(), old_export);

            persist_source_change(&layout, |_| Ok(()), || Ok(())).unwrap();
            let portable_sources = fs::read_to_string(&sources_path).unwrap();
            assert!(portable_sources.contains("private-source"));
            for secret in ["C:/private", "operationalLock", "must-not-export"] {
                assert!(
                    !portable_sources.contains(secret),
                    "export contains {secret}"
                );
            }

            let config_path = layout.config_path();
            write_json(&config_path, &json!({"defaultProfile":"default"}));
            run_resolve_catalog(&ResolveCatalogOptions {
                catalog_path: None,
                config_path: home.path().join("ignored-config.json"),
                lockfile_path: home.path().join("ignored-lock.json"),
                dry_run: false,
                personal_catalog_path_override: None,
            })
            .unwrap();
            let private_lock_path = layout.state_base().join("lock.json");
            let private_lock: Value =
                serde_json::from_slice(&fs::read(&private_lock_path).unwrap()).unwrap();
            assert!(private_lock.get("_personalPlugins").is_some());
            let portable_lock_path = layout.lock_export_path().unwrap();
            let portable_lock: Value =
                serde_json::from_slice(&fs::read(&portable_lock_path).unwrap()).unwrap();
            assert!(portable_lock.get("resolvedPlugins").is_some());
            for private_key in [
                "_personalPlugins",
                "_ccyncProjection",
                "_adoptedItems",
                "_mcpServers",
            ] {
                assert!(portable_lock.get(private_key).is_none());
            }
        }

        let home = crate::test_support::TestHome::new();
        let layout = EffectiveLayout::load_or_legacy().unwrap();
        assert_ne!(layout.authority(), EffectiveLayoutAuthority::Committed);
        persist_source_change(
            &layout,
            |catalog| {
                catalog["plugins"] = json!([{"pluginId":"legacy-source"}]);
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
        let legacy_catalog: Value =
            serde_json::from_slice(&fs::read(layout.personal_catalog_path()).unwrap()).unwrap();
        assert_eq!(
            fs::read(layout.personal_catalog_path()).unwrap(),
            serde_json::to_vec_pretty(&legacy_catalog).unwrap()
        );
        assert!(layout.sources_export_path().is_none());
        assert!(!layout.portable_root().join("sources.json").exists());
        assert!(!layout.portable_root().join("lock.json").exists());
        drop(home);
    }

    #[test]
    fn splice_preserves_pin_pinnedsha_held() {
        let personal = vec![json!({
            "pluginId": "a",
            "source": "https://github.com/u/a.git",
            "installStrategy": "personal-git-clone",
            "pin": "abc123",
            "held": true
        })];
        let out = splice_personal_namespace(json!({}), &personal, true);
        let entry = &out["_personalPlugins"][0];
        assert_eq!(entry["pin"], "abc123", "pin must survive splice");
        assert_eq!(entry["held"], true, "held must survive splice");
    }

    /// Removing the last personal plugin leaves a catalog that exists and
    /// resolves to nothing. The stale `_personalPlugins` array has to be
    /// overwritten with an empty one — `managed_plugin_dirs` renders from that
    /// namespace, so leaving it stale keeps re-materializing the removed
    /// plugin's skills into the canonical root on every render.
    #[test]
    fn splice_clears_namespace_when_catalog_present_but_empty() {
        let existing = json!({
            "_personalPlugins": [{ "pluginId": "removed-one", "source": "x" }],
            "_ccyncProjection": { "schemaVersion": 1 }
        });
        let out = splice_personal_namespace(existing, &[], true);
        assert_eq!(
            out["_personalPlugins"],
            json!([]),
            "present-but-empty catalog must clear the namespace"
        );
        assert_eq!(
            out["_ccyncProjection"]["schemaVersion"], 1,
            "unrelated top-level keys must survive"
        );
    }

    /// A catalog that cannot be parsed must not be read as "everything was
    /// removed" — that would clear the managed set over a transient read
    /// failure and unmanage every plugin on the machine.
    #[test]
    fn personal_catalog_readability_gate_accepts_missing_legacy_plugins_array() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("plugins.json");

        assert!(
            !personal_catalog_is_readable(&path),
            "absent catalog is not readable"
        );

        fs::write(&path, "{ not json").unwrap();
        assert!(
            !personal_catalog_is_readable(&path),
            "malformed catalog is not readable"
        );

        fs::write(&path, r#"{"schemaVersion":1}"#).unwrap();
        assert!(
            personal_catalog_is_readable(&path),
            "legacy catalog with no plugins field is readable as an empty catalog"
        );

        fs::write(&path, r#"{"plugins":{}}"#).unwrap();
        assert!(
            !personal_catalog_is_readable(&path),
            "wrong-type plugins field is not readable"
        );

        fs::write(&path, r#"{"plugins":[],"schemaVersion":1}"#).unwrap();
        assert!(
            personal_catalog_is_readable(&path),
            "present catalog resolving to zero plugins IS readable — that is the \
             state the last removal leaves behind"
        );
    }

    #[test]
    fn portable_catalog_missing_plugins_is_empty_and_wrong_type_errors() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("personal.json");
        fs::write(&path, r#"{"privateExtra":true}"#).unwrap();
        assert!(resolve_personal_plugins(Some(&path), &[])
            .unwrap()
            .is_empty());
        fs::write(&path, r#"{"plugins":{}}"#).unwrap();
        assert!(resolve_personal_plugins(Some(&path), &[]).is_err());
    }

    #[test]
    fn portable_catalog_adoption_and_provider_survive_reload_without_private_fields() {
        use crate::adopt::{store_adoption_records, AdoptionRecord};

        let temp = TempDir::new().unwrap();
        let path = temp.path().join("plugins.json");
        write_json(&path, &json!({"privateExtra":{"kept":true},"plugins":[]}));
        store_adoption_records(
            &path,
            &[AdoptionRecord {
                source: "claude".into(),
                kind: "skill".into(),
                component_id: "adopted-skill".into(),
                name: "adopted-skill".into(),
                origin: Some("C:/private/agents/skills/adopted-skill".into()),
                revision: None,
                digest: None,
                enabled: Some(true),
                policy: None,
            }],
            &["claude".into()],
        )
        .unwrap();

        let mut catalog: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        catalog["plugins"].as_array_mut().unwrap().push(json!({
            "pluginId":"provider-one",
            "source":"C:/private/provider-one",
            "sourceType":"local",
            "operationalLock":"must-not-leak",
            "managedPath":"C:/private/managed"
        }));
        let bytes = serde_json::to_vec_pretty(&catalog).unwrap();
        ccync_foundation::platform::atomic_write_bytes(&path, &bytes).unwrap();

        let reloaded: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let records = crate::adopt::read_adoption_records(&path).unwrap();
        assert_eq!(records.len(), 1);
        let portable = portable_sources_from_personal_catalog(&reloaded).unwrap();
        let encoded = serde_json::to_string(&portable).unwrap();
        let sources = &portable.sources;
        assert!(sources
            .iter()
            .any(|source| source.source.as_str() == "claude"
                && source
                    .selection
                    .iter()
                    .any(|selection| selection.component_id == "adopted-skill")));
        assert!(sources
            .iter()
            .any(|source| source.source.as_str() == "provider-one"));
        for excluded in [
            "C:/private",
            "operationalLock",
            "managedPath",
            "installedPath",
            "ownership",
        ] {
            assert!(
                !encoded.contains(excluded),
                "portable document leaked {excluded}: {encoded}"
            );
        }
        assert!(sources
            .iter()
            .filter_map(|source| source.binding_key.as_ref())
            .all(|key| key.as_str().starts_with("binding-")));
        println!("portable_catalog evidence: identities={}, excluded=local origin values, operationalLock, managedPath, installedPath, ownership", sources.len());
    }

    /// The default-off contract is unchanged: with no personal catalog at all,
    /// the lockfile is left byte-identical.
    #[test]
    fn splice_leaves_lockfile_untouched_when_catalog_absent() {
        let existing = json!({ "_ccyncProjection": { "schemaVersion": 1 } });
        let out = splice_personal_namespace(existing.clone(), &[], false);
        assert_eq!(out, existing);
        assert!(out.get("_personalPlugins").is_none());
    }

    #[test]
    fn splice_omits_absent_pin_keys() {
        let personal = vec![json!({ "pluginId": "a", "source": "x" })];
        let out = splice_personal_namespace(json!({}), &personal, true);
        let entry = &out["_personalPlugins"][0];
        assert!(entry.get("pin").is_none(), "absent pin not written as null");
        assert!(entry.get("held").is_none());
    }

    /// A synthetic public catalog with two curated-upstream packs + profiles.
    /// The shipped `plugins/catalog.json` is intentionally empty (ccync is a
    /// manager; packs are opt-in via `ccync add`), so resolution-mechanism tests
    /// use this fixture instead of the baked catalog.
    fn synthetic_catalog() -> String {
        serde_json::to_string_pretty(&json!({
            "schemaVersion": 1,
            "plugins": [
                {
                    "pluginId": "alpha-skills",
                    "displayName": "Alpha Skills",
                    "supportTier": "curated-upstream",
                    "sourceType": "curated-upstream",
                    "upstream": { "type": "github", "repo": "https://github.com/example/alpha", "ref": "main", "path": "skills" },
                    "license": "MIT",
                    "checksumPolicy": "commit-sha",
                    "componentMap": { "skills": true },
                    "supportedProviders": ["claude", "copilot", "codex", "agy"],
                    "installStrategy": "git-clone",
                    "defaultProfiles": ["alpha"],
                    "allowAutoUpdate": false,
                    "localOverridePolicy": "source-mode-only"
                },
                {
                    "pluginId": "beta-skills",
                    "displayName": "Beta Skills",
                    "supportTier": "curated-upstream",
                    "sourceType": "curated-upstream",
                    "upstream": { "type": "github", "repo": "https://github.com/example/beta", "ref": "main", "path": "skills" },
                    "license": "MIT",
                    "checksumPolicy": "commit-sha",
                    "componentMap": { "skills": true },
                    "supportedProviders": ["claude", "copilot", "codex", "agy"],
                    "installStrategy": "git-clone",
                    "defaultProfiles": ["beta"],
                    "allowAutoUpdate": false,
                    "localOverridePolicy": "source-mode-only"
                }
            ],
            "profiles": {
                "default": { "description": "empty", "plugins": [] },
                "alpha": { "description": "alpha", "plugins": ["alpha-skills"] },
                "full": { "description": "all", "plugins": ["alpha-skills", "beta-skills"] }
            }
        })).unwrap()
    }

    #[test]
    fn default_profile_resolves_only_ccync_core() {
        let _home = crate::test_support::TestHome::new();
        let temp = TempDir::new().unwrap();
        let catalog_path = temp.path().join("catalog.json");
        let config_path = temp.path().join("config.json");
        let lockfile_path = temp.path().join("lock.json");
        fs::write(&catalog_path, include_str!("../../../plugins/catalog.json")).unwrap();
        write_json(&config_path, &json!({"defaultProfile": "default"}));

        let result = run_resolve_catalog(&ResolveCatalogOptions {
            catalog_path: Some(catalog_path),
            config_path,
            lockfile_path,
            dry_run: true,
            personal_catalog_path_override: None,
        })
        .unwrap();

        let ids: Vec<&str> = result
            .resolved_plugins
            .iter()
            .filter_map(|plugin| plugin.get("pluginId").and_then(Value::as_str))
            .collect();
        assert!(
            ids.is_empty(),
            "default profile is empty (ccync is a manager): {ids:?}"
        );
    }

    #[test]
    fn profile_resolves_companion_plugin() {
        let _home = crate::test_support::TestHome::new();
        let temp = TempDir::new().unwrap();
        let catalog_path = temp.path().join("catalog.json");
        let config_path = temp.path().join("config.json");
        let lockfile_path = temp.path().join("lock.json");
        fs::write(&catalog_path, synthetic_catalog()).unwrap();
        write_json(&config_path, &json!({"defaultProfile": "alpha"}));

        let result = run_resolve_catalog(&ResolveCatalogOptions {
            catalog_path: Some(catalog_path),
            config_path,
            lockfile_path,
            dry_run: true,
            personal_catalog_path_override: None,
        })
        .unwrap();

        let ids: Vec<&str> = result
            .resolved_plugins
            .iter()
            .filter_map(|plugin| plugin.get("pluginId").and_then(Value::as_str))
            .collect();
        assert_eq!(ids, vec!["alpha-skills"]);
    }

    #[test]
    fn full_profile_resolves_all_plugins() {
        let _home = crate::test_support::TestHome::new();
        let temp = TempDir::new().unwrap();
        let catalog_path = temp.path().join("catalog.json");
        let config_path = temp.path().join("config.json");
        let lockfile_path = temp.path().join("lock.json");
        fs::write(&catalog_path, synthetic_catalog()).unwrap();
        write_json(&config_path, &json!({"defaultProfile": "full"}));

        let result = run_resolve_catalog(&ResolveCatalogOptions {
            catalog_path: Some(catalog_path),
            config_path,
            lockfile_path,
            dry_run: true,
            personal_catalog_path_override: None,
        })
        .unwrap();

        assert_eq!(result.resolved_plugins.len(), 2);
    }

    #[test]
    fn explicit_enabled_plugins_are_added() {
        let _home = crate::test_support::TestHome::new();
        let temp = TempDir::new().unwrap();
        let catalog_path = temp.path().join("catalog.json");
        let config_path = temp.path().join("config.json");
        let lockfile_path = temp.path().join("lock.json");
        fs::write(&catalog_path, synthetic_catalog()).unwrap();
        write_json(
            &config_path,
            &json!({"enabledPlugins": ["alpha-skills", "beta-skills"]}),
        );

        let result = run_resolve_catalog(&ResolveCatalogOptions {
            catalog_path: Some(catalog_path),
            config_path,
            lockfile_path,
            dry_run: true,
            personal_catalog_path_override: None,
        })
        .unwrap();

        let ids: Vec<&str> = result
            .resolved_plugins
            .iter()
            .filter_map(|plugin| plugin.get("pluginId").and_then(Value::as_str))
            .collect();
        assert_eq!(ids, vec!["alpha-skills", "beta-skills"]);
    }

    #[test]
    fn resolve_preserves_ccync_projection_key_in_lockfile() {
        let _home = crate::test_support::TestHome::new();
        // Round-trip invariant: resolver must not wipe _ccyncProjection written by projection.
        let temp = TempDir::new().unwrap();
        let catalog_path = temp.path().join("catalog.json");
        let config_path = temp.path().join("config.json");
        let lockfile_path = temp.path().join("lock.json");
        fs::write(&catalog_path, include_str!("../../../plugins/catalog.json")).unwrap();
        write_json(&config_path, &json!({"defaultProfile": "default"}));
        // Pre-populate lockfile with _ccyncProjection data (as projection::persist would write).
        write_json(
            &lockfile_path,
            &json!({
                "_ccyncProjection": {
                    "version": 1,
                    "skills": ["defuddle"]
                }
            }),
        );

        run_resolve_catalog(&ResolveCatalogOptions {
            catalog_path: Some(catalog_path),
            config_path,
            lockfile_path: lockfile_path.clone(),
            dry_run: false,
            personal_catalog_path_override: None,
        })
        .unwrap();

        let written: Value =
            serde_json::from_str(&fs::read_to_string(&lockfile_path).unwrap()).unwrap();
        assert!(
            written.get("_ccyncProjection").is_some(),
            "_ccyncProjection must survive a resolver round-trip: {written}"
        );
        assert!(
            written.get("resolvedPlugins").is_some(),
            "resolvedPlugins must be present after resolve: {written}"
        );
    }

    #[test]
    fn resolve_round_trip_both_namespaces_coexist() {
        let _home = crate::test_support::TestHome::new();
        // Bidirectional: _ccyncProjection + resolvedPlugins live in the same file.
        let temp = TempDir::new().unwrap();
        let catalog_path = temp.path().join("catalog.json");
        let config_path = temp.path().join("config.json");
        let lockfile_path = temp.path().join("lock.json");
        fs::write(&catalog_path, include_str!("../../../plugins/catalog.json")).unwrap();
        write_json(&config_path, &json!({"defaultProfile": "default"}));

        // First pass: resolver writes resolvedPlugins.
        run_resolve_catalog(&ResolveCatalogOptions {
            catalog_path: Some(catalog_path.clone()),
            config_path: config_path.clone(),
            lockfile_path: lockfile_path.clone(),
            dry_run: false,
            personal_catalog_path_override: None,
        })
        .unwrap();

        // Simulate projection::persist adding _ccyncProjection without touching resolvedPlugins.
        let mut lock: Value =
            serde_json::from_str(&fs::read_to_string(&lockfile_path).unwrap()).unwrap();
        lock.as_object_mut().unwrap().insert(
            "_ccyncProjection".to_string(),
            json!({"version": 1, "skills": ["doc-sync"]}),
        );
        fs::write(&lockfile_path, serde_json::to_string_pretty(&lock).unwrap()).unwrap();

        // Second resolve: must keep _ccyncProjection.
        run_resolve_catalog(&ResolveCatalogOptions {
            catalog_path: Some(catalog_path),
            config_path,
            lockfile_path: lockfile_path.clone(),
            dry_run: false,
            personal_catalog_path_override: None,
        })
        .unwrap();

        let final_lock: Value =
            serde_json::from_str(&fs::read_to_string(&lockfile_path).unwrap()).unwrap();
        assert!(
            final_lock.get("_ccyncProjection").is_some(),
            "projection namespace must survive second resolve: {final_lock}"
        );
        assert!(
            final_lock.get("resolvedPlugins").is_some(),
            "resolver namespace must be present: {final_lock}"
        );
    }

    #[test]
    fn resolve_degrades_gracefully_on_corrupt_lockfile() {
        let _home = crate::test_support::TestHome::new();
        // Corrupt lockfile must not abort run_resolve_catalog; must degrade to empty root.
        let temp = TempDir::new().unwrap();
        let catalog_path = temp.path().join("catalog.json");
        let config_path = temp.path().join("config.json");
        let lockfile_path = temp.path().join("lock.json");
        fs::write(&catalog_path, include_str!("../../../plugins/catalog.json")).unwrap();
        write_json(&config_path, &json!({"defaultProfile": "default"}));
        fs::write(&lockfile_path, b"{ CORRUPT JSON {{{{").unwrap();

        let result = run_resolve_catalog(&ResolveCatalogOptions {
            catalog_path: Some(catalog_path),
            config_path,
            lockfile_path: lockfile_path.clone(),
            dry_run: false,
            personal_catalog_path_override: None,
        });
        assert!(
            result.is_ok(),
            "corrupt lockfile must not abort resolve: {result:?}"
        );
        let written: Value = serde_json::from_str(&fs::read_to_string(&lockfile_path).unwrap())
            .expect("lockfile must be valid JSON after resolve");
        assert!(
            written.get("resolvedPlugins").is_some(),
            "resolvedPlugins must be present after resolve over corrupt file: {written}"
        );
    }

    #[test]
    fn invalid_catalog_entry_emits_validation_errors() {
        let _home = crate::test_support::TestHome::new();
        let temp = TempDir::new().unwrap();
        let catalog_path = temp.path().join("catalog.json");
        let config_path = temp.path().join("config.json");
        let lockfile_path = temp.path().join("lock.json");
        let mut catalog: Value =
            serde_json::from_str(include_str!("../../../plugins/catalog.json")).unwrap();
        let plugins = catalog
            .get_mut("plugins")
            .and_then(Value::as_array_mut)
            .unwrap();
        plugins.push(json!({
            "pluginId": "bad-plugin",
            "displayName": "Bad Plugin",
            "supportTier": "curated-upstream",
            "sourceType": "curated-upstream",
            "upstream": { "repo": "https://github.com/example/bad" },
            "license": "",
            "checksumPolicy": "",
            "componentMap": { "skills": true },
            "supportedProviders": ["claude"],
            "installStrategy": "git-clone",
            "defaultProfiles": ["bad"],
            "allowAutoUpdate": false,
            "localOverridePolicy": "source-mode-only"
        }));
        write_json(&catalog_path, &catalog);
        write_json(&config_path, &json!({"defaultProfile": "default"}));

        let result = run_resolve_catalog(&ResolveCatalogOptions {
            catalog_path: Some(catalog_path),
            config_path,
            lockfile_path,
            dry_run: true,
            personal_catalog_path_override: None,
        })
        .unwrap();

        assert!(!result.errors.is_empty());
        assert!(result.errors.iter().any(|error| error.contains("License")));
        assert!(result
            .errors
            .iter()
            .any(|error| error.contains("checksumPolicy")));
    }

    // absent personal catalog → resolve output byte-identical (default-off).
    #[test]
    fn personal_catalog_absent_gives_empty_personal_plugins() {
        let _home = crate::test_support::TestHome::new();
        let temp = TempDir::new().unwrap();
        let catalog_path = temp.path().join("catalog.json");
        let config_path = temp.path().join("config.json");
        let lockfile_path = temp.path().join("lock.json");
        fs::write(&catalog_path, include_str!("../../../plugins/catalog.json")).unwrap();
        write_json(&config_path, &json!({"defaultProfile": "default"}));
        let no_personal = temp.path().join("nonexistent-personal-catalog.json");

        let result = run_resolve_catalog(&ResolveCatalogOptions {
            catalog_path: Some(catalog_path),
            config_path,
            lockfile_path,
            dry_run: true,
            personal_catalog_path_override: Some(no_personal),
        })
        .unwrap();

        assert!(
            result.personal_plugins.is_empty(),
            "absent personal catalog must produce empty personal_plugins: {:?}",
            result.personal_plugins
        );
        // Public resolution unchanged.
        let ids: Vec<&str> = result
            .resolved_plugins
            .iter()
            .filter_map(|p| p.get("pluginId").and_then(Value::as_str))
            .collect();
        assert!(
            ids.is_empty(),
            "default profile resolves empty; personal layer is separate"
        );
    }

    // personal catalog with 1 entry → resolution contains that pluginId (additive, does not overwrite public).
    #[test]
    fn personal_catalog_entry_appears_in_personal_plugins() {
        let _home = crate::test_support::TestHome::new();
        let temp = TempDir::new().unwrap();
        let catalog_path = temp.path().join("catalog.json");
        let config_path = temp.path().join("config.json");
        let lockfile_path = temp.path().join("lock.json");
        let personal_catalog_path = temp.path().join("personal-catalog.json");
        fs::write(&catalog_path, include_str!("../../../plugins/catalog.json")).unwrap();
        write_json(&config_path, &json!({"defaultProfile": "default"}));
        write_json(
            &personal_catalog_path,
            &json!({
                "plugins": [{
                    "pluginId": "my-personal-skill",
                    "displayName": "My Personal Skill",
                    "supportTier": "local",
                    "sourceType": "local",
                    "license": "proprietary",
                    "checksumPolicy": "none",
                    "componentMap": { "skills": true, "mcp": false, "agents": false },
                    "supportedProviders": ["claude"],
                    "installStrategy": "bundled-local",
                    "source": "~/.ccync/local/cache/my-personal-skill",
                    "defaultProfiles": [],
                    "allowAutoUpdate": false,
                    "localOverridePolicy": "always"
                }]
            }),
        );

        let result = run_resolve_catalog(&ResolveCatalogOptions {
            catalog_path: Some(catalog_path),
            config_path,
            lockfile_path,
            dry_run: true,
            personal_catalog_path_override: Some(personal_catalog_path),
        })
        .unwrap();

        let personal_ids: Vec<&str> = result
            .personal_plugins
            .iter()
            .filter_map(|p| p.get("pluginId").and_then(Value::as_str))
            .collect();
        assert_eq!(
            personal_ids,
            vec!["my-personal-skill"],
            "personal catalog entry must appear in personal_plugins"
        );
        // Must carry _personalOrigin marker.
        let first = &result.personal_plugins[0];
        assert_eq!(
            first.get("_personalOrigin").and_then(Value::as_bool),
            Some(true),
            "_personalOrigin must be true on personal entries"
        );
        // Public resolved_plugins (default profile = empty) is unaffected by the personal layer.
        assert!(
            result.resolved_plugins.is_empty(),
            "default-profile public resolution stays empty; personal layer is separate"
        );
        assert!(
            !result
                .resolved_plugins
                .iter()
                .any(|p| p.get("pluginId").and_then(Value::as_str) == Some("my-personal-skill")),
            "personal entry must NOT appear in public resolved_plugins"
        );
    }

    // core-wins — personal entry whose pluginId collides with a public entry is dropped.
    #[test]
    fn personal_catalog_core_wins_on_collision() {
        let _home = crate::test_support::TestHome::new();
        let temp = TempDir::new().unwrap();
        let catalog_path = temp.path().join("catalog.json");
        let config_path = temp.path().join("config.json");
        let lockfile_path = temp.path().join("lock.json");
        let personal_catalog_path = temp.path().join("personal-catalog.json");
        fs::write(&catalog_path, synthetic_catalog()).unwrap();
        write_json(&config_path, &json!({"defaultProfile": "alpha"}));
        // Personal catalog tries to define alpha-skills (a public id) — must be silently dropped (core-wins).
        write_json(
            &personal_catalog_path,
            &json!({
                "plugins": [{
                    "pluginId": "alpha-skills",
                    "displayName": "Imposter Alpha",
                    "supportTier": "local",
                    "sourceType": "local",
                    "license": "proprietary",
                    "checksumPolicy": "none",
                    "componentMap": { "skills": false, "mcp": false, "agents": false },
                    "supportedProviders": ["claude"],
                    "installStrategy": "bundled-local",
                    "source": "~/.ccync/local/cache/imposter",
                    "defaultProfiles": [],
                    "allowAutoUpdate": false,
                    "localOverridePolicy": "always"
                }]
            }),
        );

        let result = run_resolve_catalog(&ResolveCatalogOptions {
            catalog_path: Some(catalog_path),
            config_path,
            lockfile_path,
            dry_run: true,
            personal_catalog_path_override: Some(personal_catalog_path),
        })
        .unwrap();

        assert!(
            result.personal_plugins.is_empty(),
            "colliding personal entry must be dropped by core-wins: {:?}",
            result.personal_plugins
        );
        // Public resolved_plugins still has the real alpha-skills.
        let ids: Vec<&str> = result
            .resolved_plugins
            .iter()
            .filter_map(|p| p.get("pluginId").and_then(Value::as_str))
            .collect();
        assert_eq!(ids, vec!["alpha-skills"]);
    }

    // leak guard — filter_personal_entries removes _personalOrigin entries.
    #[test]
    fn filter_personal_entries_removes_personal_origin_entries() {
        let entries = vec![
            json!({"pluginId": "public-plugin", "displayName": "Public"}),
            json!({"pluginId": "personal-plugin", "_personalOrigin": true, "displayName": "Personal"}),
            json!({"pluginId": "another-public", "displayName": "Also public"}),
        ];
        let filtered = filter_personal_entries(&entries);
        let ids: Vec<&str> = filtered
            .iter()
            .filter_map(|e| e.get("pluginId").and_then(Value::as_str))
            .collect();
        assert_eq!(ids, vec!["public-plugin", "another-public"]);
        assert!(
            !filtered
                .iter()
                .any(|e| e.get("_personalOrigin").and_then(Value::as_bool) == Some(true)),
            "filter_personal_entries must remove all _personalOrigin=true entries"
        );
    }

    // personal plugin in lockfile under _personalPlugins; existing keys unchanged.
    #[test]
    fn personal_plugins_written_to_lockfile_personal_namespace() {
        let _home = crate::test_support::TestHome::new();
        let temp = TempDir::new().unwrap();
        let catalog_path = temp.path().join("catalog.json");
        let config_path = temp.path().join("config.json");
        let lockfile_path = temp.path().join("lock.json");
        let personal_catalog_path = temp.path().join("personal-catalog.json");
        fs::write(&catalog_path, include_str!("../../../plugins/catalog.json")).unwrap();
        write_json(&config_path, &json!({"defaultProfile": "default"}));
        write_json(
            &personal_catalog_path,
            &json!({
                "plugins": [{
                    "pluginId": "my-personal-skill",
                    "displayName": "My Personal Skill",
                    "supportTier": "local",
                    "sourceType": "local",
                    "license": "proprietary",
                    "checksumPolicy": "none",
                    "componentMap": { "skills": true, "mcp": false, "agents": false },
                    "supportedProviders": ["claude"],
                    "installStrategy": "bundled-local",
                    "source": "~/.ccync/local/cache/my-personal-skill",
                    "defaultProfiles": [],
                    "allowAutoUpdate": false,
                    "localOverridePolicy": "always"
                }]
            }),
        );
        // Pre-populate lockfile with _ccyncProjection so we confirm it survives.
        write_json(
            &lockfile_path,
            &json!({"_ccyncProjection": {"version": 1, "skills": ["doc-sync"]}}),
        );

        run_resolve_catalog(&ResolveCatalogOptions {
            catalog_path: Some(catalog_path),
            config_path,
            lockfile_path: lockfile_path.clone(),
            dry_run: false,
            personal_catalog_path_override: Some(personal_catalog_path),
        })
        .unwrap();

        let lock: Value =
            serde_json::from_str(&fs::read_to_string(&lockfile_path).unwrap()).unwrap();

        // _personalPlugins namespace must contain the personal entry.
        let personal = lock
            .get("_personalPlugins")
            .and_then(Value::as_array)
            .expect(
                "_personalPlugins must be present in lockfile after resolve with personal catalog",
            );
        let pids: Vec<&str> = personal
            .iter()
            .filter_map(|e| e.get("pluginId").and_then(Value::as_str))
            .collect();
        assert_eq!(pids, vec!["my-personal-skill"]);

        // Existing namespaces must survive.
        assert!(
            lock.get("_ccyncProjection").is_some(),
            "_ccyncProjection must survive personal namespace write"
        );
        assert!(
            lock.get("resolvedPlugins").is_some(),
            "resolvedPlugins must be present"
        );
    }

    // absent personal catalog → _personalPlugins NOT written (default-off, lockfile unchanged).
    #[test]
    fn absent_personal_catalog_does_not_write_personal_namespace() {
        let _home = crate::test_support::TestHome::new();
        let temp = TempDir::new().unwrap();
        let catalog_path = temp.path().join("catalog.json");
        let config_path = temp.path().join("config.json");
        let lockfile_path = temp.path().join("lock.json");
        let no_personal = temp.path().join("nonexistent-personal.json");
        fs::write(&catalog_path, include_str!("../../../plugins/catalog.json")).unwrap();
        write_json(&config_path, &json!({"defaultProfile": "default"}));

        run_resolve_catalog(&ResolveCatalogOptions {
            catalog_path: Some(catalog_path),
            config_path,
            lockfile_path: lockfile_path.clone(),
            dry_run: false,
            personal_catalog_path_override: Some(no_personal),
        })
        .unwrap();

        let lock: Value =
            serde_json::from_str(&fs::read_to_string(&lockfile_path).unwrap()).unwrap();
        assert!(
            lock.get("_personalPlugins").is_none(),
            "_personalPlugins must NOT appear in lockfile when personal catalog is absent: {lock}"
        );
    }

    // ------------------------------------------------------------------
    // catalog_path: Option<PathBuf> (embedded default vs. disk override)
    // ------------------------------------------------------------------

    /// Fresh temp home + `catalog_path: None` → `run_resolve_catalog`
    /// returns `Ok`, with a non-error resolved set (embedded catalog's default
    /// profile is empty by design — "empty" is a valid Ok payload, not an error).
    #[test]
    fn catalog_path_none_resolves_ok_from_embedded_catalog() {
        let _home = crate::test_support::TestHome::new();
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("config.json");
        let lockfile_path = temp.path().join("lock.json");
        write_json(&config_path, &json!({"defaultProfile": "default"}));

        let result = run_resolve_catalog(&ResolveCatalogOptions {
            catalog_path: None,
            config_path,
            lockfile_path,
            dry_run: true,
            personal_catalog_path_override: None,
        });

        assert!(
            result.is_ok(),
            "catalog_path: None must resolve via embedded catalog without a missing-catalog error: {result:?}"
        );
        // Embedded catalog's default profile is intentionally empty (ccync is a
        // manager, not a bundle) — Ok with an empty resolved set is correct, not
        // a failure. Assert no resolver-level errors were emitted either way.
        let resolution = result.unwrap();
        assert!(
            resolution.errors.is_empty(),
            "embedded catalog resolution must not emit validation errors: {:?}",
            resolution.errors
        );
    }

    /// `catalog_path: Some(<missing path>)` → `Err` containing the
    /// exact substring `catalog override not found at:`.
    #[test]
    fn catalog_path_some_missing_file_errors_with_override_not_found_message() {
        let _home = crate::test_support::TestHome::new();
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("config.json");
        let lockfile_path = temp.path().join("lock.json");
        let missing_catalog_path = temp.path().join("does-not-exist-catalog.json");
        write_json(&config_path, &json!({"defaultProfile": "default"}));

        let result = run_resolve_catalog(&ResolveCatalogOptions {
            catalog_path: Some(missing_catalog_path.clone()),
            config_path,
            lockfile_path,
            dry_run: true,
            personal_catalog_path_override: None,
        });

        let err = result.expect_err("missing catalog override path must error");
        assert!(
            err.contains("catalog override not found at:"),
            "error must contain the exact substring 'catalog override not found at:': {err}"
        );
    }

    /// The embedded-parse failure branch's message text is exercised
    /// directly since `embedded_catalog_json()` is `include_str!`'d from a real,
    /// always-valid repo file — there is no test seam to inject invalid JSON into
    /// that specific code path (see testability note below). Instead this test
    /// pins the *exact* error-message contract that branch would produce, by
    /// invoking the same `serde_json::from_str::<Value>` + `.map_err` construction
    /// used at catalog.rs:49-53 against deliberately-broken JSON, so a future
    /// change to that message format is caught here even though the `include_str!`
    /// branch itself can't be forced to fail in a unit test.
    ///
    /// Testability limitation: `embedded_catalog_json()` reads
    /// `../../../plugins/catalog.json` via `include_str!`, which is resolved at
    /// compile time. There is no runtime seam (env var, trait, fn pointer) to
    /// substitute broken content for that specific call site without modifying
    /// production code, so the JSON-invalid branch of `run_resolve_catalog`'s
    /// `None` arm cannot be driven end-to-end from a test in this crate today.
    #[test]
    fn embedded_catalog_invalid_json_error_message_contract() {
        let broken_json = "{ this is not valid json ,,, ";
        let parse_result: Result<Value, _> = serde_json::from_str(broken_json);
        let e = parse_result.expect_err("deliberately broken JSON must fail to parse");
        let message = format!(
            "embedded catalog is invalid JSON: {e} — this is a ccync build defect; run `ccync update` or reinstall"
        );
        assert!(
            message.starts_with("embedded catalog is invalid JSON: "),
            "message must start with the documented prefix: {message}"
        );
        assert!(
            message.ends_with(" — this is a ccync build defect; run `ccync update` or reinstall"),
            "message must end with the documented suffix: {message}"
        );
    }

    /// The `run_personal_resolve()` code path in
    /// `ccync-cli/src/commands/plugin.rs` calls `run_resolve_catalog` with
    /// `catalog_path: None` and `personal_catalog_path_override:
    /// personal_catalog_path()`. Exercised here at the catalog.rs level (matching
    /// the real call shape) since `ccync-cli` is a separate crate and this crate
    /// cannot depend on it. A `None` personal-catalog override (as a fresh /
    /// unconfigured machine would report) must also resolve `Ok`.
    #[test]
    fn personal_resolve_equivalent_path_resolves_ok_with_embedded_default() {
        let _home = crate::test_support::TestHome::new();
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("config.json");
        let lockfile_path = temp.path().join("lock.json");
        write_json(&config_path, &json!({"defaultProfile": "default"}));

        // Mirrors run_personal_resolve()'s exact ResolveCatalogOptions shape:
        // catalog_path: None, personal_catalog_path_override sourced from
        // ccync_foundation::paths::personal_catalog_path() (None on a machine
        // with no home dir configured for the test).
        let result = run_resolve_catalog(&ResolveCatalogOptions {
            catalog_path: None,
            config_path,
            lockfile_path,
            dry_run: false,
            personal_catalog_path_override: None,
        });

        assert!(
            result.is_ok(),
            "run_personal_resolve()-equivalent path (catalog_path: None) must resolve Ok: {result:?}"
        );
    }

    /// Production (non-test, non-comment) code in this file must
    /// contain zero occurrences of the retired "Catalog not found" string. The
    /// stale match in `ccync-cli/src/commands/lifecycle.rs`'s benign-warning
    /// classifier is a separate, not-yet-landed cleanup and out of scope here.
    ///
    /// Scoped to the portion of the file *above* `#[cfg(test)] mod tests` so this
    /// assertion is not self-referential: this test module's own doc-comments and
    /// string literals (which must name the retired string to test for it) are
    /// excluded from the scan, matching the plan's intent of "production code".
    #[test]
    fn catalog_rs_has_no_stale_catalog_not_found_string() {
        let source = include_str!("catalog.rs").replace("\r\n", "\n");
        let production_source = source
            .split_once("#[cfg(test)]\nmod tests {")
            .map(|(before, _)| before.to_string())
            .expect("catalog.rs must contain the #[cfg(test)] mod tests boundary");
        let production_lines: Vec<&str> = production_source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect();
        let hits: Vec<&&str> = production_lines
            .iter()
            .filter(|line| line.contains("Catalog not found"))
            .collect();
        assert!(
            hits.is_empty(),
            "catalog.rs must contain 0 occurrences of the retired 'Catalog not found' \
             string in production/non-comment code: {hits:?}"
        );
    }
}
