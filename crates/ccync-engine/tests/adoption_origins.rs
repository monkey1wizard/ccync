use ccync_engine::adopt::{rebuild_adoption_lock_rows, store_adoption_records, AdoptionRecord};
use serde_json::json;

#[test]
fn preview_consent_rejects_changed_source() {
    use ccync_engine::adopt::{
        AdoptableState, AgentKind, ConsentRefusal, InstalledItem, ItemKind, SourceScan, SourceState,
    };
    use ccync_foundation::target::AgentId;
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("alternate.json");
    let catalog = tmp.path().join("plugins.json");
    let lock = tmp.path().join("lock.json");
    std::fs::write(
        &source,
        json!({"mcpServers":{"memory":{"command":"node"}}}).to_string(),
    )
    .unwrap();
    let mut adoptable = AdoptableState::default();
    adoptable.items.push(InstalledItem {
        name: "memory".into(),
        version: None,
        agent: AgentKind::Claude,
        kind: ItemKind::McpServer,
        source: Some(source.to_string_lossy().into_owned()),
        baseline_version: None,
    });
    let scan = SourceScan {
        agent: AgentId::Claude,
        state: SourceState::Ready,
        origins: vec![],
        adoptable,
        reason: None,
        unsupported: vec![],
    };
    let digests = ccync_engine::adopt::preview_mcp_digests(AgentId::Claude, &scan);
    let record = AdoptionRecord {
        source: "claude".into(),
        kind: "mcp-server".into(),
        component_id: "memory".into(),
        name: "memory".into(),
        origin: Some(source.to_string_lossy().into_owned()),
        revision: None,
        digest: digests.get("memory").cloned(),
        enabled: None,
        policy: None,
    };
    store_adoption_records(&catalog, std::slice::from_ref(&record), &[]).unwrap();
    std::fs::write(
        &source,
        json!({"mcpServers":{"memory":{"command":"npx"}}}).to_string(),
    )
    .unwrap();
    assert_eq!(
        ccync_engine::adopt::verify_consented_mcp(&record),
        Err(ConsentRefusal::Changed)
    );
    assert_eq!(
        ccync_engine::adopt::adopt_consented_mcp_definitions(&catalog, &lock, AgentId::Claude)
            .unwrap(),
        vec![("memory".into(), ConsentRefusal::Changed)]
    );
    let stored: serde_json::Value = serde_json::from_slice(&std::fs::read(lock).unwrap()).unwrap();
    assert!(stored["_mcpServers"].get("memory").is_none());
}

