use super::*;
use ccync_foundation::provider::{ComponentKind, ProviderSourceKey};
use std::fs;
use tempfile::TempDir;

#[test]
fn direct_skills_publish_resources_to_planned_roots_and_skip_served_targets() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source").join("Original");
    put(
        &source,
        "SKILL.md",
        "# Original\n\n[resource](resources/data.txt)",
    );
    put(&source, "resources/data.txt", "resource");
    let agents = temp.path().join(".agents/skills");
    let copilot = temp.path().join(".copilot/skills");
    let plan = DirectSkillsPlan {
        packages: vec![DirectSkillPackage {
            source_id: "src-one".into(),
            original_name: "Original".into(),
            alias: "Original".into(),
            source_path: source,
        }],
        roots: vec![
            DirectSkillRoot {
                path: agents.clone(),
                targets: vec![ProjectionTargetId::Codex],
            },
            DirectSkillRoot {
                path: copilot.clone(),
                targets: vec![ProjectionTargetId::Copilot],
            },
        ],
        served_targets: vec![ProjectionTargetId::Opencode],
    };
    let evidence = publish_direct_skills(&plan, &temp.path().join("lock.json"), false);
    for root in [&agents, &copilot] {
        assert_eq!(
            fs::read_to_string(root.join("Original/resources/data.txt")).unwrap(),
            "resource"
        );
        assert!(fs::read_to_string(root.join("Original/SKILL.md"))
            .unwrap()
            .contains("[resource](resources/data.txt)"));
    }
    assert_eq!(
        evidence.by_target[&ProjectionTargetId::Opencode]
            .skipped
            .as_deref(),
        Some("served by a planned shared skill root")
    );
}

#[test]
fn direct_skills_publish_provider_qualified_alias_and_preserves_vscode_setting() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source").join("alpha-skill");
    put(&source, "SKILL.md", "# alpha\n[ref](resources/config.json)");
    put(&source, "resources/config.json", "{}");

    let agents_root = temp.path().join(".agents/skills");
    let copilot_root = temp.path().join(".copilot/skills");
    let plan = DirectSkillsPlan {
        packages: vec![DirectSkillPackage {
            source_id: "src-one".into(),
            original_name: "alpha-skill".into(),
            alias: "provider-alpha-skill".into(),
            source_path: source,
        }],
        roots: vec![
            DirectSkillRoot {
                path: agents_root.clone(),
                targets: vec![ProjectionTargetId::Codex],
            },
            DirectSkillRoot {
                path: copilot_root.clone(),
                targets: vec![ProjectionTargetId::Copilot],
            },
        ],
        served_targets: vec![],
    };

    let _evidence = publish_direct_skills(&plan, &temp.path().join("lock.json"), false);
    for root in [&agents_root, &copilot_root] {
        let published_dir = root.join("provider-alpha-skill");
        assert!(published_dir.exists(), "must publish under alias");
        assert_eq!(
            fs::read_to_string(published_dir.join("resources/config.json")).unwrap(),
            "{}"
        );
        assert!(fs::read_to_string(published_dir.join("SKILL.md"))
            .unwrap()
            .contains("[ref](resources/config.json)"));
    }

    let mut vscode = serde_json::Map::new();
    set_vscode_skills_bridge(&mut vscode);
    assert_eq!(
        vscode["chat.agentSkillsLocations"]["~/.agents/skills"],
        serde_json::Value::Bool(false)
    );
}

#[test]
fn direct_skills_preserve_unowned_destination_with_empty_registry() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    put(&source, "SKILL.md", "new");
    let dest = temp.path().join("root/Original");
    put(&dest, "SKILL.md", "user");
    let before = fs::read(dest.join("SKILL.md")).unwrap();
    let plan = DirectSkillsPlan {
        packages: vec![DirectSkillPackage {
            source_id: "src-one".into(),
            original_name: "Original".into(),
            alias: "Original".into(),
            source_path: source,
        }],
        roots: vec![DirectSkillRoot {
            path: dest.parent().unwrap().to_path_buf(),
            targets: vec![ProjectionTargetId::Codex],
        }],
        served_targets: vec![],
    };
    let evidence = publish_direct_skills(&plan, &temp.path().join("empty-lock.json"), false);
    assert_eq!(fs::read(dest.join("SKILL.md")).unwrap(), before);
    assert!(evidence.by_target[&ProjectionTargetId::Codex]
        .failure
        .is_some());
}

#[test]
fn direct_skill_plan_publishes_only_the_assigned_package_root_pairs() {
    let temp = TempDir::new().unwrap();
    let alpha = temp.path().join("source").join("alpha");
    let beta = temp.path().join("source").join("beta");
    put(&alpha, "SKILL.md", "alpha");
    put(&beta, "SKILL.md", "beta");
    let agents = temp.path().join(".agents/skills");
    let copilot = temp.path().join(".copilot/skills");
    let publication = DirectSkillPublication {
        assignments: vec![
            direct_assignment(
                "src-alpha",
                "alpha",
                &alpha,
                &agents,
                ProjectionTargetId::Codex,
            ),
            direct_assignment(
                "src-beta",
                "beta",
                &beta,
                &copilot,
                ProjectionTargetId::Copilot,
            ),
        ],
        served_targets: vec![],
    };

    let evidence = publish_direct_skill_plan(&publication, &temp.path().join("lock.json"), false);

    assert_eq!(
        fs::read_to_string(agents.join("alpha/SKILL.md")).unwrap(),
        "alpha"
    );
    assert_eq!(
        fs::read_to_string(copilot.join("beta/SKILL.md")).unwrap(),
        "beta"
    );
    assert!(
        !agents.join("beta").exists(),
        "beta was not assigned to Codex's root"
    );
    assert!(
        !copilot.join("alpha").exists(),
        "alpha was not assigned to Copilot's root"
    );
    for target in [ProjectionTargetId::Codex, ProjectionTargetId::Copilot] {
        assert!(evidence.by_target[&target].failure.is_none());
        assert_eq!(evidence.by_target[&target].paths.len(), 1);
    }
}

