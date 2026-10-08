use super::{
    artifact_tree::TreeManifest,
    model::{ToolDeclaration, ToolFacets, ToolId},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::PathBuf};

const VERSION: u32 = 2;
const DECLARATION_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("tool store path is unavailable")]
    PathUnavailable,
    #[error("tool store I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("tool store JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported tool store version {0}")]
    UnknownVersion(u32),
    #[error("tool declaration {0} already exists with different contents; untrack it first")]
    ConflictRequiresUntrack(ToolId),
    #[error("declaration map key does not match the declaration id")]
    DeclarationKeyMismatch,
    #[error("tool ownership metadata is invalid or conflicts with existing proof")]
    InvalidOwnership,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertResult {
    Inserted,
    Unchanged,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateRecord {
    pub committed_generation: Option<u64>,
    pub artifact_fingerprint: Option<String>,
    pub source_evidence: serde_json::Value,
    #[serde(rename = "operationId")]
    pub operation_id: Option<String>,
    pub facets: ToolFacets,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationOwnership {
    pub tool_id: ToolId,
    pub generation: u64,
    pub operation_id: String,
    pub snapshot_relative_path: PathBuf,
    pub manifest_digest: String,
    pub manifest: TreeManifest,
    pub directories: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct Versioned<T> {
    version: u32,
    records: T,
}

#[derive(Serialize, Deserialize)]
struct StateDocument {
    version: u32,
    records: BTreeMap<ToolId, StateRecord>,
    #[serde(default)]
    ownership: BTreeMap<ToolId, BTreeMap<u64, GenerationOwnership>>,
}

fn declaration_path() -> Result<PathBuf, StoreError> {
    ccync_foundation::paths::tools_json_path().ok_or(StoreError::PathUnavailable)
}
fn state_path() -> Result<PathBuf, StoreError> {
    ccync_foundation::paths::tool_state_path().ok_or(StoreError::PathUnavailable)
}

fn read_versioned<T: for<'de> Deserialize<'de> + Default>(path: PathBuf) -> Result<T, StoreError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(T::default()),
        Err(e) => return Err(e.into()),
    };
    let doc: Versioned<T> = serde_json::from_slice(&bytes)?;
    if doc.version != DECLARATION_VERSION {
        return Err(StoreError::UnknownVersion(doc.version));
    }
    Ok(doc.records)
}

fn write_versioned<T: Serialize>(path: PathBuf, records: &T) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec_pretty(&Versioned {
        version: DECLARATION_VERSION,
        records,
    })?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    ccync_foundation::platform::atomic_write_bytes(&path, &bytes)
        .map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))?;
    Ok(())
}

pub fn load_declarations() -> Result<BTreeMap<ToolId, ToolDeclaration>, StoreError> {
    read_versioned(declaration_path()?)
}

pub fn replace_declarations(
    declarations: &BTreeMap<ToolId, ToolDeclaration>,
) -> Result<(), StoreError> {
    // Parse the current file first. Invalid input must never be replaced implicitly.
    let _ = load_declarations()?;
    if declarations
        .iter()
        .any(|(id, declaration)| id != &declaration.id)
    {
        return Err(StoreError::DeclarationKeyMismatch);
    }
    write_versioned(declaration_path()?, declarations)
}

pub fn insert_declaration(declaration: ToolDeclaration) -> Result<InsertResult, StoreError> {
    let mut all = load_declarations()?;
    if let Some(existing) = all.get(&declaration.id) {
        return if existing == &declaration {
            Ok(InsertResult::Unchanged)
        } else {
            Err(StoreError::ConflictRequiresUntrack(declaration.id))
        };
    }
    all.insert(declaration.id.clone(), declaration);
    write_versioned(declaration_path()?, &all)?;
    Ok(InsertResult::Inserted)
}

