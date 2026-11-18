use super::*;
use ccync_foundation::health::HealthCheck;
use ccync_foundation::platform::{create_dir_link, is_symlink_or_junction};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

// All fixtures stay inside a temporary home and build root.
struct StandaloneFixture {
    temp: TempDir,
    request: StandalonePublicationRequest,
}

impl StandaloneFixture {
    fn new() -> Self {
        use ccync_foundation::provider::{PathRoot, RelativePath, ScopedPath};
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        let discovery = home.join(".agents").join("skills");
        let build = temp.path().join(".ccync").join("build");
        let source = temp.path().join("upstream").join("demo");
        fs::create_dir_all(&discovery).unwrap();
        fs::create_dir_all(&build).unwrap();
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("SKILL.md"), "# Demo\nUse assets/data.bin.").unwrap();
        let request = StandalonePublicationRequest {
            operation_id: "publication-1".into(),
            source_path: source,
            source_id: "upstream-provider".into(),
            source_revision: "commit:0123456789abcdef".into(),
            destination: ScopedPath {
                root: PathRoot::UserHome,
                path: RelativePath::new(".agents/skills/demo").unwrap(),
            },
            destination_root: home,
            build_root: build,
            stage_path: RelativePath::new("standalone-publications/publication-1/stage").unwrap(),
            backup_path: RelativePath::new("standalone-publications/publication-1/backup").unwrap(),
            discovery_roots: vec![discovery],
            targets: vec!["codex@fixture".into(), "copilot@fixture".into()],
            reserved_paths: Vec::new(),
            prior_owner: None,
            owned_fingerprint: None,
            manifest_proofs: Vec::new(),
        };
        Self { temp, request }
    }

    fn destination(&self) -> PathBuf {
        self.request
            .destination
            .path
            .join_to(&self.request.destination_root)
    }

    fn stage(&self) -> PathBuf {
        self.request.stage_path.join_to(&self.request.build_root)
    }
    fn backup(&self) -> PathBuf {
        self.request.backup_path.join_to(&self.request.build_root)
    }

    fn prepare(&self) -> PreparedStandalonePublication {
        prepare_standalone_publication(self.request.clone()).unwrap()
    }

    fn persist(&self, prepared: &PreparedStandalonePublication) {
        use ccync_foundation::provider::ProviderLifecycleJournal;
        let mut journal = ProviderLifecycleJournal::default();
        journal.standalone_intents.push(prepared.intent().clone());
        fs::write(
            self.request.build_root.join("projection.txn.json"),
            journal.to_json_string().unwrap(),
        )
        .unwrap();
    }

    fn clear(&self) {
        use ccync_foundation::provider::ProviderLifecycleJournal;
        fs::write(
            self.request.build_root.join("projection.txn.json"),
            ProviderLifecycleJournal::default()
                .to_json_string()
                .unwrap(),
        )
        .unwrap();
    }

    fn own_old_tree(&mut self) {
        let destination = self.destination();
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("old.txt"), b"previous complete bytes").unwrap();
        let mut report = ProjectionReport::default();
        let mut registry =
            ManagedArtifactRegistry::load(&self.request.build_root.join("lock.json"), &mut report);
        registry.mark_with_source(MANAGED_SKILL_PATHS, &destination, "legacy-owner");
        registry.persist(&[], false, &mut report).unwrap();
        self.request.prior_owner = Some("legacy-owner".into());
        self.request.owned_fingerprint = shared_path_fingerprint(&destination);
    }

    fn stage_and_publish(&self, prepared: &PreparedStandalonePublication) {
        self.persist(prepared);
        stage_standalone_publication(prepared, prepared.intent()).unwrap();
        publish_standalone_publication(prepared, prepared.intent()).unwrap();
    }
}

struct StandaloneWithdrawalFixture {
    publication: StandaloneFixture,
    request: StandaloneWithdrawalRequest,
}

impl StandaloneWithdrawalFixture {
    fn new() -> Self {
        use ccync_foundation::provider::{
            RelativePath, StandaloneWithdrawalReaderIntent, StandaloneWithdrawalReaderProof,
        };
        let publication = StandaloneFixture::new();
        let prepared = publication.prepare();
        publication.stage_and_publish(&prepared);
        complete_standalone_publication(&prepared, prepared.intent()).unwrap();
        publication.clear();
        cleanup_standalone_publication(&prepared, prepared.intent()).unwrap();
        let committed_record =
            standalone_publication_records(&publication.request.build_root.join("lock.json"))
                .unwrap()
                .remove(0);
        let readers = committed_record
            .targets
            .iter()
            .map(|reader| StandaloneWithdrawalReaderProof {
                reader: reader.clone(),
                intent: StandaloneWithdrawalReaderIntent::Disabled,
                evidence: "current source settings deliberately disable this reader".into(),
            })
            .collect();
        let request = StandaloneWithdrawalRequest {
            operation_id: "withdrawal-1".into(),
            committed_record,
            readers,
            destination: publication.request.destination.clone(),
            destination_root: publication.request.destination_root.clone(),
            build_root: publication.request.build_root.clone(),
            backup_path: RelativePath::new("standalone-withdrawals/withdrawal-1/backup").unwrap(),
            discovery_roots: publication.request.discovery_roots.clone(),
            reserved_paths: vec![],
        };
        Self {
            publication,
            request,
        }
    }

    fn prepare(&self) -> PreparedStandaloneWithdrawal {
        prepare_standalone_withdrawal(self.request.clone()).unwrap()
    }

    fn backup(&self) -> PathBuf {
        self.request.backup_path.join_to(&self.request.build_root)
    }

    fn persist(&self, prepared: &PreparedStandaloneWithdrawal) {
        use ccync_foundation::provider::ProviderLifecycleJournal;
        let mut journal = ProviderLifecycleJournal::default();
        journal
            .standalone_withdrawal_intents
            .push(prepared.intent().clone());
        self.write_journal(&journal);
    }

    fn write_journal(&self, journal: &ccync_foundation::provider::ProviderLifecycleJournal) {
        fs::write(
            self.request.build_root.join("projection.txn.json"),
            journal.to_json_string().unwrap(),
        )
        .unwrap();
    }
}

#[test]
fn standalone_withdrawal_prepare_is_read_only_after_source_removal() {
    let fixture = StandaloneWithdrawalFixture::new();
    fs::remove_dir_all(&fixture.publication.request.source_path).unwrap();
    let lock = fixture.request.build_root.join("lock.json");
    let before = fs::read(&lock).unwrap();
    let prepared = fixture.prepare();
    assert_eq!(
        prepared.intent().source_id,
        fixture.request.committed_record.source_id
    );
    assert_eq!(fs::read(lock).unwrap(), before);
    assert!(!fixture.backup().parent().unwrap().exists());
    assert!(fixture.publication.destination().join("SKILL.md").is_file());
}

#[test]
fn standalone_withdrawal_requires_one_durable_matching_intent_before_writes() {
    let fixture = StandaloneWithdrawalFixture::new();
    let prepared = fixture.prepare();
    assert!(apply_standalone_withdrawal(&prepared, prepared.intent()).is_err());
    assert!(!fixture.backup().parent().unwrap().exists());
    fixture.persist(&prepared);
    let journal = ccync_foundation::provider::ProviderLifecycleJournal {
        standalone_withdrawal_intents: vec![prepared.intent().clone(); 2],
        ..Default::default()
    };
    fixture.write_journal(&journal);
    assert!(apply_standalone_withdrawal(&prepared, prepared.intent()).is_err());
    assert!(fixture.publication.destination().is_dir());
    assert!(!fixture.backup().exists());
}

#[test]
fn standalone_withdrawal_source_gone_completes_and_retains_unknown_registry_data() {
    let fixture = StandaloneWithdrawalFixture::new();
    fs::remove_dir_all(&fixture.publication.request.source_path).unwrap();
    let lock = fixture.request.build_root.join("lock.json");
    let mut root: Value = serde_json::from_slice(&fs::read(&lock).unwrap()).unwrap();
    root["unrelated"] = serde_json::json!({"keep": 17});
    fs::write(&lock, serde_json::to_vec(&root).unwrap()).unwrap();
    let prepared = fixture.prepare();
    fixture.persist(&prepared);
    apply_standalone_withdrawal(&prepared, prepared.intent()).unwrap();
    assert!(!fixture.publication.destination().exists());
    assert!(fixture.backup().join("SKILL.md").is_file());
    assert!(!standalone_publication_records(&lock).unwrap().is_empty());
    complete_standalone_withdrawal(&prepared, prepared.intent()).unwrap();
    assert!(standalone_publication_records(&lock).unwrap().is_empty());
    assert!(direct_records(&lock).is_empty());
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&lock).unwrap()).unwrap()["unrelated"],
        root["unrelated"]
    );
    assert!(cleanup_standalone_withdrawal(&prepared, prepared.intent()).is_err());
    fixture.publication.clear();
    cleanup_standalone_withdrawal(&prepared, prepared.intent()).unwrap();
    cleanup_standalone_withdrawal(&prepared, prepared.intent()).unwrap();
    assert!(!fixture.backup().exists());
}

#[test]
fn standalone_withdrawal_reader_coverage_and_freshness_are_separate() {
    use ccync_foundation::provider::{
        StandaloneWithdrawalReaderIntent, StandaloneWithdrawalReaderProof,
    };
    let mut fixture = StandaloneWithdrawalFixture::new();
    fixture
        .request
        .readers
        .push(StandaloneWithdrawalReaderProof {
            reader: "additional-unselected-reader".into(),
            intent: StandaloneWithdrawalReaderIntent::ReplacementVerified,
            evidence: "current native route verified at its owned selector".into(),
        });
    let prepared = fixture.prepare();
    fixture.persist(&prepared);
    let lock_before = fs::read(fixture.request.build_root.join("lock.json")).unwrap();
    let mut changed = fixture.request.clone();
    changed.readers[0].evidence = "source intent changed".into();
    assert!(resume_standalone_withdrawal(changed, prepared.intent()).is_err());
    let mut changed = fixture.request.clone();
    changed.readers.push(StandaloneWithdrawalReaderProof {
        reader: "newly-discovered-reader".into(),
        intent: StandaloneWithdrawalReaderIntent::Disabled,
        evidence: "current setting".into(),
    });
    assert!(resume_standalone_withdrawal(changed, prepared.intent()).is_err());
    assert_eq!(
        fs::read(fixture.request.build_root.join("lock.json")).unwrap(),
        lock_before
    );
    let mut missing = fixture.request.clone();
    missing.readers.remove(0);
    assert!(prepare_standalone_withdrawal(missing).is_err());
    let mut invalid = fixture.request.clone();
    invalid.operation_id = "withdrawal-2".into();
    invalid.backup_path =
        ccync_foundation::provider::RelativePath::new("standalone-withdrawals/withdrawal-2/backup")
            .unwrap();
    invalid.readers[0].evidence.clear();
    assert!(prepare_standalone_withdrawal(invalid).is_err());
    assert!(fixture.publication.destination().is_dir());
}

#[test]
fn standalone_withdrawal_resumes_each_durable_boundary_without_writes() {
    let fixture = StandaloneWithdrawalFixture::new();
    let prepared = fixture.prepare();
    fixture.persist(&prepared);
    let before = fs::read(fixture.request.build_root.join("projection.txn.json")).unwrap();
    let resumed = resume_standalone_withdrawal(fixture.request.clone(), prepared.intent()).unwrap();
    assert!(!fixture.backup().parent().unwrap().exists());
    assert_eq!(
        fs::read(fixture.request.build_root.join("projection.txn.json")).unwrap(),
        before
    );
    apply_standalone_withdrawal(&resumed, resumed.intent()).unwrap();
    let resumed = resume_standalone_withdrawal(fixture.request.clone(), prepared.intent()).unwrap();
    assert!(fixture.backup().join("SKILL.md").is_file());
    complete_standalone_withdrawal(&resumed, resumed.intent()).unwrap();
    let resumed = resume_standalone_withdrawal(fixture.request.clone(), prepared.intent()).unwrap();
    apply_standalone_withdrawal(&resumed, resumed.intent()).unwrap();
    complete_standalone_withdrawal(&resumed, resumed.intent()).unwrap();
    fixture.publication.clear();
    // A restarted caller cannot reconstruct an unjournaled backup for deletion.
    assert!(resume_standalone_withdrawal(fixture.request.clone(), prepared.intent()).is_err());
    assert!(fixture.backup().join("SKILL.md").is_file());
    cleanup_standalone_withdrawal(&resumed, resumed.intent()).unwrap();
}

#[test]
fn standalone_withdrawal_foreign_reappearance_after_commit_is_forward_blocked() {
    let fixture = StandaloneWithdrawalFixture::new();
    let prepared = fixture.prepare();
    fixture.persist(&prepared);
    apply_standalone_withdrawal(&prepared, prepared.intent()).unwrap();
    complete_standalone_withdrawal(&prepared, prepared.intent()).unwrap();
    fs::create_dir(fixture.publication.destination()).unwrap();
    fs::write(
        fixture.publication.destination().join("foreign.txt"),
        "retain",
    )
    .unwrap();
    assert!(resume_standalone_withdrawal(fixture.request.clone(), prepared.intent()).is_err());
    assert!(apply_standalone_withdrawal(&prepared, prepared.intent()).is_err());
    assert!(complete_standalone_withdrawal(&prepared, prepared.intent()).is_err());
    fixture.publication.clear();
    assert!(cleanup_standalone_withdrawal(&prepared, prepared.intent()).is_err());
    assert_eq!(
        fs::read(fixture.publication.destination().join("foreign.txt")).unwrap(),
        b"retain"
    );
    assert!(fixture.backup().join("SKILL.md").is_file());
}

#[test]
fn standalone_withdrawal_backup_and_registry_drift_preserve_evidence() {
    let fixture = StandaloneWithdrawalFixture::new();
    let prepared = fixture.prepare();
    fixture.persist(&prepared);
    apply_standalone_withdrawal(&prepared, prepared.intent()).unwrap();
    let backup_bytes = fs::read(fixture.backup().join("SKILL.md")).unwrap();
    fs::write(fixture.backup().join("SKILL.md"), "external drift").unwrap();
    assert!(resume_standalone_withdrawal(fixture.request.clone(), prepared.intent()).is_err());
    assert!(complete_standalone_withdrawal(&prepared, prepared.intent()).is_err());
    fs::write(fixture.backup().join("SKILL.md"), &backup_bytes).unwrap();
    let lock = fixture.request.build_root.join("lock.json");
    let lock_bytes = fs::read(&lock).unwrap();
    fs::write(&lock, "invalid registry").unwrap();
    assert!(complete_standalone_withdrawal(&prepared, prepared.intent()).is_err());
    assert!(fixture.backup().join("SKILL.md").is_file());
    assert!(fixture
        .request
        .build_root
        .join("projection.txn.json")
        .is_file());
    fs::write(&lock, lock_bytes).unwrap();
    complete_standalone_withdrawal(&prepared, prepared.intent()).unwrap();
}

#[test]
fn standalone_withdrawal_rejects_legacy_proof_and_unproven_ownership() {
    let mut fixture = StandaloneWithdrawalFixture::new();
    fixture.request.committed_record.tree_fingerprint =
        shared_path_fingerprint(&fixture.publication.destination()).unwrap();
    assert!(prepare_standalone_withdrawal(fixture.request.clone()).is_err());
    let fixture = StandaloneWithdrawalFixture::new();
    forget_direct_record(
        &fixture.request.build_root.join("lock.json"),
        DirectArtifactKind::Skill,
        &fixture.publication.destination(),
        false,
    )
    .unwrap();
    assert!(prepare_standalone_withdrawal(fixture.request.clone()).is_err());
    assert!(fixture.publication.destination().join("SKILL.md").is_file());
}

#[test]
fn standalone_withdrawal_reservation_added_after_prepare_blocks_before_backup_creation() {
    let fixture = StandaloneWithdrawalFixture::new();
    let prepared = fixture.prepare();
    fixture.persist(&prepared);
    let mut other = prepared.intent().clone();
    other.operation_id = "other-operation".into();
    other.destination.path =
        ccync_foundation::provider::RelativePath::new(".agents/skills/demo/child").unwrap();
    other.preimage_ref.path = other.destination.clone();
    other.backup_path = ccync_foundation::provider::RelativePath::new(
        "standalone-withdrawals/other-operation/backup",
    )
    .unwrap();
    other.preimage_ref.saved = other.backup_path.clone();
    let journal = ccync_foundation::provider::ProviderLifecycleJournal {
        standalone_withdrawal_intents: vec![prepared.intent().clone(), other],
        ..Default::default()
    };
    fixture.write_journal(&journal);
    assert!(apply_standalone_withdrawal(&prepared, prepared.intent()).is_err());
    assert!(!fixture.backup().parent().unwrap().exists());
    assert!(fixture.publication.destination().is_dir());
}

#[test]
fn standalone_withdrawal_publication_identity_and_preimage_reservations_block() {
    let fixture = StandaloneWithdrawalFixture::new();
    let prepared = fixture.prepare();
    let mut request = fixture.publication.request.clone();
    request.operation_id = prepared.intent().operation_id.clone();
    request.destination.path =
        ccync_foundation::provider::RelativePath::new(".agents/skills/other").unwrap();
    request.stage_path =
        ccync_foundation::provider::RelativePath::new("standalone-publications/withdrawal-1/stage")
            .unwrap();
    request.backup_path = ccync_foundation::provider::RelativePath::new(
        "standalone-publications/withdrawal-1/backup",
    )
    .unwrap();
    let publication = prepare_standalone_publication(request).unwrap();
    let mut journal = ccync_foundation::provider::ProviderLifecycleJournal::default();
    journal
        .standalone_intents
        .push(publication.intent().clone());
    fixture.write_journal(&journal);
    assert!(prepare_standalone_withdrawal(fixture.request.clone()).is_err());
    journal
        .standalone_withdrawal_intents
        .push(prepared.intent().clone());
    fixture.write_journal(&journal);
    assert!(apply_standalone_withdrawal(&prepared, prepared.intent()).is_err());
    assert!(!fixture.backup().exists());
}

#[test]
fn standalone_withdrawal_dangling_destination_is_present_and_retained() {
    let fixture = StandaloneWithdrawalFixture::new();
    let prepared = fixture.prepare();
    fixture.persist(&prepared);
    apply_standalone_withdrawal(&prepared, prepared.intent()).unwrap();
    let target = fixture.publication.temp.path().join("temporary-target");
    fs::create_dir(&target).unwrap();
    create_dir_link(&target, &fixture.publication.destination()).unwrap();
    fs::remove_dir(&target).unwrap();
    assert!(fs::symlink_metadata(fixture.publication.destination()).is_ok());
    assert!(complete_standalone_withdrawal(&prepared, prepared.intent()).is_err());
    assert!(resume_standalone_withdrawal(fixture.request.clone(), prepared.intent()).is_err());
    assert!(fixture.backup().join("SKILL.md").is_file());
}

#[cfg(unix)]
#[test]
fn standalone_withdrawal_permission_drift_retains_backup_and_intent() {
    let fixture = StandaloneWithdrawalFixture::new();
    let prepared = fixture.prepare();
    fixture.persist(&prepared);
    apply_standalone_withdrawal(&prepared, prepared.intent()).unwrap();
    let file = fixture.backup().join("SKILL.md");
    let previous = standalone_mode(&file);
    standalone_set_mode(&file, previous ^ 0o100);
    assert!(resume_standalone_withdrawal(fixture.request.clone(), prepared.intent()).is_err());
    assert!(complete_standalone_withdrawal(&prepared, prepared.intent()).is_err());
    assert!(fixture
        .request
        .build_root
        .join("projection.txn.json")
        .is_file());
}

#[cfg(unix)]
#[test]
fn standalone_withdrawal_independent_permission_denied_is_not_absence() {
    use std::os::unix::fs::PermissionsExt;

    struct RestorePermissions {
        path: PathBuf,
        permissions: fs::Permissions,
    }

    impl Drop for RestorePermissions {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.path, self.permissions.clone());
        }
    }

    fn deny_traversal(path: &Path) -> RestorePermissions {
        let permissions = fs::metadata(path).unwrap().permissions();
        let restore = RestorePermissions {
            path: path.to_path_buf(),
            permissions: permissions.clone(),
        };
        fs::set_permissions(path, fs::Permissions::from_mode(0)).unwrap();
        restore
    }

    let fixture = StandaloneWithdrawalFixture::new();
    let destination = fixture.publication.destination();
    let lock = fixture.request.build_root.join("lock.json");
    let journal = fixture.request.build_root.join("projection.txn.json");
    let lock_before = fs::read(&lock).unwrap();
    let journal_before = fs::read(&journal).unwrap();
    let tree_before = fs::read(destination.join("SKILL.md")).unwrap();
    let denied = deny_traversal(destination.parent().unwrap());
    assert_eq!(
        fs::symlink_metadata(&destination).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied,
        "the nonroot fixture must actually deny access before prepare"
    );
    assert!(prepare_standalone_withdrawal(fixture.request.clone()).is_err());
    assert_eq!(fs::read(&lock).unwrap(), lock_before);
    assert_eq!(fs::read(&journal).unwrap(), journal_before);
    assert!(!fixture.backup().parent().unwrap().exists());
    drop(denied);
    assert_eq!(fs::read(destination.join("SKILL.md")).unwrap(), tree_before);

    let prepared = fixture.prepare();
    fixture.persist(&prepared);
    apply_standalone_withdrawal(&prepared, prepared.intent()).unwrap();
    let journal_before = fs::read(&journal).unwrap();
    let denied = deny_traversal(fixture.backup().parent().unwrap());
    assert_eq!(
        fs::symlink_metadata(fixture.backup()).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied,
        "the nonroot fixture must actually deny access before recovery"
    );
    assert!(resume_standalone_withdrawal(fixture.request.clone(), prepared.intent()).is_err());
    assert!(apply_standalone_withdrawal(&prepared, prepared.intent()).is_err());
    assert!(complete_standalone_withdrawal(&prepared, prepared.intent()).is_err());
    assert_eq!(fs::read(&lock).unwrap(), lock_before);
    assert_eq!(fs::read(&journal).unwrap(), journal_before);
    assert!(fs::symlink_metadata(&destination)
        .is_err_and(|error| { error.kind() == std::io::ErrorKind::NotFound }));
    drop(denied);
    assert_eq!(
        fs::read(fixture.backup().join("SKILL.md")).unwrap(),
        tree_before
    );
    complete_standalone_withdrawal(&prepared, prepared.intent()).unwrap();
}

#[test]
fn standalone_withdrawal_native_live_and_saved_reservations_block() {
    use ccync_foundation::provider::{
        PathRoot, PendingIntent, PreimageRef, ProviderLifecycleJournal, RelativePath, ScopedPath,
    };
    let fixture = StandaloneWithdrawalFixture::new();
    let prepared = fixture.prepare();
    let mut native: PendingIntent = serde_json::from_value(serde_json::json!({
        "target": "codex", "ownershipFingerprints": [], "scopedPaths": [],
        "desiredGeneration": {"source": "native-provider", "generation": "generation-1", "packageDigest": "sha256:native"},
        "phase": "staged"
    })).unwrap();
    native.preimage_refs.push(PreimageRef {
        path: ScopedPath {
            root: PathRoot::UserHome,
            path: RelativePath::new(".other/native").unwrap(),
        },
        saved: RelativePath::new("standalone-withdrawals/withdrawal-1").unwrap(),
    });
    let mut journal = ProviderLifecycleJournal::default();
    journal.intents.push(native.clone());
    fixture.write_journal(&journal);
    assert!(prepare_standalone_withdrawal(fixture.request.clone()).is_err());
    journal
        .standalone_withdrawal_intents
        .push(prepared.intent().clone());
    fixture.write_journal(&journal);
    assert!(apply_standalone_withdrawal(&prepared, prepared.intent()).is_err());
    native.preimage_refs.clear();
    native.scoped_paths.push(ScopedPath {
        root: PathRoot::UserHome,
        path: RelativePath::new(".agents/skills").unwrap(),
    });
    journal.intents = vec![native];
    fixture.write_journal(&journal);
    assert!(apply_standalone_withdrawal(&prepared, prepared.intent()).is_err());
    assert!(!fixture.backup().exists());
    assert!(fixture.publication.destination().join("SKILL.md").is_file());
}

#[test]
fn standalone_withdrawal_cleanup_failure_retains_committed_absence() {
    let fixture = StandaloneWithdrawalFixture::new();
    let prepared = fixture.prepare();
    fixture.persist(&prepared);
    apply_standalone_withdrawal(&prepared, prepared.intent()).unwrap();
    complete_standalone_withdrawal(&prepared, prepared.intent()).unwrap();
    let journal = fixture.request.build_root.join("projection.txn.json");
    fs::remove_file(&journal).unwrap();
    fs::create_dir(&journal).unwrap();
    assert!(cleanup_standalone_withdrawal(&prepared, prepared.intent()).is_err());
    assert!(!fixture.publication.destination().exists());
    assert!(fixture.backup().join("SKILL.md").is_file());
    assert!(
        standalone_publication_records(&fixture.request.build_root.join("lock.json"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn standalone_withdrawal_registry_ancestor_added_after_rename_retains_every_proof() {
    let fixture = StandaloneWithdrawalFixture::new();
    let prepared = fixture.prepare();
    fixture.persist(&prepared);
    apply_standalone_withdrawal(&prepared, prepared.intent()).unwrap();
    let lock = fixture.request.build_root.join("lock.json");
    let mut report = ProjectionReport::default();
    let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
    registry.mark_with_source(
        MANAGED_SKILL_PATHS,
        fixture.publication.destination().parent().unwrap(),
        "foreign",
    );
    registry.persist(&[], false, &mut report).unwrap();
    let before = fs::read(&lock).unwrap();
    assert!(resume_standalone_withdrawal(fixture.request.clone(), prepared.intent()).is_err());
    assert!(complete_standalone_withdrawal(&prepared, prepared.intent()).is_err());
    assert_eq!(fs::read(&lock).unwrap(), before);
    assert!(fixture.backup().join("SKILL.md").is_file());
    assert!(fixture
        .request
        .build_root
        .join("projection.txn.json")
        .is_file());
}

#[test]
fn standalone_withdrawal_private_backup_must_remain_outside_all_discovery_roots() {
    let mut fixture = StandaloneWithdrawalFixture::new();
    fixture
        .request
        .discovery_roots
        .push(fixture.request.build_root.clone());
    assert!(prepare_standalone_withdrawal(fixture.request.clone()).is_err());
    assert!(!fixture.backup().exists());
    assert!(fixture.publication.destination().is_dir());
}

#[test]
fn standalone_withdrawal_root_link_preserves_permission_proof_and_referent() {
    let fixture = StandaloneWithdrawalFixture::new();
    let referent = fixture.publication.temp.path().join("owned-referent");
    fs::rename(fixture.publication.destination(), &referent).unwrap();
    create_dir_link(&referent, &fixture.publication.destination()).unwrap();
    let prepared = fixture.prepare();
    fixture.persist(&prepared);
    apply_standalone_withdrawal(&prepared, prepared.intent()).unwrap();
    assert!(is_symlink_or_junction(&fixture.backup()));
    let resumed = resume_standalone_withdrawal(fixture.request.clone(), prepared.intent()).unwrap();
    complete_standalone_withdrawal(&resumed, resumed.intent()).unwrap();
    fixture.publication.clear();
    cleanup_standalone_withdrawal(&resumed, resumed.intent()).unwrap();
    assert!(referent.join("SKILL.md").is_file());
    assert!(!fixture.publication.destination().exists());
}

#[cfg(unix)]
#[test]
fn standalone_withdrawal_independent_relative_root_link_resumes_and_cleans_forward() {
    use ccync_foundation::platform::{create_symlink_dir, LinkShape};

    let fixture = StandaloneWithdrawalFixture::new();
    let destination = fixture.publication.destination();
    let referent = fixture
        .publication
        .temp
        .path()
        .join("relative-owned-referent");
    fs::rename(&destination, &referent).unwrap();
    let raw_target = PathBuf::from("../../../relative-owned-referent");
    assert_eq!(
        create_symlink_dir(&raw_target, &destination).unwrap(),
        LinkShape::Symlink
    );
    assert_eq!(fs::read_link(&destination).unwrap(), raw_target);
    assert_eq!(
        fs::canonicalize(&destination).unwrap(),
        fs::canonicalize(&referent).unwrap()
    );
    let bytes = fs::read(referent.join("SKILL.md")).unwrap();
    let fingerprint = shared_path_fingerprint(&referent).unwrap();
    let prepared = fixture.prepare();
    fixture.persist(&prepared);
    let lock = fixture.request.build_root.join("lock.json");
    let journal = fixture.request.build_root.join("projection.txn.json");
    let lock_before = fs::read(&lock).unwrap();
    let journal_before = fs::read(&journal).unwrap();
    apply_standalone_withdrawal(&prepared, prepared.intent()).unwrap();

    fs::write(referent.join("SKILL.md"), "foreign referent drift").unwrap();
    assert!(resume_standalone_withdrawal(fixture.request.clone(), prepared.intent()).is_err());
    assert!(complete_standalone_withdrawal(&prepared, prepared.intent()).is_err());
    assert_eq!(fs::read(&lock).unwrap(), lock_before);
    assert_eq!(fs::read(&journal).unwrap(), journal_before);
    assert_eq!(fs::read_link(fixture.backup()).unwrap(), raw_target);
    fs::write(referent.join("SKILL.md"), &bytes).unwrap();

    let resumed = resume_standalone_withdrawal(fixture.request.clone(), prepared.intent());
    let completed = complete_standalone_withdrawal(&prepared, prepared.intent());
    eprintln!(
        "relative-link resume={:?}; complete={completed:?}",
        resumed.as_ref().map(|_| ())
    );
    assert_eq!(fs::read_link(fixture.backup()).unwrap(), raw_target);
    assert_eq!(fs::read(referent.join("SKILL.md")).unwrap(), bytes);
    assert_eq!(shared_path_fingerprint(&referent).unwrap(), fingerprint);
    assert!(fs::symlink_metadata(&destination)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound));
    assert_eq!(fs::read(&journal).unwrap(), journal_before);
    if completed.is_err() {
        assert_eq!(fs::read(&lock).unwrap(), lock_before);
    }
    let resumed = resumed.expect("an unchanged relative root Link must resume after rename");
    completed.expect("an unchanged relative root Link must complete after rename");
    let committed =
        resume_standalone_withdrawal(fixture.request.clone(), resumed.intent()).unwrap();
    apply_standalone_withdrawal(&committed, committed.intent()).unwrap();
    complete_standalone_withdrawal(&committed, committed.intent()).unwrap();
    assert!(standalone_publication_records(&lock).unwrap().is_empty());
    fixture.publication.clear();
    cleanup_standalone_withdrawal(&committed, committed.intent()).unwrap();
    assert!(fs::symlink_metadata(fixture.backup())
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound));
    assert_eq!(fs::read(referent.join("SKILL.md")).unwrap(), bytes);
    assert_eq!(shared_path_fingerprint(&referent).unwrap(), fingerprint);
}

#[cfg(unix)]
#[test]
fn standalone_withdrawal_independent_unreadable_foreign_alias_blocks_both_operations() {
    use ccync_foundation::provider::{
        PathRoot, PendingIntent, ProviderLifecycleJournal, RelativePath, ScopedPath,
    };
    use std::os::unix::fs::{symlink, PermissionsExt};

    struct RestorePermissions {
        path: PathBuf,
        permissions: fs::Permissions,
    }
    impl Drop for RestorePermissions {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.path, self.permissions.clone());
        }
    }
    fn native_alias(home: &Path, destination: &Path) -> (PendingIntent, RestorePermissions) {
        let private = home.join("private");
        fs::create_dir(&private).unwrap();
        let alias = private.join("alias");
        symlink(destination, &alias).unwrap();
        assert_eq!(
            fs::canonicalize(&alias).unwrap(),
            fs::canonicalize(destination).unwrap()
        );
        let restore = RestorePermissions {
            path: private.clone(),
            permissions: fs::metadata(&private).unwrap().permissions(),
        };
        let mut native: PendingIntent = serde_json::from_value(serde_json::json!({
            "target": "codex", "ownershipFingerprints": [], "scopedPaths": [],
            "desiredGeneration": {"source": "native-provider", "generation": "generation-1", "packageDigest": "sha256:native"},
            "phase": "staged"
        })).unwrap();
        native.scoped_paths.push(ScopedPath {
            root: PathRoot::UserHome,
            path: RelativePath::new("private/alias").unwrap(),
        });
        fs::set_permissions(&private, fs::Permissions::from_mode(0)).unwrap();
        assert_eq!(
            fs::symlink_metadata(&alias).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied,
            "the foreign reservation must actually be unreadable under nonroot"
        );
        (native, restore)
    }

    let withdrawal = StandaloneWithdrawalFixture::new();
    let prepared = withdrawal.prepare();
    let destination = withdrawal.publication.destination();
    let bytes = fs::read(destination.join("SKILL.md")).unwrap();
    let (native, denied_withdrawal) =
        native_alias(&withdrawal.request.destination_root, &destination);
    withdrawal.write_journal(&ProviderLifecycleJournal {
        intents: vec![native.clone()],
        ..Default::default()
    });
    let lock = withdrawal.request.build_root.join("lock.json");
    let journal = withdrawal.request.build_root.join("projection.txn.json");
    let lock_before = fs::read(&lock).unwrap();
    let prepare_journal_before = fs::read(&journal).unwrap();
    let withdraw_prepare = prepare_standalone_withdrawal(withdrawal.request.clone());
    assert_eq!(fs::read(&lock).unwrap(), lock_before);
    assert_eq!(fs::read(&journal).unwrap(), prepare_journal_before);
    withdrawal.write_journal(&ProviderLifecycleJournal {
        intents: vec![native],
        standalone_withdrawal_intents: vec![prepared.intent().clone()],
        ..Default::default()
    });
    let journal_before = fs::read(&journal).unwrap();
    let withdraw_apply = apply_standalone_withdrawal(&prepared, prepared.intent());
    let withdraw_resume =
        resume_standalone_withdrawal(withdrawal.request.clone(), prepared.intent());
    let withdrawal_destination = fs::read(destination.join("SKILL.md")).ok();
    let withdrawal_backup = fs::symlink_metadata(withdrawal.backup()).is_ok();
    eprintln!(
        "foreign-alias withdrawal prepare={:?}; apply={withdraw_apply:?}; resume={:?}; destination_retained={}; backup_present={withdrawal_backup}",
        withdraw_prepare.as_ref().map(|_| ()),
        withdraw_resume.as_ref().map(|_| ()),
        withdrawal_destination.as_deref() == Some(bytes.as_slice())
    );
    assert_eq!(fs::read(&lock).unwrap(), lock_before);
    assert_eq!(fs::read(&journal).unwrap(), journal_before);
    drop(denied_withdrawal);
    assert_eq!(
        fs::read_link(withdrawal.request.destination_root.join("private/alias")).unwrap(),
        destination
    );

    let mut publication = StandaloneFixture::new();
    publication.own_old_tree();
    let prepared_publication = publication.prepare();
    let (native, denied_publication) = native_alias(
        &publication.request.destination_root,
        &publication.destination(),
    );
    let mut pending = ProviderLifecycleJournal {
        intents: vec![native],
        ..Default::default()
    };
    fs::write(
        publication.request.build_root.join("projection.txn.json"),
        pending.to_json_string().unwrap(),
    )
    .unwrap();
    let pub_lock = publication.request.build_root.join("lock.json");
    let pub_journal = publication.request.build_root.join("projection.txn.json");
    let pub_lock_before = fs::read(&pub_lock).unwrap();
    let prepare_journal_before = fs::read(&pub_journal).unwrap();
    let publication_prepare = prepare_standalone_publication(publication.request.clone());
    assert_eq!(fs::read(&pub_lock).unwrap(), pub_lock_before);
    assert_eq!(fs::read(&pub_journal).unwrap(), prepare_journal_before);
    pending
        .standalone_intents
        .push(prepared_publication.intent().clone());
    fs::write(&pub_journal, pending.to_json_string().unwrap()).unwrap();
    let pub_journal_before = fs::read(&pub_journal).unwrap();
    let publication_stage =
        stage_standalone_publication(&prepared_publication, prepared_publication.intent());
    let publication_stage_present = publication.stage().parent().unwrap().exists();
    eprintln!(
        "foreign-alias publication prepare={:?}; stage={publication_stage:?}; stage_parent_present={publication_stage_present}",
        publication_prepare.as_ref().map(|_| ())
    );
    assert_eq!(fs::read(&pub_lock).unwrap(), pub_lock_before);
    assert_eq!(fs::read(&pub_journal).unwrap(), pub_journal_before);
    assert_eq!(
        fs::read(publication.destination().join("old.txt")).unwrap(),
        b"previous complete bytes"
    );
    drop(denied_publication);
    assert_eq!(
        fs::read_link(publication.request.destination_root.join("private/alias")).unwrap(),
        publication.destination()
    );

    assert!(
        withdraw_prepare.is_err(),
        "unreadable native reservation must block withdrawal prepare"
    );
    assert!(
        withdraw_apply.is_err(),
        "unreadable native reservation must block withdrawal apply"
    );
    assert!(
        withdraw_resume.is_err(),
        "unreadable native reservation must block withdrawal resume"
    );
    assert_eq!(withdrawal_destination.as_deref(), Some(bytes.as_slice()));
    assert!(!withdrawal_backup);
    assert!(!withdrawal.backup().parent().unwrap().exists());
    assert!(
        publication_prepare.is_err(),
        "unreadable native reservation must block publication prepare"
    );
    assert!(
        publication_stage.is_err(),
        "unreadable native reservation must block publication stage"
    );
    assert!(!publication_stage_present);
}

