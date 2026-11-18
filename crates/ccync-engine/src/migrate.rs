//! One-shot migration shim: old-to-new `~/.ccync/` layout.

use crate::config::{write_projection_selection_at, CcyncConfig, ProjectionTargetResolution};
use ccync_foundation::json_util;
use ccync_foundation::paths::{EffectiveLayout, EffectiveLayoutAuthority};
use ccync_foundation::provider::{LayoutRootIdentity, StorageLayoutCommit};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

// ── layout migration ─────────────────────────────────────────────────────────

/// A single file/directory move to apply during migration.
#[derive(Debug, Clone)]
pub struct LayoutMove {
    pub from: PathBuf,
    pub to: PathBuf,
}

/// Planned migration: a list of layout moves to apply.
#[derive(Debug, Default)]
pub struct MigrationPlan {
    pub moves: Vec<LayoutMove>,
}

/// A read-only inventory of data that must be retained during a layout move.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortableInventory {
    pub objects: Vec<InventoryObject>,
    pub provider_native_rows: Vec<Value>,
    pub standalone_proofs: Vec<Value>,
    pub provider_intents: Vec<Value>,
    pub standalone_intents: Vec<Value>,
    pub standalone_withdrawal_intents: Vec<Value>,
    pub mcp_journal: Value,
    pub tool_journal: Value,
    pub ownership_rows: Vec<OwnershipInventoryRow>,
    pub counts_without_fingerprint: BTreeMap<String, usize>,
    pub pending_older_sublayout_fixup: bool,
    pub blocked: bool,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventoryObject {
    pub category: String,
    pub path: PathBuf,
    pub fingerprint: Option<String>,
    pub retained: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnershipInventoryRow {
    pub category: String,
    pub classification: String,
    pub recorded: Value,
    pub live_path: Option<PathBuf>,
    pub current_fingerprint: Option<String>,
    pub matches_recorded_fingerprint: Option<bool>,
}

/// Stage private copies and portable exports without changing layout authority.
/// The hook runs after each destination is written and verified.
pub fn stage_portable_migration(
    layout: &EffectiveLayout,
    mut copy_hook: impl FnMut(&Path, &Path) -> Result<(), String>,
) -> Result<Value, String> {
    let _guard = crate::tool_manager::lock::LifecycleLock::acquire_if_needed()
        .map_err(|error| error.to_string())?;
    if layout.authority() == EffectiveLayoutAuthority::Committed
        || layout.authority() == EffectiveLayoutAuthority::MutationBlocked
        || layout.root_error().is_some()
    {
        return Err("portable migration requires an uncommitted, writable layout".into());
    }
    let inventory = build_portable_inventory(layout);
    if inventory.blocked {
        return Err(format!(
            "migration blocked: {}",
            inventory.blockers.join("; ")
        ));
    }
    let roots = layout.storage_roots();
    let legacy = layout
        .migration_sentinel_path()
        .parent()
        .expect("sentinel has a parent")
        .to_path_buf();
    let commit = StorageLayoutCommit {
        layout_version: 1,
        roots: vec![
            LayoutRootIdentity {
                name: "portable".into(),
                path: roots.portable.clone(),
            },
            LayoutRootIdentity {
                name: "config".into(),
                path: roots.config.clone(),
            },
            LayoutRootIdentity {
                name: "state".into(),
                path: roots.state.clone(),
            },
            LayoutRootIdentity {
                name: "cache".into(),
                path: roots.cache.clone(),
            },
        ],
    };
    let journal = layout.migration_journal_path();
    let sentinel = layout.migration_sentinel_path();
    let manifest_path = journal.clone();
    let mut created = Vec::<PathBuf>::new();
    let result = (|| {
        let mut manifest = if journal.exists() {
            let bytes =
                fs::read(&journal).map_err(|e| format!("migration journal cannot be read: {e}"))?;
            let manifest: Value = serde_json::from_slice(&bytes)
                .map_err(|e| format!("migration journal is invalid: {e}"))?;
            if manifest["version"] != 1
                || manifest["commit"] != serde_json::to_value(&commit).map_err(|e| e.to_string())?
            {
                return Err("migration journal does not match the current storage roots".into());
            }
            if manifest["stage"] != "staging" && manifest["stage"] != "prepared" {
                return Err("migration journal has an unsupported stage".into());
            }
            manifest
        } else {
            let inventory_value = migration_drift_evidence(&inventory);
            let manifest = serde_json::json!({
                "version": 1,
                "stage": "staging",
                "commit": commit,
                "legacyFingerprints": inventory_value.0,
                "managedFingerprints": inventory_value.1,
            });
            let journal_bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
            write_staging_file(&manifest_path, &journal_bytes, &mut created)?;
            copy_hook(&manifest_path, &manifest_path)?;
            manifest
        };

        let mut copies = BTreeMap::<PathBuf, PathBuf>::new();
        for object in &inventory.objects {
            if object.retained || object.category == "portable" {
                continue;
            }
            let source = &object.path;
            if source == &layout.build_lock_path()
                || source == &layout.legacy_lock_path()
                || source == &layout.state_lock_path()
                || source == &layout.layout_path()
                || source == &sentinel
                || source == &journal
                || !source.exists()
            {
                continue;
            }
            let destination = match object.category.as_str() {
                "config" => roots.config.join(source.file_name().unwrap_or_default()),
                "state" => {
                    let Ok(relative) = source.strip_prefix(&legacy) else {
                        continue;
                    };
                    roots.state.join(relative)
                }
                "cache" => roots.state.join("sources"),
                _ => continue,
            };
            if &destination != source {
                copies.insert(source.clone(), destination);
            }
        }
        for (source, destination) in copies {
            copy_staging_object(&source, &destination, &mut created)?;
            copy_hook(&source, &destination)?;
        }
        let catalog_path = legacy.join("plugins.json");
        if catalog_path.exists() {
            let catalog: Value =
                serde_json::from_slice(&fs::read(&catalog_path).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            let sources = crate::catalog::portable_sources_export_bytes(&catalog)?;
            let path = roots.portable.join("sources.json");
            write_staging_file(&path, &sources, &mut created)?;
            copy_hook(&catalog_path, &path)?;
        }
        let lock_path = legacy.join("build/lock.json");
        if lock_path.exists() {
            let mut lock: Value =
                serde_json::from_slice(&fs::read(&lock_path).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            classify_staged_ownership(&mut lock, &inventory);
            let bytes = serde_json::to_vec_pretty(&lock).map_err(|e| e.to_string())?;
            let private_lock = roots.state.join("build/lock.json");
            write_staging_file(&private_lock, &bytes, &mut created)?;
            copy_hook(&lock_path, &private_lock)?;
            let mcp_history = historical_mcp_rows(&lock);
            let history_path = roots.state.join("build/mcp/historical-managed.json");
            let history_bytes =
                serde_json::to_vec_pretty(&mcp_history).map_err(|e| e.to_string())?;
            write_staging_file(&history_path, &history_bytes, &mut created)?;
            copy_hook(&lock_path, &history_path)?;
            let export = crate::catalog::portable_lock_export_bytes(&lock)?;
            let export_path = roots.portable.join("lock.json");
            write_staging_file(&export_path, &export, &mut created)?;
            copy_hook(&private_lock, &export_path)?;
        }
        manifest["stage"] = Value::String("prepared".into());
        let prepared = manifest;
        let bytes = serde_json::to_vec_pretty(&prepared).map_err(|e| e.to_string())?;
        fs::write(&journal, &bytes).map_err(|e| e.to_string())?;
        fs::OpenOptions::new()
            .write(true)
            .open(&journal)
            .and_then(|file| file.sync_all())
            .map_err(|e| format!("prepared migration manifest could not be made durable: {e}"))?;
        if fs::read(&journal).map_err(|e| e.to_string())? != bytes {
            return Err("prepared migration manifest verification failed".into());
        }
        copy_hook(&journal, &journal)?;
        Ok(prepared)
    })();
    if result.is_err() {
        for path in created.into_iter().rev() {
            if let Ok(metadata) = fs::metadata(&path) {
                let mut permissions = metadata.permissions();
                permissions.set_readonly(false);
                let _ = fs::set_permissions(&path, permissions);
            }
            if path.is_dir() {
                let _ = fs::remove_dir(path);
            } else {
                let _ = fs::remove_file(path);
            }
        }
    }
    result
}

fn migration_drift_evidence(inventory: &PortableInventory) -> (Vec<Value>, Vec<Value>) {
    let legacy = inventory
        .objects
        .iter()
        .filter(|object| {
            !object.retained
                && object.category != "portable"
                && !matches!(
                    object.path.file_name().and_then(|name| name.to_str()),
                    Some("layout.json" | "migration.json" | "lifecycle.lock")
                )
        })
        .map(|object| {
            serde_json::json!({
                "path": object.path,
                "fingerprint": object.fingerprint,
            })
        })
        .collect();
    let managed = inventory
        .ownership_rows
        .iter()
        .filter(|row| row.classification == "current")
        .filter_map(|row| {
            Some(serde_json::json!({
                "path": row.live_path,
                "fingerprint": row.current_fingerprint,
            }))
        })
        .collect();
    (legacy, managed)
}

/// Commit a fully staged migration and reload the resulting authority.
pub fn commit_portable_migration(layout: &EffectiveLayout) -> Result<EffectiveLayout, String> {
    commit_portable_migration_with(layout, projection::paths_share_filesystem, |_| {})
}

fn commit_portable_migration_with(
    layout: &EffectiveLayout,
    filesystem_check: impl Fn(&Path, &Path) -> Result<bool, projection::ProjectionError>,
    mut boundary: impl FnMut(&str),
) -> Result<EffectiveLayout, String> {
    let _guard = crate::tool_manager::lock::LifecycleLock::acquire_if_needed()
        .map_err(|error| error.to_string())?;
    if let Some(error) = layout.root_error() {
        return Err(error.to_string());
    }
    let home = ccync_foundation::paths::user_home()
        .ok_or_else(|| "user home is unavailable".to_string())?;
    match filesystem_check(&layout.storage_roots().state.join("build"), &home) {
        Ok(true) => {}
        Ok(false) => return Err("S/build and user home are on different filesystems".into()),
        Err(error) => {
            return Err(format!(
                "filesystem relationship could not be verified: {error}"
            ))
        }
    }

    let journal = layout.migration_journal_path();
    let mut manifest: Value = serde_json::from_slice(
        &fs::read(&journal)
            .map_err(|error| format!("prepared migration manifest is unavailable: {error}"))?,
    )
    .map_err(|error| format!("prepared migration manifest is invalid: {error}"))?;
    if manifest["stage"] == "staging"
        || (manifest["stage"] == "prepared" && !layout.layout_path().exists())
    {
        stage_portable_migration(layout, |_, _| Ok(()))?;
        manifest = serde_json::from_slice(
            &fs::read(&journal)
                .map_err(|error| format!("prepared migration manifest is unavailable: {error}"))?,
        )
        .map_err(|error| format!("prepared migration manifest is invalid: {error}"))?;
    }
    if manifest["version"] != 1 || manifest["stage"] != "prepared" {
        return Err("migration journal is not durably prepared".into());
    }
    let expected = layout_commit(layout);
    if manifest["commit"] != serde_json::to_value(&expected).map_err(|error| error.to_string())? {
        return Err("prepared migration manifest does not match the current storage roots".into());
    }
    boundary("prepared_manifest");

    let layout_bytes = serde_json::to_vec_pretty(&expected).map_err(|error| error.to_string())?;
    publish_atomic(&layout.layout_path(), &layout_bytes)?;
    boundary("layout_published");
    publish_atomic(&layout.migration_sentinel_path(), &layout_bytes)?;
    boundary("sentinel_published");
    EffectiveLayout::load_or_legacy()
        .map_err(|error| format!("committed layout could not be reloaded: {error}"))
}

/// Commit an empty fresh layout before its first mutating write.
pub fn commit_fresh_layout(layout: &EffectiveLayout) -> Result<EffectiveLayout, String> {
    commit_fresh_layout_with(layout, projection::paths_share_filesystem)
}

fn commit_fresh_layout_with(
    layout: &EffectiveLayout,
    filesystem_check: impl Fn(&Path, &Path) -> Result<bool, projection::ProjectionError>,
) -> Result<EffectiveLayout, String> {
    let _guard = crate::tool_manager::lock::LifecycleLock::acquire_if_needed()
        .map_err(|error| error.to_string())?;
    if let Some(error) = layout.root_error() {
        return Err(error.to_string());
    }
    if layout.authority() != EffectiveLayoutAuthority::Fresh
        && layout.authority() != EffectiveLayoutAuthority::MutationBlocked
    {
        return Err("fresh layout commit requires Fresh authority".into());
    }
    let home = ccync_foundation::paths::user_home()
        .ok_or_else(|| "user home is unavailable".to_string())?;
    match filesystem_check(&layout.storage_roots().state.join("build"), &home) {
        Ok(true) => {}
        Ok(false) => return Err("S/build and user home are on different filesystems".into()),
        Err(error) => {
            return Err(format!(
                "filesystem relationship could not be verified: {error}"
            ))
        }
    }
    let bytes =
        serde_json::to_vec_pretty(&layout_commit(layout)).map_err(|error| error.to_string())?;
    publish_atomic(&layout.layout_path(), &bytes)?;
    publish_atomic(&layout.migration_sentinel_path(), &bytes)?;
    EffectiveLayout::load_or_legacy()
        .map_err(|error| format!("committed layout could not be reloaded: {error}"))
}

/// Return drift descriptions for recorded legacy inputs or managed live paths.
pub fn detect_legacy_drift(layout: &EffectiveLayout) -> Result<Vec<String>, String> {
    if layout.authority() != EffectiveLayoutAuthority::Committed {
        return Err("legacy drift detection requires Committed authority".into());
    }
    let manifest: Value = serde_json::from_slice(
        &fs::read(layout.migration_journal_path())
            .map_err(|error| format!("migration journal cannot be read: {error}"))?,
    )
    .map_err(|error| format!("migration journal is invalid: {error}"))?;
    if manifest["stage"] != "prepared"
        || manifest["commit"]
            != serde_json::to_value(layout_commit(layout)).map_err(|e| e.to_string())?
    {
        return Err("migration journal does not match the committed layout".into());
    }
    let mut drift = Vec::new();
    for (kind, key) in [
        ("legacy input", "legacyFingerprints"),
        ("managed live path", "managedFingerprints"),
    ] {
        let rows = manifest[key]
            .as_array()
            .ok_or_else(|| format!("migration journal has no {key} evidence"))?;
        for row in rows {
            let path = row["path"]
                .as_str()
                .ok_or_else(|| format!("{key} evidence has an invalid path"))?;
            let expected = row["fingerprint"].as_str();
            let actual = projection::shared_path_fingerprint(Path::new(path));
            if expected != actual.as_deref() {
                drift.push(format!("{kind} changed: {path}"));
            }
        }
    }
    Ok(drift)
}

/// Return drift descriptions for recorded legacy inputs only.
///
/// Missing legacy inputs are expected after a committed uninstall has pruned
/// the old private tree, so they do not count as drift. Managed live paths are
/// intentionally ignored because sync may change them after migration.
pub fn detect_legacy_input_drift(layout: &EffectiveLayout) -> Result<Vec<String>, String> {
    if layout.authority() != EffectiveLayoutAuthority::Committed {
        return Err("legacy drift detection requires Committed authority".into());
    }
    let manifest: Value = serde_json::from_slice(
        &fs::read(layout.migration_journal_path())
            .map_err(|error| format!("migration journal cannot be read: {error}"))?,
    )
    .map_err(|error| format!("migration journal is invalid: {error}"))?;
    if manifest["stage"] != "prepared"
        || manifest["commit"]
            != serde_json::to_value(layout_commit(layout)).map_err(|e| e.to_string())?
    {
        return Err("migration journal does not match the committed layout".into());
    }
    let rows = manifest["legacyFingerprints"]
        .as_array()
        .ok_or_else(|| "migration journal has no legacyFingerprints evidence".to_string())?;
    let mut drift = Vec::new();
    for row in rows {
        let path = row["path"]
            .as_str()
            .ok_or_else(|| "legacyFingerprints evidence has an invalid path".to_string())?;
        let path = Path::new(path);
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => drift.push(format!("legacy input changed: {}", path.display())),
            Ok(_) => {
                let expected = row["fingerprint"].as_str();
                let actual = projection::shared_path_fingerprint(path);
                if expected != actual.as_deref() {
                    drift.push(format!("legacy input changed: {}", path.display()));
                }
            }
        }
    }
    Ok(drift)
}

fn layout_commit(layout: &EffectiveLayout) -> StorageLayoutCommit {
    let roots = layout.storage_roots();
    StorageLayoutCommit {
        layout_version: 1,
        roots: vec![
            LayoutRootIdentity {
                name: "portable".into(),
                path: roots.portable.clone(),
            },
            LayoutRootIdentity {
                name: "config".into(),
                path: roots.config.clone(),
            },
            LayoutRootIdentity {
                name: "state".into(),
                path: roots.state.clone(),
            },
            LayoutRootIdentity {
                name: "cache".into(),
                path: roots.cache.clone(),
            },
        ],
    }
}

fn publish_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if path.exists() {
        return if fs::read(path).map_err(|error| error.to_string())? == bytes {
            Ok(())
        } else {
            Err(format!(
                "layout commit conflicts with existing bytes: {}",
                path.display()
            ))
        };
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("commit path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temp = parent.join(format!(
        ".{}.pending",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    if temp.exists() && fs::read(&temp).map_err(|error| error.to_string())? != bytes {
        fs::remove_file(&temp).map_err(|error| error.to_string())?;
    }
    if !temp.exists() {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|error| error.to_string())?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|error| error.to_string())?;
    }
    match fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temp);
            Err(format!(
                "atomic layout publication failed for {}: {error}",
                path.display()
            ))
        }
    }
}