#[test]
fn consent_checks_each_source_and_unknown_fields() {
    use ccync_engine::adopt::{
        adopt_consented_mcp_definitions, verify_consented_mcp, ConsentedMcp,
    };
    use ccync_foundation::target::AgentId;
    for (source_name, agent, root) in [
        ("claude", AgentId::Claude, "mcpServers"),
        ("codex", AgentId::Codex, "mcp_servers"),
        ("copilot", AgentId::Copilot, "mcpServers"),
        ("opencode", AgentId::Opencode, "mcp"),
        ("agy", AgentId::Agy, "mcpServers"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("alternate.json");
        let catalog = tmp.path().join("plugins.json");
        let lock = tmp.path().join("lock.json");
        let definition = json!({"command":"node", "unknown":"private"});
        let mut document = serde_json::Map::new();
        document.insert(root.into(), json!({"memory":definition}));
        std::fs::write(&source, serde_json::to_vec(&document).unwrap()).unwrap();
        let record = AdoptionRecord {
            source: source_name.into(),
            kind: "mcp-server".into(),
            component_id: "memory".into(),
            name: "memory".into(),
            origin: Some(source.to_string_lossy().into_owned()),
            revision: None,
            digest: Some(ccync_engine::adopt::mcp_source_fingerprint(&definition).unwrap()),
            enabled: None,
            policy: None,
        };
        store_adoption_records(&catalog, std::slice::from_ref(&record), &[]).unwrap();
        assert!(matches!(
            verify_consented_mcp(&record),
            Ok(ConsentedMcp::SourcePreserved(_))
        ));
        assert!(adopt_consented_mcp_definitions(&catalog, &lock, agent)
            .unwrap()
            .is_empty());
        let text = std::fs::read_to_string(lock).unwrap();
        assert!(!text.contains("private"));
    }
}

#[test]
fn adoption_origins_round_trip_without_credentials_and_rebuild_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("plugins.json");
    let lock = tmp.path().join("lock.json");
    std::fs::write(&catalog, r#"{"unknown":{"kept":true}}"#).unwrap();
    std::fs::write(&lock, r#"{"other":"preserved"}"#).unwrap();
    let skill = AdoptionRecord {
        source: "claude".into(),
        kind: "skill".into(),
        component_id: "graphify".into(),
        name: "graphify".into(),
        origin: Some("C:/home/.claude/skills/graphify".into()),
        revision: None,
        digest: Some("sha256:abcd".into()),
        enabled: Some(true),
        policy: Some(json!({"disabled":false})),
    };
    store_adoption_records(
        &catalog,
        std::slice::from_ref(&skill),
        &["claude".into(), "codex".into()],
    )
    .unwrap();
    let serialized = std::fs::read_to_string(&catalog).unwrap();
    assert!(!serialized.contains("SECRET"));
    assert!(serialized.contains("\"_adoption\""));
    assert!(serialized.contains("targetSnapshot"));
    assert!(serialized.contains("unknown"));
    assert_eq!(
        ccync_engine::adopt::read_adoption_records(&catalog)
            .unwrap()
            .len(),
        1
    );
    store_adoption_records(
        &catalog,
        &[AdoptionRecord {
            source: "codex".into(),
            origin: Some("C:/home/.agents/skills/graphify".into()),
            ..skill
        }],
        &["codex".into()],
    )
    .unwrap();
    rebuild_adoption_lock_rows(&catalog, &lock).unwrap();
    let rows: serde_json::Value = serde_json::from_slice(&std::fs::read(&lock).unwrap()).unwrap();
    assert_eq!(rows["_looseSkills"].as_array().unwrap().len(), 2);
    assert_eq!(rows["other"], "preserved");
}

#[test]
fn adoption_origins_same_name_are_kept_as_distinct_loose_lock_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("plugins.json");
    let lock = tmp.path().join("lock.json");
    let make = |source: &str, origin: &str| AdoptionRecord {
        source: source.into(),
        kind: "skill".into(),
        component_id: "shared".into(),
        name: "shared".into(),
        origin: Some(origin.into()),
        revision: None,
        digest: None,
        enabled: Some(true),
        policy: None,
    };
    store_adoption_records(&catalog, &[make("claude", "C:/claude/shared")], &[]).unwrap();
    store_adoption_records(&catalog, &[make("codex", "C:/agents/shared")], &[]).unwrap();
    rebuild_adoption_lock_rows(&catalog, &lock).unwrap();
    let rows: serde_json::Value = serde_json::from_slice(&std::fs::read(lock).unwrap()).unwrap();
    assert_eq!(rows["_looseSkills"].as_array().unwrap().len(), 2);
}

#[test]
fn adoption_origins_legacy_name_without_origin_blocks_update_for_rescan() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("plugins.json");
    std::fs::write(
        &catalog,
        json!({"_adoption":{"records":[{
            "source":"claude", "kind":"skill", "componentId":"legacy",
            "name":"legacy", "origin":null
        }]}})
        .to_string(),
    )
    .unwrap();
    let update = AdoptionRecord {
        source: "claude".into(),
        kind: "skill".into(),
        component_id: "legacy".into(),
        name: "legacy".into(),
        origin: Some("C:/claude/skills/legacy".into()),
        revision: None,
        digest: None,
        enabled: Some(true),
        policy: None,
    };
    let err = store_adoption_records(&catalog, &[update], &[]).unwrap_err();
    assert!(err.to_string().contains("rescan"));
}

#[test]
fn adoption_origins_materialize_same_name_from_recorded_origin_and_missing_origin_keeps_output() {
    let tmp = tempfile::tempdir().unwrap();
    for (source_name, content) in [("claude", "from claude"), ("codex", "from codex")] {
        let origin = tmp.path().join(source_name).join("shared");
        std::fs::create_dir_all(&origin).unwrap();
        std::fs::write(origin.join("SKILL.md"), content).unwrap();
        let canonical = tmp.path().join(format!("canonical-{source_name}"));
        std::fs::create_dir_all(&canonical).unwrap();
        let skill = ccync_engine::truth::LooseSkill {
            name: "shared".into(),
            source: Some(source_name.into()),
            origin: Some(origin),
        };
        assert!(
            ccync_engine::install::materialize_loose_skills_from(&canonical, &[skill])
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            std::fs::read_to_string(canonical.join("skills/shared/SKILL.md")).unwrap(),
            content
        );
    }

    let canonical = tmp.path().join("canonical-missing");
    let output = canonical.join("skills/shared/SKILL.md");
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    std::fs::write(&output, "existing output").unwrap();
    let skill = ccync_engine::truth::LooseSkill {
        name: "shared".into(),
        source: Some("claude".into()),
        origin: Some(tmp.path().join("missing")),
    };
    let warnings =
        ccync_engine::install::materialize_loose_skills_from(&canonical, &[skill]).unwrap();
    assert!(warnings.iter().any(|warning| warning.contains("rescan")));
    assert_eq!(std::fs::read_to_string(output).unwrap(), "existing output");
}

