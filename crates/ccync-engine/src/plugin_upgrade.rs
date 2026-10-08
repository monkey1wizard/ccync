//! Plugin upgrade support for `ccync upgrade`.
//!
//! Layers: `resolve_upgrades` classifies the personal-plugin set and compares
//! each git plugin's pinned sha against its remote HEAD via an injected
//! `ls_remote` seam; `apply_upgrade` re-clones a changed plugin and re-pins it
//! (pin-after-cache); `run_upgrade` orchestrates filter / dry-run / projection.

use serde_json::Value;

/// Per-plugin classification produced by [`resolve_upgrades`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeOutcome {
    /// A git plugin whose remote HEAD differs from its pinned sha.
    Changed {
        plugin_id: String,
        source: String,
        old_sha: Option<String>,
        new_sha: String,
    },
    /// A git plugin already at the latest remote HEAD.
    Unchanged { plugin_id: String },
    /// A non-git plugin (archive/local): no upstream "latest" concept.
    Skipped { plugin_id: String, reason: String },
    /// Resolving this plugin failed (e.g. `git ls-remote` errored).
    Failed { plugin_id: String, reason: String },
    /// The user pinned (`ccync pin`) this plugin: hold it at its current sha,
    /// never querying the remote or re-cloning.
    Held { plugin_id: String },
}

impl UpgradeOutcome {
    /// The plugin id this outcome refers to.
    pub fn plugin_id(&self) -> &str {
        match self {
            UpgradeOutcome::Changed { plugin_id, .. }
            | UpgradeOutcome::Unchanged { plugin_id }
            | UpgradeOutcome::Skipped { plugin_id, .. }
            | UpgradeOutcome::Failed { plugin_id, .. }
            | UpgradeOutcome::Held { plugin_id } => plugin_id,
        }
    }
}

/// Extract the personal-plugin entry array from a lockfile (`_personalPlugins`)
/// or personal catalog (`plugins`) value, or treat `plugins` as an array itself.
fn personal_entries(plugins: &Value) -> Vec<&Value> {
    if let Some(arr) = plugins.as_array() {
        return arr.iter().collect();
    }
    for key in ["_personalPlugins", "plugins"] {
        if let Some(arr) = plugins.get(key).and_then(Value::as_array) {
            return arr.iter().collect();
        }
    }
    Vec::new()
}

/// True when an entry is a git-cloned plugin (re-resolvable against a remote).
/// Prefers the explicit `installStrategy`, falling back to the source URL shape.
fn is_git_entry(entry: &Value) -> bool {
    if let Some(strategy) = entry.get("installStrategy").and_then(Value::as_str) {
        return strategy == "personal-git-clone";
    }
    match entry.get("source").and_then(Value::as_str) {
        Some(s) => s.ends_with(".git") || s.starts_with("git@") || s.contains("github.com"),
        None => false,
    }
}

/// The currently pinned commit sha for an entry, if recorded.
fn current_pin(entry: &Value) -> Option<String> {
    ccync_foundation::pins::entry_pin(entry)
}

/// Classify every personal plugin for upgrade. Git plugins are compared against
/// their remote HEAD via `ls_remote(source, ref)`; non-git plugins are
/// [`UpgradeOutcome::Skipped`]. Pure aside from the injected `ls_remote` seam.
pub fn resolve_upgrades(
    plugins: &Value,
    ls_remote: impl Fn(&str, &str) -> Result<String, String>,
) -> Vec<UpgradeOutcome> {
    let mut out = Vec::new();
    for entry in personal_entries(plugins) {
        let plugin_id = entry
            .get("pluginId")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>")
            .to_string();

        // Held plugins are frozen by the user: never query the remote.
        if entry.get("held").and_then(Value::as_bool).unwrap_or(false) {
            out.push(UpgradeOutcome::Held { plugin_id });
            continue;
        }

        if !is_git_entry(entry) {
            let reason = entry
                .get("installStrategy")
                .and_then(Value::as_str)
                .map(|s| format!("non-git source ({s})"))
                .unwrap_or_else(|| "non-git source".to_string());
            out.push(UpgradeOutcome::Skipped { plugin_id, reason });
            continue;
        }

        let source = match entry.get("source").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => {
                out.push(UpgradeOutcome::Failed {
                    plugin_id,
                    reason: "git entry has no source url".to_string(),
                });
                continue;
            }
        };

        let remote = match ls_remote(&source, "HEAD") {
            Ok(sha) => sha,
            Err(e) => {
                out.push(UpgradeOutcome::Failed {
                    plugin_id,
                    reason: format!("ls-remote failed: {e}"),
                });
                continue;
            }
        };

        let pin = current_pin(entry);
        if pin.as_deref() == Some(remote.as_str()) {
            out.push(UpgradeOutcome::Unchanged { plugin_id });
        } else {
            out.push(UpgradeOutcome::Changed {
                plugin_id,
                source,
                old_sha: pin,
                new_sha: remote,
            });
        }
    }
    out
}

// ── apply upgrade (re-clone + pin-after-cache) ────────────────────────────────

use std::path::Path;

/// A confirmed-changed git plugin to re-clone and re-pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeChange {
    pub plugin_id: String,
    pub source: String,
    /// The currently pinned sha, if recorded (carried through from
    /// [`UpgradeOutcome::Changed`] so `upgrade --dry-run` /
    /// `list --upgrade-available` can render `current → latest`).
    pub old_sha: Option<String>,
    pub new_sha: String,
}

/// Failure surface for [`apply_upgrade`].
#[derive(Debug)]
pub enum UpgradeError {
    /// The re-clone seam failed.
    Clone(String),
    /// The clone reported success but the cache dir did not materialize.
    CacheVerify(String),
    /// Writing the new pin into a pin file failed.
    PinWrite(String),
    /// No pin file carried an entry for this plugin.
    PinMissing(String),
}

impl std::fmt::Display for UpgradeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpgradeError::Clone(m) => write!(f, "clone failed: {m}"),
            UpgradeError::CacheVerify(m) => write!(f, "cache verify failed: {m}"),
            UpgradeError::PinWrite(m) => write!(f, "pin write failed: {m}"),
            UpgradeError::PinMissing(id) => write!(f, "no pin entry for plugin: {id}"),
        }
    }
}

impl std::error::Error for UpgradeError {}

/// Re-clone a changed plugin into `cache_root/<id>@<new_sha>` and, **only after**
/// the cache is verified present, atomically re-pin it across `pin_files`
/// (`plugins.json` and/or `lock.json`). The clone is injected so the function is
/// unit-testable without the network. Pin-after-cache order is mandatory: a pin
/// is never written before its cache dir exists.
pub fn apply_upgrade(
    change: &UpgradeChange,
    cache_root: &Path,
    pin_files: &[&Path],
    clone: impl Fn(&str, &Path) -> Result<(), String>,
) -> Result<(), UpgradeError> {
    apply_upgrade_inner(change, false, cache_root, pin_files, clone)
}