fn read_state_document() -> Result<StateDocument, StoreError> {
    let path = state_path()?;
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StateDocument {
                version: VERSION,
                records: BTreeMap::new(),
                ownership: BTreeMap::new(),
            })
        }
        Err(e) => return Err(e.into()),
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or(StoreError::InvalidOwnership)?;
    let document = match version {
        1 => Ok(StateDocument {
            version: VERSION,
            records: serde_json::from_value(value.get("records").cloned().unwrap_or_default())?,
            ownership: legacy_ownership(&serde_json::from_value(
                value.get("records").cloned().unwrap_or_default(),
            )?)?,
        }),
        VERSION => Ok(serde_json::from_value(value)?),
        other => Err(StoreError::UnknownVersion(other)),
    }?;
    for (id, generations) in &document.ownership {
        for (generation, proof) in generations {
            if id != &proof.tool_id || generation != &proof.generation {
                return Err(StoreError::InvalidOwnership);
            }
            validate_ownership(proof)?;
        }
    }
    Ok(document)
}

fn legacy_ownership(
    records: &BTreeMap<ToolId, StateRecord>,
) -> Result<BTreeMap<ToolId, BTreeMap<u64, GenerationOwnership>>, StoreError> {
    let mut result = BTreeMap::new();
    let Some(root) = ccync_foundation::paths::tool_artifacts_root() else {
        return Ok(result);
    };
    for (id, record) in records {
        let (Some(generation), Some(fingerprint), Some(operation_id)) = (
            record.committed_generation,
            record.artifact_fingerprint.as_deref(),
            record.operation_id.as_deref(),
        ) else {
            continue;
        };
        if operation_id.is_empty() {
            continue;
        }
        let path = root.join(id.as_str()).join(generation.to_string());
        let Ok(manifest) = crate::tool_manager::artifact_tree::scan_tree(&path) else {
            continue;
        };
        if fingerprint != format!("sha256:{}", manifest.sha256) {
            continue;
        }
        let relative = expected_snapshot_relative_path(id, generation)?;
        result.entry(id.clone()).or_default().insert(
            generation,
            GenerationOwnership {
                tool_id: id.clone(),
                generation,
                operation_id: operation_id.to_owned(),
                snapshot_relative_path: relative,
                manifest_digest: fingerprint.to_owned(),
                directories: directory_inventory(&path)?,
                manifest,
            },
        );
    }
    Ok(result)
}

fn write_state_document(document: &StateDocument) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec_pretty(document)?;
    let path = state_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    ccync_foundation::platform::atomic_write_bytes(&path, &bytes)
        .map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))?;
    Ok(())
}

pub fn load_state() -> Result<BTreeMap<ToolId, StateRecord>, StoreError> {
    Ok(read_state_document()?.records)
}

pub fn load_ownership() -> Result<BTreeMap<ToolId, BTreeMap<u64, GenerationOwnership>>, StoreError>
{
    Ok(read_state_document()?.ownership)
}

pub fn commit_state(state: &BTreeMap<ToolId, StateRecord>) -> Result<(), StoreError> {
    let mut document = read_state_document()?;
    document.version = VERSION;
    document.records = state.clone();
    write_state_document(&document)
}

pub fn commit_generation(
    state: &BTreeMap<ToolId, StateRecord>,
    proof: GenerationOwnership,
) -> Result<(), StoreError> {
    validate_ownership(&proof)?;
    let mut document = read_state_document()?;
    if document
        .ownership
        .get(&proof.tool_id)
        .and_then(|items| items.get(&proof.generation))
        .is_some_and(|existing| existing != &proof)
    {
        return Err(StoreError::InvalidOwnership);
    }
    for (id, generations) in legacy_ownership(&document.records)? {
        for (generation, imported) in generations {
            document
                .ownership
                .entry(id.clone())
                .or_default()
                .entry(generation)
                .or_insert(imported);
        }
    }
    let record = state
        .get(&proof.tool_id)
        .ok_or(StoreError::InvalidOwnership)?;
    if record.committed_generation != Some(proof.generation)
        || record.artifact_fingerprint.as_deref() != Some(proof.manifest_digest.as_str())
        || record.operation_id.as_deref() != Some(proof.operation_id.as_str())
    {
        return Err(StoreError::InvalidOwnership);
    }
    document.version = VERSION;
    document.records = state.clone();
    document
        .ownership
        .entry(proof.tool_id.clone())
        .or_default()
        .insert(proof.generation, proof);
    write_state_document(&document)
}

