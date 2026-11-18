mod support;

use ccync_engine::adopt::{jsonc, read_mcp_definitions, scan_source, AgentKind, SourceState};
use ccync_foundation::target::AgentId;
use std::fs;

#[test]
fn source_scan_claude_distinguishes_states_without_writing() {
    let (home, _lock) = support::isolated();
    let absent = scan_source(AgentId::Claude);
    assert_eq!(absent.state, SourceState::Absent);

    let skills = home.root().join(".claude/skills");
    fs::create_dir_all(&skills).unwrap();
    assert_eq!(scan_source(AgentId::Claude).state, SourceState::Empty);

    fs::write(
        home.root().join(".claude.json"),
        r#"{"mcpServers":{"memory":{"command":"node"}}}"#,
    )
    .unwrap();
    assert_eq!(scan_source(AgentId::Claude).state, SourceState::Ready);

    fs::write(home.root().join(".claude.json"), "{bad json").unwrap();
    assert_eq!(scan_source(AgentId::Claude).state, SourceState::Invalid);

    fs::remove_file(home.root().join(".claude.json")).unwrap();
    fs::create_dir_all(home.root().join(".claude.json")).unwrap();
    assert_eq!(scan_source(AgentId::Claude).state, SourceState::Unreadable);

    assert_eq!(scan_source(AgentId::Copilot).state, SourceState::Absent);
    assert!(!home.root().join(".ccync").exists());
}

#[test]
fn source_scan_codex_classifies_sources_and_validates_home_override() {
    let (home, _lock) = support::isolated();
    assert_eq!(scan_source(AgentId::Codex).state, SourceState::Absent);
    let config = home.root().join(".codex/config.toml");
    fs::write(&config, "").unwrap();
    assert_eq!(scan_source(AgentId::Codex).state, SourceState::Empty);
    fs::write(&config, "[mcp_servers.memory]\ncommand = 'node'\n").unwrap();
    assert_eq!(scan_source(AgentId::Codex).state, SourceState::Ready);
    fs::write(&config, "not = [toml").unwrap();
    assert_eq!(scan_source(AgentId::Codex).state, SourceState::Invalid);

    std::env::set_var("CODEX_HOME", home.root().join(".codex"));
    assert_ne!(scan_source(AgentId::Codex).state, SourceState::Unsupported);
    std::env::set_var("CODEX_HOME", home.root().join("other-codex"));
    assert_eq!(scan_source(AgentId::Codex).state, SourceState::Unsupported);
}

#[test]
fn source_scan_claude_config_override_equal_to_default_is_accepted() {
    let (home, _lock) = support::isolated();
    std::env::set_var("CLAUDE_CONFIG_DIR", home.root().join(".claude"));
    assert_ne!(scan_source(AgentId::Claude).state, SourceState::Unsupported);
    std::env::set_var("CLAUDE_CONFIG_DIR", home.root().join("alternate"));
    assert_eq!(scan_source(AgentId::Claude).state, SourceState::Unsupported);
}