/// Variant of [`apply_upgrade`] that rewrites both `pin` and `source` in one write.
pub fn apply_upgrade_rewriting_source(
    change: &UpgradeChange,
    cache_root: &Path,
    pin_files: &[&Path],
    clone: impl Fn(&str, &Path) -> Result<(), String>,
) -> Result<(), UpgradeError> {
    apply_upgrade_inner(change, true, cache_root, pin_files, clone)
}

fn apply_upgrade_inner(
    change: &UpgradeChange,
    rewrite_source: bool,
    cache_root: &Path,
    pin_files: &[&Path],
    clone: impl Fn(&str, &Path) -> Result<(), String>,
) -> Result<(), UpgradeError> {
    use std::fs;

    // 1. Re-clone into the new pinned cache dir.
    let dest = cache_root.join(format!("{}@{}", change.plugin_id, change.new_sha));
    clone(&change.source, &dest).map_err(UpgradeError::Clone)?;

    // 2. Verify the cache actually landed before touching any pin.
    let landed = dest.is_dir()
        && fs::read_dir(&dest)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
    if !landed {
        return Err(UpgradeError::CacheVerify(format!(
            "cache dir absent or empty: {}",
            dest.display()
        )));
    }

    // 3. Pin-after-cache: update each pin file that carries this plugin.
    let mut any = false;
    let new_source = rewrite_source.then_some(change.source.as_str());
    for file in pin_files {
        match update_pin_in_file(file, &change.plugin_id, &change.new_sha, new_source) {
            Ok(true) => any = true,
            Ok(false) => {}
            Err(e) => return Err(UpgradeError::PinWrite(e)),
        }
    }
    if !any {
        return Err(UpgradeError::PinMissing(change.plugin_id.clone()));
    }
    Ok(())
}

/// Update the pin of `plugin_id` in a JSON file's personal-plugin array, writing
/// atomically (tmp + rename). Returns `Ok(true)` when an entry was updated,
/// `Ok(false)` when the file has no such entry (not an error — the entry may
/// live only in the other pin file). A missing file is treated as "no entry".
fn update_pin_in_file(
    path: &Path,
    plugin_id: &str,
    new_sha: &str,
    new_source: Option<&str>,
) -> Result<bool, String> {
    use std::fs;

    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Ok(false),
    };
    let mut v: Value = serde_json::from_str(&text).map_err(|e| format!("parse {path:?}: {e}"))?;

    let mut updated = false;
    for key in ["_personalPlugins", "plugins"] {
        if let Some(arr) = v.get_mut(key).and_then(Value::as_array_mut) {
            for entry in arr.iter_mut() {
                if entry.get("pluginId").and_then(Value::as_str) == Some(plugin_id) {
                    // Prefer the key already present; default to `pin`.
                    if entry.get("pinnedSha").is_some() {
                        entry["pinnedSha"] = Value::String(new_sha.to_string());
                    } else {
                        entry["pin"] = Value::String(new_sha.to_string());
                    }
                    if let Some(url) = new_source {
                        entry["source"] = Value::String(url.to_string());
                    }
                    updated = true;
                }
            }
        }
    }

    if !updated {
        return Ok(false);
    }

    let pretty =
        serde_json::to_string_pretty(&v).map_err(|e| format!("serialize {path:?}: {e}"))?;
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    fs::write(&tmp, pretty).map_err(|e| format!("write tmp {tmp:?}: {e}"))?;
    fs::rename(&tmp, path).map_err(|e| format!("rename {tmp:?}->{path:?}: {e}"))?;
    Ok(true)
}

// ── adopted-item upgrade classification (detect + report only) ───────────────
//
// Adopted items (`_adoptedItems`) are read-only by design: ccync never
// auto-promotes them. This classification is pure reporting — its output type
// has no fields that could be threaded into `UpgradeChange` / `apply_upgrade`,
// no pin file, no clone seam, no projection call. Structurally, an adopted
// item cannot reach the personal-plugin apply path through this function.

const ADOPTED_KEY: &str = "_adoptedItems";

/// Per-adopted-item classification produced by [`resolve_adopted_upgrades`].
/// Deliberately a separate type from [`UpgradeOutcome`]: it carries no sha/pin
/// fields and is never converted into an [`UpgradeChange`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdoptedUpgradeOutcome {
    /// The marketplace catalog reports a version newer than the recorded
    /// baseline. Report-only: the caller should print `current -> latest`
    /// plus Claude's own `/plugin` update path, and — only when
    /// `takeover_supported` — `ccync add <source>` to take over management.
    /// Never apply directly.
    Changed {
        name: String,
        source: String,
        current_version: Option<String>,
        latest_version: String,
        /// Whether `source` is something `ccync add` can actually fetch.
        ///
        /// `false` for a source that cannot be fetched by `ccync add`, such as
        /// an embedded path from a local marketplace repository. Remote
        /// embedded marketplace plugins use the selector form
        /// `<repo-url>//<subpath>`, which `cmd_add` can fetch, so they are
        /// directly clonable even though their catalog source is embedded.
        ///
        /// `true` when the source is fetchable, including direct sources and
        /// remote embedded selector sources.
        takeover_supported: bool,
    },
    /// The marketplace catalog version matches the recorded baseline.
    Unchanged { name: String },
    /// The plugin's source IS known (from catalog resolution or the entry's
    /// own recorded fallback), but version evidence is incomplete — the
    /// catalog entry carries no version, the adopted entry carries no
    /// `baselineVersion`, or the catalog/marketplace itself could not be
    /// re-resolved. Distinct from [`Self::Skipped`]'s source-unresolved
    /// reasons: a source-bearing item must never be mislabeled
    /// source-unresolved just because upgrade-comparison evidence is
    /// missing.
    VersionUnresolved { name: String, source: String },
    /// No upstream "latest" concept applies (adopted MCP/skill), or the
    /// plugin's source itself could not be resolved. `reason` is a fixed,
    /// human-readable explanation.
    Skipped { name: String, reason: String },
}

impl AdoptedUpgradeOutcome {
    /// The adopted item id (`_adoptedItems[].name`) this outcome refers to.
    pub fn name(&self) -> &str {
        match self {
            AdoptedUpgradeOutcome::Changed { name, .. }
            | AdoptedUpgradeOutcome::Unchanged { name }
            | AdoptedUpgradeOutcome::VersionUnresolved { name, .. }
            | AdoptedUpgradeOutcome::Skipped { name, .. } => name,
        }
    }
}

