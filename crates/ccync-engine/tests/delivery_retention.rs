use ccync_engine::install::prune_build_except_retained;
use ccync_foundation::paths::EffectiveLayout;
use ccync_foundation::provider::{
    CapabilityInventory, ClientScope, CommittedOwnership, ComponentIdentity, ComponentKind,
    GenerationId, IntentPhase, NativeSelector, PackageDigest, PendingIntent, ProviderGeneration,
    ProviderLifecycleJournal, ProviderLifecycleLock, ProviderSourceKey,
};
use ccync_foundation::target::ProjectionTargetId;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

mod support;
use support::isolated;

/// Lay out `render/providers/demo` with two generations and their views,
/// adding resources inside each generation, and write `lock.json` committing
/// the given reference.
fn retention_fixture(_home_root: &Path, committed_id: &str) -> PathBuf {
    let build = EffectiveLayout::load_or_legacy().unwrap().state_base();
    let providers = build.join("render/providers/demo");

    for dir in [
        "g-0001/codex-only",
        "g-0001.views/codex/v-abc",
        "g-obsolete",
        "g-obsolete.views/codex/v-old",
    ] {
        fs::create_dir_all(providers.join(dir)).unwrap();
    }

    fs::write(
        providers.join("g-0001/resources.json"),
        r#"{"name":"res1"}"#,
    )
    .unwrap();
    fs::write(
        providers.join("g-obsolete/resources.json"),
        r#"{"name":"obsolete"}"#,
    )
    .unwrap();

    let mut lock = json!({});
    ProviderLifecycleLock {
        schema_version: 1,
        committed: vec![CommittedOwnership {
            target: ProjectionTargetId::Codex,
            selector: NativeSelector::new("demo@ccync").unwrap(),
            plugin_name: "demo".to_string(),
            upstream_revision: Some("rev1".to_string()),
            generation: ProviderGeneration {
                source: ProviderSourceKey::new("demo").unwrap(),
                generation: GenerationId::new(committed_id).unwrap(),
                package_digest: PackageDigest::new("sha256:abc").unwrap(),
            },
            capabilities: CapabilityInventory::new(),
            client_scope: Some(ClientScope::new("demo-client").unwrap()),
            source_digest: Some(PackageDigest::new("sha256:src").unwrap()),
            view_digest: Some(PackageDigest::new("sha256:view").unwrap()),
            observed_preimage: None,
            enablement_intent: None,
            group_substep: None,
        }],
    }
    .write_to_lock(&mut lock)
    .unwrap();
    fs::create_dir_all(&build).unwrap();
    fs::write(build.join("lock.json"), serde_json::to_vec(&lock).unwrap()).unwrap();
    build
}

