//! Immutable per-provider render generations.
//!
//! Builds a provider inventory from the pinned plugin sources recorded in the
//! lockfile and renders each provider into its own immutable generation under
//! `~/.ccync/build/render/providers/<source-key>/<generation>/`.
//!
//! Unlike the combined canonical render, a generation keeps the provider's
//! whole payload topology (skills, commands, agents, hooks, resources,
//! manifests, authorship and policy files) and is never merged with another
//! provider, so two providers exporting the same skill basename stay intact
//! side by side.
//!
//! Guarantees:
//!
//! - Source keys come from the lockfile plugin id (source identity, never
//!   display text) and are lowercased, so two providers can never share one
//!   generation directory on a case-insensitive filesystem. Two distinct
//!   source records that normalize to one key are an explicit conflict.
//! - The generation id is derived from the package digest, so identical
//!   input always maps to the same generation directory.
//! - A generation is staged in a sibling directory on the same filesystem,
//!   validated against the source digest, and published with a rename. A
//!   published generation is never overwritten or deleted here; a failed
//!   render removes its staging directory and leaves prior generations as
//!   they were.
//! - Loose skills without recorded provenance are not providers and are not
//!   part of the inventory. MCP aggregation stays with the existing render.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use ccync_foundation::provider::{
    upstream_revision_from_lock_entry, ComponentIdentity, ComponentKind, ComponentOrigin,
    GenerationId, PackageDigest, ProviderContractError, ProviderGeneration, ProviderSourceKey,
    RelativePath,
};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use thiserror::Error;

use crate::install::{cache_candidates, collect_dir_snapshot, is_link_like, pick_live_dirs};

/// Domain tag mixed into every package digest. Changing the generation
/// layout must change this tag so old and new layouts never share an id.
const DIGEST_DOMAIN: &[u8] = b"ccync-provider-generation\0v1\0";

/// Number of digest hex characters used in a generation id.
const GENERATION_ID_HEX_LEN: usize = 16;

/// Prefix of a staging directory inside a provider directory.
const STAGE_PREFIX: &str = ".stage-";

/// Manifest files consulted, in order, for the original plugin name.
const PLUGIN_MANIFESTS: &[&str] = &[
    ".claude-plugin/plugin.json",
    "plugin.json",
    ".codex-plugin/plugin.json",
];

/// Lockfile source-record key holding the explicit catalog package name.
const CATALOG_NAME_KEY: &str = "packageName";

/// Directory basenames too generic to identify a package. A skill name that
/// is only one of these is not authoritative evidence of the package name.
const GENERIC_BASENAMES: &[&str] = &[
    "app", "dist", "lib", "package", "packages", "plugin", "plugins", "skill", "skills", "src",
];

/// Top-level payload entries that are version-control metadata, not payload.
const EXCLUDED_TOP_LEVEL: &[&str] = &[".git"];

// ── errors ───────────────────────────────────────────────────────────────────

/// Error raised while building the provider inventory or rendering a
/// provider generation.
#[derive(Debug, Error)]
pub enum ProviderRenderError {
    /// The home directory could not be resolved.
    #[error("could not determine the ccync home directory")]
    NoHome,
    /// The lockfile exists but cannot be read or parsed.
    #[error("cannot read lockfile {path}: {message}")]
    Lockfile { path: PathBuf, message: String },
    /// A lockfile source record is malformed.
    #[error("invalid {record} source record: {message}")]
    InvalidSourceRecord {
        record: ProviderRecordKind,
        message: String,
    },
    /// A plugin id cannot become a safe provider source key.
    #[error("{record} plugin '{plugin_id}' cannot be used as a provider source key: {source}")]
    InvalidSourceKey {
        record: ProviderRecordKind,
        plugin_id: String,
        #[source]
        source: ProviderContractError,
    },
    /// Two distinct source records normalize to the same provider key.
    #[error(
        "provider source key '{key}' is claimed by both {first} and {second}; \
         rename one of the plugins so each source has its own key"
    )]
    SourceKeyConflict {
        key: String,
        first: String,
        second: String,
    },
    /// A provider manifest is present but unusable.
    #[error("plugin '{plugin_id}' has an invalid manifest {path}: {reason}")]
    InvalidManifest {
        plugin_id: String,
        path: PathBuf,
        reason: String,
    },
    /// An explicit package name outside a manifest is empty or contains
    /// control characters.
    #[error("plugin '{plugin_id}' declares an invalid {origin} package name: {reason}")]
    InvalidPackageName {
        plugin_id: String,
        origin: String,
        reason: String,
    },
    /// The provider payload could not be read safely.
    #[error("cannot read payload of plugin '{plugin_id}': {message}")]
    Payload { plugin_id: String, message: String },
    /// The provider payload contains no files.
    #[error("plugin '{plugin_id}' has an empty payload at {path}")]
    EmptyPayload { plugin_id: String, path: PathBuf },
    /// A staged generation failed validation before publication.
    #[error("generation for plugin '{plugin_id}' failed validation: {message}")]
    Validation { plugin_id: String, message: String },
    /// A published generation no longer matches the digest its id encodes.
    #[error(
        "published generation {path} does not match digest {expected}; \
         it is left untouched and must be inspected before re-rendering"
    )]
    GenerationMismatch { path: PathBuf, expected: String },
    /// A path that must be a real directory is a link or reparse point.
    #[error("refusing to write through link or reparse point {path}")]
    LinkedPath { path: PathBuf },
    /// A filesystem operation failed.
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A provider contract value failed validation.
    #[error(transparent)]
    Contract(#[from] ProviderContractError),
    /// A hook declaration cannot be resolved inside the package.
    #[error("plugin '{plugin_id}' has an invalid hook declaration in {origin}: {reason}")]
    InvalidHookDeclaration {
        plugin_id: String,
        origin: String,
        reason: String,
    },
    /// Provider manifests disagree on the plugin identity.
    #[error("plugin '{plugin_id}' declares a conflicting identity in {path}: {reason}")]
    ManifestConflict {
        plugin_id: String,
        path: String,
        reason: String,
    },
    /// A skill sidecar is malformed or uses unsupported YAML; nothing is
    /// written for the view.
    #[error("plugin '{plugin_id}' has an unsupported skill sidecar {path}: {reason}")]
    InvalidSidecar {
        plugin_id: String,
        path: String,
        reason: String,
    },
    /// The native package version cannot be formed or fails the target
    /// version grammar.
    #[error("cannot version the {target} package of plugin '{plugin_id}': {reason}")]
    InvalidNativeVersion {
        target: NativePackageTarget,
        plugin_id: String,
        reason: String,
    },
    /// A declaration the target view cannot carry safely blocks that view.
    #[error("cannot build the {target} package of plugin '{plugin_id}': {reason}")]
    UnsupportedNativeDeclaration {
        target: NativePackageTarget,
        plugin_id: String,
        reason: String,
    },
    /// A declaration is not eligible for non-live portable package storage.
    #[error(transparent)]
    PortablePackageRejected(#[from] PortablePackageRejection),
}

fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> ProviderRenderError + '_ {
    move |source| ProviderRenderError::Io {
        path: path.to_path_buf(),
        source,
    }
}

// ── inventory ────────────────────────────────────────────────────────────────

/// Lockfile namespace a provider source record came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProviderRecordKind {
    /// A bundled catalog plugin (`resolvedPlugins`, `bundled-local`).
    Bundled,
    /// A personal plugin (`_personalPlugins`).
    Personal,
}

impl std::fmt::Display for ProviderRecordKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ProviderRecordKind::Bundled => "bundled",
            ProviderRecordKind::Personal => "personal",
        })
    }
}

/// One provider whose payload is available for rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderEntry {
    /// Lowercased source key derived from the lockfile plugin id.
    pub key: ProviderSourceKey,
    /// Lockfile namespace of the source record.
    pub record: ProviderRecordKind,
    /// Plugin id exactly as recorded in the lockfile.
    pub plugin_id: String,
    /// Recorded source (resolved bundle path or fetch source).
    pub source: String,
    /// Original plugin name declared by the provider manifest, or the plugin
    /// id when the provider declares none.
    pub plugin_name: String,
    /// Upstream revision (pin) recorded for the source, when known.
    pub upstream_revision: Option<String>,
    /// Pinned payload directory the generation is rendered from.
    pub payload_dir: PathBuf,
}

/// A provider with a source record but no renderable payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnavailableProvider {
    /// Source key derived from the plugin id.
    pub key: ProviderSourceKey,
    /// Lockfile namespace of the source record.
    pub record: ProviderRecordKind,
    /// Plugin id exactly as recorded in the lockfile.
    pub plugin_id: String,
    /// Why the payload is not available.
    pub reason: String,
}

/// Providers resolved from pinned sources, ordered by source key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderInventory {
    /// Providers whose pinned payload is present.
    pub providers: Vec<ProviderEntry>,
    /// Providers recorded in the lockfile whose payload cannot be rendered.
    /// Callers must not treat these as removed.
    pub unavailable: Vec<UnavailableProvider>,
}

/// Derive the provider source key for a plugin id: lowercase, then validate
/// as a single safe path segment.
pub fn derive_source_key(
    record: ProviderRecordKind,
    plugin_id: &str,
) -> Result<ProviderSourceKey, ProviderRenderError> {
    ProviderSourceKey::new(plugin_id.to_lowercase()).map_err(|source| {
        ProviderRenderError::InvalidSourceKey {
            record,
            plugin_id: plugin_id.to_string(),
            source,
        }
    })
}

/// Build the provider inventory from the lockfile at its default location.
///
/// An absent lockfile yields an empty inventory. A lockfile that exists but
/// cannot be parsed is an error, so callers never mistake a corrupt lockfile
/// for "no providers".
pub fn load_provider_inventory() -> Result<ProviderInventory, ProviderRenderError> {
    let layout = ccync_foundation::paths::EffectiveLayout::load_or_legacy()
        .map_err(|_| ProviderRenderError::NoHome)?;
    let lock_path = layout.build_lock_path();
    let plugins_root = crate::paths::ccync_plugins_root().ok_or(ProviderRenderError::NoHome)?;
    let cache_root = layout.source_cache_path();
    let lock = match fs::read_to_string(&lock_path) {
        Ok(text) => {
            serde_json::from_str::<Value>(&text).map_err(|e| ProviderRenderError::Lockfile {
                path: lock_path.clone(),
                message: e.to_string(),
            })?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProviderInventory::default())
        }
        Err(e) => {
            return Err(ProviderRenderError::Lockfile {
                path: lock_path,
                message: e.to_string(),
            })
        }
    };
    build_provider_inventory(&lock, &plugins_root, &cache_root)
}

/// One lockfile source record before payload resolution.
struct SourceRecord<'a> {
    record: ProviderRecordKind,
    plugin_id: &'a str,
    source: &'a str,
    entry: &'a Value,
}

/// Build the provider inventory from a parsed lockfile.
///
/// Bundled plugins resolve under `plugins_root/<resolvedSource>`; personal
/// plugins resolve to their pinned `<id>@<pin>` cache directory under
/// `cache_root`, using the same live-directory selection as the combined
/// render. Every source key is checked for conflicts before any payload is
/// read.
pub fn build_provider_inventory(
    lock: &Value,
    plugins_root: &Path,
    cache_root: &Path,
) -> Result<ProviderInventory, ProviderRenderError> {
    let records = collect_source_records(lock)?;

    let mut claimed: BTreeMap<ProviderSourceKey, String> = BTreeMap::new();
    let mut keyed = Vec::with_capacity(records.len());
    for record in &records {
        let key = derive_source_key(record.record, record.plugin_id)?;
        let label = format!("{} plugin '{}'", record.record, record.plugin_id);
        if let Some(first) = claimed.get(&key) {
            return Err(ProviderRenderError::SourceKeyConflict {
                key: key.to_string(),
                first: first.clone(),
                second: label,
            });
        }
        claimed.insert(key.clone(), label);
        keyed.push((key, record));
    }

    let mut inventory = ProviderInventory::default();
    for (key, record) in keyed {
        match resolve_payload_dir(record, plugins_root, cache_root)? {
            Ok(payload_dir) => {
                let catalog_name = record.entry.get(CATALOG_NAME_KEY).and_then(Value::as_str);
                let plugin_name = read_plugin_name(record.plugin_id, &payload_dir, catalog_name)?;
                inventory.providers.push(ProviderEntry {
                    key,
                    record: record.record,
                    plugin_id: record.plugin_id.to_string(),
                    source: record.source.to_string(),
                    plugin_name,
                    upstream_revision: upstream_revision_from_lock_entry(record.entry),
                    payload_dir,
                });
            }
            Err(reason) => inventory.unavailable.push(UnavailableProvider {
                key,
                record: record.record,
                plugin_id: record.plugin_id.to_string(),
                reason,
            }),
        }
    }
    inventory.providers.sort_by(|a, b| a.key.cmp(&b.key));
    inventory.unavailable.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(inventory)
}

/// Read bundled and personal source records from the lockfile. Entries
/// without a plugin id or source are not source records and are skipped, as
/// in the combined render.
fn collect_source_records(lock: &Value) -> Result<Vec<SourceRecord<'_>>, ProviderRenderError> {
    if !lock.is_object() {
        return Err(ProviderRenderError::InvalidSourceRecord {
            record: ProviderRecordKind::Bundled,
            message: "lockfile must be a JSON object".to_string(),
        });
    }
    let mut records = Vec::new();
    if let Some(plugins) = lock.get("resolvedPlugins").and_then(Value::as_array) {
        for entry in plugins {
            if entry.get("installStrategy").and_then(Value::as_str) != Some("bundled-local") {
                continue;
            }
            let (Some(plugin_id), Some(source)) = (
                entry.get("pluginId").and_then(Value::as_str),
                entry.get("resolvedSource").and_then(Value::as_str),
            ) else {
                continue;
            };
            records.push(SourceRecord {
                record: ProviderRecordKind::Bundled,
                plugin_id,
                source,
                entry,
            });
        }
    }
    if let Some(plugins) = lock.get("_personalPlugins").and_then(Value::as_array) {
        for entry in plugins {
            let (Some(plugin_id), Some(source)) = (
                entry.get("pluginId").and_then(Value::as_str),
                entry.get("source").and_then(Value::as_str),
            ) else {
                continue;
            };
            records.push(SourceRecord {
                record: ProviderRecordKind::Personal,
                plugin_id,
                source,
                entry,
            });
        }
    }
    Ok(records)
}

/// Resolve the pinned payload directory of one source record. The outer
/// error is a hard failure; the inner `Err` is a reason the payload is not
/// available right now.
fn resolve_payload_dir(
    record: &SourceRecord<'_>,
    plugins_root: &Path,
    cache_root: &Path,
) -> Result<Result<PathBuf, String>, ProviderRenderError> {
    match record.record {
        ProviderRecordKind::Bundled => {
            let relative = RelativePath::new(record.source).map_err(|e| {
                ProviderRenderError::InvalidSourceRecord {
                    record: record.record,
                    message: format!("plugin '{}': {e}", record.plugin_id),
                }
            })?;
            let dir = relative.join_to(plugins_root);
            if dir.is_dir() {
                Ok(Ok(dir))
            } else {
                Ok(Err(format!(
                    "bundle directory {} is missing",
                    dir.display()
                )))
            }
        }
        ProviderRecordKind::Personal => {
            let pin = upstream_revision_from_lock_entry(record.entry);
            let candidates = cache_candidates(cache_root, record.plugin_id);
            let (chosen, ambiguous) = pick_live_dirs(&candidates, pin.as_deref(), record.plugin_id);
            if ambiguous {
                return Ok(Err(format!(
                    "{} cache directories and no recorded pin",
                    candidates.len()
                )));
            }
            match chosen.as_slice() {
                [dir] => Ok(Ok(dir.clone())),
                [] => Ok(Err("no pinned cache directory".to_string())),
                _ => Ok(Err(format!(
                    "{} cache directories match the pin",
                    chosen.len()
                ))),
            }
        }
    }
}

/// The canonical package name a provider declares, when one is authoritative.
///
/// Precedence: the first manifest that declares a name, then the explicit
/// catalog `packageName`, then the front-matter `name` of a bare skill
/// payload. A manifest name and a catalog name that disagree are a conflict.
/// A skill name that is only a generic basename is not evidence, so the
/// result is `None` and the caller keeps the storage id.
pub fn authoritative_plugin_name(
    plugin_id: &str,
    payload_dir: &Path,
    catalog_name: Option<&str>,
) -> Result<Option<String>, ProviderRenderError> {
    let manifest = manifest_plugin_name(plugin_id, payload_dir)?;
    let catalog = match catalog_name {
        Some(name) if is_usable_name(name) => Some(name.to_string()),
        Some(name) => {
            return Err(ProviderRenderError::InvalidPackageName {
                plugin_id: plugin_id.to_string(),
                origin: CATALOG_NAME_KEY.to_string(),
                reason: format!("'{name}' must be non-empty without control characters"),
            })
        }
        None => None,
    };
    if let (Some(manifest), Some(catalog)) = (&manifest, &catalog) {
        if manifest != catalog {
            return Err(ProviderRenderError::ManifestConflict {
                plugin_id: plugin_id.to_string(),
                path: "lockfile catalog".to_string(),
                reason: format!(
                    "manifest name '{manifest}' differs from catalog package name '{catalog}'"
                ),
            });
        }
    }
    if let Some(name) = manifest.or(catalog) {
        return Ok(Some(name));
    }
    Ok(bare_skill_source_name(payload_dir))
}

/// Canonical name of a provider: its authoritative name, else the storage
/// plugin id unchanged.
fn read_plugin_name(
    plugin_id: &str,
    payload_dir: &Path,
    catalog_name: Option<&str>,
) -> Result<String, ProviderRenderError> {
    Ok(
        authoritative_plugin_name(plugin_id, payload_dir, catalog_name)?
            .unwrap_or_else(|| plugin_id.to_string()),
    )
}

/// The first `name` declared by a manifest in [`PLUGIN_MANIFESTS`] order.
fn manifest_plugin_name(
    plugin_id: &str,
    payload_dir: &Path,
) -> Result<Option<String>, ProviderRenderError> {
    for manifest in PLUGIN_MANIFESTS {
        let path = RelativePath::new(manifest)?.join_to(payload_dir);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(ProviderRenderError::InvalidManifest {
                    plugin_id: plugin_id.to_string(),
                    path,
                    reason: e.to_string(),
                })
            }
        };
        let invalid = |reason: String| ProviderRenderError::InvalidManifest {
            plugin_id: plugin_id.to_string(),
            path: path.clone(),
            reason,
        };
        let value: Value = serde_json::from_str(&text).map_err(|e| invalid(e.to_string()))?;
        match value.get("name") {
            None => continue,
            Some(Value::String(name)) if !name.trim().is_empty() => {
                if name.chars().any(char::is_control) {
                    return Err(invalid("name contains control characters".to_string()));
                }
                return Ok(Some(name.clone()));
            }
            Some(_) => return Err(invalid("name must be a non-empty string".to_string())),
        }
    }
    Ok(None)
}

/// Front-matter `name` of a bare skill payload (a root `SKILL.md` with no
/// `skills/` or `commands/` tree), when it is usable and not generic.
fn bare_skill_source_name(payload_dir: &Path) -> Option<String> {
    if payload_dir.join("skills").is_dir() || payload_dir.join("commands").is_dir() {
        return None;
    }
    let bytes = fs::read(payload_dir.join("SKILL.md")).ok()?;
    frontmatter_string(&bytes, "name")
        .filter(|name| is_usable_name(name) && !is_generic_basename(name))
}

/// Whether a declared name is non-empty and free of control characters.
fn is_usable_name(name: &str) -> bool {
    !name.trim().is_empty() && !name.chars().any(char::is_control)
}

/// Whether a name is only a generic directory basename.
fn is_generic_basename(name: &str) -> bool {
    let name = name.trim();
    GENERIC_BASENAMES
        .iter()
        .any(|generic| generic.eq_ignore_ascii_case(name))
}

// ── generations ──────────────────────────────────────────────────────────────

/// Outcome of rendering one provider generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedProviderGeneration {
    /// Original plugin id from the lockfile.
    pub plugin_id: String,
    /// Original plugin name.
    pub plugin_name: String,
    /// Upstream revision recorded for the source, when known.
    pub upstream_revision: Option<String>,
    /// Published generation (source key, generation id, package digest).
    pub generation: ProviderGeneration,
    /// Absolute generation directory.
    pub path: PathBuf,
    /// `true` when an identical generation was already published.
    pub reused: bool,
    /// Components the generation exports, with original provenance.
    pub components: Vec<ComponentIdentity>,
    /// Component resources preserved with their logical component.
    pub component_resources: Vec<ComponentResources>,
    /// Whether any provider-scope hook declaration was present.
    pub hooks_declared: bool,
}

/// Files that belong to a component but are not its primary entrypoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentResources {
    /// Identity of the component requiring these files.
    pub component: ComponentIdentity,
    /// Package-relative resources owned by this component.
    pub resources: Vec<RelativePath>,
    /// Whether the provider explicitly disables this component.
    pub enabled: bool,
}

/// Payload snapshot: sorted `(relative path, bytes)` pairs.
type Snapshot = Vec<(String, Vec<u8>)>;

/// Extra check run on a staged generation before publication.
type StageCheck<'a> = &'a dyn Fn(&Path) -> Result<(), ProviderRenderError>;

/// Render every available provider into the default provider render root.
pub fn render_provider_generations(
    inventory: &ProviderInventory,
) -> Result<Vec<RenderedProviderGeneration>, ProviderRenderError> {
    let root =
        ccync_foundation::paths::provider_render_root().ok_or(ProviderRenderError::NoHome)?;
    render_provider_generations_at(inventory, &root)
}

/// Render every available provider under `providers_root`.
///
/// Each provider is published independently; on error, generations already
/// published by this call stay in place (they are immutable and unreferenced
/// until a caller switches to them).
pub fn render_provider_generations_at(
    inventory: &ProviderInventory,
    providers_root: &Path,
) -> Result<Vec<RenderedProviderGeneration>, ProviderRenderError> {
    // Inspect the complete batch before the first publication. Providers with
    // different source keys have independent immutable generations, while a
    // repeated claim for one source would target the same published path.
    let mut claims: BTreeMap<(ProviderSourceKey, ComponentKind, String), String> = BTreeMap::new();
    for entry in &inventory.providers {
        let files: PackageFiles = snapshot_payload(&entry.plugin_id, &entry.payload_dir)?
            .into_iter()
            .collect();
        let owner = ComponentOwner {
            plugin_id: &entry.plugin_id,
            source: &entry.key,
            plugin_name: &entry.plugin_name,
            upstream_revision: entry.upstream_revision.as_ref(),
        };
        for identity in enumerate_components(&owner, &files, HookScope::Provider)? {
            let claim = (
                entry.key.clone(),
                identity.kind,
                identity.component_id.clone(),
            );
            if let Some(first) = claims.insert(claim, entry.plugin_id.clone()) {
                return Err(ProviderRenderError::ManifestConflict {
                    plugin_id: entry.plugin_id.clone(),
                    path: "component inventory".to_string(),
                    reason: format!(
                        "component '{}' is claimed by both '{}' and this provider for source '{}'",
                        identity.component_id, first, entry.key
                    ),
                });
            }
        }
    }
    inventory
        .providers
        .iter()
        .map(|entry| render_provider_generation(entry, providers_root, &|_| Ok(())))
        .collect()
}

/// Render one provider generation, reusing an identical published one.
fn render_provider_generation(
    entry: &ProviderEntry,
    providers_root: &Path,
    stage_check: StageCheck<'_>,
) -> Result<RenderedProviderGeneration, ProviderRenderError> {
    // The payload is held once, keyed by path; publication and component
    // enumeration both read this one map.
    let files: PackageFiles = snapshot_payload(&entry.plugin_id, &entry.payload_dir)?
        .into_iter()
        .collect();
    if files.is_empty() {
        return Err(ProviderRenderError::EmptyPayload {
            plugin_id: entry.plugin_id.clone(),
            path: entry.payload_dir.clone(),
        });
    }
    let digest_hex = files_digest_hex(&files);
    let generation = ProviderGeneration {
        source: entry.key.clone(),
        generation: GenerationId::new(format!("g-{}", &digest_hex[..GENERATION_ID_HEX_LEN]))?,
        package_digest: PackageDigest::new(format!("sha256:{digest_hex}"))?,
    };

    // Enumerate before publication so an unusable hook declaration fails
    // without writing anything.
    let owner = ComponentOwner {
        plugin_id: &entry.plugin_id,
        source: &entry.key,
        plugin_name: &entry.plugin_name,
        upstream_revision: entry.upstream_revision.as_ref(),
    };
    let mut components = enumerate_components(&owner, &files, HookScope::Provider)?;
    let package_digest = generation.package_digest.clone();
    let resolved_path = fs::canonicalize(&entry.payload_dir).ok();
    for component in &mut components {
        component.origin = Some(ComponentOrigin {
            structure_version: 1,
            canonical_reference: Some(entry.source.clone()),
            resolved_path: resolved_path.clone(),
            revision: entry.upstream_revision.clone(),
            digest: Some(package_digest.clone()),
            enabled: Some(component_enabled(component, &files)),
        });
    }
    let component_resources = components
        .iter()
        .map(|component| ComponentResources {
            component: component.clone(),
            resources: component_resource_paths(component, &files),
            enabled: component_enabled(component, &files),
        })
        .collect();
    let hooks_declared = !hook_sources(&entry.plugin_id, &files, HookScope::Provider)?.is_empty();

    let provider_dir = providers_root.join(entry.key.as_str());
    let final_dir = provider_dir.join(generation.generation.as_str());

    if exists_no_follow(&provider_dir)? && is_link(&provider_dir)? {
        return Err(ProviderRenderError::LinkedPath { path: provider_dir });
    }
    let reused = if exists_no_follow(&final_dir)? {
        verify_published(&entry.plugin_id, &final_dir, &digest_hex)?;
        true
    } else {
        fs::create_dir_all(&provider_dir).map_err(io_error(&provider_dir))?;
        publish_generation(
            &entry.plugin_id,
            &files,
            &digest_hex,
            &provider_dir,
            &final_dir,
            stage_check,
        )?
    };
    Ok(RenderedProviderGeneration {
        plugin_id: entry.plugin_id.clone(),
        plugin_name: entry.plugin_name.clone(),
        upstream_revision: entry.upstream_revision.clone(),
        generation,
        path: final_dir,
        reused,
        components,
        component_resources,
        hooks_declared,
    })
}

/// Stage, validate, and publish a new immutable package directory (a
/// generation or a native package view) inside `parent_dir`. Returns `true`
/// when a concurrent publisher already placed an identical package.
fn publish_generation(
    plugin_id: &str,
    files: &PackageFiles,
    digest_hex: &str,
    parent_dir: &Path,
    final_dir: &Path,
    stage_check: StageCheck<'_>,
) -> Result<bool, ProviderRenderError> {
    let stage = parent_dir.join(format!("{STAGE_PREFIX}{}", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        fs::create_dir(&stage).map_err(io_error(&stage))?;
        write_package_files(&stage, files)?;
        stage_check(&stage)?;
        let staged = snapshot_dir(plugin_id, &stage)?;
        if package_digest_hex(&staged) != digest_hex {
            return Err(ProviderRenderError::Validation {
                plugin_id: plugin_id.to_string(),
                message: "staged content differs from the source payload".to_string(),
            });
        }
        match fs::rename(&stage, final_dir) {
            Ok(()) => Ok(false),
            Err(source) => {
                // Another publisher may have placed the same package
                // between the existence check and this rename.
                if exists_no_follow(final_dir)? {
                    verify_published(plugin_id, final_dir, digest_hex)?;
                    Ok(true)
                } else {
                    Err(ProviderRenderError::Io {
                        path: final_dir.to_path_buf(),
                        source,
                    })
                }
            }
        }
    })();
    if exists_no_follow(&stage).unwrap_or(false) {
        let _ = fs::remove_dir_all(&stage);
    }
    result
}

/// Check that a published generation still matches its digest.
fn verify_published(
    plugin_id: &str,
    dir: &Path,
    digest_hex: &str,
) -> Result<(), ProviderRenderError> {
    if is_link(dir)? {
        return Err(ProviderRenderError::LinkedPath {
            path: dir.to_path_buf(),
        });
    }
    let published = snapshot_dir(plugin_id, dir)?;
    if package_digest_hex(&published) == digest_hex {
        Ok(())
    } else {
        Err(ProviderRenderError::GenerationMismatch {
            path: dir.to_path_buf(),
            expected: format!("sha256:{digest_hex}"),
        })
    }
}

/// Write a snapshot into an empty directory.
fn write_package_files(root: &Path, files: &PackageFiles) -> Result<(), ProviderRenderError> {
    for (rel, bytes) in files {
        let path = RelativePath::new(rel)?.join_to(root);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(io_error(parent))?;
        }
        fs::write(&path, bytes).map_err(io_error(&path))?;
    }
    Ok(())
}

/// Upper bound on the bytes of one provider payload. A render holds the
/// payload in memory once, so a larger tree fails closed before any read.
const MAX_PROVIDER_PAYLOAD_BYTES: u64 = 256 * 1024 * 1024;

