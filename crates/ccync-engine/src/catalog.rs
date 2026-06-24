//! `ccync resolve-catalog` — deterministic plugin resolution and lockfile output.

use ccync_foundation::paths::local_catalog_path;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogResolution {
    pub profile_name: String,
    pub resolved_plugins: Vec<Value>,
    /// Personal plugins from `~/.ccync/local/catalog.json` (or the injected override).
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
    pub catalog_path: PathBuf,
    pub config_path: PathBuf,
    pub lockfile_path: PathBuf,
    pub dry_run: bool,
    /// Inject a test-specific personal catalog path instead of `local_catalog_path()`.
    /// `None` (production default) → reads `~/.ccync/local/catalog.json`.
    pub personal_catalog_path_override: Option<PathBuf>,
}

/// The public plugin catalog, baked into the binary at compile time.
///
/// `ccync install` deploys this to `~/.ccync/plugins/ccync/catalog.json` so the
/// runtime resolve path (incl. `ccync plugin add`) can find it without the source repo.
pub fn embedded_catalog_json() -> &'static str {
    include_str!("../../../plugins/catalog.json")
}

pub fn run_resolve_catalog(opts: &ResolveCatalogOptions) -> Result<CatalogResolution, String> {
    let catalog = read_json_file(&opts.catalog_path)?
        .ok_or_else(|| format!("Catalog not found at: {}", opts.catalog_path.display()))?;
    let config = read_json_file(&opts.config_path)?.unwrap_or_else(|| json!({}));

    let mut resolution = resolve_plugin_set(&catalog, &config, &opts.lockfile_path)?;

    // Additive merge of personal catalog (default-off — absent = byte-identical).
    let personal_catalog_path = opts
        .personal_catalog_path_override
        .clone()
        .or_else(local_catalog_path);
    resolution.personal_plugins = resolve_personal_plugins(
        personal_catalog_path.as_deref(),
        &resolution.resolved_plugins,
    );

    if !opts.dry_run {
        // Round-trip: read existing lockfile root (fail-safe → empty object), splice
        // resolver keys in, write back.  Preserves `_ccyncProjection` and any other
        // non-resolver top-level keys written by `projection::persist`.
        let existing = read_json_file(&opts.lockfile_path)
            .unwrap_or(None)
            .unwrap_or_else(|| json!({}));
        let merged = splice_resolver_keys(existing, &resolution.lockfile);
        // Write personal plugins into their own namespace (only when non-empty
        // so absent personal catalog leaves the lockfile byte-identical — default-off).
        let merged = splice_personal_namespace(merged, &resolution.personal_plugins);
        write_json_file(&opts.lockfile_path, &merged)?;
    }
    Ok(resolution)
}