struct IsolatedStorageRoots(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl IsolatedStorageRoots {
    fn new(home: &Path) -> Self {
        let values = [
            ("APPDATA", home.join("AppData/Roaming")),
            ("LOCALAPPDATA", home.join("AppData/Local")),
            ("XDG_CONFIG_HOME", home.join("xdg/config")),
            ("XDG_STATE_HOME", home.join("xdg/state")),
            ("XDG_CACHE_HOME", home.join("xdg/cache")),
        ];
        let prior = values
            .iter()
            .map(|(key, _)| (*key, std::env::var_os(key)))
            .collect();
        for (key, value) in values {
            std::env::set_var(key, value);
        }
        Self(prior)
    }
}

impl Drop for IsolatedStorageRoots {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}

#[test]
fn delivery_retention_committed_view_keeps_its_generation_and_resources() {
    let (home, _lock) = isolated();
    let _roots = IsolatedStorageRoots::new(home.root());
    let build = retention_fixture(home.root(), "v-abc");
    let providers = build.join("render/providers/demo");

    let mut warnings = Vec::new();
    prune_build_except_retained(&build, false, false, true, &mut warnings);

    assert!(
        warnings.is_empty(),
        "expected no warnings, got: {warnings:?}"
    );
    assert!(
        providers.join("g-0001/codex-only").is_dir(),
        "referenced generation directory must be retained"
    );
    assert!(
        providers.join("g-0001/resources.json").is_file(),
        "resources inside referenced generation must be retained"
    );
    assert!(
        providers.join("g-0001.views/codex/v-abc").is_dir(),
        "views directory of referenced generation must be retained"
    );
    assert!(
        !providers.join("g-obsolete").exists(),
        "unreferenced obsolete generation must be pruned"
    );
    assert!(
        !providers.join("g-obsolete.views").exists(),
        "views directory of unreferenced generation must be pruned"
    );
}

#[test]
fn generation_retention_keeps_native_marketplaces_and_committed_view_bytes() {
    let (home, _lock) = isolated();
    let _roots = IsolatedStorageRoots::new(home.root());
    let build = retention_fixture(home.root(), "v-abc");
    let providers = build.join("render/providers");
    let native = providers.join("native-marketplaces/catalog/owned.json");
    fs::create_dir_all(native.parent().unwrap()).unwrap();
    fs::write(&native, "native marketplace survives").unwrap();

    prune_build_except_retained(&build, false, false, true, &mut Vec::new());

    assert_eq!(
        fs::read_to_string(native).unwrap(),
        "native marketplace survives"
    );
    assert_eq!(
        fs::read_to_string(providers.join("demo/g-0001/resources.json")).unwrap(),
        r#"{"name":"res1"}"#
    );
    println!(
        "generation_retention: committed view retained bytes; native-marketplaces retained bytes"
    );
}

#[test]
fn generation_retention_keeps_nested_registry_path_references() {
    let (home, _lock) = isolated();
    let _roots = IsolatedStorageRoots::new(home.root());
    let build = retention_fixture(home.root(), "g-0001");
    let providers = build.join("render/providers/demo");
    let nested = providers.join("g-registry/registry-proof.txt");
    fs::create_dir_all(nested.parent().unwrap()).unwrap();
    fs::write(&nested, "registry envelope bytes").unwrap();
    let nested_view = providers.join("g-registry.views/codex/v-registry/view-proof.txt");
    fs::create_dir_all(nested_view.parent().unwrap()).unwrap();
    fs::write(&nested_view, "registry view bytes").unwrap();

    let lock_path = build.join("lock.json");
    let mut lock: serde_json::Value =
        serde_json::from_slice(&fs::read(&lock_path).unwrap()).unwrap();
    lock["_ccyncProjection"]["standalonePublications"] = json!({
        "proof": {
            "rollback": {
                "preimagePath": "render/providers/demo/g-registry.views/codex/v-registry"
            }
        }
    });
    fs::write(&lock_path, serde_json::to_vec(&lock).unwrap()).unwrap();

    prune_build_except_retained(&build, false, false, true, &mut Vec::new());

    assert_eq!(
        fs::read_to_string(nested).unwrap(),
        "registry envelope bytes"
    );
    assert_eq!(
        fs::read_to_string(nested_view).unwrap(),
        "registry view bytes"
    );
    println!("generation_retention: nested registry preimage path retained bytes");
}

#[test]
fn generation_retention_keeps_standalone_intent_paths_and_reader_proofs() {
    use ccync_foundation::provider::{
        PathRoot, PreimageRef, RelativePath, ScopedPath, StandalonePreimage,
        StandalonePublicationIntent, StandaloneWithdrawalIntent, StandaloneWithdrawalReaderIntent,
        StandaloneWithdrawalReaderProof,
    };

    let (home, _lock) = isolated();
    let _roots = IsolatedStorageRoots::new(home.root());
    let build = retention_fixture(home.root(), "g-0001");
    let providers = build.join("render/providers/demo");
    let ids = ["g-pub-stage", "g-pub-preimage", "g-withdraw-backup"];
    for id in ids {
        fs::create_dir_all(providers.join(format!("{id}/owned"))).unwrap();
        fs::create_dir_all(providers.join(format!("{id}.views/codex/v-{id}/view"))).unwrap();
        fs::write(providers.join(format!("{id}/owned/bytes")), id).unwrap();
        fs::write(
            providers.join(format!("{id}.views/codex/v-{id}/view/bytes")),
            id,
        )
        .unwrap();
    }
    let digest = PackageDigest::new("standalone-v1:fnv1a64:1234").unwrap();
    let destination = ScopedPath {
        root: PathRoot::UserHome,
        path: RelativePath::new(".agents/skills/retained-fixture").unwrap(),
    };
    let pub_backup = RelativePath::new("render/providers/demo/g-pub-preimage/backup").unwrap();
    let withdrawal_backup =
        RelativePath::new("render/providers/demo/g-withdraw-backup/backup").unwrap();
    let journal = ProviderLifecycleJournal {
        standalone_intents: vec![StandalonePublicationIntent {
            operation_id: "retained-publication".into(),
            source: "fixture".into(),
            source_fingerprint: digest.clone(),
            targets: vec!["codex".into()],
            destination: destination.clone(),
            preimage: StandalonePreimage::Tree {
                fingerprint: digest.clone(),
            },
            link_kind: None,
            preimage_ref: Some(PreimageRef {
                path: destination.clone(),
                saved: pub_backup.clone(),
            }),
            preimage_fingerprint: Some(digest.clone()),
            stage_path: RelativePath::new("render/providers/demo/g-pub-stage/stage").unwrap(),
            backup_path: pub_backup,
            expected_tree_fingerprint: digest.clone(),
            registry_before: Some(json!({
                "rollbackPath": "render/providers/demo/g-pub-stage.views/codex/v-g-pub-stage"
            })),
            registry_after: None,
            phase: IntentPhase::Publishing,
        }],
        standalone_withdrawal_intents: vec![StandaloneWithdrawalIntent {
            operation_id: "retained-withdrawal".into(),
            source_id: "fixture".into(),
            source_revision: "sha256:fixture".into(),
            source_fingerprint: digest.clone(),
            readers: vec![StandaloneWithdrawalReaderProof {
                reader: "codex".into(),
                intent: StandaloneWithdrawalReaderIntent::Disabled,
                evidence: "render/providers/demo/g-withdraw-backup.views/codex/v-g-withdraw-backup"
                    .into(),
            }],
            destination: destination.clone(),
            preimage: StandalonePreimage::Tree {
                fingerprint: digest.clone(),
            },
            link_kind: None,
            preimage_ref: PreimageRef {
                path: destination,
                saved: withdrawal_backup.clone(),
            },
            preimage_fingerprint: digest.clone(),
            backup_path: withdrawal_backup,
            registry_before: json!({
                "categories": ["directSkillProjectionPaths"],
                "source": "fixture",
                "standalone": {
                    "path": "retained-fixture",
                    "sourceId": "fixture",
                    "sourceRevision": "sha256:fixture",
                    "sourceFingerprint": digest.as_str(),
                    "treeFingerprint": digest.as_str(),
                    "targets": ["codex"],
                    "generationProof": {
                        "source": "demo",
                        "generation": "g-withdraw-backup"
                    }
                }
            }),
            registry_after: json!({"categories": [], "source": null, "standalone": null}),
            phase: IntentPhase::Publishing,
        }],
        ..Default::default()
    };
    fs::write(
        build.join("projection.txn.json"),
        journal.to_json_string().unwrap(),
    )
    .unwrap();

    prune_build_except_retained(&build, false, false, true, &mut Vec::new());

    for id in ids {
        assert_eq!(
            fs::read_to_string(providers.join(format!("{id}/owned/bytes"))).unwrap(),
            id
        );
        assert_eq!(
            fs::read_to_string(providers.join(format!("{id}.views/codex/v-{id}/view/bytes")))
                .unwrap(),
            id
        );
    }
    println!("generation_retention: standalone stage, preimage, backup, registry, and reader proof bytes retained");
}

#[test]
fn generation_retention_unknown_view_reference_prunes_nothing() {
    let (home, _lock) = isolated();
    let _roots = IsolatedStorageRoots::new(home.root());
    let build = retention_fixture(home.root(), "v-missing");
    let providers = build.join("render/providers/demo");

    let mut warnings = Vec::new();
    prune_build_except_retained(&build, false, false, true, &mut warnings);

    assert!(
        providers.join("g-obsolete").is_dir(),
        "obsolete generation must be kept when reference is unknown"
    );
    assert!(
        providers.join("g-obsolete/resources.json").is_file(),
        "resources must be kept when reference is unknown"
    );
    assert!(
        providers.join("g-obsolete.views/codex/v-old").is_dir(),
        "obsolete views must be kept when reference is unknown"
    );
    assert!(
        providers.join("g-0001.views/codex/v-abc").is_dir(),
        "existing views must be kept when reference is unknown"
    );
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("keeping all provider generations")),
        "expected warning about unresolved reference, got: {warnings:?}"
    );
    println!("generation_retention: unknown view reference retained candidate bytes");
}

