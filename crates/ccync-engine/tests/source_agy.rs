mod support;

use ccync_engine::adopt::{scan_source, AgentKind, ItemKind, SourceState};
use ccync_foundation::target::AgentId;
use std::fs;

fn face_roots(home: &support::IsolatedHome) -> [std::path::PathBuf; 3] {
    let gemini = home.root().join(".gemini");
    [
        gemini.join("antigravity-cli/skills"),
        gemini.join("antigravity-ide/skills"),
        gemini.join("skills"),
    ]
}

fn add_skill(root: &std::path::Path, name: &str, content: &str) {
    let skill = root.join(name);
    fs::create_dir_all(&skill).unwrap();
    fs::write(skill.join("SKILL.md"), content).unwrap();
}

#[test]
fn source_agy_keeps_each_face_observation_and_imports_one_available_face() {
    let (home, _lock) = support::isolated();
    let roots = face_roots(&home);
    add_skill(&roots[2], "gui-skill", "# shared");

    let scan = scan_source(AgentId::Agy);
    assert_eq!(scan.state, SourceState::Ready);
    assert_eq!(scan.origins.len(), 3);
    assert_eq!(scan.origins[0].origin, roots[0]);
    assert_eq!(scan.origins[0].state, SourceState::Absent);
    assert_eq!(scan.origins[1].origin, roots[1]);
    assert_eq!(scan.origins[1].state, SourceState::Absent);
    assert_eq!(scan.origins[2].origin, fs::canonicalize(&roots[2]).unwrap());
    assert_eq!(scan.origins[2].state, SourceState::Ready);
    let item = scan.adoptable.items.first().unwrap();
    assert_eq!(item.name, "gui-skill");
    assert_eq!(item.agent, AgentKind::Agy);
    assert_eq!(item.kind, ItemKind::Skill);
    assert_eq!(
        item.source.as_deref(),
        Some(
            fs::canonicalize(roots[2].join("gui-skill"))
                .unwrap()
                .to_string_lossy()
                .as_ref()
        )
    );
}

#[test]
fn source_agy_merges_identical_content_across_faces() {
    let (home, _lock) = support::isolated();
    let roots = face_roots(&home);
    for root in &roots {
        add_skill(root, "same", "# same content");
    }

    let scan = scan_source(AgentId::Agy);
    assert_eq!(scan.state, SourceState::Ready);
    assert_eq!(scan.origins.len(), 3);
    assert_eq!(
        scan.adoptable
            .items
            .iter()
            .filter(|i| i.name == "same")
            .count(),
        1
    );
}

#[test]
fn source_agy_blocks_divergent_content_under_one_identity() {
    let (home, _lock) = support::isolated();
    let roots = face_roots(&home);
    add_skill(&roots[0], "same", "# CLI version");
    add_skill(&roots[1], "same", "# IDE version");

    let scan = scan_source(AgentId::Agy);
    assert_eq!(scan.state, SourceState::Invalid);
    assert!(scan.reason.as_deref().unwrap().contains("same"));
    assert!(scan.adoptable.items.is_empty());
}

#[test]
fn source_agy_reports_all_faces_absent_only_when_none_has_data() {
    let (home, _lock) = support::isolated();
    let scan = scan_source(AgentId::Agy);
    assert_eq!(scan.state, SourceState::Absent);
    assert_eq!(scan.origins.len(), 3);
    assert!(scan.origins.iter().all(|o| o.state == SourceState::Absent));

    add_skill(&face_roots(&home)[0], "cli", "# cli");
    assert_eq!(scan_source(AgentId::Agy).state, SourceState::Ready);
}

fn create_dir_link(target: &std::path::Path, link: &std::path::Path) -> bool {
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .args([
                "/c",
                "mklink",
                "/J",
                link.to_str().unwrap(),
                target.to_str().unwrap(),
            ])
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
}

#[test]
fn source_agy_imports_ide_without_cli_preference() {
    let (home, _lock) = support::isolated();
    let roots = face_roots(&home);
    add_skill(&roots[1], "ide-only-skill", "# IDE only");

    let scan = scan_source(AgentId::Agy);
    assert_eq!(scan.state, SourceState::Ready);
    assert_eq!(scan.origins.len(), 3);
    assert_eq!(scan.origins[0].state, SourceState::Absent);
    assert_eq!(scan.origins[1].state, SourceState::Ready);
    assert_eq!(scan.origins[2].state, SourceState::Absent);

    assert_eq!(scan.adoptable.items.len(), 1);
    let item = &scan.adoptable.items[0];
    assert_eq!(item.name, "ide-only-skill");
    assert_eq!(item.agent, AgentKind::Agy);
    assert_eq!(item.kind, ItemKind::Skill);
}

