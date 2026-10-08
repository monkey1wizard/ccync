//! Materialization of committed external-tool snapshots into canonical skills.
use super::{
    artifact_tree,
    journal::{OperationJournal, OperationKind},
    model::{DeliveryId, ToolId},
    store,
};
use std::{
    cell::RefCell,
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("tool render failed: {0}")]
    Io(#[from] io::Error),
    #[error("tool state failed: {0}")]
    Store(#[from] super::store::StoreError),
    #[error("tool snapshot is invalid for {0}")]
    Snapshot(String),
    #[error("tool render journal is invalid")]
    Journal,
}

thread_local! { static PENDING: RefCell<Option<(ToolId,u64)>> = const { RefCell::new(None) }; }

/// Restricts one render on this thread to one uncommitted generation.
pub struct PendingGenerationScope {
    previous: Option<(ToolId, u64)>,
}
impl PendingGenerationScope {
    pub fn new(id: ToolId, generation: u64) -> Self {
        let previous = PENDING.with(|scope| scope.replace(Some((id, generation))));
        Self { previous }
    }
}
impl Drop for PendingGenerationScope {
    fn drop(&mut self) {
        let old = self.previous.take();
        PENDING.with(|scope| {
            scope.replace(old);
        });
    }
}

pub fn materialize_tool_snapshots(staging: &Path) -> Result<Vec<String>, RenderError> {
    let declarations = store::load_declarations()?;
    let states = store::load_state()?;
    let journal_path = ccync_foundation::paths::tool_operation_journal_path();
    let active = if let Some(path) = journal_path.filter(|p| p.exists()) {
        let bytes = fs::read(path)?;
        Some(serde_json::from_slice::<OperationJournal>(&bytes).map_err(|_| RenderError::Journal)?)
    } else {
        None
    };
    let mut warnings = Vec::new();
    for (id, declaration) in declarations {
        if declaration.delivery_id != DeliveryId::GraphifyAgents {
            continue;
        }
        if active
            .as_ref()
            .is_some_and(|j| j.tool_id == id && j.kind == OperationKind::Untrack)
        {
            continue;
        }
        let committed = states.get(&id).and_then(|s| s.committed_generation);
        let pending = PENDING.with(|slot| {
            slot.borrow()
                .as_ref()
                .filter(|(pending, _)| pending == &id)
                .map(|(_, g)| *g)
        });
        let generation = pending.or(committed);
        let Some(generation) = generation else {
            continue;
        };
        let source = ccync_foundation::paths::tool_artifacts_root()
            .ok_or_else(|| RenderError::Snapshot(id.to_string()))?
            .join(id.as_str())
            .join(generation.to_string());
        let manifest =
            artifact_tree::scan_tree(&source).map_err(|_| RenderError::Snapshot(id.to_string()))?;
        if manifest
            .files
            .iter()
            .any(|file| !file.relative_path.starts_with(".agents/skills/graphify/"))
            || manifest.files.is_empty()
        {
            return Err(RenderError::Snapshot(id.to_string()));
        }
        if Some(generation) == committed {
            let record = &states[&id];
            if record.artifact_fingerprint.as_deref()
                != Some(format!("sha256:{}", manifest.sha256).as_str())
            {
                return Err(RenderError::Snapshot(id.to_string()));
            }
        } else if pending == Some(generation) {
            let journal = active
                .as_ref()
                .filter(|journal| {
                    journal.tool_id == id
                        && journal.kind != OperationKind::Untrack
                        && journal.pending_generation == Some(generation)
                })
                .ok_or_else(|| RenderError::Snapshot(id.to_string()))?;
            if journal.pending_manifest_digest != format!("sha256:{}", manifest.sha256) {
                return Err(RenderError::Snapshot(id.to_string()));
            }
        }
        let skill_name = id.as_str();
        let dest = staging.join("skills").join(skill_name);
        if dest.exists() {
            warnings.push(format!("tool snapshot '{skill_name}' skipped because a plugin-owned skill has the same name"));
            continue;
        }
        let loose_source = find_loose_source(skill_name);
        if let Some(source_path) = loose_source {
            warnings.push(format!(
                "tracked tool '{skill_name}' takes precedence over loose skill at {}",
                source_path.display()
            ));
        }
        fs::create_dir_all(&dest)?;
        let skill_source = source.join(".agents").join("skills").join("graphify");
        let skill_manifest = artifact_tree::scan_tree(&skill_source)
            .map_err(|_| RenderError::Snapshot(id.to_string()))?;
        artifact_tree::materialize(&skill_manifest, &skill_source, &dest)
            .map_err(|_| RenderError::Snapshot(id.to_string()))?;
    }
    Ok(warnings)
}

fn find_loose_source(name: &str) -> Option<PathBuf> {
    [
        crate::paths::claude_skills_path(),
        crate::paths::shared_agents_skills_path(),
    ]
    .into_iter()
    .flatten()
    .map(|root| root.join(name))
    .find(|p| p.is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_manager::{
        journal::{OperationJournal, OperationKind, OperationStage, RetryIntent},
        model::{
            AdapterKind, ArtifactState, DeliveryId, EvidenceLevel, HoldPolicy,
            InstallationIdentity, InstallState, ManagerAvailability, ManagerPinState,
            OperationState, PackageState, ProjectionState, SelectionState,
            ToolDeclaration, ToolFacets, ToolId, UpdateState,
        },
        store::{self, StateRecord},
    };
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn test_home() -> (TempDir, std::sync::MutexGuard<'static, ()>) {
        let guard = crate::test_support::env_guard();
        let dir = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", dir.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", dir.path());
        }
        (dir, guard)
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
            operation_state: OperationState::Committed,
            last_check: Some("2026-09-28T00:00:00Z".into()),
        }
    }

    fn setup_tool_snapshot(
        id: &str,
        delivery: DeliveryId,
        generation: u64,
        files: &[(&str, &[u8])],
    ) -> (ToolId, String) {
        let tool_id = ToolId::new(id).unwrap();
        let decl = ToolDeclaration {
            id: tool_id.clone(),
            identity: InstallationIdentity {
                adapter_kind: AdapterKind::InterpreterPip,
                fields: BTreeMap::new(),
            },
            delivery_id: delivery,
            hold_policy: HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        };
        let mut decls = store::load_declarations().unwrap_or_default();
        decls.insert(tool_id.clone(), decl);
        store::replace_declarations(&decls).unwrap();

        let artifacts_root = ccync_foundation::paths::tool_artifacts_root().unwrap();
        let snapshot_dir = artifacts_root.join(id).join(generation.to_string());
        for (rel, content) in files {
            let file_path = snapshot_dir.join(rel);
            fs::create_dir_all(file_path.parent().unwrap()).unwrap();
            fs::write(&file_path, content).unwrap();
        }
        let manifest = artifact_tree::scan_tree(&snapshot_dir).unwrap();
        let digest = format!("sha256:{}", manifest.sha256);

        let mut states = store::load_state().unwrap_or_default();
        states.insert(
            tool_id.clone(),
            StateRecord {
                committed_generation: Some(generation),
                artifact_fingerprint: Some(digest.clone()),
                source_evidence: serde_json::json!({}),
                operation_id: Some("op-test".into()),
                facets: test_facets(),
            },
        );
        store::commit_state(&states).unwrap();

        (tool_id, digest)
    }

    #[test]
    fn render_canonical_root_materialization() {
        let (_home, _guard) = test_home();
        let staging = tempfile::tempdir().unwrap();
        let (id, _digest) = setup_tool_snapshot(
            "graphify",
            DeliveryId::GraphifyAgents,
            1,
            &[
                (".agents/skills/graphify/SKILL.md", b"# graphify skill"),
                (".agents/skills/graphify/nested/file.txt", b"nested content"),
            ],
        );

        let warnings = materialize_tool_snapshots(staging.path()).unwrap();
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

        let skill_root = staging.path().join("skills").join(id.as_str());
        assert_eq!(
            fs::read_to_string(skill_root.join("SKILL.md")).unwrap(),
            "# graphify skill"
        );
        assert_eq!(
            fs::read_to_string(skill_root.join("nested/file.txt")).unwrap(),
            "nested content"
        );
    }

    #[test]
    fn render_canonical_root_plugin_precedence_with_warning() {
        let (_home, _guard) = test_home();
        let staging = tempfile::tempdir().unwrap();
        let (id, _digest) = setup_tool_snapshot(
            "graphify",
            DeliveryId::GraphifyAgents,
            1,
            &[(".agents/skills/graphify/SKILL.md", b"# tool skill")],
        );

        let plugin_skill = staging.path().join("skills").join(id.as_str());
        fs::create_dir_all(&plugin_skill).unwrap();
        fs::write(plugin_skill.join("SKILL.md"), b"# plugin wins").unwrap();

        let warnings = materialize_tool_snapshots(staging.path()).unwrap();
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            warnings[0],
            "tool snapshot 'graphify' skipped because a plugin-owned skill has the same name"
        );
        assert_eq!(
            fs::read_to_string(plugin_skill.join("SKILL.md")).unwrap(),
            "# plugin wins"
        );
    }

    #[test]
    fn render_canonical_root_tool_precedence_over_loose_skill_with_warning() {
        let (_home, _guard) = test_home();
        let staging = tempfile::tempdir().unwrap();
        let (id, _digest) = setup_tool_snapshot(
            "graphify",
            DeliveryId::GraphifyAgents,
            1,
            &[(".agents/skills/graphify/SKILL.md", b"# tool wins")],
        );

        let claude_skills = crate::paths::claude_skills_path().unwrap();
        let loose_skill_dir = claude_skills.join(id.as_str());
        fs::create_dir_all(&loose_skill_dir).unwrap();
        fs::write(loose_skill_dir.join("SKILL.md"), b"# loose skill content").unwrap();

        let warnings = materialize_tool_snapshots(staging.path()).unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("tracked tool 'graphify' takes precedence over loose skill at"),
            "warning was: {}",
            warnings[0]
        );
        assert!(
            warnings[0].contains(&loose_skill_dir.display().to_string()),
            "warning should name loose skill path: {}",
            warnings[0]
        );

        let materialized = staging.path().join("skills").join(id.as_str());
        assert_eq!(
            fs::read_to_string(materialized.join("SKILL.md")).unwrap(),
            "# tool wins"
        );

        assert_eq!(
            fs::read_to_string(loose_skill_dir.join("SKILL.md")).unwrap(),
            "# loose skill content"
        );
    }

    #[test]
    fn render_canonical_root_skip_during_untrack() {
        let (_home, _guard) = test_home();
        let staging = tempfile::tempdir().unwrap();
        let (id, _digest) = setup_tool_snapshot(
            "graphify",
            DeliveryId::GraphifyAgents,
            1,
            &[(".agents/skills/graphify/SKILL.md", b"# content")],
        );

        let journal = OperationJournal::new(
            id.clone(),
            OperationKind::Untrack,
            RetryIntent::Untrack { tool_id: id.clone() },
            None,
            PathBuf::new(),
            String::new(),
        );
        let jpath = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        fs::create_dir_all(jpath.parent().unwrap()).unwrap();
        fs::write(&jpath, serde_json::to_vec(&journal).unwrap()).unwrap();

        let warnings = materialize_tool_snapshots(staging.path()).unwrap();
        assert!(warnings.is_empty());
        assert!(!staging.path().join("skills").join(id.as_str()).exists());
    }

    #[test]
    fn render_canonical_root_live_root_preservation_on_manifest_mismatch() {
        let (_home, _guard) = test_home();
        let staging = tempfile::tempdir().unwrap();
        let (id, _digest) = setup_tool_snapshot(
            "graphify",
            DeliveryId::GraphifyAgents,
            1,
            &[(".agents/skills/graphify/SKILL.md", b"# content")],
        );

        let mut states = store::load_state().unwrap();
        states.get_mut(&id).unwrap().artifact_fingerprint =
            Some("sha256:0000000000000000000000000000000000000000000000000000000000000000".into());
        store::commit_state(&states).unwrap();

        let live_root = tempfile::tempdir().unwrap();
        let live_skill = live_root.path().join("skills").join("original");
        fs::create_dir_all(&live_skill).unwrap();
        fs::write(live_skill.join("SKILL.md"), b"# original live skill").unwrap();

        let err = materialize_tool_snapshots(staging.path()).unwrap_err();
        assert!(matches!(err, RenderError::Snapshot(_)));

        assert_eq!(
            fs::read_to_string(live_skill.join("SKILL.md")).unwrap(),
            "# original live skill"
        );
    }

    #[test]
    fn render_canonical_root_extra_created_path_fails() {
        let (_home, _guard) = test_home();
        let staging = tempfile::tempdir().unwrap();
        let (_id, _digest) = setup_tool_snapshot(
            "graphify",
            DeliveryId::GraphifyAgents,
            1,
            &[
                (".agents/skills/graphify/SKILL.md", b"# content"),
                ("unexpected_extra_file.txt", b"extra path"),
            ],
        );

        let err = materialize_tool_snapshots(staging.path()).unwrap_err();
        assert!(matches!(err, RenderError::Snapshot(_)));
    }

    #[test]
    fn render_canonical_root_crash_at_artifact_validated_and_pending_generation_scope() {
        let (_home, _guard) = test_home();
        let staging = tempfile::tempdir().unwrap();

        let (id, _digest1) = setup_tool_snapshot(
            "graphify",
            DeliveryId::GraphifyAgents,
            1,
            &[(".agents/skills/graphify/SKILL.md", b"# generation 1")],
        );

        let artifacts_root = ccync_foundation::paths::tool_artifacts_root().unwrap();
        let gen2_dir = artifacts_root.join(id.as_str()).join("2");
        fs::create_dir_all(gen2_dir.join(".agents/skills/graphify")).unwrap();
        fs::write(
            gen2_dir.join(".agents/skills/graphify/SKILL.md"),
            b"# generation 2",
        )
        .unwrap();
        let manifest2 = artifact_tree::scan_tree(&gen2_dir).unwrap();
        let digest2 = format!("sha256:{}", manifest2.sha256);

        let mut journal = OperationJournal::new(
            id.clone(),
            OperationKind::Refresh,
            RetryIntent::Refresh,
            Some(2),
            PathBuf::from("graphify/2"),
            digest2,
        );
        journal.stage = OperationStage::ArtifactValidated;
        let jpath = ccync_foundation::paths::tool_operation_journal_path().unwrap();
        fs::create_dir_all(jpath.parent().unwrap()).unwrap();
        fs::write(&jpath, serde_json::to_vec(&journal).unwrap()).unwrap();

        materialize_tool_snapshots(staging.path()).unwrap();
        let rendered_skill = staging.path().join("skills").join(id.as_str()).join("SKILL.md");
        assert_eq!(
            fs::read_to_string(&rendered_skill).unwrap(),
            "# generation 1"
        );

        let staging_pending = tempfile::tempdir().unwrap();
        {
            let _scope = PendingGenerationScope::new(id.clone(), 2);
            materialize_tool_snapshots(staging_pending.path()).unwrap();
            let rendered_gen2 = staging_pending
                .path()
                .join("skills")
                .join(id.as_str())
                .join("SKILL.md");
            assert_eq!(
                fs::read_to_string(&rendered_gen2).unwrap(),
                "# generation 2"
            );
        }

        let staging_after = tempfile::tempdir().unwrap();
        materialize_tool_snapshots(staging_after.path()).unwrap();
        let rendered_after = staging_after
            .path()
            .join("skills")
            .join(id.as_str())
            .join("SKILL.md");
        assert_eq!(
            fs::read_to_string(&rendered_after).unwrap(),
            "# generation 1"
        );
    }

    #[test]
    fn render_canonical_root_rtk_path_empty_render_contribution() {
        let (_home, _guard) = test_home();
        let staging = tempfile::tempdir().unwrap();
        let tool_id = ToolId::new("rtk").unwrap();
        let decl = ToolDeclaration {
            id: tool_id.clone(),
            identity: InstallationIdentity {
                adapter_kind: AdapterKind::Cargo,
                fields: BTreeMap::new(),
            },
            delivery_id: DeliveryId::RtkPath,
            hold_policy: HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        };
        let mut decls = BTreeMap::new();
        decls.insert(tool_id.clone(), decl);
        store::replace_declarations(&decls).unwrap();

        let warnings = materialize_tool_snapshots(staging.path()).unwrap();
        assert!(warnings.is_empty());
        assert!(!staging.path().join("skills").join("rtk").exists());
    }
}