#[test]
fn delivery_retention_pending_intent_view_keeps_generation_and_views() {
    let (home, _lock) = isolated();
    let build = home.root().join(".ccync/build");
    let providers = build.join("render/providers/demo");

    for dir in [
        "g-pending/codex-only",
        "g-pending.views/codex/v-intent",
        "g-obsolete",
        "g-obsolete.views/codex/v-old",
    ] {
        fs::create_dir_all(providers.join(dir)).unwrap();
    }

    fs::write(
        providers.join("g-pending/intent-resource.txt"),
        "pending intent payload",
    )
    .unwrap();

    // lock.json has no committed records
    let mut lock = json!({});
    ProviderLifecycleLock {
        schema_version: 1,
        committed: vec![],
    }
    .write_to_lock(&mut lock)
    .unwrap();
    fs::create_dir_all(&build).unwrap();
    fs::write(build.join("lock.json"), serde_json::to_vec(&lock).unwrap()).unwrap();

    // projection.txn.json has pending intent with desired_generation pointing to v-intent
    let journal = ProviderLifecycleJournal {
        schema_version: 1,
        intents: vec![PendingIntent {
            phase: IntentPhase::Publishing,
            client_scope: None,
            source_digest: None,
            view_digest: None,
            observed_preimage: None,
            enablement_intent: None,
            group_substep: None,
            target: ProjectionTargetId::Codex,
            ownership_fingerprints: vec![],
            scoped_paths: vec![],
            preimage_refs: vec![],
            desired_generation: ProviderGeneration {
                source: ProviderSourceKey::new("demo").unwrap(),
                generation: GenerationId::new("v-intent").unwrap(),
                package_digest: PackageDigest::new("sha256:intent").unwrap(),
            },
            previous_generation: None,
        }],
        ..Default::default()
    };
    fs::write(
        build.join("projection.txn.json"),
        journal.to_json_string().unwrap(),
    )
    .unwrap();

    let mut warnings = Vec::new();
    prune_build_except_retained(&build, false, false, true, &mut warnings);

    assert!(
        warnings.is_empty(),
        "expected no warnings, got: {warnings:?}"
    );
    assert!(
        providers.join("g-pending/codex-only").is_dir(),
        "generation referenced by pending intent must be retained"
    );
    assert!(
        providers.join("g-pending/intent-resource.txt").is_file(),
        "resources of generation referenced by pending intent must be retained"
    );
    assert!(
        providers.join("g-pending.views/codex/v-intent").is_dir(),
        "views of generation referenced by pending intent must be retained"
    );
    assert!(
        !providers.join("g-obsolete").exists(),
        "unreferenced obsolete generation must be pruned"
    );
}