#[test]
fn source_agy_imports_distinct_skills_across_all_three_faces() {
    let (home, _lock) = support::isolated();
    let roots = face_roots(&home);
    add_skill(&roots[0], "cli-skill", "# cli");
    add_skill(&roots[1], "ide-skill", "# ide");
    add_skill(&roots[2], "gui-skill", "# gui");

    let scan = scan_source(AgentId::Agy);
    assert_eq!(scan.state, SourceState::Ready);
    assert_eq!(scan.origins.len(), 3);
    assert!(scan.origins.iter().all(|o| o.state == SourceState::Ready));

    assert_eq!(scan.adoptable.items.len(), 3);
    let names: std::collections::HashSet<_> = scan
        .adoptable
        .items
        .iter()
        .map(|i| i.name.as_str())
        .collect();
    assert!(names.contains("cli-skill"));
    assert!(names.contains("ide-skill"));
    assert!(names.contains("gui-skill"));
}

#[test]
fn source_agy_merges_identical_physical_roots() {
    let (home, _lock) = support::isolated();
    let roots = face_roots(&home);
    fs::create_dir_all(&roots[0]).unwrap();
    add_skill(&roots[0], "shared-physical", "# physical content");

    if create_dir_link(&roots[0], &roots[1]) {
        let scan = scan_source(AgentId::Agy);
        assert_eq!(scan.state, SourceState::Ready);
        assert_eq!(scan.origins.len(), 3);
        assert_eq!(scan.origins[0].state, SourceState::Ready);
        assert_eq!(scan.origins[1].state, SourceState::Ready);
        assert_eq!(scan.origins[2].state, SourceState::Absent);

        assert_eq!(
            scan.adoptable
                .items
                .iter()
                .filter(|i| i.name == "shared-physical")
                .count(),
            1,
            "identical physical source must be merged into one adoptable item"
        );
    }
}

#[test]
fn source_agy_does_not_infer_native_plugin_reader_from_plugin_root() {
    let (home, _lock) = support::isolated();
    let roots = face_roots(&home);
    add_skill(&roots[0], "agy-skill", "# skill content");

    let scan = scan_source(AgentId::Agy);
    assert_eq!(scan.state, SourceState::Ready);
    assert!(
        scan.adoptable
            .items
            .iter()
            .all(|i| i.kind != ItemKind::Plugin),
        "AGY scan should not infer native plugins from skills"
    );
}

fn write_mcp(path: &std::path::Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

#[test]
fn source_agy_mcp_definitions_import_shared_config_and_block_tool_policy() {
    use ccync_engine::adopt::read_mcp_definitions;
    let (home, _lock) = support::isolated();
    let gemini = home.root().join(".gemini");
    write_mcp(
        &gemini.join("config/mcp_config.json"),
        r#"{"mcpServers":{
            "fetch":{"command":"uvx","args":["mcp-server-fetch"]},
            "remote":{"serverUrl":"https://example.test/mcp"},
            "restricted":{"command":"node","disabledTools":["write"]},
            "off":{"command":"node","disabled":true}
        }}"#,
    );
    // The same definition seen by a second face imports once.
    write_mcp(
        &gemini.join("antigravity-cli/mcp_config.json"),
        r#"{"mcpServers":{"fetch":{"command":"uvx","args":["mcp-server-fetch"]}}}"#,
    );

    let definitions = read_mcp_definitions(&AgentKind::Agy);
    let mut names: Vec<_> = definitions.store.keys().cloned().collect();
    names.sort();
    assert_eq!(names, vec!["fetch".to_string(), "remote".to_string()]);
    assert_eq!(
        definitions.store["remote"].url.as_deref(),
        Some("https://example.test/mcp")
    );
    assert!(definitions.blocked.contains_key("restricted"));
    assert_eq!(
        definitions.policy.get("off"),
        Some(&serde_json::json!({"enabled": false}))
    );

    let scan = scan_source(AgentId::Agy);
    assert_eq!(scan.state, SourceState::Ready);
    assert_eq!(
        scan.adoptable
            .items
            .iter()
            .filter(|i| i.kind == ItemKind::McpServer)
            .map(|i| i.name.as_str())
            .collect::<Vec<_>>(),
        vec!["fetch", "remote"]
    );
    assert!(!home.root().join(".ccync").exists());
}