/// Marker carried inside the `Skipped` reason that
/// [`resolve_adopted_upgrades`] emits for an adopted *plugin* with no
/// resolvable source at all (catalog miss AND no recorded fallback). A
/// plugin with a known source but incomplete version evidence is
/// [`AdoptedUpgradeOutcome::VersionUnresolved`] instead — this marker is
/// reserved for the genuinely sourceless case, where source metadata and
/// version evidence are tracked as two separate signals. Repair-
/// eligibility is not derived from this marker string; it comes from the
/// lock-derived `ccync_engine::adopt::adopted_items_without_source` seam
/// instead (covers plugin/MCP/skill uniformly, not just plugin skips).
const SOURCE_UNRESOLVED_MARKER: &str = "source-unresolved";

/// Fixed skip reason for adopted MCP servers: config entries have no
/// versioned upstream to compare against.
const MCP_SKIP_REASON: &str = "adopted MCP server — config entry, not versioned";

/// Fixed skip reason for adopted skills: local content has no upstream.
const SKILL_SKIP_REASON: &str = "adopted skill — local content, no upstream";

/// Classify an adopted entry's kind from its `sourceId` tag, mirroring
/// `managed_view.rs`'s `ItemKind::from_source_id` convention (kept local here
/// since that helper is private to its module).
fn adopted_kind_from_source_id(source_id: &str) -> &'static str {
    if source_id.contains("mcp") {
        "mcp"
    } else if source_id.contains("skill") {
        "skill"
    } else {
        "plugin"
    }
}

/// Split an adopted plugin's `name` (`"<plugin>@<marketplace>"`) into its
/// plugin/marketplace parts, mirroring `adopt.rs::parse_installed_plugins`.
/// Returns `None` when there is no `@` (no marketplace to re-resolve against).
fn split_plugin_marketplace(name: &str) -> Option<(&str, &str)> {
    name.split_once('@')
}

/// Classify every adopted item for upgrade reporting: detect + report only,
/// never auto-apply — this function's output is read-only and must never
/// mutate installed state or trigger an upgrade itself.
///
/// - MCP / skill entries are always [`AdoptedUpgradeOutcome::Skipped`] with a
///   fixed reason (no upstream "latest" concept for either).
/// - Plugin entries are re-resolved against the marketplace's *local clone*
///   catalog (`read_known_marketplaces` + `read_marketplace_plugin_source`,
///   never a ccync-side `ls-remote`/fetch), falling back to the entry's own
///   recorded `source` when the catalog can't supply one.
/// - Source and version evidence are tracked as two separate signals: a
///   plugin with **no** resolvable source
///   (catalog miss AND no recorded fallback) is `Skipped` with a
///   source-unresolved reason. A plugin **with** a resolvable source but
///   incomplete version evidence (no catalog version, no recorded
///   `baselineVersion`, or the marketplace/catalog itself unavailable) is
///   [`AdoptedUpgradeOutcome::VersionUnresolved`] — never mislabeled
///   source-unresolved just because upgrade comparison couldn't run. Only
///   when both the catalog version and the baseline are present is the
///   catalog's current version compared against the baseline via
///   [`crate::self_update::is_newer_version`] to decide `Changed`/`Unchanged`.
///
/// Pure aside from the local-filesystem reads inside `read_known_marketplaces`
/// / `read_marketplace_plugin_source` (both already-existing seams from
/// `adopt.rs`); no network, no writes, no clone, no projection.
pub fn resolve_adopted_upgrades(lock: &Value) -> Vec<AdoptedUpgradeOutcome> {
    use crate::adopt::read_known_marketplaces;
    use crate::adopt::read_marketplace_plugin_source;
    use crate::self_update::is_newer_version;

    let mut out = Vec::new();
    let Some(entries) = lock.get(ADOPTED_KEY).and_then(Value::as_array) else {
        return out;
    };

    let known_marketplaces = read_known_marketplaces();

    for entry in entries {
        let Some(name) = entry.get("name").and_then(Value::as_str) else {
            continue;
        };
        let name = name.to_string();

        let source_id = entry.get("sourceId").and_then(Value::as_str).unwrap_or("");
        match adopted_kind_from_source_id(source_id) {
            "mcp" => {
                out.push(AdoptedUpgradeOutcome::Skipped {
                    name,
                    reason: MCP_SKIP_REASON.to_string(),
                });
                continue;
            }
            "skill" => {
                out.push(AdoptedUpgradeOutcome::Skipped {
                    name,
                    reason: SKILL_SKIP_REASON.to_string(),
                });
                continue;
            }
            _ => {}
        }

        // Plugin entry: attempt catalog re-resolution, falling back to the
        // entry's own recorded `source` when the catalog can't
        // supply one.
        let catalog_resolved =
            split_plugin_marketplace(&name).and_then(|(plugin_name, marketplace)| {
                let repo = known_marketplaces.get(marketplace);
                read_marketplace_plugin_source(marketplace, plugin_name, repo)
            });
        let (resolved_source, resolved_version, catalog_not_clonable) = match &catalog_resolved {
            Some(r) => (r.source.clone(), r.version.clone(), r.not_directly_clonable),
            None => (None, None, false),
        };
        let entry_recorded_source = entry
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_string);
        // The non-clonable marker belongs to the *catalog-resolved* URL only.
        // Falling back to the entry's own recorded `source` means the value
        // came from `ccync repair`, which writes a provider clone URL.
        let takeover_supported = resolved_source.is_none() || !catalog_not_clonable;
        let source = resolved_source.or(entry_recorded_source);

        // Source metadata and version evidence are split. Only a
        // *sourceless* item is `Skipped` with the source-unresolved reason —
        // a source-bearing item with incomplete version evidence (no catalog
        // version, no recorded baseline, or catalog/marketplace unavailable)
        // is `VersionUnresolved`, never mislabeled source-unresolved.
        let Some(source) = source else {
            out.push(AdoptedUpgradeOutcome::Skipped {
                name,
                reason: format!(
                    "adopted plugin — {SOURCE_UNRESOLVED_MARKER} (no clonable source recorded)"
                ),
            });
            continue;
        };

        let baseline = entry
            .get("baselineVersion")
            .and_then(Value::as_str)
            .map(str::to_string);

        let (Some(latest_version), Some(baseline)) = (resolved_version, baseline) else {
            out.push(AdoptedUpgradeOutcome::VersionUnresolved { name, source });
            continue;
        };

        if is_newer_version(&baseline, &latest_version) {
            out.push(AdoptedUpgradeOutcome::Changed {
                name,
                source,
                current_version: Some(baseline),
                latest_version,
                takeover_supported,
            });
        } else {
            out.push(AdoptedUpgradeOutcome::Unchanged { name });
        }
    }

    out
}