#[test]
fn delivery_retention_unreadable_reference_stops_pruning() {
    let (home, _lock) = isolated();
    let build = home.root().join(".ccync/build");
    let providers = build.join("render/providers/demo");

    let generation_path = providers.join("g-0001");
    fs::create_dir_all(&generation_path).unwrap();
    fs::write(generation_path.join("data.txt"), "data").unwrap();
    fs::write(build.join("lock.json"), "{ invalid json syntax").unwrap();

    let mut warnings = Vec::new();
    prune_build_except_retained(&build, false, false, true, &mut warnings);

    assert!(
        generation_path.exists(),
        "unreadable references must not delete provider generations"
    );
    assert!(
        generation_path.join("data.txt").is_file(),
        "unreadable references must retain resources"
    );
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("keeping all provider generations")),
        "expected warning about unreadable references, got: {warnings:?}"
    );
}

#[test]
fn p7_t12_generation_read_by_another_consumer_is_retained() {
    let (home, _lock) = isolated();
    let build = home.root().join(".ccync/build");
    let providers = build.join("render/providers/demo");

    for dir in [
        "g-shared/codex-only",
        "g-shared.views/codex/v-shared",
        "g-next",
        "g-obsolete",
        "g-obsolete.views/codex/v-old",
    ] {
        fs::create_dir_all(providers.join(dir)).unwrap();
    }
    fs::write(
        providers.join("g-shared/resources.json"),
        r#"{"name":"shared"}"#,
    )
    .unwrap();
    fs::write(
        providers.join("g-obsolete/resources.json"),
        r#"{"name":"obsolete"}"#,
    )
    .unwrap();

    // No committed record names g-shared: the owning registration is gone.
    let mut lock = json!({});
    ProviderLifecycleLock {
        schema_version: 1,
        committed: vec![],
    }
    .write_to_lock(&mut lock)
    .unwrap();
    fs::create_dir_all(&build).unwrap();
    fs::write(build.join("lock.json"), serde_json::to_vec(&lock).unwrap()).unwrap();

    // Another target's pending switch still reads g-shared as its previous generation.
    let journal = ProviderLifecycleJournal {
        schema_version: 1,
        intents: vec![PendingIntent {
            phase: IntentPhase::Publishing,
            client_scope: None,
            source_digest: None,
            view_digest: None,
            observed_preimage: None,
            enablement_intent: None,
            group_substep: None,
            target: ProjectionTargetId::Claude,
            ownership_fingerprints: vec![],
            scoped_paths: vec![],
            preimage_refs: vec![],
            desired_generation: ProviderGeneration {
                source: ProviderSourceKey::new("demo").unwrap(),
                generation: GenerationId::new("g-next").unwrap(),
                package_digest: PackageDigest::new("sha256:next").unwrap(),
            },
            previous_generation: Some(ProviderGeneration {
                source: ProviderSourceKey::new("demo").unwrap(),
                generation: GenerationId::new("g-shared").unwrap(),
                package_digest: PackageDigest::new("sha256:shared").unwrap(),
            }),
        }],
        ..Default::default()
    };
    fs::write(
        build.join("projection.txn.json"),
        journal.to_json_string().unwrap(),
    )
    .unwrap();

    let mut warnings = Vec::new();
    prune_build_except_retained(&build, false, false, true, &mut warnings);

    assert!(
        warnings.is_empty(),
        "expected no warnings, got: {warnings:?}"
    );
    assert!(
        providers.join("g-shared/codex-only").is_dir(),
        "generation read by another consumer must be retained"
    );
    assert!(
        providers.join("g-shared/resources.json").is_file(),
        "resources of a generation read by another consumer must be retained"
    );
    assert!(
        providers.join("g-shared.views/codex/v-shared").is_dir(),
        "views of a generation read by another consumer must be retained"
    );
    assert!(
        !providers.join("g-obsolete").exists(),
        "unreferenced obsolete generation must be pruned"
    );
    assert!(!providers.join("g-obsolete.views").exists());
}