pub fn remove_tool_state(id: &ToolId) -> Result<(), StoreError> {
    let mut document = read_state_document()?;
    document.records.remove(id);
    document.ownership.remove(id);
    write_state_document(&document)
}

pub fn directory_inventory(root: &std::path::Path) -> Result<Vec<String>, StoreError> {
    validate_snapshot_ancestors(root)?;
    fn walk(
        root: &std::path::Path,
        dir: &std::path::Path,
        out: &mut Vec<String>,
    ) -> std::io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if super::file_identity::is_reparse_or_link(&entry.path())? {
                return Err(std::io::Error::other("snapshot contains a link"));
            }
            if entry.file_type()?.is_dir() {
                out.push(
                    entry
                        .path()
                        .strip_prefix(root)
                        .map_err(|_| std::io::Error::other("snapshot path escaped root"))?
                        .to_str()
                        .ok_or_else(|| std::io::Error::other("snapshot path is not UTF-8"))?
                        .replace('\\', "/"),
                );
                walk(root, &entry.path(), out)?;
            }
        }
        Ok(())
    }
    let mut dirs = Vec::new();
    walk(root, root, &mut dirs)?;
    dirs.sort();
    Ok(dirs)
}

pub(crate) fn validate_snapshot_ancestors(path: &std::path::Path) -> Result<(), StoreError> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(_) if super::file_identity::is_reparse_or_link(ancestor)? => {
                return Err(StoreError::InvalidOwnership);
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn safe_relative(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(['\\', ':'])
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

pub(crate) fn validate_manifest(
    manifest: &TreeManifest,
    digest: &str,
    directories: &[String],
) -> Result<(), StoreError> {
    let hex_digest = |s: &str| {
        s.len() == 64
            && s.bytes()
                .all(|v| v.is_ascii_hexdigit() && !v.is_ascii_uppercase())
    };
    let mut hash = Sha256::new();
    let mut prior = "";
    for file in &manifest.files {
        if !safe_relative(&file.relative_path)
            || file.relative_path.as_str() <= prior
            || !hex_digest(&file.sha256)
        {
            return Err(StoreError::InvalidOwnership);
        }
        prior = &file.relative_path;
        hash.update(file.relative_path.as_bytes());
        hash.update([0]);
        hash.update(file.length.to_be_bytes());
        hash.update([0]);
        hash.update(file.sha256.as_bytes());
        hash.update(b"\n");
    }
    if !hex_digest(&manifest.sha256)
        || hex::encode(hash.finalize()) != manifest.sha256
        || digest != format!("sha256:{}", manifest.sha256)
        || directories.iter().any(|dir| !safe_relative(dir))
        || directories.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(StoreError::InvalidOwnership);
    }
    for name in manifest
        .files
        .iter()
        .map(|f| f.relative_path.as_str())
        .chain(directories.iter().map(String::as_str))
    {
        let mut parent = std::path::Path::new(name).parent();
        while let Some(path) = parent.filter(|path| !path.as_os_str().is_empty()) {
            let dir = path
                .to_str()
                .ok_or(StoreError::InvalidOwnership)?
                .replace('\\', "/");
            if !directories.contains(&dir) {
                return Err(StoreError::InvalidOwnership);
            }
            parent = path.parent();
        }
    }
    Ok(())
}

pub(crate) fn validate_ownership(proof: &GenerationOwnership) -> Result<(), StoreError> {
    if proof.generation == 0
        || proof.operation_id.trim().is_empty()
        || proof.snapshot_relative_path
            != expected_snapshot_relative_path(&proof.tool_id, proof.generation)?
    {
        return Err(StoreError::InvalidOwnership);
    }
    validate_manifest(&proof.manifest, &proof.manifest_digest, &proof.directories)
}

pub(crate) fn committed_operation(
    journal: &super::journal::OperationJournal,
) -> Result<Option<StateRecord>, StoreError> {
    let document = read_state_document()?;
    let Some(record) = document.records.get(&journal.tool_id) else {
        return Ok(None);
    };
    let Some(generation) = journal.pending_generation else {
        return Ok(None);
    };
    if record.operation_id.as_deref() != Some(journal.operation_id.as_str())
        || record.committed_generation != Some(generation)
        || record.artifact_fingerprint.as_deref() != Some(journal.pending_manifest_digest.as_str())
        || journal.pending_snapshot_relative_path
            != expected_snapshot_relative_path(&journal.tool_id, generation)?
    {
        return Ok(None);
    }
    let Some(proof) = document
        .ownership
        .get(&journal.tool_id)
        .and_then(|items| items.get(&generation))
    else {
        return Ok(None);
    };
    if proof.operation_id != journal.operation_id
        || proof.manifest_digest != journal.pending_manifest_digest
    {
        return Err(StoreError::InvalidOwnership);
    }
    let root = ccync_foundation::paths::ccync_build_root()
        .ok_or(StoreError::PathUnavailable)?
        .join(&proof.snapshot_relative_path);
    validate_snapshot_ancestors(&root)?;
    let manifest =
        super::artifact_tree::scan_tree(&root).map_err(|_| StoreError::InvalidOwnership)?;
    if manifest != proof.manifest || directory_inventory(&root)? != proof.directories {
        return Err(StoreError::InvalidOwnership);
    }
    Ok(Some(record.clone()))
}

pub fn expected_snapshot_relative_path(
    tool_id: &ToolId,
    generation: u64,
) -> Result<PathBuf, StoreError> {
    let build = ccync_foundation::paths::ccync_build_root().ok_or(StoreError::PathUnavailable)?;
    let artifacts =
        ccync_foundation::paths::tool_artifacts_root().ok_or(StoreError::PathUnavailable)?;
    let expected = artifacts
        .join(tool_id.as_str())
        .join(generation.to_string());
    expected
        .strip_prefix(build)
        .map(PathBuf::from)
        .map_err(|_| StoreError::Io(std::io::Error::other("artifact path escaped build root")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_manager::model::{AdapterKind, DeliveryId, HoldPolicy, InstallationIdentity};
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn home() -> (TempDir, std::sync::MutexGuard<'static, ()>) {
        let guard = crate::test_support::env_guard();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("USERPROFILE", dir.path());
        (dir, guard)
    }
    fn declaration() -> ToolDeclaration {
        ToolDeclaration {
            id: ToolId::new("graphify").unwrap(),
            identity: InstallationIdentity {
                adapter_kind: AdapterKind::Cargo,
                fields: BTreeMap::new(),
            },
            delivery_id: DeliveryId::GraphifyAgents,
            hold_policy: HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        }
    }
    #[test]
    fn missing_file_loads_empty_and_declarations_round_trip() {
        let (_home, _guard) = home();
        assert!(load_declarations().unwrap().is_empty());
        let mut values = BTreeMap::new();
        values.insert(declaration().id.clone(), declaration());
        replace_declarations(&values).unwrap();
        assert_eq!(load_declarations().unwrap(), values);
    }

    #[test]
    fn overflowing_state_versions_and_invalid_ownership_are_never_overwritten() {
        let (_home, _guard) = home();
        let path = state_path().unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for value in [
            serde_json::json!({"version":4294967297u64,"records":{}}),
            serde_json::json!({"version":2,"records":{},"ownership":{"graphify":{"1":{"toolId":"graphify","generation":1,
                "operationId":"owner","snapshotRelativePath":"../foreign","manifestDigest":"sha256:invalid",
                "manifest":{"files":[],"sha256":"invalid"},"directories":[]}}}}),
        ] {
            let bytes = serde_json::to_vec(&value).unwrap();
            fs::write(&path, &bytes).unwrap();
            assert!(load_state().is_err());
            assert!(commit_state(&BTreeMap::new()).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }
    #[test]
    fn state_round_trips_with_committed_identity_fields() {
        let (_home, _guard) = home();
        let id = ToolId::new("graphify").unwrap();
        let record = StateRecord {
            committed_generation: Some(3),
            artifact_fingerprint: Some("sha256:abc".into()),
            source_evidence: serde_json::json!({"version":"1.2.3"}),
            operation_id: Some("op-123".into()),
            facets: ToolFacets {
                evidence_level: super::super::model::EvidenceLevel::Verified,
                selection_state: super::super::model::SelectionState::Unique,
                manager_availability: super::super::model::ManagerAvailability::Available,
                install_state: super::super::model::InstallState::Installed,
                package_state: super::super::model::PackageState::MatchesArtifactSource,
                update_state: super::super::model::UpdateState::NotChecked,
                artifact_state: super::super::model::ArtifactState::Current,
                projection_state: super::super::model::ProjectionState::Current,
                hold_policy: HoldPolicy::Unheld,
                manager_pin_state: super::super::model::ManagerPinState::Unknown,
                operation_state: super::super::model::OperationState::Committed,
                last_check: Some("2026-09-27T15:00:00Z".into()),
            },
        };
        let values = [(id.clone(), record)].into();
        commit_state(&values).unwrap();
        assert_eq!(load_state().unwrap(), values);
    }
    #[test]
    fn identical_insert_preserves_bytes_and_conflict_requires_untrack() {
        let (_home, _guard) = home();
        let value = declaration();
        assert_eq!(
            insert_declaration(value.clone()).unwrap(),
            InsertResult::Inserted
        );
        let path = declaration_path().unwrap();
        let before = fs::read(&path).unwrap();
        assert_eq!(
            insert_declaration(value.clone()).unwrap(),
            InsertResult::Unchanged
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        let mut changed = value;
        changed.hold_policy = HoldPolicy::Held;
        assert!(matches!(
            insert_declaration(changed),
            Err(StoreError::ConflictRequiresUntrack(_))
        ));
    }
    #[test]
    fn malformed_and_unsafe_files_are_not_overwritten() {
        let (_home, _guard) = home();
        let decl_path = declaration_path().unwrap();
        fs::create_dir_all(decl_path.parent().unwrap()).unwrap();
        for bytes in [
            b"not json".as_slice(),
            br#"{"version":9,"records":{}}"#.as_slice(),
            br#"{"version":1,"records":{"../bad":{"id":"../bad"}}}"#.as_slice(),
        ] {
            fs::write(&decl_path, bytes).unwrap();
            let before = fs::read(&decl_path).unwrap();
            assert!(load_declarations().is_err());
            assert!(replace_declarations(&BTreeMap::new()).is_err());
            assert_eq!(fs::read(&decl_path).unwrap(), before);
        }

        let st_path = state_path().unwrap();
        fs::create_dir_all(st_path.parent().unwrap()).unwrap();
        for bytes in [
            b"not json".as_slice(),
            br#"{"version":9,"records":{}}"#.as_slice(),
            br#"{"version":1,"records":{"../bad":{"committedGeneration":1}}}"#.as_slice(),
        ] {
            fs::write(&st_path, bytes).unwrap();
            let before = fs::read(&st_path).unwrap();
            assert!(load_state().is_err());
            assert!(commit_state(&BTreeMap::new()).is_err());
            assert_eq!(fs::read(&st_path).unwrap(), before);
        }
    }
}