#[cfg(windows)]
#[test]
fn standalone_withdrawal_atomic_registry_write_failure_retains_pending_backup() {
    use std::os::windows::fs::OpenOptionsExt;
    let fixture = StandaloneWithdrawalFixture::new();
    let prepared = fixture.prepare();
    fixture.persist(&prepared);
    apply_standalone_withdrawal(&prepared, prepared.intent()).unwrap();
    let lock = fixture.request.build_root.join("lock.json");
    let before = fs::read(&lock).unwrap();
    // Readers may inspect the registry, but the open handle denies rename.
    let handle = fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&lock)
        .unwrap();
    assert!(complete_standalone_withdrawal(&prepared, prepared.intent()).is_err());
    assert_eq!(fs::read(&lock).unwrap(), before);
    assert!(fixture.backup().join("SKILL.md").is_file());
    assert!(fixture
        .request
        .build_root
        .join("projection.txn.json")
        .is_file());
    drop(handle);
    complete_standalone_withdrawal(&prepared, prepared.intent()).unwrap();
}

#[test]
fn standalone_publication_cleanup_respects_pending_withdrawal_references() {
    use ccync_foundation::provider::ProviderLifecycleJournal;
    let mut publication = StandaloneFixture::new();
    publication.own_old_tree();
    let prepared = publication.prepare();
    publication.stage_and_publish(&prepared);
    complete_standalone_publication(&prepared, prepared.intent()).unwrap();
    publication.clear();
    let withdrawal_fixture = StandaloneWithdrawalFixture::new();
    let mut withdrawal = withdrawal_fixture.prepare().intent().clone();
    withdrawal.backup_path = prepared.intent().backup_path.clone();
    withdrawal.preimage_ref.saved = withdrawal.backup_path.clone();
    let mut journal = ProviderLifecycleJournal::default();
    journal.standalone_withdrawal_intents.push(withdrawal);
    fs::write(
        publication.request.build_root.join("projection.txn.json"),
        journal.to_json_string().unwrap(),
    )
    .unwrap();
    assert!(cleanup_standalone_publication(&prepared, prepared.intent()).is_err());
    assert!(publication.backup().join("old.txt").is_file());
}

#[cfg(windows)]
fn standalone_assert_windows_alias_identity_then_remove(lower: &Path, upper: &Path) {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetFileInformationByHandle(handle: *mut c_void, info: *mut [u32; 13]) -> i32;
    }
    fs::create_dir(lower).unwrap();
    let lower_marker = lower.join("same-file.txt");
    let upper_marker = upper.join("same-file.txt");
    fs::write(&lower_marker, b"filesystem alias identity probe").unwrap();
    {
        let left = fs::File::open(&lower_marker).unwrap();
        let right = fs::File::open(&upper_marker).unwrap();
        let mut left_info = [0u32; 13];
        let mut right_info = [0u32; 13];
        // BY_HANDLE_FILE_INFORMATION consists of thirteen DWORDs. Compare the
        // volume serial and file-index pair while both handles are open.
        unsafe {
            assert_ne!(
                GetFileInformationByHandle(left.as_raw_handle(), &mut left_info),
                0
            );
            assert_ne!(
                GetFileInformationByHandle(right.as_raw_handle(), &mut right_info),
                0
            );
        }
        assert_eq!(
            (left_info[7], left_info[11], left_info[12]),
            (right_info[7], right_info[11], right_info[12]),
            "the test filesystem must resolve these names to the same file identity"
        );
    }
    fs::remove_file(lower_marker).unwrap();
    fs::remove_dir(lower).unwrap();
    assert!(!lower.exists());
    assert!(!upper.exists());
}

#[cfg(windows)]
#[test]
fn standalone_publication_independent_rejects_unicode_case_alias_reservation() {
    use ccync_foundation::provider::RelativePath;
    let mut fixture = StandaloneFixture::new();
    fixture.request.destination.path = RelativePath::new(".agents/skills/d\u{00e9}mo").unwrap();
    let reserved = fixture.request.discovery_roots[0].join("D\u{00c9}MO");
    standalone_assert_windows_alias_identity_then_remove(&fixture.destination(), &reserved);
    fixture.request.reserved_paths.push(reserved);
    let before = snapshot_tree(fixture.temp.path());
    let result = prepare_standalone_publication(fixture.request.clone());
    assert_eq!(snapshot_tree(fixture.temp.path()), before);
    assert!(
        result.is_err(),
        "Unicode case alias must reserve the absent destination"
    );
}

#[cfg(windows)]
#[test]
fn standalone_publication_independent_rejects_unicode_case_alias_child_reservation() {
    use ccync_foundation::provider::RelativePath;
    let mut fixture = StandaloneFixture::new();
    fixture.request.destination.path = RelativePath::new(".agents/skills/d\u{00e9}mo").unwrap();
    let alias = fixture.request.discovery_roots[0].join("D\u{00c9}MO");
    standalone_assert_windows_alias_identity_then_remove(&fixture.destination(), &alias);
    fixture
        .request
        .reserved_paths
        .push(alias.join("reserved-child"));
    let before = snapshot_tree(fixture.temp.path());
    let result = prepare_standalone_publication(fixture.request.clone());
    assert_eq!(snapshot_tree(fixture.temp.path()), before);
    assert!(
        result.is_err(),
        "Unicode case-aliased descendant must reserve the destination"
    );
}

#[cfg(windows)]
#[test]
fn standalone_publication_independent_rejects_unicode_case_alias_parent_reservation() {
    use ccync_foundation::provider::RelativePath;
    let mut fixture = StandaloneFixture::new();
    let root = fixture.request.discovery_roots[0].join("d\u{00e9}mo");
    let reserved = fixture.request.discovery_roots[0].join("D\u{00c9}MO");
    standalone_assert_windows_alias_identity_then_remove(&root, &reserved);
    fs::create_dir(&root).unwrap();
    fixture.request.discovery_roots = vec![root];
    fixture.request.destination.path =
        RelativePath::new(".agents/skills/d\u{00e9}mo/leaf").unwrap();
    fixture.request.reserved_paths.push(reserved);
    let before = snapshot_tree(fixture.temp.path());
    let result = prepare_standalone_publication(fixture.request.clone());
    assert_eq!(snapshot_tree(fixture.temp.path()), before);
    assert!(
        result.is_err(),
        "Unicode case-aliased ancestor must reserve the destination"
    );
}

#[cfg(windows)]
#[test]
fn standalone_publication_independent_rejects_greek_case_alias_reservation() {
    use ccync_foundation::provider::RelativePath;
    let mut fixture = StandaloneFixture::new();
    fixture.request.destination.path = RelativePath::new(".agents/skills/\u{03c3}demo").unwrap();
    let reserved = fixture.request.discovery_roots[0].join("\u{03a3}DEMO");
    standalone_assert_windows_alias_identity_then_remove(&fixture.destination(), &reserved);
    fixture.request.reserved_paths.push(reserved);
    let before = snapshot_tree(fixture.temp.path());
    let result = prepare_standalone_publication(fixture.request.clone());
    assert_eq!(snapshot_tree(fixture.temp.path()), before);
    assert!(
        result.is_err(),
        "native Greek case alias must reserve the absent destination"
    );
}

#[cfg(windows)]
#[test]
fn standalone_publication_independent_allows_distinct_reserved_name_prefix() {
    let mut fixture = StandaloneFixture::new();
    fixture
        .request
        .reserved_paths
        .push(fixture.destination().with_file_name("Demo-extra"));
    let before = snapshot_tree(fixture.temp.path());
    prepare_standalone_publication(fixture.request.clone()).unwrap();
    assert_eq!(snapshot_tree(fixture.temp.path()), before);
    assert!(!fixture.stage().exists());
    assert!(!fixture.destination().exists());
}

#[cfg(windows)]
#[test]
fn standalone_publication_independent_rejects_unicode_registry_reservation() {
    use ccync_foundation::provider::RelativePath;
    let mut fixture = StandaloneFixture::new();
    fixture.request.destination.path = RelativePath::new(".agents/skills/d\u{00e9}mo").unwrap();
    let alias = fixture.request.discovery_roots[0].join("D\u{00c9}MO");
    standalone_assert_windows_alias_identity_then_remove(&fixture.destination(), &alias);
    fixture.request.destination.path = RelativePath::new(".agents/skills/D\u{00c9}MO").unwrap();
    fixture.own_old_tree();
    fs::remove_dir_all(fixture.destination()).unwrap();
    fixture.request.destination.path = RelativePath::new(".agents/skills/d\u{00e9}mo").unwrap();
    fixture.request.prior_owner = None;
    fixture.request.owned_fingerprint = None;
    assert!(!fixture.destination().exists());
    let registry_path = fixture.request.build_root.join("lock.json");
    let registry_before = fs::read(&registry_path).unwrap();
    let before = snapshot_tree(fixture.temp.path());
    let result = prepare_standalone_publication(fixture.request.clone());
    assert_eq!(snapshot_tree(fixture.temp.path()), before);
    assert_eq!(fs::read(registry_path).unwrap(), registry_before);
    assert!(
        result.is_err(),
        "Unicode alias of a retained registry row must reserve the absent destination"
    );
}

#[cfg(windows)]
fn standalone_assert_managed_ancestor_of_owned_root_link_rejected(verbatim: bool) {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetFileInformationByHandle(handle: *mut c_void, info: *mut [u32; 13]) -> i32;
    }
    let mut fixture = StandaloneFixture::new();
    let ordinary_root = fixture.request.destination_root.clone();
    if verbatim {
        fixture.request.destination_root = fs::canonicalize(&ordinary_root).unwrap();
        assert!(matches!(
            fixture.request.destination_root.components().next(),
            Some(std::path::Component::Prefix(prefix))
                if matches!(prefix.kind(), std::path::Prefix::VerbatimDisk(_))
        ));
    }
    fixture.own_old_tree();
    fs::remove_dir_all(fixture.destination()).unwrap();
    let external = fixture.temp.path().join("old-render").join("demo");
    fs::create_dir_all(&external).unwrap();
    fs::write(
        external.join("retained.txt"),
        b"external target must survive",
    )
    .unwrap();
    create_dir_link(&external, &fixture.destination()).unwrap();
    fixture.request.owned_fingerprint = shared_path_fingerprint(&fixture.destination());
    let raw_target = fs::read_link(fixture.destination()).unwrap();
    let ordinary_destination = fixture.request.destination.path.join_to(&ordinary_root);
    assert_eq!(fs::read_link(&ordinary_destination).unwrap(), raw_target);
    {
        let left = fs::File::open(ordinary_destination.join("retained.txt")).unwrap();
        let right = fs::File::open(fixture.destination().join("retained.txt")).unwrap();
        let mut left_info = [0u32; 13];
        let mut right_info = [0u32; 13];
        unsafe {
            assert_ne!(
                GetFileInformationByHandle(left.as_raw_handle(), &mut left_info),
                0
            );
            assert_ne!(
                GetFileInformationByHandle(right.as_raw_handle(), &mut right_info),
                0
            );
        }
        assert_eq!(
            (left_info[7], left_info[11], left_info[12]),
            (right_info[7], right_info[11], right_info[12]),
            "ordinary and verbatim destination spellings must reach the same file"
        );
    }
    let registry_path = fixture.request.build_root.join("lock.json");
    let mut report = ProjectionReport::default();
    let mut registry = ManagedArtifactRegistry::load(&registry_path, &mut report);
    registry.mark_with_source(
        MANAGED_SKILL_PATHS,
        &fixture.request.discovery_roots[0],
        "foreign-ancestor-owner",
    );
    registry.persist(&[], false, &mut report).unwrap();
    let registry_before = fs::read(&registry_path).unwrap();
    let before = snapshot_tree(fixture.temp.path());

    let result = prepare_standalone_publication(fixture.request.clone());

    assert_eq!(snapshot_tree(fixture.temp.path()), before);
    assert_eq!(fs::read(registry_path).unwrap(), registry_before);
    assert_eq!(fs::read_link(fixture.destination()).unwrap(), raw_target);
    assert_eq!(
        fs::read(external.join("retained.txt")).unwrap(),
        b"external target must survive"
    );
    assert!(!fixture.stage().exists());
    assert!(!fixture.backup().exists());
    assert!(
        result.is_err(),
        "a managed lexical ancestor must reserve an owned root-Link destination"
    );
}

#[cfg(windows)]
#[test]
fn standalone_publication_independent_rejects_managed_ancestor_of_owned_root_link() {
    standalone_assert_managed_ancestor_of_owned_root_link_rejected(false);
}

#[cfg(windows)]
#[test]
fn standalone_publication_independent_rejects_managed_ancestor_of_verbatim_owned_root_link() {
    standalone_assert_managed_ancestor_of_owned_root_link_rejected(true);
}

#[test]
fn standalone_publication_independent_rejects_managed_ancestor_added_to_pending_root_link() {
    let mut fixture = StandaloneFixture::new();
    fixture.own_old_tree();
    fs::remove_dir_all(fixture.destination()).unwrap();
    let external = fixture.temp.path().join("old-render").join("demo");
    fs::create_dir_all(&external).unwrap();
    fs::write(
        external.join("retained.txt"),
        b"pending target must survive",
    )
    .unwrap();
    create_dir_link(&external, &fixture.destination()).unwrap();
    fixture.request.owned_fingerprint = shared_path_fingerprint(&fixture.destination());
    let prepared = fixture.prepare();
    fixture.persist(&prepared);
    stage_standalone_publication(&prepared, prepared.intent()).unwrap();

    let registry_path = fixture.request.build_root.join("lock.json");
    let mut report = ProjectionReport::default();
    let mut registry = ManagedArtifactRegistry::load(&registry_path, &mut report);
    registry.mark_with_source(
        MANAGED_SKILL_PATHS,
        &fixture.request.discovery_roots[0],
        "foreign-ancestor-owner",
    );
    registry.persist(&[], false, &mut report).unwrap();
    let registry_before = fs::read(&registry_path).unwrap();
    let journal_path = fixture.request.build_root.join("projection.txn.json");
    let journal_before = fs::read(&journal_path).unwrap();
    let raw_target = fs::read_link(fixture.destination()).unwrap();
    let before = snapshot_tree(fixture.temp.path());

    let result = resume_standalone_publication(fixture.request.clone(), prepared.intent());

    assert_eq!(snapshot_tree(fixture.temp.path()), before);
    assert_eq!(fs::read(registry_path).unwrap(), registry_before);
    assert_eq!(fs::read(journal_path).unwrap(), journal_before);
    assert_eq!(fs::read_link(fixture.destination()).unwrap(), raw_target);
    assert_eq!(
        fs::read(external.join("retained.txt")).unwrap(),
        b"pending target must survive"
    );
    assert!(fixture.stage().join("SKILL.md").is_file());
    assert!(!fixture.backup().exists());
    assert!(
        result.is_err(),
        "pending root-Link recovery must reject a newly reserved managed ancestor"
    );
}

#[cfg(windows)]
#[test]
fn standalone_publication_independent_rejects_absent_case_alias_reservation() {
    let mut fixture = StandaloneFixture::new();
    let reserved = fixture.destination().with_file_name("Demo");
    assert!(!reserved.exists());
    fixture.request.reserved_paths.push(reserved);
    let before = snapshot_tree(fixture.temp.path());
    let result = prepare_standalone_publication(fixture.request.clone());
    assert_eq!(snapshot_tree(fixture.temp.path()), before);
    assert!(
        result.is_err(),
        "case alias of an absent destination must be reserved"
    );
}

#[cfg(windows)]
#[test]
fn standalone_publication_independent_rejects_case_alias_child_reservation() {
    let mut fixture = StandaloneFixture::new();
    let reserved = fixture
        .destination()
        .with_file_name("Demo")
        .join("reserved-child");
    assert!(!reserved.exists());
    fixture.request.reserved_paths.push(reserved);
    let before = snapshot_tree(fixture.temp.path());
    let result = prepare_standalone_publication(fixture.request.clone());
    assert_eq!(snapshot_tree(fixture.temp.path()), before);
    assert!(
        result.is_err(),
        "a case-aliased reserved descendant must reserve the destination"
    );
}

#[cfg(windows)]
#[test]
fn standalone_publication_independent_rejects_case_alias_parent_reservation() {
    let mut fixture = StandaloneFixture::new();
    let reserved = fixture.request.discovery_roots[0].with_file_name("SKILLS");
    assert!(reserved.is_dir());
    fixture.request.reserved_paths.push(reserved);
    let before = snapshot_tree(fixture.temp.path());
    let result = prepare_standalone_publication(fixture.request.clone());
    assert_eq!(snapshot_tree(fixture.temp.path()), before);
    assert!(
        result.is_err(),
        "a case-aliased reserved ancestor must reserve the destination"
    );
}

#[test]
fn standalone_publication_independent_rejects_linked_tree_directory_preimage_read_only() {
    let mut fixture = StandaloneFixture::new();
    fixture.own_old_tree();
    let assets = fixture.destination().join("assets");
    fs::create_dir(&assets).unwrap();
    fs::write(assets.join("payload.bin"), b"original preimage asset").unwrap();
    let alias = fixture.destination().join("linked-assets");
    create_dir_link(&assets, &alias).unwrap();
    assert!(is_symlink_or_junction(&alias));
    let raw_target = fs::read_link(&alias).unwrap();
    fixture.request.owned_fingerprint = shared_path_fingerprint(&fixture.destination());
    let before = snapshot_tree(fixture.temp.path());
    let result = prepare_standalone_publication(fixture.request.clone());
    assert_eq!(snapshot_tree(fixture.temp.path()), before);
    assert_eq!(fs::read_link(&alias).unwrap(), raw_target);
    assert_eq!(
        fs::read(assets.join("payload.bin")).unwrap(),
        b"original preimage asset"
    );
    assert!(!fixture.stage().exists());
    assert!(!fixture.backup().exists());
    assert!(
        result.is_err(),
        "linked Tree preimages must be rejected before backup rename"
    );
}

#[cfg(unix)]
#[test]
fn standalone_publication_independent_rejects_linked_tree_file_preimage_read_only() {
    let mut fixture = StandaloneFixture::new();
    fixture.own_old_tree();
    let asset = fixture.destination().join("old.txt");
    let alias = fixture.destination().join("linked-file");
    std::os::unix::fs::symlink(&asset, &alias).unwrap();
    let raw_target = fs::read_link(&alias).unwrap();
    fixture.request.owned_fingerprint = shared_path_fingerprint(&fixture.destination());
    let before = snapshot_tree(fixture.temp.path());
    let result = prepare_standalone_publication(fixture.request.clone());
    assert_eq!(snapshot_tree(fixture.temp.path()), before);
    assert_eq!(fs::read_link(&alias).unwrap(), raw_target);
    assert_eq!(fs::read(&asset).unwrap(), b"previous complete bytes");
    assert!(!fixture.stage().exists());
    assert!(!fixture.backup().exists());
    assert!(
        result.is_err(),
        "linked Tree file preimages must be rejected before backup rename"
    );
}

#[test]
fn standalone_publication_preparation_is_read_only_and_captures_source_proof() {
    let fixture = StandaloneFixture::new();
    let before = snapshot_tree(fixture.temp.path());
    let prepared = fixture.prepare();
    assert_eq!(snapshot_tree(fixture.temp.path()), before);
    assert!(!fixture.stage().parent().unwrap().exists());
    assert!(!fixture.destination().exists());
    assert_eq!(
        prepared.intent().source_fingerprint.as_str(),
        standalone_source_fingerprint(&fixture.request.source_path).unwrap()
    );
    assert!(prepared.intent().source.contains("upstream-provider"));
    assert!(prepared.intent().source.contains("commit:0123456789abcdef"));
    assert!(prepared.intent().registry_after.is_some());
}

#[test]
fn standalone_publication_requires_durable_matching_intent_before_staging() {
    let fixture = StandaloneFixture::new();
    let prepared = fixture.prepare();
    assert!(stage_standalone_publication(&prepared, prepared.intent()).is_err());
    assert!(!fixture.stage().exists());
    fixture.persist(&prepared);
    let mut foreign = prepared.intent().clone();
    foreign.source = "foreign".into();
    assert!(stage_standalone_publication(&prepared, &foreign).is_err());
    assert!(!fixture.stage().exists());
    stage_standalone_publication(&prepared, prepared.intent()).unwrap();
}

#[test]
fn standalone_publication_materializes_internal_directory_links() {
    let fixture = StandaloneFixture::new();
    let assets = fixture.request.source_path.join("assets");
    fs::create_dir(&assets).unwrap();
    fs::write(assets.join("data.bin"), b"all asset bytes").unwrap();
    create_dir_link(&assets, &fixture.request.source_path.join("linked-assets")).unwrap();
    let prepared = fixture.prepare();
    fixture.stage_and_publish(&prepared);
    assert!(!is_symlink_or_junction(
        &fixture.destination().join("linked-assets")
    ));
    assert_eq!(
        fs::read(fixture.destination().join("linked-assets/data.bin")).unwrap(),
        b"all asset bytes"
    );
    assert_eq!(
        fs::read(fixture.destination().join("assets/data.bin")).unwrap(),
        b"all asset bytes"
    );
}

#[cfg(windows)]
#[test]
fn standalone_publication_materializes_junction_with_short_target_path() {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    #[link(name = "kernel32")]
    extern "system" {
        fn GetShortPathNameW(
            long_path: *const u16,
            short_path: *mut u16,
            buffer_length: u32,
        ) -> u32;
    }

    let fixture = StandaloneFixture::new();
    let assets = fixture
        .request
        .source_path
        .join("assets-with-a-long-directory-name");
    fs::create_dir(&assets).unwrap();
    fs::write(assets.join("data.bin"), b"all asset bytes").unwrap();

    let mut input: Vec<u16> = assets.as_os_str().encode_wide().collect();
    input.push(0);
    let mut capacity = 260usize;
    let short_assets = loop {
        let mut buffer = vec![0u16; capacity];
        let length =
            unsafe { GetShortPathNameW(input.as_ptr(), buffer.as_mut_ptr(), capacity as u32) }
                as usize;
        assert_ne!(length, 0, "could not query the short directory path");
        if length < capacity {
            buffer.truncate(length);
            break PathBuf::from(std::ffi::OsString::from_wide(&buffer));
        }
        capacity = length + 1;
    };
    if short_assets
        .to_string_lossy()
        .eq_ignore_ascii_case(&assets.to_string_lossy())
    {
        eprintln!("SKIP: this volume has no 8.3 alias for the long-named test directory");
        return;
    }

    let junction = fixture.request.source_path.join("linked-assets");
    let status = Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            &junction.to_string_lossy(),
            &short_assets.to_string_lossy(),
        ])
        .status()
        .unwrap();
    assert!(
        status.success(),
        "mklink /J failed to create the test junction"
    );
    assert!(is_symlink_or_junction(&junction));

    let prepared = fixture.prepare();
    fixture.stage_and_publish(&prepared);
    assert_eq!(
        fs::read(fixture.destination().join("linked-assets/data.bin")).unwrap(),
        b"all asset bytes"
    );
    assert!(!is_symlink_or_junction(
        &fixture.destination().join("linked-assets")
    ));
}

#[test]
fn standalone_publication_blocks_escaping_links_without_writes() {
    let fixture = StandaloneFixture::new();
    let outside = fixture.temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("private.txt"), b"not a skill resource").unwrap();
    create_dir_link(&outside, &fixture.request.source_path.join("escape")).unwrap();
    assert!(prepare_standalone_publication(fixture.request.clone()).is_err());
    assert!(!fixture.stage().exists());
    assert_eq!(
        fs::read(outside.join("private.txt")).unwrap(),
        b"not a skill resource"
    );
}

#[test]
fn standalone_publication_blocks_cyclic_links_without_writes() {
    let fixture = StandaloneFixture::new();
    create_dir_link(
        &fixture.request.source_path,
        &fixture.request.source_path.join("cycle"),
    )
    .unwrap();
    assert!(prepare_standalone_publication(fixture.request.clone()).is_err());
    assert!(!fixture.stage().exists());
}

#[test]
fn standalone_publication_accepts_known_original_subtree_alias() {
    let mut fixture = StandaloneFixture::new();
    let alias = fixture.temp.path().join("known-original");
    create_dir_link(&fixture.request.source_path, &alias).unwrap();
    fixture.request.source_path = alias;
    let prepared = fixture.prepare();
    fixture.stage_and_publish(&prepared);
    assert!(fixture.destination().join("SKILL.md").is_file());
    assert!(!is_symlink_or_junction(&fixture.destination()));
}

#[test]
fn standalone_publication_preserves_complete_nested_and_binary_assets() {
    let fixture = StandaloneFixture::new();
    let assets = fixture.request.source_path.join("assets/nested");
    fs::create_dir_all(&assets).unwrap();
    fs::write(assets.join("data.bin"), [0, 0xff, 1, 2, 0]).unwrap();
    fs::create_dir(fixture.request.source_path.join("empty-assets")).unwrap();
    let prepared = fixture.prepare();
    fixture.stage_and_publish(&prepared);
    assert_eq!(
        snapshot_tree(&fixture.destination()),
        snapshot_tree(&fixture.request.source_path)
    );
    assert!(fixture.destination().join("empty-assets").is_dir());
}