fn write_skill(dir: &std::path::Path, content: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), content).unwrap();
}

fn loose(name: &str, source: &str, origin: &std::path::Path) -> ccync_engine::truth::LooseSkill {
    ccync_engine::truth::LooseSkill {
        name: name.into(),
        source: Some(source.into()),
        origin: Some(origin.to_path_buf()),
    }
}

#[test]
fn adoption_origins_reversed_same_name_sources_block_the_whole_group() {
    let tmp = tempfile::tempdir().unwrap();
    let claude = tmp.path().join("claude").join("shared");
    let codex = tmp.path().join("codex").join("shared");
    let unrelated = tmp.path().join("claude").join("unrelated");
    write_skill(&claude, "# from claude");
    write_skill(&codex, "# from codex");
    write_skill(&unrelated, "# unrelated");

    let forward = vec![
        loose("shared", "claude", &claude),
        loose("shared", "codex", &codex),
        loose("unrelated", "claude", &unrelated),
    ];
    let mut reversed = forward.clone();
    reversed.reverse();

    let canonical_forward = tmp.path().join("canonical-forward");
    let canonical_reversed = tmp.path().join("canonical-reversed");
    let warnings_forward =
        ccync_engine::install::materialize_loose_skills_from(&canonical_forward, &forward).unwrap();
    let warnings_reversed =
        ccync_engine::install::materialize_loose_skills_from(&canonical_reversed, &reversed)
            .unwrap();

    assert_eq!(warnings_forward, warnings_reversed);
    assert_eq!(warnings_forward.len(), 1, "one report for the whole group");
    let report = &warnings_forward[0];
    assert!(report.contains("conflicting recorded origins"));
    assert!(report.contains(&claude.display().to_string()));
    assert!(report.contains(&codex.display().to_string()));
    assert!(!canonical_forward.join("skills/shared").exists());
    assert!(!canonical_reversed.join("skills/shared").exists());

    assert_eq!(
        std::fs::read_to_string(canonical_forward.join("skills/unrelated/SKILL.md")).unwrap(),
        "# unrelated"
    );
    assert_eq!(
        std::fs::read_dir(canonical_forward.join("skills"))
            .unwrap()
            .count(),
        1,
        "only the unrelated identity has one writer"
    );
}

#[test]
fn adoption_origins_rebuilt_rows_keep_source_and_same_identity_writes_once() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("plugins.json");
    let lock = tmp.path().join("lock.json");
    let shared = tmp.path().join("physical").join("shared");
    write_skill(&shared, "# physical");
    let make = |source: &str| AdoptionRecord {
        source: source.into(),
        kind: "skill".into(),
        component_id: "shared".into(),
        name: "shared".into(),
        origin: Some(shared.to_string_lossy().into_owned()),
        revision: None,
        digest: None,
        enabled: Some(true),
        policy: None,
    };
    store_adoption_records(&catalog, &[make("claude")], &[]).unwrap();
    store_adoption_records(&catalog, &[make("codex")], &[]).unwrap();
    rebuild_adoption_lock_rows(&catalog, &lock).unwrap();

    let rows: serde_json::Value = serde_json::from_slice(&std::fs::read(&lock).unwrap()).unwrap();
    let skills: Vec<ccync_engine::truth::LooseSkill> =
        serde_json::from_value(rows["_looseSkills"].clone()).unwrap();
    let mut sources: Vec<_> = skills.iter().filter_map(|s| s.source.clone()).collect();
    sources.sort();
    assert_eq!(sources, vec!["claude", "codex"]);

    let canonical = tmp.path().join("canonical");
    let warnings =
        ccync_engine::install::materialize_loose_skills_from(&canonical, &skills).unwrap();
    assert!(warnings.is_empty(), "one physical identity: {warnings:?}");
    assert_eq!(
        std::fs::read_to_string(canonical.join("skills/shared/SKILL.md")).unwrap(),
        "# physical"
    );
}