#[test]
fn direct_skill_plan_writes_a_shared_destination_once_for_all_its_targets() {
    let temp = TempDir::new().unwrap();
    let alpha = temp.path().join("source").join("alpha");
    put(&alpha, "SKILL.md", "alpha");
    let shared = temp.path().join(".agents/skills");
    let publication = DirectSkillPublication {
        assignments: vec![
            direct_assignment(
                "src-alpha",
                "alpha",
                &alpha,
                &shared,
                ProjectionTargetId::Codex,
            ),
            direct_assignment(
                "src-alpha",
                "alpha",
                &alpha,
                &shared,
                ProjectionTargetId::Copilot,
            ),
        ],
        served_targets: vec![],
    };

    let evidence = publish_direct_skill_plan(&publication, &temp.path().join("lock.json"), false);

    assert_eq!(
        fs::read_to_string(shared.join("alpha/SKILL.md")).unwrap(),
        "alpha"
    );
    for target in [ProjectionTargetId::Codex, ProjectionTargetId::Copilot] {
        assert!(evidence.by_target[&target].failure.is_none());
        assert_eq!(
            evidence.by_target[&target].paths.len(),
            1,
            "one destination must be reported once per target"
        );
    }
}

#[test]
fn direct_skill_plan_rejects_a_second_package_for_a_claimed_destination() {
    let temp = TempDir::new().unwrap();
    let first = temp.path().join("source").join("first");
    let second = temp.path().join("source").join("second");
    put(&first, "SKILL.md", "first");
    put(&second, "SKILL.md", "second");
    let shared = temp.path().join(".agents/skills");
    let publication = DirectSkillPublication {
        assignments: vec![
            direct_assignment(
                "src-first",
                "shared-name",
                &first,
                &shared,
                ProjectionTargetId::Codex,
            ),
            direct_assignment(
                "src-second",
                "shared-name",
                &second,
                &shared,
                ProjectionTargetId::Copilot,
            ),
        ],
        served_targets: vec![],
    };

    let evidence = publish_direct_skill_plan(&publication, &temp.path().join("lock.json"), false);

    assert_eq!(
        fs::read_to_string(shared.join("shared-name/SKILL.md")).unwrap(),
        "first"
    );
    assert!(evidence.by_target[&ProjectionTargetId::Copilot]
        .failure
        .is_some());
}

#[cfg(windows)]
#[test]
fn direct_skill_plan_failed_replacement_keeps_old_bytes_and_ownership() {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_NONE: u32 = 0;

    let temp = TempDir::new().unwrap();
    let lock = temp.path().join("lock.json");
    let root = temp.path().join(".agents/skills");
    let old_source = temp.path().join("old");
    let new_source = temp.path().join("new");
    put(&old_source, "SKILL.md", "old");
    put(&new_source, "SKILL.md", "new");
    let first = DirectSkillPublication {
        assignments: vec![direct_assignment(
            "src-alpha",
            "alpha",
            &old_source,
            &root,
            ProjectionTargetId::Codex,
        )],
        served_targets: vec![],
    };
    publish_direct_skill_plan(&first, &lock, false);

    // An open handle without read sharing blocks the swap that replaces the tree.
    let dest = root.join("alpha");
    put(&dest, "held.txt", "held open");
    let _locked = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_NONE)
        .open(dest.join("held.txt"))
        .unwrap();
    let second = DirectSkillPublication {
        assignments: vec![direct_assignment(
            "src-alpha",
            "alpha",
            &new_source,
            &root,
            ProjectionTargetId::Codex,
        )],
        served_targets: vec![],
    };

    let evidence = publish_direct_skill_plan(&second, &lock, false);

    assert!(evidence.by_target[&ProjectionTargetId::Codex]
        .failure
        .is_some());
    assert_eq!(fs::read_to_string(dest.join("SKILL.md")).unwrap(), "old");
    let records = direct_records(&lock);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, DirectArtifactKind::Skill);
    assert_eq!(records[0].source_id.as_deref(), Some("src-alpha"));
}