#[test]
fn source_scan_jsonc_accepts_comments_and_trailing_commas_only() {
    assert!(jsonc::parse(
        r#"{ "x": [1,], // ok
      "y": 2, /* ok */ }"#
    )
    .is_ok());
    assert!(jsonc::parse(r#"{ "x": 1 /* ok */ 2 }"#).is_err());
    assert!(jsonc::parse(r#"{ "x": 1,, }"#).is_err());
}

#[test]
fn source_scan_keeps_disabled_mcp_policy_out_of_portable_servers() {
    let (home, _lock) = support::isolated();
    fs::write(
        home.root().join(".claude.json"),
        r#"{"mcpServers":{"active":{"command":"node"},"disabled":{"command":"node"},"restricted":{"command":"node","allowedTools":["read"]}},"disabledMcpServers":["disabled"]}"#,
    )
    .unwrap();
    let definitions = read_mcp_definitions(&AgentKind::Claude);
    assert!(definitions.store.contains_key("active"));
    assert!(!definitions.store.contains_key("disabled"));
    assert_eq!(
        definitions.policy.get("disabled"),
        Some(&serde_json::json!({"enabled": false}))
    );
    assert!(!definitions.store.contains_key("restricted"));
    assert!(definitions.blocked.contains_key("restricted"));
}

#[test]
fn source_scan_blocks_unportable_policy_in_claude_compatibility_input() {
    let (home, _lock) = support::isolated();
    fs::write(
        home.root().join(".claude/.mcp.json"),
        r#"{"mcpServers":{"restricted":{"command":"node","allowedTools":["read"]}}}"#,
    )
    .unwrap();

    let definitions = read_mcp_definitions(&AgentKind::Claude);
    assert!(!definitions.store.contains_key("restricted"));
    assert!(definitions.blocked.contains_key("restricted"));
}

#[test]
fn source_scan_codex_reports_unreadable_and_preserves_zero_writes() {
    let (home, _lock) = support::isolated();
    let config = home.root().join(".codex/config.toml");
    fs::create_dir_all(&config).unwrap();
    assert_eq!(scan_source(AgentId::Codex).state, SourceState::Unreadable);
    assert!(!home.root().join(".ccync").exists());
}

#[test]
fn source_scan_codex_keeps_disabled_and_unportable_mcp_policy() {
    let (home, _lock) = support::isolated();
    let config = home.root().join(".codex/config.toml");
    fs::write(
        &config,
        r#"
[mcp_servers.active]
command = "node"

[mcp_servers.disabled]
command = "node"
enabled = false

[mcp_servers.restricted]
command = "node"
allowed_tools = ["read"]
"#,
    )
    .unwrap();
    let definitions = read_mcp_definitions(&AgentKind::Codex);
    assert!(definitions.store.contains_key("active"));
    assert!(!definitions.store.contains_key("disabled"));
    assert_eq!(
        definitions.policy.get("disabled"),
        Some(&serde_json::json!({"enabled": false}))
    );
    assert!(!definitions.store.contains_key("restricted"));
    assert!(definitions.blocked.contains_key("restricted"));
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
fn source_scan_excludes_ccync_generated_output_and_resolves_links() {
    let (home, _lock) = support::isolated();
    let skills_dir = home.root().join(".claude/skills");
    fs::create_dir_all(&skills_dir).unwrap();

    let normal_skill = skills_dir.join("normal-skill");
    fs::create_dir_all(&normal_skill).unwrap();
    fs::write(normal_skill.join("SKILL.md"), "# normal").unwrap();

    let ccync_skill = home.root().join(".ccync/build/render/generated-skill");
    fs::create_dir_all(&ccync_skill).unwrap();
    fs::write(ccync_skill.join("SKILL.md"), "# generated").unwrap();

    let linked_gen = skills_dir.join("ccync-link");
    let linked = create_dir_link(&ccync_skill, &linked_gen);

    let ext_skill = home.root().join("external/ext-skill");
    fs::create_dir_all(&ext_skill).unwrap();
    fs::write(ext_skill.join("SKILL.md"), "# external").unwrap();

    let linked_ext = skills_dir.join("external-link");
    let ext_linked = create_dir_link(&ext_skill, &linked_ext);

    let state = ccync_engine::adopt::read_claude_adoptable_state();
    let skill_names: Vec<_> = state
        .items
        .into_iter()
        .filter(|i| i.kind == ccync_engine::adopt::ItemKind::Skill)
        .map(|i| i.name)
        .collect();

    assert!(skill_names.contains(&"normal-skill".to_string()));
    if linked {
        assert!(!skill_names.contains(&"ccync-link".to_string()));
    }
    if ext_linked {
        assert!(skill_names.contains(&"external-link".to_string()));
    }
}

/// Write a skill directory with a `SKILL.md` under `root`.
fn write_skill(root: &std::path::Path, name: &str) -> std::path::PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("SKILL.md"), format!("# {name}\n")).unwrap();
    dir
}

/// The recorded source of the adoptable item `name` of `kind`.
fn item_source(
    scan: &ccync_engine::adopt::SourceScan,
    name: &str,
    kind: ccync_engine::adopt::ItemKind,
) -> std::path::PathBuf {
    let item = scan
        .adoptable
        .items
        .iter()
        .find(|item| item.name == name && item.kind == kind)
        .unwrap_or_else(|| panic!("{name} must be adoptable: {:?}", scan.adoptable.items));
    std::path::PathBuf::from(
        item.source
            .as_deref()
            .unwrap_or_else(|| panic!("{name} must record its origin")),
    )
}

fn same_path(a: &std::path::Path, b: &std::path::Path) -> bool {
    fs::canonicalize(a).unwrap() == fs::canonicalize(b).unwrap()
}

#[test]
fn source_scan_every_source_records_skill_directories_and_mcp_config_files_as_origins() {
    use ccync_engine::adopt::ItemKind;
    let (home, _lock) = support::isolated();
    let root = home.root();

    // Claude: skills root, ~/.claude.json, and ~/.claude/.mcp.json.
    let claude_skill = write_skill(&root.join(".claude/skills"), "claude-skill");
    fs::write(
        root.join(".claude.json"),
        r#"{"mcpServers":{"top":{"command":"node"}}}"#,
    )
    .unwrap();
    fs::write(
        root.join(".claude/.mcp.json"),
        r#"{"mcpServers":{"nested":{"command":"node"}}}"#,
    )
    .unwrap();
    let scan = scan_source(AgentId::Claude);
    assert!(same_path(
        &item_source(&scan, "claude-skill", ItemKind::Skill),
        &claude_skill
    ));
    assert!(same_path(
        &item_source(&scan, "top", ItemKind::McpServer),
        &root.join(".claude.json")
    ));
    assert!(same_path(
        &item_source(&scan, "nested", ItemKind::McpServer),
        &root.join(".claude/.mcp.json")
    ));

    // Codex: the shared skills root and config.toml.
    let codex_skill = write_skill(&root.join(".agents/skills"), "codex-skill");
    fs::write(
        root.join(".codex/config.toml"),
        "[mcp_servers.fetch]\ncommand = 'uvx'\n",
    )
    .unwrap();
    let scan = scan_source(AgentId::Codex);
    assert!(same_path(
        &item_source(&scan, "codex-skill", ItemKind::Skill),
        &codex_skill
    ));
    assert!(same_path(
        &item_source(&scan, "fetch", ItemKind::McpServer),
        &root.join(".codex/config.toml")
    ));

    // Copilot: its private skills root and mcp-config.json.
    let copilot_skill = write_skill(&root.join(".copilot/skills"), "copilot-skill");
    fs::write(
        root.join(".copilot/mcp-config.json"),
        r#"{"mcpServers":{"pilot":{"command":"node"}}}"#,
    )
    .unwrap();
    let scan = scan_source(AgentId::Copilot);
    assert!(same_path(
        &item_source(&scan, "copilot-skill", ItemKind::Skill),
        &copilot_skill
    ));
    assert!(same_path(
        &item_source(&scan, "pilot", ItemKind::McpServer),
        &root.join(".copilot/mcp-config.json")
    ));

    // OpenCode: its config-dir skills root and opencode.json.
    let opencode_dir = ccync_foundation::paths::config_dir()
        .unwrap()
        .join("opencode");
    let opencode_skill = write_skill(&opencode_dir.join("skills"), "opencode-skill");
    fs::write(
        opencode_dir.join("opencode.json"),
        r#"{"mcp":{"oc":{"type":"local","command":["node"]}}}"#,
    )
    .unwrap();
    let scan = scan_source(AgentId::Opencode);
    assert!(same_path(
        &item_source(&scan, "opencode-skill", ItemKind::Skill),
        &opencode_skill
    ));
    assert!(same_path(
        &item_source(&scan, "oc", ItemKind::McpServer),
        &opencode_dir.join("opencode.json")
    ));

    // AGY: a face's native skills root.
    let agy_skill = write_skill(&root.join(".gemini/antigravity-cli/skills"), "agy-skill");
    let scan = scan_source(AgentId::Agy);
    assert!(same_path(
        &item_source(&scan, "agy-skill", ItemKind::Skill),
        &agy_skill
    ));
}

#[test]
fn source_scan_excludes_and_reports_skill_directories_ccync_owns() {
    let (home, _lock) = support::isolated();
    let skills = home.root().join(".agents/skills");
    write_skill(&skills, "user-skill");
    write_skill(&skills, "published");
    // The projection ownership record, in the registry's normalized spelling.
    let recorded = projection::normalize_managed_artifact_path(&skills.join("published"));
    let lock = ccync_foundation::paths::plugins_lock_path().unwrap();
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    fs::write(
        &lock,
        serde_json::json!({"_ccyncProjection": {"skillProjectionPaths": [recorded]}}).to_string(),
    )
    .unwrap();

    let scan = scan_source(AgentId::Codex);
    let names: Vec<&str> = scan
        .adoptable
        .items
        .iter()
        .map(|item| item.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["user-skill"],
        "a ccync-owned copy is never adoptable"
    );
    assert_eq!(scan.adoptable.skipped_owned.len(), 1);
    assert!(
        scan.adoptable.skipped_owned[0].starts_with("published ("),
        "{:?}",
        scan.adoptable.skipped_owned
    );
}

#[test]
fn source_scan_outcome_reports_unportable_and_disabled_claude_mcp_without_values() {
    use ccync_engine::adopt::{scan_source_outcome, ItemKind};
    let (home, _lock) = support::isolated();
    let config = home.root().join(".claude.json");
    fs::write(
        &config,
        r#"{"mcpServers":{"active":{"command":"node"},"disabled":{"command":"node"},"restricted":{"command":"node","allowedTools":["read"],"env":{"API_KEY":"key-secret-value"}}},"disabledMcpServers":["disabled"]}"#,
    )
    .unwrap();

    let outcome = scan_source_outcome(AgentId::Claude);
    assert_eq!(outcome.disabled_mcp, vec!["disabled".to_string()]);
    assert_eq!(outcome.blocked_mcp.len(), 1);
    let restricted = &outcome.blocked_mcp[0];
    assert_eq!(restricted.name, "restricted");
    assert!(same_path(restricted.origin.as_deref().unwrap(), &config));
    assert!(restricted.reason.contains("tool policy"));
    assert!(!restricted.disabled);
    assert!(outcome
        .scan
        .adoptable
        .items
        .iter()
        .any(|i| i.name == "active" && i.kind == ItemKind::McpServer));

    assert!(!serde_json::to_string(&outcome.blocked_mcp)
        .unwrap()
        .contains("key-secret-value"));
    assert!(!format!("{outcome:?}").contains("key-secret-value"));
}

#[test]
fn source_scan_outcome_reports_unportable_and_disabled_codex_mcp_without_values() {
    use ccync_engine::adopt::{scan_source_outcome, ItemKind};
    let (home, _lock) = support::isolated();
    let config = home.root().join(".codex/config.toml");
    fs::write(
        &config,
        r#"
[mcp_servers.active]
command = "node"

[mcp_servers.disabled]
command = "node"
enabled = false

[mcp_servers.restricted]
command = "node"
allowed_tools = ["read"]
[mcp_servers.restricted.env]
API_KEY = "codex-secret-key"
"#,
    )
    .unwrap();

    let outcome = scan_source_outcome(AgentId::Codex);
    assert_eq!(outcome.disabled_mcp, vec!["disabled".to_string()]);
    assert_eq!(outcome.blocked_mcp.len(), 1);
    let restricted = &outcome.blocked_mcp[0];
    assert_eq!(restricted.name, "restricted");
    assert!(same_path(restricted.origin.as_deref().unwrap(), &config));
    assert!(restricted.reason.contains("tool policy"));
    assert!(!restricted.disabled);
    assert!(outcome
        .scan
        .adoptable
        .items
        .iter()
        .any(|i| i.name == "active" && i.kind == ItemKind::McpServer));

    assert!(!serde_json::to_string(&outcome.blocked_mcp)
        .unwrap()
        .contains("codex-secret-key"));
    assert!(!format!("{outcome:?}").contains("codex-secret-key"));
}
