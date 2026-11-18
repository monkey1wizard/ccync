mod support;

use ccync_engine::adopt::{scan_source, AgentKind, ItemKind, SourceState};
use ccync_foundation::target::AgentId;
use std::fs;

#[test]
fn source_copilot_imports_private_skills_mcp_and_plugin_origin_once() {
    let (home, _lock) = support::isolated();
    let skill = home.root().join(".copilot/skills/writing");
    fs::create_dir_all(&skill).unwrap();
    fs::write(skill.join("SKILL.md"), "# writing").unwrap();
    let shared = home.root().join(".agents/skills/shared");
    fs::create_dir_all(&shared).unwrap();
    fs::write(shared.join("SKILL.md"), "# shared").unwrap();
    fs::write(
        home.root().join(".copilot/mcp-config.json"),
        r#"{"mcpServers":{"local":{"command":"node"}}}"#,
    )
    .unwrap();
    fs::write(home.root().join(".copilot/installed-plugins.json"), r#"{"plugins":{"publisher.tool":{"version":"1.2","installPath":"C:/plugins/tool","enabled":true},"publisher.disabled":{"version":"2.0","installPath":"C:/plugins/disabled","enabled":false}}}"#).unwrap();

    let scan = scan_source(AgentId::Copilot);
    assert_eq!(scan.state, SourceState::Ready);
    assert!(scan
        .origins
        .iter()
        .any(|o| o.origin.ends_with(".agents/skills")));
    assert_eq!(
        scan.origins
            .iter()
            .filter(|o| o.origin.ends_with(".agents/skills"))
            .count(),
        1,
        "shared root is observed once"
    );
    assert_eq!(
        scan.adoptable
            .items
            .iter()
            .filter(|i| i.kind == ItemKind::Skill)
            .count(),
        1
    );
    assert!(scan
        .adoptable
        .items
        .iter()
        .any(|i| i.name == "local" && i.kind == ItemKind::McpServer));
    let plugin = scan
        .adoptable
        .items
        .iter()
        .find(|i| i.name == "publisher.tool")
        .unwrap();
    assert_eq!(plugin.agent, AgentKind::Copilot);
    assert_eq!(plugin.source.as_deref(), Some("C:/plugins/tool"));
    let disabled = scan
        .adoptable
        .items
        .iter()
        .find(|i| i.name == "publisher.disabled")
        .unwrap();
    assert_eq!(disabled.source.as_deref(), Some("C:/plugins/disabled"));
    assert_eq!(
        scan.adoptable.policy.get("publisher.disabled"),
        Some(&serde_json::json!({"enabled": false}))
    );
    assert!(!home.root().join(".ccync").exists());
}

#[test]
fn source_copilot_distinguishes_empty_invalid_and_rejects_home_override() {
    let (home, _lock) = support::isolated();
    fs::create_dir_all(home.root().join(".copilot/skills")).unwrap();
    assert_eq!(scan_source(AgentId::Copilot).state, SourceState::Empty);
    fs::write(home.root().join(".copilot/mcp-config.json"), "{broken").unwrap();
    assert_eq!(scan_source(AgentId::Copilot).state, SourceState::Invalid);
    std::env::set_var("COPILOT_HOME", home.root().join("other"));
    assert_eq!(
        scan_source(AgentId::Copilot).state,
        SourceState::Unsupported
    );
    std::env::remove_var("COPILOT_HOME");
}

#[test]
fn source_copilot_preserves_disabled_mcp_and_reports_unhandled_fields() {
    let (home, _lock) = support::isolated();
    fs::write(
        home.root().join(".copilot/mcp-config.json"),
        r#"{"mcpServers":{"active":{"command":"node"},"off":{"command":"node"}}}"#,
    )
    .unwrap();
    fs::write(
        home.root().join(".copilot/settings.json"),
        r#"{"disabledMcpServers":["off"],"customPolicy":true}"#,
    )
    .unwrap();
    let scan = scan_source(AgentId::Copilot);
    assert!(scan.adoptable.items.iter().any(|i| i.name == "active"));
    assert!(!scan.adoptable.items.iter().any(|i| i.name == "off"));
    assert_eq!(
        scan.adoptable.policy.get("off"),
        Some(&serde_json::json!({"enabled": false}))
    );
    assert!(scan.unsupported.iter().any(|field| field == "customPolicy"));
}

#[test]
fn source_copilot_accepts_default_home_override_and_classifies_unreadable() {
    let (home, _lock) = support::isolated();
    std::env::set_var("COPILOT_HOME", home.root().join(".copilot"));
    assert_ne!(
        scan_source(AgentId::Copilot).state,
        SourceState::Unsupported
    );
    std::env::remove_var("COPILOT_HOME");

    fs::create_dir_all(home.root().join(".copilot/mcp-config.json")).unwrap();
    assert_eq!(scan_source(AgentId::Copilot).state, SourceState::Unreadable);
}

#[test]
fn source_copilot_mcp_definitions_import_portable_servers_and_block_tool_policy() {
    use ccync_engine::adopt::read_mcp_definitions;
    let (home, _lock) = support::isolated();
    fs::write(
        home.root().join(".copilot/mcp-config.json"),
        r#"{"mcpServers":{
            "local":{"type":"local","command":"node","args":["server.js"],"tools":["*"]},
            "remote":{"type":"http","url":"https://example.test/mcp","tools":["*"]},
            "restricted":{"type":"local","command":"node","tools":["read_only"]},
            "off":{"type":"local","command":"node","tools":["*"]}
        }}"#,
    )
    .unwrap();
    fs::write(
        home.root().join(".copilot/settings.json"),
        r#"{"disabledMcpServers":["off"]}"#,
    )
    .unwrap();

    let definitions = read_mcp_definitions(&AgentKind::Copilot);
    let mut names: Vec<_> = definitions.store.keys().cloned().collect();
    names.sort();
    assert_eq!(names, vec!["local".to_string(), "remote".to_string()]);
    let local = &definitions.store["local"];
    assert_eq!(local.command.as_deref(), Some("node"));
    assert_eq!(local.server_type.as_deref(), Some("stdio"));
    assert_eq!(
        definitions.store["remote"].url.as_deref(),
        Some("https://example.test/mcp")
    );
    assert!(definitions.blocked.contains_key("restricted"));
    assert_eq!(
        definitions.policy.get("off"),
        Some(&serde_json::json!({"enabled": false}))
    );
    assert!(!home.root().join(".ccync").exists());
}
