use ccync_engine::plugin_render::{
    render_provider_generations_at, ProviderEntry, ProviderInventory, ProviderRecordKind,
};
use ccync_foundation::provider::{ComponentKind, ProviderSourceKey};
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn entry(id: &str, source: &str, payload_dir: PathBuf) -> ProviderEntry {
    ProviderEntry {
        key: ProviderSourceKey::new(id.to_lowercase()).unwrap(),
        record: ProviderRecordKind::Bundled,
        plugin_id: id.to_string(),
        source: source.to_string(),
        plugin_name: id.to_string(),
        upstream_revision: Some("rev-1".to_string()),
        payload_dir,
    }
}

#[test]
fn component_inventory_preserves_identity_resources_enabled_hooks_and_mcp_files() {
    let tmp = tempdir().unwrap();
    let payload = tmp.path().join("provider");
    write(
        &payload.join("skills/search/SKILL.md"),
        "---\nname: search\nenabled: false\n---\nBody\n",
    );
    write(
        &payload.join("skills/search/scripts/run.py"),
        "print('ok')\n",
    );
    write(
        &payload.join("hooks/hooks.json"),
        r#"{"hooks":{"SessionStart":[]}}"#,
    );
    write(
        &payload.join(".mcp.json"),
        r#"{"mcpServers":{"one":{"command":"x"}}}"#,
    );
    let inventory = ProviderInventory {
        providers: vec![entry("provider", "git+https://example/provider", payload)],
        unavailable: vec![],
    };

    let rendered = render_provider_generations_at(&inventory, &tmp.path().join("render"))
        .expect("render inventory");
    let provider = &rendered[0];
    let skill = provider
        .components
        .iter()
        .find(|component| component.kind == ComponentKind::Skill)
        .unwrap();
    assert_eq!(skill.source.as_str(), "provider");
    assert_eq!(skill.plugin_name, "provider");
    assert_eq!(skill.component_id, "search");
    let origin = skill.origin.as_ref().unwrap();
    assert_eq!(
        origin.canonical_reference.as_deref(),
        Some("git+https://example/provider")
    );
    assert_eq!(origin.revision.as_deref(), Some("rev-1"));
    assert_eq!(origin.enabled, Some(false));
    let resources = provider
        .component_resources
        .iter()
        .find(|item| item.component.component_id == "search")
        .unwrap();
    assert!(!resources.enabled);
    assert!(resources
        .resources
        .iter()
        .any(|path| path.as_str() == "skills/search/scripts/run.py"));
    assert!(provider.hooks_declared);
    assert!(provider.path.join(".mcp.json").is_file());
}

#[test]
fn component_inventory_conflicting_claims_fail_before_publication() {
    let tmp = tempdir().unwrap();
    let first = tmp.path().join("first");
    write(
        &first.join("skills/shared/SKILL.md"),
        "---\nname: shared\n---\n",
    );
    let providers_root = tmp.path().join("render");
    let duplicate_source = entry("first", "first", first);
    let inventory = ProviderInventory {
        providers: vec![duplicate_source.clone(), duplicate_source],
        unavailable: vec![],
    };

    assert!(render_provider_generations_at(&inventory, &providers_root).is_err());
    assert!(!providers_root.exists());
}

#[test]
fn component_inventory_handles_adopted_sources_and_no_hooks() {
    let tmp = tempdir().unwrap();
    let payload = tmp.path().join("adopted_provider");
    write(
        &payload.join("skills/helper/SKILL.md"),
        "---\nname: helper\n---\nBody\n",
    );
    write(
        &payload.join("commands/run.md"),
        "---\ndescription: run command\n---\nCommand body\n",
    );
    write(
        &payload.join("agents/reviewer.md"),
        "---\ndescription: agent\n---\nAgent body\n",
    );
    let mut provider_entry = entry("adopted_prov", "claude:~/.claude/plugins/my-plugin", payload);
    provider_entry.record = ProviderRecordKind::Personal;

    let inventory = ProviderInventory {
        providers: vec![provider_entry],
        unavailable: vec![],
    };

    let rendered = render_provider_generations_at(&inventory, &tmp.path().join("render"))
        .expect("render inventory for adopted source");
    assert_eq!(rendered.len(), 1);
    let provider = &rendered[0];

    // Hooks declared should be false since no hooks were declared
    assert!(!provider.hooks_declared);

    // Verify all components (skill, command, agent) are inventoried
    let skill = provider
        .components
        .iter()
        .find(|c| c.kind == ComponentKind::Skill)
        .expect("skill component found");
    assert_eq!(skill.component_id, "helper");

    let command = provider
        .components
        .iter()
        .find(|c| c.kind == ComponentKind::Command)
        .expect("command component found");
    assert_eq!(command.component_id, "run");

    let agent = provider
        .components
        .iter()
        .find(|c| c.kind == ComponentKind::Agent)
        .expect("agent component found");
    assert_eq!(agent.component_id, "reviewer");
}

#[test]
fn component_inventory_distinct_providers_with_same_component_name_stay_separate_generations() {
    let tmp = tempdir().unwrap();
    let alpha_payload = tmp.path().join("alpha_payload");
    write(
        &alpha_payload.join("skills/shared/SKILL.md"),
        "---\nname: shared\n---\nAlpha\n",
    );
    let beta_payload = tmp.path().join("beta_payload");
    write(
        &beta_payload.join("skills/shared/SKILL.md"),
        "---\nname: shared\n---\nBeta\n",
    );

    let inventory = ProviderInventory {
        providers: vec![
            entry("alpha", "git+https://example/alpha", alpha_payload),
            entry("beta", "git+https://example/beta", beta_payload),
        ],
        unavailable: vec![],
    };

    let render_root = tmp.path().join("render");
    let rendered = render_provider_generations_at(&inventory, &render_root)
        .expect("distinct providers with same component name should produce separate generations");

    assert_eq!(rendered.len(), 2);
    assert_eq!(rendered[0].generation.source.as_str(), "alpha");
    assert_eq!(rendered[1].generation.source.as_str(), "beta");
    assert_ne!(rendered[0].path, rendered[1].path);
    assert!(rendered[0].components.iter().any(|c| c.component_id == "shared"));
    assert!(rendered[1].components.iter().any(|c| c.component_id == "shared"));
}