#[cfg(test)]
mod adopted_tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    // Serializes tests that mutate the process-wide HOME/USERPROFILE env var.
    // Shared crate-wide lock (see `crate::test_support::env_guard`) — must be
    // the SAME lock used by `adopt.rs`/`install.rs`'s tests, since env
    // mutation is process-wide and `cargo test` runs multithreaded across
    // modules in the same test binary.
    use crate::test_support::env_guard;

    pub(super) fn set_home(path: &Path) {
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", path);
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", path);
        }
    }

    pub(super) fn write_marketplace_fixture(
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

    pub(super) fn adopted_plugin_entry(name: &str, baseline: &str) -> Value {
        json!({
            "name": name,
            "adoptedFrom": "claude",
            "version": baseline,
            "sourceId": "claude-plugin",
            "baselineVersion": baseline,
        })
    }

    #[test]
    fn plugin_with_newer_catalog_version_is_changed() {
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
        let lock = json!({ "_adoptedItems": [adopted_plugin_entry("tool@market", "2.0.0")] });
        let out = resolve_adopted_upgrades(&lock);
        assert_eq!(
            out,
            vec![AdoptedUpgradeOutcome::Changed {
                name: "tool@market".into(),
                source: "https://github.com/example/tool-plugin.git".into(),
                current_version: Some("2.0.0".into()),
                latest_version: "2.1.0".into(),
                takeover_supported: true,
            }]
        );
    }

    #[test]
    fn plugin_with_same_catalog_version_is_unchanged() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        write_marketplace_fixture(
            tmp.path(),
            "market",
            "https://github.com/example/market.git",
            "tool",
            "2.0.0",
            "https://github.com/example/tool-plugin.git",
        );
        let lock = json!({ "_adoptedItems": [adopted_plugin_entry("tool@market", "2.0.0")] });
        let out = resolve_adopted_upgrades(&lock);
        assert_eq!(
            out,
            vec![AdoptedUpgradeOutcome::Unchanged {
                name: "tool@market".into()
            }]
        );
    }

    #[test]
    fn plugin_with_unresolved_marketplace_is_skipped_with_clear_reason() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        // No known_marketplaces.json / no local clone at all.
        let lock = json!({ "_adoptedItems": [adopted_plugin_entry("tool@nope", "1.0.0")] });
        let out = resolve_adopted_upgrades(&lock);
        assert_eq!(out.len(), 1);
        match &out[0] {
            AdoptedUpgradeOutcome::Skipped { name, reason } => {
                assert_eq!(name, "tool@nope");
                assert!(
                    reason.contains(SOURCE_UNRESOLVED_MARKER),
                    "reason: {reason}"
                );
            }
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[test]
    fn adopted_mcp_entry_is_skipped_with_d3_reason() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        let lock = json!({
            "_adoptedItems": [
                { "name": "memory", "adoptedFrom": "claude", "version": null, "sourceId": "claude-mcp" }
            ]
        });
        let out = resolve_adopted_upgrades(&lock);
        assert_eq!(
            out,
            vec![AdoptedUpgradeOutcome::Skipped {
                name: "memory".into(),
                reason: MCP_SKIP_REASON.to_string(),
            }]
        );
    }

    #[test]
    fn adopted_skill_entry_is_skipped_with_d3_reason() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        let lock = json!({
            "_adoptedItems": [
                { "name": "doc-sync", "adoptedFrom": "claude", "version": null, "sourceId": "claude-skill" }
            ]
        });
        let out = resolve_adopted_upgrades(&lock);
        assert_eq!(
            out,
            vec![AdoptedUpgradeOutcome::Skipped {
                name: "doc-sync".into(),
                reason: SKILL_SKIP_REASON.to_string(),
            }]
        );
    }

    #[test]
    fn plugin_not_in_catalog_is_skipped_source_unresolved() {
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
        let lock = json!({ "_adoptedItems": [adopted_plugin_entry("tool@market", "1.0.0")] });
        let out = resolve_adopted_upgrades(&lock);
        match &out[0] {
            AdoptedUpgradeOutcome::Skipped { reason, .. } => {
                assert!(
                    reason.contains(SOURCE_UNRESOLVED_MARKER),
                    "reason: {reason}"
                );
            }
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[test]
    fn plugin_with_resolved_source_but_no_catalog_version_is_version_unresolved() {
        // Source/version split: the catalog resolves the plugin's source but the
        // catalog entry itself carries no `version` field — the item must
        // never be mislabeled source-unresolved just because upgrade
        // comparison can't run.
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
            json!({
                "plugins": [
                    { "name": "tool", "source": "https://github.com/example/tool.git" }
                ]
            })
            .to_string(),
        )
        .unwrap();

        let lock = json!({ "_adoptedItems": [adopted_plugin_entry("tool@market", "1.0.0")] });
        let out = resolve_adopted_upgrades(&lock);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0],
            AdoptedUpgradeOutcome::VersionUnresolved {
                name: "tool@market".into(),
                source: "https://github.com/example/tool.git".into(),
            }
        );
    }

    #[test]
    fn plugin_with_resolved_source_but_no_baseline_version_is_version_unresolved() {
        // Same split, the other missing half: catalog has a version, but the
        // adopted entry itself carries no `baselineVersion`.
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        write_marketplace_fixture(
            tmp.path(),
            "market",
            "https://github.com/example/market.git",
            "tool",
            "2.1.0",
            "https://github.com/example/tool.git",
        );
        let lock = json!({
            "_adoptedItems": [{
                "name": "tool@market",
                "adoptedFrom": "claude",
                "version": "2.0.0",
                "sourceId": "claude-plugin"
            }]
        });
        let out = resolve_adopted_upgrades(&lock);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0],
            AdoptedUpgradeOutcome::VersionUnresolved {
                name: "tool@market".into(),
                source: "https://github.com/example/tool.git".into(),
            }
        );
    }

    #[test]
    fn plugin_with_entry_recorded_source_but_unresolvable_catalog_is_version_unresolved() {
        // No known_marketplaces.json / no local clone at all, but the entry
        // itself already carries a recorded `source` (e.g. from a prior
        // `ccync repair`) — must be VersionUnresolved, never Skipped, since
        // the source IS known.
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        let mut entry = adopted_plugin_entry("tool@nope", "1.0.0");
        entry["source"] = json!("https://github.com/example/already-repaired.git");
        let lock = json!({ "_adoptedItems": [entry] });
        let out = resolve_adopted_upgrades(&lock);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0],
            AdoptedUpgradeOutcome::VersionUnresolved {
                name: "tool@nope".into(),
                source: "https://github.com/example/already-repaired.git".into(),
            }
        );
    }

    /// A catalog entry whose `source` is an in-repo relative path resolves to
    /// a remote selector source and remains eligible for takeover.
    #[test]
    fn embedded_plugin_changed_supports_takeover() {
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
        // In-repo subpath source — the embedded-plugin case.
        std::fs::write(
            catalog_dir.join("marketplace.json"),
            json!({
                "plugins": [
                    { "name": "tool", "version": "2.1.0", "source": "./plugins/tool" }
                ]
            })
            .to_string(),
        )
        .unwrap();

        let lock = json!({ "_adoptedItems": [adopted_plugin_entry("tool@market", "2.0.0")] });
        let out = resolve_adopted_upgrades(&lock);
        assert_eq!(
            out,
            vec![AdoptedUpgradeOutcome::Changed {
                name: "tool@market".into(),
                source: "https://github.com/example/market.git//plugins/tool".into(),
                current_version: Some("2.0.0".into()),
                latest_version: "2.1.0".into(),
                takeover_supported: true,
            }]
        );
    }

    /// The non-clonable marker belongs to the catalog-resolved URL only. When
    /// resolution falls back to the entry's own recorded `source` — which
    /// `ccync repair` fills with a provider clone URL — take-over is
    /// supported again.
    #[test]
    fn directly_clonable_catalog_source_keeps_takeover_supported() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        write_marketplace_fixture(
            tmp.path(),
            "market",
            "https://github.com/example/market.git",
            "tool",
            "2.1.0",
            "https://github.com/example/tool.git",
        );
        let lock = json!({ "_adoptedItems": [adopted_plugin_entry("tool@market", "2.0.0")] });
        let out = resolve_adopted_upgrades(&lock);
        assert_eq!(
            out,
            vec![AdoptedUpgradeOutcome::Changed {
                name: "tool@market".into(),
                source: "https://github.com/example/tool.git".into(),
                current_version: Some("2.0.0".into()),
                latest_version: "2.1.0".into(),
                takeover_supported: true,
            }]
        );
    }

    #[test]
    fn version_unresolved_name_accessor_returns_item_name() {
        let outcome = AdoptedUpgradeOutcome::VersionUnresolved {
            name: "x@m".into(),
            source: "s".into(),
        };
        assert_eq!(outcome.name(), "x@m");
    }

    #[test]
    fn empty_or_missing_adopted_set_yields_nothing() {
        assert!(resolve_adopted_upgrades(&json!({})).is_empty());
        assert!(resolve_adopted_upgrades(&json!({ "_adoptedItems": [] })).is_empty());
    }

    /// A baseline that is *newer* than the catalog's current version (the
    /// local marketplace clone is stale) must never be reported as `Changed`
    /// — `is_newer_version(baseline, latest)` is false in that direction, so
    /// this must classify as `Unchanged`, never a bogus upgrade.
    #[test]
    fn baseline_newer_than_catalog_is_unchanged_not_changed() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        write_marketplace_fixture(
            tmp.path(),
            "market",
            "https://github.com/example/market.git",
            "tool",
            "1.0.0", // catalog is stale/behind the recorded baseline
            "https://github.com/example/tool-plugin.git",
        );
        let lock = json!({ "_adoptedItems": [adopted_plugin_entry("tool@market", "2.0.0")] });
        let out = resolve_adopted_upgrades(&lock);
        assert_eq!(
            out,
            vec![AdoptedUpgradeOutcome::Unchanged {
                name: "tool@market".into()
            }],
            "stale local catalog (baseline > latest) must never be reported as Changed"
        );
    }

    /// The same adopted plugin name appearing twice in `_adoptedItems` must be
    /// classified per-entry, independently, with no deduping — each entry
    /// gets its own outcome in the output.
    #[test]
    fn duplicate_adopted_entries_are_each_classified_independently() {
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
        let lock = json!({ "_adoptedItems": [
            adopted_plugin_entry("tool@market", "2.0.0"),
            adopted_plugin_entry("tool@market", "2.0.0"),
        ] });
        let out = resolve_adopted_upgrades(&lock);
        assert_eq!(out.len(), 2, "no dedup: both entries produce an outcome");
        for o in &out {
            assert!(
                matches!(o, AdoptedUpgradeOutcome::Changed { name, .. } if name == "tool@market")
            );
        }
    }

    /// When the catalog resolves the plugin as an embedded/in-repo-path
    /// plugin but the marketplace repo URL itself is unknown, `resolved.source`
    /// is `None` (per `resolve_plugin_source_in_catalog`'s "never fabricate"
    /// rule). The `.or_else` fallback must then use the previously-recorded
    /// previously-recorded `entry.get("source")` so a real newer version is still reported
    /// as `Changed` with a usable source, instead of being silently dropped
    /// to `Skipped`.
    #[test]
    fn resolved_source_none_falls_back_to_entry_recorded_source() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        // known_marketplaces.json exists but has no `source` for "market", so
        // the marketplace repo url is unknown; the plugin's catalog `source`
        // is an in-repo relative path, which cannot be resolved to a URL
        // without a known repo url => `resolved.source` is None.
        let plugins_dir = tmp.path().join(".claude").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("known_marketplaces.json"),
            json!({ "market": {} }).to_string(),
        )
        .unwrap();
        let catalog_dir = plugins_dir
            .join("marketplaces")
            .join("market")
            .join(".claude-plugin");
        std::fs::create_dir_all(&catalog_dir).unwrap();
        std::fs::write(
            catalog_dir.join("marketplace.json"),
            json!({
                "plugins": [
                    { "name": "tool", "version": "2.1.0", "source": "./plugins/tool" }
                ]
            })
            .to_string(),
        )
        .unwrap();

        let mut entry = adopted_plugin_entry("tool@market", "2.0.0");
        entry["source"] = json!("https://github.com/example/recorded-fallback.git");
        let lock = json!({ "_adoptedItems": [entry] });
        let out = resolve_adopted_upgrades(&lock);
        assert_eq!(
            out,
            vec![AdoptedUpgradeOutcome::Changed {
                name: "tool@market".into(),
                source: "https://github.com/example/recorded-fallback.git".into(),
                current_version: Some("2.0.0".into()),
                latest_version: "2.1.0".into(),
                takeover_supported: true,
            }],
            "must fall back to the entry's recorded source when catalog resolution has none"
        );
    }

    #[test]
    fn bare_plugin_name_with_no_marketplace_is_skipped() {
        let _env = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        let lock = json!({ "_adoptedItems": [adopted_plugin_entry("standalone-tool", "1.0.0")] });
        let out = resolve_adopted_upgrades(&lock);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], AdoptedUpgradeOutcome::Skipped { .. }));
    }

    // Structural guard: `AdoptedUpgradeOutcome` has no constructor path into
    // `UpgradeChange`/`apply_upgrade`. This is enforced by the type itself
    // (no shared fields, no `From`/`Into` impl between the two enums) rather
    // than by a runtime assertion — there is nothing here for such an
    // assertion to check. The test below simply pins down that `Changed`'s
    // shape stays distinct (no `plugin_id`/`old_sha`/`new_sha` naming that a
    // future refactor might be tempted to feed into `UpgradeChange` directly).
    #[test]
    fn changed_outcome_shape_is_distinct_from_upgrade_change() {
        let outcome = AdoptedUpgradeOutcome::Changed {
            name: "x@m".into(),
            source: "s".into(),
            current_version: Some("1.0.0".into()),
            latest_version: "1.1.0".into(),
            takeover_supported: true,
        };
        // `name()` accessor exists and is distinct from `UpgradeOutcome::plugin_id()`.
        assert_eq!(outcome.name(), "x@m");
    }
}