#[test]
fn mcp_policy_credentials_are_projected_only_after_source_fingerprint_matches() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("plugins.json");
    let lock = tmp.path().join("lock.json");
    let source = tmp.path().join("claude.json");
    let definition = json!({"command":"server", "env":{"TOKEN":"SECRET-SENTINEL-TP23"}});
    std::fs::write(
        &source,
        json!({"mcpServers":{"memory":definition}}).to_string(),
    )
    .unwrap();
    let fingerprint = ccync_engine::adopt::mcp_source_fingerprint(&definition).unwrap();
    let record = AdoptionRecord {
        source: "claude".into(),
        kind: "mcp-server".into(),
        component_id: "memory".into(),
        name: "memory".into(),
        origin: Some(source.to_string_lossy().into_owned()),
        revision: None,
        digest: Some(fingerprint),
        enabled: Some(true),
        policy: None,
    };
    store_adoption_records(&catalog, std::slice::from_ref(&record), &["codex".into()]).unwrap();
    rebuild_adoption_lock_rows(&catalog, &lock).unwrap();
    let catalog_text = std::fs::read_to_string(&catalog).unwrap();
    let lock_text = std::fs::read_to_string(&lock).unwrap();
    assert!(!catalog_text.contains("SECRET-SENTINEL-TP23"));
    assert!(!lock_text.contains("SECRET-SENTINEL-TP23"));
    let rows: serde_json::Value = serde_json::from_str(&lock_text).unwrap();
    assert_eq!(rows["_mcpServers"], json!({}));
    assert!(matches!(
        ccync_engine::adopt::verify_consented_mcp(&record),
        Ok(ccync_engine::adopt::ConsentedMcp::SourcePreserved(_))
    ));

    std::fs::write(
        &source,
        json!({"mcpServers":{"memory":{"command":"changed"}}}).to_string(),
    )
    .unwrap();
    rebuild_adoption_lock_rows(&catalog, &lock).unwrap();
    let lock_text = std::fs::read_to_string(lock).unwrap();
    assert!(!lock_text.contains("SECRET-SENTINEL-TP23"));
    let rows: serde_json::Value = serde_json::from_str(&lock_text).unwrap();
    assert_eq!(rows["_mcpServers"], json!({}));
}

#[test]
fn mcp_policy_metadata_never_persists_source_credentials() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("plugins.json");
    let record = AdoptionRecord {
        source: "claude".into(),
        kind: "mcp-server".into(),
        component_id: "memory".into(),
        name: "memory".into(),
        origin: Some("~/.claude.json".into()),
        revision: None,
        digest: None,
        enabled: Some(true),
        policy: Some(
            json!({"enabled": true, "headers": {"Authorization": "SECRET-SENTINEL-TP23"}, "token": "SECRET-SENTINEL-TP23"}),
        ),
    };
    ccync_engine::adopt::store_adoption_records(&catalog, &[record], &["codex".into()]).unwrap();
    let text = std::fs::read_to_string(catalog).unwrap();
    assert!(!text.contains("SECRET-SENTINEL-TP23"));
    assert!(!text.contains("headers"));
    assert!(text.contains("\"enabled\": true"));
}