#[test]
fn standalone_publication_blocks_embedded_discovery_without_loading_proof() {
    let fixture = StandaloneFixture::new();
    let manifest = fixture.request.source_path.join(".codex-plugin");
    fs::create_dir(&manifest).unwrap();
    fs::write(manifest.join("plugin.json"), br#"{"name":"upstream"}"#).unwrap();
    let error = prepare_standalone_publication(fixture.request.clone())
        .unwrap_err()
        .to_string();
    assert!(error.contains("embedded manifest"));
    assert!(!fixture.stage().exists());
}

#[test]
fn standalone_publication_preserves_embedded_manifest_bytes_with_bound_proof() {
    use ccync_foundation::provider::RelativePath;
    let mut fixture = StandaloneFixture::new();
    let manifest = fixture.request.source_path.join(".codex-plugin");
    fs::create_dir(&manifest).unwrap();
    let bytes = b"{\n  \"name\": \"Upstream.Identity\", \"version\": \"1.2.3\"\n}\n";
    fs::write(manifest.join("plugin.json"), bytes).unwrap();
    fixture
        .request
        .manifest_proofs
        .push(StandaloneManifestProof {
            path: RelativePath::new(".codex-plugin/plugin.json").unwrap(),
            fingerprint: shared_path_fingerprint(&manifest.join("plugin.json")).unwrap(),
            source_id: fixture.request.source_id.clone(),
            revision: fixture.request.source_revision.clone(),
            reader_versions: fixture
                .request
                .targets
                .iter()
                .map(|target| (target.clone(), "tested-1.2".into()))
                .collect(),
        });
    let prepared = fixture.prepare();
    fixture.stage_and_publish(&prepared);
    assert_eq!(
        fs::read(fixture.destination().join(".codex-plugin/plugin.json")).unwrap(),
        bytes
    );
    fixture.request.manifest_proofs[0]
        .reader_versions
        .remove("copilot@fixture");
    assert!(prepare_standalone_publication(fixture.request).is_err());
}

#[test]
fn standalone_publication_blocks_manifest_provenance_and_version_mismatch() {
    use ccync_foundation::provider::RelativePath;
    let mut fixture = StandaloneFixture::new();
    fs::write(
        fixture.request.source_path.join("plugin.json"),
        b"upstream bytes",
    )
    .unwrap();
    fixture
        .request
        .manifest_proofs
        .push(StandaloneManifestProof {
            path: RelativePath::new("plugin.json").unwrap(),
            fingerprint: shared_path_fingerprint(&fixture.request.source_path.join("plugin.json"))
                .unwrap(),
            source_id: "foreign".into(),
            revision: fixture.request.source_revision.clone(),
            reader_versions: fixture
                .request
                .targets
                .iter()
                .map(|target| (target.clone(), "v1".into()))
                .collect(),
        });
    assert!(prepare_standalone_publication(fixture.request.clone()).is_err());
    fixture.request.manifest_proofs[0].source_id = fixture.request.source_id.clone();
    fixture.request.manifest_proofs[0].revision = "different-pin".into();
    assert!(prepare_standalone_publication(fixture.request.clone()).is_err());
    fixture.request.manifest_proofs[0].revision = fixture.request.source_revision.clone();
    fixture.request.manifest_proofs[0].fingerprint = "fnv1a64:wrong".into();
    assert!(prepare_standalone_publication(fixture.request).is_err());
}

#[test]
fn standalone_publication_blocks_cross_subtree_and_plugin_root_resources() {
    for reference in [
        "Read ../../other-skill/data.json",
        "Run ${CLAUDE_PLUGIN_ROOT}/scripts/run.py",
        "Read ${CODEX_PLUGIN_ROOT}/data.bin",
        "Read file:///outside/data",
    ] {
        let fixture = StandaloneFixture::new();
        fs::write(fixture.request.source_path.join("SKILL.md"), reference).unwrap();
        assert!(
            prepare_standalone_publication(fixture.request.clone()).is_err(),
            "{reference}"
        );
        assert!(!fixture.stage().exists());
    }
}

#[test]
fn standalone_publication_preserves_foreign_and_unattributed_destinations() {
    let fixture = StandaloneFixture::new();
    fs::create_dir(fixture.destination()).unwrap();
    fs::write(fixture.destination().join("owner.txt"), b"user bytes").unwrap();
    let before = snapshot_tree(&fixture.destination());
    assert!(prepare_standalone_publication(fixture.request.clone()).is_err());
    assert_eq!(snapshot_tree(&fixture.destination()), before);

    let mut fixture = StandaloneFixture::new();
    fixture.own_old_tree();
    fixture.request.prior_owner = Some("another-owner".into());
    assert!(prepare_standalone_publication(fixture.request.clone()).is_err());
    assert!(fixture.destination().join("old.txt").is_file());
}

#[test]
fn standalone_publication_preserves_reservations_and_registry_collisions() {
    let mut fixture = StandaloneFixture::new();
    fixture.request.reserved_paths.push(fixture.destination());
    assert!(prepare_standalone_publication(fixture.request.clone()).is_err());
    fixture.request.reserved_paths.clear();
    let mut report = ProjectionReport::default();
    let mut registry =
        ManagedArtifactRegistry::load(&fixture.request.build_root.join("lock.json"), &mut report);
    registry.mark_with_source(
        MANAGED_AGENT_PATHS,
        &fixture.destination(),
        &fixture.request.source_id,
    );
    registry.persist(&[], false, &mut report).unwrap();
    fixture.request.prior_owner = Some(fixture.request.source_id.clone());
    assert!(prepare_standalone_publication(fixture.request).is_err());
}

#[test]
fn standalone_publication_blocks_linked_destination_ancestors() {
    let mut fixture = StandaloneFixture::new();
    let other = fixture.temp.path().join("other-discovery");
    fs::create_dir(&other).unwrap();
    let discovery = fixture.request.discovery_roots[0].clone();
    fs::remove_dir(&discovery).unwrap();
    create_dir_link(&other, &discovery).unwrap();
    fixture.request.discovery_roots[0] = discovery;
    assert!(prepare_standalone_publication(fixture.request).is_err());
    assert!(fs::read_dir(&other).unwrap().next().is_none());
}

#[test]
fn standalone_publication_blocks_combined_manifest_ancestors() {
    let fixture = StandaloneFixture::new();
    let manifest = fixture
        .request
        .destination_root
        .join(".agents/.codex-plugin");
    fs::create_dir(&manifest).unwrap();
    fs::write(manifest.join("plugin.json"), br#"{"name":"ccync"}"#).unwrap();
    assert!(prepare_standalone_publication(fixture.request.clone()).is_err());
    assert!(!fixture.destination().exists());
}

#[test]
fn standalone_publication_rejects_unsafe_stage_paths_and_linked_build_ancestors() {
    use ccync_foundation::provider::RelativePath;
    let mut fixture = StandaloneFixture::new();
    fixture.request.stage_path = RelativePath::new("some-user-directory").unwrap();
    assert!(prepare_standalone_publication(fixture.request.clone()).is_err());
    fixture.request.stage_path =
        RelativePath::new("standalone-publications/publication-1/stage").unwrap();
    let link = fixture.request.build_root.join("standalone-publications");
    let foreign = fixture.temp.path().join("foreign-build");
    fs::create_dir(&foreign).unwrap();
    create_dir_link(&foreign, &link).unwrap();
    assert!(prepare_standalone_publication(fixture.request.clone()).is_err());
    assert!(fs::read_dir(foreign).unwrap().next().is_none());

    let mut fixture = StandaloneFixture::new();
    fixture.request.build_root = fixture.request.discovery_roots[0].join("build");
    fs::create_dir(&fixture.request.build_root).unwrap();
    assert!(prepare_standalone_publication(fixture.request).is_err());
}

#[test]
fn standalone_publication_rechecks_source_destination_and_staged_content() {
    let fixture = StandaloneFixture::new();
    let prepared = fixture.prepare();
    fixture.persist(&prepared);
    fs::write(
        fixture.request.source_path.join("SKILL.md"),
        b"source drift",
    )
    .unwrap();
    assert!(stage_standalone_publication(&prepared, prepared.intent()).is_err());
    assert!(!fixture.stage().exists());

    let mut fixture = StandaloneFixture::new();
    fixture.own_old_tree();
    let prepared = fixture.prepare();
    fixture.persist(&prepared);
    stage_standalone_publication(&prepared, prepared.intent()).unwrap();
    fs::write(fixture.destination().join("old.txt"), b"owner drift").unwrap();
    assert!(publish_standalone_publication(&prepared, prepared.intent()).is_err());
    assert!(!fixture.backup().exists());
    assert_eq!(
        fs::read(fixture.destination().join("old.txt")).unwrap(),
        b"owner drift"
    );

    let fixture = StandaloneFixture::new();
    let prepared = fixture.prepare();
    fixture.persist(&prepared);
    stage_standalone_publication(&prepared, prepared.intent()).unwrap();
    fs::write(fixture.stage().join("SKILL.md"), b"stage drift").unwrap();
    assert!(publish_standalone_publication(&prepared, prepared.intent()).is_err());
    assert!(!fixture.destination().exists());
}

#[test]
fn standalone_publication_retains_backup_through_registry_commit_and_journal_clear() {
    let mut fixture = StandaloneFixture::new();
    fixture.own_old_tree();
    let prepared = fixture.prepare();
    fixture.stage_and_publish(&prepared);
    assert_eq!(
        fs::read(fixture.backup().join("old.txt")).unwrap(),
        b"previous complete bytes"
    );
    assert!(cleanup_standalone_publication(&prepared, prepared.intent()).is_err());
    complete_standalone_publication(&prepared, prepared.intent()).unwrap();
    assert!(fixture.backup().join("old.txt").is_file());
    let records = direct_records(&fixture.request.build_root.join("lock.json"));
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].source_id.as_deref(), Some("upstream-provider"));
    assert!(cleanup_standalone_publication(&prepared, prepared.intent()).is_err());
    fixture.clear();
    cleanup_standalone_publication(&prepared, prepared.intent()).unwrap();
    assert!(!fixture.backup().exists());
    assert!(fixture.destination().join("SKILL.md").is_file());
    cleanup_standalone_publication(&prepared, prepared.intent()).unwrap();
}

#[test]
fn standalone_publication_registry_failure_and_foreign_backup_drift_retain_evidence() {
    let mut fixture = StandaloneFixture::new();
    fixture.own_old_tree();
    let prepared = fixture.prepare();
    fixture.stage_and_publish(&prepared);
    let lock = fixture.request.build_root.join("lock.json");
    let original = fs::read(&lock).unwrap();
    fs::write(&lock, b"{ invalid registry").unwrap();
    assert!(complete_standalone_publication(&prepared, prepared.intent()).is_err());
    assert!(fixture.backup().join("old.txt").is_file());
    assert!(fixture
        .request
        .build_root
        .join("projection.txn.json")
        .is_file());
    fs::write(&lock, original).unwrap();
    complete_standalone_publication(&prepared, prepared.intent()).unwrap();
    fixture.clear();
    fs::write(fixture.backup().join("old.txt"), b"foreign backup drift").unwrap();
    assert!(cleanup_standalone_publication(&prepared, prepared.intent()).is_err());
    assert_eq!(
        fs::read(fixture.backup().join("old.txt")).unwrap(),
        b"foreign backup drift"
    );
}

#[test]
fn standalone_publication_resumes_after_switch_and_retries_registry_completion() {
    let mut fixture = StandaloneFixture::new();
    fixture.own_old_tree();
    let prepared = fixture.prepare();
    fixture.stage_and_publish(&prepared);
    let resumed =
        resume_standalone_publication(fixture.request.clone(), prepared.intent()).unwrap();
    complete_standalone_publication(&resumed, resumed.intent()).unwrap();
    complete_standalone_publication(&resumed, resumed.intent()).unwrap();
    assert!(fixture.backup().exists());
    fixture.clear();
    cleanup_standalone_publication(&resumed, resumed.intent()).unwrap();
    assert!(!fixture.backup().exists());
}

#[test]
fn standalone_publication_resume_preserves_foreign_published_drift() {
    let fixture = StandaloneFixture::new();
    let prepared = fixture.prepare();
    fixture.stage_and_publish(&prepared);
    fs::write(
        fixture.destination().join("SKILL.md"),
        b"owner changed publication",
    )
    .unwrap();
    assert!(resume_standalone_publication(fixture.request.clone(), prepared.intent()).is_err());
    assert_eq!(
        fs::read(fixture.destination().join("SKILL.md")).unwrap(),
        b"owner changed publication"
    );
    assert!(fixture
        .request
        .build_root
        .join("projection.txn.json")
        .is_file());
}

#[test]
fn standalone_publication_private_stage_survives_legacy_orphan_sweep() {
    let fixture = StandaloneFixture::new();
    let prepared = fixture.prepare();
    fixture.persist(&prepared);
    stage_standalone_publication(&prepared, prepared.intent()).unwrap();
    support::sweep_staging_orphans(&fixture.destination());
    assert!(fixture.stage().join("SKILL.md").is_file());
    publish_standalone_publication(&prepared, prepared.intent()).unwrap();
    assert!(fixture.destination().join("SKILL.md").is_file());
}

#[test]
fn standalone_publication_preserves_resolvable_parent_resource_references() {
    let fixture = StandaloneFixture::new();
    fs::create_dir(fixture.request.source_path.join("references")).unwrap();
    fs::create_dir(fixture.request.source_path.join("assets")).unwrap();
    fs::write(
        fixture.request.source_path.join("assets").join("data.bin"),
        b"resource",
    )
    .unwrap();
    let text = b"Read [asset](../assets/data.bin)";
    fs::write(
        fixture
            .request
            .source_path
            .join("references")
            .join("guide.md"),
        text,
    )
    .unwrap();
    let prepared = fixture.prepare();
    fixture.stage_and_publish(&prepared);
    assert_eq!(
        fs::read(fixture.destination().join("references/guide.md")).unwrap(),
        text
    );
    assert_eq!(
        fs::read(fixture.destination().join("assets/data.bin")).unwrap(),
        b"resource"
    );
}

#[test]
fn standalone_publication_blocks_links_that_escape_then_return_inside() {
    let fixture = StandaloneFixture::new();
    let assets = fixture.request.source_path.join("assets");
    fs::create_dir(&assets).unwrap();
    fs::write(assets.join("data.bin"), b"inside bytes").unwrap();
    let external = fixture.temp.path().join("external-alias");
    create_dir_link(&assets, &external).unwrap();
    create_dir_link(&external, &fixture.request.source_path.join("roundtrip")).unwrap();
    assert!(prepare_standalone_publication(fixture.request.clone()).is_err());
    assert!(!fixture.stage().exists());
}

#[test]
fn standalone_publication_resumes_after_backup_rename_before_switch() {
    let mut fixture = StandaloneFixture::new();
    fixture.own_old_tree();
    let prepared = fixture.prepare();
    fixture.persist(&prepared);
    stage_standalone_publication(&prepared, prepared.intent()).unwrap();
    fs::rename(fixture.destination(), fixture.backup()).unwrap();
    let resumed =
        resume_standalone_publication(fixture.request.clone(), prepared.intent()).unwrap();
    publish_standalone_publication(&resumed, resumed.intent()).unwrap();
    publish_standalone_publication(&resumed, resumed.intent()).unwrap();
    complete_standalone_publication(&resumed, resumed.intent()).unwrap();
    assert!(fixture.backup().join("old.txt").is_file());
    fixture.clear();
    cleanup_standalone_publication(&resumed, resumed.intent()).unwrap();
}

#[test]
fn standalone_publication_retains_raw_owned_link_preimage_and_target() {
    use ccync_foundation::provider::StandalonePreimage;
    let mut fixture = StandaloneFixture::new();
    fixture.own_old_tree();
    fs::remove_dir_all(fixture.destination()).unwrap();
    let old = fixture.temp.path().join("old-render");
    fs::create_dir(&old).unwrap();
    fs::write(old.join("retained.txt"), b"old target must survive").unwrap();
    create_dir_link(&old, &fixture.destination()).unwrap();
    fixture.request.owned_fingerprint = shared_path_fingerprint(&fixture.destination());
    let raw_target = fs::read_link(fixture.destination()).unwrap();
    let prepared = fixture.prepare();
    assert_eq!(
        prepared.intent().preimage,
        StandalonePreimage::Link {
            raw_target: raw_target.clone()
        }
    );
    assert!(prepared.intent().link_kind.is_some());
    fixture.stage_and_publish(&prepared);
    assert_eq!(fs::read_link(fixture.backup()).unwrap(), raw_target);
    complete_standalone_publication(&prepared, prepared.intent()).unwrap();
    fixture.clear();
    cleanup_standalone_publication(&prepared, prepared.intent()).unwrap();
    assert!(!fixture.backup().exists());
    assert_eq!(
        fs::read(old.join("retained.txt")).unwrap(),
        b"old target must survive"
    );
}

#[test]
fn standalone_publication_registry_retains_version_proof_and_unknown_fields() {
    let fixture = StandaloneFixture::new();
    let prepared = fixture.prepare();
    fixture.stage_and_publish(&prepared);
    complete_standalone_publication(&prepared, prepared.intent()).unwrap();
    fixture.clear();
    let lockfile = fixture.request.build_root.join("lock.json");
    let mut root: Value = serde_json::from_slice(&fs::read(&lockfile).unwrap()).unwrap();
    root["foreignNamespace"] = serde_json::json!({"leave": "unchanged"});
    root["_ccyncProjection"]["unknownProjectionField"] = serde_json::json!({"opaque": [1, 2]});
    fs::write(&lockfile, serde_json::to_vec(&root).unwrap()).unwrap();
    let mut report = ProjectionReport::default();
    let mut registry = ManagedArtifactRegistry::load(&lockfile, &mut report);
    registry.mark_with_source(
        MANAGED_AGENT_PATHS,
        &fixture.temp.path().join("other-agent"),
        "other-source",
    );
    registry.persist(&[], false, &mut report).unwrap();
    let records = standalone_publication_records(&lockfile).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].path, fixture.destination());
    assert_eq!(records[0].source_id, fixture.request.source_id);
    assert_eq!(records[0].source_revision, fixture.request.source_revision);
    assert_eq!(
        records[0].tree_fingerprint,
        standalone_source_fingerprint(&fixture.destination()).unwrap()
    );
    assert_ne!(
        records[0].tree_fingerprint,
        shared_path_fingerprint(&fixture.destination()).unwrap()
    );
    assert_eq!(records[0].targets, fixture.request.targets);
    let root: Value = serde_json::from_slice(&fs::read(lockfile).unwrap()).unwrap();
    assert_eq!(
        root["foreignNamespace"],
        serde_json::json!({"leave": "unchanged"})
    );
    assert_eq!(
        root["_ccyncProjection"]["unknownProjectionField"],
        serde_json::json!({"opaque": [1, 2]})
    );
}

#[test]
fn standalone_publication_forget_replacement_and_prune_invalidate_version_proof() {
    for operation in ["forget", "replace", "prune"] {
        let fixture = StandaloneFixture::new();
        let prepared = fixture.prepare();
        fixture.stage_and_publish(&prepared);
        complete_standalone_publication(&prepared, prepared.intent()).unwrap();
        fixture.clear();
        let lockfile = fixture.request.build_root.join("lock.json");
        if operation == "forget" {
            fs::remove_dir_all(fixture.destination()).unwrap();
            forget_direct_record(
                &lockfile,
                DirectArtifactKind::Skill,
                &fixture.destination(),
                false,
            )
            .unwrap();
        } else {
            let mut report = ProjectionReport::default();
            let mut registry = ManagedArtifactRegistry::load(&lockfile, &mut report);
            if operation == "replace" {
                registry.mark_with_source(
                    MANAGED_DIRECT_SKILL_PATHS,
                    &fixture.destination(),
                    &fixture.request.source_id,
                );
            } else {
                registry.replace_category(MANAGED_DIRECT_SKILL_PATHS);
            }
            registry.persist(&[], false, &mut report).unwrap();
        }
        assert!(
            standalone_publication_records(&lockfile)
                .unwrap()
                .is_empty(),
            "{operation}"
        );
    }
}

#[test]
fn standalone_publication_registry_drift_prevents_switch_and_preserves_preimage() {
    let mut fixture = StandaloneFixture::new();
    fixture.own_old_tree();
    let prepared = fixture.prepare();
    fixture.persist(&prepared);
    stage_standalone_publication(&prepared, prepared.intent()).unwrap();
    let lockfile = fixture.request.build_root.join("lock.json");
    let mut report = ProjectionReport::default();
    let mut registry = ManagedArtifactRegistry::load(&lockfile, &mut report);
    registry.mark_with_source(MANAGED_SKILL_PATHS, &fixture.destination(), "foreign");
    registry.persist(&[], false, &mut report).unwrap();
    assert!(publish_standalone_publication(&prepared, prepared.intent()).is_err());
    assert!(fixture.destination().join("old.txt").is_file());
    assert!(!fixture.backup().exists());
}

#[cfg(unix)]
fn standalone_set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(unix)]
fn standalone_mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

#[cfg(unix)]
#[test]
fn standalone_publication_preserves_unix_executable_and_directory_modes() {
    let fixture = StandaloneFixture::new();
    let source = &fixture.request.source_path;
    fs::create_dir(source.join("scripts")).unwrap();
    fs::create_dir(source.join("assets")).unwrap();
    let script = b"#!/bin/sh\nprintf 'standalone executable'\n";
    fs::write(source.join("scripts/run.sh"), script).unwrap();
    fs::write(source.join("assets/data.bin"), b"complete resource").unwrap();
    let modes = [
        ("", 0o750),
        ("SKILL.md", 0o640),
        ("scripts", 0o550),
        ("scripts/run.sh", 0o751),
        ("assets", 0o500),
        ("assets/data.bin", 0o440),
    ];
    for (relative, mode) in modes {
        standalone_set_mode(&source.join(relative), mode);
    }
    let prepared = fixture.prepare();
    fixture.persist(&prepared);
    stage_standalone_publication(&prepared, prepared.intent()).unwrap();
    for (relative, mode) in modes {
        assert_eq!(
            standalone_mode(&fixture.stage().join(relative)),
            mode,
            "stage {relative}"
        );
    }
    publish_standalone_publication(&prepared, prepared.intent()).unwrap();
    complete_standalone_publication(&prepared, prepared.intent()).unwrap();
    fixture.clear();
    cleanup_standalone_publication(&prepared, prepared.intent()).unwrap();
    for (relative, mode) in modes {
        assert_eq!(
            standalone_mode(&fixture.destination().join(relative)),
            mode,
            "published {relative}"
        );
        assert_eq!(
            standalone_mode(&source.join(relative)),
            mode,
            "source {relative}"
        );
    }
    assert_eq!(
        fs::read(fixture.destination().join("scripts/run.sh")).unwrap(),
        script
    );
    assert_eq!(
        fs::read(fixture.destination().join("assets/data.bin")).unwrap(),
        b"complete resource"
    );
    let output = Command::new(fixture.destination().join("scripts/run.sh"))
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"standalone executable");
    assert_eq!(
        standalone_source_fingerprint(source).unwrap(),
        standalone_source_fingerprint(&fixture.destination()).unwrap()
    );
    // Restore owner write access so a non-root tempfile cleanup can remove
    // these deliberately read-only directories after all assertions.
    for root in [source, &fixture.destination()] {
        for relative in ["scripts", "assets"] {
            standalone_set_mode(&root.join(relative), 0o700);
        }
    }
}

#[cfg(unix)]
#[test]
fn standalone_publication_materializes_unix_internal_target_permissions() {
    use std::os::unix::fs::symlink;
    let fixture = StandaloneFixture::new();
    let source = &fixture.request.source_path;
    fs::create_dir(source.join("scripts")).unwrap();
    fs::write(source.join("scripts/run.sh"), b"#!/bin/sh\nexit 0\n").unwrap();
    standalone_set_mode(&source.join("scripts/run.sh"), 0o751);
    standalone_set_mode(&source.join("scripts"), 0o750);
    symlink("scripts/run.sh", source.join("run.sh")).unwrap();
    symlink("scripts", source.join("linked-scripts")).unwrap();
    let prepared = fixture.prepare();
    fixture.stage_and_publish(&prepared);
    for (relative, mode) in [
        ("run.sh", 0o751),
        ("linked-scripts", 0o750),
        ("linked-scripts/run.sh", 0o751),
    ] {
        let published = fixture.destination().join(relative);
        assert!(!is_symlink_or_junction(&published));
        assert_eq!(standalone_mode(&published), mode);
    }
    assert!(fs::symlink_metadata(source.join("run.sh"))
        .unwrap()
        .file_type()
        .is_symlink());
}

#[cfg(unix)]
#[test]
fn standalone_publication_rejects_unix_source_mode_drift() {
    for relative in ["SKILL.md", ""] {
        let fixture = StandaloneFixture::new();
        let path = fixture.request.source_path.join(relative);
        standalone_set_mode(&path, 0o750);
        let prepared = fixture.prepare();
        fixture.persist(&prepared);
        let shared = shared_path_fingerprint(&fixture.request.source_path);
        standalone_set_mode(&path, 0o700);
        assert_eq!(
            shared_path_fingerprint(&fixture.request.source_path),
            shared
        );
        assert_ne!(
            standalone_source_fingerprint(&fixture.request.source_path).unwrap(),
            prepared.intent().source_fingerprint.as_str()
        );
        assert!(
            stage_standalone_publication(&prepared, prepared.intent()).is_err(),
            "{relative}"
        );
        assert!(resume_standalone_publication(fixture.request.clone(), prepared.intent()).is_err());
        assert!(!fixture.stage().exists());
        assert!(!fixture.destination().exists());
    }
}

#[cfg(unix)]
#[test]
fn standalone_publication_rejects_unix_staged_mode_drift_and_recovers() {
    for relative in ["SKILL.md", ""] {
        let fixture = StandaloneFixture::new();
        let prepared = fixture.prepare();
        fixture.persist(&prepared);
        stage_standalone_publication(&prepared, prepared.intent()).unwrap();
        let path = fixture.stage().join(relative);
        let mode = standalone_mode(&path);
        let shared = shared_path_fingerprint(&fixture.stage());
        standalone_set_mode(&path, mode ^ 0o010);
        assert_eq!(shared_path_fingerprint(&fixture.stage()), shared);
        assert!(
            publish_standalone_publication(&prepared, prepared.intent()).is_err(),
            "{relative}"
        );
        assert!(resume_standalone_publication(fixture.request.clone(), prepared.intent()).is_err());
        assert!(!fixture.destination().exists());
        assert!(path.exists());
        standalone_set_mode(&path, mode);
        let resumed =
            resume_standalone_publication(fixture.request.clone(), prepared.intent()).unwrap();
        publish_standalone_publication(&resumed, resumed.intent()).unwrap();
        complete_standalone_publication(&resumed, resumed.intent()).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn standalone_publication_rejects_unix_published_mode_drift_and_recovers() {
    for relative in ["SKILL.md", ""] {
        let mut fixture = StandaloneFixture::new();
        fixture.own_old_tree();
        let prepared = fixture.prepare();
        fixture.stage_and_publish(&prepared);
        let path = fixture.destination().join(relative);
        let mode = standalone_mode(&path);
        let shared = shared_path_fingerprint(&fixture.destination());
        standalone_set_mode(&path, mode ^ 0o010);
        assert_eq!(shared_path_fingerprint(&fixture.destination()), shared);
        assert!(
            complete_standalone_publication(&prepared, prepared.intent()).is_err(),
            "{relative}"
        );
        assert!(resume_standalone_publication(fixture.request.clone(), prepared.intent()).is_err());
        assert!(fixture.backup().join("old.txt").is_file());
        assert!(
            standalone_publication_records(&fixture.request.build_root.join("lock.json"))
                .unwrap()
                .is_empty()
        );
        assert!(fixture
            .request
            .build_root
            .join("projection.txn.json")
            .is_file());
        standalone_set_mode(&path, mode);
        let resumed =
            resume_standalone_publication(fixture.request.clone(), prepared.intent()).unwrap();
        complete_standalone_publication(&resumed, resumed.intent()).unwrap();
        assert!(cleanup_standalone_publication(&resumed, resumed.intent()).is_err());
        assert!(fixture.backup().exists());
        fixture.clear();
        cleanup_standalone_publication(&resumed, resumed.intent()).unwrap();
        assert!(!fixture.backup().exists());
    }
}

#[cfg(unix)]
#[test]
fn standalone_publication_rejects_unix_backup_mode_drift_after_restart() {
    for relative in ["old.txt", ""] {
        let mut fixture = StandaloneFixture::new();
        fixture.own_old_tree();
        let prepared = fixture.prepare();
        let expected = standalone_source_fingerprint(&fixture.destination()).unwrap();
        assert_eq!(
            prepared
                .intent()
                .preimage_fingerprint
                .as_ref()
                .unwrap()
                .as_str(),
            expected
        );
        fixture.stage_and_publish(&prepared);
        let path = fixture.backup().join(relative);
        let mode = standalone_mode(&path);
        let shared = shared_path_fingerprint(&fixture.backup());
        standalone_set_mode(&path, mode ^ 0o010);
        assert_eq!(shared_path_fingerprint(&fixture.backup()), shared);
        assert!(
            resume_standalone_publication(fixture.request.clone(), prepared.intent()).is_err(),
            "{relative}"
        );
        assert!(complete_standalone_publication(&prepared, prepared.intent()).is_err());
        assert!(path.exists());
        assert!(fixture.destination().join("SKILL.md").is_file());
        standalone_set_mode(&path, mode);
        let resumed =
            resume_standalone_publication(fixture.request.clone(), prepared.intent()).unwrap();
        complete_standalone_publication(&resumed, resumed.intent()).unwrap();
        fixture.clear();
        cleanup_standalone_publication(&resumed, resumed.intent()).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn standalone_publication_committed_unix_proof_cannot_downgrade_to_shared_content() {
    use ccync_foundation::provider::RelativePath;
    let mut fixture = StandaloneFixture::new();
    let prepared = fixture.prepare();
    fixture.stage_and_publish(&prepared);
    complete_standalone_publication(&prepared, prepared.intent()).unwrap();
    fixture.clear();
    let record = standalone_publication_records(&fixture.request.build_root.join("lock.json"))
        .unwrap()
        .remove(0);
    fixture.request.operation_id = "publication-2".into();
    fixture.request.stage_path =
        RelativePath::new("standalone-publications/publication-2/stage").unwrap();
    fixture.request.backup_path =
        RelativePath::new("standalone-publications/publication-2/backup").unwrap();
    fixture.request.prior_owner = Some(fixture.request.source_id.clone());
    fixture.request.owned_fingerprint = Some(record.tree_fingerprint.clone());
    let next = fixture.prepare();
    fixture.persist(&next);
    stage_standalone_publication(&next, next.intent()).unwrap();
    publish_standalone_publication(&next, next.intent()).unwrap();
    let resumed = resume_standalone_publication(fixture.request.clone(), next.intent()).unwrap();
    complete_standalone_publication(&resumed, resumed.intent()).unwrap();
    fixture.clear();
    cleanup_standalone_publication(&resumed, resumed.intent()).unwrap();
    fixture.request.operation_id = "publication-3".into();
    fixture.request.stage_path =
        RelativePath::new("standalone-publications/publication-3/stage").unwrap();
    fixture.request.backup_path =
        RelativePath::new("standalone-publications/publication-3/backup").unwrap();
    let path = fixture.destination().join("SKILL.md");
    standalone_set_mode(&path, standalone_mode(&path) ^ 0o010);
    assert!(prepare_standalone_publication(fixture.request.clone()).is_err());
    fixture.request.owned_fingerprint = shared_path_fingerprint(&fixture.destination());
    assert!(prepare_standalone_publication(fixture.request.clone()).is_err());
}

#[cfg(unix)]
#[test]
fn standalone_publication_blocks_unix_special_permission_bits_without_source_changes() {
    for (relative, mode) in [("SKILL.md", 0o4644), ("SKILL.md", 0o2644), ("", 0o1755)] {
        let fixture = StandaloneFixture::new();
        let path = fixture.request.source_path.join(relative);
        standalone_set_mode(&path, mode);
        let bytes = fs::read(fixture.request.source_path.join("SKILL.md")).unwrap();
        assert!(prepare_standalone_publication(fixture.request.clone()).is_err());
        assert_eq!(standalone_mode(&path), mode);
        assert_eq!(
            fs::read(fixture.request.source_path.join("SKILL.md")).unwrap(),
            bytes
        );
        assert!(!fixture.stage().exists());
    }
}

// Recursively snapshot every regular file's bytes and every symlink/junction's
// raw `read_link` target under `root`, keyed by path relative to `root`. Used
// by the idempotence and dry-run regressions below to assert an entire
// projected surface (files, link targets, and the lockfile living inside it)
// is byte-identical across two runs, without hardcoding every path a full
// pipeline touches.
fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let rel = rel.to_path_buf();
            if is_symlink_or_junction(&path) {
                let target = fs::read_link(&path)
                    .map(|t| t.to_string_lossy().into_owned())
                    .unwrap_or_default();
                out.insert(rel, target.into_bytes());
            } else if path.is_dir() {
                walk(root, &path, out);
            } else if path.is_file() {
                out.insert(rel, fs::read(&path).unwrap_or_default());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

// ── ProjectionEvidence test helpers ────────────────────────────────────────
//
// Several older tests were written against the legacy flat `ProjectionReport`
// (`written_files`/`removed_paths`/`created_links`/`warnings`, run-global, no
// target attribution). Now that every producer returns `ProjectionEvidence`
// directly, these helpers flatten `common` + every `by_target` bucket back
// into the same "any path/warning touched, anywhere" shape those tests
// actually need — deliberately collapsing attribution here (unlike the
// evidence-shaped tests elsewhere in this file, which assert attribution
// directly) since that is what these particular tests were checking.
fn evidence_paths(evidence: &ProjectionEvidence) -> impl Iterator<Item = &ProjectionPathResult> {
    evidence
        .common
        .iter()
        .chain(evidence.by_target.values().flat_map(|t| t.paths.iter()))
}

fn evidence_has_change(evidence: &ProjectionEvidence, change: ProjectionPathChange) -> bool {
    evidence_paths(evidence).any(|p| p.change == change)
}

fn evidence_all_warnings(evidence: &ProjectionEvidence) -> Vec<String> {
    let mut warnings = evidence.run_warnings.clone();
    for target in evidence.by_target.values() {
        warnings.extend(target.warnings.iter().cloned());
    }
    warnings
}

#[cfg(windows)]
#[test]
fn remove_link_removes_directory_junction_not_target() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("keep.txt"), "keep").unwrap();
    let link = tmp.path().join("junction");
    let status = Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            &link.to_string_lossy(),
            &target.to_string_lossy(),
        ])
        .status()
        .unwrap();
    assert!(
        status.success(),
        "mklink /J failed to create the test junction"
    );
    assert!(
        is_symlink_or_junction(&link),
        "test setup: link should be a junction"
    );
    // Regression: before the fix `remove_link` gated on `is_dir()` (false for a
    // Windows junction) and fell to `fs::remove_file` → ERROR_ACCESS_DENIED (os error 5).
    remove_link(&link)
        .expect("remove_link must remove a directory junction (os error 5 regression)");
    assert!(!link.exists(), "junction must be removed");
    assert!(
        target.join("keep.txt").exists(),
        "removing a junction must NOT touch the target directory"
    );
}

// Prune path (option B): only the CCYNC content header determines ownership;
// frontmatter alone is not enough. A header-stamped stale file is removed; a
// frontmatter-only file without a header is now treated as a user file and
// preserved even when absent from the active-set.
//
// Also exercises the pre-attribution regression: `stale.toml` has no
// `source_attribution` row at all (a lockfile predating attribution tracking,
// or an entry `mark`-ed without a source). `owning_source_of` returns `None`
// for it, which must fall through to normal authorization rather than being
// treated as foreign-owned — even while the registry carries an unrelated
// attribution row and `sources` is non-empty, matching the shape a real
// invocation with other installed sources would have.
#[test]
fn prune_stale_links_prunes_pre_attribution_entry_with_installed_sources() {
    use crate::support::MANAGED_SKILL_PATHS;

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("agents");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("my-analyst.agent.md"),
        "---\nname: my-analyst\n---\nbody\n",
    )
    .unwrap();
    fs::write(
        root.join("my-dockeeper.agent.md"),
        "---\nname: my-dockeeper\n---\nbody\n",
    )
    .unwrap();
    fs::write(
        root.join("stale.toml"),
        format!("{CCYNC_MANAGED_FILE_HEADER}\nx = 1\n"),
    )
    .unwrap();
    fs::write(root.join("my-notes.md"), "personal notes\n").unwrap();

    let lockfile = tmp.path().join("lock.json");
    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    // Attribution recorded for an unrelated path only — `stale.toml` itself
    // has no attribution row, but the registry and `sources` are non-empty.
    reg.mark_with_source(
        MANAGED_SKILL_PATHS,
        &tmp.path().join("other-skill"),
        "my-bundle",
    );
    reg.persist(&[], false, &mut report).unwrap();
    let reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);

    let sources = vec![
        ProjectionSource {
            root: PathBuf::new(),
            id: "ccync-core".into(),
            persistent: true,
        },
        ProjectionSource {
            root: PathBuf::new(),
            id: "my-bundle".into(),
            persistent: false,
        },
    ];
    prune_stale_links(&root, ["my-analyst.agent.md"], false, &reg2, &sources, None).unwrap();

    assert!(
        root.join("my-analyst.agent.md").exists(),
        "active agent must be kept"
    );
    assert!(
        root.join("my-dockeeper.agent.md").exists(),
        "frontmatter-only file without CCYNC header is now a user file — must be preserved"
    );
    assert!(
        !root.join("stale.toml").exists(),
        "entry with no attribution row must still fall through to normal authorization and be pruned"
    );
    assert!(
        root.join("my-notes.md").exists(),
        "non-CCYNC user file must be preserved"
    );
}