/// Sum regular-file sizes under `dir` from metadata only, without following
/// links, and stop as soon as the running total exceeds `limit`.
fn payload_size_exceeds(dir: &Path, skip_top_level: bool, limit: u64) -> std::io::Result<bool> {
    let mut total: u64 = 0;
    let mut pending = vec![(dir.to_path_buf(), true)];
    while let Some((current, top_level)) = pending.pop() {
        for entry in fs::read_dir(&current)? {
            let entry = entry?;
            if skip_top_level && top_level {
                if let Some(name) = entry.file_name().to_str() {
                    if EXCLUDED_TOP_LEVEL.contains(&name) {
                        continue;
                    }
                }
            }
            let meta = fs::symlink_metadata(entry.path())?;
            if is_link_like(&meta) {
                continue;
            }
            if meta.is_dir() {
                pending.push((entry.path(), false));
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
                if total > limit {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

/// Snapshot a provider payload, skipping top-level version-control metadata.
/// Links and non-regular entries fail closed, and so does a payload larger
/// than [`MAX_PROVIDER_PAYLOAD_BYTES`].
fn snapshot_payload(plugin_id: &str, payload_dir: &Path) -> Result<Snapshot, ProviderRenderError> {
    let payload_error = |message: String| ProviderRenderError::Payload {
        plugin_id: plugin_id.to_string(),
        message,
    };
    if payload_size_exceeds(payload_dir, true, MAX_PROVIDER_PAYLOAD_BYTES)
        .map_err(io_error(payload_dir))?
    {
        return Err(payload_error(format!(
            "payload exceeds the {MAX_PROVIDER_PAYLOAD_BYTES}-byte provider limit"
        )));
    }
    let mut snapshot = Vec::new();
    for entry in fs::read_dir(payload_dir).map_err(io_error(payload_dir))? {
        let entry = entry.map_err(io_error(payload_dir))?;
        let path = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(payload_error(format!(
                "non-UTF-8 entry name {}",
                path.display()
            )));
        };
        if EXCLUDED_TOP_LEVEL.contains(&name) {
            continue;
        }
        let meta = fs::symlink_metadata(&path).map_err(io_error(&path))?;
        if is_link_like(&meta) {
            return Err(payload_error(format!(
                "link or reparse point {} is not copied",
                path.display()
            )));
        }
        if meta.is_dir() {
            let nested = collect_dir_snapshot(&path).map_err(payload_error)?;
            snapshot.extend(
                nested
                    .into_iter()
                    .map(|(rel, bytes)| (format!("{name}/{rel}"), bytes)),
            );
        } else if meta.is_file() {
            let bytes = fs::read(&path).map_err(io_error(&path))?;
            snapshot.push((name.to_string(), bytes));
        } else {
            return Err(payload_error(format!(
                "non-regular entry {}",
                path.display()
            )));
        }
    }
    snapshot.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(snapshot)
}

/// Snapshot a whole generation directory (nothing is excluded).
fn snapshot_dir(plugin_id: &str, dir: &Path) -> Result<Snapshot, ProviderRenderError> {
    collect_dir_snapshot(dir).map_err(|message| ProviderRenderError::Payload {
        plugin_id: plugin_id.to_string(),
        message,
    })
}

/// Full SHA-256 hex over a sorted snapshot. Fields are length-prefixed so
/// distinct trees never share an encoding.
fn package_digest_hex(snapshot: &Snapshot) -> String {
    digest_entries(
        snapshot
            .iter()
            .map(|(rel, bytes)| (rel.as_str(), bytes.as_slice())),
    )
}

/// [`package_digest_hex`] over a path-keyed package map (same encoding, since
/// the map iterates in the same sorted path order).
fn files_digest_hex(files: &PackageFiles) -> String {
    digest_entries(
        files
            .iter()
            .map(|(rel, bytes)| (rel.as_str(), bytes.as_slice())),
    )
}

fn digest_entries<'a>(entries: impl Iterator<Item = (&'a str, &'a [u8])>) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(DIGEST_DOMAIN);
    for (rel, bytes) in entries {
        hasher.update((rel.len() as u64).to_le_bytes());
        hasher.update(rel.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    hex::encode(hasher.finalize())
}

/// Whether `path` exists, without following links.
fn exists_no_follow(path: &Path) -> Result<bool, ProviderRenderError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(ProviderRenderError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Whether an existing `path` is a link or reparse point.
fn is_link(path: &Path) -> Result<bool, ProviderRenderError> {
    let meta = fs::symlink_metadata(path).map_err(io_error(path))?;
    Ok(is_link_like(&meta))
}

// ── component enumeration ────────────────────────────────────────────────────

/// In-memory package tree: normalized relative path to file bytes.
type PackageFiles = BTreeMap<String, Vec<u8>>;

/// Root portable plugin manifest.
const ROOT_MANIFEST: &str = "plugin.json";

/// Claude Code plugin manifest.
const CLAUDE_MANIFEST: &str = ".claude-plugin/plugin.json";

/// Codex plugin manifest (portable overlay or legacy manifest).
const CODEX_MANIFEST: &str = ".codex-plugin/plugin.json";

/// Hook configuration discovered by default when no manifest overrides it.
const DEFAULT_HOOKS: &str = "hooks/hooks.json";

/// Root MCP files the centralized host MCP projection reads from a plugin
/// payload. They are the default native MCP entrypoints of a plugin root.
const ROOT_MCP_FILES: &[&str] = &[".mcp.json", "mcp.json"];

/// Hook command placeholder for the plugin root.
const PLUGIN_ROOT_PLACEHOLDER: &str = "${CLAUDE_PLUGIN_ROOT}/";

/// Provenance shared by every component of one provider.
struct ComponentOwner<'a> {
    plugin_id: &'a str,
    source: &'a ProviderSourceKey,
    plugin_name: &'a str,
    upstream_revision: Option<&'a String>,
}

impl ComponentOwner<'_> {
    fn identity(
        &self,
        kind: ComponentKind,
        id: &str,
    ) -> Result<ComponentIdentity, ProviderRenderError> {
        ComponentIdentity::new(
            self.source.clone(),
            self.plugin_name,
            self.upstream_revision.cloned(),
            kind,
            id,
        )
        .map_err(ProviderRenderError::from)
    }
}

/// Which hook declarations count as loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookScope {
    /// Every hook declaration the provider ships, for any loader.
    Provider,
    /// Only the declarations the given native loader honors.
    Target(NativePackageTarget),
}

/// Name of a bare-skill payload (a root `SKILL.md` with no `skills/` or
/// `commands/` tree): the front-matter `name`, else the plugin name.
fn bare_skill_name(files: &PackageFiles, plugin_name: &str) -> Option<String> {
    let skill = files.get("SKILL.md")?;
    if files
        .keys()
        .any(|path| path.starts_with("skills/") || path.starts_with("commands/"))
    {
        return None;
    }
    Some(
        frontmatter_string(skill, "name")
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| plugin_name.to_string()),
    )
}

/// Skill ids: directories directly under `skills/` that hold a `SKILL.md`.
fn skill_ids(files: &PackageFiles) -> BTreeSet<String> {
    files
        .keys()
        .filter_map(
            |path| match path.split('/').collect::<Vec<_>>().as_slice() {
                ["skills", id, "SKILL.md"] if !id.starts_with('.') => Some((*id).to_string()),
                _ => None,
            },
        )
        .collect()
}

/// Command forms present for one command id.
#[derive(Debug, Default, Clone, Copy)]
struct CommandForms {
    /// `commands/<id>.md`.
    flat: bool,
    /// `commands/<id>/SKILL.md`.
    directory: bool,
}

/// Command ids: `.md` files directly under `commands/` and directories
/// there that hold a `SKILL.md`. Dot entries are not commands.
fn command_forms(files: &PackageFiles) -> BTreeMap<String, CommandForms> {
    let mut forms: BTreeMap<String, CommandForms> = BTreeMap::new();
    for path in files.keys() {
        match path.split('/').collect::<Vec<_>>().as_slice() {
            ["commands", file] if !file.starts_with('.') => {
                if let Some(stem) = file.strip_suffix(".md").filter(|stem| !stem.is_empty()) {
                    forms.entry(stem.to_string()).or_default().flat = true;
                }
            }
            ["commands", id, "SKILL.md"] if !id.starts_with('.') => {
                forms.entry((*id).to_string()).or_default().directory = true;
            }
            _ => {}
        }
    }
    forms
}

/// Agent ids: `.md` files directly under `agents/`.
fn agent_ids(files: &PackageFiles) -> BTreeSet<String> {
    files
        .keys()
        .filter_map(
            |path| match path.split('/').collect::<Vec<_>>().as_slice() {
                ["agents", file] if !file.starts_with('.') => file
                    .strip_suffix(".md")
                    .filter(|stem| !stem.is_empty())
                    .map(str::to_string),
                _ => None,
            },
        )
        .collect()
}

/// Enumerate the logical components a package exports.
///
/// A skill is a `skills/<id>/` directory holding `SKILL.md`; a bare-skill
/// payload exports exactly one skill. Commands are `commands/<id>.md` files or
/// `commands/<id>/SKILL.md` directories, agents are `agents/<id>.md` files,
/// and dot entries are never components. Hook identity is the event name
/// declared by each hook configuration in scope, not a file name. Everything
/// else is carried as resources and not enumerated.
fn enumerate_components(
    owner: &ComponentOwner<'_>,
    files: &PackageFiles,
    scope: HookScope,
) -> Result<Vec<ComponentIdentity>, ProviderRenderError> {
    let mut found: BTreeSet<(ComponentKind, String)> = BTreeSet::new();
    if let Some(name) = bare_skill_name(files, owner.plugin_name) {
        found.insert((ComponentKind::Skill, name));
    } else {
        for id in skill_ids(files) {
            found.insert((ComponentKind::Skill, id));
        }
        for id in command_forms(files).into_keys() {
            found.insert((ComponentKind::Command, id));
        }
        for id in agent_ids(files) {
            found.insert((ComponentKind::Agent, id));
        }
    }
    for event in hook_events(owner.plugin_id, files, scope)? {
        found.insert((ComponentKind::Hook, event));
    }
    found
        .into_iter()
        .map(|(kind, id)| owner.identity(kind, &id))
        .collect()
}

fn component_resource_paths(
    component: &ComponentIdentity,
    files: &PackageFiles,
) -> Vec<RelativePath> {
    let prefix = match component.kind {
        ComponentKind::Skill => format!("skills/{}/", component.component_id),
        ComponentKind::Command => format!("commands/{}/", component.component_id),
        ComponentKind::Agent => format!("agents/{}/", component.component_id),
        ComponentKind::Hook | ComponentKind::Resource => return Vec::new(),
    };
    files
        .keys()
        .filter(|path| path.starts_with(&prefix))
        .filter(|path| !path.ends_with("/SKILL.md"))
        .filter_map(|path| RelativePath::new(path).ok())
        .collect()
}

fn component_enabled(component: &ComponentIdentity, files: &PackageFiles) -> bool {
    let sidecar = match component.kind {
        ComponentKind::Skill => format!("skills/{}/SKILL.md", component.component_id),
        ComponentKind::Command => format!("commands/{}/SKILL.md", component.component_id),
        _ => return true,
    };
    let Some(bytes) = files.get(&sidecar) else {
        return true;
    };
    let Some(text) = std::str::from_utf8(bytes).ok() else {
        return true;
    };
    let Some((yaml, _, _)) = split_frontmatter(text) else {
        return true;
    };
    let Ok(value) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(yaml) else {
        return true;
    };
    let mapping = value.as_mapping();
    let enabled = mapping
        .and_then(|m| m.get(serde_yaml_ng::Value::String("enabled".to_string())))
        .and_then(serde_yaml_ng::Value::as_bool);
    let disabled = mapping
        .and_then(|m| m.get(serde_yaml_ng::Value::String("disabled".to_string())))
        .and_then(serde_yaml_ng::Value::as_bool)
        .unwrap_or(false);
    enabled.unwrap_or(true) && !disabled
}

// ── manifests and hooks ──────────────────────────────────────────────────────

/// Parse a JSON manifest from the package, if present. It must be an object.
fn parse_manifest(
    plugin_id: &str,
    files: &PackageFiles,
    path: &str,
) -> Result<Option<serde_json::Map<String, Value>>, ProviderRenderError> {
    let Some(bytes) = files.get(path) else {
        return Ok(None);
    };
    let invalid = |reason: String| ProviderRenderError::InvalidManifest {
        plugin_id: plugin_id.to_string(),
        path: PathBuf::from(path),
        reason,
    };
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(map)) => Ok(Some(map)),
        Ok(_) => Err(invalid("manifest must be a JSON object".to_string())),
        Err(e) => Err(invalid(e.to_string())),
    }
}

/// The `extensions["com.openai"]` settings object of a root manifest, when
/// declared. A declared value that is not an object is an error.
fn openai_extension<'a>(
    plugin_id: &str,
    root: &'a serde_json::Map<String, Value>,
) -> Result<Option<&'a serde_json::Map<String, Value>>, ProviderRenderError> {
    let Some(extensions) = root.get("extensions") else {
        return Ok(None);
    };
    let invalid = |reason: &str| ProviderRenderError::InvalidManifest {
        plugin_id: plugin_id.to_string(),
        path: PathBuf::from(ROOT_MANIFEST),
        reason: reason.to_string(),
    };
    let extensions = extensions
        .as_object()
        .ok_or_else(|| invalid("extensions must be an object"))?;
    match extensions.get("com.openai") {
        None => Ok(None),
        Some(Value::Object(settings)) => Ok(Some(settings)),
        Some(_) => Err(invalid("extensions[\"com.openai\"] must be an object")),
    }
}

/// One hook configuration in scope, with where it was declared.
struct HookSource {
    origin: String,
    config: Value,
}

/// Collect the hook configurations a loader honors.
///
/// - Provider scope: every declaration (default `hooks/hooks.json`, Claude
///   and Codex manifest `hooks`, root `hooks`, and root
///   `extensions["com.openai"].hooks`).
/// - Claude: default `hooks/hooks.json` plus the Claude manifest `hooks`.
/// - Codex: root `extensions["com.openai"].hooks` when that settings object
///   exists, else the `.codex-plugin` manifest `hooks`; an explicit value
///   replaces default `hooks/hooks.json` discovery.
fn hook_sources(
    plugin_id: &str,
    files: &PackageFiles,
    scope: HookScope,
) -> Result<Vec<HookSource>, ProviderRenderError> {
    let root = parse_manifest(plugin_id, files, ROOT_MANIFEST)?;
    let claude = parse_manifest(plugin_id, files, CLAUDE_MANIFEST)?;
    let codex = parse_manifest(plugin_id, files, CODEX_MANIFEST)?;
    let openai = match &root {
        Some(root) => openai_extension(plugin_id, root)?,
        None => None,
    };

    let mut declarations: Vec<(String, &Value)> = Vec::new();
    let mut use_default = true;
    match scope {
        HookScope::Provider => {
            for (origin, manifest) in [
                (CLAUDE_MANIFEST, claude.as_ref()),
                (CODEX_MANIFEST, codex.as_ref()),
                (ROOT_MANIFEST, root.as_ref()),
            ] {
                if let Some(hooks) = manifest.and_then(|m| m.get("hooks")) {
                    declarations.push((format!("{origin}#hooks"), hooks));
                }
            }
            if let Some(hooks) = openai.and_then(|s| s.get("hooks")) {
                declarations.push((
                    format!("{ROOT_MANIFEST}#extensions.com.openai.hooks"),
                    hooks,
                ));
            }
        }
        HookScope::Target(NativePackageTarget::Claude) => {
            if let Some(hooks) = claude.as_ref().and_then(|m| m.get("hooks")) {
                declarations.push((format!("{CLAUDE_MANIFEST}#hooks"), hooks));
            }
        }
        HookScope::Target(NativePackageTarget::Codex) => {
            let explicit = match openai {
                Some(settings) => settings.get("hooks").map(|hooks| {
                    (
                        format!("{ROOT_MANIFEST}#extensions.com.openai.hooks"),
                        hooks,
                    )
                }),
                None => codex
                    .as_ref()
                    .and_then(|m| m.get("hooks"))
                    .map(|hooks| (format!("{CODEX_MANIFEST}#hooks"), hooks)),
            };
            if let Some(declaration) = explicit {
                use_default = false;
                declarations.push(declaration);
            }
        }
    }

    let mut sources = Vec::new();
    if use_default && files.contains_key(DEFAULT_HOOKS) {
        sources.push(read_hook_file(
            plugin_id,
            files,
            DEFAULT_HOOKS,
            DEFAULT_HOOKS,
        )?);
    }
    for (origin, declaration) in declarations {
        expand_hook_declaration(plugin_id, files, &origin, declaration, &mut sources)?;
    }
    Ok(sources)
}

/// Expand one manifest `hooks` value: a path, an inline configuration, or an
/// array of either.
fn expand_hook_declaration(
    plugin_id: &str,
    files: &PackageFiles,
    origin: &str,
    declaration: &Value,
    sources: &mut Vec<HookSource>,
) -> Result<(), ProviderRenderError> {
    match declaration {
        Value::String(path) => {
            let relative = path.strip_prefix("./").unwrap_or(path);
            let checked = RelativePath::new(relative).map_err(|e| {
                hook_error(
                    plugin_id,
                    origin,
                    format!("hook path '{path}' is invalid: {e}"),
                )
            })?;
            sources.push(read_hook_file(plugin_id, files, checked.as_str(), origin)?);
        }
        Value::Object(_) => sources.push(HookSource {
            origin: origin.to_string(),
            config: declaration.clone(),
        }),
        Value::Array(items) => {
            for item in items {
                if item.is_array() {
                    return Err(hook_error(
                        plugin_id,
                        origin,
                        "nested hook arrays are not supported".to_string(),
                    ));
                }
                expand_hook_declaration(plugin_id, files, origin, item, sources)?;
            }
        }
        _ => {
            return Err(hook_error(
                plugin_id,
                origin,
                "hooks must be a path, an object, or an array of those".to_string(),
            ))
        }
    }
    Ok(())
}

fn read_hook_file(
    plugin_id: &str,
    files: &PackageFiles,
    path: &str,
    origin: &str,
) -> Result<HookSource, ProviderRenderError> {
    let bytes = files.get(path).ok_or_else(|| {
        hook_error(
            plugin_id,
            origin,
            format!("hook file '{path}' is not in the package"),
        )
    })?;
    let config: Value = serde_json::from_slice(bytes).map_err(|e| {
        hook_error(
            plugin_id,
            origin,
            format!("hook file '{path}' is not valid JSON: {e}"),
        )
    })?;
    Ok(HookSource {
        origin: path.to_string(),
        config,
    })
}

fn hook_error(plugin_id: &str, origin: &str, reason: String) -> ProviderRenderError {
    ProviderRenderError::InvalidHookDeclaration {
        plugin_id: plugin_id.to_string(),
        origin: origin.to_string(),
        reason,
    }
}

/// Event names declared by one hook configuration. Accepts both the
/// `{"hooks": {<event>: [...]}}` file form and a bare `{<event>: [...]}` map.
fn source_events(plugin_id: &str, source: &HookSource) -> Result<Vec<String>, ProviderRenderError> {
    let events = match source.config.get("hooks") {
        Some(Value::Object(events)) => events,
        Some(_) => {
            return Err(hook_error(
                plugin_id,
                &source.origin,
                "hooks must map event names to hook lists".to_string(),
            ))
        }
        None => source.config.as_object().ok_or_else(|| {
            hook_error(
                plugin_id,
                &source.origin,
                "hook configuration must be an object".to_string(),
            )
        })?,
    };
    events
        .iter()
        .map(|(event, entries)| {
            if event.trim().is_empty() || !entries.is_array() {
                Err(hook_error(
                    plugin_id,
                    &source.origin,
                    format!("event '{event}' must be a non-empty name with a hook list"),
                ))
            } else {
                Ok(event.clone())
            }
        })
        .collect()
}

/// Sorted, de-duplicated hook event names in scope.
fn hook_events(
    plugin_id: &str,
    files: &PackageFiles,
    scope: HookScope,
) -> Result<BTreeSet<String>, ProviderRenderError> {
    let mut events = BTreeSet::new();
    for source in hook_sources(plugin_id, files, scope)? {
        events.extend(source_events(plugin_id, &source)?);
    }
    Ok(events)
}

/// Helper paths referenced as `${CLAUDE_PLUGIN_ROOT}/<path>` by the hooks in
/// scope, each with the declaration it came from.
fn hook_helper_refs(
    plugin_id: &str,
    files: &PackageFiles,
    scope: HookScope,
) -> Result<Vec<(String, String)>, ProviderRenderError> {
    let mut refs = Vec::new();
    for source in hook_sources(plugin_id, files, scope)? {
        let text = source.config.to_string();
        let mut rest = text.as_str();
        while let Some(start) = rest.find(PLUGIN_ROOT_PLACEHOLDER) {
            let after = &rest[start + PLUGIN_ROOT_PLACEHOLDER.len()..];
            let end = after
                .find(|c: char| !(c.is_ascii_alphanumeric() || "._-/".contains(c)))
                .unwrap_or(after.len());
            let helper = after[..end].trim_end_matches('/');
            rest = &after[end..];
            if !helper.is_empty() {
                refs.push((source.origin.clone(), helper.to_string()));
            }
        }
    }
    Ok(refs)
}

/// Whether `path` is `helper` itself or a file below the `helper` directory.
fn is_helper_path(path: &str, helper: &str) -> bool {
    path == helper
        || path
            .strip_prefix(helper)
            .is_some_and(|tail| tail.starts_with('/'))
}

/// Check that every `${CLAUDE_PLUGIN_ROOT}/<path>` helper referenced by a
/// hook in scope exists in the package.
fn check_hook_helpers(
    plugin_id: &str,
    files: &PackageFiles,
    scope: HookScope,
) -> Result<(), ProviderRenderError> {
    for (origin, helper) in hook_helper_refs(plugin_id, files, scope)? {
        let present = RelativePath::new(&helper).is_ok()
            && files.keys().any(|path| is_helper_path(path, &helper));
        if !present {
            return Err(hook_error(
                plugin_id,
                &origin,
                format!("hook helper '{helper}' is not in the package"),
            ));
        }
    }
    Ok(())
}

/// Move a bare skill's own content under `skills/<name>/` so a native loader
/// discovers it, keeping plugin-root files where the loader expects them:
/// manifests, hook configurations, and root MCP files stay at the root. A
/// hook helper is kept at the root for the hook and also copied into the
/// skill, so both the hook command and the skill's relative links resolve.
fn relocate_bare_skill(
    plugin_id: &str,
    name: &str,
    files: PackageFiles,
) -> Result<PackageFiles, ProviderRenderError> {
    let hook_files: BTreeSet<String> = hook_sources(plugin_id, &files, HookScope::Provider)?
        .into_iter()
        .map(|source| source.origin)
        .filter(|origin| files.contains_key(origin))
        .collect();
    let helpers: Vec<String> = hook_helper_refs(plugin_id, &files, HookScope::Provider)?
        .into_iter()
        .map(|(_, helper)| helper)
        .collect();
    let mut relocated = PackageFiles::new();
    for (path, bytes) in files {
        let root_only = path == ROOT_MANIFEST
            || path.starts_with(".claude-plugin/")
            || path.starts_with(".codex-plugin/")
            || path.starts_with("hooks/")
            || ROOT_MCP_FILES.contains(&path.as_str())
            || hook_files.contains(&path);
        if root_only {
            relocated.insert(path, bytes);
            continue;
        }
        let skill_path = format!("skills/{name}/{path}");
        if helpers.iter().any(|helper| is_helper_path(&path, helper)) {
            relocated.insert(skill_path, bytes.clone());
            relocated.insert(path, bytes);
        } else {
            relocated.insert(skill_path, bytes);
        }
    }
    Ok(relocated)
}

// ── front matter ─────────────────────────────────────────────────────────────

/// Split a Markdown document into its YAML front matter, the byte offset just
/// after the opening fence line, and the newline style. `None` when the
/// document has no closed front matter.
fn split_frontmatter(text: &str) -> Option<(&str, usize, &'static str)> {
    let (opening, newline) = if text.starts_with("---\r\n") {
        (5, "\r\n")
    } else if text.starts_with("---\n") {
        (4, "\n")
    } else {
        return None;
    };
    let mut offset = opening;
    for line in text[opening..].split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Some((&text[opening..offset], opening, newline));
        }
        offset += line.len();
    }
    None
}

/// Read one string field from a Markdown front matter block.
fn frontmatter_string(bytes: &[u8], field: &str) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let (yaml, _, _) = split_frontmatter(text)?;
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml).ok()?;
    value.get(field)?.as_str().map(str::to_string)
}

/// Concise purpose from a `SKILL.md` description: its first sentence with
/// whitespace collapsed.
fn skill_purpose(bytes: &[u8]) -> Option<String> {
    let description = frontmatter_string(bytes, "description")?;
    let collapsed = description.split_whitespace().collect::<Vec<_>>().join(" ");
    let first = match collapsed.find(". ") {
        Some(end) => &collapsed[..end],
        None => collapsed.trim_end_matches('.'),
    };
    let first = first.trim();
    (!first.is_empty()).then(|| first.to_string())
}

/// Turn a command document into a skill document: keep the body and every
/// front-matter field, adding only a missing `name` or `description`.
fn command_as_skill(
    target: NativePackageTarget,
    plugin_id: &str,
    plugin_name: &str,
    id: &str,
    bytes: &[u8],
) -> Result<Vec<u8>, ProviderRenderError> {
    let blocked = |reason: String| ProviderRenderError::UnsupportedNativeDeclaration {
        target,
        plugin_id: plugin_id.to_string(),
        reason,
    };
    let text = std::str::from_utf8(bytes)
        .map_err(|_| blocked(format!("command '{id}' is not UTF-8 text")))?;
    let quote = |value: &str| serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string());
    let description = format!("{plugin_name} command {id}");
    let converted = match split_frontmatter(text) {
        Some((yaml, opening, newline)) => {
            let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml).map_err(|e| {
                blocked(format!(
                    "command '{id}' front matter is not valid YAML: {e}"
                ))
            })?;
            if !(value.is_null() || value.is_mapping()) {
                return Err(blocked(format!(
                    "command '{id}' front matter must be a mapping"
                )));
            }
            let mut added = String::new();
            if value.get("name").is_none() {
                added.push_str(&format!("name: {}{newline}", quote(id)));
            }
            if value.get("description").is_none() {
                added.push_str(&format!("description: {}{newline}", quote(&description)));
            }
            format!("{}{added}{}", &text[..opening], &text[opening..])
        }
        None => format!(
            "---\nname: {}\ndescription: {}\n---\n{text}",
            quote(id),
            quote(&description)
        ),
    };
    Ok(converted.into_bytes())
}

// ── YAML sidecars ────────────────────────────────────────────────────────────

/// Management marker shown next to the original provider identity.
pub const MANAGEMENT_MARKER: &str = "managed by ccync";

/// Suffix carrying the management marker in a short description.
const MANAGEMENT_SUFFIX: &str = " (managed by ccync)";

/// Upper bound for a Codex skill `interface.short_description`, in
/// characters. The official submission rules require a non-empty string but
/// publish no length limit; Codex's skill-creator guidance keeps the field
/// within 25-64 characters, so the conservative upper bound is used.
pub const SKILL_SHORT_DESCRIPTION_MAX_CHARS: usize = 64;

/// Maximum native plugin name and version length (Codex submission rules).
const NATIVE_FIELD_MAX_CHARS: usize = 64;

/// Reject YAML features the semantic sidecar update cannot carry faithfully:
/// directives, anchors, aliases, and explicit tags. The scan is lexical and
/// conservative: it honors quotes, comments, and block scalars, and may
/// reject an unusual but valid plain scalar rather than accept an alias.
fn reject_unsupported_yaml_syntax(text: &str) -> Result<(), String> {
    let mut quote: Option<char> = None;
    let mut block_parent: Option<usize> = None;
    for (index, line) in text.lines().enumerate() {
        let line_no = index + 1;
        let indent = line.len() - line.trim_start_matches(' ').len();
        if quote.is_none() {
            if let Some(parent) = block_parent {
                if line.trim().is_empty() || indent > parent {
                    continue;
                }
                block_parent = None;
            }
            if line.starts_with('%') {
                return Err(format!("line {line_no}: YAML directives are not supported"));
            }
        }
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        let mut node_start = quote.is_none();
        let mut after_blank = true;
        if quote.is_none() && (line == "---" || line.starts_with("--- ")) {
            i = 3;
        }
        while i < chars.len() {
            let c = chars[i];
            if let Some(open) = quote {
                if open == '\'' && c == '\'' {
                    if chars.get(i + 1) == Some(&'\'') {
                        i += 2;
                        continue;
                    }
                    quote = None;
                } else if open == '"' && c == '\\' {
                    i += 2;
                    continue;
                } else if open == '"' && c == '"' {
                    quote = None;
                }
                after_blank = false;
                i += 1;
                continue;
            }
            if c == ' ' || c == '\t' {
                after_blank = true;
                i += 1;
                continue;
            }
            if c == '#' && after_blank {
                break;
            }
            let next_blank = chars.get(i + 1).is_none_or(|n| *n == ' ' || *n == '\t');
            if node_start {
                match c {
                    '&' => return Err(format!("line {line_no}: YAML anchors are not supported")),
                    '*' => return Err(format!("line {line_no}: YAML aliases are not supported")),
                    '!' => {
                        return Err(format!(
                            "line {line_no}: explicit YAML tags are not supported"
                        ))
                    }
                    '\'' | '"' => {
                        quote = Some(c);
                        node_start = false;
                    }
                    '|' | '>' => {
                        block_parent = Some(indent);
                        break;
                    }
                    '-' | '?' | ':' if next_blank => {}
                    '[' | '{' | ',' => {}
                    _ => node_start = false,
                }
            } else if (c == ':' && next_blank) || matches!(c, ',' | '[' | '{') {
                node_start = true;
            }
            after_blank = false;
            i += 1;
        }
    }
    Ok(())
}