#[test]
fn tp03_spec_publish_disjoint_skill_assignments_deduplicate_and_preserve_on_failure() {
    let temp = TempDir::new().unwrap();
    let lock = temp.path().join("lock.json");

    // 1. Setup disjoint sources for two providers plus a shared provider
    let source_a = temp.path().join("source-a").join("skill-a");
    put(&source_a, "SKILL.md", "# Skill A\n[res](resources/a.txt)");
    put(&source_a, "resources/a.txt", "content-a");

    let source_b = temp.path().join("source-b").join("skill-b");
    put(&source_b, "SKILL.md", "# Skill B\n[res](resources/b.txt)");
    put(&source_b, "resources/b.txt", "content-b");

    let source_shared = temp.path().join("source-shared").join("skill-shared");
    put(
        &source_shared,
        "SKILL.md",
        "# Shared\n[res](resources/s.txt)",
    );
    put(&source_shared, "resources/s.txt", "content-s");

    let root_agents = temp.path().join(".agents/skills");
    let root_copilot = temp.path().join(".copilot/skills");
    let root_shared = temp.path().join(".shared/skills");

    // 2. Publish disjoint assignments + deduplicated shared destination
    let publication = DirectSkillPublication {
        assignments: vec![
            direct_assignment(
                "src-provider-a",
                "skill-a",
                &source_a,
                &root_agents,
                ProjectionTargetId::Codex,
            ),
            direct_assignment(
                "src-provider-b",
                "skill-b",
                &source_b,
                &root_copilot,
                ProjectionTargetId::Copilot,
            ),
            direct_assignment(
                "src-provider-shared",
                "skill-shared",
                &source_shared,
                &root_shared,
                ProjectionTargetId::Codex,
            ),
            direct_assignment(
                "src-provider-shared",
                "skill-shared",
                &source_shared,
                &root_shared,
                ProjectionTargetId::Copilot,
            ),
        ],
        served_targets: vec![ProjectionTargetId::Opencode],
    };

    let evidence = publish_direct_skill_plan(&publication, &lock, false);

    // Verify disjoint outputs: only assigned pairs exist
    assert_eq!(
        fs::read_to_string(root_agents.join("skill-a/SKILL.md")).unwrap(),
        "# Skill A\n[res](resources/a.txt)"
    );
    assert_eq!(
        fs::read_to_string(root_agents.join("skill-a/resources/a.txt")).unwrap(),
        "content-a"
    );
    assert!(
        !root_agents.join("skill-b").exists(),
        "skill-b was never assigned to root_agents"
    );

    assert_eq!(
        fs::read_to_string(root_copilot.join("skill-b/SKILL.md")).unwrap(),
        "# Skill B\n[res](resources/b.txt)"
    );
    assert_eq!(
        fs::read_to_string(root_copilot.join("skill-b/resources/b.txt")).unwrap(),
        "content-b"
    );
    assert!(
        !root_copilot.join("skill-a").exists(),
        "skill-a was never assigned to root_copilot"
    );

    // Verify deduplicated shared destination
    assert_eq!(
        fs::read_to_string(root_shared.join("skill-shared/SKILL.md")).unwrap(),
        "# Shared\n[res](resources/s.txt)"
    );
    assert_eq!(
        fs::read_to_string(root_shared.join("skill-shared/resources/s.txt")).unwrap(),
        "content-s"
    );
    assert!(evidence.by_target[&ProjectionTargetId::Codex]
        .failure
        .is_none());
    assert!(evidence.by_target[&ProjectionTargetId::Copilot]
        .failure
        .is_none());
    assert_eq!(
        evidence.by_target[&ProjectionTargetId::Opencode]
            .skipped
            .as_deref(),
        Some("served by a planned shared skill root")
    );

    // Verify direct records in lock.json
    let records = direct_records(&lock);
    assert_eq!(
        records.len(),
        3,
        "three distinct physical skill destinations"
    );
    for rec in &records {
        assert_eq!(rec.kind, DirectArtifactKind::Skill);
    }
    let target_a = root_agents.join("skill-a");
    let target_b = root_copilot.join("skill-b");
    let target_shared = root_shared.join("skill-shared");
    assert!(
        records
            .iter()
            .any(|r| r.source_id.as_deref() == Some("src-provider-a")
                && same_path(&r.path, &target_a))
    );
    assert!(
        records
            .iter()
            .any(|r| r.source_id.as_deref() == Some("src-provider-b")
                && same_path(&r.path, &target_b))
    );
    assert!(records
        .iter()
        .any(|r| r.source_id.as_deref() == Some("src-provider-shared")
            && same_path(&r.path, &target_shared)));

    // 3. Injected replacement failure keeps old bytes and ownership
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_NONE: u32 = 0;

        let locked_target = root_agents.join("skill-a");
        let held_file = locked_target.join("held.txt");
        fs::write(&held_file, "lock-handle").unwrap();
        let _handle = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_NONE)
            .open(&held_file)
            .unwrap();

        let updated_source_a = temp.path().join("source-a-updated").join("skill-a");
        put(&updated_source_a, "SKILL.md", "# Skill A Updated");

        let second_pub = DirectSkillPublication {
            assignments: vec![direct_assignment(
                "src-provider-a",
                "skill-a",
                &updated_source_a,
                &root_agents,
                ProjectionTargetId::Codex,
            )],
            served_targets: vec![],
        };

        let second_evidence = publish_direct_skill_plan(&second_pub, &lock, false);
        assert!(second_evidence.by_target[&ProjectionTargetId::Codex]
            .failure
            .is_some());

        // Previous bytes intact
        assert_eq!(
            fs::read_to_string(root_agents.join("skill-a/SKILL.md")).unwrap(),
            "# Skill A\n[res](resources/a.txt)"
        );
        // Direct record intact
        let records_after = direct_records(&lock);
        assert!(records_after
            .iter()
            .any(|r| r.source_id.as_deref() == Some("src-provider-a")
                && same_path(&r.path, &target_a)));
    }
}

fn same_path(a: &std::path::Path, b: &std::path::Path) -> bool {
    crate::normalize_managed_artifact_path(a) == crate::normalize_managed_artifact_path(b)
}

fn direct_assignment(
    source_id: &str,
    name: &str,
    source: &std::path::Path,
    root: &std::path::Path,
    target: ProjectionTargetId,
) -> DirectSkillAssignment {
    DirectSkillAssignment {
        package: DirectSkillPackage {
            source_id: source_id.into(),
            original_name: name.into(),
            alias: name.into(),
            source_path: source.to_path_buf(),
        },
        root: DirectSkillRoot {
            path: root.to_path_buf(),
            targets: vec![target],
        },
    }
}

fn put(root: &std::path::Path, relative: &str, value: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, value).unwrap();
}

fn identity(source: &str, plugin: &str, id: &str) -> ComponentIdentity {
    ComponentIdentity::new(
        ProviderSourceKey::new(source).unwrap(),
        plugin,
        None,
        ComponentKind::Skill,
        id,
    )
    .unwrap()
}

#[test]
fn provider_alias_keeps_unique_names_and_qualifies_collisions_stably() {
    let inventory = ProviderProjectionInventory {
        identities: vec![
            identity("src-one", "alpha", "shared"),
            identity("src-two", "beta", "shared"),
            identity("demo", "demo", "demo-pipeline"),
        ],
        aliases: vec![],
    };
    let aliases = plan_provider_aliases(ProjectionTargetId::Opencode, &inventory).unwrap();
    assert_eq!(aliases[0].alias, "alpha-src-one-shared");
    assert_eq!(aliases[1].alias, "beta-src-two-shared");
    assert_eq!(aliases[2].alias, "demo-pipeline");
    let persisted = ProviderProjectionInventory {
        identities: inventory.identities.clone(),
        aliases: aliases.clone(),
    };
    assert_eq!(
        plan_provider_aliases(ProjectionTargetId::Opencode, &persisted).unwrap(),
        aliases
    );
}