fn identity(source: &str, plugin: &str, kind: ComponentKind, id: &str) -> ComponentIdentity {
    ComponentIdentity {
        source: ProviderSourceKey::new(source).unwrap(),
        plugin_name: plugin.to_string(),
        upstream_revision: None,
        kind,
        component_id: id.to_string(),
        origin: None,
        route: None,
    }
}

#[test]
fn tp28_spec_publish_held_skill_write_and_missing_output_retains_failure_and_accumulates_failed() {
    use ccync_engine::delivery_apply::publish;
    use ccync_engine::install::{
        ProjectionPhase, ProjectionRunAccumulator, ProjectionTargetStatus,
    };
    use ccync_engine::plugin_render::RenderedProviderGeneration;
    use ccync_foundation::provider::{
        ComponentKind, GenerationId, PackageDigest, ProviderGeneration, ProviderSourceKey,
    };
    use ccync_foundation::target::ProjectionTargetId as Target;

    let (home, _lock) = isolated();
    let root = home.root();

    let provider_dir = root.join("rendered_provider");
    let skill_dir = provider_dir.join("skills/present_skill");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: present_skill\ndescription: Present skill\n---\nBody\n",
    )
    .unwrap();

    let agent_dir = provider_dir.join("agents");
    fs::create_dir_all(&agent_dir).unwrap();
    fs::write(
        agent_dir.join("helper.md"),
        "---\nname: helper\ndescription: Helper agent\n---\nBody\n",
    )
    .unwrap();

    let build_dir = root.join(".ccync/build");
    fs::create_dir_all(&build_dir).unwrap();
    let lockfile = build_dir.join("lock.json");
    fs::write(&lockfile, "{}").unwrap();

    // Target 1: Claude has a held skill write.
    // Creating a file at `.claude/skills` causes the skill batch write to fail.
    fs::create_dir_all(root.join(".claude")).unwrap();
    fs::write(root.join(".claude/skills"), "file blocking skills dir").unwrap();

    // Target 2: Copilot will have a missing skill route (missing_skill has no dir in provider).

    let generation = RenderedProviderGeneration {
        plugin_id: "example".to_string(),
        plugin_name: "example".to_string(),
        upstream_revision: Some("1.0.0".to_string()),
        generation: ProviderGeneration {
            source: ProviderSourceKey::new("example").unwrap(),
            generation: GenerationId::new("g-1").unwrap(),
            package_digest: PackageDigest::new(
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            )
            .unwrap(),
        },
        path: provider_dir,
        reused: false,
        components: vec![
            identity("example", "example", ComponentKind::Skill, "present_skill"),
            identity("example", "example", ComponentKind::Agent, "helper"),
            identity("example", "example", ComponentKind::Skill, "missing_skill"),
        ],
        component_resources: vec![],
        hooks_declared: false,
    };

    let selected = vec![Target::Claude, Target::Copilot];
    let (evidence, _plan, failed_set) = publish(&[generation], &selected, root, &lockfile);

    // 1. Target 1 (Claude):
    // Failed skill batch followed by successful agent batch leaves the target failure.
    let claude_entry = evidence
        .by_target
        .get(&Target::Claude)
        .expect("Claude entry");
    assert!(
        claude_entry.failure.is_some(),
        "Claude must retain target failure from earlier failed skill batch"
    );
    let claude_fail = claude_entry.failure.as_ref().unwrap();
    assert!(
        !claude_fail.reason.is_empty(),
        "Claude failure must contain error reason"
    );
    // Agent paths should have been merged into Claude entry
    assert!(
        claude_entry
            .paths
            .iter()
            .any(|p| p.path.to_string_lossy().contains("helper")),
        "Claude entry must include agent paths from the successful agent batch"
    );
    assert!(
        failed_set.contains(&Target::Claude),
        "Claude must be in failed set"
    );

    // 2. Target 2 (Copilot):
    // Writer succeeded (no batch error), but missing output was detected by route verification,
    // which added a target failure naming the failed route and reason.
    let copilot_entry = evidence
        .by_target
        .get(&Target::Copilot)
        .expect("Copilot entry");
    assert!(
        copilot_entry.failure.is_some(),
        "Copilot must receive a target failure for missing route output"
    );
    let copilot_fail = copilot_entry.failure.as_ref().unwrap();
    assert!(
        copilot_fail.reason.contains("missing_skill"),
        "Copilot failure reason must name the failed route: {}",
        copilot_fail.reason
    );
    assert!(
        failed_set.contains(&Target::Copilot),
        "Copilot must be in failed set"
    );

    // 3. Accumulator status:
    // A run accumulator built from that evidence derives `Failed` for both targets.
    let mut accumulator = ProjectionRunAccumulator::new("normal", selected.clone());
    accumulator.record(ProjectionPhase::Machine, &evidence);
    let report = accumulator.finish();

    assert!(
        matches!(
            report.targets[&Target::Claude].status,
            ProjectionTargetStatus::Failed { .. }
        ),
        "Accumulator must derive Failed for Claude: {:?}",
        report.targets[&Target::Claude].status
    );
    assert!(
        matches!(
            report.targets[&Target::Copilot].status,
            ProjectionTargetStatus::Failed { .. }
        ),
        "Accumulator must derive Failed for Copilot: {:?}",
        report.targets[&Target::Copilot].status
    );
}