#[test]
fn source_agy_mcp_definitions_divergent_same_name_across_faces_imports_nothing() {
    use ccync_engine::adopt::read_mcp_definitions;
    let (home, _lock) = support::isolated();
    let gemini = home.root().join(".gemini");
    write_mcp(
        &gemini.join("config/mcp_config.json"),
        r#"{"mcpServers":{"shared":{"command":"node","args":["a.js"]},"other":{"command":"uvx"}}}"#,
    );
    write_mcp(
        &gemini.join("antigravity-ide/mcp_config.json"),
        r#"{"mcpServers":{"shared":{"command":"node","args":["b.js"]}}}"#,
    );

    let definitions = read_mcp_definitions(&AgentKind::Agy);
    assert!(!definitions.store.contains_key("shared"));
    assert!(definitions.store.contains_key("other"));
    assert!(definitions.blocked.contains_key("shared"));
    assert!(!scan_source(AgentId::Agy)
        .adoptable
        .items
        .iter()
        .any(|i| i.name == "shared"));
}

#[test]
fn source_agy_mcp_diagnostics_keep_blocked_and_disabled_servers_separate() {
    use ccync_engine::adopt::scan_source_outcome;
    let (home, _lock) = support::isolated();
    let gemini = home.root().join(".gemini");
    let config = gemini.join("config/mcp_config.json");
    write_mcp(
        &config,
        r#"{"mcpServers":{
            "fetch":{"command":"uvx","args":["mcp-server-fetch"]},
            "restricted":{"command":"node","env":{"API_TOKEN":"tok-secret-value"},"disabledTools":["write"]},
            "off":{"command":"node","disabled":true}
        }}"#,
    );
    let cli_config = gemini.join("antigravity-cli/mcp_config.json");
    write_mcp(
        &cli_config,
        r#"{"mcpServers":{"shared":{"command":"node","args":["a.js"]}}}"#,
    );
    write_mcp(
        &gemini.join("antigravity-ide/mcp_config.json"),
        r#"{"mcpServers":{"shared":{"command":"node","args":["b.js"]}}}"#,
    );

    let outcome = scan_source_outcome(AgentId::Agy);
    let blocked: Vec<_> = outcome
        .blocked_mcp
        .iter()
        .map(|b| (b.name.as_str(), b.disabled))
        .collect();
    assert_eq!(blocked, vec![("restricted", false), ("shared", false)]);
    let restricted = &outcome.blocked_mcp[0];
    assert_eq!(restricted.origin, Some(fs::canonicalize(&config).unwrap()));
    assert!(restricted.reason.contains("tool policy"));
    let shared = &outcome.blocked_mcp[1];
    assert_eq!(shared.origin, Some(fs::canonicalize(&cli_config).unwrap()));
    assert!(shared.reason.contains("divergent"));

    assert_eq!(outcome.disabled_mcp, vec!["off".to_string()]);
    let imported: Vec<_> = outcome
        .scan
        .adoptable
        .items
        .iter()
        .filter(|i| i.kind == ItemKind::McpServer)
        .map(|i| i.name.as_str())
        .collect();
    assert_eq!(imported, vec!["fetch"]);

    assert!(!serde_json::to_string(&outcome.blocked_mcp)
        .unwrap()
        .contains("tok-secret-value"));
    assert!(!format!("{outcome:?}").contains("tok-secret-value"));
}

#[test]
fn source_agy_skill_conflict_invalidates_source_without_folding_mcp_blocks() {
    use ccync_engine::adopt::scan_source_outcome;
    let (home, _lock) = support::isolated();
    let roots = face_roots(&home);
    add_skill(&roots[0], "same", "# CLI version");
    add_skill(&roots[1], "same", "# IDE version");
    write_mcp(
        &home.root().join(".gemini/config/mcp_config.json"),
        r#"{"mcpServers":{"restricted":{"command":"node","disabledTools":["write"]},"fetch":{"command":"uvx"}}}"#,
    );

    let outcome = scan_source_outcome(AgentId::Agy);
    assert_eq!(outcome.scan.state, SourceState::Invalid);
    assert_eq!(
        outcome.scan.reason.as_deref(),
        Some("conflicting AGY skill contents: same")
    );
    assert_eq!(outcome.scan.unsupported, vec!["same".to_string()]);
    assert!(outcome.scan.adoptable.items.is_empty());
    let blocked: Vec<_> = outcome
        .blocked_mcp
        .iter()
        .map(|b| b.name.as_str())
        .collect();
    assert_eq!(blocked, vec!["restricted"]);
}