#[test]
fn provider_alias_rejects_ambiguous_generated_reference() {
    let identities = vec![
        identity("src-one", "alpha", "shared"),
        identity("src-two", "beta", "shared"),
    ];
    let aliases = plan_provider_aliases(
        ProjectionTargetId::Copilot,
        &ProviderProjectionInventory {
            identities: identities.clone(),
            aliases: vec![],
        },
    )
    .unwrap();
    let error = support::resolve_provider_reference(
        "shared",
        &identities,
        &aliases,
        ProjectionTargetId::Copilot,
    )
    .unwrap_err();
    assert!(error.to_string().contains("ambiguous provider reference"));
}

#[test]
fn provider_alias_survives_temporary_removal_and_readdition() {
    let one = identity("src-one", "alpha", "shared");
    let two = identity("src-two", "beta", "shared");
    let both = ProviderProjectionInventory {
        identities: vec![one.clone(), two.clone()],
        aliases: vec![],
    };
    let aliases = plan_provider_aliases(ProjectionTargetId::Opencode, &both).unwrap();
    let only_one = ProviderProjectionInventory {
        identities: vec![one.clone()],
        aliases: aliases.clone(),
    };
    let reduced = plan_provider_aliases(ProjectionTargetId::Opencode, &only_one).unwrap();
    assert_eq!(reduced[0].alias, "alpha-src-one-shared");
    assert_eq!(reduced.len(), 2, "removed provider alias remains persisted");
    let restored = ProviderProjectionInventory {
        identities: vec![one, two],
        aliases: reduced,
    };
    assert_eq!(
        plan_provider_aliases(ProjectionTargetId::Opencode, &restored).unwrap(),
        aliases
    );
}

#[test]
fn provider_alias_rewrites_generated_refs_but_preserves_opaque_text() {
    let identities = vec![
        identity("src-one", "alpha", "shared"),
        identity("src-two", "beta", "shared"),
        identity("demo", "demo", "demo-pipeline"),
    ];
    let inventory = ProviderProjectionInventory {
        identities,
        aliases: vec![],
    };
    let aliases = plan_provider_aliases(ProjectionTargetId::Opencode, &inventory).unwrap();
    let rendered = rewrite_generated_provider_references(
        "Run /demo-pipeline, then refer to upstream shared by name.",
        &inventory,
        &aliases,
        ProjectionTargetId::Opencode,
    )
    .unwrap();
    assert_eq!(
        rendered,
        "Run /demo-pipeline, then refer to upstream shared by name."
    );
    assert!(rewrite_generated_provider_references(
        "Run /shared now.",
        &inventory,
        &aliases,
        ProjectionTargetId::Opencode,
    )
    .unwrap_err()
    .to_string()
    .contains("ambiguous provider reference"));
}

#[test]
fn native_provider_namespace_conflict_preserves_both_sources_as_error() {
    let inventory = ProviderProjectionInventory {
        identities: vec![
            identity("src-one", "same-plugin", "first"),
            identity("src-two", "same-plugin", "second"),
        ],
        aliases: vec![],
    };
    let error = plan_provider_aliases(ProjectionTargetId::Claude, &inventory).unwrap_err();
    assert!(error
        .to_string()
        .contains("native provider namespace conflict"));
    assert!(error.to_string().contains("src-one"));
    assert!(error.to_string().contains("src-two"));
}

#[test]
fn native_provider_distinct_plugins_produce_unqualified_aliases() {
    let inventory = ProviderProjectionInventory {
        identities: vec![
            identity("src-one", "plugin-one", "first"),
            identity("src-two", "plugin-two", "second"),
        ],
        aliases: vec![],
    };
    let aliases_claude = plan_provider_aliases(ProjectionTargetId::Claude, &inventory).unwrap();
    assert_eq!(aliases_claude.len(), 2);
    assert_eq!(aliases_claude[0].alias, "first");
    assert_eq!(aliases_claude[1].alias, "second");
}

#[test]
fn provider_reference_resolution_unique_and_unknown() {
    let inventory = ProviderProjectionInventory {
        identities: vec![
            identity("src-one", "alpha", "collide"),
            identity("src-two", "beta", "collide"),
            identity("demo", "demo", "demo-pipeline"),
        ],
        aliases: vec![],
    };
    let aliases = plan_provider_aliases(ProjectionTargetId::Opencode, &inventory).unwrap();

    let unique = resolve_provider_reference(
        "demo-pipeline",
        &inventory,
        &aliases,
        ProjectionTargetId::Opencode,
    )
    .unwrap();
    assert_eq!(unique, Some("demo-pipeline".to_string()));

    let unknown = resolve_provider_reference(
        "unknown-tool",
        &inventory,
        &aliases,
        ProjectionTargetId::Opencode,
    )
    .unwrap();
    assert_eq!(unknown, None);

    let ambiguous = resolve_provider_reference(
        "collide",
        &inventory,
        &aliases,
        ProjectionTargetId::Opencode,
    );
    assert!(ambiguous.is_err());
    assert!(ambiguous
        .unwrap_err()
        .to_string()
        .contains("ambiguous provider reference"));
}

#[test]
fn provider_alias_deterministic_regardless_of_input_order() {
    let a = identity("src-a", "plugin-a", "common");
    let b = identity("src-b", "plugin-b", "common");
    let inv1 = ProviderProjectionInventory {
        identities: vec![a.clone(), b.clone()],
        aliases: vec![],
    };
    let inv2 = ProviderProjectionInventory {
        identities: vec![b.clone(), a.clone()],
        aliases: vec![],
    };
    let res1 = plan_provider_aliases(ProjectionTargetId::Opencode, &inv1).unwrap();
    let res2 = plan_provider_aliases(ProjectionTargetId::Opencode, &inv2).unwrap();
    for id in [&a, &b] {
        let alias1 = res1.iter().find(|x| x.identity == *id).unwrap();
        let alias2 = res2.iter().find(|x| x.identity == *id).unwrap();
        assert_eq!(alias1.alias, alias2.alias);
    }
}