// ── upgrade orchestrator ──────────────────────────────────────────────────────

use std::path::PathBuf;

/// Inputs for [`run_upgrade`].
#[derive(Debug, Clone)]
pub struct UpgradeOpts {
    /// The personal-plugin set (lockfile or catalog value).
    pub plugins: Value,
    /// `Some(id)` to upgrade a single plugin, `None` for all git plugins.
    pub filter_id: Option<String>,
    /// `--dry-run`: classify only, never clone, re-pin, or project.
    pub dry_run: bool,
    /// Cache root (`~/.ccync/cache`).
    pub cache_root: PathBuf,
    /// Pin files to update (`plugins.json` and/or `lock.json`).
    pub pin_files: Vec<PathBuf>,
}

/// Aggregate result of [`run_upgrade`] — every plugin is accounted for.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct UpgradeReport {
    /// Plugin ids successfully re-cloned + re-pinned.
    pub upgraded: Vec<String>,
    /// Plugin ids already at the latest remote HEAD.
    pub unchanged: Vec<String>,
    /// `(id, reason)` for non-git plugins skipped.
    pub skipped: Vec<(String, String)>,
    /// `(id, reason)` for plugins that errored (resolve or apply).
    pub failed: Vec<(String, String)>,
    /// Plugin ids held (`ccync pin`): skipped, kept at their current sha.
    pub held: Vec<String>,
    /// Pending changes (populated on dry-run; what *would* be upgraded).
    pub pending: Vec<UpgradeChange>,
    /// Whether `run_unified_projection` was invoked.
    pub projected: bool,
    /// Whether this was a dry run (nothing written).
    pub dry_run: bool,
    /// Top-level error (e.g. a `--plugin <id>` that does not exist).
    pub error: Option<String>,
}