// Skip/failure exclusivity: a target's evidence cannot be built with both a
// skip reason and an expected failure at once.
#[test]
fn projection_target_evidence_rejects_skip_and_failure_together() {
    let result = ProjectionTargetEvidence::new(
        Vec::new(),
        Vec::new(),
        Some("nothing to project".to_string()),
        Some(ProjectionExpectedFailure {
            path: None,
            reason: "boom".to_string(),
        }),
    );
    assert!(result.is_err());

    let skip_only = ProjectionTargetEvidence::new(
        Vec::new(),
        Vec::new(),
        Some("nothing to project".to_string()),
        None,
    )
    .unwrap();
    assert!(skip_only.is_valid());

    let failure_only = ProjectionTargetEvidence::new(
        Vec::new(),
        Vec::new(),
        None,
        Some(ProjectionExpectedFailure {
            path: Some(PathBuf::from("/tmp/x")),
            reason: "boom".to_string(),
        }),
    )
    .unwrap();
    assert!(failure_only.is_valid());
}

// completed_targets and by_target are separate collections: a target can be
// completed with nothing to report (present in completed_targets, absent
// from by_target) versus never having run that phase at all (absent from
// completed_targets entirely). Both must be representable and distinguishable.
#[test]
fn projection_evidence_empty_completed_differs_from_absent() {
    let mut evidence = ProjectionEvidence::default();
    evidence.completed_targets.insert(ProjectionTargetId::Codex);

    assert!(
        evidence
            .completed_targets
            .contains(&ProjectionTargetId::Codex),
        "codex completed this phase"
    );
    assert!(
        !evidence.by_target.contains_key(&ProjectionTargetId::Codex),
        "codex completed with nothing to report — empty, not absent"
    );
    assert!(
        !evidence
            .completed_targets
            .contains(&ProjectionTargetId::Copilot),
        "copilot never ran this phase — absent, not merely empty"
    );
    assert!(!evidence
        .by_target
        .contains_key(&ProjectionTargetId::Copilot));
}

// Single source: projection's empty-selection fallback is the foundation's
// canonical target table — an absent/empty selection (LegacyState::Absent)
// falls back to all eight canonical projection targets.
#[test]
fn selected_projection_targets_empty_config_falls_back_to_canonical_targets() {
    let got = selected_projection_targets_from_config(&CcyncConfig::default());
    let mut expected: Vec<String> = ccync_foundation::target::CANONICAL_PROJECTION_TARGETS
        .iter()
        .map(|t| t.as_str().to_string())
        .collect();
    expected.sort();
    expected.dedup();
    assert_eq!(got, expected);
}

// Regression: machine projection must honor the runtime selection folded into
// config.json by the data-layout flatten, not the removed install-state.json.
#[test]
fn selected_projection_targets_honor_config_selection() {
    let config = CcyncConfig {
        selected_runtimes: vec!["codex".into(), "claude".into()],
        ..Default::default()
    };
    let got = selected_projection_targets_from_config(&config);
    assert_eq!(got, vec!["claude".to_string(), "codex".to_string()]);
}

// Canonical-only selection (LegacyState::Canonical): the canonical
// `selectedProjectionTargets` key must be honored even though the legacy
// `selectedRuntimes` key is empty. Before this delegated to the foundation
// resolver, this function only ever read `selected_runtimes`, so a
// canonical-only config silently fell through to the default-all fallback —
// this fixture proves that bug is fixed.
#[test]
fn selected_projection_targets_honor_canonical_only_selection() {
    let config = CcyncConfig {
        selected_projection_targets: vec!["codex".into(), "claude".into()],
        ..Default::default()
    };
    let got = selected_projection_targets_from_config(&config);
    assert_eq!(got, vec!["claude".to_string(), "codex".to_string()]);
}

// Both keys present (LegacyState::Both): canonical and legacy hold
// genuinely different values, so a passing assertion proves canonical truly
// wins rather than the two fixtures happening to agree.
#[test]
fn selected_projection_targets_both_present_canonical_wins() {
    let config = CcyncConfig {
        selected_projection_targets: vec!["codex".into(), "claude".into()],
        selected_runtimes: vec!["agy-gui".into()],
        ..Default::default()
    };
    let got = selected_projection_targets_from_config(&config);
    assert_eq!(got, vec!["claude".to_string(), "codex".to_string()]);
}

// ManagedDeprojection authority (Protected Path) — registry-owned path
// authorized for removal; non-managed same-named sibling NOT authorized.
#[test]
fn managed_deprojection_authorizes_only_recorded_paths() {
    let tmp = TempDir::new().unwrap();
    let lock = tmp.path().join("build").join("lock.json");
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    let managed = tmp.path().join("skills").join("ccync");
    let sibling = tmp.path().join("skills").join("user-tool");

    // Record `managed` via the registry's own writer so the stored key uses the
    // exact same normalization the authority checks against.
    let mut report = ProjectionReport::default();
    let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
    registry.mark(MANAGED_SKILL_PATHS, &managed);
    registry.persist(&[], false, &mut report).unwrap();

    let authority = ManagedDeprojection::from_lockfile(&lock);
    assert!(!authority.is_empty());
    assert!(
        authority.is_authorized_for_removal(&managed),
        "registry-owned path must be authorized for removal"
    );
    assert!(
        !authority.is_authorized_for_removal(&sibling),
        "a non-managed same-named sibling must NOT be authorized"
    );
}

// Empty/missing prior authorizes nothing — proves is_managed semantics, NOT
// can_mutate's allow-all-on-empty-prior (which would delete user files during a
// common partial-state uninstall).
#[test]
fn managed_deprojection_empty_or_missing_prior_authorizes_nothing() {
    let tmp = TempDir::new().unwrap();
    let any = tmp.path().join("skills").join("ccync");

    let authority = ManagedDeprojection::from_lockfile(&tmp.path().join("nope.json"));
    assert!(authority.is_empty(), "missing lockfile → empty authority");
    assert!(
        !authority.is_authorized_for_removal(&any),
        "empty/missing prior must authorize nothing (no can_mutate allow-all)"
    );
}

fn fixture_roots() -> (TempDir, PathBuf, PathBuf, PathBuf) {
    let temp = TempDir::new().unwrap();
    let repo_root = temp.path().join("repo");
    let source_root = repo_root.join("plugins").join("ccync-core");
    let home = temp.path().join("home");
    fs::create_dir_all(source_root.join("skills").join("sample-skill")).unwrap();
    fs::create_dir_all(source_root.join("agents")).unwrap();
    fs::create_dir_all(source_root.join("commands")).unwrap();
    (temp, repo_root, source_root, home)
}

fn fixture_roots_with_appdata() -> (TempDir, PathBuf, PathBuf, PathBuf, PathBuf) {
    let (temp, repo_root, source_root, home) = fixture_roots();
    let appdata = temp.path().join("appdata");
    fs::create_dir_all(&appdata).unwrap();
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo_root)
        .output()
        .unwrap();
    (temp, repo_root, source_root, home, appdata)
}

fn write_skill_fixture(source_root: &Path, name: &str) {
    let skill_root = source_root.join("skills").join(name);
    fs::create_dir_all(&skill_root).unwrap();
    fs::write(skill_root.join("SKILL.md"), format!("# {name}\n")).unwrap();
}

fn write_command_fixture(source_root: &Path, name: &str) {
    let command_root = source_root.join("commands").join(name);
    fs::create_dir_all(&command_root).unwrap();
    fs::write(
        command_root.join("SKILL.template.md"),
        "---\ndescription: sample command\n---\nUse {{CCYNC_ROOT}}\n",
    )
    .unwrap();
    fs::write(command_root.join("SKILL.local.md"), "Local note\n").unwrap();
}