fn historical_mcp_rows(lock: &Value) -> Vec<Value> {
    lock.pointer("/_ccyncProjection/historicalOwnership")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|row| {
            row.get("category")
                .or_else(|| row.get("kind"))
                .and_then(Value::as_str)
                .is_some_and(|label| label.to_ascii_lowercase().contains("mcp"))
        })
        .cloned()
        .collect()
}

fn write_staging_file(path: &Path, bytes: &[u8], created: &mut Vec<PathBuf>) -> Result<(), String> {
    if path.exists() {
        return if fs::read(path).map_err(|error| error.to_string())? == bytes {
            Ok(())
        } else {
            Err(format!(
                "staging destination conflicts with existing bytes: {}",
                path.display()
            ))
        };
    }
    if let Some(parent) = path.parent() {
        create_staging_dirs(parent, created)?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    created.push(path.to_path_buf());
    file.write_all(bytes).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    if fs::read(path).map_err(|e| e.to_string())? != bytes {
        return Err(format!("staging verification failed: {}", path.display()));
    }
    Ok(())
}

fn copy_staging_object(
    source: &Path,
    destination: &Path,
    created: &mut Vec<PathBuf>,
) -> Result<(), String> {
    if source.is_dir() {
        create_staging_dirs(destination, created)?;
        for entry in fs::read_dir(source).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            copy_staging_object(&entry.path(), &destination.join(entry.file_name()), created)?;
        }
        copy_staging_permissions(source, destination)?;
    } else {
        write_staging_file(
            destination,
            &fs::read(source).map_err(|e| e.to_string())?,
            created,
        )?;
        copy_staging_permissions(source, destination)?;
    }
    Ok(())
}

fn copy_staging_permissions(source: &Path, destination: &Path) -> Result<(), String> {
    let source_metadata = fs::metadata(source).map_err(|e| e.to_string())?;
    fs::set_permissions(destination, source_metadata.permissions()).map_err(|e| e.to_string())?;
    let destination_metadata = fs::metadata(destination).map_err(|e| e.to_string())?;
    if source_metadata.permissions().readonly() != destination_metadata.permissions().readonly() {
        return Err(format!(
            "staging permission verification failed: {}",
            destination.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if source_metadata.permissions().mode() != destination_metadata.permissions().mode() {
            return Err(format!(
                "staging permission verification failed: {}",
                destination.display()
            ));
        }
    }
    Ok(())
}

fn create_staging_dirs(path: &Path, created: &mut Vec<PathBuf>) -> Result<(), String> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        create_staging_dirs(parent, created)?;
    }
    fs::create_dir(path).map_err(|e| e.to_string())?;
    created.push(path.to_path_buf());
    Ok(())
}

fn classify_staged_ownership(lock: &mut Value, inventory: &PortableInventory) {
    let Some(projection) = lock
        .get_mut("_ccyncProjection")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    let mut historical = projection
        .get("historicalOwnership")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for row in &inventory.ownership_rows {
        if row.classification == "historical" && row.category != "historicalOwnership" {
            historical.push(row.recorded.clone());
        }
    }
    projection.insert("historicalOwnership".into(), Value::Array(historical));
    for key in [
        "agentProjectionPaths",
        "codexAgentProjectionPaths",
        "opencodeAgentProjectionPaths",
        "commandProjectionPaths",
        "discussSkillProjectionPaths",
        "legacyProjectionPaths",
        "skillProjectionPaths",
        "agyRootProjectionPaths",
        "directSkillProjectionPaths",
        "directAgentProjectionPaths",
        "directCommandProjectionPaths",
        "sourceLinkProjectionPaths",
    ] {
        if let Some(rows) = projection.get_mut(key).and_then(Value::as_array_mut) {
            rows.retain(|row| {
                let path = row
                    .as_str()
                    .or_else(|| row.get("path").and_then(Value::as_str));
                inventory.ownership_rows.iter().any(|item| {
                    item.classification == "current"
                        && item.recorded == *row
                        && item.live_path.as_deref().and_then(|p| p.to_str()) == path
                })
            });
        }
    }
}

/// Build a complete, read-only view of migration inputs and retained data.
pub fn build_portable_inventory(layout: &EffectiveLayout) -> PortableInventory {
    build_portable_inventory_with_filesystem_check(layout, projection::paths_share_filesystem)
}

