mod support;

use ccync_engine::adopt::{read_mcp_definitions, scan_source, AgentKind, ItemKind, SourceState};
use ccync_foundation::paths::config_dir;
use ccync_foundation::target::AgentId;
use std::fs;

struct IsolatedOpenCodeOverrides(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl IsolatedOpenCodeOverrides {
    fn new() -> Self {
        let keys = [
            "OPENCODE_CONFIG_DIR",
            "OPENCODE_CONFIG",
            "OPENCODE_CONFIG_CONTENT",
        ];
        let prior = keys
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        for key in keys {
            std::env::remove_var(key);
        }
        Self(prior)
    }
}

impl Drop for IsolatedOpenCodeOverrides {
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
fn source_opencode_imports_jsonc_skills_and_mcp_without_writing_sources() {
    let (home, _lock) = support::isolated();
    let _overrides = IsolatedOpenCodeOverrides::new();
    let opencode_root = config_dir().unwrap().join("opencode");
    fs::create_dir_all(&opencode_root).unwrap();
    let specific = opencode_root.join("skills/local");
    fs::create_dir_all(&specific).unwrap();
    fs::write(specific.join("SKILL.md"), "# local").unwrap();
    let claude = home.root().join(".claude/skills/shared");
    let agents = home.root().join(".agents/skills/shared");
    for skill in [&claude, &agents] {
        fs::create_dir_all(skill).unwrap();
        fs::write(skill.join("SKILL.md"), "# shared").unwrap();
    }
    let config = opencode_root.join("opencode.jsonc");
    let original = r#"{
      // OpenCode command form
      "mcp": {
        "active": { "type": "local", "command": ["node", "server.js", "--fast"], "environment": { "TOKEN": "secret" }, },
        "disabled": { "type": "local", "command": ["node", "off.js"], "enabled": false },
      },
      "experimentalFeature": true,
    }"#;
    fs::write(&config, original).unwrap();

    let scan = scan_source(AgentId::Opencode);
    assert_eq!(scan.state, SourceState::Ready);
    assert!(scan
        .origins
        .iter()
        .any(|origin| origin.origin.ends_with("opencode.jsonc")));
    for suffix in ["opencode/skills", ".claude/skills", ".agents/skills"] {
        assert_eq!(
            scan.origins
                .iter()
                .filter(|origin| origin.origin.ends_with(suffix))
                .count(),
            1,
            "{suffix}"
        );
    }
    assert_eq!(
        scan.adoptable
            .items
            .iter()
            .filter(|item| item.kind == ItemKind::Skill)
            .count(),
        3
    );
    assert!(!scan
        .adoptable
        .items
        .iter()
        .any(|item| item.name == "disabled"));
    assert_eq!(
        scan.adoptable.policy.get("disabled"),
        Some(&serde_json::json!({"enabled": false}))
    );
    assert!(scan
        .unsupported
        .iter()
        .any(|field| field == "experimentalFeature"));
    let definitions = read_mcp_definitions(&AgentKind::OpenCode);
    let active = definitions.store.get("active").unwrap();
    assert_eq!(active.command.as_deref(), Some("node"));
    assert_eq!(active.args.as_deref().unwrap(), &["server.js", "--fast"]);
    assert_eq!(
        active
            .env
            .as_ref()
            .unwrap()
            .get("TOKEN")
            .map(String::as_str),
        Some("secret")
    );
    assert_eq!(
        definitions.policy.get("disabled"),
        Some(&serde_json::json!({"enabled": false}))
    );
    assert_eq!(fs::read_to_string(config).unwrap(), original);
    assert!(!home.root().join(".ccync").exists());
}

#[test]
fn source_opencode_reports_divergent_config_files_as_conflict() {
    let (_home, _lock) = support::isolated();
    let _overrides = IsolatedOpenCodeOverrides::new();
    let root = config_dir().unwrap().join("opencode");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("opencode.json"),
        r#"{"mcp":{"first":{"command":["node"]}}}"#,
    )
    .unwrap();
    fs::write(
        root.join("opencode.jsonc"),
        r#"{"mcp":{"second":{"command":["node"]}}}"#,
    )
    .unwrap();
    let scan = scan_source(AgentId::Opencode);
    assert_eq!(scan.state, SourceState::Invalid, "{:?}", scan.reason);
    assert!(scan.reason.as_deref().unwrap().contains("conflicting"));
    assert!(
        scan.adoptable.items.is_empty(),
        "conflicting configs are never selected"
    );
}

#[test]
fn source_opencode_rejects_nondefault_environment_overrides() {
    let (home, _lock) = support::isolated();
    let _overrides = IsolatedOpenCodeOverrides::new();
    for (key, value) in [
        ("OPENCODE_CONFIG_DIR", "alternate"),
        ("OPENCODE_CONFIG", "alternate.json"),
        ("OPENCODE_CONFIG_CONTENT", "{\"mcp\":{}}"),
    ] {
        std::env::set_var(key, home.root().join(value));
        let scan = scan_source(AgentId::Opencode);
        assert_eq!(scan.state, SourceState::Unsupported, "{key}");
        assert!(scan.reason.as_deref().unwrap().contains(key));
        std::env::remove_var(key);
    }
    std::env::set_var(
        "OPENCODE_CONFIG_DIR",
        config_dir().unwrap().join("opencode"),
    );
    assert_ne!(
        scan_source(AgentId::Opencode).state,
        SourceState::Unsupported
    );
}

#[test]
fn source_opencode_distinguishes_absent_empty_invalid_and_empty_overrides() {
    let (_home, _lock) = support::isolated();
    let _overrides = IsolatedOpenCodeOverrides::new();
    assert_eq!(scan_source(AgentId::Opencode).state, SourceState::Absent);

    let opencode_root = config_dir().unwrap().join("opencode");
    fs::create_dir_all(opencode_root.join("skills")).unwrap();
    assert_eq!(scan_source(AgentId::Opencode).state, SourceState::Empty);

    let config = opencode_root.join("opencode.json");
    fs::write(&config, "{broken json").unwrap();
    assert_eq!(scan_source(AgentId::Opencode).state, SourceState::Invalid);

    std::env::set_var("OPENCODE_CONFIG_CONTENT", "");
    assert_ne!(
        scan_source(AgentId::Opencode).state,
        SourceState::Unsupported
    );
    std::env::remove_var("OPENCODE_CONFIG_CONTENT");

    std::env::set_var("OPENCODE_CONFIG", &config);
    assert_ne!(
        scan_source(AgentId::Opencode).state,
        SourceState::Unsupported
    );
    std::env::remove_var("OPENCODE_CONFIG");
}