/// Reject explicit tags and merge keys that survived parsing.
fn reject_unsupported_yaml_value(value: &serde_yaml_ng::Value) -> Result<(), String> {
    use serde_yaml_ng::Value as Yaml;
    match value {
        Yaml::Tagged(_) => Err("explicit YAML tags are not supported".to_string()),
        Yaml::Mapping(mapping) => {
            for (key, item) in mapping {
                if key.as_str() == Some("<<") {
                    return Err("YAML merge keys (<<) are not supported".to_string());
                }
                reject_unsupported_yaml_value(key)?;
                reject_unsupported_yaml_value(item)?;
            }
            Ok(())
        }
        Yaml::Sequence(items) => items.iter().try_for_each(reject_unsupported_yaml_value),
        _ => Ok(()),
    }
}

/// Split `text` at every ASCII-case-insensitive occurrence of `needle`,
/// dropping the occurrences.
fn split_ascii_ci<'a>(text: &'a str, needle: &str) -> Vec<&'a str> {
    let lower = text.to_ascii_lowercase();
    let needle = needle.to_ascii_lowercase();
    let mut pieces = Vec::new();
    let mut cursor = 0;
    while let Some(found) = lower[cursor..].find(&needle) {
        pieces.push(&text[cursor..cursor + found]);
        cursor += found + needle.len();
    }
    pieces.push(&text[cursor..]);
    pieces
}

/// Separator characters dropped only next to a removed management marker.
fn is_marker_separator(c: char) -> bool {
    c.is_whitespace() || ",;:-".contains(c)
}

/// Strip any management marker from a description, leaving the purpose.
///
/// Separators are trimmed only at the edges where a marker was removed, so
/// upstream punctuation such as a closing parenthesis survives.
fn strip_management_marker(text: &str) -> String {
    // Every boundary between consecutive pieces is a removed marker.
    let pieces: Vec<&str> = split_ascii_ci(text, MANAGEMENT_SUFFIX.trim())
        .into_iter()
        .flat_map(|piece| split_ascii_ci(piece, MANAGEMENT_MARKER))
        .collect();
    let last = pieces.len() - 1;
    let kept: Vec<&str> = pieces
        .iter()
        .enumerate()
        .map(|(index, piece)| {
            let piece = if index > 0 {
                piece.trim_start_matches(is_marker_separator)
            } else {
                piece
            };
            if index < last {
                piece.trim_end_matches(is_marker_separator)
            } else {
                piece
            }
        })
        .collect();
    kept.join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Compose `<purpose> (managed by ccync)`, keeping the marker exactly once
/// and the whole text within `limit` characters when one is given.
fn with_management_marker(purpose: &str, limit: Option<usize>) -> String {
    let purpose = strip_management_marker(purpose);
    if purpose.is_empty() {
        return MANAGEMENT_MARKER.to_string();
    }
    let Some(limit) = limit else {
        return format!("{purpose}{MANAGEMENT_SUFFIX}");
    };
    let budget = limit.saturating_sub(MANAGEMENT_SUFFIX.chars().count());
    let purpose = if purpose.chars().count() > budget {
        let kept: String = purpose.chars().take(budget.saturating_sub(3)).collect();
        format!("{}...", kept.trim_end())
    } else {
        purpose
    };
    format!("{purpose}{MANAGEMENT_SUFFIX}")
}

/// Update one Codex skill sidecar (`agents/openai.yaml`).
///
/// Fills a missing `interface.display_name` with `<plugin>:<skill>` and keeps
/// an upstream label unchanged. Sets `interface.short_description` to the
/// skill purpose plus the management
/// marker, exactly once and within [`SKILL_SHORT_DESCRIPTION_MAX_CHARS`].
/// Every other field (policies, dependencies, unknown keys) is carried
/// through a semantic value roundtrip; formatting and comments are not
/// preserved. Malformed YAML, directives, anchors, aliases, merge keys, and
/// explicit tags are rejected before anything is written.
fn update_skill_sidecar(
    plugin_id: &str,
    path: &str,
    existing: Option<&[u8]>,
    plugin_name: &str,
    skill_id: &str,
    fallback_purpose: Option<&str>,
) -> Result<Vec<u8>, ProviderRenderError> {
    use serde_yaml_ng::{Mapping, Value as Yaml};
    let invalid = |reason: String| ProviderRenderError::InvalidSidecar {
        plugin_id: plugin_id.to_string(),
        path: path.to_string(),
        reason,
    };
    let mut document = match existing {
        None => Yaml::Mapping(Mapping::new()),
        Some(bytes) => {
            let text =
                std::str::from_utf8(bytes).map_err(|_| invalid("not UTF-8 text".to_string()))?;
            reject_unsupported_yaml_syntax(text).map_err(invalid)?;
            let value: Yaml = serde_yaml_ng::from_str(text)
                .map_err(|e| invalid(format!("malformed YAML: {e}")))?;
            reject_unsupported_yaml_value(&value).map_err(invalid)?;
            match value {
                Yaml::Null => Yaml::Mapping(Mapping::new()),
                Yaml::Mapping(_) => value,
                _ => return Err(invalid("the document must be a mapping".to_string())),
            }
        }
    };
    let Yaml::Mapping(root) = &mut document else {
        return Err(invalid("the document must be a mapping".to_string()));
    };
    let interface = root
        .entry(Yaml::from("interface"))
        .or_insert_with(|| Yaml::Mapping(Mapping::new()));
    if interface.is_null() {
        *interface = Yaml::Mapping(Mapping::new());
    }
    let Yaml::Mapping(interface) = interface else {
        return Err(invalid("interface must be a mapping".to_string()));
    };
    let current = match interface.get("short_description") {
        None | Some(Yaml::Null) => None,
        Some(Yaml::String(text)) => Some(text.clone()),
        Some(_) => {
            return Err(invalid(
                "interface.short_description must be a string".to_string(),
            ))
        }
    };
    if !matches!(
        interface.get("display_name"),
        None | Some(Yaml::Null) | Some(Yaml::String(_))
    ) {
        return Err(invalid(
            "interface.display_name must be a string".to_string(),
        ));
    }
    let purpose = current
        .as_deref()
        .map(strip_management_marker)
        .filter(|purpose| !purpose.is_empty())
        .or_else(|| fallback_purpose.map(str::to_string))
        .unwrap_or_default();
    // An upstream label stays as the author wrote it; the provider-prefixed
    // name only fills a missing one.
    if matches!(interface.get("display_name"), None | Some(Yaml::Null)) {
        interface.insert(
            Yaml::from("display_name"),
            Yaml::from(format!("{plugin_name}:{skill_id}")),
        );
    }
    interface.insert(
        Yaml::from("short_description"),
        Yaml::from(with_management_marker(
            &purpose,
            Some(SKILL_SHORT_DESCRIPTION_MAX_CHARS),
        )),
    );
    serde_yaml_ng::to_string(&document)
        .map(String::into_bytes)
        .map_err(|e| invalid(format!("cannot serialize: {e}")))
}

// ── native package views ─────────────────────────────────────────────────────

/// Domain tag of the native package version suffix. Changing how views are
/// derived must change this tag so a changed view never reuses a version.
const VIEW_VERSION_DOMAIN: &[u8] = b"ccync-native-package-view\0v1\0";

/// Number of digest hex characters in the version suffix.
const VERSION_SUFFIX_HEX_LEN: usize = 12;

/// Suffix of the directory holding a generation's native package views.
const VIEWS_DIR_SUFFIX: &str = ".views";

/// Native loader a package view is generated for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NativePackageTarget {
    /// Codex native plugin.
    Codex,
    /// Claude Code native plugin.
    Claude,
}

impl NativePackageTarget {
    /// Directory and display name of the target.
    pub fn as_str(self) -> &'static str {
        self.projection_target().as_str()
    }

    /// The projection target this native loader belongs to.
    pub fn projection_target(self) -> ccync_foundation::target::ProjectionTargetId {
        use ccync_foundation::target::ProjectionTargetId;
        match self {
            NativePackageTarget::Codex => ProjectionTargetId::Codex,
            NativePackageTarget::Claude => ProjectionTargetId::Claude,
        }
    }

    /// Manifest the target loader reads first; always present in a view.
    fn primary_manifest(self) -> &'static str {
        match self {
            NativePackageTarget::Codex => CODEX_MANIFEST,
            NativePackageTarget::Claude => CLAUDE_MANIFEST,
        }
    }

    /// Manifest precedence for reading the upstream version. Codex reads the
    /// portable root `plugin.json` before its legacy `.codex-plugin` overlay.
    fn manifest_order(self) -> [&'static str; 3] {
        match self {
            NativePackageTarget::Codex => [ROOT_MANIFEST, CODEX_MANIFEST, CLAUDE_MANIFEST],
            NativePackageTarget::Claude => [CLAUDE_MANIFEST, ROOT_MANIFEST, CODEX_MANIFEST],
        }
    }
}

impl std::fmt::Display for NativePackageTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A published, immutable native package view of one provider generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePackageView {
    /// Native loader the view is built for.
    pub target: NativePackageTarget,
    /// Original plugin id from the lockfile.
    pub plugin_id: String,
    /// Original plugin name, published unchanged as the native plugin name.
    pub plugin_name: String,
    /// Upstream version declared by the provider manifests, when any.
    pub upstream_version: Option<String>,
    /// Upstream revision (pin) recorded for the source, when known.
    pub upstream_revision: Option<String>,
    /// Published native version: the upstream version (or `0.0.0`) with a
    /// deterministic `ccync.<hex>` build-metadata suffix.
    pub version: String,
    /// Generation the view was derived from.
    pub generation: ProviderGeneration,
    /// View directory name (`v-<hex>` of the view content digest).
    pub view_id: String,
    /// Digest of the view content.
    pub view_digest: PackageDigest,
    /// Absolute view directory.
    pub path: PathBuf,
    /// `true` when an identical view was already published.
    pub reused: bool,
    /// Components the target loader gets from the view, with original
    /// provenance. A command converted to a skill keeps its command identity.
    pub components: Vec<ComponentIdentity>,
    /// Command ids published to the target as skills.
    pub converted_commands: Vec<String>,
    /// Command ids with a form left out of the view because another form (or
    /// a same-id skill) already serves that invocation on the target.
    pub dropped_command_forms: Vec<String>,
    /// Native MCP entrypoints removed from the view (`<file>` or
    /// `<manifest>#mcpServers`). Source MCP declarations stay in the
    /// generation for the centralized host projection.
    pub removed_mcp_entrypoints: Vec<String>,
    /// Hook entrypoints removed from the view (`<manifest>#hooks`,
    /// `<manifest>#extensions.com.openai.hooks`, or `hooks/hooks.json`).
    /// Hook declarations stay in the generation; the view does not activate
    /// them.
    pub removed_hook_entrypoints: Vec<String>,
}

/// Immutable portable package view admitted from a verified provider generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortablePackageView {
    /// Source package identity.
    pub plugin_id: String,
    /// Provider generation copied into this view.
    pub generation: ProviderGeneration,
    /// Full digest of the admitted bytes.
    pub package_digest: PackageDigest,
    /// Immutable package directory.
    pub path: PathBuf,
    /// Whether an identical view already existed.
    pub reused: bool,
}

/// Reasons a provider generation cannot be admitted as a portable package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortablePackageRejection {
    /// Source package identity.
    pub plugin_id: String,
    /// Every unsupported or unsafe declaration found before publication.
    pub reasons: Vec<String>,
}

impl std::fmt::Display for PortablePackageRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "portable package '{}' rejected: {}",
            self.plugin_id,
            self.reasons.join("; ")
        )
    }
}

impl std::error::Error for PortablePackageRejection {}

/// Admit a digest-verified provider generation under an explicitly selected
/// non-live root. Rejected generations remain untouched at their private source.
pub fn admit_portable_package(
    rendered: &RenderedProviderGeneration,
    packages_root: &Path,
) -> Result<PortablePackageView, ProviderRenderError> {
    let files: PackageFiles = snapshot_dir(&rendered.plugin_id, &rendered.path)?
        .into_iter()
        .collect();
    let expected = rendered
        .generation
        .package_digest
        .as_str()
        .strip_prefix("sha256:")
        .ok_or_else(|| ProviderRenderError::Validation {
            plugin_id: rendered.plugin_id.clone(),
            message: "generation digest is not sha256".to_string(),
        })?;
    if files_digest_hex(&files) != expected {
        return Err(ProviderRenderError::GenerationMismatch {
            path: rendered.path.clone(),
            expected: rendered.generation.package_digest.to_string(),
        });
    }
    let reasons = portable_package_rejection_reasons(&files);
    if !reasons.is_empty() {
        return Err(ProviderRenderError::PortablePackageRejected(
            PortablePackageRejection {
                plugin_id: rendered.plugin_id.clone(),
                reasons,
            },
        ));
    }
    let digest = files_digest_hex(&files);
    let id = format!("g-{}", &digest[..GENERATION_ID_HEX_LEN]);
    let final_dir = packages_root
        .join(rendered.generation.source.as_str())
        .join(id);
    let parent = final_dir.parent().expect("package path has parent");
    if exists_no_follow(parent)? && is_link(parent)? {
        return Err(ProviderRenderError::LinkedPath {
            path: parent.to_path_buf(),
        });
    }
    if exists_no_follow(&final_dir)? {
        verify_published(&rendered.plugin_id, &final_dir, &digest)?;
        return Ok(PortablePackageView {
            plugin_id: rendered.plugin_id.clone(),
            generation: rendered.generation.clone(),
            package_digest: rendered.generation.package_digest.clone(),
            path: final_dir,
            reused: true,
        });
    }
    fs::create_dir_all(parent).map_err(io_error(parent))?;
    let reused = publish_generation(
        &rendered.plugin_id,
        &files,
        &digest,
        parent,
        &final_dir,
        &|_| Ok(()),
    )?;
    Ok(PortablePackageView {
        plugin_id: rendered.plugin_id.clone(),
        generation: rendered.generation.clone(),
        package_digest: rendered.generation.package_digest.clone(),
        path: final_dir,
        reused,
    })
}

fn portable_package_rejection_reasons(files: &PackageFiles) -> Vec<String> {
    let mut reasons = Vec::new();
    for (path, bytes) in files {
        let is_manifest = [ROOT_MANIFEST, CLAUDE_MANIFEST, CODEX_MANIFEST].contains(&path.as_str());
        let is_mcp = ROOT_MCP_FILES.contains(&path.as_str());
        let is_sidecar = is_supported_skill_config(path);
        let lower = path.to_ascii_lowercase();
        let is_hook = path == DEFAULT_HOOKS || lower.split('/').any(|segment| segment == "hooks");
        let is_config = [
            ".json",
            ".yaml",
            ".yml",
            ".toml",
            ".ini",
            ".cfg",
            ".conf",
            ".properties",
            ".env",
        ]
        .iter()
        .any(|ext| lower.ends_with(ext))
            || lower
                .rsplit('/')
                .next()
                .is_some_and(|name| name.starts_with(".env"));
        if is_hook {
            reasons.push(format!("{path}: hook configuration is not admitted"));
            continue;
        }
        if !is_manifest && !is_mcp && !is_sidecar && is_config {
            reasons.push(format!("{path}: unclassified configuration"));
            continue;
        }
        if is_sidecar {
            reasons.extend(
                validate_portable_skill_sidecar(bytes)
                    .into_iter()
                    .map(|reason| format!("{path}: {reason}")),
            );
            continue;
        }
        if !is_manifest && !is_mcp {
            continue;
        }
        let Ok(value) = parse_certified_json(bytes) else {
            reasons.push(format!("{path}: invalid JSON declaration"));
            continue;
        };
        if is_manifest {
            let Some(object) = value.as_object() else {
                reasons.push(format!("{path}: plugin manifest must be an object"));
                continue;
            };
            reasons.extend(
                validate_portable_manifest(object)
                    .into_iter()
                    .map(|reason| format!("{path}: {reason}")),
            );
        } else if let Some(object) = value.as_object() {
            if object.keys().any(|key| key != "mcpServers")
                || object.get("mcpServers").is_none_or(|servers| {
                    !servers.as_object().is_some_and(|servers| {
                        servers
                            .iter()
                            .all(|(name, server)| portable_mcp_server_is_admitted(name, server))
                    })
                })
            {
                reasons.push(format!("{path}: unsupported MCP declaration shape"));
            }
        } else {
            reasons.push(format!("{path}: MCP declaration must be an object"));
        }
    }
    reasons.sort();
    reasons.dedup();
    reasons
}

fn parse_certified_json(bytes: &[u8]) -> Result<Value, serde_json::Error> {
    struct JsonValue(Value);

    impl<'de> Deserialize<'de> for JsonValue {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            struct JsonValueVisitor;

            impl<'de> Visitor<'de> for JsonValueVisitor {
                type Value = JsonValue;

                fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    formatter.write_str("a JSON value without duplicate object keys")
                }

                fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                    Ok(JsonValue(Value::Bool(value)))
                }

                fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                    Ok(JsonValue(Value::from(value)))
                }

                fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                    Ok(JsonValue(Value::from(value)))
                }

                fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E> {
                    Ok(JsonValue(Value::from(value)))
                }

                fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
                where
                    E: de::Error,
                {
                    Ok(JsonValue(Value::String(value.to_owned())))
                }

                fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                    Ok(JsonValue(Value::String(value)))
                }

                fn visit_none<E>(self) -> Result<Self::Value, E> {
                    Ok(JsonValue(Value::Null))
                }

                fn visit_unit<E>(self) -> Result<Self::Value, E> {
                    Ok(JsonValue(Value::Null))
                }

                fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
                where
                    A: SeqAccess<'de>,
                {
                    let mut values = Vec::new();
                    while let Some(JsonValue(value)) = sequence.next_element()? {
                        values.push(value);
                    }
                    Ok(JsonValue(Value::Array(values)))
                }

                fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
                where
                    A: MapAccess<'de>,
                {
                    let mut values = serde_json::Map::new();
                    while let Some(key) = map.next_key::<String>()? {
                        if values.contains_key(&key) {
                            return Err(de::Error::custom(format!("duplicate object key '{key}'")));
                        }
                        let JsonValue(value) = map.next_value()?;
                        values.insert(key, value);
                    }
                    Ok(JsonValue(Value::Object(values)))
                }
            }

            deserializer.deserialize_any(JsonValueVisitor)
        }
    }

    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let JsonValue(value) = JsonValue::deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

fn is_supported_skill_config(path: &str) -> bool {
    let parts: Vec<_> = path.split('/').collect();
    parts.len() == 4
        && parts[0] == "skills"
        && !parts[1].is_empty()
        && parts[2] == "agents"
        && parts[3] == "openai.yaml"
}

fn validate_portable_skill_sidecar(bytes: &[u8]) -> Vec<String> {
    use serde_yaml_ng::Value as Yaml;
    let Ok(text) = std::str::from_utf8(bytes) else {
        return vec!["skill sidecar must be UTF-8".into()];
    };
    if let Err(error) = reject_unsupported_yaml_syntax(text) {
        return vec![error];
    }
    let value: Yaml = match serde_yaml_ng::from_slice(bytes) {
        Ok(value) => value,
        Err(error) => return vec![format!("invalid skill sidecar YAML: {error}")],
    };
    if let Err(error) = reject_unsupported_yaml_value(&value) {
        return vec![error];
    }
    if value.is_null() {
        return Vec::new();
    }
    let Some(root) = value.as_mapping() else {
        return vec!["skill sidecar root must be null or a mapping".into()];
    };
    let mut reasons = Vec::new();
    for (key, value) in root {
        let Some(key) = key.as_str() else {
            reasons.push("skill sidecar keys must be strings".into());
            continue;
        };
        match key {
            "interface" => {
                let Some(fields) = value.as_mapping() else {
                    reasons.push("interface must be a mapping".into());
                    continue;
                };
                for (key, value) in fields {
                    let Some(key) = key.as_str() else {
                        reasons.push("interface keys must be strings".into());
                        continue;
                    };
                    if ![
                        "display_name",
                        "short_description",
                        "icon_small",
                        "icon_large",
                        "brand_color",
                        "default_prompt",
                    ]
                    .contains(&key)
                    {
                        reasons.push(format!("unsupported interface field '{key}'"));
                        continue;
                    }
                    let Some(text) = value.as_str() else {
                        reasons.push(format!("interface.{key} must be a string"));
                        continue;
                    };
                    if matches!(key, "icon_small" | "icon_large") && !portable_relative_path(text) {
                        reasons.push(format!("interface.{key} must be a safe relative path"));
                    }
                }
            }
            "policy" => {
                let Some(fields) = value.as_mapping() else {
                    reasons.push("policy must be a mapping".into());
                    continue;
                };
                for (key, value) in fields {
                    if key.as_str() == Some("dependencies") {
                        reasons.push("skill MCP dependency is not admitted".into());
                        continue;
                    }
                    if key.as_str() != Some("allow_implicit_invocation") {
                        reasons.push(format!("unsupported policy field '{key:?}'"));
                        continue;
                    }
                    if !value.is_bool() {
                        reasons.push("policy.allow_implicit_invocation must be a boolean".into());
                    }
                }
            }
            _ => reasons.push(format!("unsupported skill sidecar field '{key}'")),
        }
    }
    reasons
}

fn portable_relative_path(value: &str) -> bool {
    !value.starts_with(['/', '\\'])
        && !value.contains("://")
        && !value.as_bytes().get(1).is_some_and(|b| *b == b':')
        && !value.split(['/', '\\']).any(|part| part == "..")
}

fn validate_portable_manifest(object: &serde_json::Map<String, Value>) -> Vec<String> {
    let mut reasons = Vec::new();
    for (key, value) in object {
        let valid = match key.as_str() {
            "name" => value.as_str().is_some_and(is_portable_identifier),
            "version" | "description" | "license" => value.is_string(),
            "keywords" => value
                .as_array()
                .is_some_and(|a| a.iter().all(Value::is_string)),
            "author" => {
                value.is_string()
                    || value.as_object().is_some_and(|a| {
                        a.iter().all(|(k, v)| {
                            ["name", "email"].contains(&k.as_str()) && v.is_string()
                                || k == "url" && v.as_str().is_some_and(is_portable_mcp_url)
                        })
                    })
            }
            "homepage" => value.as_str().is_some_and(is_portable_mcp_url),
            "repository" => {
                value.as_str().is_some_and(is_portable_mcp_url)
                    || value.as_object().is_some_and(|a| {
                        a.iter().all(|(k, v)| {
                            (k == "type" && v.is_string())
                                || (k == "url" && v.as_str().is_some_and(is_portable_mcp_url))
                        }) && a.contains_key("url")
                    })
            }
            "commands" | "agents" | "skills" => {
                let path_ok = |v: &Value| v.as_str().is_some_and(portable_relative_path);
                path_ok(value) || value.as_array().is_some_and(|a| a.iter().all(path_ok))
            }
            "mcpServers" => is_centralized_mcp_declaration(value),
            "hooks" => {
                reasons.push("hook declaration is not admitted".into());
                false
            }
            "lspServers" => {
                reasons.push("LSP declaration is not admitted".into());
                false
            }
            _ => {
                reasons.push(format!("unsupported declaration '{key}'"));
                false
            }
        };
        if !valid && !["hooks", "lspServers"].contains(&key.as_str()) {
            reasons.push(format!("invalid manifest field '{key}'"));
        }
    }
    reasons
}

fn portable_mcp_server_is_admitted(name: &str, server: &Value) -> bool {
    if !is_portable_identifier(name) {
        return false;
    }
    let Some(server) = server.as_object() else {
        return false;
    };
    if server
        .keys()
        .any(|key| !["type", "command", "args", "url", "env", "headers"].contains(&key.as_str()))
    {
        return false;
    }
    if server
        .get("type")
        .is_some_and(|value| !matches!(value.as_str(), Some("stdio" | "http" | "sse")))
        || server.get("command").is_some_and(|value| {
            !value
                .as_str()
                .is_some_and(|command| !command.chars().any(char::is_whitespace))
        })
        || server.get("args").is_some_and(|value| {
            !value
                .as_array()
                .is_some_and(|args| portable_mcp_args_are_admitted(args))
        })
        || server
            .get("url")
            .is_some_and(|value| !value.as_str().is_some_and(is_portable_mcp_url))
        || ["env", "headers"].iter().any(|field| {
            server.get(*field).is_some_and(|value| {
                !value.as_object().is_some_and(|values| {
                    values.iter().all(|(name, value)| {
                        is_portable_identifier(name)
                            && value.as_str().is_some_and(is_portable_placeholder)
                    })
                })
            })
        })
    {
        return false;
    }
    true
}

/// Credential option names are classified conservatively. A bare positional
/// literal without any marker cannot be classified and is treated like an
/// opaque asset.
fn portable_mcp_args_are_admitted(args: &[Value]) -> bool {
    const TERMS: [&str; 13] = [
        "token",
        "secret",
        "password",
        "passwd",
        "bearer",
        "credential",
        "apikey",
        "key",
        "auth",
        "cookie",
        "session",
        "signature",
        "private",
    ];
    let contains_term = |value: &str| {
        let normalized: String = value
            .chars()
            .filter(|ch| !matches!(ch, '-' | '_' | '.'))
            .flat_map(char::to_lowercase)
            .collect();
        TERMS.iter().any(|term| normalized.contains(term))
    };
    let mut index = 0;
    while index < args.len() {
        let Some(arg) = args[index].as_str() else {
            return false;
        };
        if arg.chars().any(|ch| ch.is_ascii_whitespace())
            && !is_portable_mcp_argument_with_value(arg)
        {
            return false;
        }
        let (name, inline_value) = arg
            .split_once('=')
            .map_or((arg, None), |(name, value)| (name, Some(value)));
        let credential_option =
            (inline_value.is_some() || name.starts_with('-')) && contains_term(name);
        if credential_option {
            match inline_value {
                Some(value) if is_portable_placeholder(value) => {}
                Some(_) => return false,
                None => {
                    let Some(value) = args.get(index + 1).and_then(Value::as_str) else {
                        return false;
                    };
                    if !is_portable_placeholder(value) {
                        return false;
                    }
                    index += 1;
                }
            }
        } else if arg.chars().any(|ch| matches!(ch, '=' | '-' | '_' | '.')) && contains_term(arg) {
            return false;
        }
        index += 1;
    }
    true
}

fn is_portable_mcp_argument_with_value(value: &str) -> bool {
    let Some((prefix, placeholder)) = value.split_once(':') else {
        return false;
    };
    if prefix.is_empty()
        || !prefix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return false;
    }
    let rest = placeholder.strip_prefix(' ').unwrap_or(placeholder);
    let (label, token) = if let Some((label, token)) = rest.split_once(' ') {
        (Some(label), token)
    } else {
        (None, rest)
    };
    if label.is_some_and(|s| s.is_empty() || !s.chars().all(|c| c.is_ascii_alphabetic())) {
        return false;
    }
    is_portable_placeholder(token)
}

fn is_portable_placeholder(value: &str) -> bool {
    let Some(name) = value
        .strip_prefix("${")
        .and_then(|value| value.strip_suffix("}"))
    else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_uppercase() || first == '_')
        && chars.all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
}

fn is_portable_mcp_url(url: &str) -> bool {
    if url.contains(['?', '#', '%']) {
        return false;
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    if authority.contains('@') {
        return false;
    }
    let (host, port) = authority
        .split_once(':')
        .map_or((authority, None), |(host, port)| (host, Some(port)));
    let allowed_host = |ch: char| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-');
    let allowed_path =
        |ch: char| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '~' | '/');
    !host.is_empty()
        && host.chars().all(allowed_host)
        && port.is_none_or(|port| !port.is_empty() && port.chars().all(|ch| ch.is_ascii_digit()))
        && path.chars().all(allowed_path)
        && !path.split('/').any(|segment| matches!(segment, "." | ".."))
}

fn is_portable_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    value.len() <= 64
        && (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
}

/// Directory holding every native package view of a generation:
/// `<providers>/<source-key>/<generation>.views/`. It is a sibling of the
/// immutable generation directory, never inside it.
pub fn native_package_views_dir(generation_dir: &Path) -> PathBuf {
    let mut name = generation_dir
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(VIEWS_DIR_SUFFIX);
    generation_dir.with_file_name(name)
}

/// Deterministic version suffix for one target and generation.
fn native_version_suffix(target: NativePackageTarget, package_digest: &PackageDigest) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(VIEW_VERSION_DOMAIN);
    hasher.update(target.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(package_digest.as_str().as_bytes());
    let hex = hex::encode(hasher.finalize());
    format!("ccync.{}", &hex[..VERSION_SUFFIX_HEX_LEN])
}

/// Append the suffix to the upstream version as semantic-version build
/// metadata and validate the result against the native version grammar.
fn native_package_version(upstream: Option<&str>, suffix: &str) -> Result<String, String> {
    let base = upstream.unwrap_or("0.0.0");
    let parsed = semver::Version::parse(base)
        .map_err(|e| format!("upstream version '{base}' is not a semantic version: {e}"))?;
    let version = if parsed.build.is_empty() {
        format!("{base}+{suffix}")
    } else {
        format!("{base}.{suffix}")
    };
    semver::Version::parse(&version)
        .map_err(|e| format!("version '{version}' is not a semantic version: {e}"))?;
    if version.chars().count() > NATIVE_FIELD_MAX_CHARS {
        return Err(format!(
            "version '{version}' exceeds {NATIVE_FIELD_MAX_CHARS} characters"
        ));
    }
    Ok(version)
}