#[test]
fn mcp_policy_disabled_servers_stay_disabled_for_all_five_sources_and_reimport_is_stable() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("plugins.json");
    let lock = tmp.path().join("lock.json");
    let fixtures = [
        ("claude", "claude.json", "mcpServers", "TOKEN"),
        ("codex", "codex.json", "mcp_servers", "env"),
        ("copilot", "copilot.json", "mcpServers", "TOKEN"),
        ("opencode", "opencode.json", "mcp", "environment"),
        ("agy", "agy.json", "mcpServers", "TOKEN"),
    ];
    let mut records = Vec::new();

    for (source, filename, servers_key, secret_key) in fixtures {
        let path = tmp.path().join(filename);
        let definition = if source == "codex" {
            json!({"command":"server", "env": {secret_key:"SECRET-SENTINEL-TP23"}, "enabled": false})
        } else if source == "opencode" {
            json!({"type":"local", "command":["server"], "environment": {secret_key:"SECRET-SENTINEL-TP23"}, "enabled": false})
        } else {
            json!({"command":"server", "env": {secret_key:"SECRET-SENTINEL-TP23"}, "enabled": false})
        };
        std::fs::write(
            &path,
            json!({servers_key:{"disabled":definition}}).to_string(),
        )
        .unwrap();
        let normalized = if source == "opencode" {
            json!({"command":"server", "enabled":false})
        } else {
            definition.clone()
        };
        records.push(AdoptionRecord {
            source: source.into(),
            kind: "mcp-server".into(),
            component_id: "disabled".into(),
            name: format!("disabled-{source}"),
            origin: Some(path.to_string_lossy().into_owned()),
            revision: None,
            digest: Some(ccync_engine::adopt::mcp_source_fingerprint(&normalized).unwrap()),
            enabled: Some(false),
            policy: Some(json!({"enabled": false})),
        });
    }

    ccync_engine::adopt::store_adoption_records(&catalog, &records, &["claude".into()]).unwrap();
    ccync_engine::adopt::rebuild_adoption_lock_rows(&catalog, &lock).unwrap();
    let first_catalog = std::fs::read(&catalog).unwrap();
    let first_lock = std::fs::read(&lock).unwrap();
    let persisted = String::from_utf8_lossy(&first_catalog);
    assert!(!persisted.contains("SECRET-SENTINEL-TP23"));
    assert!(!String::from_utf8_lossy(&first_lock).contains("SECRET-SENTINEL-TP23"));
    let projected: serde_json::Value = serde_json::from_slice(&first_lock).unwrap();
    assert_eq!(projected["_mcpServers"], json!({}));

    ccync_engine::adopt::store_adoption_records(&catalog, &records, &["claude".into()]).unwrap();
    ccync_engine::adopt::rebuild_adoption_lock_rows(&catalog, &lock).unwrap();
    assert_eq!(std::fs::read(&catalog).unwrap(), first_catalog);
    assert_eq!(std::fs::read(&lock).unwrap(), first_lock);
}

#[test]
fn tp10_spec_reversed_loose_inputs_block_whole_conflict_group_and_materialize_unrelated() {
    let tmp = tempfile::tempdir().unwrap();
    let claude_origin = tmp.path().join("claude").join("conflict-tool");
    let codex_origin = tmp.path().join("codex").join("conflict-tool");
    let unrelated_origin = tmp.path().join("claude").join("unrelated-tool");

    write_skill(&claude_origin, "# from claude");
    write_skill(&codex_origin, "# from codex");
    write_skill(&unrelated_origin, "# unrelated content");

    let forward = vec![
        loose("conflict-tool", "claude", &claude_origin),
        loose("conflict-tool", "codex", &codex_origin),
        loose("unrelated-tool", "claude", &unrelated_origin),
        loose("unrelated-tool", "claude", &unrelated_origin),
    ];
    let mut reversed = forward.clone();
    reversed.reverse();

    let canonical_forward = tmp.path().join("canonical-forward");
    let canonical_reversed = tmp.path().join("canonical-reversed");

    let warnings_forward =
        ccync_engine::install::materialize_loose_skills_from(&canonical_forward, &forward).unwrap();
    let warnings_reversed =
        ccync_engine::install::materialize_loose_skills_from(&canonical_reversed, &reversed)
            .unwrap();

    assert_eq!(warnings_forward, warnings_reversed);
    assert_eq!(warnings_forward.len(), 1);
    let conflict_report = &warnings_forward[0];
    assert!(conflict_report.contains("conflicting recorded origins"));
    assert!(conflict_report.contains(&claude_origin.display().to_string()));
    assert!(conflict_report.contains(&codex_origin.display().to_string()));

    assert!(!canonical_forward.join("skills/conflict-tool").exists());
    assert!(!canonical_reversed.join("skills/conflict-tool").exists());

    assert_eq!(
        std::fs::read_to_string(canonical_forward.join("skills/unrelated-tool/SKILL.md")).unwrap(),
        "# unrelated content"
    );
    assert_eq!(
        std::fs::read_to_string(canonical_reversed.join("skills/unrelated-tool/SKILL.md")).unwrap(),
        "# unrelated content"
    );
    assert_eq!(
        std::fs::read_dir(canonical_forward.join("skills"))
            .unwrap()
            .count(),
        1
    );

    let occupied = canonical_forward.join("skills/occupied-skill");
    std::fs::create_dir_all(&occupied).unwrap();
    std::fs::write(occupied.join("SKILL.md"), "# plugin-owned").unwrap();

    let collision_skill = loose("occupied-skill", "codex", &codex_origin);
    let collision_warnings = ccync_engine::install::materialize_loose_skills_from(
        &canonical_forward,
        &[collision_skill],
    )
    .unwrap();

    assert_eq!(collision_warnings.len(), 1);
    assert!(collision_warnings[0].contains("plugin-owned skill already occupies"));
    assert!(collision_warnings[0].contains("plugin content wins"));
    assert_eq!(
        std::fs::read_to_string(occupied.join("SKILL.md")).unwrap(),
        "# plugin-owned"
    );
}