#[test]
fn provider_alias_flat_targets_consistency() {
    let inventory = ProviderProjectionInventory {
        identities: vec![
            identity("src-one", "alpha", "collide"),
            identity("src-two", "beta", "collide"),
            identity("demo", "demo", "demo-pipeline"),
        ],
        aliases: vec![],
    };
    for target in [
        ProjectionTargetId::Opencode,
        ProjectionTargetId::Copilot,
        ProjectionTargetId::AgyCli,
        ProjectionTargetId::AgyIde,
        ProjectionTargetId::AgyGui,
    ] {
        let aliases = plan_provider_aliases(target, &inventory).unwrap();
        assert_eq!(aliases.len(), 3);
        let demo = aliases
            .iter()
            .find(|a| a.identity.component_id == "demo-pipeline")
            .unwrap();
        assert_eq!(demo.alias, "demo-pipeline");
        let a1 = aliases
            .iter()
            .find(|a| a.identity.source.as_str() == "src-one")
            .unwrap();
        assert_eq!(a1.alias, "alpha-src-one-collide");
        let a2 = aliases
            .iter()
            .find(|a| a.identity.source.as_str() == "src-two")
            .unwrap();
        assert_eq!(a2.alias, "beta-src-two-collide");
    }
}

#[test]
fn rewrite_generated_provider_references_preserves_unique_and_unrelated() {
    let inventory = ProviderProjectionInventory {
        identities: vec![
            identity("src-one", "alpha", "collide"),
            identity("src-two", "beta", "collide"),
            identity("demo", "demo", "demo-pipeline"),
        ],
        aliases: vec![],
    };
    let aliases = plan_provider_aliases(ProjectionTargetId::Opencode, &inventory).unwrap();
    let text = "Use /demo-pipeline and /help, but not raw commands.";
    let rewritten = rewrite_generated_provider_references(
        text,
        &inventory,
        &aliases,
        ProjectionTargetId::Opencode,
    )
    .unwrap();
    assert_eq!(rewritten, text);
}

#[test]
fn provider_surface_matrix_publishes_one_copy_under_overridden_home() {
    for target in [
        ProjectionTargetId::Opencode,
        ProjectionTargetId::Copilot,
        ProjectionTargetId::AgyCli,
        ProjectionTargetId::AgyIde,
        ProjectionTargetId::AgyGui,
    ] {
        let tmp = TempDir::new().unwrap();
        let package = tmp.path().join("package");
        if target == ProjectionTargetId::Opencode {
            put(&package, "skills/example/SKILL.md", "provider skill");
        } else {
            put(&package, "plugin.json", "{}");
            put(&package, "skills/example/SKILL.md", "provider skill");
            put(&package, "resources/data.txt", "resource");
        }
        let home = tmp.path().join("home-override");
        let lock = tmp.path().join("lock.json");
        let evidence = publish_provider_surfaces(
            target,
            &[ProviderSurfacePackage {
                source_id: "source-a".into(),
                component_id: "example".into(),
                alias: "source-a-example".into(),
                root: package.clone(),
            }],
            &home,
            &lock,
            false,
        );
        let changes = &evidence.by_target[&target].paths;
        let published = changes
            .iter()
            .find(|change| change.path.starts_with(&home))
            .unwrap();
        assert_eq!(
            changes
                .iter()
                .filter(|change| change.path.starts_with(&home))
                .count(),
            1,
            "one provider root should be published"
        );
        assert!(published.path.starts_with(&home));
        let skill_path = if target == ProjectionTargetId::Opencode {
            published.path.join("SKILL.md")
        } else {
            published.path.join("skills/example/SKILL.md")
        };
        assert!(skill_path.is_file());
        if target != ProjectionTargetId::Opencode {
            assert!(published.path.join("resources/data.txt").is_file());
        }
        let again = publish_provider_surfaces(
            target,
            &[ProviderSurfacePackage {
                source_id: "source-a".into(),
                component_id: "example".into(),
                alias: "source-a-example".into(),
                root: package,
            }],
            &home,
            &lock,
            false,
        );
        assert!(again
            .by_target
            .get(&target)
            .is_none_or(|entry| entry.paths.is_empty()));
    }
}

#[test]
fn provider_surface_preserves_unowned_occupant_and_rejects_invalid_shape() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let dest = home.join(".gemini/antigravity-cli/plugins/source-a");
    put(&dest, "owner.txt", "keep");
    let package = tmp.path().join("package");
    put(&package, "plugin.json", "{}");
    let lock = tmp.path().join("lock.json");
    let preserved = publish_provider_surfaces(
        ProjectionTargetId::AgyCli,
        &[ProviderSurfacePackage {
            source_id: "source-a".into(),
            component_id: "example".into(),
            alias: "source-a-example".into(),
            root: package.clone(),
        }],
        &home,
        &lock,
        false,
    );
    assert_eq!(fs::read_to_string(dest.join("owner.txt")).unwrap(), "keep");
    assert!(preserved.by_target[&ProjectionTargetId::AgyCli]
        .skipped
        .is_some());

    let owned_package = tmp.path().join("owned-package");
    put(&owned_package, "plugin.json", "old-root");
    let initial = publish_provider_surfaces(
        ProjectionTargetId::AgyIde,
        &[ProviderSurfacePackage {
            source_id: "source-a".into(),
            component_id: "example".into(),
            alias: "source-a-example".into(),
            root: owned_package,
        }],
        &home,
        &lock,
        false,
    );
    assert!(initial.by_target[&ProjectionTargetId::AgyIde]
        .paths
        .iter()
        .any(|path| path.path.starts_with(&home)));
    let invalid = publish_provider_surfaces(
        ProjectionTargetId::AgyIde,
        &[ProviderSurfacePackage {
            source_id: "source-a".into(),
            component_id: "example".into(),
            alias: "source-a-example".into(),
            root: tmp.path().join("missing"),
        }],
        &home,
        &lock,
        false,
    );
    assert!(invalid.by_target[&ProjectionTargetId::AgyIde]
        .failure
        .is_some());
    assert_eq!(
        fs::read_to_string(home.join(".gemini/antigravity-ide/plugins/source-a/plugin.json"))
            .unwrap(),
        "old-root"
    );
}