/// Native plugin name grammar: an ASCII letter or digit, then ASCII letters,
/// digits, `_`, or `-`, at most 64 characters.
fn validate_native_plugin_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && name.len() <= NATIVE_FIELD_MAX_CHARS;
    if valid {
        Ok(())
    } else {
        Err(format!(
            "plugin name '{name}' must start with an ASCII letter or digit, contain only \
             ASCII letters, digits, '_' or '-', and be at most {NATIVE_FIELD_MAX_CHARS} characters"
        ))
    }
}

/// Whether a manifest `mcpServers` value only points at the root MCP files
/// the centralized host projection already reads (or declares nothing).
fn is_centralized_mcp_declaration(value: &Value) -> bool {
    match value {
        Value::String(path) => ROOT_MCP_FILES.contains(&path.strip_prefix("./").unwrap_or(path)),
        Value::Object(servers) => servers.is_empty(),
        _ => false,
    }
}

/// A derived view before publication.
struct DerivedView {
    files: PackageFiles,
    upstream_version: Option<String>,
    version: String,
    components: Vec<ComponentIdentity>,
    converted_commands: Vec<String>,
    dropped_command_forms: Vec<String>,
    removed_mcp_entrypoints: Vec<String>,
    removed_hook_entrypoints: Vec<String>,
}

/// Generate and publish the native package view of a rendered generation.
///
/// The view is derived in memory from the verified generation content and
/// published under [`native_package_views_dir`]`/<target>/<view-id>/` with
/// the same stage, validate, and no-overwrite rename discipline as
/// generations. The generation itself is never modified. Any unsupported or
/// ambiguous declaration blocks the view with a named error before anything
/// is written.
pub fn generate_native_package_view(
    rendered: &RenderedProviderGeneration,
    target: NativePackageTarget,
) -> Result<NativePackageView, ProviderRenderError> {
    generate_native_package_view_checked(rendered, target, &|_| Ok(()))
}

fn generate_native_package_view_checked(
    rendered: &RenderedProviderGeneration,
    target: NativePackageTarget,
    stage_check: StageCheck<'_>,
) -> Result<NativePackageView, ProviderRenderError> {
    let plugin_id = rendered.plugin_id.as_str();
    let generation_hex = rendered
        .generation
        .package_digest
        .as_str()
        .strip_prefix("sha256:")
        .ok_or_else(|| ProviderRenderError::Validation {
            plugin_id: plugin_id.to_string(),
            message: "generation digest is not a sha256 digest".to_string(),
        })?;
    // Read the generation once and check that read against its digest.
    if is_link(&rendered.path)? {
        return Err(ProviderRenderError::LinkedPath {
            path: rendered.path.clone(),
        });
    }
    let files: PackageFiles = snapshot_dir(plugin_id, &rendered.path)?
        .into_iter()
        .collect();
    if files_digest_hex(&files) != generation_hex {
        return Err(ProviderRenderError::GenerationMismatch {
            path: rendered.path.clone(),
            expected: rendered.generation.package_digest.to_string(),
        });
    }

    let derived = derive_native_view(rendered, target, files)?;
    let view_hex = files_digest_hex(&derived.files);
    let view_id = format!("v-{}", &view_hex[..GENERATION_ID_HEX_LEN]);
    let views_dir = native_package_views_dir(&rendered.path);
    let target_dir = views_dir.join(target.as_str());
    for dir in [&views_dir, &target_dir] {
        if exists_no_follow(dir)? && is_link(dir)? {
            return Err(ProviderRenderError::LinkedPath { path: dir.clone() });
        }
    }
    let final_dir = target_dir.join(&view_id);
    let reused = if exists_no_follow(&final_dir)? {
        verify_published(plugin_id, &final_dir, &view_hex)?;
        true
    } else {
        fs::create_dir_all(&target_dir).map_err(io_error(&target_dir))?;
        publish_generation(
            plugin_id,
            &derived.files,
            &view_hex,
            &target_dir,
            &final_dir,
            stage_check,
        )?
    };
    Ok(NativePackageView {
        target,
        plugin_id: plugin_id.to_string(),
        plugin_name: rendered.plugin_name.clone(),
        upstream_version: derived.upstream_version,
        upstream_revision: rendered.upstream_revision.clone(),
        version: derived.version,
        generation: rendered.generation.clone(),
        view_id,
        view_digest: PackageDigest::new(format!("sha256:{view_hex}"))?,
        path: final_dir,
        reused,
        components: derived.components,
        converted_commands: derived.converted_commands,
        dropped_command_forms: derived.dropped_command_forms,
        removed_mcp_entrypoints: derived.removed_mcp_entrypoints,
        removed_hook_entrypoints: derived.removed_hook_entrypoints,
    })
}

/// Derive a target view from generation content, entirely in memory.
fn derive_native_view(
    rendered: &RenderedProviderGeneration,
    target: NativePackageTarget,
    mut files: PackageFiles,
) -> Result<DerivedView, ProviderRenderError> {
    let plugin_id = rendered.plugin_id.as_str();
    let plugin_name = rendered.plugin_name.as_str();
    let blocked = |reason: String| ProviderRenderError::UnsupportedNativeDeclaration {
        target,
        plugin_id: plugin_id.to_string(),
        reason,
    };

    // A bare skill's content moves to `skills/<name>/` so the native loader
    // discovers it; paths inside the skill keep their relative layout and
    // plugin-root files stay at the root.
    if let Some(name) = bare_skill_name(&files, plugin_name) {
        if name.contains('/') || RelativePath::new(&name).is_err() {
            return Err(blocked(format!(
                "bare skill name '{name}' is not a safe directory name"
            )));
        }
        files = relocate_bare_skill(plugin_id, &name, files)?;
    }

    validate_native_plugin_name(plugin_name).map_err(blocked)?;

    let mut manifests: BTreeMap<&'static str, serde_json::Map<String, Value>> = BTreeMap::new();
    for path in [ROOT_MANIFEST, CLAUDE_MANIFEST, CODEX_MANIFEST] {
        if let Some(manifest) = parse_manifest(plugin_id, &files, path)? {
            manifests.insert(path, manifest);
        }
    }
    let conflict = |path: &str, reason: String| ProviderRenderError::ManifestConflict {
        plugin_id: plugin_id.to_string(),
        path: path.to_string(),
        reason,
    };
    for (path, manifest) in &manifests {
        if let Some(name) = manifest.get("name") {
            if name.as_str() != Some(plugin_name) {
                return Err(conflict(
                    path,
                    format!("name {name} differs from the plugin name '{plugin_name}'"),
                ));
            }
        }
    }
    let mut upstream_version: Option<String> = None;
    for path in target.manifest_order() {
        let Some(version) = manifests.get(path).and_then(|m| m.get("version")) else {
            continue;
        };
        let version = version
            .as_str()
            .ok_or_else(|| conflict(path, "version must be a string".to_string()))?;
        match &upstream_version {
            None => upstream_version = Some(version.to_string()),
            Some(first) if first != version => {
                return Err(conflict(
                    path,
                    format!("version '{version}' differs from '{first}'"),
                ))
            }
            Some(_) => {}
        }
    }
    let version = native_package_version(
        upstream_version.as_deref(),
        &native_version_suffix(target, &rendered.generation.package_digest),
    )
    .map_err(|reason| ProviderRenderError::InvalidNativeVersion {
        target,
        plugin_id: plugin_id.to_string(),
        reason,
    })?;

    // Native MCP entrypoints leave the view; the generation keeps them for
    // the centralized host MCP projection.
    let mut removed_mcp_entrypoints = Vec::new();
    for (path, manifest) in manifests.iter_mut() {
        if let Some(declaration) = manifest.remove("mcpServers") {
            if !is_centralized_mcp_declaration(&declaration) {
                return Err(blocked(format!(
                    "{path} declares mcpServers {declaration}, which the centralized MCP \
                     projection does not read; only the root .mcp.json is supported"
                )));
            }
            removed_mcp_entrypoints.push(format!("{path}#mcpServers"));
        }
    }
    // Codex also reads MCP servers from its effective settings object, so the
    // same remove-or-block rule applies there. Other loaders ignore it.
    if target == NativePackageTarget::Codex {
        let settings = manifests
            .get_mut(ROOT_MANIFEST)
            .and_then(|root| root.get_mut("extensions"))
            .and_then(Value::as_object_mut)
            .and_then(|extensions| extensions.get_mut("com.openai"))
            .and_then(Value::as_object_mut);
        if let Some(declaration) = settings.and_then(|settings| settings.remove("mcpServers")) {
            let label = format!("{ROOT_MANIFEST}#extensions.com.openai.mcpServers");
            if !is_centralized_mcp_declaration(&declaration) {
                return Err(blocked(format!(
                    "{label} declares {declaration}, which the centralized MCP projection \
                     does not read; only the root .mcp.json is supported"
                )));
            }
            removed_mcp_entrypoints.push(label);
        }
    }
    for file in ROOT_MCP_FILES {
        if files.remove(*file).is_some() {
            removed_mcp_entrypoints.push((*file).to_string());
        }
    }

    // Hook entrypoints leave the view too; the generation keeps their
    // declarations and helper files. Every hook in scope is checked first, so
    // an unresolvable or malformed hook still blocks the view.
    let scope = HookScope::Target(target);
    check_hook_helpers(plugin_id, &files, scope)?;
    hook_events(plugin_id, &files, scope)?;
    let mut removed_hook_entrypoints = Vec::new();
    for (path, manifest) in manifests.iter_mut() {
        if manifest.remove("hooks").is_some() {
            removed_hook_entrypoints.push(format!("{path}#hooks"));
        }
    }
    if target == NativePackageTarget::Codex {
        let settings = manifests
            .get_mut(ROOT_MANIFEST)
            .and_then(|root| root.get_mut("extensions"))
            .and_then(Value::as_object_mut)
            .and_then(|extensions| extensions.get_mut("com.openai"))
            .and_then(Value::as_object_mut);
        if settings
            .and_then(|settings| settings.remove("hooks"))
            .is_some()
        {
            removed_hook_entrypoints.push(format!("{ROOT_MANIFEST}#extensions.com.openai.hooks"));
        }
    }
    if files.remove(DEFAULT_HOOKS).is_some() {
        removed_hook_entrypoints.push(DEFAULT_HOOKS.to_string());
    }

    if !manifests.contains_key(target.primary_manifest()) {
        let mut primary = serde_json::Map::new();
        if let Some(description) = manifests.values().find_map(|m| m.get("description")) {
            primary.insert("description".to_string(), description.clone());
        }
        manifests.insert(target.primary_manifest(), primary);
    }
    for manifest in manifests.values_mut() {
        manifest.insert("name".to_string(), Value::from(plugin_name));
        manifest.insert("version".to_string(), Value::from(version.as_str()));
    }

    // Commands: one form per loader.
    let mut converted_commands = Vec::new();
    let mut dropped_command_forms = Vec::new();
    let existing_skills = skill_ids(&files);
    for (id, forms) in command_forms(&files) {
        let flat = format!("commands/{id}.md");
        let dir_prefix = format!("commands/{id}/");
        match target {
            NativePackageTarget::Codex => {
                let dir_files: Vec<String> = files
                    .keys()
                    .filter(|path| path.starts_with(&dir_prefix))
                    .cloned()
                    .collect();
                let flat_bytes = files.remove(&flat);
                let mut moved: Vec<(String, Vec<u8>)> = Vec::new();
                for path in dir_files {
                    if let Some(bytes) = files.remove(&path) {
                        moved.push((path, bytes));
                    }
                }
                if existing_skills.contains(&id) {
                    dropped_command_forms.push(id);
                    continue;
                }
                let skill_file = format!("skills/{id}/SKILL.md");
                if forms.directory {
                    for (path, bytes) in moved {
                        let relocated = format!("skills/{id}/{}", &path[dir_prefix.len()..]);
                        let bytes = if relocated == skill_file {
                            command_as_skill(target, plugin_id, plugin_name, &id, &bytes)?
                        } else {
                            bytes
                        };
                        files.insert(relocated, bytes);
                    }
                    if forms.flat {
                        dropped_command_forms.push(id.clone());
                    }
                } else if let Some(bytes) = flat_bytes {
                    files.insert(
                        skill_file,
                        command_as_skill(target, plugin_id, plugin_name, &id, &bytes)?,
                    );
                }
                converted_commands.push(id);
            }
            NativePackageTarget::Claude => {
                if forms.flat && forms.directory {
                    files.remove(&format!("{dir_prefix}SKILL.md"));
                    dropped_command_forms.push(id);
                }
            }
        }
    }

    if target == NativePackageTarget::Codex {
        for id in skill_ids(&files) {
            let sidecar = format!("skills/{id}/agents/openai.yaml");
            let purpose = files
                .get(&format!("skills/{id}/SKILL.md"))
                .and_then(|bytes| skill_purpose(bytes));
            let updated = update_skill_sidecar(
                plugin_id,
                &sidecar,
                files.get(&sidecar).map(Vec::as_slice),
                plugin_name,
                &id,
                purpose.as_deref(),
            )?;
            files.insert(sidecar, updated);
        }
        update_codex_plugin_interface(plugin_id, &mut manifests, plugin_name).map_err(blocked)?;
    }

    for (path, manifest) in &manifests {
        let mut text = serde_json::to_string_pretty(&Value::Object(manifest.clone()))
            .map_err(|e| blocked(format!("cannot serialize {path}: {e}")))?;
        text.push('\n');
        files.insert((*path).to_string(), text.into_bytes());
    }

    let owner = ComponentOwner {
        plugin_id,
        source: &rendered.generation.source,
        plugin_name,
        upstream_revision: rendered.upstream_revision.as_ref(),
    };
    let mut components = enumerate_components(&owner, &files, scope)?;
    if target == NativePackageTarget::Codex {
        // Codex loads skills and hooks only; converted commands keep their
        // command identity.
        components.retain(|c| {
            matches!(c.kind, ComponentKind::Skill | ComponentKind::Hook)
                && !(c.kind == ComponentKind::Skill && converted_commands.contains(&c.component_id))
        });
        for id in &converted_commands {
            components.push(owner.identity(ComponentKind::Command, id)?);
        }
        components.sort();
    }

    Ok(DerivedView {
        files,
        upstream_version,
        version,
        components,
        converted_commands,
        dropped_command_forms,
        removed_mcp_entrypoints,
        removed_hook_entrypoints,
    })
}

