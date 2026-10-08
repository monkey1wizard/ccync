//! Journaled removal of a tool declaration and positively owned snapshots.
use super::{
    artifact_tree,
    journal::{self, OperationJournal, OperationKind, OperationStage, RetryIntent},
    lock::LifecycleGuard,
    model::{DeliveryId, ToolId},
    store,
};
use crate::install::{ProjectionRunReport, ProjectionTargetStatus};
use std::fs;

#[derive(Debug, thiserror::Error)]
pub enum UntrackError {
    #[error("tool declaration was not found")]
    Missing,
    #[error("tool artifacts contain entries without proven ownership")]
    Ownership,
    #[error("projection did not complete successfully")]
    Projection,
    #[error("untrack operation failed: {0}")]
    Failed(String),
}

/// Remove a declaration only after projection and snapshot ownership are proven.
pub fn untrack_tool<P>(guard: &LifecycleGuard, id: ToolId, project: P) -> Result<(), UntrackError>
where
    P: FnOnce() -> ProjectionRunReport,
{
    untrack_with_checkpoint(guard, id, project, |_| Ok(()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CleanupCheckpoint {
    Projected,
    FileRemoved,
    ArtifactsRemoved,
    StateRemoved,
    DeclarationRemoved,
    Committed,
}

fn untrack_with_checkpoint<P, C>(
    guard: &LifecycleGuard,
    id: ToolId,
    project: P,
    mut checkpoint: C,
) -> Result<(), UntrackError>
where
    P: FnOnce() -> ProjectionRunReport,
    C: FnMut(CleanupCheckpoint) -> Result<(), UntrackError>,
{
    let mut declarations = store::load_declarations().map_err(fail)?;
    let existing_journal = read_journal().map_err(fail)?;
    let declaration = declarations.get(&id).cloned();
    if declaration.is_none() && existing_journal.is_none() {
        return Err(UntrackError::Missing);
    }
    if let Some(journal) = &existing_journal {
        if journal.tool_id != id || journal.kind != OperationKind::Untrack {
            return Err(UntrackError::Ownership);
        }
        journal.validate_identity().map_err(fail)?;
    }
    let states = store::load_state().map_err(fail)?;
    let state = states.get(&id).cloned();
    let ownership = store::load_ownership().map_err(fail)?;
    let artifacts_root = ccync_foundation::paths::tool_artifacts_root()
        .ok_or_else(|| UntrackError::Failed("artifact path unavailable".into()))?;
    let tool_root = artifacts_root.join(id.as_str());
    store::validate_snapshot_ancestors(&tool_root).map_err(|_| UntrackError::Ownership)?;
    if let Some(journal) = &existing_journal {
        if journal.stage == OperationStage::Committed {
            if declaration.is_some()
                || state.is_some()
                || ownership.contains_key(&id)
                || tool_root.exists()
            {
                return Err(UntrackError::Ownership);
            }
            journal.remove(guard).map_err(fail)?;
            return Ok(());
        }
        if tool_root.exists() {
            for entry in fs::read_dir(&tool_root).map_err(fail)? {
                let entry = entry.map_err(fail)?;
                if !journal
                    .cleanup_plan
                    .iter()
                    .any(|plan| entry.file_name() == plan.generation.to_string().as_str())
                {
                    return Err(UntrackError::Ownership);
                }
            }
        }
        for plan in &journal.cleanup_plan {
            let path = artifact_root_from_relative(&id, plan)?;
            validate_remaining(&path, plan)?;
            if journal.stage == OperationStage::Prepared {
                let manifest =
                    artifact_tree::scan_tree(&path).map_err(|_| UntrackError::Ownership)?;
                if manifest != plan.manifest
                    || store::directory_inventory(&path).map_err(fail)? != plan.directories
                {
                    return Err(UntrackError::Ownership);
                }
            }
        }
    }
    let mut owned = Vec::new();
    if existing_journal.is_none() && tool_root.exists() {
        for entry in fs::read_dir(&tool_root).map_err(fail)? {
            let entry = entry.map_err(fail)?;
            if !entry.file_type().map_err(fail)?.is_dir() {
                return Err(UntrackError::Ownership);
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let generation: u64 = name.parse().map_err(|_| UntrackError::Ownership)?;
            if generation.to_string() != name {
                return Err(UntrackError::Ownership);
            }
            let manifest =
                artifact_tree::scan_tree(entry.path()).map_err(|_| UntrackError::Ownership)?;
            let proof = ownership.get(&id).and_then(|items| items.get(&generation));
            let legacy = proof.is_none()
                && state.as_ref().is_some_and(|record| {
                    record.committed_generation == Some(generation)
                        && record.artifact_fingerprint.as_deref()
                            == Some(&format!("sha256:{}", manifest.sha256))
                        && record
                            .operation_id
                            .as_deref()
                            .is_some_and(|s| !s.is_empty())
                });
            if let Some(proof) = proof {
                if proof.tool_id != id
                    || proof.generation != generation
                    || proof.snapshot_relative_path
                        != store::expected_snapshot_relative_path(&id, generation).map_err(fail)?
                    || proof.manifest_digest != format!("sha256:{}", manifest.sha256)
                    || proof.manifest != manifest
                    || store::directory_inventory(&entry.path()).map_err(fail)? != proof.directories
                {
                    return Err(UntrackError::Ownership);
                }
            } else if !legacy {
                return Err(UntrackError::Ownership);
            }
            let manifest = proof.map(|p| p.manifest.clone()).unwrap_or(manifest);
            owned.push(journal::CleanupGeneration {
                generation,
                operation_id: proof
                    .map(|proof| proof.operation_id.clone())
                    .or_else(|| state.as_ref().and_then(|state| state.operation_id.clone()))
                    .ok_or(UntrackError::Ownership)?,
                snapshot_relative_path: store::expected_snapshot_relative_path(&id, generation)
                    .map_err(fail)?,
                manifest_digest: format!("sha256:{}", manifest.sha256),
                directories: store::directory_inventory(&entry.path()).map_err(fail)?,
                manifest,
            });
        }
    }
    let committed_mode_c = declaration
        .as_ref()
        .is_some_and(|d| d.delivery_id == DeliveryId::GraphifyAgents)
        && state
            .as_ref()
            .and_then(|record| record.committed_generation)
            .is_some();
    let mut journal = if let Some(journal) = existing_journal {
        journal
    } else {
        let mut journal = OperationJournal::new(
            id.clone(),
            OperationKind::Untrack,
            RetryIntent::Untrack {
                tool_id: id.clone(),
            },
            None,
            Default::default(),
            String::new(),
        );
        journal.cleanup_plan = owned;
        journal.validate_identity().map_err(fail)?;
        journal.create(guard).map_err(fail)?;
        journal
    };
    if journal.stage == OperationStage::Prepared && committed_mode_c {
        let report = project();
        let targets_valid = !report.targets.is_empty()
            && !report.targets.values().any(|target| {
                matches!(
                    target.status,
                    ProjectionTargetStatus::Failed { .. } | ProjectionTargetStatus::NotRun
                )
            })
            && report.targets.values().all(|target| match &target.status {
                ProjectionTargetStatus::Applied | ProjectionTargetStatus::Unchanged => true,
                ProjectionTargetStatus::Skipped { reason } => !reason.trim().is_empty(),
                _ => false,
            });
        if !targets_valid {
            return Err(UntrackError::Projection);
        }
    }
    if journal.stage == OperationStage::Prepared {
        journal
            .advance(guard, OperationStage::ProjectionSucceeded)
            .map_err(fail)?;
    }
    checkpoint(CleanupCheckpoint::Projected)?;
    for plan in &journal.cleanup_plan {
        let path = artifact_root_from_relative(&id, plan).map_err(fail)?;
        validate_remaining(&path, plan).map_err(fail)?;
        for file in plan.manifest.files.iter().rev() {
            let file_path = path.join(
                file.relative_path
                    .replace('/', std::path::MAIN_SEPARATOR_STR),
            );
            if file_path.exists() {
                validate_remaining(&path, plan)?;
                if !file_path.is_file() {
                    return Err(UntrackError::Ownership);
                }
                fs::remove_file(file_path).map_err(fail)?;
                checkpoint(CleanupCheckpoint::FileRemoved)?;
            }
        }
        for dir in plan.directories.iter().rev() {
            match fs::remove_dir(path.join(dir)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(fail(error)),
            }
        }
        if path.exists() {
            fs::remove_dir(path).map_err(fail)?;
        }
    }
    if tool_root.exists() && fs::read_dir(&tool_root).map_err(fail)?.next().is_none() {
        fs::remove_dir(&tool_root).map_err(fail)?;
    }
    checkpoint(CleanupCheckpoint::ArtifactsRemoved)?;
    store::remove_tool_state(&id).map_err(fail)?;
    checkpoint(CleanupCheckpoint::StateRemoved)?;
    declarations.remove(&id);
    store::replace_declarations(&declarations).map_err(fail)?;
    checkpoint(CleanupCheckpoint::DeclarationRemoved)?;
    journal
        .advance(guard, OperationStage::Committed)
        .map_err(fail)?;
    checkpoint(CleanupCheckpoint::Committed)?;
    journal.remove(guard).map_err(fail)?;
    Ok(())
}

fn fail(error: impl std::fmt::Display) -> UntrackError {
    UntrackError::Failed(error.to_string())
}

fn read_journal() -> Result<Option<OperationJournal>, std::io::Error> {
    let path = ccync_foundation::paths::tool_operation_journal_path()
        .ok_or_else(|| std::io::Error::other("journal path unavailable"))?;
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| std::io::Error::other(e.to_string()))
}

fn artifact_root_from_relative(
    id: &ToolId,
    plan: &journal::CleanupGeneration,
) -> Result<std::path::PathBuf, UntrackError> {
    let build = ccync_foundation::paths::ccync_build_root()
        .ok_or_else(|| UntrackError::Failed("build root unavailable".into()))?;
    let expected = store::expected_snapshot_relative_path(id, plan.generation).map_err(fail)?;
    if plan.snapshot_relative_path != expected || plan.snapshot_relative_path.is_absolute() {
        return Err(UntrackError::Ownership);
    }
    let path = build.join(&plan.snapshot_relative_path);
    store::validate_snapshot_ancestors(&path).map_err(|_| UntrackError::Ownership)?;
    if !path.starts_with(&build) {
        return Err(UntrackError::Ownership);
    }
    Ok(path)
}

fn validate_remaining(
    path: &std::path::Path,
    plan: &journal::CleanupGeneration,
) -> Result<(), UntrackError> {
    if !path.exists() {
        return Ok(());
    }
    let current = artifact_tree::scan_tree(path).map_err(|_| UntrackError::Ownership)?;
    if !current
        .files
        .iter()
        .all(|file| plan.manifest.files.iter().any(|expected| expected == file))
        || !store::directory_inventory(path)
            .map_err(fail)?
            .iter()
            .all(|dir| plan.directories.contains(dir))
    {
        return Err(UntrackError::Ownership);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::{ProjectionRunReport, ProjectionTargetResult, ProjectionTargetStatus};
    use crate::tool_manager::{
        lock::LifecycleLock,
        model::{
            AdapterKind, ArtifactState, EvidenceLevel, HoldPolicy, InstallState,
            InstallationIdentity, ManagerAvailability, ManagerPinState, OperationState,
            PackageState, ProjectionState, SelectionState, ToolDeclaration, ToolFacets,
            UpdateState,
        },
    };
    use ccync_foundation::target::ProjectionTargetId;
    use std::collections::BTreeMap;

    fn owned_fixture() -> (ToolId, std::path::PathBuf) {
        let id = ToolId::new("graphify").unwrap();
        store::insert_declaration(ToolDeclaration {
            id: id.clone(),
            identity: InstallationIdentity {
                adapter_kind: AdapterKind::Cargo,
                fields: BTreeMap::new(),
            },
            delivery_id: DeliveryId::GraphifyAgents,
            hold_policy: HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        })
        .unwrap();
        let path = ccync_foundation::paths::tool_artifacts_root()
            .unwrap()
            .join(id.as_str())
            .join("1");
        fs::create_dir_all(path.join("empty")).unwrap();
        fs::write(path.join("a"), "first").unwrap();
        fs::write(path.join("z"), "last").unwrap();
        let manifest = artifact_tree::scan_tree(&path).unwrap();
        let proof = store::GenerationOwnership {
            tool_id: id.clone(),
            generation: 1,
            operation_id: "origin-operation".into(),
            snapshot_relative_path: store::expected_snapshot_relative_path(&id, 1).unwrap(),
            manifest_digest: format!("sha256:{}", manifest.sha256),
            directories: store::directory_inventory(&path).unwrap(),
            manifest,
        };
        store::commit_generation(
            &[(
                id.clone(),
                test_record(
                    Some(1),
                    Some(proof.manifest_digest.clone()),
                    Some(proof.operation_id.clone()),
                ),
            )]
            .into(),
            proof,
        )
        .unwrap();
        (id, path)
    }

    #[test]
    fn cleanup_resumes_at_every_persisted_boundary() {
        for point in [
            CleanupCheckpoint::Projected,
            CleanupCheckpoint::FileRemoved,
            CleanupCheckpoint::ArtifactsRemoved,
            CleanupCheckpoint::StateRemoved,
            CleanupCheckpoint::DeclarationRemoved,
            CleanupCheckpoint::Committed,
        ] {
            let _env = crate::test_support::env_guard();
            let home = tempfile::tempdir().unwrap();
            std::env::set_var("USERPROFILE", home.path());
            let guard = LifecycleLock::acquire().unwrap();
            let (id, path) = owned_fixture();
            let mut projections = 0;
            let result = untrack_with_checkpoint(
                &guard,
                id.clone(),
                || {
                    projections += 1;
                    make_report(ProjectionTargetStatus::Applied)
                },
                |observed| {
                    if observed == point {
                        Err(UntrackError::Failed("injected interruption".into()))
                    } else {
                        Ok(())
                    }
                },
            );
            assert!(result.is_err(), "{point:?}");
            assert!(read_journal().unwrap().is_some());
            untrack_tool(&guard, id.clone(), || {
                panic!("persisted projection must not repeat: {point:?}")
            })
            .unwrap();
            assert_eq!(projections, 1);
            assert!(!path.parent().unwrap().exists(), "{point:?}");
            assert!(!store::load_state().unwrap().contains_key(&id));
            assert!(!store::load_ownership().unwrap().contains_key(&id));
            assert!(!store::load_declarations().unwrap().contains_key(&id));
            assert!(read_journal().unwrap().is_none());
        }
    }

    #[test]
    fn cleanup_retry_blocks_changed_files_and_extra_entries_before_more_deletion() {
        for scenario in [
            "changed",
            "extra-file",
            "extra-directory",
            "extra-generation",
        ] {
            let _env = crate::test_support::env_guard();
            let home = tempfile::tempdir().unwrap();
            std::env::set_var("USERPROFILE", home.path());
            let guard = LifecycleLock::acquire().unwrap();
            let (id, path) = owned_fixture();
            assert!(untrack_with_checkpoint(
                &guard,
                id.clone(),
                || make_report(ProjectionTargetStatus::Applied),
                |point| if point == CleanupCheckpoint::FileRemoved {
                    Err(fail("injected"))
                } else {
                    Ok(())
                }
            )
            .is_err());
            match scenario {
                "changed" => fs::write(path.join("a"), "foreign-change").unwrap(),
                "extra-file" => fs::write(path.join("foreign"), "foreign-file").unwrap(),
                "extra-directory" => fs::create_dir(path.join("foreign")).unwrap(),
                _ => fs::create_dir(path.parent().unwrap().join("2")).unwrap(),
            }
            let before = fs::read(path.join("a")).unwrap();
            assert!(
                untrack_tool(&guard, id.clone(), || panic!(
                    "no projection on tampered retry"
                ))
                .is_err(),
                "{scenario}"
            );
            assert_eq!(fs::read(path.join("a")).unwrap(), before);
            assert!(store::load_declarations().unwrap().contains_key(&id));
        }
    }

    #[test]
    fn legacy_digest_mismatch_is_never_adopted_or_deleted() {
        let _env = crate::test_support::env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("USERPROFILE", home.path());
        let guard = LifecycleLock::acquire().unwrap();
        let (id, path) = owned_fixture();
        let mut state = store::load_state().unwrap();
        state.get_mut(&id).unwrap().artifact_fingerprint = Some("sha256:wrong".into());
        fs::write(
            ccync_foundation::paths::tool_state_path().unwrap(),
            serde_json::to_vec(&serde_json::json!({"version":1,"records":state})).unwrap(),
        )
        .unwrap();
        assert!(untrack_tool(&guard, id.clone(), || panic!(
            "unproven legacy must not project"
        ))
        .is_err());
        assert_eq!(fs::read(path.join("a")).unwrap(), b"first");
        assert!(read_journal().unwrap().is_none());
    }

    #[test]
    fn ownership_survives_generic_commits_and_rejects_conflicts_without_overwrite() {
        let _env = crate::test_support::env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("USERPROFILE", home.path());
        let (id, _) = owned_fixture();
        let proofs = store::load_ownership().unwrap();
        let state = store::load_state().unwrap();
        store::commit_state(&state).unwrap();
        assert_eq!(store::load_ownership().unwrap(), proofs);
        let path = ccync_foundation::paths::tool_state_path().unwrap();
        let before = fs::read(&path).unwrap();
        let mut proof = proofs[&id][&1].clone();
        proof.operation_id = "conflicting-operation".into();
        assert!(store::commit_generation(&state, proof).is_err());
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn graphify_untrack_excludes_the_tool_from_actual_canonical_render() {
        let _env = crate::test_support::env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("USERPROFILE", home.path());
        let guard = LifecycleLock::acquire().unwrap();
        let (id, generation_one) = owned_fixture();
        let path = generation_one.parent().unwrap().join("2");
        fs::create_dir_all(path.join(".agents/skills/graphify")).unwrap();
        fs::write(
            path.join(".agents/skills/graphify/SKILL.md"),
            b"# Graphify\n",
        )
        .unwrap();
        let manifest = artifact_tree::scan_tree(&path).unwrap();
        let proof = store::GenerationOwnership {
            tool_id: id.clone(),
            generation: 2,
            operation_id: "second-operation".into(),
            snapshot_relative_path: store::expected_snapshot_relative_path(&id, 2).unwrap(),
            manifest_digest: format!("sha256:{}", manifest.sha256),
            directories: store::directory_inventory(&path).unwrap(),
            manifest,
        };
        store::commit_generation(
            &[(
                id.clone(),
                test_record(
                    Some(2),
                    Some(proof.manifest_digest.clone()),
                    Some(proof.operation_id.clone()),
                ),
            )]
            .into(),
            proof,
        )
        .unwrap();
        let before = tempfile::tempdir().unwrap();
        super::super::render::materialize_tool_snapshots(before.path()).unwrap();
        assert!(before.path().join("skills/graphify/SKILL.md").exists());
        let during = tempfile::tempdir().unwrap();
        untrack_tool(&guard, id, || {
            super::super::render::materialize_tool_snapshots(during.path()).unwrap();
            assert!(!during.path().join("skills/graphify").exists());
            make_report(ProjectionTargetStatus::Applied)
        })
        .unwrap();
        let after = tempfile::tempdir().unwrap();
        super::super::render::materialize_tool_snapshots(after.path()).unwrap();
        assert!(!after.path().join("skills/graphify").exists());
        assert!(!generation_one.parent().unwrap().exists());
    }

    fn make_report(status: ProjectionTargetStatus) -> ProjectionRunReport {
        let mut targets = BTreeMap::new();
        targets.insert(
            ProjectionTargetId::Claude,
            ProjectionTargetResult {
                target: ProjectionTargetId::Claude,
                status,
                paths: Vec::new(),
                warnings: Vec::new(),
            },
        );
        ProjectionRunReport {
            common: Vec::new(),
            targets,
            mode: "normal".into(),
            run_warnings: Vec::new(),
        }
    }

    fn test_facets() -> ToolFacets {
        ToolFacets {
            evidence_level: EvidenceLevel::Verified,
            selection_state: SelectionState::Unique,
            manager_availability: ManagerAvailability::Available,
            install_state: InstallState::Installed,
            package_state: PackageState::MatchesArtifactSource,
            update_state: UpdateState::Current,
            artifact_state: ArtifactState::Current,
            projection_state: ProjectionState::Current,
            hold_policy: HoldPolicy::Unheld,
            manager_pin_state: ManagerPinState::Unpinned,
            operation_state: OperationState::None,
            last_check: None,
        }
    }

    fn test_record(
        generation: Option<u64>,
        fingerprint: Option<String>,
        operation_id: Option<String>,
    ) -> store::StateRecord {
        store::StateRecord {
            committed_generation: generation,
            artifact_fingerprint: fingerprint,
            source_evidence: serde_json::json!({}),
            operation_id,
            facets: test_facets(),
        }
    }

    #[test]
    fn observe_only_without_state_skips_projection_and_removes_declaration() {
        let _env = crate::test_support::env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("USERPROFILE", home.path());
        let guard = LifecycleLock::acquire().unwrap();
        let id = ToolId::new("manual-tool").unwrap();
        let declaration = ToolDeclaration {
            id: id.clone(),
            identity: InstallationIdentity {
                adapter_kind: AdapterKind::ManualPath,
                fields: BTreeMap::new(),
            },
            delivery_id: DeliveryId::ObserveOnly,
            hold_policy: HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: Some(serde_json::json!({})),
            manager_takeover: None,
        };
        store::insert_declaration(declaration).unwrap();
        untrack_tool(&guard, id.clone(), || {
            panic!("observe-only untrack must not project")
        })
        .unwrap();
        assert!(!store::load_declarations().unwrap().contains_key(&id));
        assert!(!store::load_state().unwrap().contains_key(&id));
    }

    #[test]
    fn never_refreshed_tool_skips_projection_and_removes_declaration() {
        let _env = crate::test_support::env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("USERPROFILE", home.path());
        let guard = LifecycleLock::acquire().unwrap();
        let id = ToolId::new("graphify").unwrap();
        let declaration = ToolDeclaration {
            id: id.clone(),
            identity: InstallationIdentity {
                adapter_kind: AdapterKind::InterpreterPip,
                fields: BTreeMap::new(),
            },
            delivery_id: DeliveryId::GraphifyAgents,
            hold_policy: HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        };
        store::insert_declaration(declaration).unwrap();
        untrack_tool(&guard, id.clone(), || {
            panic!("never-refreshed tool untrack must not project")
        })
        .unwrap();
        assert!(!store::load_declarations().unwrap().contains_key(&id));
        assert!(!store::load_state().unwrap().contains_key(&id));
    }

    #[test]
    fn successful_untrack_for_committed_mode_c_removes_snapshot_state_and_declaration() {
        let _env = crate::test_support::env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("USERPROFILE", home.path());
        let guard = LifecycleLock::acquire().unwrap();
        let id = ToolId::new("graphify").unwrap();
        let declaration = ToolDeclaration {
            id: id.clone(),
            identity: InstallationIdentity {
                adapter_kind: AdapterKind::InterpreterPip,
                fields: BTreeMap::new(),
            },
            delivery_id: DeliveryId::GraphifyAgents,
            hold_policy: HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        };
        store::insert_declaration(declaration).unwrap();

        let artifacts_root = ccync_foundation::paths::tool_artifacts_root().unwrap();
        let gen_dir = artifacts_root.join(id.as_str()).join("1");
        fs::create_dir_all(&gen_dir).unwrap();
        fs::write(gen_dir.join("skill.md"), b"skill-content").unwrap();
        let manifest = artifact_tree::scan_tree(&gen_dir).unwrap();
        let digest = format!("sha256:{}", manifest.sha256);

        let mut states = BTreeMap::new();
        states.insert(
            id.clone(),
            test_record(Some(1), Some(digest), Some("op-123".into())),
        );
        store::commit_state(&states).unwrap();

        let mut project_calls = 0;
        let mut stages_during_projection = Vec::new();
        untrack_tool(&guard, id.clone(), || {
            project_calls += 1;
            if let Some(journal) = read_journal().unwrap() {
                stages_during_projection.push((
                    journal.kind,
                    journal.stage,
                    journal.pending_stages,
                ));
            }
            make_report(ProjectionTargetStatus::Applied)
        })
        .unwrap();

        assert_eq!(project_calls, 1);
        assert_eq!(stages_during_projection.len(), 1);
        let (kind, stage, pending_stages) = &stages_during_projection[0];
        assert_eq!(*kind, OperationKind::Untrack);
        assert_eq!(*stage, OperationStage::Prepared);
        assert_eq!(
            pending_stages,
            &[
                OperationStage::Prepared,
                OperationStage::ProjectionSucceeded,
                OperationStage::Committed
            ]
        );

        assert!(!artifacts_root.join(id.as_str()).exists());
        assert!(!store::load_declarations().unwrap().contains_key(&id));
        assert!(!store::load_state().unwrap().contains_key(&id));
        assert!(!ccync_foundation::paths::tool_operation_journal_path()
            .unwrap()
            .exists());
    }

    #[test]
    fn unknown_children_in_artifacts_fail_closed_and_keep_declaration_and_content() {
        let _env = crate::test_support::env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("USERPROFILE", home.path());
        let guard = LifecycleLock::acquire().unwrap();
        let id = ToolId::new("graphify").unwrap();
        let declaration = ToolDeclaration {
            id: id.clone(),
            identity: InstallationIdentity {
                adapter_kind: AdapterKind::InterpreterPip,
                fields: BTreeMap::new(),
            },
            delivery_id: DeliveryId::GraphifyAgents,
            hold_policy: HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        };
        store::insert_declaration(declaration).unwrap();

        let artifacts_root = ccync_foundation::paths::tool_artifacts_root().unwrap();
        let gen_dir = artifacts_root.join(id.as_str()).join("1");
        fs::create_dir_all(&gen_dir).unwrap();
        fs::write(gen_dir.join("skill.md"), b"skill-content").unwrap();
        let manifest = artifact_tree::scan_tree(&gen_dir).unwrap();
        let digest = format!("sha256:{}", manifest.sha256);

        let mut states = BTreeMap::new();
        states.insert(
            id.clone(),
            test_record(Some(1), Some(digest), Some("op-123".into())),
        );
        store::commit_state(&states).unwrap();

        let rogue_path = artifacts_root.join(id.as_str()).join("rogue.txt");
        fs::write(&rogue_path, b"intruder").unwrap();

        let err = untrack_tool(&guard, id.clone(), || {
            panic!("untrack must fail before projection")
        })
        .unwrap_err();

        assert!(matches!(err, UntrackError::Ownership));
        assert!(rogue_path.exists());
        assert!(gen_dir.join("skill.md").exists());
        assert!(store::load_declarations().unwrap().contains_key(&id));
        assert!(store::load_state().unwrap().contains_key(&id));
        assert!(!ccync_foundation::paths::tool_operation_journal_path()
            .unwrap()
            .exists());
    }

    #[test]
    fn unmatched_generation_fails_closed_and_keeps_declaration_and_content() {
        let _env = crate::test_support::env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("USERPROFILE", home.path());
        let guard = LifecycleLock::acquire().unwrap();
        let id = ToolId::new("graphify").unwrap();
        let declaration = ToolDeclaration {
            id: id.clone(),
            identity: InstallationIdentity {
                adapter_kind: AdapterKind::InterpreterPip,
                fields: BTreeMap::new(),
            },
            delivery_id: DeliveryId::GraphifyAgents,
            hold_policy: HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        };
        store::insert_declaration(declaration).unwrap();

        let artifacts_root = ccync_foundation::paths::tool_artifacts_root().unwrap();
        let gen1_dir = artifacts_root.join(id.as_str()).join("1");
        fs::create_dir_all(&gen1_dir).unwrap();
        fs::write(gen1_dir.join("skill.md"), b"skill-content").unwrap();
        let manifest = artifact_tree::scan_tree(&gen1_dir).unwrap();
        let digest = format!("sha256:{}", manifest.sha256);

        let gen2_dir = artifacts_root.join(id.as_str()).join("2");
        fs::create_dir_all(&gen2_dir).unwrap();
        fs::write(gen2_dir.join("skill.md"), b"unowned-content").unwrap();

        let mut states = BTreeMap::new();
        states.insert(
            id.clone(),
            test_record(Some(1), Some(digest), Some("op-123".into())),
        );
        store::commit_state(&states).unwrap();

        let err = untrack_tool(&guard, id.clone(), || {
            panic!("untrack must fail before projection")
        })
        .unwrap_err();

        assert!(matches!(err, UntrackError::Ownership));
        assert!(gen1_dir.join("skill.md").exists());
        assert!(gen2_dir.join("skill.md").exists());
        assert!(store::load_declarations().unwrap().contains_key(&id));
        assert!(store::load_state().unwrap().contains_key(&id));
        assert!(!ccync_foundation::paths::tool_operation_journal_path()
            .unwrap()
            .exists());
    }

    #[test]
    fn projection_failure_keeps_declaration_and_content() {
        let _env = crate::test_support::env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("USERPROFILE", home.path());
        let guard = LifecycleLock::acquire().unwrap();
        let id = ToolId::new("graphify").unwrap();
        let declaration = ToolDeclaration {
            id: id.clone(),
            identity: InstallationIdentity {
                adapter_kind: AdapterKind::InterpreterPip,
                fields: BTreeMap::new(),
            },
            delivery_id: DeliveryId::GraphifyAgents,
            hold_policy: HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        };
        store::insert_declaration(declaration).unwrap();

        let artifacts_root = ccync_foundation::paths::tool_artifacts_root().unwrap();
        let gen_dir = artifacts_root.join(id.as_str()).join("1");
        fs::create_dir_all(&gen_dir).unwrap();
        fs::write(gen_dir.join("skill.md"), b"skill-content").unwrap();
        let manifest = artifact_tree::scan_tree(&gen_dir).unwrap();
        let digest = format!("sha256:{}", manifest.sha256);

        let mut states = BTreeMap::new();
        states.insert(
            id.clone(),
            test_record(Some(1), Some(digest), Some("op-123".into())),
        );
        store::commit_state(&states).unwrap();

        let err = untrack_tool(&guard, id.clone(), || {
            make_report(ProjectionTargetStatus::Failed {
                path: Some(std::path::PathBuf::from("test")),
                reason: "projection failed".into(),
            })
        })
        .unwrap_err();

        assert!(matches!(err, UntrackError::Projection));
        assert!(gen_dir.join("skill.md").exists());
        assert!(store::load_declarations().unwrap().contains_key(&id));
        assert!(store::load_state().unwrap().contains_key(&id));
    }
}