/// Records `path` as owned under `category` in the lockfile at `lock`.
fn p7_t10_own(lock: &Path, category: &str, path: &Path) {
    let mut report = ProjectionReport::default();
    let mut registry = ManagedArtifactRegistry::load(lock, &mut report);
    registry.mark(category, path);
    registry.persist(&[], false, &mut report).unwrap();
}

/// A shared skill at `.agents/skills/shared-skill`, recorded as ccync-owned.
fn p7_t10_shared_skill(temp: &TempDir) -> (PathBuf, PathBuf) {
    let lock = temp.path().join("lock.json");
    let shared = temp.path().join(".agents/skills/shared-skill");
    put(&shared, "SKILL.md", "# shared\n");
    p7_t10_own(&lock, MANAGED_DIRECT_SKILL_PATHS, &shared);
    (lock, shared)
}

fn p7_t10_reader(group: &str, state: SharedPathReaderState) -> SharedPathReader {
    SharedPathReader {
        group: group.into(),
        state,
    }
}

/// Proof built from the path's current fingerprint, as ccync would record it.
fn p7_t10_proof(shared: &Path, readers: Vec<SharedPathReader>) -> SharedPathRetirement {
    SharedPathRetirement {
        path: shared.to_path_buf(),
        owned_fingerprint: shared_path_fingerprint(shared).expect("shared path fingerprint"),
        readers,
    }
}

fn historical_ownership_bytes(document: &[u8]) -> &[u8] {
    let marker = b"\"historicalOwnership\"";
    let marker_start = document
        .windows(marker.len())
        .position(|window| window == marker)
        .expect("historicalOwnership section");
    let value_start = document[marker_start + marker.len()..]
        .iter()
        .position(|byte| *byte == b'[')
        .map(|offset| marker_start + marker.len() + offset)
        .expect("historicalOwnership array");

    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, byte) in document[value_start..].iter().copied().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }

        match byte {
            b'"' => in_string = true,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return &document[value_start..=value_start + offset];
                }
            }
            _ => {}
        }
    }

    panic!("historicalOwnership array is not terminated")
}

#[test]
fn historical_projection_ownership_does_not_authorize_overwrite_or_prune() {
    let temp = TempDir::new().unwrap();
    let lock = temp.path().join("lock.json");
    let source = temp.path().join("source/skill");
    let destination = temp.path().join(".agents/skills/skill");
    put(&source, "SKILL.md", "new content");
    put(&destination, "SKILL.md", "user content");
    append_historical_ownership(
        &lock,
        serde_json::json!({"path": destination.to_string_lossy(), "reason": "old record"}),
    )
    .unwrap();

    let publication = DirectSkillPublication {
        assignments: vec![direct_assignment(
            "source-a",
            "skill",
            &source,
            destination.parent().unwrap(),
            ProjectionTargetId::Codex,
        )],
        served_targets: vec![],
    };
    let evidence = publish_direct_skill_plan(&publication, &lock, false);
    assert!(evidence.by_target[&ProjectionTargetId::Codex]
        .failure
        .is_some());
    assert_eq!(
        fs::read_to_string(destination.join("SKILL.md")).unwrap(),
        "user content"
    );

    let retirement = p7_t10_proof(
        &destination,
        vec![p7_t10_reader(
            "codex",
            SharedPathReaderState::ReplacementVerified,
        )],
    );
    assert!(matches!(
        retire_shared_path(&retirement, &lock, false).unwrap(),
        SharedPathRetirementOutcome::Kept { .. }
    ));
    assert!(destination.join("SKILL.md").is_file());
}

#[test]
fn historical_projection_ownership_registry_save_preserves_section_and_current_row_mutates() {
    let temp = TempDir::new().unwrap();
    let lock = temp.path().join("lock.json");
    let historical = serde_json::json!([{"path": "C:/old/path", "note": "  original bytes\n"}]);
    append_historical_ownership(&lock, historical[0].clone()).unwrap();
    let historical_before = read_historical_ownership(&lock).unwrap();
    let historical_bytes_before = historical_ownership_bytes(&fs::read(&lock).unwrap()).to_vec();

    let old_source = temp.path().join("old/skill");
    let new_source = temp.path().join("new/skill");
    let root = temp.path().join(".agents/skills");
    put(&old_source, "SKILL.md", "old content");
    put(&new_source, "SKILL.md", "new content");
    let initial = DirectSkillPublication {
        assignments: vec![direct_assignment(
            "source-a",
            "skill",
            &old_source,
            &root,
            ProjectionTargetId::Codex,
        )],
        served_targets: vec![],
    };
    publish_direct_skill_plan(&initial, &lock, false);
    let replacement = DirectSkillPublication {
        assignments: vec![direct_assignment(
            "source-a",
            "skill",
            &new_source,
            &root,
            ProjectionTargetId::Codex,
        )],
        served_targets: vec![],
    };
    let evidence = publish_direct_skill_plan(&replacement, &lock, false);

    assert!(evidence.by_target[&ProjectionTargetId::Codex]
        .failure
        .is_none());
    assert_eq!(
        fs::read_to_string(root.join("skill/SKILL.md")).unwrap(),
        "new content"
    );
    assert_eq!(read_historical_ownership(&lock).unwrap(), historical_before);
    assert_eq!(
        historical_ownership_bytes(&fs::read(&lock).unwrap()),
        historical_bytes_before
    );
}