#[test]
fn tp28_spec_existing_target_failure_is_preserved_over_route_verification_summary() {
    use ccync_engine::delivery_apply::publish;
    use ccync_engine::plugin_render::RenderedProviderGeneration;
    use ccync_foundation::provider::{
        GenerationId, PackageDigest, ProviderGeneration, ProviderSourceKey,
    };
    use ccync_foundation::target::ProjectionTargetId as Target;

    let (home, _lock) = isolated();
    let root = home.root();

    let provider_dir = root.join("rendered_provider");
    let skill_dir = provider_dir.join("skills/present_skill");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: present_skill\ndescription: Present skill\n---\nBody\n",
    )
    .unwrap();

    let agent_dir = provider_dir.join("agents");
    fs::create_dir_all(&agent_dir).unwrap();
    fs::write(
        agent_dir.join("helper.md"),
        "---\nname: helper\ndescription: Helper agent\n---\nBody\n",
    )
    .unwrap();

    let build_dir = root.join(".ccync/build");
    fs::create_dir_all(&build_dir).unwrap();
    let lockfile = build_dir.join("lock.json");
    fs::write(&lockfile, "{}").unwrap();

    // Block skills directory with a regular file
    fs::create_dir_all(root.join(".claude")).unwrap();
    fs::write(root.join(".claude/skills"), "file blocking skills dir").unwrap();

    let generation = RenderedProviderGeneration {
        plugin_id: "example".to_string(),
        plugin_name: "example".to_string(),
        upstream_revision: Some("1.0.0".to_string()),
        generation: ProviderGeneration {
            source: ProviderSourceKey::new("example").unwrap(),
            generation: GenerationId::new("g-1").unwrap(),
            package_digest: PackageDigest::new(
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            )
            .unwrap(),
        },
        path: provider_dir,
        reused: false,
        components: vec![
            identity("example", "example", ComponentKind::Skill, "present_skill"),
            identity("example", "example", ComponentKind::Agent, "helper"),
        ],
        component_resources: vec![],
        hooks_declared: false,
    };

    let selected = vec![Target::Claude];
    let (evidence, _plan, _failed_set) = publish(&[generation], &selected, root, &lockfile);

    let claude_entry = evidence
        .by_target
        .get(&Target::Claude)
        .expect("Claude entry");
    let claude_fail = claude_entry.failure.as_ref().expect("failure must be kept");
    // Existing failure from skill batch is kept and NOT replaced by "routes did not publish" summary
    assert!(
        !claude_fail.reason.starts_with("routes did not publish:"),
        "Existing failure must not be replaced by route verification summary: {}",
        claude_fail.reason
    );
}