/// Add the management marker to the effective Codex plugin presentation.
///
/// The effective OpenAI settings are the root `extensions["com.openai"]`
/// object when present (Codex then ignores the `.codex-plugin` overlay), else
/// the `.codex-plugin` manifest. Only `interface.shortDescription` and a
/// missing `interface.displayName` change; the overlay, its hooks, and every
/// other field are kept, and no `extensions["com.openai"]` object is added.
fn update_codex_plugin_interface(
    plugin_id: &str,
    manifests: &mut BTreeMap<&'static str, serde_json::Map<String, Value>>,
    plugin_name: &str,
) -> Result<(), String> {
    let has_extension = match manifests.get(ROOT_MANIFEST) {
        Some(root) => openai_extension(plugin_id, root)
            .map_err(|e| e.to_string())?
            .is_some(),
        None => false,
    };
    let settings = if has_extension {
        manifests
            .get_mut(ROOT_MANIFEST)
            .and_then(|root| root.get_mut("extensions"))
            .and_then(|extensions| extensions.get_mut("com.openai"))
            .and_then(Value::as_object_mut)
    } else {
        manifests.get_mut(CODEX_MANIFEST)
    }
    .ok_or_else(|| "effective Codex plugin settings are missing".to_string())?;
    let interface = settings
        .entry("interface")
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| "plugin interface must be an object".to_string())?;
    match interface.get("displayName") {
        None | Some(Value::Null) => {
            interface.insert("displayName".to_string(), Value::from(plugin_name));
        }
        Some(Value::String(_)) => {}
        Some(_) => return Err("plugin interface.displayName must be a string".to_string()),
    }
    let purpose = match interface.get("shortDescription") {
        None | Some(Value::Null) => plugin_name.to_string(),
        Some(Value::String(text)) => text.clone(),
        Some(_) => return Err("plugin interface.shortDescription must be a string".to_string()),
    };
    interface.insert(
        "shortDescription".to_string(),
        Value::from(with_management_marker(&purpose, None)),
    );
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, contents).expect("write");
    }

    /// A plugin payload with a skill named `shared` plus resources, hooks,
    /// commands, agents, relative links, and policy files.
    fn write_plugin(root: &Path, plugin_name: &str, marker: &str) {
        write(
            &root.join(".claude-plugin").join("plugin.json"),
            &json!({"name": plugin_name, "version": "1.0.0"}).to_string(),
        );
        write(
            &root.join("skills").join("shared").join("SKILL.md"),
            &format!(
                "---\nname: shared\ndescription: {marker}\n---\nSee [ref](references/guide.md).\n"
            ),
        );
        write(
            &root
                .join("skills")
                .join("shared")
                .join("references")
                .join("guide.md"),
            &format!("guide for {marker}\n"),
        );
        write(
            &root.join("commands").join("run.md"),
            &format!("run {marker}\n"),
        );
        write(
            &root.join("agents").join("helper.md"),
            &format!("agent {marker}\n"),
        );
        write(
            &root.join("hooks").join("hooks.json"),
            r#"{"hooks":{"SessionStart":[{"command":"${CLAUDE_PLUGIN_ROOT}/scripts/start.sh"}]}}"#,
        );
        write(
            &root.join("scripts").join("start.sh"),
            &format!("echo {marker}\n"),
        );
        write(&root.join("LICENSE"), &format!("license {marker}\n"));
        write(&root.join(".git").join("HEAD"), "ref: refs/heads/main\n");
    }

    struct Fixture {
        _tmp: TempDir,
        plugins_root: PathBuf,
        cache_root: PathBuf,
        providers_root: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = TempDir::new().expect("tempdir");
        let base = tmp.path().join(".ccync");
        Fixture {
            plugins_root: base.join("plugins"),
            cache_root: base.join("cache"),
            providers_root: base.join("build").join("render").join("providers"),
            _tmp: tmp,
        }
    }

    fn two_provider_lock() -> Value {
        json!({
            "resolvedPlugins": [
                {"pluginId": "alpha", "resolvedSource": "alpha", "installStrategy": "bundled-local", "sha": "a1"},
                {"pluginId": "remote", "resolvedSource": "https://x", "installStrategy": "git"}
            ],
            "_personalPlugins": [
                {"pluginId": "Beta", "source": "https://example.invalid/beta.git", "pin": "b2"}
            ],
            "_looseSkills": [{"name": "shared"}]
        })
    }

    fn two_provider_fixture() -> (Fixture, ProviderInventory) {
        let fx = fixture();
        write_plugin(&fx.plugins_root.join("alpha"), "alpha-plugin", "alpha");
        write_plugin(&fx.cache_root.join("Beta@b2"), "beta-plugin", "beta");
        // A stale sibling for the personal plugin must not be selected.
        write_plugin(&fx.cache_root.join("Beta@old"), "beta-plugin", "stale");
        let inventory =
            build_provider_inventory(&two_provider_lock(), &fx.plugins_root, &fx.cache_root)
                .expect("inventory");
        (fx, inventory)
    }

    fn snapshot_of(dir: &Path) -> Snapshot {
        collect_dir_snapshot(dir).expect("snapshot")
    }

    fn dir_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn provider_render_inventory_derives_lowercase_keys_from_source_identity() {
        let (_fx, inventory) = two_provider_fixture();
        let keys: Vec<&str> = inventory.providers.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, ["alpha", "beta"]);
        let beta = &inventory.providers[1];
        assert_eq!(beta.plugin_id, "Beta");
        assert_eq!(beta.record, ProviderRecordKind::Personal);
        assert_eq!(beta.plugin_name, "beta-plugin");
        assert_eq!(beta.upstream_revision.as_deref(), Some("b2"));
        assert!(beta.payload_dir.ends_with("Beta@b2"));
        let alpha = &inventory.providers[0];
        assert_eq!(alpha.record, ProviderRecordKind::Bundled);
        assert_eq!(alpha.upstream_revision.as_deref(), Some("a1"));
        // Loose skills and non-bundled public entries are not providers.
        assert!(inventory.unavailable.is_empty());
        assert!(!keys.contains(&"shared"));
        assert!(!keys.contains(&"remote"));
    }

    #[test]
    fn provider_render_inventory_rejects_case_folded_key_conflict() {
        let fx = fixture();
        write_plugin(&fx.plugins_root.join("demo"), "demo", "bundled");
        write_plugin(&fx.cache_root.join("DEMO@p1"), "demo", "personal");
        let lock = json!({
            "resolvedPlugins": [
                {"pluginId": "demo", "resolvedSource": "demo", "installStrategy": "bundled-local"}
            ],
            "_personalPlugins": [
                {"pluginId": "DEMO", "source": "https://example.invalid/demo.git", "pin": "p1"}
            ]
        });
        let err = build_provider_inventory(&lock, &fx.plugins_root, &fx.cache_root)
            .expect_err("conflict");
        match &err {
            ProviderRenderError::SourceKeyConflict { key, first, second } => {
                assert_eq!(key, "demo");
                assert!(first.contains("bundled plugin 'demo'"), "{first}");
                assert!(second.contains("personal plugin 'DEMO'"), "{second}");
            }
            other => panic!("unexpected error {other:?}"),
        }
        let text = err.to_string();
        assert!(text.contains("'demo'") && text.contains("'DEMO'"), "{text}");
    }

    #[test]
    fn provider_render_inventory_rejects_unsafe_keys_and_records() {
        let fx = fixture();
        let device = json!({"_personalPlugins": [{"pluginId": "con", "source": "x"}]});
        assert!(matches!(
            build_provider_inventory(&device, &fx.plugins_root, &fx.cache_root),
            Err(ProviderRenderError::InvalidSourceKey { .. })
        ));
        let traversal = json!({"resolvedPlugins": [
            {"pluginId": "evil", "resolvedSource": "../outside", "installStrategy": "bundled-local"}
        ]});
        assert!(matches!(
            build_provider_inventory(&traversal, &fx.plugins_root, &fx.cache_root),
            Err(ProviderRenderError::InvalidSourceRecord { .. })
        ));
        assert!(build_provider_inventory(&json!([]), &fx.plugins_root, &fx.cache_root).is_err());
    }

    #[test]
    fn p7_t02_subpath_manifest_yields_native_name_and_keeps_storage_id() {
        let fx = fixture();
        let root = fx.plugins_root.join("plugin");
        write(
            &root.join(".claude-plugin").join("plugin.json"),
            &json!({"name": "impeccable", "version": "1.0.0"}).to_string(),
        );
        write(
            &root.join("skills").join("craft").join("SKILL.md"),
            "---\nname: craft\ndescription: Craft\n---\n",
        );
        let inventory =
            build_provider_inventory(&bundled_lock(&["plugin"]), &fx.plugins_root, &fx.cache_root)
                .expect("inventory");
        let entry = &inventory.providers[0];
        assert_eq!(entry.plugin_name, "impeccable");
        assert_eq!(entry.plugin_id, "plugin");
        assert_eq!(entry.key.as_str(), "plugin");
        assert!(entry.payload_dir.ends_with("plugin"));
    }

    #[test]
    fn p7_t02_graphify_skill_only_record_yields_skill_name_and_keeps_storage_id() {
        let fx = fixture();
        write(
            &fx.plugins_root.join("graphify-agents").join("SKILL.md"),
            "---\nname: graphify\ndescription: Graph\n---\n",
        );
        let inventory = build_provider_inventory(
            &bundled_lock(&["graphify-agents"]),
            &fx.plugins_root,
            &fx.cache_root,
        )
        .expect("inventory");
        let entry = &inventory.providers[0];
        assert_eq!(entry.plugin_name, "graphify");
        assert_eq!(entry.plugin_id, "graphify-agents");
        assert_eq!(entry.key.as_str(), "graphify-agents");
    }

    #[test]
    fn p7_t02_generic_basename_is_never_a_native_name() {
        let fx = fixture();
        let root = fx.plugins_root.join("plugin");
        write(
            &root.join("SKILL.md"),
            "---\nname: plugin\ndescription: Generic\n---\n",
        );
        assert_eq!(
            authoritative_plugin_name("plugin", &root, None).expect("name"),
            None
        );
        let inventory =
            build_provider_inventory(&bundled_lock(&["plugin"]), &fx.plugins_root, &fx.cache_root)
                .expect("inventory");
        assert_eq!(inventory.providers[0].plugin_name, "plugin");
    }

    #[test]
    fn p7_t02_catalog_name_fills_generic_gap_and_bad_catalog_name_is_rejected() {
        let fx = fixture();
        write(
            &fx.plugins_root.join("plugin").join("SKILL.md"),
            "---\nname: plugin\ndescription: Generic\n---\n",
        );
        let lock = |name: &str| {
            json!({"resolvedPlugins": [{
                "pluginId": "plugin",
                "resolvedSource": "plugin",
                "installStrategy": "bundled-local",
                "packageName": name,
            }]})
        };
        let inventory =
            build_provider_inventory(&lock("impeccable"), &fx.plugins_root, &fx.cache_root)
                .expect("inventory");
        assert_eq!(inventory.providers[0].plugin_name, "impeccable");
        let err = build_provider_inventory(&lock("  "), &fx.plugins_root, &fx.cache_root)
            .expect_err("empty catalog name");
        assert!(
            matches!(&err, ProviderRenderError::InvalidPackageName { plugin_id, .. } if plugin_id == "plugin"),
            "{err}"
        );
    }

    #[test]
    fn p7_t02_conflicting_manifest_and_catalog_names_return_scoped_error() {
        let fx = fixture();
        write(
            &fx.plugins_root
                .join("plugin")
                .join(".claude-plugin")
                .join("plugin.json"),
            &json!({"name": "impeccable", "version": "1.0.0"}).to_string(),
        );
        let lock = json!({"resolvedPlugins": [{
            "pluginId": "plugin",
            "resolvedSource": "plugin",
            "installStrategy": "bundled-local",
            "packageName": "other",
        }]});
        let err = build_provider_inventory(&lock, &fx.plugins_root, &fx.cache_root)
            .expect_err("conflicting names");
        assert!(
            matches!(&err, ProviderRenderError::ManifestConflict { plugin_id, .. } if plugin_id == "plugin"),
            "{err}"
        );
    }

    #[test]
    fn provider_render_inventory_reports_unavailable_payloads() {
        let fx = fixture();
        write_plugin(&fx.cache_root.join("multi@s1"), "multi", "one");
        write_plugin(&fx.cache_root.join("multi@s2"), "multi", "two");
        let lock = json!({
            "resolvedPlugins": [
                {"pluginId": "gone", "resolvedSource": "gone", "installStrategy": "bundled-local"}
            ],
            "_personalPlugins": [
                {"pluginId": "multi", "source": "https://example.invalid/m.git"}
            ]
        });
        let inventory =
            build_provider_inventory(&lock, &fx.plugins_root, &fx.cache_root).expect("inventory");
        assert!(inventory.providers.is_empty());
        let ids: Vec<&str> = inventory
            .unavailable
            .iter()
            .map(|u| u.plugin_id.as_str())
            .collect();
        assert_eq!(ids, ["gone", "multi"]);
    }

    #[test]
    fn provider_render_rejects_malformed_manifest() {
        let fx = fixture();
        write(
            &fx.plugins_root.join("bad").join("plugin.json"),
            "{not json",
        );
        write(
            &fx.plugins_root
                .join("bad")
                .join("skills")
                .join("x")
                .join("SKILL.md"),
            "x",
        );
        let lock = json!({"resolvedPlugins": [
            {"pluginId": "bad", "resolvedSource": "bad", "installStrategy": "bundled-local"}
        ]});
        assert!(matches!(
            build_provider_inventory(&lock, &fx.plugins_root, &fx.cache_root),
            Err(ProviderRenderError::InvalidManifest { .. })
        ));
    }

    #[test]
    fn provider_render_keeps_same_basename_providers_complete_and_separate() {
        let (fx, inventory) = two_provider_fixture();
        let rendered =
            render_provider_generations_at(&inventory, &fx.providers_root).expect("render");
        assert_eq!(rendered.len(), 2);

        for (index, marker) in [(0, "alpha"), (1, "beta")] {
            let generation = &rendered[index];
            assert!(!generation.reused);
            let source_key = generation.generation.source.as_str();
            assert_eq!(
                generation.path,
                fx.providers_root
                    .join(source_key)
                    .join(generation.generation.generation.as_str())
            );
            let skill =
                fs::read_to_string(generation.path.join("skills/shared/SKILL.md")).expect("skill");
            assert!(skill.contains(marker), "{skill}");
            for resource in [
                "skills/shared/references/guide.md",
                "commands/run.md",
                "agents/helper.md",
                "hooks/hooks.json",
                "scripts/start.sh",
                "LICENSE",
                ".claude-plugin/plugin.json",
            ] {
                assert!(generation.path.join(resource).is_file(), "{resource}");
            }
            assert!(
                !generation.path.join(".git").exists(),
                "version-control metadata is not payload"
            );
            let guide =
                fs::read_to_string(generation.path.join("skills/shared/references/guide.md"))
                    .expect("guide");
            assert_eq!(guide, format!("guide for {marker}\n"));

            let skills: Vec<&ComponentIdentity> = generation
                .components
                .iter()
                .filter(|c| c.kind == ComponentKind::Skill)
                .collect();
            assert_eq!(skills.len(), 1);
            assert_eq!(skills[0].component_id, "shared");
            assert_eq!(skills[0].source.as_str(), source_key);
            let kinds: BTreeSet<(ComponentKind, &str)> = generation
                .components
                .iter()
                .map(|c| (c.kind, c.component_id.as_str()))
                .collect();
            assert!(kinds.contains(&(ComponentKind::Command, "run")));
            assert!(kinds.contains(&(ComponentKind::Agent, "helper")));
            // Hook identity comes from the declared event, not the file name.
            assert!(kinds.contains(&(ComponentKind::Hook, "SessionStart")));
            assert!(!kinds.contains(&(ComponentKind::Hook, "hooks.json")));
        }
        assert_eq!(rendered[0].plugin_name, "alpha-plugin");
        assert_eq!(rendered[1].plugin_name, "beta-plugin");
        assert_ne!(
            rendered[0].generation.package_digest,
            rendered[1].generation.package_digest
        );
        assert_eq!(dir_names(&fx.providers_root), ["alpha", "beta"]);
    }

    #[test]
    fn provider_render_identical_input_reuses_generation() {
        let (fx, inventory) = two_provider_fixture();
        let first = render_provider_generations_at(&inventory, &fx.providers_root).expect("first");
        let before = snapshot_of(&fx.providers_root);
        let second =
            render_provider_generations_at(&inventory, &fx.providers_root).expect("second");
        assert!(second.iter().all(|g| g.reused));
        for (a, b) in first.iter().zip(&second) {
            assert_eq!(a.generation, b.generation);
            assert_eq!(a.components, b.components);
        }
        assert_eq!(snapshot_of(&fx.providers_root), before);
        assert_eq!(dir_names(&fx.providers_root.join("alpha")).len(), 1);
        let id = first[0].generation.generation.as_str();
        assert!(id.starts_with("g-") && id.len() == 2 + GENERATION_ID_HEX_LEN);
        assert!(first[0]
            .generation
            .package_digest
            .as_str()
            .starts_with("sha256:"));
    }

    #[test]
    fn provider_render_failed_stage_keeps_old_generation_and_leaves_no_residue() {
        let (fx, inventory) = two_provider_fixture();
        let alpha = inventory.providers[0].clone();
        let old = render_provider_generation(&alpha, &fx.providers_root, &|_| Ok(()))
            .expect("old generation");
        let provider_dir = fx.providers_root.join("alpha");
        let old_bytes = snapshot_of(&old.path);

        // New upstream content produces a new generation id.
        write(
            &alpha.payload_dir.join("skills/shared/SKILL.md"),
            "---\nname: shared\ndescription: changed\n---\n",
        );
        let injected = |stage: &Path| {
            assert!(stage.join("skills/shared/SKILL.md").is_file());
            Err(ProviderRenderError::Validation {
                plugin_id: "alpha".to_string(),
                message: "injected failure".to_string(),
            })
        };
        let err = render_provider_generation(&alpha, &fx.providers_root, &injected)
            .expect_err("injected failure");
        assert!(err.to_string().contains("injected failure"));

        assert_eq!(
            dir_names(&provider_dir),
            [old.generation.generation.to_string()]
        );
        assert_eq!(snapshot_of(&old.path), old_bytes);

        // A later successful render publishes alongside and keeps the old one.
        let new = render_provider_generation(&alpha, &fx.providers_root, &|_| Ok(()))
            .expect("new generation");
        assert_ne!(new.generation.generation, old.generation.generation);
        assert_eq!(dir_names(&provider_dir).len(), 2);
        assert_eq!(snapshot_of(&old.path), old_bytes);
    }

    #[test]
    fn provider_render_never_overwrites_tampered_generation() {
        let (fx, inventory) = two_provider_fixture();
        let rendered =
            render_provider_generations_at(&inventory, &fx.providers_root).expect("render");
        let tampered = rendered[0].path.join("LICENSE");
        fs::write(&tampered, "tampered").expect("tamper");
        let err =
            render_provider_generations_at(&inventory, &fx.providers_root).expect_err("mismatch");
        assert!(matches!(
            err,
            ProviderRenderError::GenerationMismatch { .. }
        ));
        assert_eq!(fs::read_to_string(&tampered).expect("read"), "tampered");
    }

    #[test]
    fn provider_render_bare_skill_payload_keeps_topology() {
        let fx = fixture();
        let payload = fx.cache_root.join("solo@s1");
        write(
            &payload.join("SKILL.md"),
            "---\nname: solo\ndescription: d\n---\n",
        );
        write(&payload.join("assets").join("a.txt"), "asset");
        let lock = json!({"_personalPlugins": [
            {"pluginId": "solo", "source": "https://example.invalid/solo.git", "pin": "s1"}
        ]});
        let inventory =
            build_provider_inventory(&lock, &fx.plugins_root, &fx.cache_root).expect("inventory");
        let rendered =
            render_provider_generations_at(&inventory, &fx.providers_root).expect("render");
        let generation = &rendered[0];
        assert!(generation.path.join("SKILL.md").is_file());
        assert!(generation.path.join("assets/a.txt").is_file());
        assert_eq!(generation.components.len(), 1);
        assert_eq!(generation.components[0].component_id, "solo");
        assert_eq!(generation.components[0].plugin_name, "solo");
    }

    #[test]
    fn provider_render_rejects_empty_payload() {
        let fx = fixture();
        fs::create_dir_all(fx.plugins_root.join("empty").join(".git")).expect("mkdir");
        let lock = json!({"resolvedPlugins": [
            {"pluginId": "empty", "resolvedSource": "empty", "installStrategy": "bundled-local"}
        ]});
        let inventory =
            build_provider_inventory(&lock, &fx.plugins_root, &fx.cache_root).expect("inventory");
        assert!(matches!(
            render_provider_generations_at(&inventory, &fx.providers_root),
            Err(ProviderRenderError::EmptyPayload { .. })
        ));
        assert!(!fx.providers_root.join("empty").exists());
    }

    /// Restores the home variable on drop so a failed assertion cannot leak
    /// a temp home into other tests.
    struct HomeOverride {
        var: &'static str,
        prior: Option<std::ffi::OsString>,
    }

    impl HomeOverride {
        fn set(home: &Path) -> Self {
            let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
            let prior = std::env::var_os(var);
            std::env::set_var(var, home);
            Self { var, prior }
        }
    }

    impl Drop for HomeOverride {
        fn drop(&mut self) {
            match self.prior.take() {
                Some(value) => std::env::set_var(self.var, value),
                None => std::env::remove_var(self.var),
            }
        }
    }

    #[test]
    fn provider_render_default_roots_use_isolated_home() {
        let _env = crate::test_support::env_guard();
        let tmp = TempDir::new().expect("tempdir");
        let _home = HomeOverride::set(tmp.path());

        // Absent lockfile: empty inventory, nothing written.
        let empty = load_provider_inventory().expect("absent lock");
        assert_eq!(empty, ProviderInventory::default());

        let ccync = tmp.path().join(".ccync");
        write_plugin(
            &ccync.join("plugins").join("alpha"),
            "alpha-plugin",
            "alpha",
        );
        let lock_path = crate::paths::plugins_lock_path().expect("lock path");
        assert!(lock_path.starts_with(tmp.path()));
        write(&lock_path, "{corrupt");
        assert!(matches!(
            load_provider_inventory(),
            Err(ProviderRenderError::Lockfile { .. })
        ));

        write(&lock_path, &two_provider_lock().to_string());
        let inventory = load_provider_inventory().expect("inventory");
        assert_eq!(inventory.providers.len(), 1);
        assert_eq!(inventory.unavailable.len(), 1, "Beta has no cache dir");
        let rendered = render_provider_generations(&inventory).expect("render");
        let expected = rendered[0].generation.path().expect("generation path");
        assert_eq!(rendered[0].path, expected);
        assert!(expected.starts_with(ccync.join("build").join("render").join("providers")));
    }

    // ── independent verification probes ─────────────────────────────────────

    fn write_bytes(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, bytes).expect("write");
    }

    /// The generation directory a provider entry is expected to land in,
    /// computed from its source payload.
    fn expected_generation_dir(entry: &ProviderEntry, providers_root: &Path) -> PathBuf {
        let snapshot = snapshot_payload(&entry.plugin_id, &entry.payload_dir).expect("snapshot");
        let hex = package_digest_hex(&snapshot);
        providers_root
            .join(entry.key.as_str())
            .join(format!("g-{}", &hex[..GENERATION_ID_HEX_LEN]))
    }

    fn stage_residue(provider_dir: &Path) -> Vec<String> {
        if !provider_dir.exists() {
            return Vec::new();
        }
        dir_names(provider_dir)
            .into_iter()
            .filter(|n| n.starts_with(STAGE_PREFIX))
            .collect()
    }

    fn bundled_lock(ids: &[&str]) -> Value {
        let plugins: Vec<Value> = ids
            .iter()
            .map(|id| json!({"pluginId": id, "resolvedSource": id, "installStrategy": "bundled-local"}))
            .collect();
        json!({ "resolvedPlugins": plugins })
    }

    fn try_link(target: &Path, link: &Path) -> bool {
        fs::create_dir_all(link.parent().expect("parent")).expect("mkdir");
        ccync_foundation::platform::create_dir_link(target, link).is_ok()
    }

    #[test]
    fn provider_render_same_basename_skill_command_agent_kept_byte_for_byte() {
        let (fx, inventory) = two_provider_fixture();
        let rendered =
            render_provider_generations_at(&inventory, &fx.providers_root).expect("render");
        for (entry, generation) in inventory.providers.iter().zip(&rendered) {
            let source = snapshot_payload(&entry.plugin_id, &entry.payload_dir).expect("source");
            assert!(source.iter().all(|(rel, _)| !rel.starts_with(".git/")));
            assert_eq!(
                snapshot_of(&generation.path),
                source,
                "generation of {} must equal its source payload byte-for-byte",
                entry.plugin_id
            );
            // The relative link inside SKILL.md resolves inside the generation.
            let skill_dir = generation.path.join("skills/shared");
            assert!(skill_dir.join("references/guide.md").is_file());
        }
        // Same-named command and agent files carry each provider's own body.
        for rel in ["commands/run.md", "agents/helper.md", "scripts/start.sh"] {
            let a = fs::read_to_string(rendered[0].path.join(rel)).expect("alpha");
            let b = fs::read_to_string(rendered[1].path.join(rel)).expect("beta");
            assert!(
                a.contains("alpha") && b.contains("beta"),
                "{rel}: {a} / {b}"
            );
        }
        // The stale personal sibling never leaks into the beta generation.
        let beta_all: Vec<u8> = snapshot_of(&rendered[1].path)
            .into_iter()
            .flat_map(|(_, b)| b)
            .collect();
        assert!(!String::from_utf8_lossy(&beta_all).contains("stale"));
    }

    #[test]
    fn provider_render_generation_id_ignores_mtime_and_creation_order() {
        let fx = fixture();
        let files: [(&str, &str); 4] = [
            ("skills/s/SKILL.md", "---\nname: s\ndescription: d\n---\n"),
            ("skills/s/references/r.md", "ref\n"),
            ("commands/c.md", "cmd\n"),
            ("agents/a.md", "agent\n"),
        ];
        for (rel, body) in files {
            write(&fx.plugins_root.join("one").join(rel), body);
        }
        for (rel, body) in files.iter().rev() {
            write(&fx.plugins_root.join("two").join(rel), body);
        }
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        fs::File::options()
            .write(true)
            .open(fx.plugins_root.join("two").join("commands/c.md"))
            .expect("open")
            .set_modified(old)
            .expect("set mtime");
        let inventory = build_provider_inventory(
            &bundled_lock(&["one", "two"]),
            &fx.plugins_root,
            &fx.cache_root,
        )
        .expect("inventory");
        let rendered =
            render_provider_generations_at(&inventory, &fx.providers_root).expect("render");
        assert_eq!(
            rendered[0].generation.generation,
            rendered[1].generation.generation
        );
        assert_eq!(
            rendered[0].generation.package_digest,
            rendered[1].generation.package_digest
        );
        assert_ne!(rendered[0].path, rendered[1].path, "keys stay separate");

        // Touching an mtime later does not create a new generation.
        fs::File::options()
            .write(true)
            .open(fx.plugins_root.join("one").join("agents/a.md"))
            .expect("open")
            .set_modified(old)
            .expect("set mtime");
        let again = render_provider_generations_at(&inventory, &fx.providers_root).expect("again");
        assert!(again.iter().all(|g| g.reused));
        assert_eq!(dir_names(&fx.providers_root.join("one")).len(), 1);
    }

    #[test]
    fn provider_render_crlf_and_lf_content_get_distinct_generations() {
        let fx = fixture();
        write_bytes(
            &fx.plugins_root.join("lf").join("skills/s/SKILL.md"),
            b"---\nname: s\ndescription: d\n---\nbody\n",
        );
        write_bytes(
            &fx.plugins_root.join("crlf").join("skills/s/SKILL.md"),
            b"---\r\nname: s\r\ndescription: d\r\n---\r\nbody\r\n",
        );
        let inventory = build_provider_inventory(
            &bundled_lock(&["lf", "crlf"]),
            &fx.plugins_root,
            &fx.cache_root,
        )
        .expect("inventory");
        let rendered =
            render_provider_generations_at(&inventory, &fx.providers_root).expect("render");
        assert_ne!(
            rendered[0].generation.generation,
            rendered[1].generation.generation
        );
        let crlf = rendered
            .iter()
            .find(|g| g.plugin_id == "crlf")
            .expect("crlf");
        assert_eq!(
            fs::read(crlf.path.join("skills/s/SKILL.md")).expect("read"),
            b"---\r\nname: s\r\ndescription: d\r\n---\r\nbody\r\n",
            "content is not normalized"
        );
    }

    #[test]
    fn provider_render_stage_tampered_during_validation_fails_and_keeps_prior() {
        let (fx, inventory) = two_provider_fixture();
        let alpha = inventory.providers[0].clone();
        let old = render_provider_generation(&alpha, &fx.providers_root, &|_| Ok(()))
            .expect("old generation");
        let old_bytes = snapshot_of(&old.path);
        let provider_dir = fx.providers_root.join("alpha");
        write(&alpha.payload_dir.join("commands/run.md"), "run v2\n");

        let modify = |stage: &Path| -> Result<(), ProviderRenderError> {
            fs::write(stage.join("commands/run.md"), "corrupted").expect("tamper");
            Ok(())
        };
        let add = |stage: &Path| -> Result<(), ProviderRenderError> {
            fs::write(stage.join("EXTRA"), "x").expect("extra");
            Ok(())
        };
        let remove = |stage: &Path| -> Result<(), ProviderRenderError> {
            fs::remove_file(stage.join("LICENSE")).expect("remove");
            Ok(())
        };
        let checks: [StageCheck<'_>; 3] = [&modify, &add, &remove];
        for check in checks {
            let err = render_provider_generation(&alpha, &fx.providers_root, check)
                .expect_err("validation must fail");
            assert!(
                matches!(err, ProviderRenderError::Validation { .. }),
                "{err:?}"
            );
            assert_eq!(
                dir_names(&provider_dir),
                [old.generation.generation.to_string()]
            );
            assert_eq!(snapshot_of(&old.path), old_bytes);
            assert!(stage_residue(&provider_dir).is_empty());
        }
    }

    #[test]
    fn provider_render_concurrent_identical_publish_is_reused() {
        let (fx, inventory) = two_provider_fixture();
        let alpha = inventory.providers[0].clone();
        let final_dir = expected_generation_dir(&alpha, &fx.providers_root);
        let race = |stage: &Path| -> Result<(), ProviderRenderError> {
            for (rel, bytes) in snapshot_of(stage) {
                write_bytes(&final_dir.join(rel), &bytes);
            }
            Ok(())
        };
        let rendered =
            render_provider_generation(&alpha, &fx.providers_root, &race).expect("render");
        assert!(rendered.reused);
        assert_eq!(rendered.path, final_dir);
        let provider_dir = fx.providers_root.join("alpha");
        assert_eq!(dir_names(&provider_dir).len(), 1);
        assert!(stage_residue(&provider_dir).is_empty());
    }

    #[test]
    fn provider_render_concurrent_different_publish_is_mismatch_and_untouched() {
        let (fx, inventory) = two_provider_fixture();
        let alpha = inventory.providers[0].clone();
        let final_dir = expected_generation_dir(&alpha, &fx.providers_root);
        let race = |_: &Path| -> Result<(), ProviderRenderError> {
            write(&final_dir.join("foreign.txt"), "foreign");
            Ok(())
        };
        let err =
            render_provider_generation(&alpha, &fx.providers_root, &race).expect_err("mismatch");
        assert!(
            matches!(err, ProviderRenderError::GenerationMismatch { .. }),
            "{err:?}"
        );
        assert_eq!(dir_names(&final_dir), ["foreign.txt"]);
        assert_eq!(
            fs::read_to_string(final_dir.join("foreign.txt")).expect("read"),
            "foreign"
        );
        assert!(stage_residue(&fx.providers_root.join("alpha")).is_empty());
    }

    #[test]
    fn provider_render_preexisting_stage_residue_is_never_published() {
        let (fx, inventory) = two_provider_fixture();
        let provider_dir = fx.providers_root.join("alpha");
        let residue = provider_dir.join(format!("{STAGE_PREFIX}deadbeef"));
        write(&residue.join("half.txt"), "partial");
        let rendered =
            render_provider_generations_at(&inventory, &fx.providers_root).expect("render");
        assert!(!rendered[0].reused);
        assert_ne!(rendered[0].path, residue);
        assert!(!rendered[0].path.join("half.txt").exists());
        let generations: Vec<String> = dir_names(&provider_dir)
            .into_iter()
            .filter(|n| !n.starts_with(STAGE_PREFIX))
            .collect();
        assert_eq!(generations, [rendered[0].generation.generation.to_string()]);
    }

    #[test]
    fn provider_render_tampered_generation_extra_or_missing_file_is_mismatch() {
        let (fx, inventory) = two_provider_fixture();
        let rendered =
            render_provider_generations_at(&inventory, &fx.providers_root).expect("render");
        let alpha = &rendered[0].path;
        write(&alpha.join("injected.md"), "x");
        assert!(matches!(
            render_provider_generations_at(&inventory, &fx.providers_root),
            Err(ProviderRenderError::GenerationMismatch { .. })
        ));
        fs::remove_file(alpha.join("injected.md")).expect("rm");
        fs::remove_file(alpha.join("agents/helper.md")).expect("rm");
        assert!(matches!(
            render_provider_generations_at(&inventory, &fx.providers_root),
            Err(ProviderRenderError::GenerationMismatch { .. })
        ));
        assert!(!alpha.join("agents/helper.md").exists(), "never repaired");
    }

    #[test]
    fn provider_render_link_inside_payload_fails_closed() {
        let fx = fixture();
        let outside = fx.plugins_root.parent().expect("base").join("outside");
        write(&outside.join("secret.txt"), "secret");
        write_plugin(&fx.plugins_root.join("nested"), "nested", "n");
        write_plugin(&fx.plugins_root.join("top"), "top", "t");
        if !try_link(
            &outside,
            &fx.plugins_root
                .join("nested")
                .join("skills")
                .join("shared")
                .join("linked"),
        ) || !try_link(&outside, &fx.plugins_root.join("top").join("linked"))
        {
            eprintln!("directory links unavailable; skipping");
            return;
        }
        let lock = bundled_lock(&["nested", "top"]);
        let inventory =
            build_provider_inventory(&lock, &fx.plugins_root, &fx.cache_root).expect("inventory");
        for entry in &inventory.providers {
            let err = render_provider_generation(entry, &fx.providers_root, &|_| Ok(()))
                .expect_err("link must fail closed");
            assert!(
                matches!(err, ProviderRenderError::Payload { .. }),
                "{}: {err:?}",
                entry.plugin_id
            );
            assert!(!fx.providers_root.join(entry.key.as_str()).exists());
        }
        assert_eq!(dir_names(&outside), ["secret.txt"]);
    }

    #[test]
    fn provider_render_linked_provider_or_generation_dir_is_refused() {
        let (fx, inventory) = two_provider_fixture();
        let alpha = inventory.providers[0].clone();
        let outside = fx.plugins_root.parent().expect("base").join("outside");
        fs::create_dir_all(&outside).expect("mkdir");
        if !try_link(&outside, &fx.providers_root.join("alpha")) {
            eprintln!("directory links unavailable; skipping");
            return;
        }
        let err = render_provider_generation(&alpha, &fx.providers_root, &|_| Ok(()))
            .expect_err("linked provider dir");
        assert!(
            matches!(err, ProviderRenderError::LinkedPath { .. }),
            "{err:?}"
        );
        assert!(dir_names(&outside).is_empty(), "nothing written via link");
        ccync_foundation::platform::remove_dir_link(&fx.providers_root.join("alpha"))
            .expect("unlink");

        // A generation directory that is a link to identical content is not
        // accepted as a published generation.
        let identical = fx.plugins_root.parent().expect("base").join("identical");
        let source = snapshot_payload(&alpha.plugin_id, &alpha.payload_dir).expect("snapshot");
        for (rel, bytes) in &source {
            write_bytes(&identical.join(rel), bytes);
        }
        let final_dir = expected_generation_dir(&alpha, &fx.providers_root);
        assert!(try_link(&identical, &final_dir));
        let err = render_provider_generation(&alpha, &fx.providers_root, &|_| Ok(()))
            .expect_err("linked generation dir");
        assert!(
            matches!(err, ProviderRenderError::LinkedPath { .. }),
            "{err:?}"
        );
        assert_eq!(snapshot_of(&identical), source);
    }

    #[test]
    fn provider_render_git_excluded_only_at_top_level() {
        let fx = fixture();
        let root = fx.plugins_root.join("g");
        write(&root.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
        write(&root.join(".git"), "gitdir: ../.git/modules/g\n");
        write(&root.join("skills/s/.git/config"), "nested");
        write(&root.join("vendor/lib/.git"), "gitdir: x\n");
        write(&root.join(".github/workflows/ci.yml"), "on: push\n");
        write(&root.join(".gitignore"), "target\n");
        let inventory =
            build_provider_inventory(&bundled_lock(&["g"]), &fx.plugins_root, &fx.cache_root)
                .expect("inventory");
        let rendered =
            render_provider_generations_at(&inventory, &fx.providers_root).expect("render");
        let path = &rendered[0].path;
        assert!(!path.join(".git").exists());
        for kept in [
            "skills/s/.git/config",
            "vendor/lib/.git",
            ".github/workflows/ci.yml",
            ".gitignore",
        ] {
            assert!(path.join(kept).is_file(), "{kept}");
        }
    }

    #[test]
    fn provider_render_payload_with_only_empty_dirs_is_empty() {
        let fx = fixture();
        fs::create_dir_all(fx.plugins_root.join("hollow/skills/s")).expect("mkdir");
        fs::create_dir_all(fx.plugins_root.join("hollow/.git/objects")).expect("mkdir");
        write(&fx.plugins_root.join("hollow/.git/HEAD"), "ref\n");
        let inventory =
            build_provider_inventory(&bundled_lock(&["hollow"]), &fx.plugins_root, &fx.cache_root)
                .expect("inventory");
        assert!(matches!(
            render_provider_generations_at(&inventory, &fx.providers_root),
            Err(ProviderRenderError::EmptyPayload { .. })
        ));
        assert!(!fx.providers_root.exists());
    }

    #[test]
    fn provider_render_key_conflicts_name_both_records_even_without_payloads() {
        let fx = fixture();
        let cases = [
            json!({"_personalPlugins": [
                {"pluginId": "Foo", "source": "https://example.invalid/a.git"},
                {"pluginId": "foo", "source": "https://example.invalid/b.git"}
            ]}),
            json!({"resolvedPlugins": [
                {"pluginId": "Foo", "resolvedSource": "Foo", "installStrategy": "bundled-local"},
                {"pluginId": "FOO", "resolvedSource": "FOO", "installStrategy": "bundled-local"}
            ]}),
            json!({"resolvedPlugins": [
                {"pluginId": "foo", "resolvedSource": "foo", "installStrategy": "bundled-local"}
            ], "_personalPlugins": [
                {"pluginId": "fOo", "source": "https://example.invalid/c.git", "pin": "p"}
            ]}),
        ];
        for lock in cases {
            let err = build_provider_inventory(&lock, &fx.plugins_root, &fx.cache_root)
                .expect_err("conflict");
            let ProviderRenderError::SourceKeyConflict { key, first, second } = &err else {
                panic!("unexpected {err:?}");
            };
            assert_eq!(key, "foo");
            assert_ne!(first, second);
            let text = err.to_string();
            assert!(text.contains(first.as_str()) && text.contains(second.as_str()));
        }
        assert!(!fx.providers_root.exists());
    }

    #[test]
    fn provider_render_unavailable_payload_keeps_prior_generation() {
        let (fx, inventory) = two_provider_fixture();
        let rendered =
            render_provider_generations_at(&inventory, &fx.providers_root).expect("render");
        let alpha_old = snapshot_of(&rendered[0].path);
        let beta_old = snapshot_of(&rendered[1].path);

        // Bundle directory gone; personal pin moved to a missing cache dir.
        fs::remove_dir_all(fx.plugins_root.join("alpha")).expect("rm");
        let lock = json!({
            "resolvedPlugins": [
                {"pluginId": "alpha", "resolvedSource": "alpha", "installStrategy": "bundled-local"},
                {"pluginId": "file", "resolvedSource": "file", "installStrategy": "bundled-local"}
            ],
            "_personalPlugins": [
                {"pluginId": "Beta", "source": "https://example.invalid/beta.git", "pin": "b3"}
            ]
        });
        write(&fx.plugins_root.join("file"), "not a directory");
        let later =
            build_provider_inventory(&lock, &fx.plugins_root, &fx.cache_root).expect("inventory");
        assert!(later.providers.is_empty());
        let ids: Vec<&str> = later
            .unavailable
            .iter()
            .map(|u| u.plugin_id.as_str())
            .collect();
        assert_eq!(ids, ["alpha", "Beta", "file"]);
        assert!(later.unavailable.iter().all(|u| !u.reason.is_empty()));

        let none = render_provider_generations_at(&later, &fx.providers_root).expect("render");
        assert!(none.is_empty());
        assert_eq!(snapshot_of(&rendered[0].path), alpha_old);
        assert_eq!(snapshot_of(&rendered[1].path), beta_old);
    }

    #[test]
    fn provider_render_writes_only_inside_providers_root() {
        let (fx, inventory) = two_provider_fixture();
        let base = fx.plugins_root.parent().expect("base").to_path_buf();
        let plugins_before = snapshot_of(&fx.plugins_root);
        let cache_before = snapshot_of(&fx.cache_root);
        render_provider_generations_at(&inventory, &fx.providers_root).expect("render");
        assert_eq!(snapshot_of(&fx.plugins_root), plugins_before);
        assert_eq!(snapshot_of(&fx.cache_root), cache_before);
        assert_eq!(dir_names(&base), ["build", "cache", "plugins"]);
        assert_eq!(dir_names(&base.join("build")), ["render"]);
        assert_eq!(dir_names(&base.join("build/render")), ["providers"]);
        assert_eq!(dir_names(fx._tmp.path()), [".ccync"]);
    }

    // ── native package views ────────────────────────────────────────────────

    const DEMO_VERSION: &str = "0.3.0-g25e1012dda86";

    const UPSTREAM_SIDECAR: &str = "\
# upstream sidecar
interface:
  display_name: Pipeline
  short_description: Runs tasks end to end
  brand_color: \"#123456\"
policy:
  allow_implicit_invocation: false
  x_future_policy:
    mode: strict
dependencies:
  tools:
    - type: mcp
      value: srv
";

    /// A DEMO-shaped plugin: three manifests, legacy Codex hooks, Claude
    /// default hooks with a helper, both MCP files, skills with and without a
    /// sidecar, flat and directory commands, agents, and non-component noise.
    fn write_demo_like_plugin(root: &Path) {
        write(
            &root.join("plugin.json"),
            &json!({"$schema": "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json",
                    "name": "demo", "version": DEMO_VERSION, "description": "DEMO"})
            .to_string(),
        );
        write(
            &root.join(".claude-plugin/plugin.json"),
            &json!({"name": "demo", "version": DEMO_VERSION, "description": "DEMO",
                    "mcpServers": "./.mcp.json"})
            .to_string(),
        );
        write(
            &root.join(".codex-plugin/plugin.json"),
            &json!({
                "name": "demo", "version": DEMO_VERSION, "author": {"name": "DEMO"},
                "x-unknown-policy": {"keep": true},
                "hooks": {"hooks": {
                    "PreToolUse": [{"hooks": [{"type": "command", "command": "demo hook pre"}]}],
                    "Stop": [{"hooks": [{"type": "command", "command": "demo hook stop"}]}]
                }},
                "interface": {"displayName": "Demo Agents Legion",
                              "shortDescription": "DEMO workflow commands"}
            })
            .to_string(),
        );
        write(
            &root.join(".mcp.json"),
            r#"{"mcpServers":{"srv":{"command":"npx"}}}"#,
        );
        write(&root.join("mcp.json"), r#"{"mcpServers":{}}"#);
        write(
            &root.join("skills/pipeline/SKILL.md"),
            "---\nname: pipeline\ndescription: \"Run the DEMO pipeline. Iterates tasks.\"\n---\nRead [ref](references/r.md).\n",
        );
        write(&root.join("skills/pipeline/references/r.md"), "ref\n");
        write(
            &root.join("skills/pipeline/agents/openai.yaml"),
            UPSTREAM_SIDECAR,
        );
        write(
            &root.join("skills/plain/SKILL.md"),
            "---\nname: plain\ndescription: Plain helper skill\n---\nbody\n",
        );
        write(&root.join("skills/notaskill/README.md"), "not a skill\n");
        write(
            &root.join("skills/.hidden/SKILL.md"),
            "---\nname: hidden\n---\n",
        );
        write(&root.join("commands/run.md"), "Run things\n");
        write(
            &root.join("commands/wf.md"),
            "---\ndescription: wf flat\n---\nflat\n",
        );
        write(
            &root.join("commands/wf/SKILL.md"),
            "---\nname: wf\ndescription: wf dir\n---\ndir\n",
        );
        write(&root.join("commands/wf/assets/a.txt"), "asset\n");
        write(&root.join("commands/notes.txt"), "notes\n");
        write(&root.join("commands/.draft.md"), "draft\n");
        write(&root.join("agents/helper.md"), "agent\n");
        write(&root.join("agents/readme.txt"), "readme\n");
        write(
            &root.join("hooks/hooks.json"),
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"${CLAUDE_PLUGIN_ROOT}/scripts/start.sh"}]}]}}"#,
        );
        write(&root.join("scripts/start.sh"), "echo start\n");
    }

    /// Render one bundled provider and return its generation.
    fn render_bundled(fx: &Fixture, id: &str) -> RenderedProviderGeneration {
        let inventory =
            build_provider_inventory(&bundled_lock(&[id]), &fx.plugins_root, &fx.cache_root)
                .expect("inventory");
        let mut rendered =
            render_provider_generations_at(&inventory, &fx.providers_root).expect("render");
        rendered.remove(0)
    }

    fn demo_fixture() -> (Fixture, RenderedProviderGeneration) {
        let fx = fixture();
        write_demo_like_plugin(&fx.plugins_root.join("demo"));
        let rendered = render_bundled(&fx, "demo");
        (fx, rendered)
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).expect("read")).expect("json")
    }

    fn read_yaml(path: &Path) -> serde_yaml_ng::Value {
        serde_yaml_ng::from_str(&fs::read_to_string(path).expect("read")).expect("yaml")
    }

    fn kinds(components: &[ComponentIdentity]) -> BTreeSet<(ComponentKind, String)> {
        components
            .iter()
            .map(|c| (c.kind, c.component_id.clone()))
            .collect()
    }

    fn set(items: &[(ComponentKind, &str)]) -> BTreeSet<(ComponentKind, String)> {
        items
            .iter()
            .map(|(kind, id)| (*kind, (*id).to_string()))
            .collect()
    }

    #[test]
    fn provider_render_components_follow_component_rules() {
        let (_fx, rendered) = demo_fixture();
        use ComponentKind::{Agent, Command, Hook, Skill};
        assert_eq!(
            kinds(&rendered.components),
            set(&[
                (Skill, "pipeline"),
                (Skill, "plain"),
                (Command, "run"),
                (Command, "wf"),
                (Agent, "helper"),
                (Hook, "PreToolUse"),
                (Hook, "SessionStart"),
                (Hook, "Stop"),
            ])
        );
        assert!(rendered
            .components
            .iter()
            .all(|c| c.plugin_name == "demo" && c.source.as_str() == "demo"));
    }

    #[test]
    fn provider_render_bare_skill_with_sidecar_stays_one_skill() {
        let fx = fixture();
        let root = fx.plugins_root.join("solo");
        write(
            &root.join("SKILL.md"),
            "---\nname: solo\ndescription: d\n---\n",
        );
        write(&root.join("agents/openai.yaml"), "interface: {}\n");
        let rendered = render_bundled(&fx, "solo");
        assert_eq!(
            kinds(&rendered.components),
            set(&[(ComponentKind::Skill, "solo")])
        );
    }

    #[test]
    fn provider_render_malformed_hook_manifest_fails_before_publication() {
        let fx = fixture();
        let root = fx.plugins_root.join("hooky");
        write(&root.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
        write(&root.join("hooks/hooks.json"), "{not json");
        let inventory =
            build_provider_inventory(&bundled_lock(&["hooky"]), &fx.plugins_root, &fx.cache_root)
                .expect("inventory");
        assert!(matches!(
            render_provider_generations_at(&inventory, &fx.providers_root),
            Err(ProviderRenderError::InvalidHookDeclaration { .. })
        ));
        assert!(!fx.providers_root.exists());
    }

    #[test]
    fn provider_package_codex_view_keeps_identity_and_unknown_fields() {
        let (_fx, rendered) = demo_fixture();
        let generation_before = snapshot_of(&rendered.path);
        let view = generate_native_package_view(&rendered, NativePackageTarget::Codex)
            .expect("codex view");

        // Location: sibling of the immutable generation, never inside it.
        let views_dir = native_package_views_dir(&rendered.path);
        assert_eq!(
            views_dir.file_name().and_then(|n| n.to_str()),
            Some(format!("{}.views", rendered.generation.generation).as_str())
        );
        assert_eq!(view.path, views_dir.join("codex").join(&view.view_id));
        assert!(view.view_id.starts_with("v-"));
        assert_eq!(snapshot_of(&rendered.path), generation_before);
        assert!(rendered.path.join(".mcp.json").is_file(), "source MCP kept");

        // Identity and version.
        assert_eq!(view.plugin_name, "demo");
        assert_eq!(view.upstream_version.as_deref(), Some(DEMO_VERSION));
        let suffix = native_version_suffix(
            NativePackageTarget::Codex,
            &rendered.generation.package_digest,
        );
        assert_eq!(view.version, format!("{DEMO_VERSION}+{suffix}"));
        assert!(semver::Version::parse(&view.version).is_ok());
        for manifest in [CODEX_MANIFEST, ROOT_MANIFEST, CLAUDE_MANIFEST] {
            let value = read_json(&view.path.join(manifest));
            assert_eq!(value["name"], "demo", "{manifest}");
            assert_eq!(value["version"], view.version.as_str(), "{manifest}");
            assert!(value.get("mcpServers").is_none(), "{manifest}");
        }
        let codex = read_json(&view.path.join(CODEX_MANIFEST));
        assert_eq!(codex["x-unknown-policy"], json!({"keep": true}));
        assert_eq!(codex["author"], json!({"name": "DEMO"}));
        assert!(
            codex.get("hooks").is_none(),
            "hook declaration left the view"
        );
        assert_eq!(codex["interface"]["displayName"], "Demo Agents Legion");
        assert_eq!(
            codex["interface"]["shortDescription"],
            "DEMO workflow commands (managed by ccync)"
        );

        // Native MCP entrypoints are absent.
        assert!(!view.path.join(".mcp.json").exists());
        assert!(!view.path.join("mcp.json").exists());
        assert_eq!(
            view.removed_mcp_entrypoints,
            [
                ".claude-plugin/plugin.json#mcpServers",
                ".mcp.json",
                "mcp.json"
            ]
        );

        // Sidecars: identity, marker once, every other field preserved.
        let sidecar_path = view.path.join("skills/pipeline/agents/openai.yaml");
        let sidecar_text = fs::read_to_string(&sidecar_path).expect("sidecar");
        assert_eq!(sidecar_text.matches(MANAGEMENT_MARKER).count(), 1);
        let sidecar = read_yaml(&sidecar_path);
        assert_eq!(sidecar["interface"]["display_name"], "Pipeline");
        assert_eq!(
            sidecar["interface"]["short_description"],
            "Runs tasks end to end (managed by ccync)"
        );
        assert_eq!(sidecar["interface"]["brand_color"], "#123456");
        assert_eq!(sidecar["policy"]["allow_implicit_invocation"], false);
        assert_eq!(sidecar["policy"]["x_future_policy"]["mode"], "strict");
        assert_eq!(sidecar["dependencies"]["tools"][0]["value"], "srv");
        let plain = read_yaml(&view.path.join("skills/plain/agents/openai.yaml"));
        assert_eq!(plain["interface"]["display_name"], "demo:plain");
        assert_eq!(
            plain["interface"]["short_description"],
            "Plain helper skill (managed by ccync)"
        );

        // Routing descriptions and resources are untouched.
        for rel in [
            "skills/pipeline/SKILL.md",
            "skills/pipeline/references/r.md",
        ] {
            assert_eq!(
                fs::read(view.path.join(rel)).expect("view"),
                fs::read(rendered.path.join(rel)).expect("generation"),
                "{rel}"
            );
        }

        // Commands become skills exactly once; no command form remains.
        assert_eq!(view.converted_commands, ["run", "wf"]);
        assert_eq!(view.dropped_command_forms, ["wf"]);
        let run = fs::read_to_string(view.path.join("skills/run/SKILL.md")).expect("run");
        assert!(run.starts_with("---\nname: \"run\"\ndescription: \"demo command run\"\n---\n"));
        assert!(run.ends_with("Run things\n"));
        let wf = fs::read_to_string(view.path.join("skills/wf/SKILL.md")).expect("wf");
        assert!(wf.contains("wf dir") && !wf.contains("wf flat"));
        assert!(view.path.join("skills/wf/assets/a.txt").is_file());
        assert!(!view.path.join("commands/run.md").exists());
        assert!(!view.path.join("commands/wf.md").exists());
        assert!(!view.path.join("commands/wf").exists());
        assert!(
            view.path.join("commands/notes.txt").is_file(),
            "not a command"
        );

        // Hooks are not active in the view: the legacy manifest declaration and
        // the default hook file leave it; the helper script stays.
        assert_eq!(
            view.removed_hook_entrypoints,
            [".codex-plugin/plugin.json#hooks", "hooks/hooks.json"]
        );
        assert!(!view.path.join("hooks/hooks.json").exists());
        assert!(view.path.join("scripts/start.sh").is_file());
        use ComponentKind::{Command, Skill};
        assert_eq!(
            kinds(&view.components),
            set(&[
                (Skill, "pipeline"),
                (Skill, "plain"),
                (Command, "run"),
                (Command, "wf"),
            ])
        );
        assert!(view.components.iter().all(|c| c.plugin_name == "demo"));
    }

    #[test]
    fn provider_package_claude_view_keeps_commands_and_drops_native_mcp() {
        let (_fx, rendered) = demo_fixture();
        let view = generate_native_package_view(&rendered, NativePackageTarget::Claude)
            .expect("claude view");
        assert_eq!(
            view.path,
            native_package_views_dir(&rendered.path)
                .join("claude")
                .join(&view.view_id)
        );
        let claude = read_json(&view.path.join(CLAUDE_MANIFEST));
        assert_eq!(claude["name"], "demo");
        assert_eq!(claude["description"], "DEMO");
        assert!(claude.get("mcpServers").is_none());
        assert!(view.version.starts_with(&format!("{DEMO_VERSION}+ccync.")));
        assert!(!view.path.join(".mcp.json").exists());

        // Claude loads commands natively: flat forms stay, the duplicate
        // directory form of `wf` is not published, nothing becomes a skill.
        assert!(view.converted_commands.is_empty());
        assert_eq!(view.dropped_command_forms, ["wf"]);
        assert!(view.path.join("commands/run.md").is_file());
        assert!(view.path.join("commands/wf.md").is_file());
        assert!(!view.path.join("commands/wf/SKILL.md").exists());
        assert!(view.path.join("commands/wf/assets/a.txt").is_file());
        assert!(!view.path.join("skills/run").exists());

        // Codex sidecars are not generated for Claude; upstream ones are kept.
        assert!(!view.path.join("skills/plain/agents/openai.yaml").exists());
        assert_eq!(
            fs::read_to_string(view.path.join("skills/pipeline/agents/openai.yaml"))
                .expect("sidecar"),
            UPSTREAM_SIDECAR
        );

        use ComponentKind::{Agent, Command, Skill};
        assert_eq!(
            kinds(&view.components),
            set(&[
                (Skill, "pipeline"),
                (Skill, "plain"),
                (Command, "run"),
                (Command, "wf"),
                (Agent, "helper"),
            ])
        );
        assert_eq!(
            view.removed_hook_entrypoints,
            [".codex-plugin/plugin.json#hooks", "hooks/hooks.json"]
        );
        assert!(!view.path.join("hooks/hooks.json").exists());

        // Distinct targets get distinct deterministic versions.
        let codex = generate_native_package_view(&rendered, NativePackageTarget::Codex)
            .expect("codex view");
        assert_ne!(codex.version, view.version);
        assert_eq!(
            dir_names(&native_package_views_dir(&rendered.path)),
            ["claude", "codex"]
        );
    }

    #[test]
    fn provider_package_sidecar_marker_appears_once_and_fits_limit() {
        let long = "Coordinate a very long multi stage review across every task and \
                    every subagent in the workspace";
        let cases: [(Option<&str>, Option<&str>); 5] = [
            (None, Some("Plain purpose")),
            (
                Some("interface:\n  short_description: Already (managed by ccync)\n"),
                None,
            ),
            (
                Some("interface:\n  short_description: Managed by ccync - Mixed case\n"),
                None,
            ),
            (
                Some("interface:\n  short_description: managed by ccync\n"),
                Some("Fallback"),
            ),
            (Some(""), Some(long)),
        ];
        for (existing, fallback) in cases {
            let once = update_skill_sidecar(
                "p",
                "skills/s/agents/openai.yaml",
                existing.map(str::as_bytes),
                "prov",
                "s",
                fallback,
            )
            .expect("first update");
            let twice = update_skill_sidecar(
                "p",
                "skills/s/agents/openai.yaml",
                Some(&once),
                "prov",
                "s",
                fallback,
            )
            .expect("second update");
            assert_eq!(once, twice, "idempotent for {existing:?}");
            let value: serde_yaml_ng::Value = serde_yaml_ng::from_slice(&once).expect("yaml");
            let short = value["interface"]["short_description"]
                .as_str()
                .expect("short description");
            assert_eq!(
                short
                    .to_ascii_lowercase()
                    .matches(MANAGEMENT_MARKER)
                    .count(),
                1,
                "{short}"
            );
            assert!(
                short.chars().count() <= SKILL_SHORT_DESCRIPTION_MAX_CHARS,
                "{short}"
            );
            assert_eq!(value["interface"]["display_name"], "prov:s");
        }
        let truncated = with_management_marker(long, Some(SKILL_SHORT_DESCRIPTION_MAX_CHARS));
        assert!(truncated.ends_with("... (managed by ccync)"), "{truncated}");
        assert_eq!(
            with_management_marker("Mixed case - managed by CCYNC", Some(64)),
            "Mixed case (managed by ccync)"
        );
        assert_eq!(
            with_management_marker("DEMO tools (managed by ccync)", None),
            "DEMO tools (managed by ccync)"
        );
    }

    #[test]
    fn provider_package_yaml_scanner_accepts_quoted_and_block_text() {
        let accepted = [
            "interface:\n  short_description: \"*starts with a star\"\n",
            "interface:\n  short_description: '&amp; and ''quotes'''\n",
            "notes: |\n  * a markdown bullet\n  & ampersand\n  !bang\nnext: ok\n",
            "folded: >-\n  *still text\nlist:\n  - a*b\n  - c&d\n",
            "# comment with *alias &anchor !tag\nkey: value # trailing &x\n",
            "url: http://x.test/a!b\n",
            "---\nkey: value\n",
        ];
        for text in accepted {
            assert!(
                reject_unsupported_yaml_syntax(text).is_ok(),
                "should accept: {text}"
            );
        }
        let rejected = [
            "base: &base\n  a: 1\n",
            "copy: *base\n",
            "items: [a, *base]\n",
            "- &x item\n",
            "value: !custom 1\n",
            "value: !!str 1\n",
            "--- !tag\nkey: v\n",
            "%YAML 1.2\n---\nkey: v\n",
            "map: {a: &x 1}\n",
        ];
        for text in rejected {
            assert!(
                reject_unsupported_yaml_syntax(text).is_err(),
                "should reject: {text}"
            );
        }
    }

    #[test]
    fn provider_package_unsupported_yaml_leaves_prior_view_unchanged() {
        let fx = fixture();
        let root = fx.plugins_root.join("demo");
        write_demo_like_plugin(&root);
        let first = render_bundled(&fx, "demo");
        let prior =
            generate_native_package_view(&first, NativePackageTarget::Codex).expect("prior view");
        let prior_views = native_package_views_dir(&first.path);
        let prior_bytes = snapshot_of(&prior_views);

        let sidecar = root.join("skills/pipeline/agents/openai.yaml");
        let cases = [
            "interface: [unclosed\n",
            "base: &base\n  short_description: x\ninterface: *base\n",
            "base: {short_description: x}\ninterface:\n  <<: {display_name: y}\n",
            "interface: !custom\n  short_description: x\n",
            "%YAML 1.2\n---\ninterface: {}\n",
            "- not\n- a mapping\n",
            "interface: just text\n",
            "interface:\n  short_description: [1, 2]\n",
            "a: 1\n---\nb: 2\n",
        ];
        for text in cases {
            write(&sidecar, text);
            let rendered = render_bundled(&fx, "demo");
            assert_ne!(rendered.generation.generation, first.generation.generation);
            let err = generate_native_package_view(&rendered, NativePackageTarget::Codex)
                .expect_err("unsupported sidecar");
            match &err {
                ProviderRenderError::InvalidSidecar { path, .. } => {
                    assert_eq!(path, "skills/pipeline/agents/openai.yaml", "{text}");
                }
                other => panic!("{text}: unexpected {other:?}"),
            }
            assert!(
                !native_package_views_dir(&rendered.path).exists(),
                "nothing written for {text}"
            );
            assert_eq!(snapshot_of(&prior_views), prior_bytes, "{text}");
            // Claude does not rewrite Codex sidecars, so it still builds.
            assert!(generate_native_package_view(&rendered, NativePackageTarget::Claude).is_ok());
        }
        assert!(prior.path.is_dir());
    }

    #[test]
    fn provider_package_version_suffix_is_deterministic_and_validated() {
        let (_fx, rendered) = demo_fixture();
        let first =
            generate_native_package_view(&rendered, NativePackageTarget::Codex).expect("first");
        let before = snapshot_of(&first.path);
        let second =
            generate_native_package_view(&rendered, NativePackageTarget::Codex).expect("second");
        assert!(!first.reused && second.reused);
        assert_eq!(first.version, second.version);
        assert_eq!(first.view_id, second.view_id);
        assert_eq!(first.view_digest, second.view_digest);
        assert_eq!(snapshot_of(&second.path), before);
        assert_eq!(
            dir_names(&native_package_views_dir(&rendered.path).join("codex")),
            [first.view_id.as_str()]
        );

        let suffix = "ccync.0123456789ab";
        assert_eq!(
            native_package_version(None, suffix).as_deref(),
            Ok("0.0.0+ccync.0123456789ab")
        );
        assert_eq!(
            native_package_version(Some("1.2.3+build.5"), suffix).as_deref(),
            Ok("1.2.3+build.5.ccync.0123456789ab")
        );
        assert_eq!(
            native_package_version(Some("1.2.3-rc.1"), suffix).as_deref(),
            Ok("1.2.3-rc.1+ccync.0123456789ab")
        );
        for bad in ["1.0", "v1.2.3", "latest", ""] {
            assert!(native_package_version(Some(bad), suffix).is_err(), "{bad}");
        }
        let long = format!("1.0.0-{}", "a".repeat(40));
        assert!(native_package_version(Some(&long), suffix).is_err());
    }

    #[test]
    fn provider_package_invalid_upstream_version_blocks_view() {
        let fx = fixture();
        let root = fx.plugins_root.join("loose");
        write(
            &root.join(".claude-plugin/plugin.json"),
            r#"{"name":"loose","version":"1.0"}"#,
        );
        write(&root.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
        let rendered = render_bundled(&fx, "loose");
        for target in [NativePackageTarget::Codex, NativePackageTarget::Claude] {
            assert!(matches!(
                generate_native_package_view(&rendered, target),
                Err(ProviderRenderError::InvalidNativeVersion { .. })
            ));
        }
        assert!(!native_package_views_dir(&rendered.path).exists());
    }

    #[test]
    fn provider_package_blocks_mcp_declarations_outside_the_centralized_projection() {
        let cases = [
            json!("./config/servers.json"),
            json!({"srv": {"command": "npx"}}),
            json!(["./.mcp.json"]),
        ];
        for (index, declaration) in cases.into_iter().enumerate() {
            let fx = fixture();
            let id = format!("mcp{index}");
            let root = fx.plugins_root.join(&id);
            write(
                &root.join(".claude-plugin/plugin.json"),
                &json!({"name": id, "version": "1.0.0", "mcpServers": declaration}).to_string(),
            );
            write(&root.join("config/servers.json"), r#"{"mcpServers":{}}"#);
            write(&root.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
            let rendered = render_bundled(&fx, &id);
            for target in [NativePackageTarget::Codex, NativePackageTarget::Claude] {
                let err = generate_native_package_view(&rendered, target)
                    .expect_err("ambiguous MCP declaration");
                assert!(
                    matches!(
                        &err,
                        ProviderRenderError::UnsupportedNativeDeclaration { target: t, .. }
                            if *t == target
                    ),
                    "{err:?}"
                );
            }
            assert!(!native_package_views_dir(&rendered.path).exists());
        }

        // An empty inline object or the root file path is a removable entrypoint.
        let fx = fixture();
        let root = fx.plugins_root.join("empty");
        write(
            &root.join(".claude-plugin/plugin.json"),
            r#"{"name":"empty","version":"1.0.0","mcpServers":{}}"#,
        );
        write(
            &root.join(".codex-plugin/plugin.json"),
            r#"{"name":"empty","version":"1.0.0","mcpServers":"mcp.json"}"#,
        );
        write(&root.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
        let rendered = render_bundled(&fx, "empty");
        let view =
            generate_native_package_view(&rendered, NativePackageTarget::Codex).expect("view");
        assert_eq!(
            view.removed_mcp_entrypoints,
            [
                ".claude-plugin/plugin.json#mcpServers",
                ".codex-plugin/plugin.json#mcpServers"
            ]
        );
    }

    #[test]
    fn provider_package_rejects_identity_conflicts_and_invalid_names() {
        let fx = fixture();
        let root = fx.plugins_root.join("dual");
        write(
            &root.join(".claude-plugin/plugin.json"),
            r#"{"name":"dual","version":"1.0.0"}"#,
        );
        write(
            &root.join(".codex-plugin/plugin.json"),
            r#"{"name":"ccync","version":"1.0.0"}"#,
        );
        write(&root.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
        let rendered = render_bundled(&fx, "dual");
        assert!(matches!(
            generate_native_package_view(&rendered, NativePackageTarget::Codex),
            Err(ProviderRenderError::ManifestConflict { .. })
        ));

        let root = fx.plugins_root.join("skew");
        write(
            &root.join(".claude-plugin/plugin.json"),
            r#"{"name":"skew","version":"1.0.0"}"#,
        );
        write(
            &root.join("plugin.json"),
            r#"{"name":"skew","version":"2.0.0"}"#,
        );
        write(&root.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
        let rendered = render_bundled(&fx, "skew");
        assert!(matches!(
            generate_native_package_view(&rendered, NativePackageTarget::Claude),
            Err(ProviderRenderError::ManifestConflict { .. })
        ));

        let root = fx.plugins_root.join("spaced");
        write(
            &root.join(".claude-plugin/plugin.json"),
            r#"{"name":"my plugin","version":"1.0.0"}"#,
        );
        write(&root.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
        let rendered = render_bundled(&fx, "spaced");
        assert_eq!(rendered.plugin_name, "my plugin");
        assert!(matches!(
            generate_native_package_view(&rendered, NativePackageTarget::Codex),
            Err(ProviderRenderError::UnsupportedNativeDeclaration { .. })
        ));
        for id in ["dual", "skew", "spaced"] {
            let provider = fx.providers_root.join(id);
            assert!(
                dir_names(&provider).iter().all(|n| !n.ends_with(".views")),
                "{id}"
            );
        }
    }

    #[test]
    fn provider_package_blocks_unresolved_hooks() {
        let fx = fixture();
        let root = fx.plugins_root.join("hooks");
        write(
            &root.join(".claude-plugin/plugin.json"),
            r#"{"name":"hooks","version":"1.0.0","hooks":"./hooks/extra.json"}"#,
        );
        write(
            &root.join("hooks/extra.json"),
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"${CLAUDE_PLUGIN_ROOT}/scripts/missing.sh --flag"}]}]}}"#,
        );
        write(&root.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
        let rendered = render_bundled(&fx, "hooks");
        let err = generate_native_package_view(&rendered, NativePackageTarget::Claude)
            .expect_err("missing helper");
        assert!(
            matches!(&err, ProviderRenderError::InvalidHookDeclaration { reason, .. }
                if reason.contains("scripts/missing.sh")),
            "{err:?}"
        );
        // Codex does not read the Claude manifest hooks; its view builds.
        assert!(generate_native_package_view(&rendered, NativePackageTarget::Codex).is_ok());

        // A manifest hook path that is not in the package fails the render.
        let root = fx.plugins_root.join("gone");
        write(
            &root.join(".claude-plugin/plugin.json"),
            r#"{"name":"gone","version":"1.0.0","hooks":"./hooks/none.json"}"#,
        );
        write(&root.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
        let inventory =
            build_provider_inventory(&bundled_lock(&["gone"]), &fx.plugins_root, &fx.cache_root)
                .expect("inventory");
        assert!(matches!(
            render_provider_generations_at(&inventory, &fx.providers_root),
            Err(ProviderRenderError::InvalidHookDeclaration { .. })
        ));
    }

    #[test]
    fn provider_package_openai_extension_settings_stay_effective() {
        let fx = fixture();
        let root = fx.plugins_root.join("portable");
        write(
            &root.join("plugin.json"),
            &json!({
                "name": "portable", "version": "2.1.0",
                "extensions": {"com.openai": {
                    "interface": {"displayName": "Portable", "shortDescription": "Portable tools"},
                    "hooks": "./hooks/codex.json",
                    "onboardingSkill": "s"
                }, "org.other": {"keep": 1}}
            })
            .to_string(),
        );
        write(
            &root.join(".codex-plugin/plugin.json"),
            &json!({"hooks": "./hooks/overlay.json", "interface": {"displayName": "Overlay"}})
                .to_string(),
        );
        write(
            &root.join("hooks/codex.json"),
            r#"{"hooks":{"UserPromptSubmit":[{"hooks":[]}]}}"#,
        );
        write(
            &root.join("hooks/overlay.json"),
            r#"{"hooks":{"PostToolUse":[{"hooks":[]}]}}"#,
        );
        write(
            &root.join("hooks/hooks.json"),
            r#"{"hooks":{"SessionStart":[{"hooks":[]}]}}"#,
        );
        write(&root.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
        let rendered = render_bundled(&fx, "portable");
        let view =
            generate_native_package_view(&rendered, NativePackageTarget::Codex).expect("view");

        let root_manifest = read_json(&view.path.join(ROOT_MANIFEST));
        let settings = &root_manifest["extensions"]["com.openai"];
        assert!(
            settings.get("hooks").is_none(),
            "hook declaration left the view"
        );
        assert_eq!(settings["onboardingSkill"], "s");
        assert_eq!(settings["interface"]["displayName"], "Portable");
        assert_eq!(
            settings["interface"]["shortDescription"],
            "Portable tools (managed by ccync)"
        );
        assert_eq!(root_manifest["extensions"]["org.other"], json!({"keep": 1}));
        // The overlay is kept without its hook declaration.
        let overlay = read_json(&view.path.join(CODEX_MANIFEST));
        assert!(
            overlay.get("hooks").is_none(),
            "overlay hooks left the view"
        );
        assert_eq!(overlay["interface"], json!({"displayName": "Overlay"}));
        assert_eq!(overlay["name"], "portable");
        assert_eq!(overlay["version"], view.version.as_str());
        assert_eq!(
            view.removed_hook_entrypoints,
            [
                ".codex-plugin/plugin.json#hooks",
                "plugin.json#extensions.com.openai.hooks",
                "hooks/hooks.json",
            ]
        );
        assert!(view
            .components
            .iter()
            .all(|c| c.kind != ComponentKind::Hook));
        assert!(!view.path.join("hooks/hooks.json").exists());

        // Without a com.openai object no extension is added; the overlay
        // carries the presentation and its hooks.
        let root = fx.plugins_root.join("legacy");
        write(
            &root.join("plugin.json"),
            r#"{"name":"legacy","version":"1.0.0"}"#,
        );
        write(&root.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
        let rendered = render_bundled(&fx, "legacy");
        let view =
            generate_native_package_view(&rendered, NativePackageTarget::Codex).expect("view");
        let root_manifest = read_json(&view.path.join(ROOT_MANIFEST));
        assert!(root_manifest.get("extensions").is_none());
        let overlay = read_json(&view.path.join(CODEX_MANIFEST));
        assert_eq!(overlay["interface"]["displayName"], "legacy");
        assert_eq!(
            overlay["interface"]["shortDescription"],
            "legacy (managed by ccync)"
        );
    }

    #[test]
    fn provider_package_bare_skill_is_relocated_into_skills() {
        let fx = fixture();
        let payload = fx.cache_root.join("solo@s1");
        write(
            &payload.join("SKILL.md"),
            "---\nname: solo\ndescription: Solo skill. More text.\n---\nSee [a](assets/a.txt).\n",
        );
        write(&payload.join("assets/a.txt"), "asset");
        let lock = json!({"_personalPlugins": [
            {"pluginId": "solo", "source": "https://example.invalid/solo.git", "pin": "s1"}
        ]});
        let inventory =
            build_provider_inventory(&lock, &fx.plugins_root, &fx.cache_root).expect("inventory");
        let rendered = render_provider_generations_at(&inventory, &fx.providers_root)
            .expect("render")
            .remove(0);
        let view =
            generate_native_package_view(&rendered, NativePackageTarget::Codex).expect("view");
        assert!(view.path.join("skills/solo/SKILL.md").is_file());
        assert!(view.path.join("skills/solo/assets/a.txt").is_file());
        assert!(!view.path.join("SKILL.md").exists());
        let manifest = read_json(&view.path.join(CODEX_MANIFEST));
        assert_eq!(manifest["name"], "solo");
        assert!(manifest["version"]
            .as_str()
            .is_some_and(|v| v.starts_with("0.0.0+ccync.")));
        assert_eq!(view.upstream_version, None);
        assert_eq!(view.upstream_revision.as_deref(), Some("s1"));
        let sidecar = read_yaml(&view.path.join("skills/solo/agents/openai.yaml"));
        assert_eq!(sidecar["interface"]["display_name"], "solo:solo");
        assert_eq!(
            sidecar["interface"]["short_description"],
            "Solo skill (managed by ccync)"
        );
        assert_eq!(
            kinds(&view.components),
            set(&[(ComponentKind::Skill, "solo")])
        );
    }

    #[test]
    fn provider_package_tampered_generation_or_failed_stage_writes_nothing() {
        let (_fx, rendered) = demo_fixture();
        let prior = generate_native_package_view(&rendered, NativePackageTarget::Claude)
            .expect("prior view");
        let views_dir = native_package_views_dir(&rendered.path);
        let prior_bytes = snapshot_of(&prior.path);

        let injected = |stage: &Path| -> Result<(), ProviderRenderError> {
            assert!(stage.join(CODEX_MANIFEST).is_file());
            Err(ProviderRenderError::Validation {
                plugin_id: "demo".to_string(),
                message: "injected failure".to_string(),
            })
        };
        let err =
            generate_native_package_view_checked(&rendered, NativePackageTarget::Codex, &injected)
                .expect_err("injected");
        assert!(err.to_string().contains("injected failure"));
        assert!(dir_names(&views_dir.join("codex")).is_empty(), "no residue");
        assert_eq!(snapshot_of(&prior.path), prior_bytes);

        let tamper = |stage: &Path| -> Result<(), ProviderRenderError> {
            fs::write(stage.join("extra.txt"), "x").expect("tamper");
            Ok(())
        };
        assert!(matches!(
            generate_native_package_view_checked(&rendered, NativePackageTarget::Codex, &tamper),
            Err(ProviderRenderError::Validation { .. })
        ));
        assert!(dir_names(&views_dir.join("codex")).is_empty());

        // A tampered generation is refused before any view is derived.
        fs::write(rendered.path.join("LICENSE-extra"), "tampered").expect("tamper");
        assert!(matches!(
            generate_native_package_view(&rendered, NativePackageTarget::Codex),
            Err(ProviderRenderError::GenerationMismatch { .. })
        ));
        assert!(dir_names(&views_dir.join("codex")).is_empty());
        assert_eq!(snapshot_of(&prior.path), prior_bytes);
    }

    // ── independent spec probes for native package views ───────────────────

    const BOTH_TARGETS: [NativePackageTarget; 2] =
        [NativePackageTarget::Codex, NativePackageTarget::Claude];

    fn marker_count(text: &str) -> usize {
        text.to_ascii_lowercase().matches(MANAGEMENT_MARKER).count()
    }

    /// Immediate child directory names, or empty when the directory is absent.
    fn child_dirs(dir: &Path) -> Vec<String> {
        if !dir.is_dir() {
            return Vec::new();
        }
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("read_dir")
            .map(|e| e.expect("entry"))
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn provider_package_original_ids_never_become_ccync() {
        let fx = fixture();
        write_plugin(&fx.plugins_root.join("alpha"), "alpha-plugin", "alpha");
        let rendered = render_bundled(&fx, "alpha");
        for target in BOTH_TARGETS {
            let view = generate_native_package_view(&rendered, target).expect("view");
            assert_eq!(view.plugin_id, "alpha", "{target}");
            assert_eq!(view.plugin_name, "alpha-plugin", "{target}");
            assert!(view
                .components
                .iter()
                .all(|c| c.plugin_name == "alpha-plugin"
                    && c.component_id != "ccync"
                    && !c.component_id.starts_with("ccync")));
            for manifest in [CODEX_MANIFEST, ROOT_MANIFEST, CLAUDE_MANIFEST] {
                let path = view.path.join(manifest);
                if path.is_file() {
                    assert_eq!(
                        read_json(&path)["name"],
                        "alpha-plugin",
                        "{target} {manifest}"
                    );
                }
            }
            assert!(view.path.join(view.target.primary_manifest()).is_file());
            for dir in ["skills", "commands", "agents"] {
                let names = child_dirs(&view.path.join(dir));
                assert!(
                    names
                        .iter()
                        .all(|n| !n.to_ascii_lowercase().contains("ccync")),
                    "{target} {dir}: {names:?}"
                );
            }
            assert!(
                view.path.join("skills/shared/SKILL.md").is_file(),
                "{target}"
            );
        }

        // Codex display: every published skill directory names the original
        // provider and skill, never the manager.
        let codex = generate_native_package_view(&rendered, NativePackageTarget::Codex)
            .expect("codex view");
        let skills = child_dirs(&codex.path.join("skills"));
        assert!(skills.contains(&"shared".to_string()));
        for skill in skills {
            let sidecar = codex
                .path
                .join("skills")
                .join(&skill)
                .join("agents/openai.yaml");
            assert!(
                sidecar.is_file(),
                "codex skill {skill} has a display sidecar"
            );
            let value = read_yaml(&sidecar);
            assert_eq!(
                value["interface"]["display_name"],
                format!("alpha-plugin:{skill}").as_str(),
                "{skill}"
            );
            let short = value["interface"]["short_description"]
                .as_str()
                .expect("short description");
            assert_eq!(marker_count(short), 1, "{short}");
        }
    }

    #[test]
    fn provider_package_unknown_sidecar_fields_survive_roundtrip() {
        let upstream = "\
interface:
  display_name: Old
  short_description: Does the thing
  icon_small: ./assets/small.svg
  default_prompt: \"Use $s & go\"
  x_future_interface: {nested: [1, 2, {deep: true}]}
policy:
  allow_implicit_invocation: true
  products: [codex, other]
  x_unknown_policy:
    threshold: 0.75
    empty: ~
dependencies:
  tools:
    - type: env_var
      value: TOKEN
      description: |
        multi line
        & block text
x_top_level: [a, 'b', \"c\"]
";
        let fx = fixture();
        let root = fx.plugins_root.join("keep");
        write(
            &root.join(".claude-plugin/plugin.json"),
            r#"{"name":"keep","version":"1.0.0"}"#,
        );
        write(
            &root.join("skills/s/SKILL.md"),
            "---\nname: s\ndescription: Routing text kept verbatim.\n---\nbody\n",
        );
        write(&root.join("skills/s/agents/openai.yaml"), upstream);
        let rendered = render_bundled(&fx, "keep");
        let view =
            generate_native_package_view(&rendered, NativePackageTarget::Codex).expect("view");

        let mut published = read_yaml(&view.path.join("skills/s/agents/openai.yaml"));
        let mut original: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(upstream).expect("upstream yaml");
        assert_eq!(published["interface"]["display_name"], "Old");
        assert_eq!(
            published["interface"]["short_description"],
            "Does the thing (managed by ccync)"
        );
        for value in [&mut published, &mut original] {
            let interface = value["interface"].as_mapping_mut().expect("interface");
            interface.remove("display_name");
            interface.remove("short_description");
        }
        assert_eq!(published, original, "every other sidecar field survives");
        assert_eq!(
            fs::read(view.path.join("skills/s/SKILL.md")).expect("view"),
            fs::read(rendered.path.join("skills/s/SKILL.md")).expect("generation"),
            "routing description untouched"
        );
        assert_eq!(
            fs::read_to_string(rendered.path.join("skills/s/agents/openai.yaml"))
                .expect("generation sidecar"),
            upstream,
            "source sidecar untouched"
        );
    }

    #[test]
    fn provider_package_marker_once_for_any_upstream_casing_or_repetition() {
        let purposes = [
            "Managed By CCYNC",
            "MANAGED BY CCYNC tools",
            "Tools (managed by ccync) (managed by ccync)",
            "managed by ccync managed by ccync",
            "Review code - Managed by Ccync",
            "(Managed by CCYNC) Review code",
            "Plain",
        ];
        for purpose in purposes {
            for limit in [None, Some(SKILL_SHORT_DESCRIPTION_MAX_CHARS)] {
                let out = with_management_marker(purpose, limit);
                assert_eq!(marker_count(&out), 1, "{purpose:?} {limit:?} -> {out:?}");
                assert_eq!(
                    with_management_marker(&out, limit),
                    out,
                    "idempotent for {purpose:?}"
                );
            }
            let sidecar = format!("interface:\n  short_description: \"{purpose}\"\n");
            let once = update_skill_sidecar(
                "p",
                "skills/s/agents/openai.yaml",
                Some(sidecar.as_bytes()),
                "prov",
                "s",
                None,
            )
            .expect("update");
            let text = String::from_utf8(once.clone()).expect("utf8");
            assert_eq!(marker_count(&text), 1, "{purpose:?}: {text}");
            let twice = update_skill_sidecar(
                "p",
                "skills/s/agents/openai.yaml",
                Some(&once),
                "prov",
                "s",
                None,
            )
            .expect("update twice");
            assert_eq!(once, twice, "{purpose:?}");
        }
    }

    #[test]
    fn provider_package_truncation_keeps_char_boundaries_and_marker() {
        let purposes = [
            "審查每一個任務並且協調所有子代理的工作流程以及驗證結果".repeat(3),
            "🚀".repeat(80),
            format!("a{}", "é".repeat(100)),
            format!("{}👩‍👩‍👧‍👦tail", "x".repeat(40)),
            "word ".repeat(30),
        ];
        for purpose in &purposes {
            let out = with_management_marker(purpose, Some(SKILL_SHORT_DESCRIPTION_MAX_CHARS));
            assert!(
                out.chars().count() <= SKILL_SHORT_DESCRIPTION_MAX_CHARS,
                "{out:?}"
            );
            assert!(out.ends_with("(managed by ccync)"), "{out:?}");
            assert_eq!(marker_count(&out), 1, "{out:?}");
            let kept = out.trim_end_matches("(managed by ccync)");
            let kept = kept.trim_end_matches(' ').trim_end_matches("...");
            assert!(!kept.is_empty(), "some purpose is kept: {out:?}");
            assert!(purpose.starts_with(kept.trim_end()), "{out:?}");

            let updated = update_skill_sidecar(
                "p",
                "skills/s/agents/openai.yaml",
                None,
                "prov",
                "s",
                Some(purpose),
            )
            .expect("update");
            let value: serde_yaml_ng::Value = serde_yaml_ng::from_slice(&updated).expect("yaml");
            let short = value["interface"]["short_description"]
                .as_str()
                .expect("short");
            assert!(short.chars().count() <= SKILL_SHORT_DESCRIPTION_MAX_CHARS);
            assert_eq!(marker_count(short), 1);
        }
    }

    #[test]
    fn provider_package_yaml_attacks_rejected_and_literal_text_accepted() {
        let rejected = [
            "interface:\n  short_description: &a val\n",
            "key: &a val\n",
            "a: !!python/object/apply:os.system [\"id\"]\n",
            "a: !!python/object:__main__.X {}\n",
            "base: {a: 1}\nother:\n  <<: *base\n",
            "notes: |\n  text & more\nbad: &x 1\n",
            "notes: >\n  * folded\nbad: *x\n",
            "a: \"line1\n  & still quoted\"\nb: &x 1\n",
            "%TAG ! tag:x.test,2000:\n---\na: 1\n",
            "%YAML 1.1\n---\na: 1\n",
            "a: 1\n...\n---\nb: 2\n",
            "a: 1\n---\nb: 2\n",
            "--- &a\nk: v\n",
            "seq:\n- *a\n",
            "k: [ &a 1 ]\n",
            "k: { x: *a }\n",
            "interface: {}\ninterface: {}\n",
        ];
        for text in rejected {
            let result = update_skill_sidecar(
                "p",
                "skills/s/agents/openai.yaml",
                Some(text.as_bytes()),
                "prov",
                "s",
                None,
            );
            assert!(
                matches!(result, Err(ProviderRenderError::InvalidSidecar { .. })),
                "should reject {text:?}: {result:?}"
            );
        }

        let accepted: [(&str, &str, &str); 8] = [
            ("a: \"x & y *z\"\n", "a", "x & y *z"),
            ("a: 'it''s &ok'\n", "a", "it's &ok"),
            ("a: |\n  & amp\n  *star\n", "a", "& amp\n*star\n"),
            ("a: >-\n  &one\n  *two\n", "a", "&one *two"),
            ("a: value&x\n", "a", "value&x"),
            ("a: \"!notatag\"\n", "a", "!notatag"),
            ("a: b*c\n", "a", "b*c"),
            ("a: \"<<: *x\"\n", "a", "<<: *x"),
        ];
        for (text, key, expected) in accepted {
            let out = update_skill_sidecar(
                "p",
                "skills/s/agents/openai.yaml",
                Some(text.as_bytes()),
                "prov",
                "s",
                Some("Purpose"),
            )
            .unwrap_or_else(|e| panic!("should accept {text:?}: {e:?}"));
            let value: serde_yaml_ng::Value = serde_yaml_ng::from_slice(&out).expect("yaml");
            assert_eq!(value[key], expected, "{text:?}");
            assert_eq!(value["interface"]["display_name"], "prov:s");
        }
    }

    #[test]
    fn provider_package_rejected_sidecar_keeps_generation_and_prior_views() {
        let fx = fixture();
        let root = fx.plugins_root.join("demo");
        write_demo_like_plugin(&root);
        let first = render_bundled(&fx, "demo");
        let prior_views =
            BOTH_TARGETS.map(|t| generate_native_package_view(&first, t).expect("prior view"));
        let views_dir = native_package_views_dir(&first.path);
        let prior_snapshot = snapshot_of(&views_dir);

        write(
            &root.join("skills/pipeline/agents/openai.yaml"),
            "interface:\n  short_description: |\n    text & more\nnext: &anchor 1\n",
        );
        let bad = render_bundled(&fx, "demo");
        let generation_before = snapshot_of(&bad.path);
        assert!(matches!(
            generate_native_package_view(&bad, NativePackageTarget::Codex),
            Err(ProviderRenderError::InvalidSidecar { .. })
        ));
        assert_eq!(
            snapshot_of(&bad.path),
            generation_before,
            "generation untouched"
        );
        assert!(!native_package_views_dir(&bad.path).exists(), "no view dir");
        assert_eq!(
            snapshot_of(&views_dir),
            prior_snapshot,
            "prior views identical"
        );
        for view in &prior_views {
            assert!(view.path.is_dir());
        }

        // A successful Claude view of the same generation also leaves it intact.
        generate_native_package_view(&bad, NativePackageTarget::Claude).expect("claude view");
        assert_eq!(snapshot_of(&bad.path), generation_before);
        assert_eq!(
            child_dirs(&native_package_views_dir(&bad.path)),
            ["claude"],
            "no codex residue"
        );
    }

    #[test]
    fn provider_package_views_have_no_active_mcp_entrypoints_and_keep_hook_helpers() {
        let fx = fixture();
        let source = fx.plugins_root.join("demo");
        write_demo_like_plugin(&source);
        let rendered = render_bundled(&fx, "demo");
        let generation_before = snapshot_of(&rendered.path);
        for target in BOTH_TARGETS {
            let view = generate_native_package_view(&rendered, target).expect("view");
            for file in ROOT_MCP_FILES {
                assert!(!view.path.join(file).exists(), "{target} {file}");
            }
            for manifest in [CODEX_MANIFEST, ROOT_MANIFEST, CLAUDE_MANIFEST] {
                let path = view.path.join(manifest);
                if path.is_file() {
                    let value = read_json(&path);
                    assert!(value.get("mcpServers").is_none(), "{target} {manifest}");
                    assert!(
                        value.pointer("/extensions/com.openai/mcpServers").is_none(),
                        "{target} {manifest}"
                    );
                }
            }
            // The default hook file leaves the view; its helper stays
            // byte-identical to the source.
            assert!(!view.path.join("hooks/hooks.json").exists(), "{target}");
            assert_eq!(
                fs::read(view.path.join("scripts/start.sh")).expect("view helper"),
                fs::read(source.join("scripts/start.sh")).expect("source helper"),
                "{target} scripts/start.sh"
            );
            assert!(
                view.components
                    .iter()
                    .all(|c| c.kind != ComponentKind::Hook),
                "{target} has no active hooks"
            );
        }
        // Centralized host MCP definitions stay in the generation unchanged.
        for file in ROOT_MCP_FILES {
            assert_eq!(
                fs::read(rendered.path.join(file)).expect("generation mcp"),
                fs::read(source.join(file)).expect("source mcp"),
                "{file}"
            );
        }
        let claude = read_json(&rendered.path.join(CLAUDE_MANIFEST));
        assert_eq!(claude["mcpServers"], "./.mcp.json");
        assert_eq!(snapshot_of(&rendered.path), generation_before);
    }

    #[test]
    fn provider_package_mcp_reference_forms() {
        let blocked = [
            json!("../.mcp.json"),
            json!("./sub/.mcp.json"),
            json!({"srv": {"type": "http", "url": "https://x.test"}}),
            json!(true),
        ];
        for (index, declaration) in blocked.into_iter().enumerate() {
            let fx = fixture();
            let id = format!("blk{index}");
            let root = fx.plugins_root.join(&id);
            write(
                &root.join(".codex-plugin/plugin.json"),
                &json!({"name": id, "version": "1.0.0", "mcpServers": declaration}).to_string(),
            );
            write(&root.join("sub/.mcp.json"), r#"{"mcpServers":{}}"#);
            write(&root.join(".mcp.json"), r#"{"mcpServers":{}}"#);
            write(&root.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
            let rendered = render_bundled(&fx, &id);
            let err = generate_native_package_view(&rendered, NativePackageTarget::Codex)
                .expect_err("blocked MCP declaration");
            assert!(
                matches!(
                    err,
                    ProviderRenderError::UnsupportedNativeDeclaration { .. }
                ),
                "{err:?}"
            );
            assert!(!native_package_views_dir(&rendered.path).exists());
        }

        for reference in [".mcp.json", "./.mcp.json", "mcp.json", "./mcp.json"] {
            let fx = fixture();
            let root = fx.plugins_root.join("ok");
            write(
                &root.join(".claude-plugin/plugin.json"),
                &json!({"name": "ok", "version": "1.0.0", "mcpServers": reference}).to_string(),
            );
            write(
                &root.join(".mcp.json"),
                r#"{"mcpServers":{"a":{"command":"x"}}}"#,
            );
            write(
                &root.join("mcp.json"),
                r#"{"mcpServers":{"b":{"command":"y"}}}"#,
            );
            write(&root.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
            let rendered = render_bundled(&fx, "ok");
            for target in BOTH_TARGETS {
                let view = generate_native_package_view(&rendered, target).expect(reference);
                assert!(!view.path.join(".mcp.json").exists(), "{reference}");
                assert!(!view.path.join("mcp.json").exists(), "{reference}");
                assert!(read_json(&view.path.join(CLAUDE_MANIFEST))
                    .get("mcpServers")
                    .is_none());
            }
            assert!(rendered.path.join(".mcp.json").is_file());
            assert!(rendered.path.join("mcp.json").is_file());
        }
    }

    #[test]
    fn provider_package_hook_helpers_missing_or_escaping_block_the_target() {
        let cases = [
            ("missing", "${CLAUDE_PLUGIN_ROOT}/scripts/absent.sh"),
            ("escape", "${CLAUDE_PLUGIN_ROOT}/../outside.sh"),
        ];
        for (id, command) in cases {
            let fx = fixture();
            let root = fx.plugins_root.join(id);
            write(
                &root.join(".claude-plugin/plugin.json"),
                &json!({"name": id, "version": "1.0.0"}).to_string(),
            );
            write(
                &root.join("hooks/hooks.json"),
                &json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": command}]}]}})
                    .to_string(),
            );
            write(&root.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
            let inventory =
                build_provider_inventory(&bundled_lock(&[id]), &fx.plugins_root, &fx.cache_root)
                    .expect("inventory");
            let rendered = match render_provider_generations_at(&inventory, &fx.providers_root) {
                Ok(mut all) => all.remove(0),
                Err(ProviderRenderError::InvalidHookDeclaration { .. }) => continue,
                Err(other) => panic!("{id}: unexpected render error {other:?}"),
            };
            let err = generate_native_package_view(&rendered, NativePackageTarget::Claude)
                .expect_err("unresolved helper blocks Claude view");
            assert!(
                matches!(err, ProviderRenderError::InvalidHookDeclaration { .. }),
                "{id}: {err:?}"
            );
            assert!(
                !native_package_views_dir(&rendered.path)
                    .join("claude")
                    .exists()
                    || dir_names(&native_package_views_dir(&rendered.path).join("claude"))
                        .is_empty(),
                "{id}"
            );
        }
    }

    #[test]
    fn provider_package_codex_never_publishes_a_command_beside_its_skill() {
        let fx = fixture();
        let root = fx.plugins_root.join("dup");
        write(
            &root.join(".claude-plugin/plugin.json"),
            r#"{"name":"dup","version":"1.0.0"}"#,
        );
        write(
            &root.join("skills/run/SKILL.md"),
            "---\nname: run\ndescription: original skill\n---\nskill body\n",
        );
        write(&root.join("commands/run.md"), "command body\n");
        write(&root.join("commands/only.md"), "only command\n");
        let rendered = render_bundled(&fx, "dup");
        let view =
            generate_native_package_view(&rendered, NativePackageTarget::Codex).expect("view");
        assert_eq!(
            fs::read(view.path.join("skills/run/SKILL.md")).expect("skill"),
            fs::read(rendered.path.join("skills/run/SKILL.md")).expect("generation"),
            "the original skill wins"
        );
        assert!(!view.path.join("commands/run.md").exists());
        assert!(!view.path.join("commands/only.md").exists());
        assert!(view.path.join("skills/only/SKILL.md").is_file());
        assert!(view.dropped_command_forms.contains(&"run".to_string()));
        assert!(!view.converted_commands.contains(&"run".to_string()));
        assert_eq!(view.converted_commands, ["only"]);
        for id in ["run", "only"] {
            assert_eq!(
                view.components
                    .iter()
                    .filter(|c| c.component_id == id)
                    .count(),
                1,
                "{id} published once: {:?}",
                view.components
            );
        }
    }

    #[test]
    fn provider_package_version_edges_block_before_publication() {
        let long = format!("1.0.0-{}", "a".repeat(60));
        let cases = [
            ("toolong", json!(long)),
            ("prefixed", json!("v2.0.0")),
            ("partial", json!("2.0")),
        ];
        for (id, version) in cases {
            let fx = fixture();
            let root = fx.plugins_root.join(id);
            write(
                &root.join(".claude-plugin/plugin.json"),
                &json!({"name": id, "version": version}).to_string(),
            );
            write(&root.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
            let rendered = render_bundled(&fx, id);
            for target in BOTH_TARGETS {
                assert!(
                    matches!(
                        generate_native_package_view(&rendered, target),
                        Err(ProviderRenderError::InvalidNativeVersion { .. })
                    ),
                    "{id} {target}"
                );
            }
            assert!(!native_package_views_dir(&rendered.path).exists(), "{id}");
        }

        // A non-string version never panics and never yields an invalid version.
        let fx = fixture();
        let root = fx.plugins_root.join("numeric");
        write(
            &root.join(".claude-plugin/plugin.json"),
            r#"{"name":"numeric","version":1}"#,
        );
        write(&root.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
        let inventory = build_provider_inventory(
            &bundled_lock(&["numeric"]),
            &fx.plugins_root,
            &fx.cache_root,
        )
        .expect("inventory");
        if let Ok(mut all) = render_provider_generations_at(&inventory, &fx.providers_root) {
            let rendered = all.remove(0);
            if let Ok(view) = generate_native_package_view(&rendered, NativePackageTarget::Codex) {
                assert!(semver::Version::parse(&view.version).is_ok());
                assert!(view.version.chars().count() <= 64);
                assert!(view.version.contains("+ccync.") || view.version.contains(".ccync."));
            }
        }

        // Prerelease plus build metadata keeps both and stays semantic.
        let (_fx, rendered) = demo_fixture();
        let view =
            generate_native_package_view(&rendered, NativePackageTarget::Claude).expect("view");
        let parsed = semver::Version::parse(&view.version).expect("semver");
        assert_eq!(parsed.pre.as_str(), "g25e1012dda86");
        let build = parsed.build.as_str();
        assert!(
            build.starts_with("ccync.") && build.len() == "ccync.".len() + 12,
            "{build}"
        );
        assert!(build["ccync.".len()..]
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn provider_package_views_are_deterministic_across_roots_and_reused() {
        let make = || {
            let (fx, rendered) = demo_fixture();
            let views =
                BOTH_TARGETS.map(|t| generate_native_package_view(&rendered, t).expect("view"));
            (fx, rendered, views)
        };
        let (_a, rendered_a, views_a) = make();
        let (_b, rendered_b, views_b) = make();
        assert_eq!(rendered_a.generation, rendered_b.generation);
        for (a, b) in views_a.iter().zip(views_b.iter()) {
            assert!(!a.reused && !b.reused);
            assert_eq!(a.view_id, b.view_id, "{}", a.target);
            assert_eq!(a.view_digest, b.view_digest);
            assert_eq!(a.version, b.version);
            assert_eq!(snapshot_of(&a.path), snapshot_of(&b.path), "{}", a.target);
        }
        for (target, first) in BOTH_TARGETS.into_iter().zip(views_a.iter()) {
            let again = generate_native_package_view(&rendered_a, target).expect("again");
            assert!(again.reused, "{target}");
            assert_eq!(again.path, first.path);
            assert_eq!(again.version, first.version);
        }
    }

    #[test]
    fn p7_t04_native_views_repeat_with_equal_digests_and_keep_labels() {
        let (fx, rendered) = demo_fixture();
        let source = fx.plugins_root.join("demo");
        let generation_before = snapshot_of(&rendered.path);
        for target in BOTH_TARGETS {
            let first = generate_native_package_view(&rendered, target).expect("first view");
            let second = generate_native_package_view(&rendered, target).expect("second view");
            assert!(second.reused, "{target}");
            assert_eq!(first.view_digest, second.view_digest, "{target}");
            assert_eq!(first.path, second.path, "{target}");

            // Plugin identity and the upstream skill label survive.
            assert_eq!(first.plugin_name, "demo", "{target}");
            assert_eq!(
                read_json(&first.path.join(CLAUDE_MANIFEST))["name"],
                "demo",
                "{target}"
            );
            assert_eq!(
                fs::read(first.path.join("skills/pipeline/references/r.md")).expect("view ref"),
                fs::read(source.join("skills/pipeline/references/r.md")).expect("source ref"),
                "{target}"
            );

            // No active MCP or hook entrypoint is embedded in the view.
            for file in ROOT_MCP_FILES.iter().copied().chain([DEFAULT_HOOKS]) {
                assert!(!first.path.join(file).exists(), "{target} {file}");
            }
            for manifest in [CODEX_MANIFEST, ROOT_MANIFEST, CLAUDE_MANIFEST] {
                let path = first.path.join(manifest);
                if path.is_file() {
                    let value = read_json(&path);
                    assert!(value.get("hooks").is_none(), "{target} {manifest}");
                    assert!(value.get("mcpServers").is_none(), "{target} {manifest}");
                }
            }
            assert!(
                first
                    .components
                    .iter()
                    .all(|c| c.kind != ComponentKind::Hook),
                "{target}"
            );
        }
        let codex = generate_native_package_view(&rendered, NativePackageTarget::Codex)
            .expect("codex view");
        let sidecar = read_yaml(&codex.path.join("skills/pipeline/agents/openai.yaml"));
        assert_eq!(sidecar["interface"]["display_name"], "Pipeline");
        assert_eq!(snapshot_of(&rendered.path), generation_before);
    }

    #[test]
    fn p7_t04_codex_portable_root_manifest_wins_over_legacy_overlay() {
        let fx = fixture();
        let root = fx.plugins_root.join("portable-first");
        write(
            &root.join("plugin.json"),
            &json!({
                "name": "portable-first", "version": "3.0.0",
                "description": "Portable description",
                "extensions": {"com.openai": {"interface": {
                    "displayName": "Portable Label",
                    "shortDescription": "Portable purpose"
                }}}
            })
            .to_string(),
        );
        write(
            &root.join(".codex-plugin/plugin.json"),
            &json!({
                "name": "portable-first", "version": "3.0.0",
                "description": "Legacy description",
                "interface": {"displayName": "Legacy Label", "shortDescription": "Legacy purpose"}
            })
            .to_string(),
        );
        write(&root.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
        let rendered = render_bundled(&fx, "portable-first");
        let view =
            generate_native_package_view(&rendered, NativePackageTarget::Codex).expect("view");
        let again =
            generate_native_package_view(&rendered, NativePackageTarget::Codex).expect("again");
        assert_eq!(view.view_digest, again.view_digest);
        assert_eq!(view.upstream_version.as_deref(), Some("3.0.0"));

        // The effective settings are the portable object, not the overlay.
        let portable = read_json(&view.path.join(ROOT_MANIFEST));
        assert_eq!(portable["description"], "Portable description");
        let settings = &portable["extensions"]["com.openai"]["interface"];
        assert_eq!(settings["displayName"], "Portable Label");
        assert_eq!(
            settings["shortDescription"],
            "Portable purpose (managed by ccync)"
        );
        // The legacy overlay keeps its own label and is not promoted.
        let legacy = read_json(&view.path.join(CODEX_MANIFEST));
        assert_eq!(legacy["interface"]["displayName"], "Legacy Label");
        assert_eq!(legacy["description"], "Legacy description");
    }

    #[test]
    fn provider_package_components_ignore_noise_and_use_hook_events() {
        let fx = fixture();
        let root = fx.plugins_root.join("noisy");
        write(
            &root.join(".claude-plugin/plugin.json"),
            r#"{"name":"noisy","version":"1.0.0"}"#,
        );
        write(&root.join("skills/real/SKILL.md"), "---\nname: real\n---\n");
        write(&root.join("skills/README.md"), "readme\n");
        write(&root.join("skills/.gitkeep"), "");
        write(&root.join("skills/nosk/notes.md"), "no SKILL.md\n");
        write(&root.join("skills/.hidden/SKILL.md"), "---\nname: h\n---\n");
        write(&root.join("commands/go.md"), "go\n");
        write(&root.join("commands/.gitkeep"), "");
        write(&root.join("commands/.secret.md"), "s\n");
        write(&root.join("commands/data.json"), "{}");
        write(&root.join("agents/rev.md"), "agent\n");
        write(&root.join("agents/.hidden.md"), "h\n");
        write(&root.join("agents/notes.txt"), "n\n");
        write(
            &root.join("hooks/hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"hooks":[]}],"SessionEnd":[{"hooks":[]}]}}"#,
        );
        write(
            &root.join("hooks/unreferenced.json"),
            r#"{"hooks":{"Bogus":[]}}"#,
        );
        let rendered = render_bundled(&fx, "noisy");
        use ComponentKind::{Agent, Command, Hook, Skill};
        assert_eq!(
            kinds(&rendered.components),
            set(&[
                (Skill, "real"),
                (Command, "go"),
                (Agent, "rev"),
                (Hook, "PreToolUse"),
                (Hook, "SessionEnd"),
            ])
        );
        // The generation keeps the hook events; the Claude view does not
        // activate them.
        let claude =
            generate_native_package_view(&rendered, NativePackageTarget::Claude).expect("view");
        assert_eq!(
            kinds(&claude.components),
            set(&[(Skill, "real"), (Command, "go"), (Agent, "rev")])
        );
    }

    // ── review follow-ups ───────────────────────────────────────────────────

    /// A provider whose root manifest carries Codex settings with the given
    /// `mcpServers` declaration inside `extensions["com.openai"]`.
    fn write_extension_mcp_plugin(root: &Path, id: &str, declaration: Value) {
        write(
            &root.join("plugin.json"),
            &json!({
                "name": id, "version": "1.0.0",
                "extensions": {"com.openai": {
                    "interface": {"shortDescription": "Tools"},
                    "mcpServers": declaration
                }}
            })
            .to_string(),
        );
        write(
            &root.join(".mcp.json"),
            r#"{"mcpServers":{"srv":{"command":"npx"}}}"#,
        );
        write(&root.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
    }

    #[test]
    fn provider_package_codex_extension_mcp_reference_is_removed() {
        for (index, declaration) in [json!("./.mcp.json"), json!("mcp.json"), json!({})]
            .into_iter()
            .enumerate()
        {
            let fx = fixture();
            let id = format!("ext{index}");
            write_extension_mcp_plugin(&fx.plugins_root.join(&id), &id, declaration.clone());
            let rendered = render_bundled(&fx, &id);
            let view = generate_native_package_view(&rendered, NativePackageTarget::Codex)
                .expect("codex view");
            let root_manifest = read_json(&view.path.join(ROOT_MANIFEST));
            assert!(
                root_manifest
                    .pointer("/extensions/com.openai/mcpServers")
                    .is_none(),
                "{declaration}"
            );
            assert_eq!(
                root_manifest["extensions"]["com.openai"]["interface"]["shortDescription"],
                "Tools (managed by ccync)"
            );
            assert!(view
                .removed_mcp_entrypoints
                .contains(&"plugin.json#extensions.com.openai.mcpServers".to_string()));
            assert!(!view.path.join(".mcp.json").exists());
            // The generation keeps the source declaration for the host projection.
            let source = read_json(&rendered.path.join(ROOT_MANIFEST));
            assert_eq!(
                source["extensions"]["com.openai"]["mcpServers"],
                declaration
            );
        }
    }

    #[test]
    fn provider_package_codex_extension_inline_mcp_blocks_codex_view() {
        for (index, declaration) in [
            json!({"srv": {"command": "npx"}}),
            json!("./config/servers.json"),
            json!(["./.mcp.json"]),
        ]
        .into_iter()
        .enumerate()
        {
            let fx = fixture();
            let id = format!("inline{index}");
            write_extension_mcp_plugin(&fx.plugins_root.join(&id), &id, declaration.clone());
            let rendered = render_bundled(&fx, &id);
            let err = generate_native_package_view(&rendered, NativePackageTarget::Codex)
                .expect_err("ambiguous Codex MCP declaration");
            assert!(
                matches!(
                    &err,
                    ProviderRenderError::UnsupportedNativeDeclaration {
                        target: NativePackageTarget::Codex,
                        reason,
                        ..
                    } if reason.contains("extensions.com.openai.mcpServers")
                ),
                "{declaration}: {err:?}"
            );
            assert!(!native_package_views_dir(&rendered.path).exists());
            // Claude does not load Codex settings, so its view still builds.
            assert!(generate_native_package_view(&rendered, NativePackageTarget::Claude).is_ok());
        }
    }

    #[test]
    fn provider_package_marker_strip_keeps_upstream_punctuation() {
        let limit = Some(SKILL_SHORT_DESCRIPTION_MAX_CHARS);
        assert_eq!(
            with_management_marker("Run tests (fast)", limit),
            "Run tests (fast) (managed by ccync)"
        );
        assert_eq!(
            with_management_marker("(Beta) Deploy", limit),
            "(Beta) Deploy (managed by ccync)"
        );
        assert_eq!(
            with_management_marker("-x flag helper", None),
            "-x flag helper (managed by ccync)"
        );
        assert_eq!(
            with_management_marker("Run tests (fast) (managed by ccync)", limit),
            "Run tests (fast) (managed by ccync)"
        );
        assert_eq!(
            with_management_marker("managed by ccync - Deploy (v2)", limit),
            "Deploy (v2) (managed by ccync)"
        );
        assert_eq!(
            with_management_marker("Deploy (v2), managed by ccync", limit),
            "Deploy (v2) (managed by ccync)"
        );
        let sidecar = update_skill_sidecar(
            "p",
            "skills/s/agents/openai.yaml",
            Some(b"interface:\n  short_description: Run tests (fast)\n"),
            "prov",
            "s",
            None,
        )
        .expect("sidecar");
        let value: serde_yaml_ng::Value = serde_yaml_ng::from_slice(&sidecar).expect("yaml");
        assert_eq!(
            value["interface"]["short_description"],
            "Run tests (fast) (managed by ccync)"
        );
    }

    /// A bare skill (root `SKILL.md`) that also ships plugin-root files.
    fn write_bare_skill_with_root_files(root: &Path, manifest_name: &str) {
        write(
            &root.join("SKILL.md"),
            "---\nname: solo\ndescription: Solo skill\n---\nSee [a](assets/a.txt).\n",
        );
        write(&root.join("assets/a.txt"), "asset\n");
        write(
            &root.join(".claude-plugin/plugin.json"),
            &json!({"name": manifest_name, "version": "1.0.0", "mcpServers": "./.mcp.json"})
                .to_string(),
        );
        write(
            &root.join("hooks/hooks.json"),
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"${CLAUDE_PLUGIN_ROOT}/scripts/start.sh"}]}]}}"#,
        );
        write(&root.join("scripts/start.sh"), "echo start\n");
        write(&root.join(".mcp.json"), r#"{"mcpServers":{}}"#);
    }

    #[test]
    fn provider_package_bare_skill_keeps_plugin_root_files_and_drops_hooks() {
        let fx = fixture();
        write_bare_skill_with_root_files(&fx.plugins_root.join("solo"), "solo");
        let rendered = render_bundled(&fx, "solo");
        for target in BOTH_TARGETS {
            let view = generate_native_package_view(&rendered, target).expect("view");
            assert!(view.path.join("skills/solo/SKILL.md").is_file(), "{target}");
            assert!(
                view.path.join("skills/solo/assets/a.txt").is_file(),
                "{target}"
            );
            assert!(view.path.join(CLAUDE_MANIFEST).is_file(), "{target}");
            assert!(
                !view.path.join("skills/solo").join(CLAUDE_MANIFEST).exists(),
                "{target} manifest not moved"
            );
            // The default hook file is not active in the view.
            assert!(!view.path.join("hooks/hooks.json").exists(), "{target}");
            // A hook helper stays at the root and is also available to the
            // skill's relative links.
            for helper in ["scripts/start.sh", "skills/solo/scripts/start.sh"] {
                assert_eq!(
                    fs::read_to_string(view.path.join(helper)).expect("helper"),
                    "echo start\n",
                    "{target} {helper}"
                );
            }
            assert!(!view.path.join(".mcp.json").exists(), "{target}");
            assert!(
                !view.path.join("skills/solo/.mcp.json").exists(),
                "{target}"
            );
            let claude = read_json(&view.path.join(CLAUDE_MANIFEST));
            assert!(claude.get("mcpServers").is_none(), "{target}");
            assert_eq!(claude["version"], view.version.as_str());
            let skills: Vec<&str> = view
                .components
                .iter()
                .filter(|c| c.kind == ComponentKind::Skill)
                .map(|c| c.component_id.as_str())
                .collect();
            assert_eq!(skills, ["solo"], "{target}");
        }
        // The generation keeps the SessionStart hook; no view activates it.
        let rendered_hooks: Vec<&str> = rendered
            .components
            .iter()
            .filter(|c| c.kind == ComponentKind::Hook)
            .map(|c| c.component_id.as_str())
            .collect();
        assert_eq!(rendered_hooks, ["SessionStart"]);
        let claude = generate_native_package_view(&rendered, NativePackageTarget::Claude)
            .expect("claude view");
        assert!(claude
            .components
            .iter()
            .all(|c| c.kind != ComponentKind::Hook));
    }

    #[test]
    fn provider_package_bare_skill_manifest_name_is_checked() {
        let fx = fixture();
        let root = fx.plugins_root.join("solo");
        write_bare_skill_with_root_files(&root, "solo");
        // A second manifest that disagrees with the plugin name.
        write(
            &root.join(".codex-plugin/plugin.json"),
            r#"{"name":"other","version":"1.0.0"}"#,
        );
        let rendered = render_bundled(&fx, "solo");
        for target in BOTH_TARGETS {
            assert!(
                matches!(
                    generate_native_package_view(&rendered, target),
                    Err(ProviderRenderError::ManifestConflict { .. })
                ),
                "{target}"
            );
        }
        assert!(!native_package_views_dir(&rendered.path).exists());
    }

    #[test]
    fn portable_package_admission_preserves_verified_bytes() {
        let fx = fixture();
        let root = fx.plugins_root.join("portable");
        write(
            &root.join("plugin.json"),
            r#"{"name":"portable","version":"1.0.0","mcpServers":"./.mcp.json"}"#,
        );
        write(
            &root.join(".mcp.json"),
            r#"{"mcpServers":{"local":{"command":"server"}}}"#,
        );
        write(
            &root.join("skills/s/SKILL.md"),
            "---\nname: s\n---\nskill\n",
        );
        write(&root.join("assets/data.bin"), "opaque\0bytes");
        let rendered = render_bundled(&fx, "portable");
        let source_digest = rendered.generation.package_digest.clone();
        let view = admit_portable_package(&rendered, &fx.providers_root.join("packages"))
            .expect("admitted portable package");
        let published: PackageFiles = snapshot_dir("portable", &view.path)
            .expect("view")
            .into_iter()
            .collect();
        assert_eq!(
            files_digest_hex(&published),
            source_digest.as_str().strip_prefix("sha256:").unwrap()
        );
        assert_eq!(published, snapshot_of(&rendered.path).into_iter().collect());
        assert_eq!(view.package_digest, source_digest);
        let native_fixture = fixture();
        let native_root = native_fixture.plugins_root.join("native-entrypoints");
        write(
            &native_root.join("plugin.json"),
            r#"{"name":"native-entrypoints","version":"1.0.0","hooks":"./hooks/hooks.json","mcpServers":"./.mcp.json"}"#,
        );
        write(
            &native_root.join("hooks/hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"hooks":[]}]}}"#,
        );
        write(&native_root.join(".mcp.json"), r#"{"mcpServers":{}}"#);
        write(
            &native_root.join("skills/s/SKILL.md"),
            "---\nname: s\n---\n",
        );
        let native_generation = render_bundled(&native_fixture, "native-entrypoints");
        let native = generate_native_package_view(&native_generation, NativePackageTarget::Codex)
            .expect("native adapter view");
        assert!(!native.path.join(".mcp.json").exists());
        assert!(read_json(&native.path.join(ROOT_MANIFEST))
            .get("mcpServers")
            .is_none());
        assert!(!native.path.join(DEFAULT_HOOKS).exists());
        eprintln!("portable_package_admission evidence: admitted={} source={} view={} native_mcp=absent native_hooks=absent opaque=preserved", view.path.display(), source_digest, view.package_digest);
    }

    #[test]
    fn portable_package_admission_duplicate_key() {
        let cases = [
            (
                "duplicate-root-key",
                ".mcp.json",
                r#"{"mcpServers":{},"\u006dcpServers":{}}"#,
            ),
            (
                "duplicate-server-field",
                ".mcp.json",
                r#"{"mcpServers":{"srv":{"command":"safe","command":"other"}}}"#,
            ),
            (
                "duplicate-env-key",
                ".mcp.json",
                r#"{"mcpServers":{"srv":{"command":"safe","env":{"API_KEY":"literal","API_KEY":"${API_KEY}"}}}}"#,
            ),
            (
                "duplicate-header-key",
                ".mcp.json",
                r#"{"mcpServers":{"srv":{"url":"https://example.test/mcp","headers":{"Authorization":"literal","Authorization":"${TOKEN}"}}}}"#,
            ),
            (
                "duplicate-manifest-field",
                "plugin.json",
                r#"{"name":"duplicate-manifest-field","name":"other","version":"1.0.0"}"#,
            ),
            (
                "duplicate-yaml-key",
                "skills/s/agents/openai.yaml",
                "interface:\n  display_name: Safe\n  display_name: Other\n",
            ),
        ];

        for (id, path, declaration) in cases {
            let fx = fixture();
            let source = fx.plugins_root.join(id);
            write(
                &source.join("plugin.json"),
                &json!({"name":id,"version":"1.0.0"}).to_string(),
            );
            write(&source.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
            write(&source.join(path), declaration);
            let rendered = render_bundled(&fx, id);
            let before = snapshot_of(&rendered.path);
            let packages = fx.providers_root.join("packages");
            assert!(
                matches!(
                    admit_portable_package(&rendered, &packages),
                    Err(ProviderRenderError::PortablePackageRejected(_))
                ),
                "{id}"
            );
            assert_eq!(snapshot_of(&rendered.path), before, "{id} source changed");
            assert!(
                !packages.join(rendered.generation.source.as_str()).exists(),
                "{id} portable output exists"
            );
        }
    }

    #[test]
    fn portable_package_admission_identifier() {
        let cases = [
            (
                "invalid-manifest-name",
                Some(r#"{"name":"bad/name","version":"1.0.0"}"#),
                None,
            ),
            (
                "invalid-server-name",
                None,
                Some(r#"{"mcpServers":{"bad/name":{"command":"safe"}}}"#),
            ),
            (
                "invalid-env-name",
                None,
                Some(r#"{"mcpServers":{"srv":{"command":"safe","env":{"BAD/NAME":"${TOKEN}"}}}}"#),
            ),
            (
                "invalid-header-name",
                None,
                Some(
                    r#"{"mcpServers":{"srv":{"url":"https://example.test/mcp","headers":{"BAD/NAME":"${TOKEN}"}}}}"#,
                ),
            ),
        ];

        for (id, manifest, mcp) in cases {
            let fx = fixture();
            let source = fx.plugins_root.join(id);
            let manifest = manifest
                .map(str::to_owned)
                .unwrap_or_else(|| json!({"name":id,"version":"1.0.0"}).to_string());
            write(&source.join("plugin.json"), &manifest);
            write(&source.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
            if let Some(declaration) = mcp {
                write(&source.join(".mcp.json"), declaration);
            }
            let rendered = render_bundled(&fx, id);
            let before = snapshot_of(&rendered.path);
            let packages = fx.providers_root.join("packages");
            assert!(
                matches!(
                    admit_portable_package(&rendered, &packages),
                    Err(ProviderRenderError::PortablePackageRejected(_))
                ),
                "{id}"
            );
            assert_eq!(snapshot_of(&rendered.path), before, "{id} source changed");
            assert!(
                !packages.join(rendered.generation.source.as_str()).exists(),
                "{id} portable output exists"
            );
        }
    }

    #[test]
    fn portable_package_admission_enforces_certified_schemas_and_retains_rejections() {
        let cases = [
            (
                "sidecar-header",
                "skills/s/agents/openai.yaml",
                "headers:\n  Authorization: Bearer canary\n",
                "unsupported skill sidecar field",
            ),
            (
                "sidecar-root",
                "skills/s/agents/openai.yaml",
                "custom: true\n",
                "unsupported skill sidecar field",
            ),
            (
                "sidecar-policy",
                "skills/s/agents/openai.yaml",
                "policy: nope\n",
                "policy must be a mapping",
            ),
            (
                "sidecar-policy-key",
                "skills/s/agents/openai.yaml",
                "policy:\n  unknown: true\n",
                "unsupported policy field",
            ),
            (
                "sidecar-dependency",
                "skills/s/agents/openai.yaml",
                "policy:\n  dependencies:\n    tools: mcp\n",
                "skill MCP dependency is not admitted",
            ),
            (
                "sidecar-anchor",
                "skills/s/agents/openai.yaml",
                "interface: &x {}\nalias: *x\n",
                "anchors are not supported",
            ),
            (
                "sidecar-icon",
                "skills/s/agents/openai.yaml",
                "interface:\n  icon_small: ../../x\n",
                "safe relative path",
            ),
            (
                "deep-sidecar",
                "skills/a/b/agents/openai.yaml",
                "interface: {}\n",
                "unclassified configuration",
            ),
            (
                "manifest-lsp",
                "plugin.json",
                r#"{"name":"manifest-lsp","lspServers":{"x":{"env":{"AUTHORIZATION":"secret"}}}}"#,
                "LSP declaration is not admitted",
            ),
            (
                "manifest-repo",
                "plugin.json",
                r#"{"name":"manifest-repo","repository":"https://u:p@example.test/repo"}"#,
                "invalid manifest field 'repository'",
            ),
            (
                "mcp-header-arg",
                ".mcp.json",
                r#"{"mcpServers":{"srv":{"command":"srv","args":["--header","Authorization: Bearer canary"]}}}"#,
                "unsupported MCP declaration shape",
            ),
            (
                "mcp-command",
                ".mcp.json",
                r#"{"mcpServers":{"srv":{"command":"srv --api-key sk-live"}}}"#,
                "unsupported MCP declaration shape",
            ),
        ];
        for (id, path, body, expected) in cases {
            let fx = fixture();
            let source = fx.plugins_root.join(id);
            write(
                &source.join("plugin.json"),
                &json!({"name":id,"version":"1.0.0"}).to_string(),
            );
            write(&source.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
            write(&source.join(path), body);
            let rendered = render_bundled(&fx, id);
            let before = snapshot_of(&rendered.path);
            let packages = fx.providers_root.join("packages");
            let error = admit_portable_package(&rendered, &packages).expect_err(id);
            let ProviderRenderError::PortablePackageRejected(rejection) = error else {
                panic!("unexpected error for {id}: {error}")
            };
            assert!(
                rejection.reasons.iter().any(|r| r.contains(expected)),
                "{id}: {:?}",
                rejection.reasons
            );
            assert_eq!(
                snapshot_of(&rendered.path),
                before,
                "{id} private source changed"
            );
            assert!(
                !packages.join("bundled").exists(),
                "{id} portable output exists"
            );
        }
    }

    #[test]
    fn portable_package_admission_accepts_typed_sidecar_and_header_placeholder_argument() {
        let fx = fixture();
        let source = fx.plugins_root.join("typed-sidecar");
        write(
            &source.join("plugin.json"),
            r#"{"name":"typed-sidecar","version":"1.0.0"}"#,
        );
        write(&source.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
        write(&source.join("skills/s/agents/openai.yaml"), "interface:\n  display_name: S\n  icon_small: icons/s.png\npolicy:\n  allow_implicit_invocation: true\n");
        write(
            &source.join(".mcp.json"),
            r#"{"mcpServers":{"srv":{"command":"srv","args":["--header","Authorization: Bearer ${TOKEN}"]}}}"#,
        );
        let rendered = render_bundled(&fx, "typed-sidecar");
        let view = admit_portable_package(&rendered, &fx.providers_root.join("packages"))
            .expect("admitted");
        assert_eq!(view.package_digest, rendered.generation.package_digest);
    }

    #[test]
    fn portable_package_admission_rejects_unsafe_declarations_and_retains_source() {
        let cases = [
            (
                "hook",
                "hooks/hooks.json",
                r#"{"hooks":{}}"#,
                "hook configuration",
            ),
            (
                "secret",
                ".mcp.json",
                r#"{"mcpServers":{"srv":{"token":"secret"}}}"#,
                "unsupported MCP declaration shape",
            ),
            (
                "sidecar-secret",
                "skills/s/agents/openai.yaml",
                "api_token: canary-secret\n",
                "unsupported skill sidecar field",
            ),
            (
                "unknown",
                "settings.json",
                r#"{"custom":true}"#,
                "unclassified configuration",
            ),
        ];
        let mut observed = Vec::new();
        for (id, path, body, expected) in cases {
            let fx = fixture();
            let root = fx.plugins_root.join(id);
            write(
                &root.join("plugin.json"),
                &json!({"name":id,"version":"1.0.0"}).to_string(),
            );
            write(&root.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
            write(&root.join(path), body);
            let rendered = render_bundled(&fx, id);
            let before = snapshot_of(&rendered.path);
            let root = fx.providers_root.join("packages");
            let err = admit_portable_package(&rendered, &root).expect_err("rejected");
            let ProviderRenderError::PortablePackageRejected(rejection) = err else {
                panic!("unexpected rejection: {err}")
            };
            assert!(
                rejection
                    .reasons
                    .iter()
                    .any(|reason| reason.contains(expected)),
                "{:?}",
                rejection.reasons
            );
            assert!(!root.join(rendered.generation.source.as_str()).exists());
            assert_eq!(snapshot_of(&rendered.path), before, "source retained");
            observed.extend(rejection.reasons);
        }
        eprintln!(
            "portable_package_admission evidence: rejected={}",
            observed.join(" | ")
        );
    }

    #[test]
    fn portable_package_admission_rejects_unclassified_skill_configuration() {
        let fx = fixture();
        let source = fx.plugins_root.join("skill-config");
        write(
            &source.join("plugin.json"),
            r#"{"name":"skill-config","version":"1.0.0"}"#,
        );
        write(&source.join("skills/s/SKILL.md"), "---\nname: s\n---\n");
        let canary = r#"{"api_token":"audit-canary-123"}"#;
        write(&source.join("skills/s/config.json"), canary);
        let rendered = render_bundled(&fx, "skill-config");
        let before = snapshot_of(&rendered.path);
        let packages_root = fx.providers_root.join("packages");
        assert!(matches!(
            admit_portable_package(&rendered, &packages_root),
            Err(ProviderRenderError::PortablePackageRejected(_))
        ));
        assert!(!packages_root.join("bundled").exists());
        assert_eq!(snapshot_of(&rendered.path), before);
        assert_eq!(
            fs::read_to_string(rendered.path.join("skills/s/config.json")).unwrap(),
            canary
        );
        eprintln!("portable_package_admission evidence: rejected skills/s/config.json; source retained; no portable output");
    }

    #[test]
    fn portable_package_admission_classifies_mcp_args() {
        let rejected = [
            ("split-literal", r#"["--api-key","sk-live-123"]"#),
            ("inline-auth-literal", r#"["--auth-token=literal"]"#),
        ];
        for (id, args) in rejected {
            let fx = fixture();
            let source = fx.plugins_root.join(id);
            write(
                &source.join("plugin.json"),
                &json!({"name":id,"version":"1.0.0"}).to_string(),
            );
            let declaration =
                format!(r#"{{"mcpServers":{{"srv":{{"command":"server","args":{args}}}}}}}"#);
            write(&source.join(".mcp.json"), &declaration);
            let rendered = render_bundled(&fx, id);
            let before = snapshot_of(&rendered.path);
            let packages_root = fx.providers_root.join("packages");
            assert!(
                matches!(
                    admit_portable_package(&rendered, &packages_root),
                    Err(ProviderRenderError::PortablePackageRejected(_))
                ),
                "{id}"
            );
            assert!(!packages_root.join("bundled").exists(), "{id}");
            assert_eq!(snapshot_of(&rendered.path), before, "{id}");
        }

        for (id, args) in [
            ("inline-placeholder", r#"["--api-key=${API_KEY}"]"#),
            ("split-placeholder", r#"["--api-key","${API_KEY}"]"#),
        ] {
            let fx = fixture();
            let source = fx.plugins_root.join(id);
            write(
                &source.join("plugin.json"),
                &json!({"name":id,"version":"1.0.0"}).to_string(),
            );
            let declaration =
                format!(r#"{{"mcpServers":{{"srv":{{"command":"server","args":{args}}}}}}}"#);
            write(&source.join(".mcp.json"), &declaration);
            let rendered = render_bundled(&fx, id);
            let view = admit_portable_package(&rendered, &fx.providers_root.join("packages"))
                .expect("placeholder argument admitted");
            assert_eq!(view.package_digest, rendered.generation.package_digest);
        }
        eprintln!("portable_package_admission evidence: credential args rejected unless exact placeholder; split and inline placeholders admitted");
    }

    #[test]
    fn portable_package_admission_rejects_unapproved_mcp_values_without_publication() {
        let cases = [
            (
                "literal-authorization",
                r#"{"mcpServers":{"srv":{"url":"https://example.test/mcp","headers":{"Authorization":"Bearer private"}}}}"#,
            ),
            (
                "literal-env",
                r#"{"mcpServers":{"srv":{"command":"server","env":{"API_KEY":"private"}}}}"#,
            ),
            (
                "unknown-field",
                r#"{"mcpServers":{"srv":{"command":"server","metadata":true}}}"#,
            ),
            (
                "http-url",
                r#"{"mcpServers":{"srv":{"url":"http://example.test/mcp"}}}"#,
            ),
            (
                "query-url",
                r#"{"mcpServers":{"srv":{"url":"https://example.test/mcp?mode=1"}}}"#,
            ),
            (
                "credential-arg",
                r#"{"mcpServers":{"srv":{"command":"server","args":["--api_key=private"]}}}"#,
            ),
        ];

        for (id, declaration) in cases {
            let fx = fixture();
            let source = fx.plugins_root.join(id);
            write(
                &source.join("plugin.json"),
                &json!({"name":id,"version":"1.0.0"}).to_string(),
            );
            write(&source.join(".mcp.json"), declaration);
            let rendered = render_bundled(&fx, id);
            let before = snapshot_of(&rendered.path);
            let packages_root = fx.providers_root.join("packages");
            assert!(
                matches!(
                    admit_portable_package(&rendered, &packages_root),
                    Err(ProviderRenderError::PortablePackageRejected(_))
                ),
                "{id}"
            );
            assert_eq!(snapshot_of(&rendered.path), before, "{id} source retained");
            assert!(
                !packages_root
                    .join(rendered.generation.source.as_str())
                    .exists(),
                "{id} must not publish"
            );
        }
    }

    #[test]
    fn portable_package_admission_accepts_mcp_placeholders() {
        let fx = fixture();
        let source = fx.plugins_root.join("placeholders");
        write(
            &source.join("plugin.json"),
            r#"{"name":"placeholders","version":"1.0.0"}"#,
        );
        write(
            &source.join(".mcp.json"),
            r#"{"mcpServers":{"srv":{"type":"http","url":"https://example.test/mcp","headers":{"Authorization":"${MCP_AUTH}"},"env":{"API_KEY":"${API_KEY}"}}}}"#,
        );
        let rendered = render_bundled(&fx, "placeholders");
        let view = admit_portable_package(&rendered, &fx.providers_root.join("packages"))
            .expect("placeholder MCP declaration admitted");
        assert_eq!(view.package_digest, rendered.generation.package_digest);
    }

    #[test]
    fn provider_render_payload_size_cap_stops_before_reading_and_skips_git() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path();
        fs::create_dir_all(root.join(".git")).expect("git dir");
        fs::write(root.join(".git").join("pack"), vec![0u8; 64]).expect("git pack");
        fs::create_dir_all(root.join("skills").join("a")).expect("skill dir");
        fs::write(
            root.join("skills").join("a").join("SKILL.md"),
            vec![b'x'; 16],
        )
        .expect("skill");
        assert!(!payload_size_exceeds(root, true, 32).expect("walk"));
        assert!(payload_size_exceeds(root, false, 32).expect("walk"));
        assert!(payload_size_exceeds(root, true, 8).expect("walk"));
    }
}