/// Splice personal plugin entries into `_personalPlugins` in the lockfile root.
/// When `personal_plugins` is empty, returns `existing` unchanged (default-off).
/// Preserves all other top-level keys (resolver + projection namespaces).
fn splice_personal_namespace(existing: Value, personal_plugins: &[Value]) -> Value {
    if personal_plugins.is_empty() {
        return existing;
    }
    let lockfile_entries: Vec<Value> = personal_plugins
        .iter()
        .map(|p| {
            json!({
                "pluginId":       p.get("pluginId").cloned().unwrap_or(Value::Null),
                "displayName":    p.get("displayName").cloned().unwrap_or(Value::Null),
                "sourceType":     p.get("sourceType").cloned().unwrap_or(Value::Null),
                "installStrategy":p.get("installStrategy").cloned().unwrap_or(Value::Null),
                "source":         p.get("source").cloned().unwrap_or(Value::Null),
            })
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

/// Read personal catalog and return entries not already present in public resolved set.
/// Each returned entry carries `"_personalOrigin": true` for the leak guard.
/// Absent or unreadable personal catalog → empty vec (default-off; does not error).
fn resolve_personal_plugins(catalog_path: Option<&Path>, public_resolved: &[Value]) -> Vec<Value> {
    let Some(path) = catalog_path else {
        return vec![];
    };
    let catalog = match read_json_file(path) {
        Ok(Some(v)) => v,
        _ => return vec![],
    };
    let plugins = match catalog.get("plugins").and_then(Value::as_array) {
        Some(arr) => arr.clone(),
        None => return vec![],
    };
    let public_ids: std::collections::HashSet<&str> = public_resolved
        .iter()
        .filter_map(|p| p.get("pluginId").and_then(Value::as_str))
        .collect();

    plugins
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
        .collect()
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
    }
    fs::write(
        path,
        serde_json::to_string_pretty(value).map_err(|e| e.to_string())?,
    )
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
// Profile install summary (for `ccync install --profile` UX)
// ---------------------------------------------------------------------------

/// What a single plugin brings to an install (for post-install summary output).
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
        entry.get("source").and_then(Value::as_str).map(str::to_string)
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

    fn write_json(path: &Path, value: &Value) {
        fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
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
        let temp = TempDir::new().unwrap();
        let catalog_path = temp.path().join("catalog.json");
        let config_path = temp.path().join("config.json");
        let lockfile_path = temp.path().join("lock.json");
        fs::write(&catalog_path, include_str!("../../../plugins/catalog.json")).unwrap();
        write_json(&config_path, &json!({"defaultProfile": "default"}));

        let result = run_resolve_catalog(&ResolveCatalogOptions {
            catalog_path,
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
        let temp = TempDir::new().unwrap();
        let catalog_path = temp.path().join("catalog.json");
        let config_path = temp.path().join("config.json");
        let lockfile_path = temp.path().join("lock.json");
        fs::write(&catalog_path, synthetic_catalog()).unwrap();
        write_json(&config_path, &json!({"defaultProfile": "alpha"}));

        let result = run_resolve_catalog(&ResolveCatalogOptions {
            catalog_path,
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
        let temp = TempDir::new().unwrap();
        let catalog_path = temp.path().join("catalog.json");
        let config_path = temp.path().join("config.json");
        let lockfile_path = temp.path().join("lock.json");
        fs::write(&catalog_path, synthetic_catalog()).unwrap();
        write_json(&config_path, &json!({"defaultProfile": "full"}));

        let result = run_resolve_catalog(&ResolveCatalogOptions {
            catalog_path,
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
            catalog_path,
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
            catalog_path,
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
        // Bidirectional: _ccyncProjection + resolvedPlugins live in the same file.
        let temp = TempDir::new().unwrap();
        let catalog_path = temp.path().join("catalog.json");
        let config_path = temp.path().join("config.json");
        let lockfile_path = temp.path().join("lock.json");
        fs::write(&catalog_path, include_str!("../../../plugins/catalog.json")).unwrap();
        write_json(&config_path, &json!({"defaultProfile": "default"}));

        // First pass: resolver writes resolvedPlugins.
        run_resolve_catalog(&ResolveCatalogOptions {
            catalog_path: catalog_path.clone(),
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
            catalog_path,
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
        // Corrupt lockfile must not abort run_resolve_catalog; must degrade to empty root.
        let temp = TempDir::new().unwrap();
        let catalog_path = temp.path().join("catalog.json");
        let config_path = temp.path().join("config.json");
        let lockfile_path = temp.path().join("lock.json");
        fs::write(&catalog_path, include_str!("../../../plugins/catalog.json")).unwrap();
        write_json(&config_path, &json!({"defaultProfile": "default"}));
        fs::write(&lockfile_path, b"{ CORRUPT JSON {{{{").unwrap();

        let result = run_resolve_catalog(&ResolveCatalogOptions {
            catalog_path,
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
            catalog_path,
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
        let temp = TempDir::new().unwrap();
        let catalog_path = temp.path().join("catalog.json");
        let config_path = temp.path().join("config.json");
        let lockfile_path = temp.path().join("lock.json");
        fs::write(&catalog_path, include_str!("../../../plugins/catalog.json")).unwrap();
        write_json(&config_path, &json!({"defaultProfile": "default"}));
        let no_personal = temp.path().join("nonexistent-personal-catalog.json");

        let result = run_resolve_catalog(&ResolveCatalogOptions {
            catalog_path,
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
            catalog_path,
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
            catalog_path,
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
            catalog_path,
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
        let temp = TempDir::new().unwrap();
        let catalog_path = temp.path().join("catalog.json");
        let config_path = temp.path().join("config.json");
        let lockfile_path = temp.path().join("lock.json");
        let no_personal = temp.path().join("nonexistent-personal.json");
        fs::write(&catalog_path, include_str!("../../../plugins/catalog.json")).unwrap();
        write_json(&config_path, &json!({"defaultProfile": "default"}));

        run_resolve_catalog(&ResolveCatalogOptions {
            catalog_path,
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
}