#[test]
fn p1_standalone_pending_private_stage_and_backup_survive_build_prune() {
    use ccync_foundation::provider::{
        PathRoot, RelativePath, ScopedPath, StandalonePreimage, StandalonePublicationIntent,
    };
    let (home, _lock) = isolated();
    let build = home.root().join(".ccync/build");
    fs::create_dir_all(build.join("standalone-publications/p1-pending/stage")).unwrap();
    fs::create_dir_all(build.join("standalone-publications/p1-pending/backup")).unwrap();
    fs::create_dir_all(build.join("render/skills/source-proof")).unwrap();
    fs::write(
        build.join("standalone-publications/p1-pending/backup/user.txt"),
        "retained preimage",
    )
    .unwrap();
    fs::write(build.join("lock.json"), "{}").unwrap();
    fs::write(build.join("unrelated.tmp"), "orphan").unwrap();
    let mut journal = ProviderLifecycleJournal::default();
    journal
        .standalone_intents
        .push(StandalonePublicationIntent {
            operation_id: "p1-pending".into(),
            source: r#"{"origin":"loose:fixture","revision":"sha256:fixture"}"#.into(),
            source_fingerprint: PackageDigest::new("standalone-v1:fnv1a64:1234").unwrap(),
            targets: vec!["codex".into()],
            destination: ScopedPath {
                root: PathRoot::UserHome,
                path: RelativePath::new(".agents/skills/p1-fixture").unwrap(),
            },
            preimage: StandalonePreimage::Absent,
            link_kind: None,
            preimage_ref: None,
            preimage_fingerprint: None,
            stage_path: RelativePath::new("standalone-publications/p1-pending/stage").unwrap(),
            backup_path: RelativePath::new("standalone-publications/p1-pending/backup").unwrap(),
            expected_tree_fingerprint: PackageDigest::new("standalone-v1:fnv1a64:1234").unwrap(),
            registry_before: Some(json!({"source":null})),
            registry_after: Some(json!({"source":"loose:fixture"})),
            phase: IntentPhase::Publishing,
        });
    fs::write(
        build.join("projection.txn.json"),
        journal.to_json_string().unwrap(),
    )
    .unwrap();
    let mut warnings = Vec::new();
    prune_build_except_retained(&build, false, false, false, &mut warnings);
    assert!(build.join("projection.txn.json").is_file());
    assert!(build.join("lock.json").is_file());
    assert!(build
        .join("standalone-publications/p1-pending/stage")
        .is_dir());
    assert_eq!(
        fs::read_to_string(build.join("standalone-publications/p1-pending/backup/user.txt"))
            .unwrap(),
        "retained preimage"
    );
    assert!(build.join("render/skills/source-proof").is_dir());
    assert!(!build.join("unrelated.tmp").exists());
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("standalone publication evidence retained")));
}

#[test]
fn p1_standalone_unreferenced_post_clear_backup_is_preserved_without_invented_authority() {
    let (home, _lock) = isolated();
    let build = home.root().join(".ccync/build");
    let backup = build.join("standalone-publications/interrupted-after-clear/backup");
    fs::create_dir_all(&backup).unwrap();
    fs::write(backup.join("SKILL.md"), "unknown retained old tree").unwrap();
    fs::write(build.join("lock.json"), "{}").unwrap();
    fs::write(
        build.join("projection.txn.json"),
        ProviderLifecycleJournal::default()
            .to_json_string()
            .unwrap(),
    )
    .unwrap();
    let mut warnings = Vec::new();
    prune_build_except_retained(&build, false, false, false, &mut warnings);
    assert_eq!(
        fs::read_to_string(backup.join("SKILL.md")).unwrap(),
        "unknown retained old tree"
    );
    assert!(build.join("lock.json").exists());
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("cleanup verification required")));
}

