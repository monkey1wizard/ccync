//! Private staging and journal-owned artifact snapshot commits.
use super::{
    artifact_tree::{self, TreeError, TreeManifest},
    journal::OperationJournal,
    model::ToolId,
    store::{self, StateRecord, StoreError},
};
use std::{
    fs, io,
    ops::Deref,
    path::{Path, PathBuf},
};

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("snapshot path is unavailable")]
    PathUnavailable,
    #[error("snapshot I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("snapshot tree failed: {0}")]
    Tree(#[from] TreeError),
    #[error("snapshot state failed: {0}")]
    Store(#[from] StoreError),
    #[error("snapshot ownership conflict: {0}")]
    Ownership(String),
    #[error("snapshot generation must be {expected}, got {actual}")]
    Generation { expected: u64, actual: u64 },
    #[error("snapshot journal is invalid: {0}")]
    Journal(String),
}

fn artifacts_root() -> Result<PathBuf, SnapshotError> {
    tool_paths().map(|paths| paths.artifacts)
}
fn staging_root() -> Result<PathBuf, SnapshotError> {
    tool_paths().map(|paths| paths.staging)
}
fn journal_path() -> Result<PathBuf, SnapshotError> {
    tool_paths().map(|paths| paths.journal)
}
fn tool_paths() -> Result<store::ToolPaths, SnapshotError> {
    store::ToolPaths::load().map_err(|error| match error {
        StoreError::PathUnavailable => SnapshotError::PathUnavailable,
        StoreError::Io(error) => SnapshotError::Io(error),
        other => SnapshotError::Io(io::Error::other(other.to_string())),
    })
}
fn generation_path(id: &ToolId, generation: u64) -> Result<PathBuf, SnapshotError> {
    Ok(artifacts_root()?
        .join(id.as_str())
        .join(generation.to_string()))
}

/// Owns one operation's staging directory and removes only that directory on drop.
pub struct StagingDirectory {
    path: PathBuf,
}
impl Deref for StagingDirectory {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}
impl StagingDirectory {
    pub fn path(&self) -> &Path {
        &self.path
    }
}
impl Drop for StagingDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn create_staging(operation_id: &str) -> Result<StagingDirectory, SnapshotError> {
    if operation_id.is_empty()
        || !operation_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(SnapshotError::Journal("invalid operation id".into()));
    }
    let path = staging_root()?.join(operation_id);
    fs::create_dir_all(path.parent().unwrap())?;
    fs::create_dir(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(StagingDirectory { path })
}

fn write_journal(journal: &OperationJournal) -> Result<(), SnapshotError> {
    let path = journal_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes =
        serde_json::to_vec_pretty(journal).map_err(|e| SnapshotError::Journal(e.to_string()))?;
    ccync_foundation::platform::atomic_write_bytes(&path, &bytes)
        .map_err(|e| SnapshotError::Io(io::Error::other(e.to_string())))?;
    Ok(())
}

fn exact_journal_owner(
    journal: &OperationJournal,
    generation: u64,
    relative: &Path,
    digest: &str,
) -> bool {
    journal.pending_generation == Some(generation)
        && !journal.operation_id.is_empty()
        && journal.pending_snapshot_relative_path == relative
        && journal.pending_manifest_digest == digest
}

/// Commit one snapshot under an existing operation journal.
pub fn commit_snapshot(
    id: &ToolId,
    generation: u64,
    manifest: &TreeManifest,
    src: impl AsRef<Path>,
    journal: &mut OperationJournal,
) -> Result<(), SnapshotError> {
    let staging = create_staging(&journal.operation_id)?;
    let result = (|| {
        if id != &journal.tool_id {
            return Err(SnapshotError::Journal(
                "tool id does not match journal".into(),
            ));
        }
        let states = store::load_state()?;
        let previous = states
            .get(id)
            .and_then(|s| s.committed_generation)
            .unwrap_or(0);
        let prior_record = states.get(id).cloned();
        let expected = previous
            .checked_add(1)
            .ok_or_else(|| SnapshotError::Ownership("generation overflow".into()))?;
        if generation != expected {
            return Err(SnapshotError::Generation {
                expected,
                actual: generation,
            });
        }
        let paths = tool_paths()?;
        let dest = paths
            .artifacts
            .join(id.as_str())
            .join(generation.to_string());
        let rel = dest
            .strip_prefix(&paths.snapshot_base)
            .map_err(|_| SnapshotError::Ownership("snapshot path escapes build root".into()))?
            .to_path_buf();
        let digest = format!("sha256:{}", manifest.sha256);
        let recovery_owner = exact_journal_owner(journal, generation, &rel, &digest);
        journal.pending_generation = Some(generation);
        journal.pending_snapshot_relative_path = rel.clone();
        journal.pending_manifest_digest = digest.clone();
        write_journal(journal)?;
        let owner = exact_journal_owner(journal, generation, &rel, &digest);
        if dest.exists() {
            if states
                .get(id)
                .is_some_and(|s| s.committed_generation == Some(generation))
            {
                return Err(SnapshotError::Ownership("generation is committed".into()));
            }
            if !owner || !recovery_owner {
                return Err(SnapshotError::Ownership(
                    "generation is not named by this journal".into(),
                ));
            }
            let prior = artifact_tree::scan_tree(&dest)?;
            if format!("sha256:{}", prior.sha256) != digest {
                return Err(SnapshotError::Ownership(
                    "existing generation digest differs from journal".into(),
                ));
            }
            fs::remove_dir_all(&dest)?;
        }
        fs::create_dir_all(dest.parent().unwrap())?;
        let temp = dest
            .parent()
            .unwrap()
            .join(format!(".{}.{}.tmp", generation, journal.operation_id));
        if temp.exists() {
            return Err(SnapshotError::Ownership(
                "temporary sibling already exists".into(),
            ));
        }
        fs::create_dir(&temp)?;
        let built = (|| {
            artifact_tree::materialize(manifest, src, &temp)?;
            fs::rename(&temp, &dest)?;
            Ok::<_, SnapshotError>(())
        })();
        if built.is_err() {
            let _ = fs::remove_dir_all(&temp);
            return built;
        }
        let mut next = states;
        if let Some(mut record) = next.get(id).cloned() {
            record.committed_generation = Some(generation);
            record.artifact_fingerprint = Some(digest);
            record.operation_id = Some(journal.operation_id.clone());
            next.insert(id.clone(), record);
        } else {
            return Err(SnapshotError::Ownership(
                "tool state record must exist before snapshot commit".into(),
            ));
        }
        store::commit_state(&next)?;
        prune_after_commit(id, journal, prior_record.as_ref())?;
        Ok(())
    })();
    drop(staging);
    if result.is_err() {
        let _ = cleanup_uncommitted(id, generation, manifest, journal);
    }
    result
}

fn cleanup_uncommitted(
    id: &ToolId,
    generation: u64,
    manifest: &TreeManifest,
    journal: &OperationJournal,
) -> Result<(), SnapshotError> {
    let states = store::load_state()?;
    let path = generation_path(id, generation)?;
    if states
        .get(id)
        .is_some_and(|s| s.committed_generation == Some(generation))
        || !path.exists()
    {
        return Ok(());
    }
    let root = tool_paths()?.snapshot_base;
    let rel = path
        .strip_prefix(root)
        .map_err(|_| SnapshotError::Ownership("path escapes build root".into()))?;
    let digest = format!("sha256:{}", manifest.sha256);
    if exact_journal_owner(journal, generation, rel, &digest)
        && format!("sha256:{}", artifact_tree::scan_tree(&path)?.sha256) == digest
    {
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

/// Remove only other generations whose exact identity is proven by state or this journal.
pub fn prune_generations(id: &ToolId, journal: &OperationJournal) -> Result<(), SnapshotError> {
    let states = store::load_state()?;
    let root = artifacts_root()?.join(id.as_str());
    if !root.exists() {
        return Ok(());
    }
    for child in fs::read_dir(&root)? {
        let child = child?;
        if !child.file_type()?.is_dir() {
            return Err(SnapshotError::Ownership(format!(
                "unknown artifact child: {}",
                child.path().display()
            )));
        }
        let name = child.file_name().to_string_lossy().into_owned();
        let Ok(generation) = name.parse::<u64>() else {
            return Err(SnapshotError::Ownership(format!(
                "unknown artifact child: {name}"
            )));
        };
        let state_owned = states
            .get(id)
            .is_some_and(|s| s.committed_generation == Some(generation));
        let journal_owned = journal.pending_generation == Some(generation)
            && journal
                .pending_snapshot_relative_path
                .ends_with(Path::new(&name))
            && journal.pending_manifest_digest.starts_with("sha256:")
            && artifact_tree::scan_tree(child.path())
                .map(|m| format!("sha256:{}", m.sha256) == journal.pending_manifest_digest)
                .unwrap_or(false);
        if !state_owned && !journal_owned {
            return Err(SnapshotError::Ownership(format!(
                "unowned generation {generation}"
            )));
        }
    }
    Ok(())
}

fn prune_after_commit(
    id: &ToolId,
    journal: &OperationJournal,
    previous: Option<&StateRecord>,
) -> Result<(), SnapshotError> {
    let states = store::load_state()?;
    let current = states.get(id).and_then(|s| s.committed_generation);
    let root = artifacts_root()?.join(id.as_str());
    if !root.exists() {
        return Ok(());
    }
    for child in fs::read_dir(&root)? {
        let child = child?;
        let name = child.file_name().to_string_lossy().into_owned();
        let generation = name
            .parse::<u64>()
            .map_err(|_| SnapshotError::Ownership(format!("unknown artifact child: {name}")))?;
        if !child.file_type()?.is_dir() {
            return Err(SnapshotError::Ownership(format!(
                "unknown artifact child: {name}"
            )));
        }
        if Some(generation) == current {
            continue;
        }
        let manifest = artifact_tree::scan_tree(child.path())?;
        let digest = format!("sha256:{}", manifest.sha256);
        let was_committed = previous.is_some_and(|s| {
            s.committed_generation == Some(generation)
                && s.artifact_fingerprint.as_deref() == Some(digest.as_str())
                && s.operation_id.is_some()
        });
        let journal_owned = journal.pending_generation == Some(generation)
            && journal.pending_manifest_digest == digest
            && journal
                .pending_snapshot_relative_path
                .ends_with(Path::new(&name));
        if was_committed || journal_owned {
            fs::remove_dir_all(child.path())?;
        } else {
            return Err(SnapshotError::Ownership(format!(
                "unowned generation {generation}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{
        journal::{OperationKind, RetryIntent},
        model::*,
    };
    use super::*;

    fn state_record() -> StateRecord {
        StateRecord {
            committed_generation: None,
            artifact_fingerprint: None,
            source_evidence: serde_json::json!({}),
            operation_id: None,
            facets: ToolFacets {
                evidence_level: EvidenceLevel::Verified,
                selection_state: SelectionState::Unique,
                manager_availability: ManagerAvailability::Available,
                install_state: InstallState::Installed,
                package_state: PackageState::MatchesArtifactSource,
                update_state: UpdateState::NotChecked,
                artifact_state: ArtifactState::Current,
                projection_state: ProjectionState::Current,
                hold_policy: HoldPolicy::Unheld,
                manager_pin_state: ManagerPinState::Unknown,
                operation_state: OperationState::Prepared,
                last_check: None,
            },
        }
    }

    #[test]
    fn commit_increments_generation_and_records_snapshot_identity() {
        let h = crate::test_support::TestHome::new();
        let id = ToolId::new("tool").unwrap();
        store::commit_state(&[(id.clone(), state_record())].into()).unwrap();
        let src = tempfile::tempdir().unwrap();
        fs::write(src.path().join("bin"), b"tool-bytes").unwrap();
        let manifest = artifact_tree::scan_tree(src.path()).unwrap();
        let mut journal = OperationJournal::new(
            id.clone(),
            OperationKind::Refresh,
            RetryIntent::Refresh,
            None,
            PathBuf::new(),
            String::new(),
        );
        commit_snapshot(&id, 1, &manifest, src.path(), &mut journal).unwrap();
        assert_eq!(
            store::load_state().unwrap()[&id].committed_generation,
            Some(1)
        );
        assert_eq!(
            artifact_tree::scan_tree(generation_path(&id, 1).unwrap()).unwrap(),
            manifest
        );
        assert_eq!(
            store::load_state().unwrap()[&id].operation_id,
            Some(journal.operation_id)
        );
    }
    #[test]
    fn staging_is_private_and_removed_after_scope_failure() {
        let h = crate::test_support::TestHome::new();
        let p;
        {
            let stage = create_staging("op-1").unwrap();
            p = stage.path().to_path_buf();
            assert!(p.is_dir());
            assert_eq!(fs::read_dir(&p).unwrap().count(), 0);
        }
        assert!(!p.exists());
    }
    #[test]
    fn generation_conflict_fails_closed_for_unowned_directory() {
        let h = crate::test_support::TestHome::new();
        let id = ToolId::new("tool").unwrap();
        let dir = generation_path(&id, 1).unwrap();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("unknown"), b"user").unwrap();
        let journal = OperationJournal::new(
            id.clone(),
            super::super::journal::OperationKind::Refresh,
            super::super::journal::RetryIntent::Refresh,
            None,
            PathBuf::new(),
            String::new(),
        );
        assert!(prune_generations(&id, &journal).is_err());
        assert!(dir.join("unknown").exists());
    }

    #[test]
    fn replacement_only_of_uncommitted_generation_named_by_same_journal() {
        let h = crate::test_support::TestHome::new();
        let id = ToolId::new("tool").unwrap();
        store::commit_state(&[(id.clone(), state_record())].into()).unwrap();
        let src = tempfile::tempdir().unwrap();
        fs::write(src.path().join("bin"), b"tool-bytes-v1").unwrap();
        let manifest = artifact_tree::scan_tree(src.path()).unwrap();
        let dest = generation_path(&id, 1).unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("bin"), b"tool-bytes-v1").unwrap();

        let rel = dest
            .strip_prefix(ccync_foundation::paths::ccync_build_root().unwrap())
            .unwrap()
            .to_path_buf();
        let digest = format!("sha256:{}", manifest.sha256);

        // A journal that does NOT name the generation must fail to replace
        let mut unowned_journal = OperationJournal::new(
            id.clone(),
            OperationKind::Refresh,
            RetryIntent::Refresh,
            None,
            PathBuf::new(),
            String::new(),
        );
        let err = commit_snapshot(&id, 1, &manifest, src.path(), &mut unowned_journal).unwrap_err();
        assert!(matches!(err, SnapshotError::Ownership(_)));

        // A journal that positively names the exact generation, relative path, and digest can replace
        let mut matching_journal = OperationJournal::new(
            id.clone(),
            OperationKind::Refresh,
            RetryIntent::Refresh,
            Some(1),
            rel,
            digest,
        );
        commit_snapshot(&id, 1, &manifest, src.path(), &mut matching_journal).unwrap();
        assert_eq!(
            store::load_state().unwrap()[&id].committed_generation,
            Some(1)
        );
    }

    #[test]
    fn refusal_to_overwrite_committed_or_unowned_generations() {
        let h = crate::test_support::TestHome::new();
        let id = ToolId::new("tool").unwrap();
        let mut state = state_record();
        state.committed_generation = Some(1);
        store::commit_state(&[(id.clone(), state)].into()).unwrap();

        let src = tempfile::tempdir().unwrap();
        fs::write(src.path().join("bin"), b"tool-bytes").unwrap();
        let manifest = artifact_tree::scan_tree(src.path()).unwrap();
        let mut journal = OperationJournal::new(
            id.clone(),
            OperationKind::Refresh,
            RetryIntent::Refresh,
            None,
            PathBuf::new(),
            String::new(),
        );

        // Attempting to commit generation 1 when generation 1 is committed must fail
        let err = commit_snapshot(&id, 1, &manifest, src.path(), &mut journal).unwrap_err();
        assert!(matches!(
            err,
            SnapshotError::Generation {
                expected: 2,
                actual: 1
            }
        ));

        // Attempting to commit generation 2 when an unowned generation 2 directory already exists
        let dir2 = generation_path(&id, 2).unwrap();
        fs::create_dir_all(&dir2).unwrap();
        fs::write(dir2.join("bin"), b"other-content").unwrap();
        let err2 = commit_snapshot(&id, 2, &manifest, src.path(), &mut journal).unwrap_err();
        assert!(matches!(err2, SnapshotError::Ownership(_)));
    }

    #[test]
    fn fail_closed_handling_of_unknown_children() {
        let h = crate::test_support::TestHome::new();
        let id = ToolId::new("tool").unwrap();
        let root = artifacts_root().unwrap().join(id.as_str());
        fs::create_dir_all(&root).unwrap();

        // 1. Non-directory file in artifacts root
        fs::write(root.join("stray_file.txt"), b"stray").unwrap();
        let journal = OperationJournal::new(
            id.clone(),
            OperationKind::Refresh,
            RetryIntent::Refresh,
            None,
            PathBuf::new(),
            String::new(),
        );
        assert!(prune_generations(&id, &journal).is_err());
        assert!(root.join("stray_file.txt").exists());
        fs::remove_file(root.join("stray_file.txt")).unwrap();

        // 2. Directory that does not parse as u64
        fs::create_dir(root.join("not_a_number")).unwrap();
        assert!(prune_generations(&id, &journal).is_err());
        assert!(root.join("not_a_number").exists());
    }

    #[test]
    fn prune_after_success_and_cleanup_after_failure() {
        let h = crate::test_support::TestHome::new();
        let id = ToolId::new("tool").unwrap();
        store::commit_state(&[(id.clone(), state_record())].into()).unwrap();

        let src1 = tempfile::tempdir().unwrap();
        fs::write(src1.path().join("bin"), b"v1").unwrap();
        let manifest1 = artifact_tree::scan_tree(src1.path()).unwrap();
        let mut journal1 = OperationJournal::new(
            id.clone(),
            OperationKind::Refresh,
            RetryIntent::Refresh,
            None,
            PathBuf::new(),
            String::new(),
        );
        commit_snapshot(&id, 1, &manifest1, src1.path(), &mut journal1).unwrap();
        assert!(generation_path(&id, 1).unwrap().exists());

        // Now commit generation 2: prune_after_commit will remove generation 1
        let src2 = tempfile::tempdir().unwrap();
        fs::write(src2.path().join("bin"), b"v2").unwrap();
        let manifest2 = artifact_tree::scan_tree(src2.path()).unwrap();
        let mut journal2 = OperationJournal::new(
            id.clone(),
            OperationKind::Refresh,
            RetryIntent::Refresh,
            None,
            PathBuf::new(),
            String::new(),
        );
        commit_snapshot(&id, 2, &manifest2, src2.path(), &mut journal2).unwrap();
        assert!(generation_path(&id, 2).unwrap().exists());
        assert!(!generation_path(&id, 1).unwrap().exists());

        // Test cleanup_uncommitted
        let dest3 = generation_path(&id, 3).unwrap();
        fs::create_dir_all(&dest3).unwrap();
        fs::write(dest3.join("bin"), b"v3").unwrap();
        let manifest3 = artifact_tree::scan_tree(&dest3).unwrap();
        let rel3 = dest3
            .strip_prefix(ccync_foundation::paths::ccync_build_root().unwrap())
            .unwrap()
            .to_path_buf();
        let digest3 = format!("sha256:{}", manifest3.sha256);
        let journal3 = OperationJournal::new(
            id.clone(),
            OperationKind::Refresh,
            RetryIntent::Refresh,
            Some(3),
            rel3,
            digest3,
        );
        // Uncommitted gen 3 owned by journal3 gets cleaned up
        cleanup_uncommitted(&id, 3, &manifest3, &journal3).unwrap();
        assert!(!dest3.exists());
    }
}
