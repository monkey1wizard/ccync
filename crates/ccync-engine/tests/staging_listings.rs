use ccync_engine::delivery_apply::UnavailableNativeRunner;
use ccync_engine::install::{run_update_with_provider_rollout_using, ProviderRollout};
use std::path::Path;
mod support;
use support::isolated;

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

#[test]
fn staging_listings_report_routes_and_hooks_without_writing_listings() {
    let (home, _lock) = isolated();
    let root = home.root();
    let plugin = root.join(".ccync/plugins/example");
    write(
        &plugin.join(".claude-plugin/plugin.json"),
        r#"{"name":"example","version":"1.0.0"}"#,
    );
    write(
        &plugin.join("skills/example/SKILL.md"),
        "---\nname: example\ndescription: Example skill.\n---\nBody.\n",
    );
    write(
        &plugin.join("hooks/hooks.json"),
        r#"{"hooks":{"SessionStart":[]}}"#,
    );
    write(
        &root.join(".ccync/build/lock.json"),
        r#"{"resolvedPlugins":[{"pluginId":"example","resolvedSource":"example","installStrategy":"bundled-local","sha":"0123456789abcdef0123456789abcdef01234567"}]}"#,
    );
    write(
        &root.join(".ccync/config.json"),
        r#"{"selectedProjectionTargets":["claude","codex","copilot","opencode","agy-cli","agy-ide","agy-gui"]}"#,
    );
    let foreign_listing = root.join(".ccync/plugins/foreign/manifest.json");
    write(&foreign_listing, "foreign listing\n");
    let before = std::fs::read(&foreign_listing).unwrap();

    let outcome = run_update_with_provider_rollout_using(
        &ccync_engine::config::CcyncConfig::default(),
        &ProviderRollout { enabled: true },
        &UnavailableNativeRunner,
    )
    .expect("isolated sync");
    let warnings = outcome.evidence.run_warnings.join("\n");
    assert!(root.join(".claude/skills/example/SKILL.md").is_file());
    assert!(root.join(".agents/skills/example/SKILL.md").is_file());
    assert!(!warnings.contains("claude provider example route: ListedNotRegistered"));
    assert!(!warnings.contains("codex provider example route: ListedNotRegistered"));
    assert!(!warnings.contains("replacement"), "{warnings}");
    for target in [
        "claude", "codex", "copilot", "opencode", "agy-cli", "agy-ide", "agy-gui",
    ] {
        assert!(
            warnings.contains(&format!("{target} provider example hooks: unsupported")),
            "{warnings}"
        );
    }
    assert_eq!(std::fs::read(foreign_listing).unwrap(), before);
    let provider_dir = root.join(".ccync/build/render/providers/example");
    assert!(provider_dir.is_dir());
    assert!(
        std::fs::read_dir(provider_dir)
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".views")),
        "staging does not publish per-provider native views"
    );
}