#[test]
fn p1_standalone_withdrawal_only_journal_alone_survives_build_prune() {
    use ccync_foundation::provider::{
        PathRoot, PreimageRef, RelativePath, ScopedPath, StandalonePreimage,
        StandaloneWithdrawalIntent, StandaloneWithdrawalReaderIntent,
        StandaloneWithdrawalReaderProof,
    };
    let (home, _lock) = isolated();
    let build = home.root().join(".ccync/build");
    // No private directory exists, so only the journal collection can retain state.
    fs::create_dir_all(build.join("render")).unwrap();
    fs::write(build.join("lock.json"), "{}").unwrap();
    fs::write(build.join("unrelated.tmp"), "orphan").unwrap();
    let digest = PackageDigest::new("standalone-v1:fnv1a64:1234").unwrap();
    let destination = ScopedPath {
        root: PathRoot::UserHome,
        path: RelativePath::new(".agents/skills/p1-fixture").unwrap(),
    };
    let backup_path = RelativePath::new("standalone-withdrawals/p1-pending/backup").unwrap();
    let mut journal = ProviderLifecycleJournal::default();
    journal
        .standalone_withdrawal_intents
        .push(StandaloneWithdrawalIntent {
            operation_id: "p1-pending".into(),
            source_id: "loose-origin:fixture".into(),
            source_revision: "sha256:fixture".into(),
            source_fingerprint: digest.clone(),
            readers: vec![StandaloneWithdrawalReaderProof {
                reader: "codex".into(),
                intent: StandaloneWithdrawalReaderIntent::Disabled,
                evidence: "source-removed:loose-origin:fixture".into(),
            }],
            destination: destination.clone(),
            preimage: StandalonePreimage::Tree {
                fingerprint: digest.clone(),
            },
            link_kind: None,
            preimage_ref: PreimageRef {
                path: destination,
                saved: backup_path.clone(),
            },
            preimage_fingerprint: digest.clone(),
            backup_path,
            registry_before: json!({
                "categories": ["directSkillProjectionPaths"],
                "source": "loose-origin:fixture",
                "standalone": {
                    "path": "p1-fixture",
                    "sourceId": "loose-origin:fixture",
                    "sourceRevision": "sha256:fixture",
                    "sourceFingerprint": digest.as_str(),
                    "treeFingerprint": digest.as_str(),
                    "targets": ["codex"],
                },
            }),
            registry_after: json!({"categories": [], "source": null, "standalone": null}),
            phase: IntentPhase::Publishing,
        });
    fs::write(
        build.join("projection.txn.json"),
        journal.to_json_string().unwrap(),
    )
    .unwrap();
    let mut warnings = Vec::new();
    prune_build_except_retained(&build, false, false, false, &mut warnings);
    assert!(build.join("projection.txn.json").is_file());
    assert!(build.join("lock.json").is_file());
    assert!(build.join("render").is_dir());
    assert!(!build.join("unrelated.tmp").exists());
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("standalone publication evidence retained")));
}

#[test]
fn p1_standalone_unreferenced_withdrawal_backup_is_preserved_without_invented_authority() {
    let (home, _lock) = isolated();
    let build = home.root().join(".ccync/build");
    let backup = build.join("standalone-withdrawals/interrupted-after-clear/backup");
    fs::create_dir_all(&backup).unwrap();
    fs::write(backup.join("SKILL.md"), "unknown retained withdrawn tree").unwrap();
    fs::write(build.join("lock.json"), "{}").unwrap();
    fs::write(
        build.join("projection.txn.json"),
        ProviderLifecycleJournal::default()
            .to_json_string()
            .unwrap(),
    )
    .unwrap();
    let mut warnings = Vec::new();
    prune_build_except_retained(&build, false, false, false, &mut warnings);
    assert_eq!(
        fs::read_to_string(backup.join("SKILL.md")).unwrap(),
        "unknown retained withdrawn tree"
    );
    assert!(build.join("lock.json").exists());
}

#[test]
fn p1_standalone_emptied_operation_directories_do_not_retain_build_state() {
    let (home, _lock) = isolated();
    let build = home.root().join(".ccync/build");
    for private in [
        "standalone-publications/finished-op",
        "standalone-withdrawals/finished-op",
    ] {
        fs::create_dir_all(build.join(private)).unwrap();
    }
    fs::create_dir_all(build.join("render")).unwrap();
    fs::write(build.join("lock.json"), "{}").unwrap();
    fs::write(
        build.join("projection.txn.json"),
        ProviderLifecycleJournal::default()
            .to_json_string()
            .unwrap(),
    )
    .unwrap();
    let mut warnings = Vec::new();
    prune_build_except_retained(&build, false, false, false, &mut warnings);
    assert!(!build.join("lock.json").exists());
    assert!(!build.join("render").exists());
    assert!(!warnings
        .iter()
        .any(|warning| warning.contains("evidence retained")));
}