#[test]
fn tp11_spec_decode_legacy_missing_source_and_malformed_loose_rows() {
    let tmp = tempfile::tempdir().unwrap();

    let malformed_row = json!({"name": 999});
    assert!(serde_json::from_value::<ccync_engine::truth::LooseSkill>(malformed_row).is_err());

    let legacy_row = json!({"name": "legacy-plain"});
    let decoded_legacy: ccync_engine::truth::LooseSkill =
        serde_json::from_value(legacy_row).unwrap();
    assert_eq!(decoded_legacy.name, "legacy-plain");
    assert_eq!(decoded_legacy.source, None);
    assert_eq!(decoded_legacy.origin, None);

    let modern_row = json!({
        "name": "modern-skill",
        "source": "copilot",
        "origin": "C:/tools/modern"
    });
    let decoded_modern: ccync_engine::truth::LooseSkill =
        serde_json::from_value(modern_row).unwrap();
    assert_eq!(decoded_modern.name, "modern-skill");
    assert_eq!(decoded_modern.source.as_deref(), Some("copilot"));
    assert_eq!(
        decoded_modern.origin.as_deref(),
        Some(std::path::Path::new("C:/tools/modern"))
    );

    let valid_origin = tmp.path().join("valid-origin");
    write_skill(&valid_origin, "# verified identity");

    let canonical = tmp.path().join("canonical-tp11");
    let skills = vec![
        ccync_engine::truth::LooseSkill {
            name: "legacy-unresolved".into(),
            source: Some("claude".into()),
            origin: None,
        },
        ccync_engine::truth::LooseSkill {
            name: "missing-filesystem-dir".into(),
            source: Some("codex".into()),
            origin: Some(tmp.path().join("nonexistent-dir")),
        },
        ccync_engine::truth::LooseSkill {
            name: "valid-skill".into(),
            source: Some("copilot".into()),
            origin: Some(valid_origin),
        },
    ];

    let warnings =
        ccync_engine::install::materialize_loose_skills_from(&canonical, &skills).unwrap();

    assert_eq!(warnings.len(), 2);
    assert!(warnings
        .iter()
        .any(|w| w.contains("legacy-unresolved") && w.contains("rescan")));
    assert!(warnings
        .iter()
        .any(|w| w.contains("missing-filesystem-dir") && w.contains("rescan")));

    assert_eq!(
        std::fs::read_to_string(canonical.join("skills/valid-skill/SKILL.md")).unwrap(),
        "# verified identity"
    );
    assert!(!canonical.join("skills/legacy-unresolved").exists());
    assert!(!canonical.join("skills/missing-filesystem-dir").exists());

    let lock = tmp.path().join("lock.json");
    std::fs::write(
        &lock,
        json!({
            "_looseSkills": [
                {"name": "retained-same-name", "source": "claude", "origin": "C:/claude/path"},
                {"name": "retained-same-name", "source": "codex", "origin": "C:/codex/path"},
                {"name": "legacy-no-origin", "source": "claude", "origin": null}
            ],
            "_adoptedItems": [
                {"name": "retained-same-name", "sourceId": "claude:skill:retained-same-name", "source": "C:/claude/path"}
            ]
        })
        .to_string(),
    )
    .unwrap();

    assert!(ccync_engine::adopt::remove_adopted_item("retained-same-name", &lock).unwrap());

    let updated_lock: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&lock).unwrap()).unwrap();
    let remaining = updated_lock["_looseSkills"].as_array().unwrap();
    assert_eq!(remaining.len(), 2);
    assert!(remaining
        .iter()
        .any(|r| r["origin"] == "C:/codex/path" && r["name"] == "retained-same-name"));
    assert!(remaining
        .iter()
        .any(|r| r["name"] == "legacy-no-origin" && r["origin"].is_null()));
}
