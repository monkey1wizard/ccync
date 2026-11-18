//! Recovery decisions for interrupted tool lifecycle operations.
use super::{
    journal::{OperationJournal, OperationStage, RetryIntent},
    lock::LifecycleGuard,
    store,
};
use std::fs;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryRequired {
    pub failed_stage: OperationStage,
    pub irreversible_changes: Vec<super::journal::IrreversibleChange>,
    pub pending_stages: Vec<OperationStage>,
    pub journal_path: String,
    pub retry_command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryDecision {
    None,
    Resume,
    CommittedCleanup,
    RecoveryRequired(RecoveryRequired),
}

#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("operation journal path is unavailable")]
    PathUnavailable,
    #[error("operation journal could not be read: {0}")]
    Io(#[from] std::io::Error),
    #[error("operation journal is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("tool state could not be read: {0}")]
    Store(#[from] store::StoreError),
}

pub fn recover(
    _guard: &LifecycleGuard,
    retry_intent: &RetryIntent,
) -> Result<RecoveryDecision, RecoveryError> {
    let paths = store::ToolPaths::load()?;
    let path = paths.journal.clone();
    if !path.exists() {
        return Ok(RecoveryDecision::None);
    }
    let journal: OperationJournal = serde_json::from_slice(&fs::read(&path)?)?;
    journal.validate_identity().map_err(|e| {
        RecoveryError::Json(serde_json::Error::io(std::io::Error::other(e.to_string())))
    })?;
    let states = store::load_state()?;
    if journal.kind != super::journal::OperationKind::Untrack
        && store::committed_operation(&journal)?.is_some()
        && matches!(
            journal.stage,
            OperationStage::ProjectionSucceeded | OperationStage::Committed
        )
    {
        fs::remove_file(path)?;
        return Ok(RecoveryDecision::CommittedCleanup);
    }
    if journal.kind != super::journal::OperationKind::Untrack
        && states
            .get(&journal.tool_id)
            .and_then(|state| state.operation_id.as_deref())
            == Some(journal.operation_id.as_str())
        && journal.pending_generation.is_none()
        && journal
            .pending_snapshot_relative_path
            .as_os_str()
            .is_empty()
        && journal.pending_manifest_digest.is_empty()
    {
        fs::remove_file(path)?;
        return Ok(RecoveryDecision::CommittedCleanup);
    }
    if journal.stage == OperationStage::Committed {
        let committed = if journal.kind == super::journal::OperationKind::Untrack {
            !states.contains_key(&journal.tool_id)
                && !store::load_declarations()?.contains_key(&journal.tool_id)
                && !store::load_ownership()?.contains_key(&journal.tool_id)
                && !paths.artifacts.join(journal.tool_id.as_str()).exists()
        } else {
            false
        };
        if committed {
            fs::remove_file(path)?;
            return Ok(RecoveryDecision::CommittedCleanup);
        }
    }
    if retry_intent == &journal.retry_intent {
        return Ok(RecoveryDecision::Resume);
    }
    let retry_command = journal.retry_command();
    Ok(RecoveryDecision::RecoveryRequired(RecoveryRequired {
        failed_stage: journal.stage,
        irreversible_changes: journal.irreversible_changes,
        pending_stages: journal.pending_stages,
        journal_path: path.display().to_string(),
        retry_command,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_manager::{
        journal::{IrreversibleChange, OperationKind},
        lock::LifecycleLock,
        model::{
            ArtifactState, EvidenceLevel, HoldPolicy, InstallState, ManagerAvailability,
            ManagerPinState, OperationState, PackageState, ProjectionState, SelectionState,
            ToolFacets, ToolId, UpdateState,
        },
        store::{self, StateRecord},
    };
    use std::{collections::BTreeMap, path::PathBuf};

    fn test_record(op_id: Option<String>) -> StateRecord {
        StateRecord {
            committed_generation: Some(1),
            artifact_fingerprint: Some("sha256:abc".into()),
            source_evidence: serde_json::json!({}),
            operation_id: op_id,
            facets: ToolFacets {
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
                operation_state: OperationState::Committed,
                last_check: None,
            },
        }
    }

    #[test]
    fn recover_without_journal_returns_none() {
        let _home = crate::test_support::TestHome::new();
        let guard = LifecycleLock::acquire().unwrap();
        let path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        let _ = fs::remove_file(path);
        assert_eq!(
            recover(&guard, &RetryIntent::Refresh).unwrap(),
            RecoveryDecision::None
        );
    }

    #[test]
    fn recover_committed_id_cleanup_removes_journal_with_zero_recipe_or_projection() {
        let _home = crate::test_support::TestHome::new();
        let guard = LifecycleLock::acquire().unwrap();

        let tool_id = ToolId::new("rtk").unwrap();
        let op_id = "op-committed-abc-123";
        let mut states = BTreeMap::new();
        states.insert(tool_id.clone(), test_record(Some(op_id.to_string())));
        store::commit_state(&states).unwrap();

        let mut journal = OperationJournal::new(
            tool_id,
            OperationKind::Refresh,
            RetryIntent::Refresh,
            None,
            PathBuf::new(),
            String::new(),
        );
        journal.operation_id = op_id.to_string();

        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        fs::create_dir_all(journal_path.parent().unwrap()).unwrap();
        fs::write(&journal_path, serde_json::to_vec_pretty(&journal).unwrap()).unwrap();
        assert!(journal_path.exists());

        let decision = recover(&guard, &RetryIntent::Refresh).unwrap();
        assert_eq!(decision, RecoveryDecision::CommittedCleanup);
        assert!(!journal_path.exists());
    }

    #[test]
    fn recover_resume_when_requested_intent_matches_journal_intent() {
        let _home = crate::test_support::TestHome::new();
        let guard = LifecycleLock::acquire().unwrap();

        let tool_id = ToolId::new("rtk").unwrap();
        let mut states = BTreeMap::new();
        states.insert(tool_id.clone(), test_record(Some("older-op".into())));
        store::commit_state(&states).unwrap();

        let journal = OperationJournal::new(
            tool_id,
            OperationKind::Refresh,
            RetryIntent::Refresh,
            None,
            PathBuf::new(),
            String::new(),
        );

        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        fs::create_dir_all(journal_path.parent().unwrap()).unwrap();
        fs::write(&journal_path, serde_json::to_vec_pretty(&journal).unwrap()).unwrap();
        assert!(journal_path.exists());

        let decision = recover(&guard, &RetryIntent::Refresh).unwrap();
        assert_eq!(decision, RecoveryDecision::Resume);
        assert!(journal_path.exists());
    }

    #[test]
    fn recover_refusal_for_different_typed_intent_and_retry_text() {
        let _home = crate::test_support::TestHome::new();
        let guard = LifecycleLock::acquire().unwrap();

        let tool_id = ToolId::new("graphify").unwrap();
        let mut journal = OperationJournal::new(
            tool_id.clone(),
            OperationKind::Upgrade,
            RetryIntent::Upgrade {
                tool_id: tool_id.clone(),
            },
            Some(2),
            PathBuf::from("tools/artifacts/graphify/2"),
            "sha256:digest123".into(),
        );
        journal.stage = OperationStage::PackageVerified;
        journal.irreversible_changes = vec![IrreversibleChange::PackageInstalled];

        let journal_path = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        fs::create_dir_all(journal_path.parent().unwrap()).unwrap();
        fs::write(&journal_path, serde_json::to_vec_pretty(&journal).unwrap()).unwrap();

        let decision = recover(&guard, &RetryIntent::Refresh).unwrap();
        match decision {
            RecoveryDecision::RecoveryRequired(req) => {
                assert_eq!(req.failed_stage, OperationStage::PackageVerified);
                assert_eq!(
                    req.irreversible_changes,
                    vec![IrreversibleChange::PackageInstalled]
                );
                assert_eq!(
                    req.pending_stages,
                    vec![
                        OperationStage::Prepared,
                        OperationStage::PackageVerified,
                        OperationStage::ArtifactValidated,
                        OperationStage::ProjectionSucceeded,
                        OperationStage::Committed,
                    ]
                );
                assert_eq!(req.journal_path, journal_path.display().to_string());
                assert_eq!(req.retry_command, "ccync upgrade tool:graphify");
            }
            other => panic!("expected RecoveryRequired, got {other:?}"),
        }
        assert!(journal_path.exists());

        // Also verify Untrack retry text
        let untrack_tool = ToolId::new("tool-b").unwrap();
        let untrack_journal = OperationJournal::new(
            untrack_tool.clone(),
            OperationKind::Untrack,
            RetryIntent::Untrack {
                tool_id: untrack_tool,
            },
            None,
            PathBuf::new(),
            String::new(),
        );
        fs::write(
            &journal_path,
            serde_json::to_vec_pretty(&untrack_journal).unwrap(),
        )
        .unwrap();

        let decision2 = recover(&guard, &RetryIntent::Refresh).unwrap();
        match decision2 {
            RecoveryDecision::RecoveryRequired(req) => {
                assert_eq!(req.retry_command, "ccync tool untrack tool-b");
            }
            other => panic!("expected RecoveryRequired, got {other:?}"),
        }
    }
}