#[test]
fn update_skills_projects_links_and_agents() {
    // Projection materializes directory junctions (no privilege) and copies agent
    // files via `write_text` — it never creates per-file symlinks, so this needs no
    // file-symlink privilege guard (cf. `agents_projected_verbatim_with_codex_toml_and_no_discuss`).
    let (_temp, repo_root, source_root, home) = fixture_roots();
    fs::write(
        source_root
            .join("skills")
            .join("sample-skill")
            .join("SKILL.md"),
        "# Skill",
    )
    .unwrap();
    fs::write(
        source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\ncolor: green\ntools: [read, edit, execute]\n---\nBody\n",
    )
    .unwrap();

    let report = run_update_skills(&SkillUpdateOptions {
        repo_root: repo_root.clone(),
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec![
            "copilot".into(),
            "gemini-cli".into(),
            "agy-cli".into(),
            "opencode".into(),
        ],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(evidence_has_change(&report, ProjectionPathChange::Linked));
    assert!(home
        .join(".agents")
        .join("skills")
        .join("sample-skill")
        .exists());
    assert!(home
        .join(".copilot")
        .join("agents")
        .join("helper.agent.md")
        .exists());
    assert!(home.join(".copilot").join("ccync").exists());
    assert!(home.join(".gemini").join("ccync").exists());
    assert!(home
        .join(".gemini")
        .join("antigravity-cli")
        .join("ccync")
        .exists());
    let rendered = fs::read_to_string(
        home.join(".config")
            .join("opencode")
            .join("agents")
            .join("helper.md"),
    )
    .unwrap();
    assert!(rendered.starts_with(CCYNC_MANAGED_FILE_HEADER));
    assert!(rendered.contains("color: green"));
    assert!(rendered.contains("  read: allow"));
    assert!(rendered.contains("  list: allow"));
    assert!(rendered.contains("  edit: allow"));
    assert!(rendered.contains("  bash: allow"));
}

#[test]
fn agents_projected_verbatim_with_codex_toml_and_no_discuss() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    fs::write(
        source_root
            .join("skills")
            .join("sample-skill")
            .join("SKILL.md"),
        "# Skill",
    )
    .unwrap();
    // Agent with abstract tools + a frontmatter model. Generic projection must
    // NOT rewrite tools and must source the Codex model from the frontmatter.
    let raw = "---\nname: my-agent\ndescription: d\nmodel: gpt-5.4\ntools: [read, edit]\n---\nCharter body\n";
    fs::write(source_root.join("agents").join("my-agent.agent.md"), raw).unwrap();

    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["copilot".into(), "codex".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    // (1) Claude/copilot agent projected VERBATIM — abstract tools unchanged.
    let claude_agent = home
        .join(".copilot")
        .join("agents")
        .join("my-agent.agent.md");
    let projected = fs::read_to_string(&claude_agent).unwrap();
    assert_eq!(
        projected, raw,
        "Claude agent must be copied verbatim (no tool rewrite)"
    );
    assert!(
        projected.contains("tools: [read, edit]"),
        "abstract tools must NOT be rewritten"
    );

    // (2) Codex TOML projected, model sourced from frontmatter.
    let codex_toml = home.join(".codex").join("agents").join("my-agent.toml");
    let toml = fs::read_to_string(&codex_toml).unwrap();
    assert!(toml.contains("[agent]"));
    assert!(toml.contains("name = \"my-agent\""));
    assert!(
        toml.contains("model = \"gpt-5.4\""),
        "Codex model must come from frontmatter"
    );

    // (3) No discuss-skill residue.
    assert!(
        !home
            .join(".agents")
            .join("skills")
            .join("discuss-my-agent")
            .exists(),
        "no discuss-<role> skill must be projected"
    );
}

#[test]
fn update_skills_skips_shared_projection_without_codex_or_opencode() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    fs::write(
        source_root
            .join("skills")
            .join("sample-skill")
            .join("SKILL.md"),
        "# Skill",
    )
    .unwrap();

    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["copilot".into(), "gemini-cli".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(!home
        .join(".agents")
        .join("skills")
        .join("sample-skill")
        .exists());
}

#[test]
fn update_skills_gemini_cli_only_creates_no_gemini_ccync_source_link() {
    // gemini-cli is retired: the `~/.gemini/ccync` source link is re-gated on
    // agy-* selection alone. A gemini-cli-only selection must create neither
    // the link nor its parent `~/.gemini/` directory.
    let (_temp, repo_root, source_root, home) = fixture_roots();
    fs::write(
        source_root
            .join("skills")
            .join("sample-skill")
            .join("SKILL.md"),
        "# Skill",
    )
    .unwrap();

    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["gemini-cli".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(
        !home.join(".gemini").exists(),
        "gemini-cli-only selection must not create ~/.gemini or the ccync source link"
    );
}

// ── OpenCode agent write-side ownership regressions ─────────────────────────
//
// The OpenCode agents directory used to be published via a whole-directory
// staged swap: anything the user had placed in `~/.config/opencode/agents/`
// was silently deleted or overwritten. These regressions lock the guarded
// per-file replacement in place — absent-root non-creation, foreign
// survival, ownership-guarded collision handling, and atomic-failure safety.

#[test]
fn opencode_agent_publication_absent_root_does_not_create_agents_dir() {
    // Zero-agent fixture: the source's `agents/` dir is empty, so the desired
    // set is empty and the per-file write loop never runs. Unlike the prior
    // whole-directory swap (which staged/created the live directory
    // unconditionally), the guarded per-file loop must not create
    // `~/.config/opencode/agents` when nothing is ever written into it.
    let (_temp, repo_root, source_root, home) = fixture_roots();

    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(
        !home
            .join(".config")
            .join("opencode")
            .join("agents")
            .exists(),
        "zero desired agents must not create the OpenCode agents root"
    );
}

#[test]
fn opencode_agent_publication_zero_agents_preserves_foreign_sibling() {
    // Pre-existing foreign OpenCode agent, zero ccync agents desired: the
    // guarded per-file loop has nothing to write, so a pre-existing
    // user-owned sibling must survive byte-for-byte.
    let (_temp, repo_root, source_root, home) = fixture_roots();
    let opencode_agents = home.join(".config").join("opencode").join("agents");
    fs::create_dir_all(&opencode_agents).unwrap();
    let foreign = opencode_agents.join("foreign.md");
    fs::write(&foreign, "---\nname: foreign\n---\nnot ccync\n").unwrap();
    let before = fs::read(&foreign).unwrap();

    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert_eq!(
        fs::read(&foreign).unwrap(),
        before,
        "foreign OpenCode agent must survive byte-for-byte with zero ccync agents desired"
    );
}

#[test]
fn opencode_agent_publication_writes_authorized_agent_beside_foreign_sibling() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    fs::write(
        source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\ncolor: green\n---\nBody\n",
    )
    .unwrap();

    let opencode_agents = home.join(".config").join("opencode").join("agents");
    fs::create_dir_all(&opencode_agents).unwrap();
    let foreign = opencode_agents.join("foreign.md");
    fs::write(&foreign, "---\nname: foreign\n---\nnot ccync\n").unwrap();
    let before = fs::read(&foreign).unwrap();

    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    let helper = fs::read_to_string(opencode_agents.join("helper.md")).unwrap();
    assert!(helper.starts_with(CCYNC_MANAGED_FILE_HEADER));
    assert!(helper.contains("color: green"));
    assert_eq!(
        fs::read(&foreign).unwrap(),
        before,
        "a foreign sibling must remain untouched by an authorized write elsewhere in the same directory"
    );
}

#[test]
fn opencode_agent_publication_preserves_unproven_same_name_collision() {
    use crate::support::ManagedArtifactRegistry;

    let (_temp, repo_root, source_root, home) = fixture_roots();
    fs::write(
        source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\n---\nBody\n",
    )
    .unwrap();

    let opencode_agents = home.join(".config").join("opencode").join("agents");
    fs::create_dir_all(&opencode_agents).unwrap();
    let target = opencode_agents.join("helper.md");
    fs::write(&target, "user's own helper, not ccync-managed\n").unwrap();
    let before = fs::read(&target).unwrap();

    let evidence = run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert_eq!(
        fs::read(&target).unwrap(),
        before,
        "an unproven same-name target must be preserved, not overwritten"
    );
    assert!(
        evidence_all_warnings(&evidence)
            .iter()
            .any(|w| w.contains("preserved user-owned path")),
        "the collision must be surfaced as a warning"
    );

    let lockfile = home.join(".ccync").join("build").join("lock.json");
    let mut report = ProjectionReport::default();
    let registry = ManagedArtifactRegistry::load(&lockfile, &mut report);
    assert!(
        !registry.is_managed(&target),
        "an unproven collision must never be claimed as ccync-owned"
    );
}

#[test]
fn opencode_agent_publication_absent_source_collision_then_present_owner_writes() {
    // Round 1: source "owner-src" installs helper.agent.md; ownership recorded.
    let (_temp, repo_root, source_root, home) = fixture_roots();
    fs::write(
        source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\n---\nBody v1\n",
    )
    .unwrap();

    let owner_opts = SkillUpdateOptions {
        repo_root: repo_root.clone(),
        sources: vec![ProjectionSource {
            root: source_root.clone(),
            id: "owner-src".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    };
    run_update_skills(&owner_opts).unwrap();

    let target = home
        .join(".config")
        .join("opencode")
        .join("agents")
        .join("helper.md");
    assert!(fs::read_to_string(&target).unwrap().contains("Body v1"));

    // Round 2: a run where "owner-src" is no longer among installed sources —
    // a different source ("other-src") happens to publish an agent file with
    // the same file name and different content. Because the registry still
    // attributes `helper.md` to "owner-src", which is absent from this run's
    // installed set, the target is foreign to this invocation and must be
    // preserved untouched, regardless of the new content.
    let other_source_root = repo_root.join("plugins").join("other-src");
    fs::create_dir_all(other_source_root.join("agents")).unwrap();
    fs::write(
        other_source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\n---\nBody v2\n",
    )
    .unwrap();

    let foreign_run_opts = SkillUpdateOptions {
        repo_root: repo_root.clone(),
        sources: vec![ProjectionSource {
            root: other_source_root,
            id: "other-src".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    };
    let evidence = run_update_skills(&foreign_run_opts).unwrap();

    assert!(
        fs::read_to_string(&target).unwrap().contains("Body v1"),
        "a target owned by a source absent from this run's installed set must be preserved"
    );
    assert!(
        evidence_all_warnings(&evidence)
            .iter()
            .any(|w| w.contains("preserved foreign-owned OpenCode agent")),
        "the absent-source collision must be surfaced as a warning"
    );

    // Round 3 (present-owner control): re-run with "owner-src" installed again
    // and new content — this time the write must succeed, proving round 2's
    // block was specifically about the absent-source guard, not a permanent
    // lock on the target.
    fs::write(
        source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\n---\nBody v3\n",
    )
    .unwrap();
    run_update_skills(&owner_opts).unwrap();

    assert!(
        fs::read_to_string(&target).unwrap().contains("Body v3"),
        "a present-owner rerun must be able to update its own previously written target"
    );
}

#[test]
fn opencode_agent_publication_atomic_failure_leaves_no_registry_claim() {
    use crate::support::ManagedArtifactRegistry;

    let (_temp, repo_root, source_root, home) = fixture_roots();
    fs::write(
        source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\n---\nBody\n",
    )
    .unwrap();

    let opencode_agents = home.join(".config").join("opencode").join("agents");
    fs::create_dir_all(&opencode_agents).unwrap();
    // Deterministically block the atomic writer's own `<target>.tmp`
    // sibling by pre-occupying that exact path with a directory.
    fs::create_dir_all(opencode_agents.join("helper.md.tmp")).unwrap();

    let failure = run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap_err();

    assert!(matches!(failure.error, ProjectionError::Io(_)));

    let target = opencode_agents.join("helper.md");
    assert!(
        !target.exists(),
        "a failed atomic write must not leave a partially written target"
    );

    let lockfile = home.join(".ccync").join("build").join("lock.json");
    let mut report = ProjectionReport::default();
    let registry = ManagedArtifactRegistry::load(&lockfile, &mut report);
    assert!(
        !registry.is_managed(&target),
        "a target must never be claimed ccync-owned unless its write actually succeeded"
    );
}

// ── OpenCode agent prune/dry-run regressions ─────────────────────────────────
//
// The write-side guard (above) proves a single run never clobbers foreign
// content. These regressions prove the follow-on `prune_stale_links` call
// removes only what this run actually owns, leaves an absent-source stale
// entry alone, never runs at all when OpenCode is deselected, and reports
// accurate `Written`/`Removed`/warning evidence under `--dry-run` without
// touching disk.

#[test]
fn opencode_agent_publication_prunes_stale_ccync_agent_beside_foreign_content() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    fs::write(
        source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\n---\nBody\n",
    )
    .unwrap();

    let opencode_agents = home.join(".config").join("opencode").join("agents");
    fs::create_dir_all(&opencode_agents).unwrap();
    let foreign = opencode_agents.join("foreign.md");
    fs::write(&foreign, "---\nname: foreign\n---\nnot ccync\n").unwrap();
    let foreign_before = fs::read(&foreign).unwrap();

    let mut opts = SkillUpdateOptions {
        repo_root: repo_root.clone(),
        sources: vec![ProjectionSource {
            root: source_root.clone(),
            id: "ccync".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    };
    run_update_skills(&opts).unwrap();

    let helper = opencode_agents.join("helper.md");
    assert!(helper.exists(), "round 1 must publish the ccync agent");

    // Round 2: helper.agent.md is no longer part of the desired set — it must
    // be pruned as stale, while the foreign sibling survives untouched.
    fs::remove_file(source_root.join("agents").join("helper.agent.md")).unwrap();
    opts.sources = vec![ProjectionSource {
        root: source_root,
        id: "ccync".into(),
        persistent: true,
    }];
    run_update_skills(&opts).unwrap();

    assert!(
        !helper.exists(),
        "a stale ccync-owned OpenCode agent must be pruned once absent from the desired set"
    );
    assert_eq!(
        fs::read(&foreign).unwrap(),
        foreign_before,
        "a foreign sibling must survive stale-ccync pruning byte-for-byte"
    );
}

#[test]
fn opencode_agent_publication_preserves_absent_source_stale_entry() {
    // Round 1: "owner-src" installs helper.agent.md; ownership recorded.
    let (_temp, repo_root, source_root, home) = fixture_roots();
    fs::write(
        source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\n---\nBody\n",
    )
    .unwrap();

    run_update_skills(&SkillUpdateOptions {
        repo_root: repo_root.clone(),
        sources: vec![ProjectionSource {
            root: source_root,
            id: "owner-src".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    let target = home
        .join(".config")
        .join("opencode")
        .join("agents")
        .join("helper.md");
    let before = fs::read(&target).unwrap();

    // Round 2: "owner-src" is entirely absent from this run's installed
    // sources — a different source with zero agents of its own runs instead.
    // `helper.md` is not in the current desired set (stale), but it is
    // attributed to a source foreign to this invocation and must be
    // preserved rather than pruned.
    let other_source_root = repo_root.join("plugins").join("other-src");
    fs::create_dir_all(other_source_root.join("agents")).unwrap();

    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: other_source_root,
            id: "other-src".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert_eq!(
        fs::read(&target).unwrap(),
        before,
        "a stale entry owned by a source absent from this run's installed set must be preserved, not pruned"
    );
}

#[test]
fn opencode_agent_publication_deselection_preserves_byte_identity() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    fs::write(
        source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\n---\nBody\n",
    )
    .unwrap();

    run_update_skills(&SkillUpdateOptions {
        repo_root: repo_root.clone(),
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    let target = home
        .join(".config")
        .join("opencode")
        .join("agents")
        .join("helper.md");
    let before = fs::read(&target).unwrap();

    // Re-run with "opencode" absent from the selected targets: this hits the
    // early-return deselection branch before either the publish loop or the
    // prune call ever executes, so the previously published agent must
    // survive byte-for-byte.
    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["copilot".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert_eq!(
        fs::read(&target).unwrap(),
        before,
        "deselecting OpenCode must leave a previously published agent byte-identical"
    );
}

#[test]
fn opencode_agent_publication_dry_run_reports_written_removed_and_warning_without_mutation() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    fs::write(
        source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\n---\nBody\n",
    )
    .unwrap();

    let mut opts = SkillUpdateOptions {
        repo_root: repo_root.clone(),
        sources: vec![ProjectionSource {
            root: source_root.clone(),
            id: "ccync".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    };
    run_update_skills(&opts).unwrap();

    let opencode_agents = home.join(".config").join("opencode").join("agents");
    let helper = opencode_agents.join("helper.md");
    let helper_before = fs::read(&helper).unwrap();

    // Set up round 2's fixture: `helper.agent.md` goes stale (removed from
    // source), `another.agent.md` is a fresh desired agent with no
    // collision, and `newagent.agent.md` collides with a pre-existing
    // unproven user file at its target name.
    fs::remove_file(source_root.join("agents").join("helper.agent.md")).unwrap();
    fs::write(
        source_root.join("agents").join("another.agent.md"),
        "---\ndescription: another\n---\nBody\n",
    )
    .unwrap();
    fs::write(
        source_root.join("agents").join("newagent.agent.md"),
        "---\ndescription: newagent\n---\nBody\n",
    )
    .unwrap();
    let collision = opencode_agents.join("newagent.md");
    fs::write(&collision, "user's own newagent, not ccync-managed\n").unwrap();
    let collision_before = fs::read(&collision).unwrap();

    let another = opencode_agents.join("another.md");
    opts.dry_run = true;
    let evidence = run_update_skills(&opts).unwrap();

    let opencode_evidence = evidence
        .by_target
        .get(&ProjectionTargetId::Opencode)
        .expect("dry-run must still record OpenCode-scoped evidence");
    assert!(
        opencode_evidence
            .paths
            .iter()
            .any(|p| p.path == another && p.change == ProjectionPathChange::Written),
        "dry-run must preview writing the fresh, non-colliding agent"
    );
    assert!(
        opencode_evidence
            .paths
            .iter()
            .any(|p| p.path == helper && p.change == ProjectionPathChange::Removed),
        "dry-run must preview pruning the now-stale ccync agent"
    );
    assert!(
        opencode_evidence
            .warnings
            .iter()
            .any(|w| w.contains("preserved user-owned path")),
        "dry-run must still surface the unproven collision warning"
    );

    assert!(
        helper.exists(),
        "dry-run must not actually prune the stale agent from disk"
    );
    assert_eq!(
        fs::read(&helper).unwrap(),
        helper_before,
        "dry-run must not mutate the stale agent's bytes"
    );
    assert!(
        !another.exists(),
        "dry-run must not actually write the previewed fresh agent"
    );
    assert_eq!(
        fs::read(&collision).unwrap(),
        collision_before,
        "dry-run must not mutate the collision target's bytes"
    );
}

#[test]
fn healthcheck_warns_when_projection_missing() {
    let temp = TempDir::new().unwrap();
    let check = SkillsProjectionHealthCheck::with_path(temp.path().join("missing"));
    let findings = check.check();
    assert_eq!(findings.len(), 1);
    assert_eq!(
        findings[0].severity,
        ccync_foundation::health::Severity::Warning
    );
}

#[test]
fn healthcheck_accepts_existing_directory() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("skills");
    fs::create_dir_all(&path).unwrap();
    let check = SkillsProjectionHealthCheck::with_path(path);
    assert!(check.check().is_empty());
}

#[test]
fn render_opencode_agent_matches_legacy_shape() {
    let rendered = render_opencode_agent(
        "helper",
        &Frontmatter {
            description: Some("helper line 1\nhelper line 2".into()),
            color: Some("green".into()),
            tools: Vec::new(),
            model: None,
        },
        "Body",
    );

    assert_eq!(
            rendered,
            "# Generated by CCYNC Setup-Machine. Do not edit manually.\n---\ndescription: |\n  helper line 1\n  helper line 2\nmode: subagent\ncolor: green\npermission:\n  read: allow\n  list: allow\n---\n\nBody\n"
        );
}

#[test]
fn render_opencode_agent_expands_search_permissions() {
    let rendered = render_opencode_agent(
        "helper",
        &Frontmatter {
            description: Some("helper".into()),
            color: None,
            tools: vec!["search".into()],
            model: None,
        },
        "Body",
    );

    assert!(rendered.contains("  read: allow"));
    assert!(rendered.contains("  list: allow"));
    assert!(rendered.contains("  grep: allow"));
    assert!(rendered.contains("  glob: allow"));
}

#[test]
fn update_commands_bakes_and_projects_outputs() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "sample-command");

    let report = run_update_commands(&CommandUpdateOptions {
        repo_root: repo_root.clone(),
        sources: vec![ProjectionSource {
            root: source_root.clone(),
            id: "ccync-core".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["gemini-cli".into(), "codex".into(), "opencode".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    let baked_path = home
        .join(".ccync")
        .join("build")
        .join("render")
        .join("commands")
        .join("sample-command")
        .join("SKILL.md");
    let baked = fs::read_to_string(&baked_path).unwrap();
    assert!(baked.contains(&repo_root.display().to_string()));
    assert!(baked.contains("CCYNC LOCAL OVERRIDE START"));
    assert!(!source_root
        .join("commands")
        .join("sample-command")
        .join("SKILL.md")
        .exists());

    // Codex command skill now lives at the shared ~/.agents/skills (official
    // path), not the legacy ~/.codex/skills link.
    let codex_skill = home
        .join(".agents")
        .join("skills")
        .join("sample-command")
        .join("SKILL.md");
    assert!(
        codex_skill.exists(),
        "codex command projected as a shared skill"
    );
    assert!(
        !home
            .join(".codex")
            .join("skills")
            .join("sample-command")
            .exists(),
        "legacy ~/.codex/skills command target removed"
    );

    assert!(
        !home
            .join(".gemini")
            .join("commands")
            .join("sample-command.toml")
            .exists(),
        "gemini-cli command projection is retired; no .toml file is ever written, even when selected"
    );

    let opencode = fs::read_to_string(
        home.join(".config")
            .join("opencode")
            .join("commands")
            .join("sample-command.md"),
    )
    .unwrap();
    assert!(opencode.starts_with(CCYNC_MANAGED_FILE_HEADER));
    assert!(opencode.contains("User command arguments, if any: $ARGUMENTS"));
    assert!(evidence_has_change(&report, ProjectionPathChange::Written));
}

#[test]
fn update_commands_projects_copilot_command_skill() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "ccync-status");

    run_update_commands(&CommandUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["copilot".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    let skill = home
        .join(".copilot")
        .join("skills")
        .join("ccync-status")
        .join("SKILL.md");
    assert!(
        skill.exists(),
        "copilot command must be projected as a skill (~/.copilot/skills/<name>/SKILL.md)"
    );
    let body = fs::read_to_string(&skill).unwrap();
    assert!(
        body.contains("sample command"),
        "projected skill carries the baked command body"
    );
}

#[test]
fn update_commands_cleans_command_skill_when_runtime_deselected() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "ccync-status");

    run_update_commands(&CommandUpdateOptions {
        repo_root: repo_root.clone(),
        sources: vec![ProjectionSource {
            root: source_root.clone(),
            id: "ccync-core".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["codex".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();
    let shared = home.join(".agents").join("skills").join("ccync-status");
    assert!(shared.exists(), "codex command-skill written when selected");

    // Deselect codex: the CCYNC-written command-skill must be cleaned so it does
    // not linger in the shared dir and double-load for opencode/copilot.
    run_update_commands(&CommandUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["gemini-cli".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();
    assert!(
        !shared.exists(),
        "command-skill removed when its runtime is deselected"
    );
}

#[test]
fn update_commands_opencode_only_gets_no_shared_command_skill() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "ccync-status");

    run_update_commands(&CommandUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    // Double-load guard: opencode keeps its native command; the shared
    // ~/.agents/skills must NOT also carry a command-skill (that is gated on
    // codex being selected), so the same command does not appear twice.
    assert!(
        home.join(".config")
            .join("opencode")
            .join("commands")
            .join("ccync-status.md")
            .exists(),
        "opencode keeps its native command projection"
    );
    assert!(
        !home
            .join(".agents")
            .join("skills")
            .join("ccync-status")
            .exists(),
        "no shared command-skill when codex is not selected (no double-load)"
    );
}

#[test]
fn update_commands_removes_managed_outputs_for_unselected_runtimes() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "sample-command");

    let opencode_target = home
        .join(".config")
        .join("opencode")
        .join("commands")
        .join("sample-command.md");
    let claude_target = home
        .join(".claude")
        .join("commands")
        .join("sample-command.md");
    fs::create_dir_all(opencode_target.parent().unwrap()).unwrap();
    fs::create_dir_all(claude_target.parent().unwrap()).unwrap();
    fs::write(
        &opencode_target,
        format!("{CCYNC_MANAGED_FILE_HEADER}\nold\n"),
    )
    .unwrap();
    fs::write(
        &claude_target,
        format!("{CCYNC_MANAGED_FILE_HEADER}\nold\n"),
    )
    .unwrap();

    run_update_commands(&CommandUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["copilot".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(!opencode_target.exists());
    assert!(!claude_target.exists());
}

#[test]
fn update_commands_prunes_obsolete_managed_files() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "sample-command");

    // gemini-cli command projection is retired: a pre-existing `.toml` under
    // `~/.gemini/commands/` — however it got there — is a surface ccync no
    // longer manages at all, so it must be left byte-identical even with
    // gemini-cli selected. Only opencode's own stale-file pruning applies.
    let gemini_commands = home.join(".gemini").join("commands");
    let opencode_commands = home.join(".config").join("opencode").join("commands");
    fs::create_dir_all(&gemini_commands).unwrap();
    fs::create_dir_all(&opencode_commands).unwrap();
    let stale_gemini_content = format!("{CCYNC_MANAGED_FILE_HEADER}\nold\n");
    fs::write(
        gemini_commands.join("old-command.toml"),
        &stale_gemini_content,
    )
    .unwrap();
    fs::write(
        opencode_commands.join("old-command.md"),
        format!("{CCYNC_MANAGED_FILE_HEADER}\nold\n"),
    )
    .unwrap();

    run_update_commands(&CommandUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["gemini-cli".into(), "opencode".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert_eq!(
        fs::read_to_string(gemini_commands.join("old-command.toml")).unwrap(),
        stale_gemini_content,
        "gemini-cli command projection is retired; a pre-existing .toml is never pruned"
    );
    assert!(!gemini_commands.join("sample-command.toml").exists());
    assert!(!opencode_commands.join("old-command.md").exists());
    assert!(opencode_commands.join("sample-command.md").exists());
}

#[test]
fn update_commands_is_idempotent_and_preserves_untracked_tools() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "sample-command");

    let options = CommandUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    };

    let first_report = run_update_commands(&options).unwrap();
    assert!(
        evidence_has_change(&first_report, ProjectionPathChange::Written),
        "first sync should materialize managed artifacts"
    );

    let lockfile = home.join(".ccync").join("build").join("lock.json");
    let first_lockfile = fs::read_to_string(&lockfile).unwrap();
    assert!(
        first_lockfile.contains("\"_ccyncProjection\""),
        "projection marker must be persisted as the sync source of truth"
    );
    assert!(
        first_lockfile.contains("sample-command"),
        "managed command paths must be recorded in the projection marker"
    );

    let stale_managed_looking = home
        .join(".config")
        .join("opencode")
        .join("commands")
        .join("old-command.md");
    fs::write(
        &stale_managed_looking,
        format!("{CCYNC_MANAGED_FILE_HEADER}\nuser-owned\n"),
    )
    .unwrap();
    let unrelated_claude_tool = home.join(".claude").join("plugins").join("my-tool");
    fs::create_dir_all(&unrelated_claude_tool).unwrap();

    let second_report = run_update_commands(&options).unwrap();

    assert!(
        !evidence_has_change(&second_report, ProjectionPathChange::Written),
        "second sync should be idempotent when inputs do not change"
    );
    assert!(
        !evidence_has_change(&second_report, ProjectionPathChange::Removed),
        "second sync must not delete untracked surfaces"
    );
    assert!(
        evidence_all_warnings(&second_report).is_empty(),
        "stable rerun should not emit cleanup warnings"
    );
    assert!(
        stale_managed_looking.exists(),
        "files not tracked in plugins.lock.json must be preserved even if they look CCYNC-managed"
    );
    assert!(
        unrelated_claude_tool.exists(),
        "sync must not delete unrelated non-CCYNC Claude tools"
    );
    assert_eq!(
        fs::read_to_string(&lockfile).unwrap(),
        first_lockfile,
        "idempotent sync must leave the projection marker unchanged"
    );
}

#[test]
fn update_commands_preserves_user_owned_shared_skill_link() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "sample-command");

    let external_target = home.join("external-command");
    fs::create_dir_all(&external_target).unwrap();
    let shared_target = home.join(".agents").join("skills").join("sample-command");
    fs::create_dir_all(shared_target.parent().unwrap()).unwrap();
    create_dir_link(&external_target, &shared_target).unwrap();

    run_update_commands(&CommandUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["gemini-cli".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(shared_target.exists());
    assert_eq!(
        fs::canonicalize(&shared_target).unwrap(),
        fs::canonicalize(&external_target).unwrap()
    );
}

#[test]
fn update_commands_preserves_user_owned_shared_command_skill_dir() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "sample-command");

    // A genuine user directory at the shared skills target (same name as a
    // command) must be preserved, not clobbered by the command-skill write.
    let shared_target = home.join(".agents").join("skills").join("sample-command");
    fs::create_dir_all(&shared_target).unwrap();
    fs::write(shared_target.join("SKILL.md"), "user's own skill\n").unwrap();
    fs::write(shared_target.join("user.txt"), "keep").unwrap();

    let report = run_update_commands(&CommandUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["codex".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert_eq!(
        fs::read_to_string(shared_target.join("SKILL.md")).unwrap(),
        "user's own skill\n",
        "user's SKILL.md must not be overwritten"
    );
    assert!(shared_target.join("user.txt").exists());
    assert!(evidence_all_warnings(&report)
        .iter()
        .any(|warning| warning.contains("preserved user-owned path")));
}

#[test]
fn update_personalization_selecting_gemini_cli_writes_no_context_bridge_or_settings_merge() {
    let (_temp, repo_root, source_root, home, appdata) = fixture_roots_with_appdata();
    write_skill_fixture(&source_root, "sample-skill");
    fs::create_dir_all(
        repo_root
            .join("plugins")
            .join("ccync-core")
            .join("templates"),
    )
    .unwrap();
    fs::write(
        repo_root
            .join("plugins")
            .join("ccync-core")
            .join("templates")
            .join("executor-routing.example.json"),
        "{\n  \"coder\": \"gpt-5.4\"\n}\n",
    )
    .unwrap();
    fs::create_dir_all(home.join(".ccync").join("config")).unwrap();
    fs::write(
        home.join(".ccync").join("config").join("config.json"),
        "{\n  \"devMode\": true\n}\n",
    )
    .unwrap();
    fs::create_dir_all(home.join(".gemini")).unwrap();
    let settings_before = "{\n  \"context\": {\"fileName\": [\"README.md\"]}\n}\n";
    fs::write(home.join(".gemini").join("settings.json"), settings_before).unwrap();
    fs::create_dir_all(appdata.join("Code").join("User")).unwrap();

    run_update_personalization(&PersonalizationUpdateOptions {
        repo_root: repo_root.clone(),
        source_root: source_root.clone(),
        user_home: home.clone(),
        appdata_root: appdata.clone(),
        selected_projection_targets: vec!["copilot".into(), "gemini-cli".into()],
        dry_run: false,
        replace: true,
        uninstall: false,
    })
    .unwrap();

    assert!(
        !home.join(".gemini").join("ccync-context.md").exists(),
        "the Gemini context bridge must never be written, even with gemini-cli selected"
    );
    assert_eq!(
        fs::read_to_string(home.join(".gemini").join("settings.json")).unwrap(),
        settings_before,
        "~/.gemini/settings.json must be byte-identical — no merge is performed"
    );

    let vscode: Value = serde_json::from_str(
        &fs::read_to_string(appdata.join("Code").join("User").join("settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        vscode["chat.agentSkillsLocations"]["~/.agents/skills"],
        Value::Bool(false)
    );

    // D-08: routing-template copy + source-repo git config-filter registration
    // were removed (dead ccync-workflow); no longer asserted.
    let _ = &repo_root;
}

#[test]
fn update_personalization_uninstall_does_not_touch_gemini_settings_or_context() {
    let (_temp, repo_root, source_root, home, appdata) = fixture_roots_with_appdata();
    fs::create_dir_all(home.join(".gemini").join("antigravity-cli").join("plugins")).unwrap();
    fs::create_dir_all(home.join(".ccync").join("config")).unwrap();
    fs::write(
        home.join(".ccync").join("config").join("config.json"),
        "{\n  \"devMode\": true\n}\n",
    )
    .unwrap();
    fs::create_dir_all(home.join(".gemini")).unwrap();
    let settings_before = "{\n  \"context\": {\"fileName\": [\"README.md\"]}\n}\n";
    fs::write(home.join(".gemini").join("settings.json"), settings_before).unwrap();
    fs::create_dir_all(
        home.join(".gemini")
            .join("antigravity-cli")
            .join("plugins")
            .join("ccync"),
    )
    .unwrap();

    run_update_personalization(&PersonalizationUpdateOptions {
        repo_root,
        source_root,
        user_home: home.clone(),
        appdata_root: appdata,
        selected_projection_targets: vec!["gemini-cli".into(), "agy-cli".into()],
        dry_run: false,
        replace: true,
        uninstall: true,
    })
    .unwrap();

    assert!(!home.join(".gemini").join("ccync-context.md").exists());
    assert_eq!(
        fs::read_to_string(home.join(".gemini").join("settings.json")).unwrap(),
        settings_before,
        "uninstall must not touch ~/.gemini/settings.json"
    );
    // D-08: AGY plugin lifecycle is owned by install.rs `AgyProjection`, not
    // personalization — no longer asserted here.
}

#[test]
fn update_personalization_preserves_existing_vscode_skill_locations() {
    let (_temp, repo_root, source_root, home, appdata) = fixture_roots_with_appdata();
    write_skill_fixture(&source_root, "sample-skill");
    fs::create_dir_all(appdata.join("Code").join("User")).unwrap();
    fs::write(
        appdata.join("Code").join("User").join("settings.json"),
        "{\n  \"chat.agentSkillsLocations\": {\n    \"/custom/path\": true\n  }\n}\n",
    )
    .unwrap();

    run_update_personalization(&PersonalizationUpdateOptions {
        repo_root,
        source_root,
        user_home: home,
        appdata_root: appdata.clone(),
        selected_projection_targets: vec!["copilot".into()],
        dry_run: false,
        replace: true,
        uninstall: false,
    })
    .unwrap();

    let vscode: Value = serde_json::from_str(
        &fs::read_to_string(appdata.join("Code").join("User").join("settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        vscode["chat.agentSkillsLocations"]["/custom/path"],
        Value::Bool(true)
    );
    assert_eq!(
        vscode["chat.agentSkillsLocations"]["~/.agents/skills"],
        Value::Bool(false)
    );
}

#[test]
fn update_personalization_dry_run_does_not_write_agy_projection() {
    // AGY projection is owned by `crate::agy::AgyProjection`
    // inside `build_agy_plugin`, not a spawned build script. Dry-run must record
    // the would-be target without touching disk — assert the user-visible
    // invariant (the AGY install target is never created under --dry-run).
    let (_temp, repo_root, source_root, home, appdata) = fixture_roots_with_appdata();
    write_skill_fixture(&source_root, "sample-skill");
    fs::create_dir_all(home.join(".ccync").join("config")).unwrap();
    fs::write(
        home.join(".ccync").join("config").join("config.json"),
        "{\n  \"devMode\": true\n}\n",
    )
    .unwrap();

    let agy_install_target = home
        .join(".gemini")
        .join("antigravity-cli")
        .join("plugins")
        .join("ccync");

    run_update_personalization(&PersonalizationUpdateOptions {
        repo_root,
        source_root,
        user_home: home,
        appdata_root: appdata,
        selected_projection_targets: vec!["agy-cli".into()],
        dry_run: true,
        replace: true,
        uninstall: false,
    })
    .unwrap();

    assert!(
        !agy_install_target.exists(),
        "dry-run must not create the AGY projection target on disk"
    );
}

#[test]
fn personalization_runtime_dirs_gated_by_selection() {
    // Claude-only fixture: neither the Gemini nor the Antigravity agent-home
    // directory should be created; `~/.ccync` is unconditional.
    let (_temp, repo_root, source_root, home, appdata) = fixture_roots_with_appdata();
    write_skill_fixture(&source_root, "sample-skill");
    let gemini_root = home.join(".gemini");
    let antigravity_root = gemini_root.join("antigravity-cli");
    let ccync_home = home.join(".ccync");

    run_update_personalization(&PersonalizationUpdateOptions {
        repo_root,
        source_root,
        user_home: home,
        appdata_root: appdata,
        selected_projection_targets: vec!["claude".into()],
        dry_run: false,
        replace: true,
        uninstall: false,
    })
    .unwrap();

    assert!(
        !gemini_root.exists(),
        "Claude-only selection must not create the Gemini agent-home directory"
    );
    assert!(
        !antigravity_root.exists(),
        "Claude-only selection must not create the Antigravity agent-home directory"
    );
    assert!(
        ccync_home.is_dir(),
        "ccync_home must always be created regardless of runtime selection"
    );
}

#[test]
fn personalization_runtime_dirs_created_for_selected_runtime() {
    // agy-cli selected: both the Gemini agent-home directory and the
    // Antigravity agent-home directory are created — re-gated on agy-*
    // selection alone now that gemini-cli is retired.
    let (_temp, repo_root, source_root, home, appdata) = fixture_roots_with_appdata();
    write_skill_fixture(&source_root, "sample-skill");
    let gemini_root = home.join(".gemini");
    let antigravity_root = gemini_root.join("antigravity-cli");

    run_update_personalization(&PersonalizationUpdateOptions {
        repo_root,
        source_root,
        user_home: home,
        appdata_root: appdata,
        selected_projection_targets: vec!["agy-cli".into()],
        dry_run: false,
        replace: true,
        uninstall: false,
    })
    .unwrap();

    assert!(
        gemini_root.is_dir(),
        "agy-cli selection must create the Gemini agent-home directory"
    );
    assert!(
        antigravity_root.is_dir(),
        "agy-cli selection must create the Antigravity agent-home directory"
    );
}

#[test]
fn personalization_agent_home_dirs_not_created_for_gemini_cli_only_selection() {
    // gemini-cli is retired: even while the target still parses, selecting it
    // alone must not create the Gemini agent-home directory. Agent-home
    // creation is re-gated on agy-* selection alone.
    let (_temp, repo_root, source_root, home, appdata) = fixture_roots_with_appdata();
    write_skill_fixture(&source_root, "sample-skill");
    let gemini_root = home.join(".gemini");
    let antigravity_root = gemini_root.join("antigravity-cli");

    run_update_personalization(&PersonalizationUpdateOptions {
        repo_root,
        source_root,
        user_home: home,
        appdata_root: appdata,
        selected_projection_targets: vec!["gemini-cli".into()],
        dry_run: false,
        replace: true,
        uninstall: false,
    })
    .unwrap();

    assert!(
        !gemini_root.exists(),
        "gemini-cli-only selection must not create the Gemini agent-home directory"
    );
    assert!(
        !antigravity_root.exists(),
        "gemini-cli-only selection must not create the Antigravity agent-home directory"
    );
}

// ── MachineUpdateOptions::new / selected_projection_targets accessors ──────

#[test]
fn machine_update_options_new_getter_setter_round_trip_every_value() {
    let repo_root = PathBuf::from("/repo");
    let source_root = PathBuf::from("/repo/plugins/ccync-core");
    let state_root = PathBuf::from("/state");
    let user_home = PathBuf::from("/home/user");
    let appdata_root = PathBuf::from("/home/user/appdata");
    let initial_targets = vec!["claude".to_string(), "codex".to_string()];

    let mut opts = MachineUpdateOptions::new(
        repo_root.clone(),
        source_root.clone(),
        state_root.clone(),
        user_home.clone(),
        appdata_root.clone(),
        initial_targets.clone(),
        true,
        true,
        false,
        true,
    );

    // Every constructor value round-trips onto the struct exactly.
    assert_eq!(opts.repo_root, repo_root);
    assert_eq!(opts.source_root, source_root);
    assert_eq!(opts.state_root, state_root);
    assert_eq!(opts.user_home, user_home);
    assert_eq!(opts.appdata_root, appdata_root);
    assert_eq!(
        opts.selected_projection_targets(),
        initial_targets.as_slice()
    );
    assert_eq!(opts.selected_projection_targets, initial_targets);
    assert!(opts.dry_run);
    assert!(opts.replace);
    assert!(!opts.uninstall);
    assert!(opts.include_source_projections);

    let updated_targets = vec!["copilot".to_string(), "opencode".to_string()];
    opts.set_selected_projection_targets(updated_targets.clone());
    assert_eq!(
        opts.selected_projection_targets(),
        updated_targets.as_slice()
    );
    assert_eq!(opts.selected_projection_targets, updated_targets);

    // Every other option value is untouched by the setter.
    assert_eq!(opts.repo_root, repo_root);
    assert_eq!(opts.source_root, source_root);
    assert_eq!(opts.user_home, user_home);
    assert_eq!(opts.appdata_root, appdata_root);
    assert!(opts.dry_run);
    assert!(opts.replace);
    assert!(!opts.uninstall);
    assert!(opts.include_source_projections);
}

// The round-trip test above uses a single bool pattern (true, true, false,
// true) for (dry_run, replace, uninstall, include_source_projections), which
// leaves `dry_run`/`replace`/`include_source_projections` pairwise
// indistinguishable from each other (all three share the value `true`) — a
// positional mix-up between any two of them would go undetected. This case
// uses a complementary pattern so the two runs together resolve most of
// those pairs, and also exercises the empty-selection edge case: an empty
// `selected_projection_targets` must round-trip to an empty slice rather
// than silently retaining a stale or panicking value.
#[test]
fn machine_update_options_new_empty_selection_and_alternate_bool_pattern() {
    let repo_root = PathBuf::from("/repo2");
    let source_root = PathBuf::from("/repo2/plugins/ccync-core");
    let state_root = PathBuf::from("/state2");
    let user_home = PathBuf::from("/home/user2");
    let appdata_root = PathBuf::from("/home/user2/appdata");

    let opts = MachineUpdateOptions::new(
        repo_root.clone(),
        source_root.clone(),
        state_root.clone(),
        user_home.clone(),
        appdata_root.clone(),
        Vec::new(),
        false,
        false,
        true,
        false,
    );

    assert_eq!(opts.repo_root, repo_root);
    assert_eq!(opts.source_root, source_root);
    assert_eq!(opts.state_root, state_root);
    assert_eq!(opts.user_home, user_home);
    assert_eq!(opts.appdata_root, appdata_root);
    assert!(opts.selected_projection_targets().is_empty());
    assert!(opts.selected_projection_targets.is_empty());
    assert!(!opts.dry_run);
    assert!(!opts.replace);
    assert!(opts.uninstall);
    assert!(!opts.include_source_projections);
}

// ── planned_machine_touches (pure, read-only projection touch planner) ─────

// Builds a completely empty source tree (no skill/agent/command subdirectories)
// so `run_machine_update`'s per-item projection loops contribute nothing
// to the dry-run report — isolating exactly the content-independent live paths
// that `planned_machine_touches` must also enumerate purely from `opts`.
fn empty_machine_options(
    selected_projection_targets: Vec<String>,
) -> (TempDir, MachineUpdateOptions) {
    let temp = TempDir::new().unwrap();
    let repo_root = temp.path().join("repo");
    let source_root = repo_root.join("plugins").join("ccync-core");
    let home = temp.path().join("home");
    let appdata = temp.path().join("appdata");
    fs::create_dir_all(source_root.join("skills")).unwrap();
    fs::create_dir_all(source_root.join("agents")).unwrap();
    fs::create_dir_all(source_root.join("commands")).unwrap();

    let opts = MachineUpdateOptions {
        repo_root,
        source_root,
        state_root: home.join(".ccync").join("build"),
        user_home: home,
        appdata_root: appdata,
        selected_projection_targets,
        dry_run: true,
        replace: true,
        uninstall: false,
        include_source_projections: true,
    };
    (temp, opts)
}

fn observed_dry_run_touch_paths(opts: &MachineUpdateOptions) -> Vec<PathBuf> {
    let evidence = run_machine_update(opts).unwrap();
    let mut observed: Vec<PathBuf> = evidence_paths(&evidence)
        .filter(|p| {
            matches!(
                p.change,
                ProjectionPathChange::Written | ProjectionPathChange::Linked
            )
        })
        .map(|p| p.path.clone())
        .collect();
    observed.sort();
    observed.dedup();
    observed
}

#[test]
fn planned_machine_touches_matches_real_run_for_full_selection() {
    // Full canonical-target selection (all seven projection targets), run for
    // real (dry_run: false) rather than dry-run: `planned_machine_touches` is
    // dry-run-agnostic by contract. The Antigravity GUI plugin root is not in
    // this machine phase at all: the engine publishes or retires it and reports
    // it in `planned_engine_touches`, so neither `planned` nor `observed` may
    // carry it here. This fixture has zero OpenCode
    // agents, so OpenCode's per-file guarded write loop has nothing to publish
    // and touches no path at all — content-dependent per-agent writes are
    // deliberately out of scope for this content-independent planner (see its
    // own doc comment), so neither `planned` nor `observed` should carry an
    // OpenCode entry here.
    let (_temp, mut opts) = empty_machine_options(
        [
            "agy-cli", "agy-ide", "agy-gui", "claude", "codex", "copilot", "opencode",
        ]
        .into_iter()
        .map(String::from)
        .collect(),
    );
    assert_eq!(
        opts.selected_projection_targets.len(),
        ccync_foundation::target::CANONICAL_PROJECTION_TARGETS.len(),
        "fixture selection must cover the true canonical projection-target set, not a stale subset"
    );

    // Canonical root must exist for the whole-plugin junction targets (agy-*)
    // to resolve to a real directory.
    let canonical = opts.user_home.join(".ccync").join("build").join("render");
    fs::create_dir_all(&canonical).unwrap();
    opts.dry_run = false;

    let observed = observed_dry_run_touch_paths(&opts);

    let planned = planned_machine_touches(&opts);
    let mut planned_paths: Vec<PathBuf> = planned.iter().map(|(_, path)| path.clone()).collect();
    planned_paths.sort();
    planned_paths.dedup();

    assert_eq!(
        planned_paths, observed,
        "pure planner must exactly match the real run's touched path set"
    );
    assert!(
        !planned_paths.is_empty(),
        "sanity: the full-selection fixture must actually touch something"
    );

    for (runtime, _path) in &planned {
        assert!(
            runtime == "common"
                || runtime == "agy"
                || opts
                    .selected_projection_targets
                    .iter()
                    .any(|rt| rt == runtime),
            "planned touch labeled '{runtime}' must correspond to a selected runtime"
        );
    }

    let opencode_path = opts
        .user_home
        .join(".config")
        .join("opencode")
        .join("agents");
    assert!(
        !planned.iter().any(|(_, path)| path == &opencode_path),
        "the content-independent planner must not claim an OpenCode agents-directory touch \
         it cannot prove without per-agent content"
    );
    assert!(
        !observed.contains(&opencode_path),
        "a real run with opencode selected but zero agents must not touch the agents directory at all"
    );

    let agy_gui_junction = opts
        .user_home
        .join(".gemini")
        .join("config")
        .join("plugins")
        .join("ccync");
    assert!(
        !planned.iter().any(|(_, path)| path == &agy_gui_junction),
        "the machine planner must not claim the Antigravity GUI plugin root; the engine owns it"
    );
    assert!(
        !observed.contains(&agy_gui_junction),
        "a real machine-phase run with agy-gui selected must not write the GUI plugin root"
    );
}

#[test]
fn planned_machine_touches_excludes_unselected_runtime_paths() {
    // Claude-only selection: none of the gemini/copilot/agy-gated touches apply —
    // only the selection-independent lockfile marker remains, matching the
    // "selected runtimes only" gating already used for the same class of live surface.
    let (_temp, opts) = empty_machine_options(vec!["claude".to_string()]);

    let observed = observed_dry_run_touch_paths(&opts);
    let planned = planned_machine_touches(&opts);
    let mut planned_paths: Vec<PathBuf> = planned.iter().map(|(_, path)| path.clone()).collect();
    planned_paths.sort();

    assert_eq!(
        planned_paths, observed,
        "pure planner must exactly match the real dry-run report for a narrow selection"
    );
    assert_eq!(
        planned.len(),
        1,
        "claude-only selection must yield only the selection-independent lockfile touch"
    );
    assert_eq!(planned[0].0, "common");
    assert!(
        planned[0].1.ends_with(Path::new("build").join("lock.json")),
        "the sole claude-only touch must be the projection lockfile marker"
    );
}

#[test]
fn machine_update_claude_only_creates_no_unselected_agent_homes() {
    // FINDING-11-1/11-2 regression: a Claude-only `run_machine_update`
    // with a command AND a skill present used to `ensure_dir` `~/.gemini/commands`,
    // `~/.copilot/agents`, and `~/.config/opencode/commands` unconditionally —
    // creating agent homes for runtimes the user never selected and that the
    // first-projection consent gate never named. Pre-fix this test fails with
    // `~/.gemini exists = true`.
    let (_temp, mut opts) = empty_machine_options(vec!["claude".to_string()]);
    write_skill_fixture(&opts.source_root, "sample-skill");
    write_command_fixture(&opts.source_root, "sample-command");
    opts.dry_run = false;

    run_machine_update(&opts).unwrap();

    let home = &opts.user_home;
    assert!(
        !home.join(".gemini").exists(),
        "Claude-only run must not create the Gemini agent home"
    );
    assert!(
        !home.join(".copilot").exists(),
        "Claude-only run must not create the Copilot agent home"
    );
    assert!(
        !home.join(".config").join("opencode").exists(),
        "Claude-only run must not create the OpenCode agent home"
    );
}

#[test]
fn machine_update_gemini_cli_target_never_gets_command_projection() {
    // gemini-cli command projection is retired: even with gemini-cli selected
    // and a command present, `~/.gemini/commands` must never be created — no
    // evidence entry is attributed to a command file under `~/.gemini/`.
    let (_temp, mut opts) = empty_machine_options(vec!["gemini-cli".to_string()]);
    write_command_fixture(&opts.source_root, "sample-command");
    opts.dry_run = false;

    run_machine_update(&opts).unwrap();

    assert!(
        !opts.user_home.join(".gemini").join("commands").exists(),
        "gemini-cli command projection is retired; ~/.gemini/commands is never created"
    );
}

// ── MachineProjectionScope — direct delegation honored by machine writers ──

fn delegation_decision(
    component: DirectDelegationComponent,
    target: ProjectionTargetId,
    legacy_writer: &str,
    destination: PathBuf,
    outcome: DirectDelegationOutcome,
) -> DirectDelegationDecision {
    DirectDelegationDecision {
        producer: "ccync-core".into(),
        component,
        target,
        destination,
        legacy_writer: legacy_writer.into(),
        outcome,
    }
}

fn scope_with(decisions: Vec<DirectDelegationDecision>) -> MachineProjectionScope {
    MachineProjectionScope {
        decisions,
        unavailable: false,
    }
}

#[test]
fn machine_projection_scope_matches_only_overlapping_component_and_writer() {
    let home = PathBuf::from("home");
    let skill_file = home.join(".copilot/skills/sample-command/SKILL.md");
    let scope = scope_with(vec![delegation_decision(
        DirectDelegationComponent::CommandSkill,
        ProjectionTargetId::Copilot,
        "update_commands",
        skill_file.clone(),
        DirectDelegationOutcome::Blocked,
    )]);
    let skills_root = home.join(".copilot/skills");

    // The exact destination, its containing directory, and a path inside it overlap.
    assert!(scope.withholds(
        DirectDelegationComponent::CommandSkill,
        "update_commands",
        &skill_file
    ));
    assert!(scope.withholds(
        DirectDelegationComponent::CommandSkill,
        "update_commands",
        &home.join(".copilot/skills/sample-command")
    ));
    // A different component, a different writer, or a sibling entry does not overlap.
    assert!(!scope.withholds(
        DirectDelegationComponent::Skill,
        "update_commands",
        &skill_file
    ));
    assert!(!scope.withholds(
        DirectDelegationComponent::CommandSkill,
        "update_skills",
        &skill_file
    ));
    assert!(!scope.withholds(
        DirectDelegationComponent::CommandSkill,
        "update_commands",
        &home.join(".copilot/skills/other-command/SKILL.md")
    ));
    assert_eq!(
        scope.reserved_names(
            DirectDelegationComponent::CommandSkill,
            "update_commands",
            &skills_root
        ),
        vec!["sample-command".to_string()]
    );
    assert!(!scope.prune_withheld(
        DirectDelegationComponent::CommandSkill,
        "update_commands",
        &skills_root
    ));
    // A decision that covers the root itself stops pruning of that root.
    let root_scope = scope_with(vec![delegation_decision(
        DirectDelegationComponent::CommandSkill,
        ProjectionTargetId::Copilot,
        "update_commands",
        skills_root.clone(),
        DirectDelegationOutcome::Delegated,
    )]);
    assert!(root_scope.prune_withheld(
        DirectDelegationComponent::CommandSkill,
        "update_commands",
        &skills_root
    ));

    let unavailable = MachineProjectionScope {
        decisions: Vec::new(),
        unavailable: true,
    };
    assert!(unavailable.withholds(DirectDelegationComponent::Skill, "update_skills", &home));
    assert!(unavailable.prune_withheld(DirectDelegationComponent::Skill, "update_skills", &home));
    assert!(unavailable.withholds_tree(&home));

    let default_scope = MachineProjectionScope::default();
    assert!(!default_scope.withholds(DirectDelegationComponent::Skill, "update_skills", &home));
    assert!(!default_scope.withholds_tree(&home));
}

#[test]
fn machine_scope_withholds_blocked_copilot_command_skill_write() {
    let (_temp, mut opts) = empty_machine_options(vec!["copilot".to_string()]);
    write_command_fixture(&opts.source_root, "sample-command");
    opts.dry_run = false;
    let blocked = opts
        .user_home
        .join(".copilot")
        .join("skills")
        .join("sample-command")
        .join("SKILL.md");
    let scope = scope_with(vec![delegation_decision(
        DirectDelegationComponent::CommandSkill,
        ProjectionTargetId::Copilot,
        "update_commands",
        blocked.clone(),
        DirectDelegationOutcome::Blocked,
    )]);

    run_machine_update_with_scope(&opts, &scope).unwrap();
    assert!(
        !blocked.exists(),
        "a blocked direct destination must not receive a machine write"
    );

    // Control: the same fixture without a scope projects the command skill.
    run_machine_update(&opts).unwrap();
    assert!(
        blocked.exists(),
        "without a scope the machine projects the command skill"
    );
}

#[test]
fn machine_scope_withholds_blocked_shared_skill_link() {
    let (_temp, mut opts) = empty_machine_options(vec!["codex".to_string()]);
    write_skill_fixture(&opts.source_root, "sample-skill");
    opts.dry_run = false;
    let blocked = opts
        .user_home
        .join(".agents")
        .join("skills")
        .join("sample-skill");
    let scope = scope_with(vec![delegation_decision(
        DirectDelegationComponent::Skill,
        ProjectionTargetId::Codex,
        "update_skills",
        blocked.clone(),
        DirectDelegationOutcome::Blocked,
    )]);

    run_machine_update_with_scope(&opts, &scope).unwrap();
    assert!(
        !blocked.exists() && !is_symlink_or_junction(&blocked),
        "a blocked shared skill link must not be created"
    );

    run_machine_update(&opts).unwrap();
    assert!(
        is_symlink_or_junction(&blocked),
        "without a scope the machine links the shared skill"
    );
}

/// Machine fixture with a stale, machine-owned Claude native command at
/// `~/.claude/commands/old-command.md` that the current source no longer ships.
fn stale_claude_command_options() -> (TempDir, MachineUpdateOptions, PathBuf) {
    let (temp, mut opts) = empty_machine_options(vec!["claude".to_string()]);
    opts.dry_run = false;
    let claude_commands = opts.user_home.join(".claude").join("commands");
    fs::create_dir_all(&claude_commands).unwrap();
    let stale = claude_commands.join("old-command.md");
    fs::write(&stale, format!("{CCYNC_MANAGED_FILE_HEADER}\nold\n")).unwrap();
    (temp, opts, stale)
}

#[test]
fn machine_scope_keeps_delegated_claude_command_from_legacy_removal() {
    let (_temp, opts, stale) = stale_claude_command_options();
    let scope = scope_with(vec![delegation_decision(
        DirectDelegationComponent::Command,
        ProjectionTargetId::Claude,
        "update_commands",
        stale.clone(),
        DirectDelegationOutcome::Delegated,
    )]);

    run_machine_update_with_scope(&opts, &scope).unwrap();

    assert_eq!(
        fs::read_to_string(&stale).unwrap(),
        format!("{CCYNC_MANAGED_FILE_HEADER}\nold\n"),
        "a delegated Claude native command must survive legacy removal byte-identical"
    );
}

#[test]
fn machine_scope_absent_removes_stale_claude_command() {
    // Control for the delegated case: without a scope the stale command is pruned.
    let (_temp, opts, stale) = stale_claude_command_options();

    run_machine_update(&opts).unwrap();

    assert!(!stale.exists());
}

/// Machine fixture where `old-command` was projected as a Copilot command skill
/// by an earlier run and the source has since retired it.
fn retired_command_skill_options() -> (TempDir, MachineUpdateOptions, PathBuf) {
    let (temp, mut opts) = empty_machine_options(vec!["copilot".to_string()]);
    write_command_fixture(&opts.source_root, "old-command");
    opts.dry_run = false;
    run_machine_update(&opts).unwrap();
    let skill_file = opts
        .user_home
        .join(".copilot")
        .join("skills")
        .join("old-command")
        .join("SKILL.md");
    assert!(
        skill_file.exists(),
        "fixture: earlier run projects the skill"
    );
    fs::remove_dir_all(opts.source_root.join("commands").join("old-command")).unwrap();
    (temp, opts, skill_file)
}

#[test]
fn machine_scope_keeps_delegated_command_skill_from_prune() {
    let (_temp, opts, skill_file) = retired_command_skill_options();
    let scope = scope_with(vec![delegation_decision(
        DirectDelegationComponent::CommandSkill,
        ProjectionTargetId::Copilot,
        "update_commands",
        skill_file.clone(),
        DirectDelegationOutcome::Delegated,
    )]);

    run_machine_update_with_scope(&opts, &scope).unwrap();

    assert!(
        skill_file.exists(),
        "a delegated command skill must not be pruned by the machine"
    );
}

#[test]
fn machine_scope_absent_prunes_retired_command_skill_control() {
    let (_temp, opts, skill_file) = retired_command_skill_options();

    run_machine_update(&opts).unwrap();

    assert!(!skill_file.exists());
}

#[test]
fn machine_scope_unavailable_withholds_direct_writes_and_keeps_settings() {
    let (_temp, mut opts) = empty_machine_options(vec!["copilot".to_string()]);
    write_skill_fixture(&opts.source_root, "sample-skill");
    write_command_fixture(&opts.source_root, "sample-command");
    opts.dry_run = false;
    let scope = MachineProjectionScope {
        decisions: Vec::new(),
        unavailable: true,
    };

    let evidence = run_machine_update_with_scope(&opts, &scope).unwrap();

    assert!(
        !opts
            .user_home
            .join(".copilot")
            .join("skills")
            .join("sample-command")
            .join("SKILL.md")
            .exists(),
        "an unavailable scope withholds direct command skill writes"
    );
    assert!(
        evidence_all_warnings(&evidence)
            .iter()
            .any(|warning| warning.contains("scope unavailable")),
        "an unavailable scope is reported explicitly"
    );
    assert!(
        is_symlink_or_junction(&opts.user_home.join(".copilot").join("ccync")),
        "machine source links are not direct components and still update"
    );
    let settings = fs::read_to_string(
        opts.appdata_root
            .join("Code")
            .join("User")
            .join("settings.json"),
    )
    .unwrap();
    assert!(
        settings.contains("chat.agentSkillsLocations"),
        "personalization still updates under an unavailable scope"
    );
}

// ── run_machine_update — mid-run fault attribution ──

/// Fixture for `run_machine_update_mid_fault_preserves_exact_partial_attribution`:
/// a copilot-only source tree, plus a plain file sitting exactly where
/// `update_skills`'s copilot-agent block needs to create a directory
/// (`ensure_dir(&agents_target, ...)` inside the `has_target("copilot")`
/// block), so `fs::create_dir_all` faults mid-run with a real io error — an
/// unexpected fault, not an owned-path failure any branch already handles as
/// a value.
fn fault_mid_machine_options() -> (TempDir, MachineUpdateOptions) {
    let temp = TempDir::new().unwrap();
    let repo_root = temp.path().join("repo");
    let source_root = repo_root.join("plugins").join("ccync-core");
    let home = temp.path().join("home");
    let appdata = temp.path().join("appdata");
    fs::create_dir_all(source_root.join("skills")).unwrap();
    fs::create_dir_all(source_root.join("agents")).unwrap();
    fs::create_dir_all(source_root.join("commands")).unwrap();

    fs::create_dir_all(home.join(".copilot")).unwrap();
    fs::write(home.join(".copilot").join("agents"), "blocking file").unwrap();

    let opts = MachineUpdateOptions {
        repo_root,
        source_root,
        state_root: home.join(".ccync").join("build"),
        user_home: home,
        appdata_root: appdata,
        selected_projection_targets: vec!["copilot".to_string()],
        dry_run: false,
        replace: true,
        uninstall: false,
        include_source_projections: true,
    };
    (temp, opts)
}

// A mid-machine fault returns byte-equal partial evidence with exact
// common/target/warning attribution — proven by running the identical
// fixture twice, independently (separate temp roots so neither run can
// observe files the other already wrote), and asserting the two faults'
// relativized partial evidence are byte-equal to each other. This proves
// `run_machine_update`'s own partial-evidence contract is deterministic,
// not just that it happens to not crash.
#[test]
fn run_machine_update_mid_fault_preserves_exact_partial_attribution() {
    fn relativize(evidence: ProjectionEvidence, base: &Path) -> ProjectionEvidence {
        let fix_path = |p: PathBuf| p.strip_prefix(base).unwrap_or(&p).to_path_buf();
        let fix_result = |r: ProjectionPathResult| ProjectionPathResult {
            path: fix_path(r.path),
            change: r.change,
        };
        ProjectionEvidence {
            common: evidence.common.into_iter().map(fix_result).collect(),
            by_target: evidence
                .by_target
                .into_iter()
                .map(|(target, target_evidence)| {
                    (
                        target,
                        ProjectionTargetEvidence {
                            paths: target_evidence.paths.into_iter().map(fix_result).collect(),
                            ..target_evidence
                        },
                    )
                })
                .collect(),
            completed_targets: evidence.completed_targets,
            run_warnings: evidence.run_warnings,
        }
    }

    let (temp, opts) = fault_mid_machine_options();
    let failure = run_machine_update(&opts)
        .expect_err("a file blocking the copilot agents directory must fault, not succeed");

    // Independent reconstruction: a fresh fixture instance (separate temp
    // root, so this second run cannot observe files the first run already
    // wrote — `write_text`'s identical-content skip would otherwise make the
    // second run silently under-report).
    let (temp2, opts2) = fault_mid_machine_options();
    let failure2 = run_machine_update(&opts2)
        .expect_err("a file blocking the copilot agents directory must fault, not succeed");

    let actual = relativize(failure.partial, temp.path());
    let expected = relativize(failure2.partial, temp2.path());
    assert_eq!(
        actual, expected,
        "partial evidence at the fault must be byte-equal across two independent runs of the \
         identical fixture"
    );
    assert!(
        expected
            .by_target
            .contains_key(&ProjectionTargetId::Copilot),
        "sanity: the fixture must actually attribute real pre-fault work to the copilot target"
    );
    assert!(
        !expected.by_target[&ProjectionTargetId::Copilot]
            .paths
            .is_empty(),
        "sanity: copilot target evidence must contain at least the pre-fault ccync link"
    );
    assert!(
        expected.completed_targets.is_empty(),
        "sanity: a faulted run must not mark any target completed"
    );
}

#[test]
fn registry_mark_with_source_records_and_persists_attribution() {
    use crate::support::{ManagedArtifactRegistry, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let lockfile = tmp.path().join("plugins.lock.json");
    let path_a = tmp.path().join("skill-a");
    let path_b = tmp.path().join("skill-b");

    // Write an initial lockfile with no attribution so the first load is a no-attribution baseline.
    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);

    reg.mark(MANAGED_SKILL_PATHS, &path_a);
    reg.mark_with_source(MANAGED_SKILL_PATHS, &path_b, "my-bundle");
    reg.persist(&[], false, &mut report).unwrap();

    // Reload and verify attribution is present for path_b, absent for path_a.
    let reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    assert_eq!(
        reg2.owning_source_of(&path_b),
        Some("my-bundle"),
        "reload must preserve source attribution for mark_with_source path"
    );
    assert_eq!(
        reg2.owning_source_of(&path_a),
        None,
        "paths marked without source must have no attribution"
    );
}

// ── ManagedArtifactRegistry — personal plugin projection safety ──

/// Non-CCYNC paths are NOT in the registry → can_mutate() must return false
/// once a lockfile has been written (i.e. prior is non-empty).
/// This guarantees that personal plugin projection never deletes a user-owned
/// file at an unrelated path.
#[test]
fn registry_can_mutate_false_for_non_managed_path_with_populated_prior() {
    use crate::support::{ManagedArtifactRegistry, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let lockfile = tmp.path().join("plugins.lock.json");
    let ccync_managed_path = tmp.path().join("ccync-managed-skill");
    let user_path = tmp.path().join("user-owned-file");

    // Write a lockfile recording ccync_managed_path.
    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg.mark(MANAGED_SKILL_PATHS, &ccync_managed_path);
    reg.persist(&[], false, &mut report).unwrap();

    // Reload — prior is now non-empty.
    let reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);

    // CCYNC-managed path → can_mutate() = true.
    assert!(
        reg2.can_mutate(&ccync_managed_path),
        "CCYNC-managed path must be mutable"
    );
    // User-owned path (never in the registry) → can_mutate() = false.
    assert!(
        !reg2.can_mutate(&user_path),
        "non-CCYNC path must NOT be mutable (prune safety)"
    );
}

/// Personal plugin artifacts are marked via mark() during projection.
/// After persist + reload, those paths satisfy can_mutate() = true
/// (they are CCYNC-managed), while a different user path remains false.
#[test]
fn registry_personal_plugin_artifacts_are_managed_after_mark() {
    use crate::support::{ManagedArtifactRegistry, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let lockfile = tmp.path().join("plugins.lock.json");
    let personal_skill = tmp.path().join("personal-skill-projected");
    let unrelated_user_file = tmp.path().join("claude-plugins-external-never-touched");

    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    // Simulates the projection of a personal plugin skill artifact.
    reg.mark(MANAGED_SKILL_PATHS, &personal_skill);
    reg.persist(&[], false, &mut report).unwrap();

    let reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    assert!(
        reg2.can_mutate(&personal_skill),
        "personal plugin projected artifact must be mutable (CCYNC owns it)"
    );
    assert!(
        !reg2.can_mutate(&unrelated_user_file),
        "unrelated user file must NOT be mutable — prune must never delete it"
    );
}

#[test]
fn update_skills_projects_two_sources_to_shared_skills() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    // ccync-core: skill-a
    write_skill_fixture(&source_root, "skill-a");
    // bundle source: skill-b (different name → no collision)
    let bundle_root = _temp.path().join("bundle");
    fs::create_dir_all(bundle_root.join("skills").join("skill-b")).unwrap();
    fs::write(
        bundle_root.join("skills").join("skill-b").join("SKILL.md"),
        "# skill-b\n",
    )
    .unwrap();
    fs::create_dir_all(bundle_root.join("agents")).unwrap();

    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![
            ProjectionSource {
                root: source_root.clone(),
                id: "ccync-core".into(),
                persistent: true,
            },
            ProjectionSource {
                root: bundle_root,
                id: "my-bundle".into(),
                persistent: false,
            },
        ],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["codex".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    let shared = home.join(".agents").join("skills");
    assert!(
        shared.join("skill-a").exists(),
        "ccync-core skill must be projected"
    );
    assert!(
        shared.join("skill-b").exists(),
        "bundle skill must be projected"
    );
}

#[test]
fn update_skills_single_source_output_unchanged() {
    // Single-source (1-element Vec) must be byte-identical to pre-change behavior.
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "sample-skill");

    let report = run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["codex".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(home
        .join(".agents")
        .join("skills")
        .join("sample-skill")
        .exists());
    assert!(
        evidence_all_warnings(&report).is_empty(),
        "single-source must emit no warnings"
    );
}

#[test]
fn agy_gui_projects_whole_plugin_junction() {
    // The machine phase no longer publishes the agy-gui plugin root. The
    // engine publishes it, or retires it, in the canonical phase. The retired
    // per-skill decomposition and dedicated MCP writer stay retired too.
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "doc-sync");

    let canonical = home.join(".ccync").join("build").join("render");
    fs::create_dir_all(&canonical).unwrap();

    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["agy-gui".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    let gui_junction = home
        .join(".gemini")
        .join("config")
        .join("plugins")
        .join("ccync");
    assert!(
        gui_junction.symlink_metadata().is_err(),
        "update_skills must not publish the agy-gui plugin root at ~/.gemini/config/plugins/ccync"
    );

    let gui_skill = home
        .join(".gemini")
        .join("antigravity")
        .join("skills")
        .join("doc-sync");
    assert!(
        !gui_skill.exists(),
        "the retired per-skill decomposition must never write here again"
    );
    let mcp_config = home
        .join(".gemini")
        .join("antigravity")
        .join("mcp_config.json");
    assert!(
        !mcp_config.exists(),
        "the retired dedicated GUI mcp_config.json writer must never write here again"
    );
}

#[test]
fn agy_gui_not_selected_writes_no_gui_surface() {
    // Negative: when agy-gui is absent from the selected set neither the
    // junction nor any retired GUI surface is written.
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "doc-sync");

    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["codex".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    let gui_junction = home
        .join(".gemini")
        .join("config")
        .join("plugins")
        .join("ccync");
    assert!(
        !gui_junction.exists(),
        "no GUI junction without agy-gui selected"
    );
    let gui_skill = home
        .join(".gemini")
        .join("antigravity")
        .join("skills")
        .join("doc-sync");
    assert!(
        !gui_skill.exists(),
        "no GUI skill link without agy-gui selected"
    );
    let mcp_config = home
        .join(".gemini")
        .join("antigravity")
        .join("mcp_config.json");
    assert!(
        !mcp_config.exists(),
        "no GUI mcp_config.json without agy-gui selected"
    );
}

// A user-owned real directory at the agy-gui root must survive the machine
// phase. The engine owns that root now and reports its skip there, so
// `update_skills` must not touch it or record agy-gui evidence for it.
#[test]
fn agy_gui_preserves_user_owned_directory_and_reports_skipped_not_unchanged() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "doc-sync");

    let canonical = home.join(".ccync").join("build").join("render");
    fs::create_dir_all(&canonical).unwrap();

    let gui_target = home
        .join(".gemini")
        .join("config")
        .join("plugins")
        .join("ccync");
    fs::create_dir_all(&gui_target).unwrap();
    fs::write(gui_target.join("users-own-file.txt"), "do not touch").unwrap();

    let evidence = run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["agy-gui".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(
        gui_target.join("users-own-file.txt").exists(),
        "an unowned real directory must survive untouched"
    );

    // The unowned-directory report moved with the publish to the engine. The
    // machine phase neither publishes nor reports on the agy-gui root.
    let target_evidence = evidence.by_target.get(&ProjectionTargetId::AgyGui);
    assert!(
        target_evidence.map_or(true, |te| te.paths.is_empty() && te.skipped.is_none()),
        "update_skills must record no agy-gui publish or skip: {:?}",
        target_evidence
    );
}

// The machine phase publishes no agy-gui path on any run, first or repeat, so
// it can never report a spurious agy-gui change. The engine's GUI publisher
// records the change instead, and its `wrote` flag is covered in `agy.rs`.
#[test]
fn agy_gui_second_consecutive_publish_records_no_path_event() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "doc-sync");

    let canonical = home.join(".ccync").join("build").join("render");
    fs::create_dir_all(&canonical).unwrap();

    let options = SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["agy-gui".into()],
        dry_run: false,
        replace: true,
    };

    let gui_root = home
        .join(".gemini")
        .join("config")
        .join("plugins")
        .join("ccync");

    let first = run_update_skills(&options).unwrap();
    assert!(
        first
            .by_target
            .get(&ProjectionTargetId::AgyGui)
            .map_or(true, |t| t.paths.is_empty()),
        "the machine phase must record no agy-gui path event on the first run: {:?}",
        first.by_target.get(&ProjectionTargetId::AgyGui)
    );
    assert!(
        gui_root.symlink_metadata().is_err(),
        "the first machine-phase run must not create the agy-gui root"
    );

    let second = run_update_skills(&options).unwrap();
    assert!(
        second
            .by_target
            .get(&ProjectionTargetId::AgyGui)
            .map_or(true, |t| t.paths.is_empty()),
        "a repeat machine-phase run must record no agy-gui path event: {:?}",
        second.by_target.get(&ProjectionTargetId::AgyGui)
    );
}

// A deselected `agy-gui` root that was published as a link is unlinked, and
// its ownership row must go with it. A row whose artifact is gone is standing
// `remove_dir_all` authority over whatever later occupies that path, and
// nothing else collects it — `persist` re-serializes this category verbatim
// from disk. A surviving directory copy keeps its row on purpose: that row is
// the proof `ccync uninstall` needs to remove it later.
#[test]
fn agy_gui_deselection_releases_the_ownership_row_with_the_link() {
    use crate::support::AgyRootOwnership;

    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "doc-sync");

    let canonical = home.join(".ccync").join("build").join("render");
    fs::create_dir_all(&canonical).unwrap();

    let gui_target = home
        .join(".gemini")
        .join("config")
        .join("plugins")
        .join("ccync");
    fs::create_dir_all(gui_target.parent().unwrap()).unwrap();
    if create_dir_link(&canonical, &gui_target).is_err() {
        return; // host permits neither symlink nor junction here
    }

    let lockfile = home.join(".ccync").join("build").join("lock.json");
    let ownership = AgyRootOwnership::from_lockfile(&lockfile);
    ownership.record_now(&gui_target, false).unwrap();
    assert!(
        AgyRootOwnership::from_lockfile(&lockfile).is_owned(&gui_target),
        "precondition: the published link is recorded as owned"
    );

    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["codex".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(!gui_target.exists(), "the link itself must be unlinked");
    assert!(
        !AgyRootOwnership::from_lockfile(&lockfile).is_owned(&gui_target),
        "the row must not outlive the artifact it authorizes removing"
    );
}

#[test]
fn registry_attribution_round_trips_multiple_sources() {
    use crate::support::{ManagedArtifactRegistry, MANAGED_AGENT_PATHS, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let lockfile = tmp.path().join("plugins.lock.json");

    let paths = [
        (tmp.path().join("skill-core"), "ccync-core"),
        (tmp.path().join("skill-bundle"), "my-bundle"),
        (tmp.path().join("agent-bundle"), "my-bundle"),
    ];

    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);

    reg.mark_with_source(MANAGED_SKILL_PATHS, &paths[0].0, paths[0].1);
    reg.mark_with_source(MANAGED_SKILL_PATHS, &paths[1].0, paths[1].1);
    reg.mark_with_source(MANAGED_AGENT_PATHS, &paths[2].0, paths[2].1);
    reg.persist(&[], false, &mut report).unwrap();

    let reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    for (path, expected_source) in &paths {
        assert_eq!(
            reg2.owning_source_of(path),
            Some(*expected_source),
            "source attribution for {} must survive round-trip",
            path.display()
        );
    }
}

// persist() GC: an attribution row whose category row went missing (the path
// is no longer marked in any category) is dropped once its owning source is
// installed this invocation — an installed source has authority to judge its
// own rows stale. A row owned by a source absent from `installed_source_ids`
// (foreign to this invocation) survives untouched, matching the foreign-source
// protection `prune_stale_links` applies at prune time.
#[test]
fn registry_persist_gc_drops_orphan_attribution_for_installed_source_only() {
    use crate::support::{ManagedArtifactRegistry, MANAGED_AGENT_PATHS, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let lockfile = tmp.path().join("plugins.lock.json");
    let installed_path = tmp.path().join("skill-installed");
    let foreign_path = tmp.path().join("skill-foreign");

    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg.mark_with_source(MANAGED_SKILL_PATHS, &installed_path, "my-bundle");
    reg.mark_with_source(MANAGED_SKILL_PATHS, &foreign_path, "other-bundle");
    // Keep a row alive in a different category so the marker survives once
    // MANAGED_SKILL_PATHS is cleared below.
    reg.mark_with_source(
        MANAGED_AGENT_PATHS,
        &tmp.path().join("keep-alive.md"),
        "keep-alive",
    );
    reg.persist(&[], false, &mut report).unwrap();

    // Simulate both attribution rows becoming orphans: their category row
    // goes missing while attribution survives.
    let mut reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg2.replace_category(MANAGED_SKILL_PATHS);
    // "my-bundle" is installed this invocation, "other-bundle" is not.
    reg2.persist(&["my-bundle"], false, &mut report).unwrap();

    let reg3 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    assert_eq!(
        reg3.owning_source_of(&installed_path),
        None,
        "orphan row owned by an installed source must be GC'd on persist"
    );
    assert_eq!(
        reg3.owning_source_of(&foreign_path),
        Some("other-bundle"),
        "orphan row owned by a source foreign to this invocation must survive"
    );
}

// The projection marker (`_ccyncProjection`) must not be dropped merely
// because every managed-artifact category is empty — a retained attribution
// row (e.g. a foreign-source orphan the GC above preserves) still needs the
// marker present so it round-trips on the next load.
#[test]
fn registry_persist_retains_marker_when_only_attribution_remains() {
    use crate::support::{ManagedArtifactRegistry, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let lockfile = tmp.path().join("plugins.lock.json");
    let foreign_path = tmp.path().join("skill-foreign");

    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg.mark_with_source(MANAGED_SKILL_PATHS, &foreign_path, "other-bundle");
    reg.persist(&[], false, &mut report).unwrap();

    // Every category empties out, but the foreign-source attribution row
    // survives the GC (its owner is absent from `installed_source_ids`).
    let mut reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg2.replace_category(MANAGED_SKILL_PATHS);
    reg2.persist(&["my-bundle"], false, &mut report).unwrap();

    assert!(
        lockfile.exists(),
        "lockfile must still be written while an attribution row is retained"
    );
    let raw = fs::read_to_string(&lockfile).unwrap();
    assert!(
        raw.contains("_ccyncProjection"),
        "projection marker must be retained when a retained attribution row remains"
    );

    let reg3 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    assert_eq!(
        reg3.owning_source_of(&foreign_path),
        Some("other-bundle"),
        "retained attribution row must survive the round-trip"
    );
}

// Collision policy: core-wins + additive-only + warn
#[test]
fn update_skills_collision_core_wins_emits_warning() {
    // ccync-core and bundle both declare "shared-skill" → core item preserved, bundle skipped + warning.
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "shared-skill");
    write_skill_fixture(&source_root, "core-only");

    let bundle_root = _temp.path().join("bundle-collision");
    fs::create_dir_all(bundle_root.join("skills").join("shared-skill")).unwrap();
    fs::write(
        bundle_root
            .join("skills")
            .join("shared-skill")
            .join("SKILL.md"),
        "# bundle version\n",
    )
    .unwrap();
    fs::create_dir_all(bundle_root.join("agents")).unwrap();

    let report = run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![
            ProjectionSource {
                root: source_root.clone(),
                id: "ccync-core".into(),
                persistent: true,
            },
            ProjectionSource {
                root: bundle_root,
                id: "my-bundle".into(),
                persistent: false,
            },
        ],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["codex".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    // Core item must be projected.
    let shared_dir = home.join(".agents").join("skills").join("shared-skill");
    assert!(
        shared_dir.exists(),
        "core skill must be projected even when bundle collides"
    );

    // Exactly one warning must mention the collision.
    assert_eq!(
        report.run_warnings.len(),
        1,
        "exactly one collision warning expected"
    );
    let msg = &report.run_warnings[0];
    assert!(
        msg.contains("shared-skill") && msg.contains("my-bundle") && msg.contains("ccync-core"),
        "warning must name the skill and both sources; got: {msg}"
    );
}

#[test]
fn update_skills_collision_non_core_first_wins_emits_warning() {
    // Two non-core sources collide → first-loaded wins, second skipped + warning.
    let (_temp, repo_root, source_root, home) = fixture_roots();
    // ccync-core has no "conflict-skill"
    fs::create_dir_all(source_root.join("skills")).unwrap();
    fs::create_dir_all(source_root.join("agents")).unwrap();

    let bundle_a = _temp.path().join("bundle-a");
    fs::create_dir_all(bundle_a.join("skills").join("conflict-skill")).unwrap();
    fs::write(
        bundle_a
            .join("skills")
            .join("conflict-skill")
            .join("SKILL.md"),
        "# A\n",
    )
    .unwrap();
    fs::create_dir_all(bundle_a.join("agents")).unwrap();

    let bundle_b = _temp.path().join("bundle-b");
    fs::create_dir_all(bundle_b.join("skills").join("conflict-skill")).unwrap();
    fs::write(
        bundle_b
            .join("skills")
            .join("conflict-skill")
            .join("SKILL.md"),
        "# B\n",
    )
    .unwrap();
    fs::create_dir_all(bundle_b.join("agents")).unwrap();

    let report = run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![
            ProjectionSource {
                root: source_root.clone(),
                id: "ccync-core".into(),
                persistent: true,
            },
            ProjectionSource {
                root: bundle_a,
                id: "bundle-a".into(),
                persistent: false,
            },
            ProjectionSource {
                root: bundle_b,
                id: "bundle-b".into(),
                persistent: false,
            },
        ],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["codex".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    // First non-core source wins → skill exists.
    assert!(
        home.join(".agents")
            .join("skills")
            .join("conflict-skill")
            .exists(),
        "first non-core source skill must be projected"
    );

    // Exactly one warning for the second source collision.
    assert_eq!(
        report.run_warnings.len(),
        1,
        "exactly one collision warning expected"
    );
    let msg = &report.run_warnings[0];
    assert!(
        msg.contains("conflict-skill") && msg.contains("bundle-b") && msg.contains("bundle-a"),
        "warning must name skill and both non-core sources; got: {msg}"
    );
}

// Per-source prune safety: an owner present in `sources` (still installed,
// even mid-teardown, per the bundle-uninstall final-pass forward contract)
// is NOT foreign — a stale entry it owns falls through to normal
// authorization and is pruned, rather than being protected forever merely
// because its source is known.
#[test]
fn prune_stale_links_prunes_stale_item_owned_by_installed_source() {
    use crate::support::{prune_stale_links, ManagedArtifactRegistry, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();

    // Simulate two previously-projected skills: one from ccync-core, one from my-bundle.
    let core_skill = root.join("core-skill");
    let bundle_skill = root.join("bundle-skill");
    fs::create_dir_all(&core_skill).unwrap();
    fs::create_dir_all(&bundle_skill).unwrap();

    // Write a CCYNC-managed file inside the dir so is_ccync_owned_real_path authorizes removal.
    // Reference the constant (not a hardcoded string) so renaming the marker breaks
    // this fixture at compile time instead of silently desyncing.
    fs::write(
        bundle_skill.join("SKILL.md"),
        format!("{CCYNC_MANAGED_FILE_HEADER}\n# bundle-skill\n"),
    )
    .unwrap();

    let lockfile = tmp.path().join("lock.json");
    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg.mark_with_source(MANAGED_SKILL_PATHS, &core_skill, "ccync-core");
    reg.mark_with_source(MANAGED_SKILL_PATHS, &bundle_skill, "my-bundle");
    reg.persist(&[], false, &mut report).unwrap();

    // Reload (simulate a fresh invocation).
    let reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);

    // Only ccync-core in keep_names (bundle-skill not projected this invocation),
    // but my-bundle is still present in `sources` — its own stale item must be pruned.
    let sources = vec![
        ProjectionSource {
            root: PathBuf::new(),
            id: "ccync-core".into(),
            persistent: true,
        },
        ProjectionSource {
            root: PathBuf::new(),
            id: "my-bundle".into(),
            persistent: false,
        },
    ];
    prune_stale_links(&root, ["core-skill"], false, &reg2, &sources, None).unwrap();

    assert!(core_skill.exists(), "core skill in keep list must be kept");
    assert!(
        !bundle_skill.exists(),
        "stale item owned by a still-installed source must be pruned, not protected forever"
    );
}

// Per-source prune safety: an owner entirely absent from `sources` (foreign to
// this invocation) is protected even when normal authorization would otherwise
// allow removal — this invocation has no authority to judge whether it's stale.
#[test]
fn prune_stale_links_preserves_item_owned_by_foreign_source() {
    use crate::support::{prune_stale_links, ManagedArtifactRegistry, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();

    let core_skill = root.join("core-skill");
    let bundle_skill = root.join("bundle-skill");
    fs::create_dir_all(&core_skill).unwrap();
    fs::create_dir_all(&bundle_skill).unwrap();

    // Write a CCYNC-managed file inside the dir so is_ccync_owned_real_path authorizes removal.
    // Reference the constant (not a hardcoded string) so renaming the marker breaks
    // this fixture at compile time instead of silently desyncing.
    fs::write(
        bundle_skill.join("SKILL.md"),
        format!("{CCYNC_MANAGED_FILE_HEADER}\n# bundle-skill\n"),
    )
    .unwrap();

    let lockfile = tmp.path().join("lock.json");
    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg.mark_with_source(MANAGED_SKILL_PATHS, &core_skill, "ccync-core");
    reg.mark_with_source(MANAGED_SKILL_PATHS, &bundle_skill, "my-bundle");
    reg.persist(&[], false, &mut report).unwrap();

    let reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);

    // my-bundle is absent from `sources` entirely — foreign, so bundle-skill
    // must be preserved even though its CCYNC marker would otherwise authorize removal.
    let sources = vec![ProjectionSource {
        root: PathBuf::new(),
        id: "ccync-core".into(),
        persistent: true,
    }];
    prune_stale_links(&root, ["core-skill"], false, &reg2, &sources, None).unwrap();

    assert!(core_skill.exists(), "core skill must survive");
    assert!(
        bundle_skill.exists(),
        "item owned by a source foreign to this invocation must be preserved"
    );
}

// An orphan symlink whose category row went missing (e.g. lost to a
// prior GC bug) but whose attribution row survived is still pruned, live or
// dangling, once its target is proven to resolve inside the owning
// installed source's own root.
#[test]
fn prune_stale_links_orphan_symlink_with_in_root_attribution_is_pruned() {
    use crate::support::{
        prune_stale_links, ManagedArtifactRegistry, MANAGED_AGENT_PATHS, MANAGED_SKILL_PATHS,
    };

    let tmp = TempDir::new().unwrap();
    let owner_root = tmp.path().join("owned-source");
    fs::create_dir_all(&owner_root).unwrap();

    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();

    // Live orphan: the target still exists inside owner_root.
    let live_target = owner_root.join("live-target");
    fs::create_dir_all(&live_target).unwrap();
    let live_link = root.join("live-skill");
    create_dir_link(&live_target, &live_link).unwrap();

    // Dangling orphan: the target existed inside owner_root at link creation
    // time, then was removed — the link's own stored path still resolves
    // inside the root even though the target no longer exists.
    let dangling_target = owner_root.join("dangling-target");
    fs::create_dir_all(&dangling_target).unwrap();
    let dangling_link = root.join("dangling-skill");
    create_dir_link(&dangling_target, &dangling_link).unwrap();
    fs::remove_dir_all(&dangling_target).unwrap();

    let lockfile = tmp.path().join("lock.json");
    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg.mark_with_source(MANAGED_SKILL_PATHS, &live_link, "my-bundle");
    reg.mark_with_source(MANAGED_SKILL_PATHS, &dangling_link, "my-bundle");
    // Keep a row alive in a different category, untouched by the
    // `replace_category` below — `persist` drops the whole marker (and with
    // it every attribution row) once all categories go empty, which would
    // silently defeat this test's premise rather than exercise it.
    reg.mark_with_source(
        MANAGED_AGENT_PATHS,
        &tmp.path().join("keep-alive.md"),
        "keep-alive",
    );
    reg.persist(&[], false, &mut report).unwrap();

    // Simulate the category row going missing while attribution survives.
    let mut reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg2.replace_category(MANAGED_SKILL_PATHS);
    reg2.persist(&[], false, &mut report).unwrap();

    let reg3 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    let sources = vec![ProjectionSource {
        root: owner_root.clone(),
        id: "my-bundle".into(),
        persistent: false,
    }];

    prune_stale_links(&root, [], false, &reg3, &sources, None).unwrap();

    assert!(
        fs::symlink_metadata(&live_link).is_err(),
        "live orphan symlink with in-root attributed target must be pruned"
    );
    assert!(
        fs::symlink_metadata(&dangling_link).is_err(),
        "dangling orphan symlink with in-root attributed target must be pruned"
    );
}

// The same missing-category-row-plus-attribution setup as the preceding
// in-root-orphan test must NOT authorize removal of a real (non-symlink)
// directory, a live symlink whose target resolves outside the owner's root,
// or a dangling symlink whose stored target carries a `..` (ParentDir)
// component — none of these prove ownership of an in-root artifact, so all
// three are preserved.
#[test]
fn prune_stale_links_preserves_real_dir_external_target_and_parent_dir_target() {
    use crate::support::{
        prune_stale_links, ManagedArtifactRegistry, MANAGED_AGENT_PATHS, MANAGED_SKILL_PATHS,
    };

    let tmp = TempDir::new().unwrap();
    let owner_root = tmp.path().join("owned-source");
    fs::create_dir_all(&owner_root).unwrap();
    let outside_dir = tmp.path().join("outside-source");
    fs::create_dir_all(&outside_dir).unwrap();

    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();

    // Case 1: a real (non-symlink) directory — never authorized via the
    // symlink-only source-root path.
    let real_dir = root.join("real-skill");
    fs::create_dir_all(&real_dir).unwrap();

    // Case 2: a live symlink whose target lives outside owner_root.
    let external_target = outside_dir.join("external-target");
    fs::create_dir_all(&external_target).unwrap();
    let external_link = root.join("external-skill");
    create_dir_link(&external_target, &external_link).unwrap();

    let lockfile = tmp.path().join("lock.json");
    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg.mark_with_source(MANAGED_SKILL_PATHS, &real_dir, "my-bundle");
    reg.mark_with_source(MANAGED_SKILL_PATHS, &external_link, "my-bundle");
    // Keep a row alive in a different category so `persist` retains the
    // marker (and the attribution rows under it) once MANAGED_SKILL_PATHS
    // is cleared below — see the matching comment in the in-root-orphan test above.
    reg.mark_with_source(
        MANAGED_AGENT_PATHS,
        &tmp.path().join("keep-alive.md"),
        "keep-alive",
    );
    reg.persist(&[], false, &mut report).unwrap();

    let mut reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg2.replace_category(MANAGED_SKILL_PATHS);
    reg2.persist(&[], false, &mut report).unwrap();

    let reg3 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    let sources = vec![ProjectionSource {
        root: owner_root.clone(),
        id: "my-bundle".into(),
        persistent: false,
    }];

    prune_stale_links(&root, [], false, &reg3, &sources, None).unwrap();

    assert!(
        real_dir.exists(),
        "a real directory must never be authorized via the symlink-only source-root path"
    );
    assert!(
        fs::symlink_metadata(&external_link).is_ok(),
        "a live symlink resolving outside the owner's root must be preserved"
    );

    // Case 3: a dangling symlink whose raw stored target contains `..` —
    // exercised directly against the target-proof helper, since Windows
    // junctions (the unprivileged reparse mechanism this crate relies on
    // elsewhere) always store an absolute, already-resolved target and
    // cannot carry a literal `..` component to round-trip through
    // `read_link`. A real symlink can, but creating one on Windows needs
    // Developer Mode or admin — skip gracefully when unavailable, matching
    // the existing privilege-aware pattern in this workspace.
    let parent_dir_link = root.join("parent-dir-skill");
    let traversal_target = owner_root
        .join("..")
        .join("owned-source")
        .join("live-target");
    let made_link = {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&traversal_target, &parent_dir_link).is_ok()
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(&traversal_target, &parent_dir_link).is_ok()
        }
    };
    if made_link {
        let stored_target = fs::read_link(&parent_dir_link).unwrap();
        if stored_target
            .components()
            .any(|component| component == std::path::Component::ParentDir)
        {
            assert!(
                !crate::support::is_symlink_target_inside_root(&parent_dir_link, &owner_root),
                "a dangling target containing a ParentDir component must never prove in-root ownership"
            );
        } else {
            println!("NotRun: this host normalized the ParentDir target at link creation");
        }
    }
}

// Source assembly: collect_projection_sources returns ccync-core 1-element list by default
#[test]
fn collect_projection_sources_returns_ccync_core_single_source() {
    use crate::collect_projection_sources;

    let tmp = TempDir::new().unwrap();
    let source_root = tmp.path().to_path_buf();

    let sources = collect_projection_sources(&source_root);
    assert_eq!(
        sources.len(),
        1,
        "default assembly must return exactly 1 source"
    );
    assert_eq!(sources[0].id, "ccync");
    assert_eq!(sources[0].root, source_root);
    assert!(sources[0].persistent, "ccync source must be persistent");
}

// Verifies that persist() does not write ccync-self.json when the registry is empty
// (byte-identical guarantee for the no-writer stage). An empty registry fires the
// equality guard (prior == next, both empty BTreeMaps) and returns early without
// calling write_text — the file stays absent.
#[test]
fn registry_empty_ccync_self_no_write_on_persist() {
    use crate::support::ManagedArtifactRegistry;

    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("ccync-self.json");

    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&path, &mut report);
    reg.persist(&[], false, &mut report).unwrap();

    assert!(
        !path.exists(),
        "empty ccync-self registry must not write ccync-self.json (persist() non-empty guard)"
    );
}

// Interleaved coexistence: two registries at distinct lockfile paths (one
// plugins.lock.json-like for "ccync", one ccync-self.json for "ccync-self") share a
// projection output directory. Each registry owns a disjoint subset of artifacts
// via mark_with_source with a distinct source-id. Prune from each registry's side
// must not delete the other registry's artifacts, and a non-CCYNC external path must
// be untouched by both prune operations.
#[test]
fn registry_interleaved_coexistence_isolation() {
    use crate::support::{prune_stale_links, ManagedArtifactRegistry, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let shared_dir = tmp.path().join("skills");
    fs::create_dir_all(&shared_dir).unwrap();

    // Three entries in the shared output directory — directories (no CCYNC header)
    // so is_ccync_owned_real_path cannot authorize accidental removal.
    let ccync_artifact = shared_dir.join("ccync-skill");
    let ccync_self_artifact = shared_dir.join("ccync-self-skill");
    let external_path = shared_dir.join("user-external");
    fs::create_dir_all(&ccync_artifact).unwrap();
    fs::create_dir_all(&ccync_self_artifact).unwrap();
    fs::create_dir_all(&external_path).unwrap();

    let mut report = ProjectionReport::default();

    // Registry A (plugins.lock.json-like): records ccync-artifact as owned by "ccync".
    let lock_a = tmp.path().join("plugins.lock.json");
    let mut reg_a = ManagedArtifactRegistry::load(&lock_a, &mut report);
    reg_a.mark_with_source(MANAGED_SKILL_PATHS, &ccync_artifact, "ccync");
    reg_a.persist(&[], false, &mut report).unwrap();

    // Registry B (ccync-self.json): records ccync-self-artifact as owned by "ccync-self".
    let lock_b = tmp.path().join("ccync-self.json");
    let mut reg_b = ManagedArtifactRegistry::load(&lock_b, &mut report);
    reg_b.mark_with_source(MANAGED_SKILL_PATHS, &ccync_self_artifact, "ccync-self");
    reg_b.persist(&[], false, &mut report).unwrap();

    // Reload both registries to simulate a fresh invocation on each side.
    let reg_a2 = ManagedArtifactRegistry::load(&lock_a, &mut report);
    let reg_b2 = ManagedArtifactRegistry::load(&lock_b, &mut report);

    // --- Prune from registry A's perspective (source "ccync" installed) ---
    // ccync-skill is in keep_names → always kept.
    // ccync-self-skill is NOT in reg_a's prior → can_mutate = false → not authorized.
    // external_path is not in any registry → can_mutate = false → not authorized.
    prune_stale_links(
        &shared_dir,
        ["ccync-skill"],
        false,
        &reg_a2,
        &[ProjectionSource {
            root: PathBuf::new(),
            id: "ccync".into(),
            persistent: true,
        }],
        None,
    )
    .unwrap();

    assert!(
        ccync_artifact.exists(),
        "ccync artifact must survive its own registry prune"
    );
    assert!(
        ccync_self_artifact.exists(),
        "ccync-self artifact must NOT be deleted by ccync-side prune (absent from ccync registry)"
    );
    assert!(
        external_path.exists(),
        "external non-CCYNC path must be untouched by ccync-side prune"
    );

    // --- Prune from registry B's perspective (source "ccync-self" installed) ---
    // ccync-self-skill is in keep_names → always kept.
    // ccync-skill is NOT in reg_b's prior → can_mutate = false → not authorized.
    // external_path is not in any registry → can_mutate = false → not authorized.
    prune_stale_links(
        &shared_dir,
        ["ccync-self-skill"],
        false,
        &reg_b2,
        &[ProjectionSource {
            root: PathBuf::new(),
            id: "ccync-self".into(),
            persistent: true,
        }],
        None,
    )
    .unwrap();

    assert!(
        ccync_self_artifact.exists(),
        "ccync-self artifact must survive its own registry prune"
    );
    assert!(
            ccync_artifact.exists(),
            "ccync artifact must NOT be deleted by ccync-self-side prune (absent from ccync-self registry)"
        );
    assert!(
        external_path.exists(),
        "external non-CCYNC path must be untouched by ccync-self-side prune"
    );
}

// ── decompose_plugin (projection-input materialization) ────────────────────

#[test]
fn decompose_plugin_yields_component_lists() {
    let tmp = TempDir::new().unwrap();
    let plugin = tmp.path().join("my-plugin");
    // skills/<name>/SKILL.md
    for s in ["doc-sync", "git-commits"] {
        let d = plugin.join("skills").join(s);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("SKILL.md"), "x").unwrap();
    }
    // commands/<name>/
    fs::create_dir_all(plugin.join("commands").join("ccync-status")).unwrap();
    // agents/<name>.agent.md
    fs::create_dir_all(plugin.join("agents")).unwrap();
    fs::write(plugin.join("agents").join("my-architect.agent.md"), "x").unwrap();
    // .mcp.json
    fs::write(
        plugin.join(".mcp.json"),
        r#"{"servers":{"memory":{},"fetch":{}}}"#,
    )
    .unwrap();

    let c = decompose_plugin(&plugin).unwrap();
    assert_eq!(c.skills, vec!["doc-sync", "git-commits"]);
    assert_eq!(c.commands, vec!["ccync-status"]);
    assert_eq!(c.agents, vec!["my-architect.agent.md"]);
    assert_eq!(c.mcp_servers, vec!["fetch", "memory"]); // sorted
}

#[test]
fn decompose_plugin_absent_subtrees_are_empty() {
    let tmp = TempDir::new().unwrap();
    let plugin = tmp.path().join("empty-plugin");
    fs::create_dir_all(&plugin).unwrap();
    let c = decompose_plugin(&plugin).unwrap();
    assert!(c.skills.is_empty());
    assert!(c.commands.is_empty());
    assert!(c.agents.is_empty());
    assert!(c.mcp_servers.is_empty());
}

// ── single-mode resolve_projection_roots (D-07) ─────────────────────────────

#[test]
fn resolve_projection_roots_returns_canonical_root_no_err() {
    // Only env-mutating test in this crate's test binary (per-crate process).
    let tmp = TempDir::new().unwrap();
    #[cfg(windows)]
    unsafe {
        std::env::set_var("USERPROFILE", tmp.path());
    }
    #[cfg(not(windows))]
    unsafe {
        std::env::set_var("HOME", tmp.path());
    }

    // Single mode: no ccync-core on disk, default config → must NOT error and
    // must return the ccync canonical plugin root for both roots.
    let (repo_root, source_root) = resolve_projection_roots(&CcyncConfig::default()).unwrap();
    assert_eq!(repo_root, source_root);
    assert!(
        repo_root.ends_with(Path::new("build").join("render")),
        "resolve_projection_roots must return ~/.ccync/build/render, got {repo_root:?}"
    );
}

// ── name-agnostic ownership regression (option B) ──────────────────────────────

#[test]
fn arbitrary_name_agent_managed_set_membership_allows_prune() {
    use crate::support::{prune_stale_links, ManagedArtifactRegistry, MANAGED_AGENT_PATHS};

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("agents");
    fs::create_dir_all(&root).unwrap();

    let managed_agent = root.join("my-tool.agent.md");
    let user_agent = root.join("user-agent.agent.md");

    fs::write(&managed_agent, "---\nname: my-tool\n---\nbody\n").unwrap();
    fs::write(&user_agent, "---\nname: user-tool\n---\nbody\n").unwrap();

    let lockfile = tmp.path().join("build").join("lock.json");
    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg.mark(MANAGED_AGENT_PATHS, &managed_agent);
    reg.persist(&[], false, &mut report).unwrap();

    let reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    prune_stale_links(&root, [], false, &reg2, &[], None).unwrap();

    assert!(
        !managed_agent.exists(),
        "managed-set member must be pruned when no longer active (option B)"
    );
    assert!(
        user_agent.exists(),
        "non-managed user-owned agent must be preserved"
    );
}

#[test]
fn arbitrary_name_agent_first_run_preserve_user_file() {
    use crate::support::{prune_stale_links, ManagedArtifactRegistry};

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("agents");
    fs::create_dir_all(&root).unwrap();

    let user_agent = root.join("my-custom-agent.agent.md");
    fs::write(&user_agent, "---\nname: my-custom-agent\n---\nbody\n").unwrap();

    let mut report = ProjectionReport::default();
    let registry = ManagedArtifactRegistry::load(&tmp.path().join("nolock.json"), &mut report);

    prune_stale_links(&root, [], false, &registry, &[], None).unwrap();

    assert!(
        user_agent.exists(),
        "user-owned agent must survive first-run prune (empty prior lockfile)"
    );
}

// ── evidence-shaped residual producers (skills/commands/personalization) ────

#[test]
fn update_skills_with_evidence_attributes_copilot_link_to_owner() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "sample-skill");

    let evidence = run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["copilot".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    let copilot = evidence
        .by_target
        .get(&ProjectionTargetId::Copilot)
        .expect("copilot must own the ~/.copilot/ccync junction");
    assert!(copilot.paths.contains(&ProjectionPathResult {
        path: home.join(".copilot").join("ccync"),
        change: ProjectionPathChange::Linked,
    }));
    assert!(
        !evidence.by_target.contains_key(&ProjectionTargetId::Codex),
        "codex was not selected and produced no evidence, so it must have no entry at all"
    );
}

#[test]
fn update_skills_with_evidence_shared_skills_surface_falls_back_to_common() {
    // codex/opencode share `~/.agents/skills` — not uniquely owned by one
    // target, so the producer must attribute it to `common`.
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "sample-skill");

    let evidence = run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["codex".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(evidence.common.contains(&ProjectionPathResult {
        path: home.join(".agents").join("skills").join("sample-skill"),
        change: ProjectionPathChange::Linked,
    }));
    assert!(
        evidence.by_target.is_empty(),
        "codex's own agents block wrote nothing (no agents in the fixture), so no target entry should exist: {:?}",
        evidence.by_target
    );
}

#[test]
fn update_skills_with_evidence_second_identical_run_is_a_true_no_op() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "sample-skill");
    let opts = SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        state_root: home.join(".ccync").join("build"),
        user_home: home,
        selected_projection_targets: vec!["copilot".into()],
        dry_run: false,
        replace: true,
    };

    run_update_skills(&opts).unwrap();
    let second = run_update_skills(&opts).unwrap();

    assert_eq!(
        second,
        ProjectionEvidence::default(),
        "an already-converged second run must record no further evidence at all"
    );
}

#[test]
fn update_skills_with_evidence_mid_run_fault_preserves_exact_partial_attribution() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "sample-skill");
    // Block codex's own `ensure_dir(&codex_agents_target(), ..)` with a plain
    // file occupying the directory's own path — codex's block is the last
    // one to run in `update_skills_with_evidence`, so copilot's own evidence
    // (processed earlier) must already be present in the fault's partial.
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(home.join(".codex").join("agents"), "blocking file").unwrap();

    let failure = run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["copilot".into(), "codex".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap_err();

    let copilot = failure
        .partial
        .by_target
        .get(&ProjectionTargetId::Copilot)
        .expect("copilot's own block ran to completion before codex's own block faulted");
    assert!(copilot.paths.contains(&ProjectionPathResult {
        path: home.join(".copilot").join("ccync"),
        change: ProjectionPathChange::Linked,
    }));
    assert!(
        !failure.partial.by_target.contains_key(&ProjectionTargetId::Codex),
        "codex's own block faulted on its very first statement (ensure_dir), before writing anything"
    );
}

#[test]
fn update_commands_with_evidence_selecting_gemini_cli_records_no_owned_paths() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "status");

    let evidence = run_update_commands(&CommandUpdateOptions {
        repo_root: repo_root.clone(),
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec!["gemini-cli".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(
        evidence.by_target.is_empty(),
        "gemini-cli no longer parses to any target, so no path may be owned in update_commands: {:?}",
        evidence
    );
}

#[test]
fn update_commands_with_evidence_canonical_bake_falls_back_to_common() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "status");

    let evidence = run_update_commands(&CommandUpdateOptions {
        repo_root: repo_root.clone(),
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec![],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(evidence.common.iter().any(|p| p.path
        == home
            .join(".ccync")
            .join("build")
            .join("render")
            .join("commands")
            .join("status")
            .join("SKILL.md")
        && p.change == ProjectionPathChange::Written));
    assert!(
        evidence.by_target.is_empty(),
        "no runtime was selected, so nothing should be attributed to a target: {:?}",
        evidence.by_target
    );
}

#[test]
fn update_commands_with_evidence_no_command_dirs_records_empty_evidence() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    // fixture_roots() creates an empty `commands/` dir with no children.

    let evidence = run_update_commands(&CommandUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        state_root: home.join(".ccync").join("build"),
        user_home: home,
        selected_projection_targets: vec!["gemini-cli".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert_eq!(evidence, ProjectionEvidence::default());
}

#[test]
fn update_commands_with_evidence_mid_run_fault_preserves_exact_partial_attribution() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "cmd-a");
    write_command_fixture(&source_root, "cmd-b");
    // Block the second command's own canonical directory (`cmd-a` sorts first)
    // with a plain file occupying its own path, forcing `ensure_dir` to fault
    // only once `cmd-a`'s own write has already completed.
    let canonical_commands_root = home
        .join(".ccync")
        .join("build")
        .join("render")
        .join("commands");
    fs::create_dir_all(&canonical_commands_root).unwrap();
    fs::write(canonical_commands_root.join("cmd-b"), "blocking file").unwrap();

    let failure = run_update_commands(&CommandUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec![],
        dry_run: false,
        replace: true,
    })
    .unwrap_err();

    assert!(failure.partial.common.iter().any(|p| p.path
        == canonical_commands_root.join("cmd-a").join("SKILL.md")
        && p.change == ProjectionPathChange::Written));
    assert!(
        !failure
            .partial
            .common
            .iter()
            .any(|p| p.path.starts_with(canonical_commands_root.join("cmd-b"))),
        "cmd-b's own block faulted on its own ensure_dir, before writing anything"
    );
}

#[test]
fn update_personalization_with_evidence_selecting_gemini_cli_records_no_owned_paths() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "sample-skill");
    let appdata = home.join("appdata");
    fs::create_dir_all(&appdata).unwrap();

    let evidence = run_update_personalization(&PersonalizationUpdateOptions {
        repo_root,
        source_root,
        user_home: home.clone(),
        appdata_root: appdata,
        selected_projection_targets: vec!["gemini-cli".into()],
        dry_run: false,
        replace: true,
        uninstall: false,
    })
    .unwrap();

    assert!(
        evidence.by_target.is_empty(),
        "gemini-cli no longer parses to any target, so no path may be owned in update_personalization: {:?}",
        evidence
    );
}

#[test]
fn update_personalization_with_evidence_preserved_repo_rule_warning_falls_back_to_common() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    let appdata = home.join("appdata");
    fs::create_dir_all(&appdata).unwrap();
    let rules_dir = repo_root.join(".agents").join("rules");
    fs::create_dir_all(&rules_dir).unwrap();
    fs::write(
        rules_dir.join("ccync.md"),
        "# user-owned, no ccync header\n",
    )
    .unwrap();

    let evidence = run_update_personalization(&PersonalizationUpdateOptions {
        repo_root,
        source_root,
        user_home: home,
        appdata_root: appdata,
        selected_projection_targets: vec![],
        dry_run: false,
        replace: true,
        uninstall: false,
    })
    .unwrap();

    assert!(
        evidence
            .run_warnings
            .iter()
            .any(|w| w.contains("preserved user-owned repo rule")),
        "the warning has no single owning target, so it must land in run_warnings (common), not by_target: {:?}",
        evidence
    );
    assert!(evidence.by_target.is_empty());
}

#[test]
fn update_personalization_with_evidence_no_selection_records_empty_evidence() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    let appdata = home.join("appdata");
    fs::create_dir_all(&appdata).unwrap();

    let evidence = run_update_personalization(&PersonalizationUpdateOptions {
        repo_root,
        source_root,
        user_home: home,
        appdata_root: appdata,
        selected_projection_targets: vec![],
        dry_run: false,
        replace: true,
        uninstall: false,
    })
    .unwrap();

    assert_eq!(evidence, ProjectionEvidence::default());
}

// ensure_dir_link: a genuine user-owned real directory at the link path (no
// CCYNC content marker) is preserved untouched — returns `false` so the
// caller must not record it in the managed-artifact registry, and warns.
#[test]
fn ensure_dir_link_preserves_user_owned_path_returns_false_with_warning() {
    use crate::support::ensure_dir_link;

    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("target");
    fs::create_dir_all(&target).unwrap();
    let link = tmp.path().join("user-dir");
    fs::create_dir_all(&link).unwrap();
    fs::write(link.join("notes.md"), "personal notes\n").unwrap();

    let mut report = ProjectionReport::default();
    let linked = ensure_dir_link(&target, &link, false, false, &mut report).unwrap();

    assert!(!linked, "genuine user-owned directory must not be linked");
    assert!(
        link.join("notes.md").exists(),
        "user content must survive untouched"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.message.contains("preserved user-owned path")),
        "preserve-user-path must emit a warning"
    );
    assert!(
        report.created_links.is_empty(),
        "preserved path must not be recorded as a created link"
    );
}

// ensure_dir_link: fresh creation returns `true`. Every production call site
// now pairs the return value with `mark_with_source`, guarded on `true` — this
// exercises that pairing directly and confirms the correct owning source id
// round-trips through the registry.
#[test]
fn ensure_dir_link_fresh_creation_returns_true_and_records_source_id() {
    use crate::support::{ensure_dir_link, ManagedArtifactRegistry, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("target");
    fs::create_dir_all(&target).unwrap();
    let link = tmp.path().join("skills").join("my-skill");

    let mut report = ProjectionReport::default();
    let linked = ensure_dir_link(&target, &link, false, false, &mut report).unwrap();
    assert!(linked, "fresh creation must return true");
    assert!(is_symlink_or_junction(&link));

    let lockfile = tmp.path().join("lock.json");
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    if linked {
        reg.mark_with_source(MANAGED_SKILL_PATHS, &link, "my-bundle");
    }
    reg.persist(&[], false, &mut report).unwrap();

    let reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    assert_eq!(
        reg2.owning_source_of(&link),
        Some("my-bundle"),
        "ensured link must record the correct owning source id"
    );
}

// Regression for the false-attribution bug fixed alongside `ensure_dir_link`'s
// `Ok(bool)` split: a preserve-user-path outcome must never reach the
// registry, so the user's own directory disappears from both the
// managed-artifact categories and `sourceAttribution` — mirroring every
// production call site's `if linked { mark_with_source(...) }` guard.
#[test]
fn ensure_dir_link_preserved_path_never_recorded_in_registry() {
    use crate::support::{ensure_dir_link, ManagedArtifactRegistry, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("target");
    fs::create_dir_all(&target).unwrap();
    let link = tmp.path().join("skills").join("user-owned");
    fs::create_dir_all(&link).unwrap();
    fs::write(link.join("SKILL.md"), "# not ccync\n").unwrap();

    let mut report = ProjectionReport::default();
    let linked = ensure_dir_link(&target, &link, false, false, &mut report).unwrap();
    assert!(!linked);

    let lockfile = tmp.path().join("lock.json");
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    if linked {
        reg.mark_with_source(MANAGED_SKILL_PATHS, &link, "my-bundle");
    }
    reg.persist(&[], false, &mut report).unwrap();

    assert!(
        !lockfile.exists(),
        "nothing was ever marked, so persist on an empty prior/next pair must not write a lockfile"
    );
    let reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    assert_eq!(
        reg2.owning_source_of(&link),
        None,
        "preserved user path must never carry source attribution"
    );
}

// GC: an orphan attribution row (category row missing, attribution row
// alive) owned by a now-installed source is dropped on the first persist
// that names that source in `installed_source_ids` — the lockfile rewrites.
// A second persist over the now-unchanged registry state is a true no-op:
// byte-identical output, proving GC does not re-fire or otherwise perturb
// output once the orphan is already gone.
#[test]
fn registry_persist_orphan_rewrites_once_then_reruns_as_noop() {
    use crate::support::{ManagedArtifactRegistry, MANAGED_AGENT_PATHS, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let lockfile = tmp.path().join("lock.json");
    let orphan_path = tmp.path().join("skill-orphan");

    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg.mark_with_source(MANAGED_SKILL_PATHS, &orphan_path, "my-bundle");
    // Keep a row alive in a different category so the marker survives once
    // MANAGED_SKILL_PATHS is cleared below.
    reg.mark_with_source(
        MANAGED_AGENT_PATHS,
        &tmp.path().join("keep-alive.md"),
        "keep-alive",
    );
    reg.persist(&[], false, &mut report).unwrap();
    let before = fs::read_to_string(&lockfile).unwrap();
    assert!(before.contains("skill-orphan"));

    // Orphan the row: category row goes missing, attribution row survives.
    let mut reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg2.replace_category(MANAGED_SKILL_PATHS);
    reg2.persist(&["my-bundle"], false, &mut report).unwrap();
    let after_gc = fs::read_to_string(&lockfile).unwrap();
    assert_ne!(
        before, after_gc,
        "GC of a live orphan owned by an installed source must rewrite the lockfile"
    );
    assert!(
        !after_gc.contains("skill-orphan"),
        "orphan row owned by an installed source must be gone after the rewrite"
    );

    // Rerun with an unchanged registry state: prior == next, nothing left to
    // GC — must be a true no-op.
    let mut reg3 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg3.persist(&["my-bundle"], false, &mut report).unwrap();
    let after_rerun = fs::read_to_string(&lockfile).unwrap();
    assert_eq!(
        after_gc, after_rerun,
        "identical rerun with nothing left to GC must leave the lockfile byte-identical"
    );
}

// Foreign-only attribution (owner absent from `installed_source_ids`)
// survives an orphaning GC pass untouched, and `persist` — which only ever
// writes the lockfile itself — never touches an unrelated sibling path
// living beside the projected artifacts on disk.
#[test]
fn registry_persist_foreign_attribution_survives_sibling_immutable() {
    use crate::support::{ManagedArtifactRegistry, MANAGED_AGENT_PATHS, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let lockfile = tmp.path().join("lock.json");
    let foreign_path = tmp.path().join("skill-foreign");
    let sibling = tmp.path().join("skill-sibling.md");
    fs::write(&sibling, "unrelated user content\n").unwrap();
    let sibling_before = fs::read(&sibling).unwrap();

    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg.mark_with_source(MANAGED_SKILL_PATHS, &foreign_path, "other-bundle");
    reg.mark_with_source(
        MANAGED_AGENT_PATHS,
        &tmp.path().join("keep-alive.md"),
        "keep-alive",
    );
    reg.persist(&[], false, &mut report).unwrap();

    // Orphan the row while "other-bundle" stays absent from this
    // invocation's installed set — foreign, so GC must not touch it.
    let mut reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg2.replace_category(MANAGED_SKILL_PATHS);
    reg2.persist(&["my-bundle"], false, &mut report).unwrap();

    let reg3 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    assert_eq!(
        reg3.owning_source_of(&foreign_path),
        Some("other-bundle"),
        "foreign-owned orphan row must survive persist and reload"
    );
    assert_eq!(
        fs::read(&sibling).unwrap(),
        sibling_before,
        "an unrecorded sibling path must remain byte-identical — persist touches only the lockfile"
    );
}

// End-to-end idempotence: rerunning the full skill/agent projection pipeline
// with identical inputs must leave every projected surface — files, link
// targets, and the lockfile that lives inside the same tree — byte-identical.
#[test]
fn run_update_skills_identical_rerun_is_byte_identical() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "sample-skill");
    fs::write(
        source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\ncolor: green\ntools: [read, edit, execute]\n---\nBody\n",
    )
    .unwrap();

    let options = SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec![
            "copilot".into(),
            "gemini-cli".into(),
            "agy-cli".into(),
            "opencode".into(),
        ],
        dry_run: false,
        replace: true,
    };

    run_update_skills(&options).unwrap();
    let first = snapshot_tree(&home);
    assert!(
        !first.is_empty(),
        "test setup: the pipeline must project something to snapshot"
    );

    run_update_skills(&options).unwrap();
    let second = snapshot_tree(&home);

    assert_eq!(
        first, second,
        "identical rerun must leave every projected surface and the lockfile byte-identical"
    );
}

// Dry-run over already-projected state must leave disk and the lockfile
// completely untouched — every write path (`write_text`, `ensure_dir_link`,
// `prune_stale_links`, `ManagedArtifactRegistry::persist`) threads `dry_run`
// through to its final I/O, not just its evidence bookkeeping.
#[test]
fn run_update_skills_dry_run_rerun_leaves_disk_byte_identical() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "sample-skill");
    fs::write(
        source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\ncolor: green\ntools: [read, edit, execute]\n---\nBody\n",
    )
    .unwrap();

    let mut options = SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        state_root: home.clone().join(".ccync").join("build"),
        user_home: home.clone(),
        selected_projection_targets: vec![
            "copilot".into(),
            "gemini-cli".into(),
            "agy-cli".into(),
            "opencode".into(),
        ],
        dry_run: false,
        replace: true,
    };

    run_update_skills(&options).unwrap();
    let before = snapshot_tree(&home);

    options.dry_run = true;
    run_update_skills(&options).unwrap();
    let after = snapshot_tree(&home);

    assert_eq!(
        before, after,
        "a dry-run rerun over already-projected state must leave disk and the lockfile byte-identical"
    );
}

// ── Direct ownership categories ─────────────────────────────────────────

// A recorded path is compared case-insensitively: Windows registry keys are
// stored lowercased, so an exact `PathBuf` comparison would fail there.
fn same_recorded_path(recorded: &Path, path: &Path) -> bool {
    recorded
        .to_string_lossy()
        .eq_ignore_ascii_case(&path.to_string_lossy())
}

fn direct_skill_plan(
    source: PathBuf,
    root: PathBuf,
    alias: &str,
    source_id: &str,
) -> DirectSkillsPlan {
    DirectSkillsPlan {
        packages: vec![DirectSkillPackage {
            source_id: source_id.into(),
            original_name: alias.into(),
            alias: alias.into(),
            source_path: source,
        }],
        roots: vec![DirectSkillRoot {
            path: root,
            targets: vec![ProjectionTargetId::Codex],
        }],
        served_targets: vec![],
    }
}

// A machine run replaces every machine-owned category. Direct rows must
// survive that replacement and stay exactly readable through `direct_records`.
#[test]
fn machine_category_replacement_preserves_direct_categories() {
    use crate::support::{
        ManagedArtifactRegistry, MANAGED_AGENT_PATHS, MANAGED_AGY_ROOT_PATHS,
        MANAGED_CODEX_AGENT_PATHS, MANAGED_COMMAND_PATHS, MANAGED_DIRECT_AGENT_PATHS,
        MANAGED_DIRECT_COMMAND_PATHS, MANAGED_DIRECT_SKILL_PATHS, MANAGED_DISCUSS_SKILL_PATHS,
        MANAGED_LEGACY_PATHS, MANAGED_OPENCODE_AGENT_PATHS, MANAGED_SKILL_PATHS,
    };

    let tmp = TempDir::new().unwrap();
    let lock = tmp.path().join("lock.json");
    let skill = tmp.path().join("skills").join("alpha");
    let agent = tmp.path().join("agents").join("beta.md");
    let command = tmp.path().join("commands").join("gamma.md");
    let sources = ["src-one", "src-two"];

    let mut report = ProjectionReport::default();
    let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
    registry.mark_with_source(MANAGED_DIRECT_SKILL_PATHS, &skill, "src-one");
    registry.mark_with_source(MANAGED_DIRECT_AGENT_PATHS, &agent, "src-two");
    registry.mark(MANAGED_DIRECT_COMMAND_PATHS, &command);
    registry.persist(&sources, false, &mut report).unwrap();

    let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
    for category in [
        MANAGED_AGENT_PATHS,
        MANAGED_CODEX_AGENT_PATHS,
        MANAGED_OPENCODE_AGENT_PATHS,
        MANAGED_COMMAND_PATHS,
        MANAGED_DISCUSS_SKILL_PATHS,
        MANAGED_LEGACY_PATHS,
        MANAGED_SKILL_PATHS,
        MANAGED_AGY_ROOT_PATHS,
    ] {
        registry.replace_category(category);
    }
    registry.persist(&sources, false, &mut report).unwrap();

    let records = direct_records(&lock);
    assert_eq!(
        records.len(),
        3,
        "all three direct rows must survive: {records:?}"
    );
    let skill_record = records
        .iter()
        .find(|r| r.kind == DirectArtifactKind::Skill)
        .expect("skill row");
    assert!(same_recorded_path(&skill_record.path, &skill));
    assert_eq!(skill_record.source_id.as_deref(), Some("src-one"));
    let agent_record = records
        .iter()
        .find(|r| r.kind == DirectArtifactKind::Agent)
        .expect("agent row");
    assert!(same_recorded_path(&agent_record.path, &agent));
    assert_eq!(agent_record.source_id.as_deref(), Some("src-two"));
    let command_record = records
        .iter()
        .find(|r| r.kind == DirectArtifactKind::Command)
        .expect("command row");
    assert!(same_recorded_path(&command_record.path, &command));
    assert_eq!(command_record.source_id, None);
}

// ── Machine source links ────────────────────────────────────────────────────

fn machine_link_options(
    temp: &TempDir,
    targets: &[&str],
    dry_run: bool,
    replace: bool,
) -> MachineUpdateOptions {
    let repo_root = temp.path().join("repo");
    MachineUpdateOptions {
        source_root: repo_root.join("plugins").join("ccync-core"),
        repo_root,
        state_root: temp.path().join("home").join(".ccync").join("build"),
        user_home: temp.path().join("home"),
        appdata_root: temp.path().join("appdata"),
        selected_projection_targets: targets.iter().map(|t| t.to_string()).collect(),
        dry_run,
        replace,
        uninstall: false,
        include_source_projections: true,
    }
}

#[test]
fn caller_chosen_state_root_is_used_for_ownership_storage() {
    let temp = TempDir::new().unwrap();
    let repo_root = temp.path().join("repo");
    let source_root = repo_root.join("plugins").join("ccync-core");
    let home = temp.path().join("home");
    fs::create_dir_all(source_root.join("skills").join("sample")).unwrap();
    fs::write(
        source_root.join("skills").join("sample").join("SKILL.md"),
        "# Sample",
    )
    .unwrap();

    let fixture_root = temp.path().join("fixture-state");
    let mut options = SkillUpdateOptions {
        repo_root: repo_root.clone(),
        sources: vec![ProjectionSource {
            root: source_root.clone(),
            id: "ccync-core".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        state_root: home.join(".ccync").join("build"),
        selected_projection_targets: vec!["copilot".into()],
        dry_run: false,
        replace: true,
    };
    options.state_root = fixture_root.clone();
    let _fixture_report = run_update_skills(&options).unwrap();
    assert!(fixture_root.join("lock.json").is_file());
    assert!(!home.join(".ccync").join("build").exists());

    let legacy_root = home.join(".ccync").join("build");
    fs::create_dir_all(&legacy_root).unwrap();
    fs::copy(
        fixture_root.join("lock.json"),
        legacy_root.join("lock.json"),
    )
    .unwrap();
    options.state_root = legacy_root;
    run_update_skills(&options).unwrap();
    assert_eq!(
        fs::read(fixture_root.join("lock.json")).unwrap(),
        fs::read(home.join(".ccync").join("build").join("lock.json")).unwrap()
    );
    assert!(paths_share_filesystem(&fixture_root, &home).unwrap());
}

#[test]
fn machine_run_records_exactly_the_source_links_it_creates() {
    let (temp, _repo_root, _source_root, home) = fixture_roots();
    let lock = home.join(".ccync").join("build").join("lock.json");
    let targets = ["copilot", "agy-cli"];

    run_machine_update(&machine_link_options(&temp, &targets, true, true)).unwrap();
    assert!(
        source_link_records(&lock).is_empty(),
        "a dry run must not record source links"
    );

    run_machine_update(&machine_link_options(&temp, &targets, false, true)).unwrap();
    let records = source_link_records(&lock);
    assert_eq!(records.len(), 3, "exactly three source links: {records:?}");
    for link in [
        home.join(".copilot").join("ccync"),
        home.join(".gemini").join("ccync"),
        home.join(".gemini").join("antigravity-cli").join("ccync"),
    ] {
        assert!(
            records.iter().any(|r| same_recorded_path(r, &link)),
            "missing recorded source link {}: {records:?}",
            link.display()
        );
    }
}

#[test]
fn machine_run_leaves_a_preserved_user_directory_unrecorded() {
    let (temp, _repo_root, _source_root, home) = fixture_roots();
    let lock = home.join(".ccync").join("build").join("lock.json");
    let user_dir = home.join(".gemini").join("ccync");
    fs::create_dir_all(&user_dir).unwrap();
    fs::write(user_dir.join("mine.md"), "user").unwrap();

    run_machine_update(&machine_link_options(&temp, &["agy-cli"], false, false)).unwrap();

    assert!(
        !is_symlink_or_junction(&user_dir) && user_dir.join("mine.md").is_file(),
        "the user directory must be preserved"
    );
    let records = source_link_records(&lock);
    assert!(
        !records.iter().any(|r| same_recorded_path(r, &user_dir)),
        "a preserved user directory must stay unrecorded: {records:?}"
    );
}

#[test]
fn source_link_rows_survive_a_later_run_without_their_targets() {
    let (temp, _repo_root, _source_root, home) = fixture_roots();
    let lock = home.join(".ccync").join("build").join("lock.json");

    run_machine_update(&machine_link_options(
        &temp,
        &["copilot", "agy-cli"],
        false,
        true,
    ))
    .unwrap();
    run_machine_update(&machine_link_options(&temp, &["claude"], false, true)).unwrap();

    assert_eq!(
        source_link_records(&lock).len(),
        3,
        "rows must survive a run that does not select their targets"
    );
}

#[test]
fn remove_source_link_removes_the_link_and_keeps_its_target() {
    let (temp, _repo_root, _source_root, home) = fixture_roots();
    let lock = home.join(".ccync").join("build").join("lock.json");
    run_machine_update(&machine_link_options(&temp, &["copilot"], false, true)).unwrap();

    let link = home.join(".copilot").join("ccync");
    let target_marker = home.join(".ccync").join("marker.txt");
    fs::write(&target_marker, "keep").unwrap();
    assert!(is_symlink_or_junction(&link), "test setup: link must exist");

    remove_source_link(&lock, &link, true).unwrap();
    assert!(
        is_symlink_or_junction(&link),
        "a dry run must not remove the link"
    );
    assert_eq!(source_link_records(&lock).len(), 1);

    remove_source_link(&lock, &link, false).unwrap();
    assert!(!link.exists() && !is_symlink_or_junction(&link));
    assert_eq!(fs::read_to_string(&target_marker).unwrap(), "keep");
    assert!(source_link_records(&lock).is_empty());
}

#[test]
fn remove_source_link_forgets_a_row_whose_path_is_no_longer_a_link() {
    let (temp, _repo_root, _source_root, home) = fixture_roots();
    let lock = home.join(".ccync").join("build").join("lock.json");
    run_machine_update(&machine_link_options(&temp, &["copilot"], false, true)).unwrap();

    let link = home.join(".copilot").join("ccync");
    crate::support::remove_link(&link).unwrap();
    fs::create_dir_all(&link).unwrap();
    fs::write(link.join("user.md"), "user").unwrap();

    remove_source_link(&lock, &link, false).unwrap();

    assert!(source_link_records(&lock).is_empty());
    assert_eq!(fs::read_to_string(link.join("user.md")).unwrap(), "user");
}

#[test]
fn tp29_spec_machine_source_links_lifecycle_preserved_user_dir_and_removal() {
    let (temp, _repo_root, _source_root, home) = fixture_roots();
    let lock = home.join(".ccync").join("build").join("lock.json");

    // Clean run with Copilot and agy-cli records exactly three source links
    let targets = ["copilot", "agy-cli"];
    run_machine_update(&machine_link_options(&temp, &targets, false, true)).unwrap();

    let initial_records = source_link_records(&lock);
    assert_eq!(
        initial_records.len(),
        3,
        "clean run with Copilot and agy-cli must record exactly three source links: {initial_records:?}"
    );

    let copilot_link = home.join(".copilot").join("ccync");
    let gemini_link = home.join(".gemini").join("ccync");
    let agy_cli_link = home.join(".gemini").join("antigravity-cli").join("ccync");

    for link in [&copilot_link, &gemini_link, &agy_cli_link] {
        assert!(
            initial_records.iter().any(|r| same_recorded_path(r, link)),
            "link must be recorded: {}",
            link.display()
        );
        assert!(
            is_symlink_or_junction(link),
            "link must exist as symlink or junction: {}",
            link.display()
        );
    }

    // A later run without those targets (e.g. Claude only) preserves all source link rows
    run_machine_update(&machine_link_options(&temp, &["claude"], false, true)).unwrap();
    let preserved_records = source_link_records(&lock);
    assert_eq!(
        preserved_records.len(),
        3,
        "source-link rows must survive machine category replacement when targets are deselected"
    );

    // Setup a target marker file in ~/.ccync to verify link targets are not deleted
    let target_marker = home.join(".ccync").join("marker.txt");
    fs::write(&target_marker, "target contents").unwrap();

    // Dry run removal leaves link and row intact
    remove_source_link(&lock, &copilot_link, true).unwrap();
    assert!(
        is_symlink_or_junction(&copilot_link),
        "dry-run removal must not delete the link"
    );
    assert_eq!(source_link_records(&lock).len(), 3);

    // Removal deletes the link entry and forgets its row, keeping target untouched
    remove_source_link(&lock, &copilot_link, false).unwrap();
    assert!(
        !copilot_link.exists() && !is_symlink_or_junction(&copilot_link),
        "removal must delete the symlink/junction"
    );
    assert_eq!(
        fs::read_to_string(&target_marker).unwrap(),
        "target contents",
        "removal must keep link target untouched"
    );
    let after_copilot_removal = source_link_records(&lock);
    assert_eq!(after_copilot_removal.len(), 2);
    assert!(!after_copilot_removal
        .iter()
        .any(|r| same_recorded_path(r, &copilot_link)));

    // A path that is no longer a link is forgotten without deleting user directory
    crate::support::remove_link(&gemini_link).unwrap();
    fs::create_dir_all(&gemini_link).unwrap();
    let user_doc = gemini_link.join("user.txt");
    fs::write(&user_doc, "user data").unwrap();

    remove_source_link(&lock, &gemini_link, false).unwrap();
    assert!(
        gemini_link.is_dir(),
        "non-link path must not be removed by remove_source_link"
    );
    assert_eq!(
        fs::read_to_string(&user_doc).unwrap(),
        "user data",
        "files in non-link directory must be preserved"
    );
    let after_gemini_removal = source_link_records(&lock);
    assert_eq!(after_gemini_removal.len(), 1);
    assert!(!after_gemini_removal
        .iter()
        .any(|r| same_recorded_path(r, &gemini_link)));

    // Clean up last link
    remove_source_link(&lock, &agy_cli_link, false).unwrap();
    assert!(
        source_link_records(&lock).is_empty(),
        "all source link rows forgotten after removals"
    );

    // Second test fixture: pre-existing user directory stays unrecorded during run
    let (temp2, _repo2, _source2, home2) = fixture_roots();
    let lock2 = home2.join(".ccync").join("build").join("lock.json");
    let user_dir = home2.join(".gemini").join("antigravity-cli").join("ccync");
    fs::create_dir_all(&user_dir).unwrap();
    fs::write(user_dir.join("preserved.md"), "untouched").unwrap();

    run_machine_update(&machine_link_options(&temp2, &targets, false, false)).unwrap();

    assert!(
        !is_symlink_or_junction(&user_dir) && user_dir.join("preserved.md").is_file(),
        "user directory must remain a normal directory with intact files"
    );
    let records2 = source_link_records(&lock2);
    assert_eq!(
        records2.len(),
        2,
        "only the 2 newly created links should be recorded, preserved directory unrecorded"
    );
    assert!(
        !records2.iter().any(|r| same_recorded_path(r, &user_dir)),
        "preserved user directory must not have a source link record"
    );
}

// Ownership needs committed membership AND matching attribution. A legacy row
// whose attribution names another source, and an attribution row with no
// membership, must both stay foreign: the direct writer must not overwrite
// them, and must keep the legacy row recorded.
#[test]
fn direct_skill_publication_keeps_unresolved_legacy_rows_and_foreign_paths() {
    use crate::support::{ManagedArtifactRegistry, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let lock = tmp.path().join("lock.json");
    let root = tmp.path().join("root");
    let foreign_legacy = root.join("foreign");
    let attribution_only = root.join("orphan");
    for dir in [&foreign_legacy, &attribution_only] {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("SKILL.md"), "user").unwrap();
    }

    // Pass one records both attributions. Pass two drops the orphan's
    // membership, leaving an attribution row with no category membership.
    let mut report = ProjectionReport::default();
    let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
    registry.mark_with_source(MANAGED_SKILL_PATHS, &attribution_only, "src-one");
    registry.persist(&[], false, &mut report).unwrap();
    let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
    registry.replace_category(MANAGED_SKILL_PATHS);
    registry.mark_with_source(MANAGED_SKILL_PATHS, &foreign_legacy, "src-two");
    registry.persist(&[], false, &mut report).unwrap();

    let source = tmp.path().join("source");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("SKILL.md"), "new").unwrap();
    let mut plan = direct_skill_plan(source.clone(), root.clone(), "foreign", "src-one");
    plan.packages.push(DirectSkillPackage {
        source_id: "src-one".into(),
        original_name: "orphan".into(),
        alias: "orphan".into(),
        source_path: source,
    });

    let evidence = publish_direct_skills(&plan, &lock, false);

    assert!(evidence.by_target[&ProjectionTargetId::Codex]
        .failure
        .is_some());
    assert_eq!(
        fs::read_to_string(foreign_legacy.join("SKILL.md")).unwrap(),
        "user"
    );
    assert_eq!(
        fs::read_to_string(attribution_only.join("SKILL.md")).unwrap(),
        "user"
    );
    assert!(direct_records(&lock).is_empty());
    let reloaded = ManagedArtifactRegistry::load(&lock, &mut report);
    assert_eq!(
        reloaded.prior_paths(MANAGED_SKILL_PATHS).count(),
        1,
        "the unresolved legacy row must stay in its category"
    );
}

// An eligible legacy row (membership plus matching attribution) moves to the
// direct category when the direct writer takes over its path. The attribution
// row survives the move.
#[test]
fn eligible_legacy_skill_row_migrates_to_direct_category_with_attribution() {
    use crate::support::{ManagedArtifactRegistry, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let lock = tmp.path().join("lock.json");
    let root = tmp.path().join("root");
    let dest = root.join("original");
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("SKILL.md"), "old").unwrap();
    let source = tmp.path().join("source");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("SKILL.md"), "new").unwrap();

    let mut report = ProjectionReport::default();
    let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
    registry.mark_with_source(MANAGED_SKILL_PATHS, &dest, "src-one");
    registry.persist(&[], false, &mut report).unwrap();

    let plan = direct_skill_plan(source, root, "original", "src-one");
    let evidence = publish_direct_skills(&plan, &lock, false);

    assert!(evidence.by_target[&ProjectionTargetId::Codex]
        .failure
        .is_none());
    assert_eq!(fs::read_to_string(dest.join("SKILL.md")).unwrap(), "new");
    let records = direct_records(&lock);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, DirectArtifactKind::Skill);
    assert!(same_recorded_path(&records[0].path, &dest));
    assert_eq!(records[0].source_id.as_deref(), Some("src-one"));
    let reloaded = ManagedArtifactRegistry::load(&lock, &mut report);
    assert_eq!(reloaded.prior_paths(MANAGED_SKILL_PATHS).count(), 0);
}

#[test]
fn eligible_legacy_agent_row_migrates_to_direct_agent_category() {
    use crate::support::{ManagedArtifactRegistry, MANAGED_AGENT_PATHS};

    let tmp = TempDir::new().unwrap();
    let lock = tmp.path().join("lock.json");
    let home = tmp.path().join("home");
    let agents = home.join(".claude").join("agents");
    fs::create_dir_all(&agents).unwrap();
    let dest = agents.join("reviewer.md");
    fs::write(&dest, "old").unwrap();

    let mut report = ProjectionReport::default();
    let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
    registry.mark_with_source(MANAGED_AGENT_PATHS, &dest, "src-one");
    registry.persist(&[], false, &mut report).unwrap();

    let content = "---\nname: reviewer\n---\nbody\n";
    let input = DirectAgentInput {
        source_id: "src-one".into(),
        name: "reviewer".into(),
        content: content.into(),
    };
    let evidence = publish_direct_agents(ProjectionTargetId::Claude, &[input], &home, &lock, false);

    assert!(evidence.by_target[&ProjectionTargetId::Claude]
        .failure
        .is_none());
    assert_eq!(fs::read_to_string(&dest).unwrap(), content);
    let records = direct_records(&lock);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, DirectArtifactKind::Agent);
    assert!(same_recorded_path(&records[0].path, &dest));
    assert_eq!(records[0].source_id.as_deref(), Some("src-one"));
    let reloaded = ManagedArtifactRegistry::load(&lock, &mut report);
    assert_eq!(reloaded.prior_paths(MANAGED_AGENT_PATHS).count(), 0);
}

// `direct_records` returns exactly the direct rows. `forget_direct_record`
// removes only the named row and its attribution, and only when asked to write.
#[test]
fn direct_records_are_exact_and_forget_removes_only_the_named_row() {
    use crate::support::{
        ManagedArtifactRegistry, MANAGED_DIRECT_AGENT_PATHS, MANAGED_DIRECT_COMMAND_PATHS,
        MANAGED_DIRECT_SKILL_PATHS, MANAGED_SKILL_PATHS,
    };

    let tmp = TempDir::new().unwrap();
    let lock = tmp.path().join("lock.json");
    let agent = tmp.path().join("agents").join("beta.md");
    let command = tmp.path().join("commands").join("gamma.md");
    let skill = tmp.path().join("skills").join("alpha");
    let foreign = tmp.path().join("skills").join("machine");

    let mut report = ProjectionReport::default();
    let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
    registry.mark_with_source(MANAGED_DIRECT_AGENT_PATHS, &agent, "src-one");
    registry.mark(MANAGED_DIRECT_COMMAND_PATHS, &command);
    registry.mark_with_source(MANAGED_DIRECT_SKILL_PATHS, &skill, "src-two");
    registry.mark(MANAGED_SKILL_PATHS, &foreign);
    registry
        .persist(&["src-one", "src-two"], false, &mut report)
        .unwrap();

    let records = direct_records(&lock);
    assert_eq!(
        records.len(),
        3,
        "machine skill rows are not direct: {records:?}"
    );

    forget_direct_record(&lock, DirectArtifactKind::Agent, &agent, true).unwrap();
    assert_eq!(direct_records(&lock).len(), 3, "dry run must not write");

    // Wrong kind for a recorded path, and a path never recorded, are no-ops.
    forget_direct_record(&lock, DirectArtifactKind::Skill, &command, false).unwrap();
    forget_direct_record(&lock, DirectArtifactKind::Command, &foreign, false).unwrap();
    assert_eq!(direct_records(&lock).len(), 3);

    forget_direct_record(&lock, DirectArtifactKind::Agent, &agent, false).unwrap();
    let after = direct_records(&lock);
    assert_eq!(after.len(), 2);
    assert!(after.iter().all(|r| !same_recorded_path(&r.path, &agent)));
    let raw = fs::read_to_string(&lock).unwrap().to_ascii_lowercase();
    assert!(
        !raw.contains("beta.md"),
        "attribution row must go with the path"
    );

    let reloaded = ManagedArtifactRegistry::load(&lock, &mut report);
    assert!(
        reloaded.is_managed(&foreign),
        "foreign machine row must survive"
    );
    assert!(reloaded.is_managed(&command));
}

// Membership in a direct category authorizes removal. Attribution alone, with
// no category membership, does not.
#[test]
fn managed_deprojection_covers_direct_categories_and_not_attribution_alone() {
    use crate::support::{
        ManagedArtifactRegistry, MANAGED_DIRECT_AGENT_PATHS, MANAGED_SKILL_PATHS,
    };

    let tmp = TempDir::new().unwrap();
    let lock = tmp.path().join("build").join("lock.json");
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    let agent = tmp.path().join("agents").join("beta.md");
    let attribution_only = tmp.path().join("skills").join("orphan");

    let mut report = ProjectionReport::default();
    let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
    registry.mark_with_source(MANAGED_DIRECT_AGENT_PATHS, &agent, "src-one");
    registry.mark_with_source(MANAGED_SKILL_PATHS, &attribution_only, "src-one");
    registry.persist(&[], false, &mut report).unwrap();
    let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
    registry.replace_category(MANAGED_SKILL_PATHS);
    registry.persist(&[], false, &mut report).unwrap();

    let authority = ManagedDeprojection::from_lockfile(&lock);
    assert!(authority.is_authorized_for_removal(&agent));
    assert!(
        !authority.is_authorized_for_removal(&attribution_only),
        "attribution alone must not authorize removal"
    );
}

#[test]
fn tp01_spec_direct_categories_load_persist_and_machine_replace() {
    use crate::support::{
        ManagedArtifactRegistry, MANAGED_AGENT_PATHS, MANAGED_AGY_ROOT_PATHS,
        MANAGED_CODEX_AGENT_PATHS, MANAGED_COMMAND_PATHS, MANAGED_DIRECT_AGENT_PATHS,
        MANAGED_DIRECT_COMMAND_PATHS, MANAGED_DIRECT_SKILL_PATHS, MANAGED_DISCUSS_SKILL_PATHS,
        MANAGED_LEGACY_PATHS, MANAGED_OPENCODE_AGENT_PATHS, MANAGED_SKILL_PATHS,
    };

    let tmp = TempDir::new().unwrap();
    let lock = tmp.path().join("lock.json");
    let direct_skill = tmp.path().join("skills").join("skill-one");
    let direct_agent = tmp.path().join("agents").join("agent-one.md");
    let direct_command = tmp.path().join("commands").join("cmd-one.md");
    let machine_skill = tmp.path().join("skills").join("machine-skill");
    let machine_command = tmp.path().join("commands").join("machine-cmd.md");

    let sources = ["src-skill", "src-agent", "src-command", "src-machine"];

    let mut report = ProjectionReport::default();
    let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
    registry.mark_with_source(MANAGED_DIRECT_SKILL_PATHS, &direct_skill, "src-skill");
    registry.mark_with_source(MANAGED_DIRECT_AGENT_PATHS, &direct_agent, "src-agent");
    registry.mark_with_source(MANAGED_DIRECT_COMMAND_PATHS, &direct_command, "src-command");
    registry.mark_with_source(MANAGED_SKILL_PATHS, &machine_skill, "src-machine");
    registry.mark(MANAGED_COMMAND_PATHS, &machine_command);
    registry.persist(&sources, false, &mut report).unwrap();

    // Reload and assert all direct and machine records are present initially
    let initial_direct = direct_records(&lock);
    assert_eq!(initial_direct.len(), 3, "initial direct records count");
    let registry_reloaded = ManagedArtifactRegistry::load(&lock, &mut report);
    assert!(registry_reloaded.is_managed(&direct_skill));
    assert!(registry_reloaded.is_managed(&direct_agent));
    assert!(registry_reloaded.is_managed(&direct_command));
    assert!(registry_reloaded.is_managed(&machine_skill));
    assert!(registry_reloaded.is_managed(&machine_command));

    // Perform machine category replacement across every machine category
    let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
    for cat in [
        MANAGED_AGENT_PATHS,
        MANAGED_CODEX_AGENT_PATHS,
        MANAGED_OPENCODE_AGENT_PATHS,
        MANAGED_COMMAND_PATHS,
        MANAGED_DISCUSS_SKILL_PATHS,
        MANAGED_LEGACY_PATHS,
        MANAGED_SKILL_PATHS,
        MANAGED_AGY_ROOT_PATHS,
    ] {
        registry.replace_category(cat);
    }
    registry.persist(&sources, false, &mut report).unwrap();

    // Read direct records and verify exact preservation
    let records = direct_records(&lock);
    assert_eq!(
        records.len(),
        3,
        "all three direct categories must survive machine category replacement"
    );

    let s_rec = records
        .iter()
        .find(|r| r.kind == DirectArtifactKind::Skill)
        .expect("direct skill record present");
    assert!(same_recorded_path(&s_rec.path, &direct_skill));
    assert_eq!(s_rec.source_id.as_deref(), Some("src-skill"));

    let a_rec = records
        .iter()
        .find(|r| r.kind == DirectArtifactKind::Agent)
        .expect("direct agent record present");
    assert!(same_recorded_path(&a_rec.path, &direct_agent));
    assert_eq!(a_rec.source_id.as_deref(), Some("src-agent"));

    let c_rec = records
        .iter()
        .find(|r| r.kind == DirectArtifactKind::Command)
        .expect("direct command record present");
    assert!(same_recorded_path(&c_rec.path, &direct_command));
    assert_eq!(c_rec.source_id.as_deref(), Some("src-command"));

    // Verify machine categories were replaced
    let reloaded = ManagedArtifactRegistry::load(&lock, &mut report);
    assert!(!reloaded.is_managed(&machine_skill));
    assert!(!reloaded.is_managed(&machine_command));

    // Remove-after-success operation on direct records
    forget_direct_record(&lock, DirectArtifactKind::Skill, &direct_skill, false).unwrap();
    let records_after = direct_records(&lock);
    assert_eq!(records_after.len(), 2);
    assert!(records_after
        .iter()
        .all(|r| !same_recorded_path(&r.path, &direct_skill)));
}

#[test]
fn tp02_spec_migrate_category_attribution_and_reject_attribution_name_digest_only() {
    use crate::support::{ManagedArtifactRegistry, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let lock = tmp.path().join("lock.json");
    let root = tmp.path().join("root");
    let eligible_dest = root.join("eligible-skill");
    fs::create_dir_all(&eligible_dest).unwrap();
    fs::write(eligible_dest.join("SKILL.md"), "legacy-content").unwrap();

    let orphan_attribution = root.join("orphan-attribution");
    fs::create_dir_all(&orphan_attribution).unwrap();
    fs::write(orphan_attribution.join("SKILL.md"), "same-skill-content").unwrap();

    let same_name_unmanaged = root.join("same-name-unmanaged");
    fs::create_dir_all(&same_name_unmanaged).unwrap();
    fs::write(same_name_unmanaged.join("SKILL.md"), "unmanaged-same-name").unwrap();

    let same_digest_unmanaged = root.join("same-digest-unmanaged");
    fs::create_dir_all(&same_digest_unmanaged).unwrap();
    fs::write(same_digest_unmanaged.join("SKILL.md"), "legacy-content").unwrap();

    // Setup registry:
    // 1. Mark eligible_dest in MANAGED_SKILL_PATHS with attribution "src-migrated"
    // 2. Mark orphan_attribution in MANAGED_SKILL_PATHS with attribution "src-orphan"
    let mut report = ProjectionReport::default();
    let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
    registry.mark_with_source(MANAGED_SKILL_PATHS, &eligible_dest, "src-migrated");
    registry.mark_with_source(MANAGED_SKILL_PATHS, &orphan_attribution, "src-orphan");
    registry
        .persist(&["src-migrated", "src-orphan"], false, &mut report)
        .unwrap();

    // Now drop orphan_attribution from category membership but leave attribution in lockfile
    let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
    registry.replace_category(MANAGED_SKILL_PATHS);
    registry.mark_with_source(MANAGED_SKILL_PATHS, &eligible_dest, "src-migrated");
    registry
        .persist(&["src-migrated", "src-orphan"], false, &mut report)
        .unwrap();

    // Eligible migration test:
    // Publish direct skill for eligible_dest with matching source_id "src-migrated"
    let source_dir = tmp.path().join("source");
    fs::create_dir_all(&source_dir).unwrap();
    fs::write(source_dir.join("SKILL.md"), "new-content").unwrap();

    let plan = direct_skill_plan(source_dir, root.clone(), "eligible-skill", "src-migrated");
    let evidence = publish_direct_skills(&plan, &lock, false);
    assert!(
        evidence.by_target[&ProjectionTargetId::Codex]
            .failure
            .is_none(),
        "eligible migration must succeed without failure"
    );

    // Verify eligible_dest migrated to direct category with attribution preserved
    let records = direct_records(&lock);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, DirectArtifactKind::Skill);
    assert!(same_recorded_path(&records[0].path, &eligible_dest));
    assert_eq!(records[0].source_id.as_deref(), Some("src-migrated"));

    // Verify authority & ownership checks:
    let authority = ManagedDeprojection::from_lockfile(&lock);
    let registry_after = ManagedArtifactRegistry::load(&lock, &mut report);

    // 1. Eligible migrated path IS authorized
    assert!(authority.is_authorized_for_removal(&eligible_dest));
    assert!(registry_after.is_managed(&eligible_dest));

    // 2. Attribution alone NEVER grants ownership or authorization
    assert!(
        !authority.is_authorized_for_removal(&orphan_attribution),
        "attribution alone must never grant deprojection authority"
    );
    assert!(
        !registry_after.is_managed(&orphan_attribution),
        "attribution alone must never grant registry managed membership"
    );
    assert!(
        records
            .iter()
            .all(|r| !same_recorded_path(&r.path, &orphan_attribution)),
        "attribution alone must not appear in direct_records"
    );

    // 3. Same name alone NEVER grants ownership
    assert!(
        !authority.is_authorized_for_removal(&same_name_unmanaged),
        "same name alone must never grant deprojection authority"
    );
    assert!(
        !registry_after.is_managed(&same_name_unmanaged),
        "same name alone must never grant registry managed membership"
    );
    assert!(
        records
            .iter()
            .all(|r| !same_recorded_path(&r.path, &same_name_unmanaged)),
        "same name alone must not appear in direct_records"
    );

    // 4. Same digest alone NEVER grants ownership
    assert!(
        !authority.is_authorized_for_removal(&same_digest_unmanaged),
        "same digest alone must never grant deprojection authority"
    );
    assert!(
        !registry_after.is_managed(&same_digest_unmanaged),
        "same digest alone must never grant registry managed membership"
    );
    assert!(
        records
            .iter()
            .all(|r| !same_recorded_path(&r.path, &same_digest_unmanaged)),
        "same digest alone must not appear in direct_records"
    );
}

#[test]
fn tp07_spec_machine_scope_complete_and_unavailable_handling() {
    // ── Scenario A: Complete scope with direct, blocked, and machine-owned loose components ──
    let (_temp, mut opts) = empty_machine_options(vec!["copilot".to_string()]);
    opts.dry_run = false;

    // 1. Direct delegated component (pre-existing direct bytes must NOT be rewritten by machine)
    let direct_cmd_file = opts
        .user_home
        .join(".copilot")
        .join("skills")
        .join("direct-cmd")
        .join("SKILL.md");
    fs::create_dir_all(direct_cmd_file.parent().unwrap()).unwrap();
    let direct_custom_bytes = "---\nname: direct-cmd\n---\ncustom direct bytes\n";
    fs::write(&direct_cmd_file, direct_custom_bytes).unwrap();
    write_command_fixture(&opts.source_root, "direct-cmd");

    // 2. Direct delegated component retired from source (must NOT be pruned by machine)
    let retired_delegated_cmd = opts
        .user_home
        .join(".copilot")
        .join("skills")
        .join("retired-cmd")
        .join("SKILL.md");
    fs::create_dir_all(retired_delegated_cmd.parent().unwrap()).unwrap();
    let retired_bytes = format!("{CCYNC_MANAGED_FILE_HEADER}\nretired direct bytes\n");
    fs::write(&retired_delegated_cmd, &retired_bytes).unwrap();

    // 3. Blocked component (must NOT be written by machine)
    let blocked_cmd_file = opts
        .user_home
        .join(".copilot")
        .join("skills")
        .join("blocked-cmd")
        .join("SKILL.md");
    write_command_fixture(&opts.source_root, "blocked-cmd");

    // 4. Machine-owned loose skill from source (must be linked/updated by machine)
    write_skill_fixture(&opts.source_root, "loose-skill");

    // Construct the hand-built scope with direct and blocked decisions
    let scope = scope_with(vec![
        delegation_decision(
            DirectDelegationComponent::CommandSkill,
            ProjectionTargetId::Copilot,
            "update_commands",
            direct_cmd_file.clone(),
            DirectDelegationOutcome::Delegated,
        ),
        delegation_decision(
            DirectDelegationComponent::CommandSkill,
            ProjectionTargetId::Copilot,
            "update_commands",
            retired_delegated_cmd.clone(),
            DirectDelegationOutcome::Delegated,
        ),
        delegation_decision(
            DirectDelegationComponent::CommandSkill,
            ProjectionTargetId::Copilot,
            "update_commands",
            blocked_cmd_file.clone(),
            DirectDelegationOutcome::Blocked,
        ),
    ]);

    let evidence = run_machine_update_with_scope(&opts, &scope).unwrap();

    // Verification of Scenario A:
    // a. Direct component bytes preserved (no machine rewrite)
    assert_eq!(
        fs::read_to_string(&direct_cmd_file).unwrap(),
        direct_custom_bytes,
        "machine update must not rewrite direct component bytes"
    );
    // b. Blocked component destination withheld (no machine write)
    assert!(
        !blocked_cmd_file.exists(),
        "machine update must not write blocked component destination"
    );
    // c. Direct-path pruning withheld
    assert!(
        retired_delegated_cmd.exists(),
        "machine update must not prune delegated direct destination"
    );
    assert_eq!(
        fs::read_to_string(&retired_delegated_cmd).unwrap(),
        retired_bytes
    );
    // d. Machine-owned loose component still updates
    let machine_link = opts.user_home.join(".copilot").join("ccync");
    assert!(
        is_symlink_or_junction(&machine_link),
        "machine-owned loose link must be created/updated"
    );
    // e. Personalization and settings still update
    let settings_file = opts
        .appdata_root
        .join("Code")
        .join("User")
        .join("settings.json");
    assert!(
        settings_file.exists(),
        "personalization settings.json must be created"
    );
    let settings_content = fs::read_to_string(&settings_file).unwrap();
    assert!(
        settings_content.contains("chat.agentSkillsLocations"),
        "personalization settings must contain expected settings"
    );
    assert!(evidence
        .completed_targets
        .contains(&ProjectionTargetId::Copilot));

    // ── Scenario B: Scope-unavailable mode ──
    let (_temp_u, mut opts_u) = empty_machine_options(vec!["copilot".to_string()]);
    opts_u.dry_run = false;
    write_command_fixture(&opts_u.source_root, "fallback-command");
    write_skill_fixture(&opts_u.source_root, "loose-skill");
    let fallback_cmd_file = opts_u
        .user_home
        .join(".copilot")
        .join("skills")
        .join("fallback-command")
        .join("SKILL.md");

    let unavailable_scope = MachineProjectionScope {
        decisions: Vec::new(),
        unavailable: true,
    };
    let unavail_evidence = run_machine_update_with_scope(&opts_u, &unavailable_scope).unwrap();

    // Verification of Scenario B:
    // a. Direct component fallback writes explicitly suppressed
    assert!(
        !fallback_cmd_file.exists(),
        "direct component fallback writes must be suppressed under unavailable scope"
    );
    // b. Scope unavailable warning explicitly reported
    assert!(
        evidence_all_warnings(&unavail_evidence)
            .iter()
            .any(|w| w.contains("scope unavailable")),
        "unavailable scope must be reported in warnings"
    );
    // c. Machine-owned loose components still update
    assert!(
        is_symlink_or_junction(&opts_u.user_home.join(".copilot").join("ccync")),
        "machine-owned loose components must still update under unavailable scope"
    );
    // d. Personalization and settings still update
    let unavail_settings = fs::read_to_string(
        opts_u
            .appdata_root
            .join("Code")
            .join("User")
            .join("settings.json"),
    )
    .unwrap();
    assert!(
        unavail_settings.contains("chat.agentSkillsLocations"),
        "personalization/settings must still update under unavailable scope"
    );
}
