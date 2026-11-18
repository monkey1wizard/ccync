use super::{lock::LifecycleGuard, model::ToolId};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

const VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperationKind {
    Refresh,
    Upgrade,
    Untrack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperationStage {
    Prepared,
    PackageVerified,
    ArtifactValidated,
    ProjectionSucceeded,
    Committed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManagerStepState {
    NotStarted,
    ManagerOutcomeUnknown,
    Succeeded,
    Failed,
}

/// Safe command recipes contain only an operation and validated tool identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "command",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum RetryIntent {
    Refresh,
    Upgrade { tool_id: ToolId },
    Untrack { tool_id: ToolId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IrreversibleChange {
    PackageInstalled,
    PackageRemoved,
    ArtifactReplaced,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationJournal {
    pub version: u32,
    #[serde(rename = "operationId")]
    pub operation_id: String,
    pub tool_id: ToolId,
    pub kind: OperationKind,
    pub retry_intent: RetryIntent,
    pub stage: OperationStage,
    pub manager_step_state: ManagerStepState,
    pub pending_generation: Option<u64>,
    pub pending_snapshot_relative_path: PathBuf,
    pub pending_manifest_digest: String,
    pub irreversible_changes: Vec<IrreversibleChange>,
    pub pending_stages: Vec<OperationStage>,
    pub started_at: String,
    #[serde(default)]
    pub cleanup_plan: Vec<CleanupGeneration>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupGeneration {
    pub generation: u64,
    #[serde(default)]
    pub operation_id: String,
    pub snapshot_relative_path: PathBuf,
    pub manifest_digest: String,
    pub manifest: crate::tool_manager::artifact_tree::TreeManifest,
    pub directories: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("operation journal path is unavailable")]
    PathUnavailable,
    #[error("an operation journal already exists; recovery is required")]
    RecoveryRequired,
    #[error("operation journal I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("operation journal serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("operation journal can only be removed after commit")]
    NotCommitted,
    #[error("operation journal stage cannot advance from {0:?}")]
    InvalidAdvance(OperationStage),
    #[error("operation journal metadata is invalid")]
    InvalidMetadata,
}

fn journal_path() -> Result<PathBuf, JournalError> {
    super::store::ToolPaths::load()
        .map(|paths| paths.journal)
        .map_err(|error| match error {
            super::store::StoreError::PathUnavailable => JournalError::PathUnavailable,
            super::store::StoreError::Io(error) => JournalError::Io(error),
            other => JournalError::Io(std::io::Error::other(other.to_string())),
        })
}

impl OperationJournal {
    pub fn new(
        tool_id: ToolId,
        kind: OperationKind,
        retry_intent: RetryIntent,
        pending_generation: Option<u64>,
        pending_snapshot_relative_path: PathBuf,
        pending_manifest_digest: String,
    ) -> Self {
        let pending_stages = stages(kind).to_vec();
        Self {
            version: VERSION,
            operation_id: uuid::Uuid::new_v4().to_string(),
            tool_id,
            kind,
            retry_intent,
            stage: OperationStage::Prepared,
            manager_step_state: ManagerStepState::NotStarted,
            pending_generation,
            pending_snapshot_relative_path,
            pending_manifest_digest,
            irreversible_changes: Vec::new(),
            pending_stages,
            started_at: chrono::Utc::now().to_rfc3339(),
            cleanup_plan: Vec::new(),
        }
    }

    pub fn create(&self, _guard: &LifecycleGuard) -> Result<(), JournalError> {
        let path = journal_path()?;
        if path.exists() {
            return Err(JournalError::RecoveryRequired);
        }
        write_atomic(path, self)
    }

    pub fn advance(
        &mut self,
        _guard: &LifecycleGuard,
        next: OperationStage,
    ) -> Result<(), JournalError> {
        let list = stages(self.kind);
        let index = list
            .iter()
            .position(|stage| *stage == self.stage)
            .ok_or(JournalError::InvalidAdvance(self.stage))?;
        if list.get(index + 1).copied() != Some(next) {
            return Err(JournalError::InvalidAdvance(self.stage));
        }
        let previous = self.stage;
        self.stage = next;
        if let Err(error) = write_atomic(journal_path()?, self) {
            self.stage = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn remove(&self, _guard: &LifecycleGuard) -> Result<(), JournalError> {
        if self.stage != OperationStage::Committed {
            return Err(JournalError::NotCommitted);
        }
        match fs::remove_file(journal_path()?) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn retry_command(&self) -> String {
        match &self.retry_intent {
            RetryIntent::Refresh => format!("ccync tool refresh {}", self.tool_id.as_str()),
            RetryIntent::Upgrade { tool_id } => {
                format!("ccync upgrade tool:{}", tool_id.as_str())
            }
            RetryIntent::Untrack { tool_id } => {
                format!("ccync tool untrack {}", tool_id.as_str())
            }
        }
    }

    pub fn validate_identity(&self) -> Result<(), JournalError> {
        let intent_matches = match &self.retry_intent {
            RetryIntent::Refresh => self.kind == OperationKind::Refresh,
            RetryIntent::Upgrade { tool_id } | RetryIntent::Untrack { tool_id } => {
                tool_id == &self.tool_id
                    && matches!(
                        (&self.kind, &self.retry_intent),
                        (OperationKind::Upgrade, RetryIntent::Upgrade { .. })
                            | (OperationKind::Untrack, RetryIntent::Untrack { .. })
                    )
            }
        };
        if self.version != VERSION
            || self.operation_id.trim().is_empty()
            || self.started_at.trim().is_empty()
            || self.pending_stages != stages(self.kind)
            || !intent_matches
            || !stages(self.kind).contains(&self.stage)
            || (self.kind != OperationKind::Untrack && !self.cleanup_plan.is_empty())
        {
            return Err(JournalError::InvalidMetadata);
        }
        let mut seen = std::collections::BTreeSet::new();
        for plan in &self.cleanup_plan {
            if plan.generation == 0
                || plan.operation_id.trim().is_empty()
                || !seen.insert(plan.generation)
                || plan.snapshot_relative_path
                    != super::store::expected_snapshot_relative_path(&self.tool_id, plan.generation)
                        .map_err(|_| JournalError::InvalidMetadata)?
            {
                return Err(JournalError::InvalidMetadata);
            }
            super::store::validate_manifest(
                &plan.manifest,
                &plan.manifest_digest,
                &plan.directories,
            )
            .map_err(|_| JournalError::InvalidMetadata)?;
        }
        if self.kind == OperationKind::Untrack
            && (self.pending_generation.is_some()
                || !self.pending_snapshot_relative_path.as_os_str().is_empty()
                || !self.pending_manifest_digest.is_empty()
                || self.manager_step_state != ManagerStepState::NotStarted)
        {
            return Err(JournalError::InvalidMetadata);
        }
        Ok(())
    }
}

fn stages(kind: OperationKind) -> &'static [OperationStage] {
    use OperationStage::*;
    match kind {
        OperationKind::Refresh | OperationKind::Upgrade => &[
            Prepared,
            PackageVerified,
            ArtifactValidated,
            ProjectionSucceeded,
            Committed,
        ],
        OperationKind::Untrack => &[Prepared, ProjectionSucceeded, Committed],
    }
}

fn write_atomic(path: PathBuf, journal: &OperationJournal) -> Result<(), JournalError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(journal)?;
    ccync_foundation::platform::atomic_write_bytes(&path, &bytes)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_manager::lock::LifecycleLock;
    use std::fs;

    fn setup() -> (crate::test_support::TestHome, LifecycleGuard) {
        let home = crate::test_support::TestHome::new();
        let guard = LifecycleLock::acquire().unwrap();
        (home, guard)
    }
    fn journal(kind: OperationKind) -> OperationJournal {
        let id = ToolId::new("graphify").unwrap();
        let intent = match kind {
            OperationKind::Refresh => RetryIntent::Refresh,
            OperationKind::Upgrade => RetryIntent::Upgrade {
                tool_id: id.clone(),
            },
            OperationKind::Untrack => RetryIntent::Untrack {
                tool_id: id.clone(),
            },
        };
        OperationJournal::new(
            id,
            kind,
            intent,
            Some(7),
            PathBuf::from("tools/snapshot.json"),
            "sha256:abc".into(),
        )
    }

    #[test]
    fn create_refuses_existing_and_advance_is_forward_only() {
        let (_home, guard) = setup();
        let mut value = journal(OperationKind::Refresh);
        value.create(&guard).unwrap();
        assert!(matches!(
            value.create(&guard),
            Err(JournalError::RecoveryRequired)
        ));
        let path = journal_path().unwrap();
        let before = fs::read(&path).unwrap();
        assert!(value.advance(&guard, OperationStage::Prepared).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        value
            .advance(&guard, OperationStage::PackageVerified)
            .unwrap();
        assert!(matches!(
            value.remove(&guard),
            Err(JournalError::NotCommitted)
        ));
        value
            .advance(&guard, OperationStage::ArtifactValidated)
            .unwrap();
        value
            .advance(&guard, OperationStage::ProjectionSucceeded)
            .unwrap();
        value.advance(&guard, OperationStage::Committed).unwrap();
        assert!(path.exists());
        value.remove(&guard).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn journal_has_expected_untrack_stages_and_pending_ownership() {
        let value = journal(OperationKind::Untrack);
        assert_eq!(
            value.pending_stages,
            vec![
                OperationStage::Prepared,
                OperationStage::ProjectionSucceeded,
                OperationStage::Committed
            ]
        );
        assert_eq!(value.pending_generation, Some(7));
        assert_eq!(
            value.pending_snapshot_relative_path,
            PathBuf::from("tools/snapshot.json")
        );
        assert_eq!(value.pending_manifest_digest, "sha256:abc");
    }

    #[test]
    fn retry_command_uses_each_intents_cli_command() {
        let refresh = journal(OperationKind::Refresh);
        assert_eq!(refresh.retry_command(), "ccync tool refresh graphify");

        let upgrade = journal(OperationKind::Upgrade);
        assert_eq!(upgrade.retry_command(), "ccync upgrade tool:graphify");

        let untrack = journal(OperationKind::Untrack);
        assert_eq!(untrack.retry_command(), "ccync tool untrack graphify");
    }

    #[test]
    fn command_reference_documents_refresh_and_tool_upgrade() {
        let docs = include_str!("../../../../docs/commands.md");
        assert!(docs.contains("ccync tool refresh"));
        assert!(docs.contains("ccync upgrade tool:"));
    }

    #[test]
    fn serialized_journal_is_safe() {
        let mut value = journal(OperationKind::Upgrade);
        value.manager_step_state = ManagerStepState::ManagerOutcomeUnknown;
        let bytes = serde_json::to_vec(&value).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        for forbidden in [
            "TEST_SECRET_9d7e",
            "--token raw-argv",
            "https://user:pass@example.test",
        ] {
            assert!(!text.contains(forbidden));
        }
        assert_eq!(value.retry_command(), "ccync upgrade tool:graphify");
        assert!(text.contains("manager-outcome-unknown"));

        let untrack = journal(OperationKind::Untrack);
        assert_eq!(untrack.retry_command(), "ccync tool untrack graphify");
    }
}