/// Orchestrate `ccync upgrade`: resolve → (dry-run diff | apply changed) →
/// project. Network (`ls_remote`), clone, and projection are injected as seams
/// so the control flow is unit-testable. Per-plugin failures are isolated; the
/// projection seam runs once at the end only when a real pin changed.
pub fn run_upgrade(
    opts: &UpgradeOpts,
    ls_remote: impl Fn(&str, &str) -> Result<String, String>,
    clone: impl Fn(&str, &Path) -> Result<(), String>,
    project: impl Fn() -> Result<(), String>,
) -> UpgradeReport {
    let mut report = UpgradeReport {
        dry_run: opts.dry_run,
        ..Default::default()
    };

    let mut outcomes = resolve_upgrades(&opts.plugins, ls_remote);

    if let Some(want) = &opts.filter_id {
        outcomes.retain(|o| o.plugin_id() == want);
        if outcomes.is_empty() {
            report.error = Some(format!("no personal plugin with id '{want}'"));
            return report;
        }
    }

    let pin_files: Vec<&Path> = opts.pin_files.iter().map(PathBuf::as_path).collect();

    for outcome in outcomes {
        match outcome {
            UpgradeOutcome::Unchanged { plugin_id } => report.unchanged.push(plugin_id),
            UpgradeOutcome::Held { plugin_id } => report.held.push(plugin_id),
            UpgradeOutcome::Skipped { plugin_id, reason } => {
                report.skipped.push((plugin_id, reason))
            }
            UpgradeOutcome::Failed { plugin_id, reason } => report.failed.push((plugin_id, reason)),
            UpgradeOutcome::Changed {
                plugin_id,
                source,
                old_sha,
                new_sha,
            } => {
                let change = UpgradeChange {
                    plugin_id: plugin_id.clone(),
                    source,
                    old_sha,
                    new_sha,
                };
                if opts.dry_run {
                    report.pending.push(change);
                    continue;
                }
                match apply_upgrade(&change, &opts.cache_root, &pin_files, &clone) {
                    Ok(()) => report.upgraded.push(plugin_id),
                    Err(e) => report.failed.push((plugin_id, e.to_string())),
                }
            }
        }
    }

    if !opts.dry_run && !report.upgraded.is_empty() {
        match project() {
            Ok(()) => report.projected = true,
            Err(e) => report.failed.push(("<projection>".to_string(), e)),
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn git_entry(id: &str, pin: Option<&str>) -> Value {
        let mut e = json!({
            "pluginId": id,
            "installStrategy": "personal-git-clone",
            "source": format!("https://github.com/user/{id}.git"),
        });
        if let Some(p) = pin {
            e["pin"] = json!(p);
        }
        e
    }

    #[test]
    fn changed_when_remote_differs() {
        let plugins = json!({ "_personalPlugins": [git_entry("a", Some("oldsha"))] });
        let out = resolve_upgrades(&plugins, |_, _| Ok("newsha".to_string()));
        assert_eq!(
            out,
            vec![UpgradeOutcome::Changed {
                plugin_id: "a".into(),
                source: "https://github.com/user/a.git".into(),
                old_sha: Some("oldsha".into()),
                new_sha: "newsha".into(),
            }]
        );
    }

    #[test]
    fn held_entry_is_held_even_when_remote_differs() {
        let plugins = json!({ "_personalPlugins": [{
            "pluginId": "a",
            "installStrategy": "personal-git-clone",
            "source": "https://github.com/user/a.git",
            "pin": "old",
            "held": true
        }] });
        // ls_remote returns a *different* sha, but held must short-circuit.
        let out = resolve_upgrades(&plugins, |_, _| Ok("new".to_string()));
        assert_eq!(
            out,
            vec![UpgradeOutcome::Held {
                plugin_id: "a".into()
            }]
        );
    }

    #[test]
    fn unchanged_when_remote_matches_pin() {
        let plugins = json!({ "_personalPlugins": [git_entry("a", Some("samesha"))] });
        let out = resolve_upgrades(&plugins, |_, _| Ok("samesha".to_string()));
        assert_eq!(
            out,
            vec![UpgradeOutcome::Unchanged {
                plugin_id: "a".into()
            }]
        );
    }

    #[test]
    fn archive_and_local_are_skipped() {
        let plugins = json!({
            "_personalPlugins": [
                { "pluginId": "arch", "installStrategy": "personal-archive", "sourceType": "local", "source": "x.tar.gz", "pinnedSha": "h" },
                { "pluginId": "loc", "installStrategy": "personal-archive", "sourceType": "local", "source": "/path/to/p" }
            ]
        });
        let out = resolve_upgrades(&plugins, |_, _| Ok("never".to_string()));
        assert!(matches!(out[0], UpgradeOutcome::Skipped { .. }));
        assert!(matches!(out[1], UpgradeOutcome::Skipped { .. }));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn ls_remote_failure_is_isolated_failed() {
        let plugins =
            json!({ "_personalPlugins": [git_entry("a", Some("p")), git_entry("b", Some("q"))] });
        let out = resolve_upgrades(&plugins, |src, _| {
            if src.contains("/a.git") {
                Err("network".to_string())
            } else {
                Ok("q".to_string())
            }
        });
        assert!(matches!(out[0], UpgradeOutcome::Failed { .. }), "a failed");
        assert!(
            matches!(out[1], UpgradeOutcome::Unchanged { .. }),
            "b still resolved"
        );
    }

    #[test]
    fn git_entry_without_pin_is_changed() {
        // A git plugin with no recorded pin is always treated as changed.
        let plugins = json!({ "_personalPlugins": [git_entry("a", None)] });
        let out = resolve_upgrades(&plugins, |_, _| Ok("newsha".to_string()));
        match &out[0] {
            UpgradeOutcome::Changed {
                old_sha, new_sha, ..
            } => {
                assert_eq!(old_sha, &None);
                assert_eq!(new_sha, "newsha");
            }
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[test]
    fn empty_or_missing_personal_set_yields_nothing() {
        assert!(resolve_upgrades(&json!({}), |_, _| Ok("x".to_string())).is_empty());
        assert!(
            resolve_upgrades(&json!({ "_personalPlugins": [] }), |_, _| Ok(
                "x".to_string()
            ))
            .is_empty()
        );
    }

    #[test]
    fn old_lockfile_adopted_plugin_without_baseline_is_skipped_not_panics() {
        // An old-format adopted item entry that has no `baselineVersion` field
        // must produce a Skipped outcome from `resolve_adopted_upgrades` and never panic.
        // The entry has `sourceId: "claude-plugin"` and no `@marketplace` separator in
        // its name, so it takes the "no marketplace recorded" skip path.
        let lock = json!({
            "_adoptedItems": [
                {
                    "name": "standalone-old-plugin",
                    "adoptedFrom": "claude",
                    "version": "1.0.0",
                    "sourceId": "claude-plugin"
                }
            ]
        });
        let out = resolve_adopted_upgrades(&lock);
        assert_eq!(out.len(), 1);
        assert!(
            matches!(&out[0], AdoptedUpgradeOutcome::Skipped { name, .. } if name == "standalone-old-plugin"),
            "expected Skipped, got {:?}",
            out[0]
        );
    }

    #[test]
    fn personal_held_plugin_upgrade_stays_held() {
        // A personal plugin with `held: true` must produce `UpgradeOutcome::Held`
        // from `resolve_upgrades`. The ls_remote seam must never be called for it.
        let plugins = json!({
            "_personalPlugins": [{
                "pluginId": "frozen-plugin",
                "installStrategy": "personal-git-clone",
                "source": "https://github.com/user/frozen.git",
                "pin": "deadbeef",
                "held": true
            }]
        });
        let out = resolve_upgrades(&plugins, |_, _| {
            panic!("ls_remote must not be called for held plugin")
        });
        assert_eq!(
            out,
            vec![UpgradeOutcome::Held {
                plugin_id: "frozen-plugin".into()
            }]
        );
    }
}

#[cfg(test)]
mod apply_tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!(
            "ccync-apply-{tag}-{:?}",
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn change(id: &str, new: &str) -> UpgradeChange {
        UpgradeChange {
            plugin_id: id.into(),
            source: format!("https://github.com/user/{id}.git"),
            old_sha: Some("oldsha".into()),
            new_sha: new.into(),
        }
    }

    fn write_lock(dir: &std::path::Path, id: &str, pin: &str) -> std::path::PathBuf {
        let lock = dir.join("lock.json");
        fs::write(
            &lock,
            serde_json::to_string_pretty(&json!({
                "_personalPlugins": [
                    { "pluginId": id, "installStrategy": "personal-git-clone",
                      "source": format!("https://github.com/user/{id}.git"), "pin": pin }
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        lock
    }

    #[test]
    fn clone_then_pin_updates_lock_and_creates_cache() {
        let d = tmp_dir("ok");
        let cache = d.join("cache");
        let lock = write_lock(&d, "a", "oldsha");

        // clone seam: create the cache dir with a file (simulating a real clone)
        let res = apply_upgrade(
            &change("a", "newsha"),
            &cache,
            &[lock.as_path()],
            |_, dest| {
                fs::create_dir_all(dest).unwrap();
                fs::write(dest.join("plugin.json"), b"{}").unwrap();
                Ok(())
            },
        );
        assert!(res.is_ok(), "got {res:?}");
        assert!(cache.join("a@newsha").is_dir(), "cache dir created");

        let v: Value = serde_json::from_str(&fs::read_to_string(&lock).unwrap()).unwrap();
        assert_eq!(v["_personalPlugins"][0]["pin"], "newsha", "pin re-written");
        // file still valid + retains other fields (no truncate)
        assert_eq!(v["_personalPlugins"][0]["pluginId"], "a");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn clone_failure_leaves_pin_unchanged() {
        let d = tmp_dir("clonefail");
        let cache = d.join("cache");
        let lock = write_lock(&d, "a", "oldsha");

        let res = apply_upgrade(&change("a", "newsha"), &cache, &[lock.as_path()], |_, _| {
            Err("network down".to_string())
        });
        assert!(matches!(res, Err(UpgradeError::Clone(_))));
        let v: Value = serde_json::from_str(&fs::read_to_string(&lock).unwrap()).unwrap();
        assert_eq!(
            v["_personalPlugins"][0]["pin"], "oldsha",
            "pin untouched on clone fail"
        );
        assert!(!cache.join("a@newsha").exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn cache_not_landed_leaves_pin_unchanged() {
        let d = tmp_dir("noland");
        let cache = d.join("cache");
        let lock = write_lock(&d, "a", "oldsha");

        // clone seam "succeeds" but creates nothing → cache-verify must catch it.
        let res = apply_upgrade(&change("a", "newsha"), &cache, &[lock.as_path()], |_, _| {
            Ok(())
        });
        assert!(matches!(res, Err(UpgradeError::CacheVerify(_))));
        let v: Value = serde_json::from_str(&fs::read_to_string(&lock).unwrap()).unwrap();
        assert_eq!(
            v["_personalPlugins"][0]["pin"], "oldsha",
            "pin untouched when cache absent"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn missing_pin_entry_errs_after_clone() {
        let d = tmp_dir("nopin");
        let cache = d.join("cache");
        let lock = write_lock(&d, "other", "x"); // entry for a different plugin

        let res = apply_upgrade(
            &change("a", "newsha"),
            &cache,
            &[lock.as_path()],
            |_, dest| {
                fs::create_dir_all(dest).unwrap();
                fs::write(dest.join("f"), b"x").unwrap();
                Ok(())
            },
        );
        assert!(matches!(res, Err(UpgradeError::PinMissing(_))));
        let _ = fs::remove_dir_all(&d);
    }
}

#[cfg(test)]
mod orchestrator_tests {
    use super::*;
    use serde_json::json;
    use std::cell::Cell;
    use std::fs;

    fn tmp_dir(tag: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!(
            "ccync-run-up-{tag}-{:?}",
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn git(id: &str, pin: &str) -> Value {
        json!({
            "pluginId": id, "installStrategy": "personal-git-clone",
            "source": format!("https://github.com/user/{id}.git"), "pin": pin
        })
    }

    fn opts(plugins: Value, dir: &std::path::Path, filter: Option<&str>, dry: bool) -> UpgradeOpts {
        let lock = dir.join("lock.json");
        fs::write(&lock, serde_json::to_string_pretty(&plugins).unwrap()).unwrap();
        UpgradeOpts {
            plugins,
            filter_id: filter.map(String::from),
            dry_run: dry,
            cache_root: dir.join("cache"),
            pin_files: vec![lock],
        }
    }

    fn ok_clone() -> impl Fn(&str, &Path) -> Result<(), String> {
        |_, dest| {
            fs::create_dir_all(dest).unwrap();
            fs::write(dest.join("p"), b"x").unwrap();
            Ok(())
        }
    }

    #[test]
    fn dry_run_classifies_without_apply_or_project() {
        let d = tmp_dir("dry");
        let plugins = json!({ "_personalPlugins": [git("a", "old")] });
        let o = opts(plugins, &d, None, true);
        let applied = Cell::new(false);
        let projected = Cell::new(false);

        let r = run_upgrade(
            &o,
            |_, _| Ok("new".to_string()),
            |_, _| {
                applied.set(true);
                Ok(())
            },
            || {
                projected.set(true);
                Ok(())
            },
        );
        assert!(r.dry_run);
        assert_eq!(r.pending.len(), 1, "one pending change reported");
        assert!(r.upgraded.is_empty());
        assert!(
            !applied.get() && !projected.get(),
            "dry-run touches nothing"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn pending_change_carries_old_sha() {
        // A Changed outcome must thread its old_sha into UpgradeChange so
        // dry-run / list --upgrade-available can render `current → latest`.
        let d = tmp_dir("oldsha");
        let plugins = json!({ "_personalPlugins": [git("a", "old")] });
        let o = opts(plugins, &d, None, true);

        let r = run_upgrade(&o, |_, _| Ok("new".to_string()), ok_clone(), || Ok(()));
        assert_eq!(r.pending.len(), 1);
        assert_eq!(
            r.pending[0].old_sha.as_deref(),
            Some("old"),
            "old_sha must be carried through, not dropped"
        );
        assert_eq!(r.pending[0].new_sha, "new");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn real_run_upgrades_changed_and_projects_once() {
        let d = tmp_dir("real");
        let plugins = json!({ "_personalPlugins": [git("a", "old"), git("b", "same")] });
        let o = opts(plugins, &d, None, false);
        let project_calls = Cell::new(0);

        let r = run_upgrade(
            &o,
            |src, _| {
                Ok(if src.contains("/a.git") {
                    "new"
                } else {
                    "same"
                }
                .to_string())
            },
            ok_clone(),
            || {
                project_calls.set(project_calls.get() + 1);
                Ok(())
            },
        );
        assert_eq!(r.upgraded, vec!["a".to_string()]);
        assert_eq!(r.unchanged, vec!["b".to_string()]);
        assert!(r.projected);
        assert_eq!(project_calls.get(), 1, "projection runs exactly once");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn no_changes_skips_projection() {
        let d = tmp_dir("nochange");
        let plugins = json!({ "_personalPlugins": [git("a", "same")] });
        let o = opts(plugins, &d, None, false);
        let projected = Cell::new(false);

        let r = run_upgrade(
            &o,
            |_, _| Ok("same".to_string()),
            ok_clone(),
            || {
                projected.set(true);
                Ok(())
            },
        );
        assert!(r.upgraded.is_empty());
        assert!(
            !r.projected && !projected.get(),
            "no pin change → no projection"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn single_id_filter_processes_only_that_plugin() {
        let d = tmp_dir("filter");
        let plugins = json!({ "_personalPlugins": [git("a", "old"), git("b", "old")] });
        let o = opts(plugins, &d, Some("b"), false);

        let r = run_upgrade(&o, |_, _| Ok("new".to_string()), ok_clone(), || Ok(()));
        assert_eq!(r.upgraded, vec!["b".to_string()]);
        assert!(r.error.is_none());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn unknown_filter_id_is_actionable_error() {
        let d = tmp_dir("badfilter");
        let plugins = json!({ "_personalPlugins": [git("a", "old")] });
        let o = opts(plugins, &d, Some("nope"), false);

        let r = run_upgrade(&o, |_, _| Ok("new".to_string()), ok_clone(), || Ok(()));
        assert!(r.error.is_some(), "missing id reports an error");
        assert!(r.upgraded.is_empty());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn one_apply_failure_isolated_from_others() {
        let d = tmp_dir("isolate");
        let plugins = json!({ "_personalPlugins": [git("a", "old"), git("b", "old")] });
        let o = opts(plugins, &d, None, false);

        // clone fails only for plugin a.
        let r = run_upgrade(
            &o,
            |_, _| Ok("new".to_string()),
            |src, dest| {
                if src.contains("/a.git") {
                    Err("network".to_string())
                } else {
                    fs::create_dir_all(dest).unwrap();
                    fs::write(dest.join("p"), b"x").unwrap();
                    Ok(())
                }
            },
            || Ok(()),
        );
        assert_eq!(r.upgraded, vec!["b".to_string()], "b still upgraded");
        assert_eq!(r.failed.len(), 1, "a failed but did not abort b");
        assert_eq!(r.failed[0].0, "a");
        let _ = fs::remove_dir_all(&d);
    }
}