fn build_portable_inventory_with_filesystem_check(
    layout: &EffectiveLayout,
    filesystem_check: impl Fn(&Path, &Path) -> Result<bool, projection::ProjectionError>,
) -> PortableInventory {
    use ccync_foundation::provider::ProviderLifecycleJournal;

    let mut blockers = Vec::new();
    let roots = layout.storage_roots();
    let mut objects = Vec::new();
    let mut add_object = |category: &str, path: PathBuf, retained: bool| {
        let fingerprint = projection::shared_path_fingerprint(&path);
        objects.push(InventoryObject {
            category: category.to_owned(),
            path,
            fingerprint,
            retained,
        });
    };

    // These are the path-backed objects represented by the effective storage map.
    for (category, path) in [
        ("portable", roots.portable.join("sources.json")),
        ("portable", roots.portable.join("lock.json")),
        ("config", layout.config_path()),
        ("config", layout.plugins_path()),
        ("config", layout.agents_md_path()),
        ("config", layout.mcp_path()),
        ("config", layout.tools_path()),
        ("state", layout.build_lock_path()),
        ("state", layout.state_base().join("projection.txn.json")),
        ("state", layout.state_base().join("preimages")),
        ("state", layout.state_base().join("standalone-publications")),
        ("state", layout.state_base().join("standalone-withdrawals")),
        (
            "state",
            layout.state_base().join("mcp/projected-state.json"),
        ),
        ("state", layout.state_base().join("mcp/projection.txn.json")),
        ("state", layout.tool_state_path()),
        ("state", layout.tool_artifacts_root()),
        ("state", layout.tool_operation_journal_path()),
        ("state", layout.generated_mcp_path()),
        ("state", layout.routes_path()),
        ("state", layout.dist_path()),
        ("state", layout.backups_path()),
        ("state", layout.state_lock_path()),
        ("state", layout.legacy_lock_path()),
        ("state", layout.layout_path()),
        ("state", layout.migration_journal_path()),
        ("state", layout.migration_sentinel_path()),
        ("cache", layout.cache_path()),
        (
            "state",
            ccync_foundation::paths::install_receipt_path().unwrap_or_default(),
        ),
    ] {
        if !path.as_os_str().is_empty() {
            add_object(category, path, false);
        }
    }
    if let Some(path) = layout.sources_export_path() {
        add_object("portable", path, false);
    }
    if let Some(path) = layout.lock_export_path() {
        add_object("portable", path, false);
    }

    // Live surfaces and receipts are retained even when the layout changes.
    if let Some(home) = ccync_foundation::paths::user_home() {
        for (category, path) in [
            ("live-surface", home.join(".claude")),
            ("live-surface", home.join(".codex")),
            ("live-surface", home.join(".copilot")),
            ("live-surface", home.join(".gemini")),
            ("live-surface", home.join(".github")),
            ("live-surface", home.join(".opencode")),
            ("live-surface", home.join(".cursor")),
            ("live-surface", home.join(".vscode")),
            ("live-surface", home.join(".agents")),
            (
                "install-receipt",
                home.join(".ccync/install-receipts/ccync.json"),
            ),
        ] {
            add_object(category, path, true);
        }
    }

    let provider_journal_path = layout.state_base().join("projection.txn.json");
    let (provider_journal, provider_intents, standalone_intents, standalone_withdrawal_intents) =
        match fs::read_to_string(&provider_journal_path) {
            Ok(raw) => match ProviderLifecycleJournal::from_json_str(&raw) {
                Ok(journal) => {
                    for intent in &journal.intents {
                        for preimage in &intent.preimage_refs {
                            let saved = preimage.saved.join_to(&layout.state_base());
                            if !saved.exists() {
                                blockers.push(format!(
                                    "provider intent references missing preimage evidence: {}",
                                    saved.display()
                                ));
                            }
                        }
                    }
                    for intent in &journal.standalone_intents {
                        if let Some(preimage) = &intent.preimage_ref {
                            let saved = preimage.saved.join_to(&layout.state_base());
                            if !saved.exists() {
                                blockers.push(format!(
                                    "standalone intent references missing preimage evidence: {}",
                                    saved.display()
                                ));
                            }
                        }
                    }
                    for intent in &journal.standalone_withdrawal_intents {
                        let saved = intent.preimage_ref.saved.join_to(&layout.state_base());
                        if !saved.exists() {
                            blockers.push(format!(
                                "standalone withdrawal intent references missing preimage evidence: {}",
                                saved.display()
                            ));
                        }
                    }
                    (Some(journal), Vec::new(), Vec::new(), Vec::new())
                }
                Err(error) => {
                    blockers.push(format!(
                        "provider journal is unsupported or invalid: {error}"
                    ));
                    (None, Vec::new(), Vec::new(), Vec::new())
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (
                Some(ProviderLifecycleJournal::default()),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ),
            Err(error) => {
                blockers.push(format!("provider journal cannot be read: {error}"));
                (None, Vec::new(), Vec::new(), Vec::new())
            }
        };
    let provider_intents = provider_journal
        .as_ref()
        .map(|j| {
            j.intents
                .iter()
                .filter_map(|v| serde_json::to_value(v).ok())
                .collect()
        })
        .unwrap_or(provider_intents);
    let standalone_intents = provider_journal
        .as_ref()
        .map(|j| {
            j.standalone_intents
                .iter()
                .filter_map(|v| serde_json::to_value(v).ok())
                .collect()
        })
        .unwrap_or(standalone_intents);
    let standalone_withdrawal_intents = provider_journal
        .as_ref()
        .map(|j| {
            j.standalone_withdrawal_intents
                .iter()
                .filter_map(|v| serde_json::to_value(v).ok())
                .collect()
        })
        .unwrap_or(standalone_withdrawal_intents);

    let build_lock = layout.build_lock_path();
    let lock_value = read_json_value(&build_lock, "provider lock", &mut blockers);
    let provider_native_rows = lock_value
        .as_ref()
        .and_then(|v| ccync_foundation::provider::ProviderLifecycleLock::from_lock(v).ok())
        .map(|section| {
            section
                .committed
                .iter()
                .filter_map(|row| serde_json::to_value(row).ok())
                .collect()
        })
        .unwrap_or_default();
    let standalone_proofs = lock_value
        .as_ref()
        .and_then(|v| v.get("_ccyncProjection"))
        .and_then(|v| v.get("standalonePublications"))
        .map(|proofs| match proofs {
            Value::Array(rows) => rows.clone(),
            Value::Object(entries) => entries
                .iter()
                .map(|(identity, proof)| serde_json::json!({"identity": identity, "proof": proof}))
                .collect(),
            _ => Vec::new(),
        })
        .unwrap_or_default();

    let mut ownership_rows = Vec::new();
    let mut counts_without_fingerprint = BTreeMap::new();
    if let Some(projection_lock) = lock_value.as_ref().and_then(|v| v.get("_ccyncProjection")) {
        const OWNERSHIP_CATEGORIES: &[&str] = &[
            "agentProjectionPaths",
            "codexAgentProjectionPaths",
            "opencodeAgentProjectionPaths",
            "commandProjectionPaths",
            "discussSkillProjectionPaths",
            "legacyProjectionPaths",
            "skillProjectionPaths",
            "agyRootProjectionPaths",
            "directSkillProjectionPaths",
            "directAgentProjectionPaths",
            "directCommandProjectionPaths",
            "sourceLinkProjectionPaths",
        ];
        for category in OWNERSHIP_CATEGORIES {
            if let Some(rows) = projection_lock.get(*category).and_then(Value::as_array) {
                for row in rows {
                    let live_path = row
                        .as_str()
                        .or_else(|| row.get("path").and_then(Value::as_str))
                        .and_then(projection::decode_managed_artifact_key)
                        .map(PathBuf::from);
                    let current_fingerprint = live_path
                        .as_deref()
                        .and_then(projection::shared_path_fingerprint);
                    let recorded_fingerprint = row
                        .get("fingerprint")
                        .or_else(|| row.get("contentFingerprint"))
                        .or_else(|| row.get("ownedFingerprint"))
                        .and_then(Value::as_str);
                    let matches_recorded_fingerprint = recorded_fingerprint
                        .map(|fingerprint| current_fingerprint.as_deref() == Some(fingerprint));
                    if recorded_fingerprint.is_none() {
                        *counts_without_fingerprint
                            .entry((*category).to_owned())
                            .or_insert(0) += 1;
                    }
                    ownership_rows.push(OwnershipInventoryRow {
                        category: (*category).to_owned(),
                        classification: if matches_recorded_fingerprint == Some(true) {
                            "current".to_owned()
                        } else {
                            "historical".to_owned()
                        },
                        recorded: row.clone(),
                        live_path,
                        current_fingerprint,
                        matches_recorded_fingerprint,
                    });
                }
            }
        }
        if let Some(rows) = projection_lock
            .get("historicalOwnership")
            .and_then(Value::as_array)
        {
            for row in rows {
                let live_path = row
                    .get("path")
                    .and_then(Value::as_str)
                    .and_then(projection::decode_managed_artifact_key)
                    .map(PathBuf::from);
                let current_fingerprint = live_path
                    .as_deref()
                    .and_then(projection::shared_path_fingerprint);
                let recorded_fingerprint = row
                    .get("fingerprint")
                    .or_else(|| row.get("contentFingerprint"))
                    .or_else(|| row.get("ownedFingerprint"))
                    .and_then(Value::as_str);
                if recorded_fingerprint.is_none() {
                    *counts_without_fingerprint
                        .entry("historicalOwnership".to_owned())
                        .or_insert(0) += 1;
                }
                let matches_recorded_fingerprint = recorded_fingerprint
                    .map(|fingerprint| current_fingerprint.as_deref() == Some(fingerprint));
                // Rows already in `historicalOwnership` never regain authority.
                ownership_rows.push(OwnershipInventoryRow {
                    category: "historicalOwnership".to_owned(),
                    classification: "historical".to_owned(),
                    recorded: row.clone(),
                    live_path,
                    current_fingerprint,
                    matches_recorded_fingerprint,
                });
            }
        }
    }

    let mcp_inspection = mcp::inspect_mcp_journal(&layout.generated_mcp_path());
    let mcp_journal = match mcp_inspection {
        mcp::McpJournalInspection::Absent => serde_json::json!({"state":"absent"}),
        mcp::McpJournalInspection::Pending { entries } => {
            serde_json::json!({"state":"pending","entries":entries.iter().map(|e| serde_json::json!({"host":e.host,"name":e.name,"pre":e.pre,"post":e.post})).collect::<Vec<_>>()})
        }
        mcp::McpJournalInspection::UnsupportedVersion { version } => {
            blockers.push(format!("MCP journal has unsupported version {version}"));
            serde_json::json!({"state":"unsupported","version":version})
        }
        mcp::McpJournalInspection::Corrupt { reason } => {
            blockers.push(format!("MCP journal is invalid: {reason}"));
            serde_json::json!({"state":"invalid","reason":reason})
        }
    };
    let tool_journal_path = layout.tool_operation_journal_path();
    let tool_journal = match read_json_value(&tool_journal_path, "tool journal", &mut blockers) {
        Some(value) => match value.get("version").and_then(Value::as_u64) {
            Some(1) => serde_json::json!({"state":"present","value":value}),
            Some(version) => {
                blockers.push(format!("tool journal has unsupported version {version}"));
                serde_json::json!({"state":"unsupported","version":version})
            }
            None => {
                blockers.push("tool journal has no valid version".into());
                serde_json::json!({"state":"invalid"})
            }
        },
        None if !tool_journal_path.exists() => serde_json::json!({"state":"absent"}),
        None => serde_json::json!({"state":"invalid"}),
    };

    if let Some(home) = ccync_foundation::paths::user_home() {
        match filesystem_check(&layout.state_base(), &home) {
            Ok(true) => {}
            Ok(false) => blockers.push("S/build and user home are on different filesystems".into()),
            Err(error) => blockers.push(format!(
                "filesystem relationship could not be verified: {error}"
            )),
        }
    } else {
        blockers.push("user home is unavailable".into());
    }
    let older = ccync_foundation::paths::ccync_home()
        .is_some_and(|home| !build_migration_plan(&home).moves.is_empty());
    PortableInventory {
        objects,
        provider_native_rows,
        standalone_proofs,
        provider_intents,
        standalone_intents,
        standalone_withdrawal_intents,
        mcp_journal,
        tool_journal,
        ownership_rows,
        counts_without_fingerprint,
        pending_older_sublayout_fixup: older,
        blocked: !blockers.is_empty(),
        blockers,
    }
}

fn read_json_value(path: &Path, label: &str, blockers: &mut Vec<String>) -> Option<Value> {
    match fs::read_to_string(path) {
        Ok(raw) => match serde_json::from_str(&raw) {
            Ok(value) => Some(value),
            Err(error) => {
                blockers.push(format!("{label} is invalid: {error}"));
                None
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            blockers.push(format!("{label} cannot be read: {error}"));
            None
        }
    }
}

#[cfg(test)]
mod migration_inventory_tests {
    use super::*;

    struct IsolatedInventoryHome {
        _guard: Option<std::sync::MutexGuard<'static, ()>>,
        home: tempfile::TempDir,
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl IsolatedInventoryHome {
        fn new() -> Self {
            Self::with_guard(Some(crate::test_support::env_guard()))
        }

        fn new_under_existing_guard() -> Self {
            Self::with_guard(None)
        }

        fn with_guard(guard: Option<std::sync::MutexGuard<'static, ()>>) -> Self {
            let home = tempfile::tempdir().unwrap();
            let keys = [
                "HOME",
                "USERPROFILE",
                "APPDATA",
                "LOCALAPPDATA",
                "XDG_CONFIG_HOME",
                "XDG_STATE_HOME",
                "XDG_CACHE_HOME",
            ];
            let saved = keys
                .iter()
                .map(|key| (*key, std::env::var_os(key)))
                .collect::<Vec<_>>();
            for key in keys {
                let value = match key {
                    "APPDATA" => home.path().join("appdata"),
                    "LOCALAPPDATA" => home.path().join("localappdata"),
                    "XDG_CONFIG_HOME" => home.path().join("xdg-config"),
                    "XDG_STATE_HOME" => home.path().join("xdg-state"),
                    "XDG_CACHE_HOME" => home.path().join("xdg-cache"),
                    _ => home.path().to_path_buf(),
                };
                unsafe {
                    std::env::set_var(key, value);
                }
            }
            fs::create_dir_all(home.path().join(".ccync")).unwrap();
            Self {
                _guard: guard,
                home,
                saved,
            }
        }
    }

    impl Drop for IsolatedInventoryHome {
        fn drop(&mut self) {
            fn clear_readonly(path: &Path) {
                if let Ok(metadata) = fs::metadata(path) {
                    if metadata.is_dir() {
                        if let Ok(entries) = fs::read_dir(path) {
                            for entry in entries.flatten() {
                                clear_readonly(&entry.path());
                            }
                        }
                    }
                    let mut permissions = metadata.permissions();
                    if permissions.readonly() {
                        permissions.set_readonly(false);
                        let _ = fs::set_permissions(path, permissions);
                    }
                }
            }
            clear_readonly(self.home.path());
            for (key, value) in &self.saved {
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        fn visit(root: &Path, path: &Path, output: &mut Vec<(PathBuf, Vec<u8>)>) {
            let Ok(entries) = fs::read_dir(path) else {
                return;
            };
            for entry in entries.flatten() {
                let child = entry.path();
                if child.is_dir() {
                    visit(root, &child, output);
                } else if let Ok(bytes) = fs::read(&child) {
                    output.push((child.strip_prefix(root).unwrap().to_path_buf(), bytes));
                }
            }
        }
        let mut output = Vec::new();
        visit(root, root, &mut output);
        output.sort_by(|a, b| a.0.cmp(&b.0));
        output
    }

    fn migration_staging_fixture() -> (IsolatedInventoryHome, EffectiveLayout, Vec<u8>) {
        let home = IsolatedInventoryHome::new();
        let (layout, plugins) = prepare_migration_staging_fixture(&home);
        (home, layout, plugins)
    }

    fn migration_staging_fixture_under_existing_guard(
    ) -> (IsolatedInventoryHome, EffectiveLayout, Vec<u8>) {
        let home = IsolatedInventoryHome::new_under_existing_guard();
        let (layout, plugins) = prepare_migration_staging_fixture(&home);
        (home, layout, plugins)
    }

    fn prepare_migration_staging_fixture(
        home: &IsolatedInventoryHome,
    ) -> (EffectiveLayout, Vec<u8>) {
        let legacy = home.home.path().join(".ccync");
        fs::create_dir_all(legacy.join("build")).unwrap();
        fs::write(legacy.join("build/lifecycle.lock"), b"legacy lock").unwrap();
        fs::create_dir_all(legacy.join("cache")).unwrap();
        fs::write(legacy.join("cache/source.bin"), b"cached source").unwrap();
        let readonly_source = legacy.join("cache/readonly.bin");
        fs::write(&readonly_source, b"read-only source").unwrap();
        let mut permissions = fs::metadata(&readonly_source).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&readonly_source, permissions).unwrap();
        fs::write(legacy.join("config.json"), br#"{"devMode":true}"#).unwrap();
        let plugins = b"{ \"plugins\": [] }\n".to_vec();
        fs::write(legacy.join("plugins.json"), &plugins).unwrap();
        let owned_path = home.home.path().join("owned-skill");
        let edited_path = home.home.path().join("edited-skill");
        fs::write(&owned_path, "owned bytes").unwrap();
        fs::write(&edited_path, "edited bytes").unwrap();
        let owned_fingerprint = projection::shared_path_fingerprint(&owned_path).unwrap();
        fs::write(
            legacy.join("build/lock.json"),
            serde_json::to_vec(&serde_json::json!({
                "resolvedPlugins": [],
                "_ccyncProjection": {
                    "agentProjectionPaths": [
                        {"path": owned_path, "fingerprint": owned_fingerprint},
                        {"path": edited_path, "fingerprint": "mismatch"}
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let layout = EffectiveLayout::load_or_legacy().unwrap();
        (layout, plugins)
    }

    #[test]
    fn migration_commit_recovery() {
        if let Ok(hook_index) = std::env::var("CCYNC_MIGRATION_STAGING_ABORT_AT") {
            let hook_index = hook_index.parse::<usize>().unwrap();
            let layout = EffectiveLayout::load_or_legacy().unwrap();
            let mut calls = 0;
            stage_portable_migration(&layout, |_, _| {
                calls += 1;
                if calls == hook_index {
                    eprintln!("aborting at staging hook {hook_index}");
                    std::process::abort();
                }
                Ok(())
            })
            .unwrap();
            panic!("staging child did not reach hook {hook_index}");
        }

        if let Ok(boundary) = std::env::var("CCYNC_MIGRATION_RECOVERY_CHILD") {
            let layout = EffectiveLayout::load_or_legacy().unwrap();
            stage_portable_migration(&layout, |_, _| Ok(())).unwrap();
            commit_portable_migration_with(
                &layout,
                |_, _| Ok(true),
                |name| {
                    if name == boundary {
                        std::process::abort();
                    }
                },
            )
            .unwrap();
            panic!("recovery child did not reach boundary {boundary}");
        }

        let _env_guard = crate::test_support::env_guard();
        let (home, layout, _) = migration_staging_fixture_under_existing_guard();
        let mut hook_count = 0;
        stage_portable_migration(&layout, |_, _| {
            hook_count += 1;
            Ok(())
        })
        .unwrap();
        assert!(hook_count > 0, "staging must invoke its copy hook");
        drop(home);

        for hook_index in 1..=hook_count {
            let (_home, layout, plugins) = migration_staging_fixture_under_existing_guard();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("migrate::migration_inventory_tests::migration_commit_recovery")
                .arg("--nocapture")
                .env("CCYNC_MIGRATION_STAGING_ABORT_AT", hook_index.to_string())
                .output()
                .unwrap();
            let child_output = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                !output.status.success(),
                "child must abort at staging hook {hook_index}"
            );
            assert!(
                child_output.contains(&format!("aborting at staging hook {hook_index}")),
                "child did not reach staging hook {hook_index}: {child_output}"
            );

            let interrupted = EffectiveLayout::load_or_legacy().unwrap();
            assert_eq!(interrupted.authority(), EffectiveLayoutAuthority::Legacy);
            assert!(!layout.layout_path().exists());
            assert!(!layout.migration_sentinel_path().exists());

            let committed = commit_portable_migration(&layout).unwrap();
            assert_eq!(committed.authority(), EffectiveLayoutAuthority::Committed);
            assert_eq!(
                fs::read(committed.storage_roots().config.join("plugins.json")).unwrap(),
                plugins,
                "staging recovery must preserve C/plugins.json byte-for-byte"
            );
            assert!(detect_legacy_drift(&committed).unwrap().is_empty());
        }

        for boundary in [
            "prepared_manifest",
            "layout_published",
            "sentinel_published",
        ] {
            let (home, layout, _) = migration_staging_fixture_under_existing_guard();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("migrate::migration_inventory_tests::migration_commit_recovery")
                .arg("--nocapture")
                .env("CCYNC_MIGRATION_RECOVERY_CHILD", boundary)
                .status()
                .unwrap();
            assert!(!status.success(), "child must abort at {boundary}");

            let interrupted = EffectiveLayout::load_or_legacy().unwrap();
            match boundary {
                "prepared_manifest" => {
                    assert_eq!(interrupted.authority(), EffectiveLayoutAuthority::Legacy);
                    assert!(!layout.layout_path().exists());
                    assert!(!layout.migration_sentinel_path().exists());
                }
                "layout_published" => {
                    assert_eq!(
                        interrupted.authority(),
                        EffectiveLayoutAuthority::MutationBlocked
                    );
                }
                "sentinel_published" => {
                    assert_eq!(interrupted.authority(), EffectiveLayoutAuthority::Committed);
                }
                _ => unreachable!(),
            }

            if interrupted.authority() != EffectiveLayoutAuthority::Committed {
                let resumed = commit_portable_migration(&layout).unwrap();
                assert_eq!(resumed.authority(), EffectiveLayoutAuthority::Committed);
            }
            let committed = EffectiveLayout::load_or_legacy().unwrap();
            assert_eq!(committed.authority(), EffectiveLayoutAuthority::Committed);
            assert!(detect_legacy_drift(&committed).unwrap().is_empty());
            drop(home);
        }
    }

    #[test]
    fn migration_commit_recovery_fresh_layout_has_no_ownership() {
        let _home = IsolatedInventoryHome::new();
        let layout = EffectiveLayout::load_or_legacy().unwrap();
        assert_eq!(layout.authority(), EffectiveLayoutAuthority::Fresh);

        let committed = commit_fresh_layout(&layout).unwrap();

        assert_eq!(committed.authority(), EffectiveLayoutAuthority::Committed);
        assert!(!committed.build_lock_path().exists());
    }

    #[test]
    fn migration_commit_recovery_refuses_cross_filesystem_without_layout() {
        let _home = IsolatedInventoryHome::new();
        let layout = EffectiveLayout::load_or_legacy().unwrap();

        let error = commit_fresh_layout_with(&layout, |_, _| Ok(false)).unwrap_err();

        assert!(error.contains("different filesystems"));
        assert!(!layout.layout_path().exists());
    }

    #[test]
    fn migration_commit_recovery_detects_third_hash() {
        let (home, layout, _) = migration_staging_fixture();
        stage_portable_migration(&layout, |_, _| Ok(())).unwrap();
        let committed = commit_portable_migration(&layout).unwrap();
        let legacy_config = home.home.path().join(".ccync/config.json");
        fs::write(&legacy_config, b"third hash").unwrap();

        let drift = detect_legacy_drift(&committed).unwrap();

        assert!(drift.iter().any(|item| item.contains("config.json")));
    }

    #[test]
    fn migration_staging_copies_exact_plugins_and_writes_pure_exports() {
        let (home, layout, plugins) = migration_staging_fixture();
        let manifest = stage_portable_migration(&layout, |_, _| Ok(())).unwrap();
        assert_eq!(manifest["stage"], "prepared");
        assert_eq!(
            fs::read(layout.storage_roots().config.join("plugins.json")).unwrap(),
            plugins
        );
        assert_eq!(
            fs::read(layout.storage_roots().state.join("sources/source.bin")).unwrap(),
            b"cached source"
        );
        let staged_readonly = layout.storage_roots().state.join("sources/readonly.bin");
        let staged_metadata = fs::metadata(staged_readonly).unwrap();
        assert!(staged_metadata.permissions().readonly());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let source_metadata =
                fs::metadata(home.home.path().join(".ccync/cache/readonly.bin")).unwrap();
            assert_eq!(
                staged_metadata.permissions().mode(),
                source_metadata.permissions().mode()
            );
        }
        assert!(
            !layout
                .storage_roots()
                .state
                .join("build/lifecycle.lock")
                .exists(),
            "legacy lifecycle lock must not be staged"
        );
        let staged_catalog: Value = serde_json::from_slice(&plugins).unwrap();
        assert_eq!(
            fs::read(layout.storage_roots().portable.join("sources.json")).unwrap(),
            crate::catalog::portable_sources_export_bytes(&staged_catalog).unwrap()
        );
        let staged_lock: Value = serde_json::from_slice(
            &fs::read(layout.storage_roots().state.join("build/lock.json")).unwrap(),
        )
        .unwrap();
        let projection_lock = &staged_lock["_ccyncProjection"];
        assert_eq!(
            projection_lock["agentProjectionPaths"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            projection_lock["historicalOwnership"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            fs::read(layout.storage_roots().portable.join("lock.json")).unwrap(),
            crate::catalog::portable_lock_export_bytes(&staged_lock).unwrap()
        );
        assert_eq!(layout.authority(), EffectiveLayoutAuthority::Legacy);
    }

    #[test]
    fn migration_staging_hook_failures_leave_legacy_bytes_and_remove_staging_copies() {
        let (home, layout, _) = migration_staging_fixture();
        let mut hook_count = 0;
        stage_portable_migration(&layout, |_, _| {
            hook_count += 1;
            Ok(())
        })
        .unwrap();
        drop(home);
        for failing_call in 1..=hook_count {
            let (home, layout, _) = migration_staging_fixture();
            let legacy = home.home.path().join(".ccync");
            let before = snapshot(&legacy);
            let mut calls = 0;
            let result = stage_portable_migration(&layout, |_, _| {
                calls += 1;
                if calls == failing_call {
                    Err("injected copy failure".into())
                } else {
                    Ok(())
                }
            });
            assert!(result.is_err(), "hook call {failing_call} must fail");
            assert_eq!(
                snapshot(&legacy),
                before,
                "legacy data changed at hook call {failing_call}"
            );
            assert!(!layout.migration_sentinel_path().exists());
            assert!(!layout.migration_journal_path().exists());
            assert!(!layout
                .storage_roots()
                .state
                .join("build/lifecycle.lock")
                .exists());
            assert!(!layout.layout_path().exists());
            assert!(!layout.storage_roots().config.join("plugins.json").exists());
            assert!(!layout
                .storage_roots()
                .portable
                .join("sources.json")
                .exists());
            assert_eq!(layout.authority(), EffectiveLayoutAuthority::Legacy);
            drop(home);
        }
    }

    #[test]
    fn pre_migration_recovery_runs_all_owners_when_journals_are_absent() {
        let _home = IsolatedInventoryHome::new();
        let layout = EffectiveLayout::load_or_legacy().unwrap();
        let guard = crate::tool_manager::lock::LifecycleLock::acquire().unwrap();

        let inventory = recover_before_migration(&layout, &guard).unwrap();

        assert!(!inventory.blocked);
    }

    #[test]
    fn pre_migration_recovery_keeps_unsupported_mcp_journal_unchanged() {
        let _home = IsolatedInventoryHome::new();
        let layout = EffectiveLayout::load_or_legacy().unwrap();
        let managed = layout.generated_mcp_path();
        fs::create_dir_all(managed.parent().unwrap()).unwrap();
        let journal_path = managed.parent().unwrap().join("projection.txn.json");
        let original = br#"{"version":999,"transaction_id":"pending","entries":[]}"#;
        fs::write(&journal_path, original).unwrap();
        let guard = crate::tool_manager::lock::LifecycleLock::acquire().unwrap();

        let error = recover_before_migration(&layout, &guard).unwrap_err();

        assert!(error.contains("unsupported version 999"), "{error}");
        assert_eq!(fs::read(&journal_path).unwrap(), original);
    }

    #[test]
    fn pre_migration_recovery_keeps_invalid_provider_journal_unchanged() {
        let _home = IsolatedInventoryHome::new();
        let layout = EffectiveLayout::load_or_legacy().unwrap();
        let journal_path = layout.state_base().join("projection.txn.json");
        fs::create_dir_all(journal_path.parent().unwrap()).unwrap();
        let original = br#"{"schemaVersion":999}"#;
        fs::write(&journal_path, original).unwrap();
        let guard = crate::tool_manager::lock::LifecycleLock::acquire().unwrap();

        let error = recover_before_migration(&layout, &guard).unwrap_err();

        assert!(error.contains("native intent journal unreadable"));
        assert_eq!(fs::read(&journal_path).unwrap(), original);
    }

    #[test]
    fn pre_migration_recovery_blocks_native_intent_without_committed_owner_unchanged() {
        let _home = IsolatedInventoryHome::new();
        let layout = EffectiveLayout::load_or_legacy().unwrap();
        let journal_path = layout.state_base().join("projection.txn.json");
        fs::create_dir_all(journal_path.parent().unwrap()).unwrap();
        let original = br#"{"schemaVersion":3,"intents":[{"target":"codex","ownershipFingerprints":[],"scopedPaths":[],"preimageRefs":[],"desiredGeneration":{"source":"sample-source","generation":"generation-2","packageDigest":"sha256:desired-package"},"phase":"staged"}],"standaloneIntents":[],"standaloneWithdrawalIntents":[]}"#;
        fs::write(&journal_path, original).unwrap();
        let guard = crate::tool_manager::lock::LifecycleLock::acquire().unwrap();

        let error = recover_before_migration(&layout, &guard).unwrap_err();

        assert!(
            error.contains("native intent journal still has pending intents"),
            "{error}"
        );
        assert_eq!(fs::read(&journal_path).unwrap(), original);
    }

    #[test]
    fn pre_migration_recovery_blocks_publication_without_source_path_unchanged() {
        use ccync_foundation::provider::{
            IntentPhase, PackageDigest, PathRoot, ProviderLifecycleJournal, RelativePath,
            ScopedPath, StandalonePreimage, StandalonePublicationIntent,
        };

        let _home = IsolatedInventoryHome::new();
        let layout = EffectiveLayout::load_or_legacy().unwrap();
        let journal_path = layout.state_base().join("projection.txn.json");
        fs::create_dir_all(journal_path.parent().unwrap()).unwrap();
        let mut journal = ProviderLifecycleJournal::default();
        journal
            .standalone_intents
            .push(StandalonePublicationIntent {
                operation_id: "publish-pending".into(),
                source: r#"{"origin":"plugin:example","revision":"r1"}"#.into(),
                source_fingerprint: PackageDigest::new("sha256:source-proof").unwrap(),
                targets: vec!["claude".into()],
                destination: ScopedPath {
                    root: PathRoot::UserHome,
                    path: RelativePath::new(".claude/skills/example").unwrap(),
                },
                preimage: StandalonePreimage::Absent,
                link_kind: None,
                preimage_ref: None,
                preimage_fingerprint: None,
                stage_path: RelativePath::new("standalone-publications/publish-pending/stage")
                    .unwrap(),
                backup_path: RelativePath::new("standalone-publications/publish-pending/backup")
                    .unwrap(),
                expected_tree_fingerprint: PackageDigest::new("sha256:tree-proof").unwrap(),
                registry_before: None,
                registry_after: None,
                phase: IntentPhase::Publishing,
            });
        let original = journal.to_json_string().unwrap();
        fs::write(&journal_path, &original).unwrap();
        let guard = crate::tool_manager::lock::LifecycleLock::acquire().unwrap();

        let error = recover_before_migration(&layout, &guard).unwrap_err();

        assert!(error.contains("saved source tree evidence is missing"));
        assert_eq!(fs::read_to_string(&journal_path).unwrap(), original);
    }

    #[test]
    fn pre_migration_recovery_clears_pending_mcp_journal() {
        let _home = IsolatedInventoryHome::new();
        let layout = EffectiveLayout::load_or_legacy().unwrap();
        let managed = layout.generated_mcp_path();
        fs::create_dir_all(managed.parent().unwrap()).unwrap();
        fs::write(&managed, r#"{"servers":{}}"#).unwrap();
        let journal_path = managed.with_file_name("projection.txn.json");
        fs::write(
            &journal_path,
            r#"{"version":1,"transaction_id":"mcp-pending","entries":[]}"#,
        )
        .unwrap();
        assert!(matches!(
            mcp::inspect_mcp_journal(&managed),
            mcp::McpJournalInspection::Pending { .. }
        ));
        let guard = crate::tool_manager::lock::LifecycleLock::acquire().unwrap();

        recover_before_migration(&layout, &guard).unwrap();

        assert_eq!(
            mcp::inspect_mcp_journal(&managed),
            mcp::McpJournalInspection::Absent
        );
    }

    #[test]
    fn pre_migration_recovery_publishes_staged_standalone_tree() {
        use ccync_foundation::provider::{
            IntentPhase, PathRoot, ProviderLifecycleJournal, RelativePath, ScopedPath,
        };

        let _home = IsolatedInventoryHome::new();
        let layout = EffectiveLayout::load_or_legacy().unwrap();
        let journal_path = layout.state_base().join("projection.txn.json");
        fs::create_dir_all(journal_path.parent().unwrap()).unwrap();
        let stage_path =
            RelativePath::new("standalone-publications/publish-recovery/stage").unwrap();
        let stage = stage_path.join_to(&layout.state_base());
        fs::create_dir_all(stage.parent().unwrap()).unwrap();
        let source = _home.home.path().join("fixture-source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("SKILL.md"), "# recovered skill").unwrap();
        let destination = ScopedPath {
            root: PathRoot::UserHome,
            path: RelativePath::new(".claude/skills/recovered").unwrap(),
        };
        let discovery_root = PathRoot::UserHome.resolve().unwrap().join(".claude/skills");
        fs::create_dir_all(&discovery_root).unwrap();
        let request = projection::StandalonePublicationRequest {
            operation_id: "publish-recovery".into(),
            source_path: source,
            source_id: "plugin:example".into(),
            source_revision: "r1".into(),
            destination: destination.clone(),
            destination_root: PathRoot::UserHome.resolve().unwrap(),
            build_root: layout.state_base(),
            stage_path,
            backup_path: RelativePath::new("standalone-publications/publish-recovery/backup")
                .unwrap(),
            discovery_roots: vec![discovery_root],
            targets: vec!["claude".into()],
            reserved_paths: Vec::new(),
            prior_owner: None,
            owned_fingerprint: None,
            manifest_proofs: Vec::new(),
        };
        let prepared = projection::prepare_standalone_publication(request).unwrap();
        fs::create_dir_all(&stage).unwrap();
        fs::write(stage.join("SKILL.md"), "# recovered skill").unwrap();
        let mut intent = prepared.intent().clone();
        intent.phase = IntentPhase::Publishing;
        let mut journal = ProviderLifecycleJournal::default();
        journal.standalone_intents.push(intent);
        fs::write(&journal_path, journal.to_json_string().unwrap()).unwrap();
        let destination = destination
            .path
            .join_to(&PathRoot::UserHome.resolve().unwrap());
        let guard = crate::tool_manager::lock::LifecycleLock::acquire().unwrap();

        recover_before_migration(&layout, &guard).unwrap();

        assert!(destination.join("SKILL.md").is_file());
        let journal = crate::install::read_standalone_journal().unwrap();
        assert!(journal.standalone_intents.is_empty());
    }

    #[test]
    fn pre_migration_recovery_finishes_disabled_standalone_withdrawal() {
        use ccync_foundation::provider::{
            IntentPhase, PackageDigest, PathRoot, ProviderLifecycleJournal, RelativePath,
            ScopedPath, StandalonePreimage, StandaloneWithdrawalIntent,
            StandaloneWithdrawalReaderIntent, StandaloneWithdrawalReaderProof,
        };

        let _home = IsolatedInventoryHome::new();
        let layout = EffectiveLayout::load_or_legacy().unwrap();
        let journal_path = layout.state_base().join("projection.txn.json");
        fs::create_dir_all(journal_path.parent().unwrap()).unwrap();
        let destination_path = RelativePath::new(".claude/skills/withdrawn").unwrap();
        fs::create_dir_all(PathRoot::UserHome.resolve().unwrap().join(".claude/skills")).unwrap();
        let backup_path =
            RelativePath::new("standalone-withdrawals/withdraw-recovery/backup").unwrap();
        let backup = backup_path.join_to(&layout.state_base());
        fs::create_dir_all(&backup).unwrap();
        fs::write(backup.join("SKILL.md"), "# withdrawn skill").unwrap();
        let fingerprint = projection::standalone_source_fingerprint(&backup).unwrap();
        let destination = PathRoot::UserHome
            .resolve()
            .unwrap()
            .join(destination_path.as_str());
        let mut journal = ProviderLifecycleJournal::default();
        journal
            .standalone_withdrawal_intents
            .push(StandaloneWithdrawalIntent {
                operation_id: "withdraw-recovery".into(),
                source_id: "plugin:example".into(),
                source_revision: "r1".into(),
                source_fingerprint: PackageDigest::new("sha256:source-proof").unwrap(),
                readers: vec![StandaloneWithdrawalReaderProof {
                    reader: "claude".into(),
                    intent: StandaloneWithdrawalReaderIntent::Disabled,
                    evidence: "fixture: disabled".into(),
                }],
                destination: ScopedPath {
                    root: PathRoot::UserHome,
                    path: destination_path.clone(),
                },
                preimage: StandalonePreimage::Tree {
                    fingerprint: PackageDigest::new(&fingerprint).unwrap(),
                },
                link_kind: None,
                preimage_ref: ccync_foundation::provider::PreimageRef {
                    path: ScopedPath {
                        root: PathRoot::UserHome,
                        path: destination_path,
                    },
                    saved: backup_path.clone(),
                },
                preimage_fingerprint: PackageDigest::new(fingerprint.clone()).unwrap(),
                backup_path,
                registry_before: serde_json::json!({
                    "categories": ["directSkillProjectionPaths"],
                    "source": "plugin:example",
                    "standalone": {
                        "path": destination.to_string_lossy(),
                        "sourceId": "plugin:example",
                        "sourceRevision": "r1",
                        "sourceFingerprint": "sha256:source-proof",
                        "treeFingerprint": fingerprint,
                        "targets": ["claude"]
                    }
                }),
                registry_after: serde_json::json!({
                    "categories": [], "source": null, "standalone": null
                }),
                phase: IntentPhase::Staged,
            });
        fs::write(&journal_path, journal.to_json_string().unwrap()).unwrap();
        fs::write(
            layout.build_lock_path(),
            r#"{"_ccyncProjection":{"standalonePublications":{}}}"#,
        )
        .unwrap();
        let guard = crate::tool_manager::lock::LifecycleLock::acquire().unwrap();

        recover_before_migration(&layout, &guard).unwrap();

        assert!(!destination.exists());
        assert!(crate::install::read_standalone_journal()
            .unwrap()
            .standalone_withdrawal_intents
            .is_empty());
    }

    #[test]
    fn pre_migration_recovery_cleans_committed_tool_journal() {
        use crate::tool_manager::journal::{
            OperationJournal, OperationKind, OperationStage, RetryIntent,
        };
        use crate::tool_manager::model::ToolId;

        let _home = IsolatedInventoryHome::new();
        let layout = EffectiveLayout::load_or_legacy().unwrap();
        let tool_id = ToolId::new("sample").unwrap();
        let mut journal = OperationJournal::new(
            tool_id.clone(),
            OperationKind::Untrack,
            RetryIntent::Untrack { tool_id },
            None,
            PathBuf::new(),
            String::new(),
        );
        journal.stage = OperationStage::Committed;
        let journal_path = layout.tool_operation_journal_path();
        fs::create_dir_all(journal_path.parent().unwrap()).unwrap();
        fs::write(&journal_path, serde_json::to_vec_pretty(&journal).unwrap()).unwrap();
        let guard = crate::tool_manager::lock::LifecycleLock::acquire().unwrap();

        recover_before_migration(&layout, &guard).unwrap();

        assert!(!journal_path.exists());
    }

    #[test]
    fn pre_migration_recovery_blocks_tool_resume_with_retry_command_unchanged() {
        use crate::tool_manager::journal::{OperationJournal, OperationKind, RetryIntent};
        use crate::tool_manager::model::ToolId;

        let _home = IsolatedInventoryHome::new();
        let layout = EffectiveLayout::load_or_legacy().unwrap();
        let tool_id = ToolId::new("sample").unwrap();
        let journal = OperationJournal::new(
            tool_id,
            OperationKind::Refresh,
            RetryIntent::Refresh,
            None,
            PathBuf::new(),
            String::new(),
        );
        let journal_path = layout.tool_operation_journal_path();
        fs::create_dir_all(journal_path.parent().unwrap()).unwrap();
        let original = serde_json::to_vec_pretty(&journal).unwrap();
        fs::write(&journal_path, &original).unwrap();
        let guard = crate::tool_manager::lock::LifecycleLock::acquire().unwrap();

        let error = recover_before_migration(&layout, &guard).unwrap_err();

        assert!(error.contains("ccync tool refresh sample"), "{error}");
        assert_eq!(fs::read(&journal_path).unwrap(), original);
    }

    #[cfg(not(windows))]
    #[test]
    fn migration_inventory_fingerprints_legacy_key_at_decoded_path() {
        use sha2::{Digest, Sha256};
        let home = IsolatedInventoryHome::new();
        let build = home.home.path().join(".ccync/build");
        fs::create_dir_all(&build).unwrap();
        let managed = home.home.path().join("managed");
        let user = home.home.path().join("user");
        fs::write(&managed, "managed content").unwrap();
        fs::write(&user, "user content").unwrap();
        let before = Sha256::digest(fs::read(&user).unwrap());
        let fingerprint = projection::shared_path_fingerprint(&managed).unwrap();
        let legacy = managed.to_string_lossy().replace('/', "\\");
        fs::write(build.join("lock.json"), serde_json::json!({"_ccyncProjection": {
            "agentProjectionPaths": [{"path": legacy, "fingerprint": fingerprint}]
        }}).to_string()).unwrap();
        let layout = EffectiveLayout::load_or_legacy().unwrap();
        let inventory = build_portable_inventory_with_filesystem_check(&layout, |_, _| Ok(true));
        let row = inventory.ownership_rows.iter().find(|row| row.category == "agentProjectionPaths").unwrap();
        assert_eq!(row.live_path.as_deref(), Some(managed.as_path()));
        assert_eq!(row.matches_recorded_fingerprint, Some(true));
        assert_eq!(before, Sha256::digest(fs::read(&user).unwrap()));
    }

    #[test]
    fn migration_inventory_covers_retained_rows_and_blocks_unsafe_state_without_writes() {
        let home = IsolatedInventoryHome::new();
        let ccync = home.home.path().join(".ccync");
        let build = ccync.join("build");
        let live_current = home.home.path().join("live-current");
        let live_changed = home.home.path().join("live-changed");
        fs::create_dir_all(&build).unwrap();
        fs::write(&live_current, "current content").unwrap();
        fs::write(&live_changed, "changed content").unwrap();
        let current_fingerprint = projection::shared_path_fingerprint(&live_current).unwrap();
        fs::write(build.join("lock.json"), serde_json::json!({
            "_providerLifecycle": {"schemaVersion": 1, "committed": []},
            "_ccyncProjection": {
                "standalonePublications": {"provider:sample": {"source":"provider"}},
                "agentProjectionPaths": [
                    live_current.to_string_lossy(),
                    {"path": live_current.to_string_lossy(), "fingerprint": current_fingerprint},
                    {"path": live_changed.to_string_lossy(), "fingerprint": "mismatch"}
                ],
                "historicalOwnership": [
                    {"path": live_changed.to_string_lossy(), "fingerprint": "past"}
                ]
            }
        }).to_string()).unwrap();
        fs::write(build.join("projection.txn.json"), r#"{"schemaVersion":1,"intents":[],"standaloneIntents":[],"standaloneWithdrawalIntents":[]}"#).unwrap();
        let layout = EffectiveLayout::load_or_legacy().unwrap();
        let before = snapshot(home.home.path());

        let inventory = build_portable_inventory_with_filesystem_check(&layout, |_, _| Ok(true));
        assert!(!inventory.blocked, "fingerprint-less rows must not block");
        assert_eq!(inventory.standalone_proofs.len(), 1);
        assert_eq!(inventory.ownership_rows.len(), 4);
        assert!(inventory
            .ownership_rows
            .iter()
            .all(|row| row.category == "agentProjectionPaths"
                || row.category == "historicalOwnership"));
        assert!(inventory
            .ownership_rows
            .iter()
            .any(|row| row.classification == "current"
                && row.matches_recorded_fingerprint == Some(true)));
        assert!(inventory
            .ownership_rows
            .iter()
            .any(|row| row.classification == "historical"
                && row.matches_recorded_fingerprint == Some(false)));
        assert!(inventory
            .ownership_rows
            .iter()
            .filter(|row| row.current_fingerprint.is_none())
            .all(|row| row.classification == "historical"));
        assert_eq!(
            inventory
                .counts_without_fingerprint
                .get("agentProjectionPaths"),
            Some(&1)
        );
        assert!(inventory
            .counts_without_fingerprint
            .get("historicalOwnership")
            .is_none());
        assert!(inventory
            .objects
            .iter()
            .any(|object| object.retained && object.category == "install-receipt"));

        let split = build_portable_inventory_with_filesystem_check(&layout, |_, _| Ok(false));
        assert!(split.blocked);
        assert!(split
            .blockers
            .iter()
            .any(|reason| reason.contains("different filesystems")));
        assert_eq!(
            snapshot(home.home.path()),
            before,
            "inventory must not write to the home"
        );

        fs::write(build.join("projection.txn.json"), r#"{"schemaVersion":99}"#).unwrap();
        let unsupported = build_portable_inventory_with_filesystem_check(&layout, |_, _| Ok(true));
        assert!(
            unsupported.blocked,
            "unsupported provider journal must block"
        );
    }

    #[test]
    fn migration_inventory_covers_storage_and_pending_journals_and_legacy_fixup() {
        let home = IsolatedInventoryHome::new();
        let ccync = home.home.path().join(".ccync");
        let build = ccync.join("build");
        fs::create_dir_all(&build).unwrap();
        let layout = EffectiveLayout::load_or_legacy().unwrap();

        let roots = layout.storage_roots();
        let mut expected_objects = vec![
            ("portable", roots.portable.join("sources.json")),
            ("portable", roots.portable.join("lock.json")),
            ("config", layout.config_path()),
            ("config", layout.plugins_path()),
            ("config", layout.agents_md_path()),
            ("config", layout.mcp_path()),
            ("config", layout.tools_path()),
            ("state", layout.build_lock_path()),
            ("state", layout.state_base().join("projection.txn.json")),
            ("state", layout.state_base().join("preimages")),
            ("state", layout.state_base().join("standalone-publications")),
            ("state", layout.state_base().join("standalone-withdrawals")),
            (
                "state",
                layout.state_base().join("mcp/projected-state.json"),
            ),
            ("state", layout.state_base().join("mcp/projection.txn.json")),
            ("state", layout.tool_state_path()),
            ("state", layout.tool_artifacts_root()),
            ("state", layout.tool_operation_journal_path()),
            ("state", layout.generated_mcp_path()),
            ("state", layout.routes_path()),
            ("state", layout.dist_path()),
            ("state", layout.backups_path()),
            ("state", layout.state_lock_path()),
            ("state", layout.legacy_lock_path()),
            ("state", layout.layout_path()),
            ("state", layout.migration_journal_path()),
            ("state", layout.migration_sentinel_path()),
            ("cache", layout.cache_path()),
        ];
        if let Some(path) = layout.sources_export_path() {
            expected_objects.push(("portable", path));
        }
        if let Some(path) = layout.lock_export_path() {
            expected_objects.push(("portable", path));
        }
        if let Some(home_path) = ccync_foundation::paths::user_home() {
            expected_objects.extend([
                ("live-surface", home_path.join(".claude")),
                ("live-surface", home_path.join(".codex")),
                ("live-surface", home_path.join(".copilot")),
                ("live-surface", home_path.join(".gemini")),
                ("live-surface", home_path.join(".github")),
                ("live-surface", home_path.join(".opencode")),
                ("live-surface", home_path.join(".cursor")),
                ("live-surface", home_path.join(".vscode")),
                ("live-surface", home_path.join(".agents")),
                (
                    "install-receipt",
                    home_path.join(".ccync/install-receipts/ccync.json"),
                ),
            ]);
        }
        if let Some(path) = ccync_foundation::paths::install_receipt_path() {
            expected_objects.push(("state", path));
        }

        // Exercise an existing storage-map file so the inventory must include
        // its content fingerprint, while all other paths remain read-only.
        let existing_config = layout.plugins_path();
        fs::create_dir_all(existing_config.parent().unwrap()).unwrap();
        fs::write(&existing_config, "inventory fixture").unwrap();

        let live_path = home.home.path().join(".claude/plugins/sample");
        fs::create_dir_all(live_path.parent().unwrap()).unwrap();
        fs::write(&live_path, "live provider").unwrap();
        let live_fingerprint = projection::shared_path_fingerprint(&live_path).unwrap();
        let lock = serde_json::json!({
            "_providerLifecycle": {"schemaVersion": 1, "committed": [{
                "target": "codex",
                "selector": "sample@ccync-marketplace",
                "pluginName": "sample-provider",
                "generation": {
                    "source": "sample-source",
                    "generation": "generation-1",
                    "packageDigest": "sha256:sample-package"
                },
                "capabilities": ["skill"]
            }]},
            "_ccyncProjection": {
                "agentProjectionPaths": [{
                    "path": live_path.to_string_lossy(),
                    "fingerprint": live_fingerprint
                }]
            }
        });
        fs::write(layout.build_lock_path(), lock.to_string()).unwrap();

        let journal = serde_json::json!({
            "schemaVersion": 3,
            "intents": [{
                "target": "codex",
                "ownershipFingerprints": [],
                "scopedPaths": [],
                "preimageRefs": [{
                    "path": {"root": "user-home", "path": ".claude/plugins/sample"},
                    "saved": "preimages/native"
                }],
                "desiredGeneration": {
                    "source": "sample-source",
                    "generation": "generation-2",
                    "packageDigest": "sha256:desired-package"
                },
                "phase": "staged"
            }],
            "standaloneIntents": [{
                "operationId": "publish-1",
                "source": "fixture: publication",
                "sourceFingerprint": "sha256:source",
                "targets": ["claude"],
                "destination": {"root": "user-home", "path": ".agents/skills/sample"},
                "preimage": {"kind": "tree", "value": {"fingerprint": "sha256:old-tree"}},
                "preimageRef": {
                    "path": {"root": "user-home", "path": ".agents/skills/sample"},
                    "saved": "preimages/standalone"
                },
                "preimageFingerprint": "sha256:old-tree",
                "stagePath": "stages/publication",
                "backupPath": "backups/publication",
                "expectedTreeFingerprint": "sha256:new-tree",
                "phase": "publishing"
            }],
            "standaloneWithdrawalIntents": [{
                "operationId": "withdraw-1",
                "sourceId": "sample-source",
                "sourceRevision": "revision-1",
                "sourceFingerprint": "sha256:source",
                "readers": [{
                    "reader": "claude",
                    "intent": "disabled",
                    "evidence": "fixture: disabled"
                }],
                "destination": {"root": "user-home", "path": ".agents/skills/withdrawn"},
                "preimage": {"kind": "tree", "value": {"fingerprint": "sha256:withdraw-tree"}},
                "preimageRef": {
                    "path": {"root": "user-home", "path": ".agents/skills/withdrawn"},
                    "saved": "preimages/withdrawal"
                },
                "preimageFingerprint": "sha256:withdraw-tree",
                "backupPath": "preimages/withdrawal",
                "registryBefore": {
                    "categories": ["directSkillProjectionPaths"],
                    "source": "sample-source",
                    "standalone": {
                        "path": "/fixture/home/.agents/skills/withdrawn",
                        "sourceId": "sample-source",
                        "sourceRevision": "revision-1",
                        "sourceFingerprint": "sha256:source",
                        "treeFingerprint": "sha256:withdraw-tree",
                        "targets": ["claude"]
                    }
                },
                "registryAfter": {"categories": [], "source": null, "standalone": null},
                "phase": "staged"
            }]
        });
        for saved in [
            "preimages/native",
            "preimages/standalone",
            "preimages/withdrawal",
        ] {
            let path = build.join(saved);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "saved preimage").unwrap();
        }
        fs::write(build.join("projection.txn.json"), journal.to_string()).unwrap();

        let managed_json = layout.generated_mcp_path();
        fs::create_dir_all(managed_json.parent().unwrap()).unwrap();
        fs::write(&managed_json, r#"{"servers":{}}"#).unwrap();
        fs::write(
            managed_json.with_file_name("projection.txn.json"),
            r#"{"version":1,"transaction_id":"mcp-1","entries":[{"host":"claude","name":"sample","pre":"absent","post":"fingerprint-1"}]}"#,
        )
        .unwrap();

        let tool_journal = layout.tool_operation_journal_path();
        fs::create_dir_all(tool_journal.parent().unwrap()).unwrap();
        fs::write(&tool_journal, r#"{"version":1,"operations":[]}"#).unwrap();

        fs::create_dir_all(ccync.join("config")).unwrap();
        fs::write(ccync.join("config/config.json"), "legacy layout").unwrap();

        let inventory = build_portable_inventory_with_filesystem_check(&layout, |_, _| Ok(true));
        assert!(
            !inventory.blocked,
            "supported evidence must not block: {:?}",
            inventory.blockers
        );
        assert_eq!(inventory.provider_native_rows.len(), 1);
        assert_eq!(inventory.provider_intents.len(), 1);
        assert_eq!(inventory.standalone_intents.len(), 1);
        assert_eq!(inventory.standalone_withdrawal_intents.len(), 1);
        assert_eq!(inventory.mcp_journal["state"], "pending");
        assert_eq!(inventory.mcp_journal["entries"][0]["name"], "sample");
        assert_eq!(inventory.tool_journal["state"], "present");
        assert!(inventory.pending_older_sublayout_fixup);
        assert!(inventory.ownership_rows.iter().any(|row| {
            row.classification == "current" && row.matches_recorded_fingerprint == Some(true)
        }));

        fs::remove_file(build.join("preimages/native")).unwrap();
        let missing_evidence =
            build_portable_inventory_with_filesystem_check(&layout, |_, _| Ok(true));
        assert!(
            missing_evidence.blocked,
            "missing evidence referenced by a provider intent must block"
        );

        for (category, path) in expected_objects {
            let matches = inventory
                .objects
                .iter()
                .filter(|object| object.path == path && object.category == category)
                .collect::<Vec<_>>();
            assert_eq!(
                matches.len(),
                1,
                "expected one {category} object at {path:?}"
            );
        }
        let config_object = inventory
            .objects
            .iter()
            .find(|object| object.path == existing_config)
            .unwrap();
        assert!(config_object.fingerprint.is_some());
    }

    #[test]
    fn migration_build_inventory_copies_private_objects_with_fingerprints_and_skips_absent() {
        let _home = IsolatedInventoryHome::new();
        let layout = EffectiveLayout::load_or_legacy().unwrap();
        let legacy_build = layout.state_base();
        let storage_build = layout.storage_roots().state.join("build");
        let objects = [
            "projection.txn.json",
            "preimages",
            "standalone-publications",
            "standalone-withdrawals",
            "mcp/projected-state.json",
            "mcp/projection.txn.json",
        ];
        for object in objects {
            let source = legacy_build.join(object);
            if matches!(
                object,
                "preimages" | "standalone-publications" | "standalone-withdrawals"
            ) {
                fs::create_dir_all(&source).unwrap();
                fs::write(source.join("payload"), format!("fixture:{object}")).unwrap();
            } else {
                fs::create_dir_all(source.parent().unwrap()).unwrap();
                let contents = match object {
                    "projection.txn.json" => {
                        r#"{"schemaVersion":1,"intents":[],"standaloneIntents":[],"standaloneWithdrawalIntents":[]}"#
                    }
                    "mcp/projection.txn.json" => {
                        r#"{"version":1,"transaction_id":"fixture","entries":[]}"#
                    }
                    _ => "fixture",
                };
                fs::write(&source, contents).unwrap();
            }
        }
        let before = objects
            .iter()
            .map(|object| {
                let source = legacy_build.join(object);
                (
                    source.clone(),
                    projection::shared_path_fingerprint(&source).unwrap(),
                )
            })
            .collect::<Vec<_>>();

        let manifest = stage_portable_migration(&layout, |_, _| Ok(())).unwrap();
        let persisted: Value =
            serde_json::from_slice(&fs::read(layout.migration_journal_path()).unwrap()).unwrap();
        assert_eq!(persisted, manifest);

        for (source, fingerprint) in before {
            let relative = source.strip_prefix(&legacy_build).unwrap();
            let destination = storage_build.join(relative);
            assert_eq!(
                projection::shared_path_fingerprint(&destination).as_deref(),
                Some(fingerprint.as_str()),
                "staged fingerprint differs for {}",
                relative.display()
            );
            assert_eq!(
                projection::shared_path_fingerprint(&source).as_deref(),
                Some(fingerprint.as_str()),
                "legacy object changed for {}",
                relative.display()
            );
            let source_bytes = if source.is_file() {
                fs::read(&source).unwrap()
            } else {
                fs::read(source.join("payload")).unwrap()
            };
            let destination_bytes = if destination.is_file() {
                fs::read(&destination).unwrap()
            } else {
                fs::read(destination.join("payload")).unwrap()
            };
            assert_eq!(
                destination_bytes,
                source_bytes,
                "legacy bytes changed for {}",
                relative.display()
            );
            assert!(manifest["legacyFingerprints"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["path"] == source.to_string_lossy().as_ref()
                    && row["fingerprint"] == fingerprint));
        }
        let absent = legacy_build.join("mcp/absent.json");
        assert!(!absent.exists());
        assert!(!storage_build.join("mcp/absent.json").exists());
        assert!(!manifest["legacyFingerprints"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["path"] == absent.to_string_lossy().as_ref()));
        assert!(layout.migration_journal_path().exists());
    }
}

/// Resolve the layout with the specified root-validation fallback semantics.
pub fn load_portable_inventory() -> Result<PortableInventory, String> {
    let layout = EffectiveLayout::load_or_legacy().map_err(|error| error.to_string())?;
    Ok(build_portable_inventory(&layout))
}

/// Recover each journal owner before inventory can be used to migrate paths.
/// Journals remain recovery evidence and are never copied into migration state.
pub fn recover_before_migration(
    layout: &EffectiveLayout,
    guard: &crate::tool_manager::lock::LifecycleGuard,
) -> Result<PortableInventory, String> {
    crate::install::recover_native_intents(guard)?;
    crate::delivery_apply::recover_standalone_intents(layout, guard)?;

    match mcp::inspect_mcp_journal(&layout.generated_mcp_path()) {
        mcp::McpJournalInspection::Absent => {}
        mcp::McpJournalInspection::Pending { .. } => {
            mcp::run_mcp_projection_at(
                &layout.generated_mcp_path(),
                &layout.machine_config_path(),
                || Ok(()),
            )
            .map_err(|error| format!("MCP journal recovery failed: {error}"))?;
        }
        mcp::McpJournalInspection::UnsupportedVersion { version } => {
            return Err(format!(
                "MCP journal recovery blocked: unsupported version {version}"
            ));
        }
        mcp::McpJournalInspection::Corrupt { reason } => {
            return Err(format!("MCP journal recovery blocked: {reason}"));
        }
    }

    use crate::tool_manager::{journal::RetryIntent, recovery::RecoveryDecision};
    match crate::tool_manager::recovery::recover(guard, &RetryIntent::Refresh)
        .map_err(|error| format!("tool journal recovery failed: {error}"))?
    {
        RecoveryDecision::None | RecoveryDecision::CommittedCleanup => {}
        RecoveryDecision::Resume => {
            let journal: crate::tool_manager::journal::OperationJournal = serde_json::from_slice(
                &fs::read(layout.tool_operation_journal_path())
                    .map_err(|error| format!("tool journal retry command unavailable: {error}"))?,
            )
            .map_err(|error| format!("tool journal retry command unavailable: {error}"))?;
            return Err(format!(
                "tool journal recovery requires replay: {}",
                journal.retry_command()
            ));
        }
        RecoveryDecision::RecoveryRequired(required) => {
            return Err(format!(
                "tool journal recovery requires replay: {}",
                required.retry_command
            ));
        }
    }

    let inventory = build_portable_inventory(layout);
    if inventory.blocked {
        return Err(format!(
            "migration blocked after journal recovery: {}",
            inventory.blockers.join("; ")
        ));
    }
    Ok(inventory)
}

/// Scan a ccync home directory and build a migration plan for any old-layout
/// paths that are present. Absent old paths are silently skipped (idempotent).
///
/// Does **not** apply the plan — call [`apply_migration`] for that.
pub fn build_migration_plan(ccync_home: &Path) -> MigrationPlan {
    let mut plan = MigrationPlan::default();

    // `config/config.json` → `config.json`
    let old_config = ccync_home.join("config").join("config.json");
    let new_config = ccync_home.join("config.json");
    if old_config.exists() && !new_config.exists() {
        plan.moves.push(LayoutMove {
            from: old_config,
            to: new_config,
        });
    }

    // `local/catalog.json` → `plugins.json`
    let old_catalog = ccync_home.join("local").join("catalog.json");
    let new_catalog = ccync_home.join("plugins.json");
    if old_catalog.exists() && !new_catalog.exists() {
        plan.moves.push(LayoutMove {
            from: old_catalog,
            to: new_catalog,
        });
    }

    // `state/plugins.lock.json` → `build/lock.json`
    let old_lock = ccync_home.join("state").join("plugins.lock.json");
    let new_lock = ccync_home.join("build").join("lock.json");
    if old_lock.exists() && !new_lock.exists() {
        plan.moves.push(LayoutMove {
            from: old_lock,
            to: new_lock,
        });
    }

    // `generated/mcp/managed.json` → `build/mcp/managed.json`
    let old_mcp = ccync_home
        .join("generated")
        .join("mcp")
        .join("managed.json");
    let new_mcp = ccync_home.join("build").join("mcp").join("managed.json");
    if old_mcp.exists() && !new_mcp.exists() {
        plan.moves.push(LayoutMove {
            from: old_mcp,
            to: new_mcp,
        });
    }

    plan
}

/// Apply a migration plan: move old files to new locations. Idempotent —
/// absent source paths are skipped.
pub fn apply_migration(plan: &MigrationPlan) -> std::io::Result<()> {
    for m in &plan.moves {
        if m.from.exists() && !m.to.exists() {
            if let Some(parent) = m.to.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(&m.from, &m.to)?;
        }
    }
    Ok(())
}

/// Fold the legacy selection sources into `config.json`'s canonical
/// `selectedProjectionTargets` key: the removed `install-state.json`'s
/// `selectedRuntimes`, and the old-layout `runtimes` spelling that
/// [`apply_migration`] moves verbatim out of the old `config/config.json`.
///
/// The data-layout flatten moved the runtime selection out of the removed
/// `install-state.json` and into `config.json`. For an existing install the
/// migration must carry that selection across, or it is silently lost
/// (downstream it falls back to "all targets"). The same holds for a
/// pre-flatten `config/config.json` whose selection lived under `runtimes`:
/// after the verbatim layout move nothing else ever reads that spelling, so
/// this fold is what promotes it.
///
/// Precedence (D-01, normative for this writer and the `init` writer): a
/// legacy value is folded ONLY when `config.json` has no canonical
/// `selectedProjectionTargets` of its own. When `config.json` already
/// carries a selection — canonical, or a legacy spelling of its own — that
/// existing selection wins outright and `install-state.json`'s value goes
/// unused. Among the legacy sources the order is `config.json`
/// `selectedRuntimes`, then `config.json` `runtimes`, then
/// `install-state.json` `selectedRuntimes` — config.json's own record
/// always beats the removed file's, matching the resolver's own
/// `selectedRuntimes`-when-canonical-absent precedence.
/// `install-state.json`'s value is used only when `config.json` has no
/// selection of any kind yet.
///
/// Precedence is decided on the raw `config.json` key map — the same
/// representation [`write_projection_selection_at`] mutates — never on the
/// typed [`CcyncConfig`] struct. A typed load collapses ANY deserialization
/// failure (e.g. one unrelated wrong-typed field elsewhere in the file) into
/// the default struct, which would make a physically present canonical
/// selection look absent and let this fold overwrite it. Reading the raw map
/// keeps the read and write halves of the fold agreed on one view of the
/// file. The typed resolver is used only to parse the selected key's entries
/// (normalization, dedup, rejection), never to decide key presence.
///
/// The write itself goes through [`write_projection_selection_at`], the
/// canonical foundation writer: raw-map based, so unrelated `config.json`
/// keys (`secrets`, `devMode`, `ccyncRoot`, …) survive verbatim, and the
/// write is atomic — one publish of the final map; an injected failure
/// mid-write leaves `config.json` byte-identical to before. Legacy
/// `selectedRuntimes` / `primaryRuntime` keys and the old-layout `runtimes`
/// spelling stop propagating: the writer removes them from `config.json`
/// rather than carrying them forward.
///
/// Invalid selection entries in the promoted legacy source are never
/// destroyed silently: each rejected entry is warned about by name on
/// stderr, and the source record is retained on disk — the legacy file is
/// renamed to `install-state.json.bak` (when it was the source) or the
/// pre-fold `config.json` is copied to `config.json.bak` (when `config.json`
/// 's own legacy key was the source) — so the original selection stays
/// recoverable after the writer strips the legacy keys.
///
/// Idempotent: the legacy file is removed (or renamed to `.bak`) after a
/// successful fold, and the write strips the old-layout `runtimes` spelling
/// from `config.json`, so a second run finds nothing legacy and no-ops.
/// Non-destructive on parse failure: an unreadable or malformed
/// `install-state.json` is left in place and `config.json` is never
/// touched; a malformed `config.json` aborts the fold before anything is
/// deleted.
pub fn fold_install_state_into_config(ccync_home: &Path) -> std::io::Result<()> {
    let install_state = ccync_home.join("install-state.json");
    let state: Option<serde_json::Value> = if install_state.exists() {
        let Ok(state_text) = fs::read_to_string(&install_state) else {
            return Ok(());
        };
        match serde_json::from_str(&state_text) {
            Ok(value) => Some(value),
            // Malformed install-state.json: leave both files untouched.
            Err(_) => return Ok(()),
        }
    } else {
        None
    };

    let config_path = ccync_home.join("config.json");
    let config_map = match json_util::read_json_map(&config_path) {
        Ok(map) => map,
        // A malformed config.json is never a fold target. Surface the error
        // only when legacy state is actually waiting to fold; otherwise stay
        // quiet — config diagnostics belong to the paths that read it.
        Err(e) => return if state.is_some() { Err(e) } else { Ok(()) },
    };

    let old_layout_entries = non_empty_string_entries(config_map.get(OLD_LAYOUT_SELECTION_KEY));
    if state.is_none() && old_layout_entries.is_none() {
        // Nothing legacy to fold.
        return Ok(());
    }

    let mut retain_install_state = false;
    if non_empty_string_entries(config_map.get(CANONICAL_SELECTION_KEY)).is_none() {
        // No canonical selection in config.json yet — promote whichever
        // legacy source applies: config.json's own legacy spellings win
        // when present (newer spelling first); otherwise fall back to
        // install-state.json's legacy field. All are parsed and validated
        // the same way.
        let (entries, source_key, source_is_config) =
            if let Some(entries) = non_empty_string_entries(config_map.get(LEGACY_SELECTION_KEY)) {
                (entries, LEGACY_SELECTION_KEY, true)
            } else if let Some(entries) = old_layout_entries {
                (entries, OLD_LAYOUT_SELECTION_KEY, true)
            } else {
                (
                    state
                        .as_ref()
                        .and_then(|s| non_empty_string_entries(s.get(LEGACY_SELECTION_KEY)))
                        .unwrap_or_default(),
                    LEGACY_SELECTION_KEY,
                    false,
                )
            };
        let resolution = parse_selection_entries(entries);

        if !resolution.rejected.is_empty() {
            let source_label = if source_is_config {
                "~/.ccync/config.json"
            } else {
                "~/.ccync/install-state.json"
            };
            warn_rejected_selection_entries(source_label, source_key, &resolution.rejected);
            // Retain the source record so the original selection stays
            // recoverable: back up config.json before the writer strips its
            // legacy keys, or keep the legacy file as a .bak instead of
            // deleting it.
            if source_is_config {
                fs::copy(&config_path, ccync_home.join("config.json.bak"))?;
            } else {
                retain_install_state = true;
            }
        }

        write_projection_selection_at(&config_path, &resolution.targets)?;
    }

    if state.is_some() {
        if retain_install_state {
            fs::rename(&install_state, ccync_home.join("install-state.json.bak"))?;
        } else {
            fs::remove_file(&install_state)?;
        }
    }
    Ok(())
}

/// Warn on stderr, naming each legacy selection entry that did not parse as
/// a known projection target — mirroring the named-invalid-key warning the
/// runtime-selection read path already emits — so entries are never dropped
/// without a user-visible trace.
fn warn_rejected_selection_entries(source_label: &str, source_key: &str, rejected: &[String]) {
    let valid: Vec<&str> = ccync_foundation::target::CANONICAL_PROJECTION_TARGETS
        .iter()
        .map(|t| t.as_str())
        .collect();
    eprintln!(
        "ccync: ignoring invalid runtime key(s) in {source_label} `{source_key}` during migration: {} — valid: {}",
        rejected.join(", "),
        valid.join(", ")
    );
}

/// Canonical selection key in `config.json`.
const CANONICAL_SELECTION_KEY: &str = "selectedProjectionTargets";
/// Legacy selection key, in both `config.json` and `install-state.json`.
const LEGACY_SELECTION_KEY: &str = "selectedRuntimes";
/// Old-layout selection spelling from the pre-flatten `config/config.json`,
/// carried verbatim into `config.json` by [`apply_migration`].
const OLD_LAYOUT_SELECTION_KEY: &str = "runtimes";

/// Extract a JSON value's string entries when it is a non-empty array with at
/// least one string element; `None` otherwise. "Present" for the fold's
/// precedence means exactly this — matching the crate convention that an
/// absent key and an empty array are indistinguishable.
fn non_empty_string_entries(value: Option<&serde_json::Value>) -> Option<Vec<String>> {
    let entries: Vec<String> = value?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str())
        .map(str::to_owned)
        .collect();
    if entries.is_empty() {
        None
    } else {
        Some(entries)
    }
}

/// Parse raw selection entries through the shared resolver so the fold gets
/// the same normalization, dedup, and rejection reporting as every other
/// read path — used for entry parsing only, never for key-presence decisions.
fn parse_selection_entries(entries: Vec<String>) -> ProjectionTargetResolution {
    CcyncConfig {
        selected_projection_targets: entries,
        ..CcyncConfig::default()
    }
    .resolve_projection_targets()
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn migration_plan_maps_old_paths_to_new() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");

        // Create old layout files.
        fs::create_dir_all(ccync.join("config")).unwrap();
        fs::write(
            ccync.join("config").join("config.json"),
            r#"{"runtimes":["claude"]}"#,
        )
        .unwrap();

        fs::create_dir_all(ccync.join("local")).unwrap();
        fs::write(
            ccync.join("local").join("catalog.json"),
            r#"{"plugins":[]}"#,
        )
        .unwrap();

        fs::create_dir_all(ccync.join("state")).unwrap();
        fs::write(
            ccync.join("state").join("plugins.lock.json"),
            r#"{"resolvedPlugins":[]}"#,
        )
        .unwrap();

        fs::create_dir_all(ccync.join("generated").join("mcp")).unwrap();
        fs::write(
            ccync.join("generated").join("mcp").join("managed.json"),
            r#"{"servers":{}}"#,
        )
        .unwrap();

        let plan = build_migration_plan(&ccync);

        assert_eq!(
            plan.moves.len(),
            4,
            "all four old-layout paths must appear in plan"
        );
        assert!(plan
            .moves
            .iter()
            .any(|m| m.from.ends_with("config/config.json") && m.to.ends_with("config.json")));
        assert!(plan
            .moves
            .iter()
            .any(|m| m.from.ends_with("catalog.json") && m.to.ends_with("plugins.json")));
        assert!(plan
            .moves
            .iter()
            .any(|m| m.from.ends_with("plugins.lock.json") && m.to.ends_with("lock.json")));
        assert!(plan.moves.iter().any(|m| m.from.ends_with("managed.json")
            && m.to.ends_with("managed.json")
            && m.to.to_string_lossy().contains("build")));
    }

    #[test]
    fn apply_migration_moves_old_config_verbatim() {
        // apply_migration alone is a pure move: the old-layout selection
        // spellings arrive in config.json byte-for-byte. Promoting them to
        // the canonical key is the fold's job, exercised separately below.
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(ccync.join("config")).unwrap();
        let payload = r#"{"runtimes":["claude"],"primaryRuntime":"claude"}"#;
        fs::write(ccync.join("config").join("config.json"), payload).unwrap();

        let plan = build_migration_plan(&ccync);
        apply_migration(&plan).unwrap();

        let migrated = fs::read_to_string(ccync.join("config.json")).unwrap();
        assert_eq!(
            migrated, payload,
            "the layout move itself must preserve bytes verbatim"
        );
    }

    #[test]
    fn fold_promotes_old_layout_runtimes_after_apply_migration() {
        // End-to-end old layout: apply_migration moves config/config.json
        // verbatim, then the fold — with no install-state.json anywhere —
        // promotes the old `runtimes` spelling to the canonical key and
        // strips every legacy spelling from the write.
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(ccync.join("config")).unwrap();
        fs::write(
            ccync.join("config").join("config.json"),
            r#"{"runtimes":["codex","claude"],"primaryRuntime":"claude","secrets":{"TOK":"abc"}}"#,
        )
        .unwrap();

        let plan = build_migration_plan(&ccync);
        apply_migration(&plan).unwrap();
        fold_install_state_into_config(&ccync).unwrap();

        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(ccync.join("config.json")).unwrap()).unwrap();
        assert_eq!(
            merged["selectedProjectionTargets"],
            serde_json::json!(["codex", "claude"]),
            "the old-layout selection must be promoted in its on-disk order"
        );
        assert!(
            merged.get("runtimes").is_none(),
            "the old-layout spelling must be stripped"
        );
        assert!(merged.get("primaryRuntime").is_none());
        assert_eq!(merged["secrets"]["TOK"], serde_json::json!("abc"));

        // Idempotent: the spelling is gone, so a second fold no-ops.
        let after_first = fs::read_to_string(ccync.join("config.json")).unwrap();
        fold_install_state_into_config(&ccync).unwrap();
        assert_eq!(
            fs::read_to_string(ccync.join("config.json")).unwrap(),
            after_first
        );
    }

    #[test]
    fn fold_config_selected_runtimes_wins_over_old_layout_runtimes() {
        // Among config.json's own legacy spellings the newer one wins, and
        // both are stripped by the single canonical write.
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync).unwrap();
        fs::write(
            ccync.join("config.json"),
            r#"{"selectedRuntimes":["claude"],"runtimes":["codex"]}"#,
        )
        .unwrap();

        fold_install_state_into_config(&ccync).unwrap();

        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(ccync.join("config.json")).unwrap()).unwrap();
        assert_eq!(
            merged["selectedProjectionTargets"],
            serde_json::json!(["claude"])
        );
        assert!(merged.get("selectedRuntimes").is_none());
        assert!(merged.get("runtimes").is_none());
    }

    #[test]
    fn fold_old_layout_runtimes_wins_over_install_state_file() {
        // config.json's own record — even in the old-layout spelling — beats
        // the removed install-state.json's value.
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync).unwrap();
        fs::write(ccync.join("config.json"), r#"{"runtimes":["claude"]}"#).unwrap();
        fs::write(
            ccync.join("install-state.json"),
            r#"{"selectedRuntimes":["codex"]}"#,
        )
        .unwrap();

        fold_install_state_into_config(&ccync).unwrap();

        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(ccync.join("config.json")).unwrap()).unwrap();
        assert_eq!(
            merged["selectedProjectionTargets"],
            serde_json::json!(["claude"])
        );
        assert!(merged.get("runtimes").is_none());
        assert!(!ccync.join("install-state.json").exists());
    }

    #[test]
    fn fold_invalid_old_layout_entries_promote_valid_subset_and_keep_config_backup() {
        // The old-layout spelling gets the same rejected-entry contract:
        // named warning, valid subset promoted, pre-fold config.json backed
        // up so the original selection stays recoverable.
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync).unwrap();
        let original_config = r#"{"runtimes":["claude","windsurf"]}"#;
        fs::write(ccync.join("config.json"), original_config).unwrap();

        fold_install_state_into_config(&ccync).unwrap();

        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(ccync.join("config.json")).unwrap()).unwrap();
        assert_eq!(
            merged["selectedProjectionTargets"],
            serde_json::json!(["claude"])
        );
        assert!(merged.get("runtimes").is_none());
        assert_eq!(
            fs::read_to_string(ccync.join("config.json.bak")).unwrap(),
            original_config,
            "the pre-fold config.json must be backed up when old-layout entries were rejected"
        );
    }

    #[test]
    fn migration_is_idempotent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(ccync.join("config")).unwrap();
        fs::write(ccync.join("config").join("config.json"), r#"{"v":1}"#).unwrap();

        let plan1 = build_migration_plan(&ccync);
        apply_migration(&plan1).unwrap();

        // Second call: old path gone, new path exists → plan is empty.
        let plan2 = build_migration_plan(&ccync);
        assert!(
            plan2.moves.is_empty(),
            "second migration plan must be empty (idempotent)"
        );
        apply_migration(&plan2).unwrap(); // must not error
    }

    #[test]
    fn fold_install_state_legacy_only_promotes_to_canonical_and_preserves_secrets() {
        // legacy-only: config.json has no selection of any kind, so
        // install-state.json's legacy selectedRuntimes is what gets promoted.
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync).unwrap();
        // config.json carries secrets + devMode but no runtime selection yet.
        fs::write(
            ccync.join("config.json"),
            r#"{"secrets":{"TOK":"abc"},"devMode":false}"#,
        )
        .unwrap();
        fs::write(
            ccync.join("install-state.json"),
            r#"{"selectedRuntimes":["claude","codex"],"primaryRuntime":"codex"}"#,
        )
        .unwrap();

        fold_install_state_into_config(&ccync).unwrap();

        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(ccync.join("config.json")).unwrap()).unwrap();
        assert_eq!(
            merged["selectedProjectionTargets"],
            serde_json::json!(["claude", "codex"])
        );
        assert!(
            merged.get("selectedRuntimes").is_none(),
            "legacy selectedRuntimes must stop propagating into config.json"
        );
        assert!(
            merged.get("primaryRuntime").is_none(),
            "legacy primaryRuntime must stop propagating into config.json"
        );
        assert_eq!(
            merged["secrets"]["TOK"],
            serde_json::json!("abc"),
            "unrelated config keys must survive the fold"
        );
        assert!(
            !ccync.join("install-state.json").exists(),
            "legacy install-state.json must be removed after a successful fold"
        );

        // Idempotent: second run is a no-op (install-state already gone).
        fold_install_state_into_config(&ccync).unwrap();
    }

    #[test]
    fn fold_install_state_precedence_canonical_config_selection_wins_unread() {
        // precedence: config.json already has a canonical selection —
        // install-state.json's (different) legacy value must never be read.
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync).unwrap();
        fs::write(
            ccync.join("config.json"),
            r#"{"selectedProjectionTargets":["claude"]}"#,
        )
        .unwrap();
        fs::write(
            ccync.join("install-state.json"),
            r#"{"selectedRuntimes":["codex"]}"#,
        )
        .unwrap();

        fold_install_state_into_config(&ccync).unwrap();

        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(ccync.join("config.json")).unwrap()).unwrap();
        assert_eq!(
            merged["selectedProjectionTargets"],
            serde_json::json!(["claude"]),
            "an existing canonical config.json selection must win over the legacy install-state file"
        );
        assert!(
            !ccync.join("install-state.json").exists(),
            "the now-superseded install-state.json is still cleaned up"
        );
    }

    #[test]
    fn fold_keeps_canonical_selection_when_unrelated_field_is_wrong_typed() {
        // Regression: precedence must be decided on the raw key map. A typed
        // load would fail wholesale on the wrong-typed `devMode` (string, not
        // bool), report the canonical key as absent, and let install-state's
        // stale selection overwrite the user's real one. The canonical
        // selection must survive byte-identically instead.
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync).unwrap();
        let original =
            r#"{"selectedProjectionTargets":["claude"],"devMode":"true","secrets":{"TOK":"abc"}}"#;
        fs::write(ccync.join("config.json"), original).unwrap();
        fs::write(
            ccync.join("install-state.json"),
            r#"{"selectedRuntimes":["codex"]}"#,
        )
        .unwrap();

        fold_install_state_into_config(&ccync).unwrap();

        assert_eq!(
            fs::read_to_string(ccync.join("config.json")).unwrap(),
            original,
            "a physically present canonical selection must survive byte-identically even when an unrelated field is wrong-typed"
        );
        assert!(
            !ccync.join("install-state.json").exists(),
            "the unused install-state.json is still cleaned up"
        );
    }

    #[test]
    fn fold_install_state_own_legacy_selection_wins_over_install_state_file() {
        // config.json's own legacy selectedRuntimes (no canonical yet) wins
        // over install-state.json's differing value, and is what gets
        // promoted to canonical.
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync).unwrap();
        fs::write(
            ccync.join("config.json"),
            r#"{"selectedRuntimes":["claude"]}"#,
        )
        .unwrap();
        fs::write(
            ccync.join("install-state.json"),
            r#"{"selectedRuntimes":["codex"]}"#,
        )
        .unwrap();

        fold_install_state_into_config(&ccync).unwrap();

        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(ccync.join("config.json")).unwrap()).unwrap();
        assert_eq!(
            merged["selectedProjectionTargets"],
            serde_json::json!(["claude"]),
            "config.json's own legacy selection must win over the legacy install-state file"
        );
        assert!(merged.get("selectedRuntimes").is_none());
    }

    #[test]
    fn fold_invalid_install_state_entries_promote_valid_subset_and_keep_backup() {
        // Unparseable entries in the promoted install-state selection must
        // not vanish without a trace: the valid subset is promoted and the
        // source file is retained as a .bak so the original selection stays
        // recoverable. (The named-entry stderr warning is emitted on the
        // same path.)
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync).unwrap();
        fs::write(ccync.join("config.json"), r#"{"devMode":false}"#).unwrap();
        let original_state = r#"{"selectedRuntimes":["claude","windsurf"]}"#;
        fs::write(ccync.join("install-state.json"), original_state).unwrap();

        fold_install_state_into_config(&ccync).unwrap();

        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(ccync.join("config.json")).unwrap()).unwrap();
        assert_eq!(
            merged["selectedProjectionTargets"],
            serde_json::json!(["claude"]),
            "the valid subset must still be promoted"
        );
        assert!(
            !ccync.join("install-state.json").exists(),
            "the folded install-state.json must not stay under its live name"
        );
        assert_eq!(
            fs::read_to_string(ccync.join("install-state.json.bak")).unwrap(),
            original_state,
            "the source record must be retained as a .bak when entries were rejected"
        );

        // Idempotent: the live legacy file is gone, so a second run no-ops
        // and the .bak survives.
        fold_install_state_into_config(&ccync).unwrap();
        assert!(ccync.join("install-state.json.bak").exists());
    }

    #[test]
    fn fold_invalid_config_legacy_entries_promote_valid_subset_and_keep_config_backup() {
        // Same rejected-entry contract for config.json's own legacy key: the
        // writer strips that key, so the pre-fold config.json is backed up
        // before the write.
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync).unwrap();
        let original_config =
            r#"{"selectedRuntimes":["claude","windsurf"],"secrets":{"TOK":"abc"}}"#;
        fs::write(ccync.join("config.json"), original_config).unwrap();
        fs::write(
            ccync.join("install-state.json"),
            r#"{"selectedRuntimes":["codex"]}"#,
        )
        .unwrap();

        fold_install_state_into_config(&ccync).unwrap();

        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(ccync.join("config.json")).unwrap()).unwrap();
        assert_eq!(
            merged["selectedProjectionTargets"],
            serde_json::json!(["claude"])
        );
        assert_eq!(
            merged["secrets"]["TOK"],
            serde_json::json!("abc"),
            "secrets still survive the fold"
        );
        assert_eq!(
            fs::read_to_string(ccync.join("config.json.bak")).unwrap(),
            original_config,
            "the pre-fold config.json must be backed up when its own legacy entries were rejected"
        );
        assert!(
            !ccync.join("install-state.json").exists(),
            "the unused install-state.json is still removed"
        );
    }

    #[test]
    fn fold_install_state_malformed_file_leaves_config_untouched() {
        // injected-failure byte equality: a malformed install-state.json is
        // left in place, non-destructively, and config.json is never touched.
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync).unwrap();
        let original = r#"{"selectedProjectionTargets":["claude"],"secrets":{"TOK":"abc"}}"#;
        fs::write(ccync.join("config.json"), original).unwrap();
        fs::write(ccync.join("install-state.json"), "{ not valid json").unwrap();

        fold_install_state_into_config(&ccync).unwrap();

        assert_eq!(
            fs::read_to_string(ccync.join("config.json")).unwrap(),
            original,
            "config.json must be byte-identical when the fold source is unparseable"
        );
        assert!(
            ccync.join("install-state.json").exists(),
            "a malformed install-state.json must be left in place, not silently deleted"
        );
    }
}