#[test]
fn historical_projection_ownership_rejects_malformed_documents_without_writing() {
    let temp = TempDir::new().unwrap();
    let cases = [
        ("corrupt-json", "{not-json"),
        ("non-object-root", "[]"),
        ("non-object-meta", r#"{"_ccyncProjection":[]}"#),
        (
            "non-array-section",
            r#"{"_ccyncProjection":{"historicalOwnership":{"path":"old"}}}"#,
        ),
    ];

    for (name, original) in cases {
        let lock = temp.path().join(format!("{name}.json"));
        fs::write(&lock, original).unwrap();
        let original_bytes = fs::read(&lock).unwrap();

        let result = append_historical_ownership(&lock, serde_json::json!({"path": "new"}));

        assert!(result.is_err(), "{name} should be rejected");
        assert_eq!(
            fs::read(&lock).unwrap(),
            original_bytes,
            "{name} was changed"
        );
    }
}

#[test]
fn p7_t10_unselected_reader_keeps_shared_path() {
    let temp = TempDir::new().unwrap();
    let (lock, shared) = p7_t10_shared_skill(&temp);
    let proof = p7_t10_proof(
        &shared,
        vec![
            p7_t10_reader("codex", SharedPathReaderState::ReplacementVerified),
            p7_t10_reader("opencode", SharedPathReaderState::Unselected),
        ],
    );
    let outcome = retire_shared_path(&proof, &lock, false).unwrap();
    assert!(matches!(outcome, SharedPathRetirementOutcome::Kept { .. }));
    assert!(shared.join("SKILL.md").is_file());
    assert_eq!(direct_records(&lock).len(), 1);
}

#[test]
fn p7_t10_unverified_reader_keeps_shared_path() {
    let temp = TempDir::new().unwrap();
    let (lock, shared) = p7_t10_shared_skill(&temp);
    let proof = p7_t10_proof(
        &shared,
        vec![
            p7_t10_reader("codex", SharedPathReaderState::ReplacementVerified),
            p7_t10_reader("copilot", SharedPathReaderState::Unverified),
        ],
    );
    let outcome = retire_shared_path(&proof, &lock, false).unwrap();
    match outcome {
        SharedPathRetirementOutcome::Kept { reason } => assert!(reason.contains("copilot")),
        other => panic!("expected kept, got {other:?}"),
    }
    assert!(shared.join("SKILL.md").is_file());
    assert_eq!(direct_records(&lock).len(), 1);
}

#[test]
fn p7_t10_empty_or_unknown_proof_authorizes_nothing() {
    let temp = TempDir::new().unwrap();
    let (lock, shared) = p7_t10_shared_skill(&temp);
    let no_readers = p7_t10_proof(&shared, Vec::new());
    assert!(matches!(
        retire_shared_path(&no_readers, &lock, false).unwrap(),
        SharedPathRetirementOutcome::Kept { .. }
    ));
    let blank_group = p7_t10_proof(
        &shared,
        vec![p7_t10_reader(
            " ",
            SharedPathReaderState::ReplacementVerified,
        )],
    );
    assert!(matches!(
        retire_shared_path(&blank_group, &lock, false).unwrap(),
        SharedPathRetirementOutcome::Kept { .. }
    ));
    let no_fingerprint = SharedPathRetirement {
        path: shared.clone(),
        owned_fingerprint: String::new(),
        readers: vec![p7_t10_reader(
            "codex",
            SharedPathReaderState::ReplacementVerified,
        )],
    };
    assert!(matches!(
        retire_shared_path(&no_fingerprint, &lock, false).unwrap(),
        SharedPathRetirementOutcome::Kept { .. }
    ));
    assert!(shared.join("SKILL.md").is_file());
    assert_eq!(direct_records(&lock).len(), 1);
}

#[test]
fn p7_t10_all_readers_preserved_removes_shared_path_and_row() {
    let temp = TempDir::new().unwrap();
    let (lock, shared) = p7_t10_shared_skill(&temp);
    let proof = p7_t10_proof(
        &shared,
        vec![
            p7_t10_reader("codex", SharedPathReaderState::ReplacementVerified),
            p7_t10_reader("copilot", SharedPathReaderState::ReplacementVerified),
        ],
    );
    let outcome = retire_shared_path(&proof, &lock, false).unwrap();
    assert_eq!(outcome, SharedPathRetirementOutcome::Removed);
    assert!(!shared.exists());
    assert!(direct_records(&lock).is_empty());
}

#[test]
fn p7_t10_changed_preimage_keeps_shared_path() {
    let temp = TempDir::new().unwrap();
    let (lock, shared) = p7_t10_shared_skill(&temp);
    let proof = p7_t10_proof(
        &shared,
        vec![p7_t10_reader(
            "codex",
            SharedPathReaderState::ReplacementVerified,
        )],
    );
    fs::write(shared.join("SKILL.md"), "user edit").unwrap();
    let outcome = retire_shared_path(&proof, &lock, false).unwrap();
    match outcome {
        SharedPathRetirementOutcome::Kept { reason } => assert!(reason.contains("changed")),
        other => panic!("expected kept, got {other:?}"),
    }
    assert_eq!(
        fs::read_to_string(shared.join("SKILL.md")).unwrap(),
        "user edit"
    );
    assert_eq!(direct_records(&lock).len(), 1);
}

#[test]
fn p7_t10_unowned_path_is_never_removed() {
    let temp = TempDir::new().unwrap();
    let lock = temp.path().join("lock.json");
    let shared = temp.path().join(".agents/skills/shared-skill");
    put(&shared, "SKILL.md", "# user\n");
    let proof = p7_t10_proof(
        &shared,
        vec![p7_t10_reader(
            "codex",
            SharedPathReaderState::ReplacementVerified,
        )],
    );
    let outcome = retire_shared_path(&proof, &lock, false).unwrap();
    assert!(matches!(outcome, SharedPathRetirementOutcome::Kept { .. }));
    assert_eq!(
        fs::read_to_string(shared.join("SKILL.md")).unwrap(),
        "# user\n"
    );
}

#[test]
fn p7_t10_dry_run_reports_removal_without_writes() {
    let temp = TempDir::new().unwrap();
    let (lock, shared) = p7_t10_shared_skill(&temp);
    let proof = p7_t10_proof(
        &shared,
        vec![p7_t10_reader(
            "codex",
            SharedPathReaderState::ReplacementVerified,
        )],
    );
    let outcome = retire_shared_path(&proof, &lock, true).unwrap();
    assert_eq!(outcome, SharedPathRetirementOutcome::Removed);
    assert!(shared.join("SKILL.md").is_file());
    assert_eq!(direct_records(&lock).len(), 1);
}

#[test]
fn p7_t10_opencode_component_publisher_verifies_replacement_route() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let lock = temp.path().join("lock.json");
    let package_root = temp.path().join("source/plugin");
    put(&package_root, "skills/shared-skill/SKILL.md", "# shared\n");
    let package = ProviderSurfacePackage {
        source_id: "alpha-src".into(),
        component_id: "shared-skill".into(),
        alias: "shared-skill".into(),
        root: package_root,
    };
    let evidence = publish_provider_surfaces(
        ProjectionTargetId::Opencode,
        std::slice::from_ref(&package),
        &home,
        &lock,
        false,
    );
    let replacement = home.join(".config/opencode/skills/shared-skill");
    let opencode_verified = evidence
        .by_target
        .get(&ProjectionTargetId::Opencode)
        .is_none_or(|target| target.failure.is_none())
        && replacement.join("SKILL.md").is_file();
    assert!(opencode_verified);

    let shared = home.join(".agents/skills/shared-skill");
    put(&shared, "SKILL.md", "# shared\n");
    p7_t10_own(&lock, MANAGED_DIRECT_SKILL_PATHS, &shared);
    let codex_unselected = p7_t10_proof(
        &shared,
        vec![
            p7_t10_reader("codex", SharedPathReaderState::Unselected),
            p7_t10_reader("opencode", SharedPathReaderState::ReplacementVerified),
        ],
    );
    assert!(matches!(
        retire_shared_path(&codex_unselected, &lock, false).unwrap(),
        SharedPathRetirementOutcome::Kept { .. }
    ));
    let all_preserved = p7_t10_proof(
        &shared,
        vec![
            p7_t10_reader("codex", SharedPathReaderState::ReplacementVerified),
            p7_t10_reader("opencode", SharedPathReaderState::ReplacementVerified),
        ],
    );
    assert_eq!(
        retire_shared_path(&all_preserved, &lock, false).unwrap(),
        SharedPathRetirementOutcome::Removed
    );
    assert!(!shared.exists());
    assert!(replacement.join("SKILL.md").is_file());
}

#[test]
fn p7_t10_failed_opencode_replacement_keeps_shared_path() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let lock = temp.path().join("lock.json");
    let evidence = publish_provider_surfaces(
        ProjectionTargetId::Opencode,
        &[ProviderSurfacePackage {
            source_id: "alpha-src".into(),
            component_id: "shared-skill".into(),
            alias: "shared-skill".into(),
            root: temp.path().join("missing"),
        }],
        &home,
        &lock,
        false,
    );
    assert!(evidence.by_target[&ProjectionTargetId::Opencode]
        .failure
        .is_some());
    let shared = home.join(".agents/skills/shared-skill");
    put(&shared, "SKILL.md", "# shared\n");
    p7_t10_own(&lock, MANAGED_DIRECT_SKILL_PATHS, &shared);
    let proof = p7_t10_proof(
        &shared,
        vec![p7_t10_reader("opencode", SharedPathReaderState::Unverified)],
    );
    assert!(matches!(
        retire_shared_path(&proof, &lock, false).unwrap(),
        SharedPathRetirementOutcome::Kept { .. }
    ));
    assert!(shared.join("SKILL.md").is_file());
}

/// Two sources that both supply `shared`, so a command token `/shared` is
/// ambiguous, plus the aliases planned for them on OpenCode.
fn ambiguous_shared() -> (ProviderProjectionInventory, Vec<TargetAlias>) {
    let inventory = ProviderProjectionInventory {
        identities: vec![
            identity("src-one", "alpha", "shared"),
            identity("src-two", "beta", "shared"),
        ],
        aliases: vec![],
    };
    let aliases = plan_provider_aliases(ProjectionTargetId::Opencode, &inventory).unwrap();
    (inventory, aliases)
}

fn rewrite(
    body: &str,
    inventory: &ProviderProjectionInventory,
    aliases: &[TargetAlias],
) -> Result<String, ProjectionError> {
    rewrite_generated_provider_references(body, inventory, aliases, ProjectionTargetId::Opencode)
}

#[test]
fn provider_reference_rewrite_copies_path_segments_unchanged() {
    let (inventory, aliases) = ambiguous_shared();
    for body in [
        "Read [notes](../skills/shared/x.md) first.",
        "Run `${CLAUDE_PLUGIN_ROOT}/skills/shared/scripts/shared comp-spec`.",
        "See https://host/shared for details.",
        "Open `/shared/x` in the editor.",
        "Open (/shared/x) in the editor.",
    ] {
        assert_eq!(rewrite(body, &inventory, &aliases).unwrap(), body);
    }
}

#[test]
fn provider_reference_rewrite_still_rejects_ambiguous_command_tokens() {
    let (inventory, aliases) = ambiguous_shared();
    for body in [
        "/shared now",
        "Run (/shared) now",
        "Run `/shared` now",
        "Run \"/shared\" now",
        "Run '/shared' now",
        "Run [/shared] now",
    ] {
        let error = rewrite(body, &inventory, &aliases).unwrap_err();
        assert!(
            error.to_string().contains("ambiguous provider reference"),
            "{body}"
        );
    }
}

#[test]
fn provider_reference_rewrite_renames_command_tokens_but_not_paths() {
    let (planning, aliases) = ambiguous_shared();
    // Only the first source supplies bodies, so its `shared` is unique for
    // resolution while it keeps the alias planned against the second source.
    let provider = planning.identities[0].clone();
    let inventory = ProviderProjectionInventory {
        identities: vec![provider.clone()],
        aliases: vec![],
    };
    let alias = aliases
        .iter()
        .find(|alias| alias.identity == provider)
        .unwrap()
        .alias
        .clone();
    assert_ne!(alias, "shared");
    let rendered = rewrite(
        "Run /shared now, then call scripts/shared directly.",
        &inventory,
        &aliases,
    )
    .unwrap();
    assert_eq!(
        rendered,
        format!("Run /{alias} now, then call scripts/shared directly.")
    );
}
