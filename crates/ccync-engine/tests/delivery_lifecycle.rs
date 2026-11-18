use ccync_engine::adopt::store_adoption_records;
use ccync_engine::config::CcyncConfig;
use ccync_engine::delivery_apply::UnavailableNativeRunner;
use ccync_engine::install::{
    run_uninstall_using, run_update_with_provider_rollout_using, ProviderRollout,
};
use ccync_engine::plugin_upgrade::{apply_upgrade, UpgradeChange};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
mod support;
use support::{isolated as isolated_base, IsolatedHome};

struct DeliveryTestHome {
    home: IsolatedHome,
    prior: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl DeliveryTestHome {
    fn root(&self) -> &Path {
        self.home.root()
    }
}

impl Drop for DeliveryTestHome {
    fn drop(&mut self) {
        for (name, value) in self.prior.drain(..) {
            if let Some(value) = value {
                std::env::set_var(name, value);
            } else {
                std::env::remove_var(name);
            }
        }
    }
}

fn isolated() -> (DeliveryTestHome, ()) {
    let (home, _) = isolated_base();
    let root = home.root().canonicalize().expect("canonical test home");
    let env = [
        ("HOME", root.clone()),
        ("USERPROFILE", root.clone()),
        ("APPDATA", root.join("AppData/Roaming")),
        ("LOCALAPPDATA", root.join("AppData/Local")),
        ("XDG_CONFIG_HOME", root.join(".config")),
        ("XDG_STATE_HOME", root.join(".local/state")),
        ("XDG_CACHE_HOME", root.join(".cache")),
    ];
    let prior = env
        .iter()
        .map(|(name, _)| (*name, std::env::var_os(name)))
        .collect();
    for (name, value) in env {
        std::fs::create_dir_all(&value).expect("test storage root");
        let value = value.canonicalize().expect("canonical test storage root");
        std::env::set_var(name, value);
    }
    (DeliveryTestHome { home, prior }, ())
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn fixture(home: &Path, targets: &str) {
    let plugin = home.join(".ccync/plugins/example");
    write(
        &plugin.join(".claude-plugin/plugin.json"),
        r#"{"name":"example","version":"1.0.0"}"#,
    );
    write(
        &plugin.join("skills/example/SKILL.md"),
        "---\nname: example\ndescription: Example skill.\n---\nBody.\n",
    );
    write(
        &build_root(home).join("lock.json"),
        r#"{"resolvedPlugins":[{"pluginId":"example","resolvedSource":"example","installStrategy":"bundled-local","sha":"0123456789abcdef0123456789abcdef01234567"}]}"#,
    );
    set_targets(home, targets);
}

fn set_targets(_home: &Path, targets: &str) {
    let config_path = ccync_foundation::paths::EffectiveLayout::load_or_legacy()
        .expect("effective test-home layout")
        .config_path();
    write(
        &config_path,
        &format!(r#"{{"selectedProjectionTargets":[{targets}]}}"#),
    );
}

fn load_config(home: &Path) -> CcyncConfig {
    let _ = home;
    let config_path = ccync_foundation::paths::EffectiveLayout::load_or_legacy()
        .expect("effective test-home layout")
        .config_path();
    CcyncConfig::load_from_path(&config_path)
}

fn catalog(home: &Path) -> std::path::PathBuf {
    let _ = home;
    ccync_foundation::paths::EffectiveLayout::load_or_legacy()
        .expect("effective test-home layout")
        .plugins_path()
}

fn build_root(_home: impl AsRef<Path>) -> PathBuf {
    ccync_foundation::paths::EffectiveLayout::load_or_legacy()
        .expect("effective test-home layout")
        .state_base()
}

fn run(home: &Path) -> Result<(), String> {
    run_update_with_provider_rollout_using(
        &load_config(home),
        &ProviderRollout { enabled: true },
        &UnavailableNativeRunner,
    )
    .map(|_| ())
    .map_err(|error| error.to_string())
}

#[test]
fn snapshot_disagreeing_with_config_refuses_before_any_write() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude""#);
    store_adoption_records(
        &catalog(root),
        &[],
        &["claude".to_string(), "codex".to_string()],
    )
    .unwrap();
    let catalog_before = std::fs::read(catalog(root)).unwrap();

    let message = run(root).expect_err("a snapshot that disagrees with config.json must refuse");

    assert!(
        message.contains("changed outside `ccync init`"),
        "{message}"
    );
    assert!(message.contains("ccync init"), "{message}");
    assert!(
        !root.join(".claude/skills/example/SKILL.md").exists(),
        "refusal must happen before any projection write"
    );
    assert_eq!(std::fs::read(catalog(root)).unwrap(), catalog_before);
}

#[test]
fn legacy_machine_without_snapshot_is_accepted() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude""#);

    run(root).expect("a machine with no _adoption snapshot is a legacy machine");

    assert!(root.join(".claude/skills/example/SKILL.md").is_file());
}

#[test]
fn matching_snapshot_is_accepted_and_keeps_adoption_and_unknown_fields() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude""#);
    write(
        &catalog(root),
        r#"{"keepMe":{"owner":"user"},"_adoption":{"records":[],"targetSnapshot":["claude"]}}"#,
    );

    run(root).expect("a matching snapshot must not refuse");

    let text: Value = serde_json::from_slice(&std::fs::read(catalog(root)).unwrap()).unwrap();
    assert_eq!(text["keepMe"], json!({"owner": "user"}));
    assert_eq!(text["_adoption"]["targetSnapshot"], json!(["claude"]));
}

#[test]
fn deselecting_one_provider_through_matching_snapshot_keeps_the_other() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude","codex""#);
    store_adoption_records(
        &catalog(root),
        &[],
        &["claude".to_string(), "codex".to_string()],
    )
    .unwrap();
    run(root).expect("both providers selected and recorded");
    assert!(root.join(".claude/skills/example/SKILL.md").is_file());
    assert!(root.join(".agents/skills/example/SKILL.md").is_file());

    // Deselection is an `ccync init` reselect: config.json and the snapshot change together.
    set_targets(root, r#""claude""#);
    store_adoption_records(&catalog(root), &[], &["claude".to_string()]).unwrap();
    run(root).expect("a reselect that keeps the snapshot in step must not refuse");

    assert!(root.join(".claude/skills/example/SKILL.md").is_file());
    let text: Value = serde_json::from_slice(&std::fs::read(catalog(root)).unwrap()).unwrap();
    assert_eq!(text["_adoption"]["targetSnapshot"], json!(["claude"]));
}

#[test]
fn add_then_readd_keeps_both_providers_and_adoption() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude","codex""#);
    write(
        &catalog(root),
        r#"{"keepMe":1,"_adoption":{"records":[],"targetSnapshot":["claude","codex"]}}"#,
    );

    run(root).expect("add: first sync projects both providers");
    run(root).expect("re-add: second sync over the same selection");

    assert!(root.join(".claude/skills/example/SKILL.md").is_file());
    assert!(root.join(".agents/skills/example/SKILL.md").is_file());
    let text: Value = serde_json::from_slice(&std::fs::read(catalog(root)).unwrap()).unwrap();
    assert_eq!(text["keepMe"], json!(1));
    assert_eq!(
        text["_adoption"]["targetSnapshot"],
        json!(["claude", "codex"])
    );
}

fn seed_both_providers(root: &Path) {
    fixture(root, r#""claude","codex""#);
    write(
        &catalog(root),
        r#"{"keepMe":{"owner":"user"},"_adoption":{"records":[],"targetSnapshot":["claude","codex"]}}"#,
    );
    run(root).expect("both providers projected");
    assert!(root.join(".claude/skills/example/SKILL.md").is_file());
    assert!(root.join(".agents/skills/example/SKILL.md").is_file());
}

#[test]
fn remove_one_provider_keeps_the_other_provider_and_adoption() {
    let (home, _lock) = isolated();
    let root = home.root();
    seed_both_providers(root);

    set_targets(root, r#""claude""#);
    write(
        &catalog(root),
        r#"{"keepMe":{"owner":"user"},"_adoption":{"records":[],"targetSnapshot":["claude"]}}"#,
    );
    run(root).expect("removing codex through a matching snapshot must not refuse");

    assert!(root.join(".claude/skills/example/SKILL.md").is_file());
    let text: Value = serde_json::from_slice(&std::fs::read(catalog(root)).unwrap()).unwrap();
    assert_eq!(text["keepMe"], json!({"owner": "user"}));
    assert_eq!(text["_adoption"]["targetSnapshot"], json!(["claude"]));
}

#[test]
fn upgrade_pin_update_keeps_other_provider_adoption_and_unknown_fields() {
    let (home, _lock) = isolated();
    let root = home.root();
    seed_both_providers(root);

    let old = "0123456789abcdef0123456789abcdef01234567";
    let new = "89abcdef0123456789abcdef0123456789abcdef";
    write(
        &catalog(root),
        &format!(
            r#"{{"keepMe":{{"owner":"user"}},"_adoption":{{"records":[],"targetSnapshot":["claude","codex"]}},"plugins":[{{"pluginId":"example","source":"https://example.invalid/example.git","pin":"{old}"}}]}}"#
        ),
    );
    let cache_root = root.join(".ccync/cache");
    let change = UpgradeChange {
        plugin_id: "example".to_string(),
        source: "https://example.invalid/example.git".to_string(),
        old_sha: Some(old.to_string()),
        new_sha: new.to_string(),
    };
    apply_upgrade(
        &change,
        &cache_root,
        &[catalog(root).as_path()],
        |_, dest| {
            write(&dest.join("plugin.json"), "{}");
            Ok(())
        },
    )
    .expect("upgrade with a landed cache");

    let text: Value = serde_json::from_slice(&std::fs::read(catalog(root)).unwrap()).unwrap();
    assert_eq!(text["plugins"][0]["pin"], json!(new));
    assert_eq!(text["keepMe"], json!({"owner": "user"}));
    assert_eq!(
        text["_adoption"]["targetSnapshot"],
        json!(["claude", "codex"])
    );
    assert!(root.join(".agents/skills/example/SKILL.md").is_file());
}

#[test]
fn missing_source_is_not_removal_intent() {
    let (home, _lock) = isolated();
    let root = home.root();
    seed_both_providers(root);

    std::fs::remove_dir_all(root.join(".ccync/plugins/example")).unwrap();
    let message = run(root).expect_err("a missing source must refuse before any mutation");

    assert!(message.contains("missing"), "{message}");
    assert!(
        root.join(".claude/skills/example/SKILL.md").is_file(),
        "a missing source must not remove the claude surface"
    );
    assert!(
        root.join(".agents/skills/example/SKILL.md").is_file(),
        "a missing source must not remove the codex surface"
    );
}

#[test]
fn uninstall_preserves_adoption_and_config() {
    let (home, _lock) = isolated();
    let root = home.root();
    seed_both_providers(root);

    run_uninstall_using(&UnavailableNativeRunner).expect("uninstall succeeds");

    let text: Value = serde_json::from_slice(&std::fs::read(catalog(root)).unwrap()).unwrap();
    assert_eq!(text["keepMe"], json!({"owner": "user"}));
    assert_eq!(
        text["_adoption"]["targetSnapshot"],
        json!(["claude", "codex"])
    );
    assert!(root.join(".ccync/config.json").is_file());
}

#[test]
fn tp12_spec_two_provider_lifecycle_operations_reconcile_only_proven_paths() {
    let (home, _lock) = isolated();
    let root = home.root();

    let p1 = root.join(".ccync/plugins/alpha");
    write(
        &p1.join(".claude-plugin/plugin.json"),
        r#"{"name":"alpha","version":"1.0.0"}"#,
    );
    write(
        &p1.join("skills/alpha-skill/SKILL.md"),
        "---\nname: alpha-skill\ndescription: Alpha skill.\n---\nAlpha body.\n",
    );
    let p2 = root.join(".ccync/plugins/beta");
    write(
        &p2.join(".claude-plugin/plugin.json"),
        r#"{"name":"beta","version":"1.0.0"}"#,
    );
    write(
        &p2.join("skills/beta-skill/SKILL.md"),
        "---\nname: beta-skill\ndescription: Beta skill.\n---\nBeta body.\n",
    );
    let lock_path = build_root(root).join("lock.json");
    write(
        &lock_path,
        r#"{"resolvedPlugins":[{"pluginId":"alpha","resolvedSource":"alpha","installStrategy":"bundled-local","sha":"1111111111111111111111111111111111111111"},{"pluginId":"beta","resolvedSource":"beta","installStrategy":"bundled-local","sha":"2222222222222222222222222222222222222222"}]}"#,
    );
    write(
        &catalog(root),
        r#"{"keepMe":{"owner":"user"},"_adoption":{"records":[],"targetSnapshot":["claude","codex"]}}"#,
    );
    set_targets(root, r#""claude","codex""#);

    let (outcome, scope) = ccync_engine::install::run_update_with_handoff_using(
        &load_config(root),
        &ccync_engine::delivery_apply::UnavailableNativeRunner,
    )
    .expect("initial sync with handoff");
    assert!(!outcome.routes.is_empty());
    assert!(!scope.decisions.is_empty());

    let claude_alpha = root.join(".claude/skills/alpha-skill/SKILL.md");
    let codex_alpha = root.join(".agents/skills/alpha-skill/SKILL.md");
    let claude_beta = root.join(".claude/skills/beta-skill/SKILL.md");
    let codex_beta = root.join(".agents/skills/beta-skill/SKILL.md");

    assert!(claude_alpha.is_file());
    assert!(codex_alpha.is_file());
    assert!(claude_beta.is_file());
    assert!(codex_beta.is_file());

    let records_after_add = projection::direct_records(&lock_path);
    assert_eq!(records_after_add.len(), 4);
    for record in &records_after_add {
        let path_str = record.path.to_string_lossy().to_ascii_lowercase();
        if path_str.contains("alpha-skill") {
            assert_eq!(record.source_id.as_deref(), Some("alpha"));
        } else if path_str.contains("beta-skill") {
            assert_eq!(record.source_id.as_deref(), Some("beta"));
        } else {
            panic!("unexpected direct record: {record:?}");
        }
    }

    let (outcome2, _scope2) = ccync_engine::install::run_update_with_handoff_using(
        &load_config(root),
        &ccync_engine::delivery_apply::UnavailableNativeRunner,
    )
    .expect("second sync must converge");
    assert!(outcome2.routes.iter().all(|r| r.published));
    let records_after_readd = projection::direct_records(&lock_path);
    assert_eq!(records_after_add, records_after_readd);

    write(
        &p1.join("skills/alpha-skill/SKILL.md"),
        "---\nname: alpha-skill\ndescription: Alpha skill.\n---\nUpgraded alpha body.\n",
    );
    ccync_engine::install::run_update_with_handoff_using(
        &load_config(root),
        &ccync_engine::delivery_apply::UnavailableNativeRunner,
    )
    .expect("upgrade sync succeeds");
    assert!(std::fs::read_to_string(&claude_alpha)
        .unwrap()
        .contains("Upgraded alpha body."));
    assert!(std::fs::read_to_string(&claude_beta)
        .unwrap()
        .contains("Beta body."));
    let records_after_upgrade = projection::direct_records(&lock_path);
    assert_eq!(records_after_upgrade.len(), 4);

    set_targets(root, r#""claude""#);
    store_adoption_records(&catalog(root), &[], &["claude".to_string()]).unwrap();
    ccync_engine::install::run_update_with_handoff_using(
        &load_config(root),
        &ccync_engine::delivery_apply::UnavailableNativeRunner,
    )
    .expect("deselect codex sync succeeds");
    assert!(claude_alpha.is_file());
    assert!(claude_beta.is_file());
    assert!(
        !codex_alpha.exists(),
        "deselected codex alpha output must be pruned"
    );
    assert!(
        !codex_beta.exists(),
        "deselected codex beta output must be pruned"
    );
    let records_after_deselect = projection::direct_records(&lock_path);
    assert_eq!(records_after_deselect.len(), 2);
    for record in &records_after_deselect {
        let path_str = record.path.to_string_lossy().to_ascii_lowercase();
        if path_str.contains("alpha-skill") {
            assert_eq!(record.source_id.as_deref(), Some("alpha"));
        } else if path_str.contains("beta-skill") {
            assert_eq!(record.source_id.as_deref(), Some("beta"));
        }
    }

    let mut lock_val: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&lock_path).unwrap()).unwrap();
    lock_val["resolvedPlugins"] = serde_json::json!([{"pluginId":"alpha","resolvedSource":"alpha","installStrategy":"bundled-local","sha":"1111111111111111111111111111111111111111"}]);
    write(&lock_path, &lock_val.to_string());
    std::fs::remove_dir_all(&p2).unwrap();
    ccync_engine::install::run_update_with_handoff_using(
        &load_config(root),
        &ccync_engine::delivery_apply::UnavailableNativeRunner,
    )
    .expect("remove beta sync succeeds");
    assert!(
        claude_alpha.is_file(),
        "alpha output must survive beta removal"
    );
    assert!(!claude_beta.exists(), "removed beta output must be pruned");
    let records_after_remove = projection::direct_records(&lock_path);
    assert_eq!(records_after_remove.len(), 1);
    assert_eq!(records_after_remove[0].source_id.as_deref(), Some("alpha"));

    std::fs::remove_dir_all(&p1).unwrap();
    let err = ccync_engine::install::run_update_with_handoff_using(
        &load_config(root),
        &ccync_engine::delivery_apply::UnavailableNativeRunner,
    )
    .expect_err("missing source must fail before mutation");
    assert!(
        err.to_string().contains("missing"),
        "error must report missing source"
    );
    assert!(
        claude_alpha.is_file(),
        "missing source must not delete owned artifact"
    );
    let records_after_missing = projection::direct_records(&lock_path);
    assert_eq!(records_after_missing, records_after_remove);

    write(
        &p1.join(".claude-plugin/plugin.json"),
        r#"{"name":"alpha","version":"1.0.0"}"#,
    );
    write(
        &p1.join("skills/alpha-skill/SKILL.md"),
        "---\nname: alpha-skill\ndescription: Alpha skill.\n---\nUpgraded alpha body.\n",
    );

    std::fs::remove_file(&claude_alpha).unwrap();
    std::fs::create_dir_all(&claude_alpha).unwrap();
    std::fs::write(claude_alpha.join("blocking_child"), "x").unwrap();

    let _ = ccync_engine::install::run_update_with_handoff_using(
        &load_config(root),
        &ccync_engine::delivery_apply::UnavailableNativeRunner,
    );
    let records_after_failure = projection::direct_records(&lock_path);
    assert!(
        !records_after_failure.is_empty(),
        "prior proof must survive write failure"
    );
    assert_eq!(records_after_failure[0].source_id.as_deref(), Some("alpha"));
}

/// The machine projection pass that `ccync sync` runs after the engine update.
/// It records the source links that uninstall later removes.
fn machine_sync(home: &Path) {
    let state_root = build_root(home);
    let options = projection::machine_options_from_config(&load_config(home), false, state_root)
        .expect("machine options");
    projection::run_machine_update(&options).expect("machine projection");
}

fn remove_link(path: &Path) {
    std::fs::remove_dir(path)
        .or_else(|_| std::fs::remove_file(path))
        .unwrap();
}

#[test]
fn uninstall_removes_recorded_source_links_and_keeps_their_targets() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""copilot","agy-cli""#);
    write(
        &catalog(root),
        r#"{"keepMe":{"owner":"user"},"_adoption":{"records":[],"targetSnapshot":["copilot","agy-cli"]}}"#,
    );
    run(root).expect("copilot and agy-cli projected");
    machine_sync(root);

    let links = [
        root.join(".copilot/ccync"),
        root.join(".gemini/ccync"),
        root.join(".gemini/antigravity-cli/ccync"),
    ];
    let lock_path = build_root(root).join("lock.json");
    assert_eq!(projection::source_link_records(&lock_path).len(), 3);
    for link in &links {
        assert!(link.symlink_metadata().is_ok(), "{} linked", link.display());
    }
    // `~/.ccync` is the Copilot link's target. Its `config.json` is a preserved
    // input, so it proves the link removal never reached through into the target.
    let target = root.join(".ccync/config.json");
    let foreign = root.join(".copilot/foreign");
    write(&foreign.join("keep.md"), "user");

    run_uninstall_using(&UnavailableNativeRunner).expect("uninstall succeeds");

    for link in &links {
        assert!(
            link.symlink_metadata().is_err(),
            "{} removed",
            link.display()
        );
    }
    assert!(target.is_file(), "link target must survive");
    assert_eq!(
        std::fs::read_to_string(foreign.join("keep.md")).unwrap(),
        "user"
    );
    assert!(!lock_path.exists(), "no source-link row may keep lock.json");
}

#[test]
fn uninstall_leaves_a_source_link_the_user_replaced_with_a_directory() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""copilot""#);
    write(
        &catalog(root),
        r#"{"keepMe":{"owner":"user"},"_adoption":{"records":[],"targetSnapshot":["copilot"]}}"#,
    );
    run(root).expect("copilot projected");
    machine_sync(root);

    let link = root.join(".copilot/ccync");
    remove_link(&link);
    write(&link.join("user.md"), "user");

    run_uninstall_using(&UnavailableNativeRunner).expect("uninstall succeeds");

    assert!(
        link.is_dir() && !link.is_symlink(),
        "user directory must stay"
    );
    assert_eq!(
        std::fs::read_to_string(link.join("user.md")).unwrap(),
        "user"
    );
    assert!(
        !build_root(root).join("lock.json").exists(),
        "the replaced path's row is forgotten, so lock.json is not retained"
    );
}

#[test]
fn tp30_spec_sync_and_uninstall_removes_links_preserves_targets_and_user_replaced_dir() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""copilot","agy-cli""#);
    write(
        &catalog(root),
        r#"{"keepMe":{"owner":"user"},"_adoption":{"records":[],"targetSnapshot":["copilot","agy-cli"]}}"#,
    );
    run(root).expect("copilot and agy-cli projected");
    machine_sync(root);

    let lock_path = build_root(root).join("lock.json");
    let initial_records = projection::source_link_records(&lock_path);
    assert_eq!(
        initial_records.len(),
        3,
        "machine sync must record 3 source links for copilot and agy-cli"
    );

    let copilot_link = root.join(".copilot/ccync");
    let gemini_link = root.join(".gemini/ccync");
    let agy_cli_link = root.join(".gemini/antigravity-cli/ccync");
    assert!(
        copilot_link.symlink_metadata().is_ok(),
        "copilot link must exist"
    );
    assert!(
        gemini_link.symlink_metadata().is_ok(),
        "gemini link must exist"
    );
    assert!(
        agy_cli_link.symlink_metadata().is_ok(),
        "agy-cli link must exist"
    );

    // Replace one link path (.copilot/ccync) with a real user directory and content
    remove_link(&copilot_link);
    std::fs::create_dir_all(&copilot_link).unwrap();
    let user_file = copilot_link.join("user_custom.txt");
    let user_bytes = b"user replaced link with genuine directory";
    std::fs::write(&user_file, user_bytes).unwrap();

    // Create a foreign directory in the agent area with user data
    let foreign = root.join(".copilot/foreign");
    let foreign_file = foreign.join("foreign_custom.txt");
    let foreign_bytes = b"foreign directory data to be preserved";
    write(&foreign_file, std::str::from_utf8(foreign_bytes).unwrap());

    // Link target ~/.ccync/config.json with known bytes
    let target_file = root.join(".ccync/config.json");
    assert!(
        target_file.is_file(),
        "link target must exist before uninstall"
    );
    let target_bytes_before = std::fs::read(&target_file).expect("read target bytes");

    // Run uninstall
    run_uninstall_using(&UnavailableNativeRunner).expect("uninstall succeeds");

    // 1. Link presence:
    // Untouched links must be removed
    assert!(
        gemini_link.symlink_metadata().is_err(),
        "gemini link must be removed"
    );
    assert!(
        agy_cli_link.symlink_metadata().is_err(),
        "agy-cli link must be removed"
    );
    // User-replaced directory must be preserved in place
    assert!(
        copilot_link.is_dir() && !copilot_link.is_symlink(),
        "user directory must remain in place and not be a symlink"
    );
    assert_eq!(
        std::fs::read(&user_file).unwrap(),
        user_bytes,
        "user directory contents must match original bytes"
    );

    // 2. Foreign directory and target bytes unchanged:
    assert_eq!(
        std::fs::read(&foreign_file).unwrap(),
        foreign_bytes,
        "foreign directory bytes must remain untouched"
    );
    assert_eq!(
        std::fs::read(&target_file).unwrap(),
        target_bytes_before,
        "target bytes must remain untouched"
    );

    // 3. Source-link rows:
    // No source-link row remains in lockfile (or lockfile itself is removed)
    assert!(
        !lock_path.exists() || projection::source_link_records(&lock_path).is_empty(),
        "no source-link rows should remain after uninstall"
    );
}

/// Builds this worktree's CLI once per test process and returns its path.
fn p1_built_cli() -> std::path::PathBuf {
    static BUILT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .to_path_buf();
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let target = root.join("target");
            let status = std::process::Command::new(cargo)
                .args(["build", "-p", "ccync-cli", "--target-dir"])
                .arg(&target)
                .current_dir(&root)
                .env_remove("CARGO_TARGET_DIR")
                .status()
                .expect("cargo builds this worktree's CLI");
            assert!(status.success(), "building this worktree's CLI failed");
            target
                .join("debug")
                .join(format!("ccync{}", std::env::consts::EXE_SUFFIX))
        })
        .clone()
}

fn p1_cli(home: &Path) -> std::process::Output {
    let binary = match std::env::var_os("CCYNC_TEST_BINARY") {
        Some(explicit) => std::path::PathBuf::from(explicit),
        None => p1_built_cli(),
    };
    let binary = binary.canonicalize().expect("built CLI exists");
    let worktree = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap();
    assert!(
        binary.starts_with(worktree.join("target")),
        "CLI must be built in this worktree: {}",
        binary.display()
    );
    let home = home.canonicalize().unwrap();
    let empty_bin = home.join("empty-provider-bin");
    std::fs::create_dir_all(&empty_bin).unwrap();
    std::process::Command::new(binary)
        .args(["sync", "--yes"])
        .current_dir(&home)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("APPDATA", home.join("AppData/Roaming"))
        .env("LOCALAPPDATA", home.join("AppData/Local"))
        .env("CCYNC_HOME", home.join(".ccync"))
        .env("CODEX_HOME", home.join(".codex"))
        .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("PATH", empty_bin)
        .env_remove("CCYNC_ASSUME_YES")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("real CLI executes")
}

fn p1_output(output: &std::process::Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn p1_loose_fixture(home: &Path) -> std::path::PathBuf {
    let home = home.canonicalize().unwrap();
    let source = home.join("inputs/p1-fixture");
    write(
        &source.join("SKILL.md"),
        "---\nname: p1-fixture\ndescription: Isolated fixture.\n---\nRead resources/data.txt.\n",
    );
    write(&source.join("resources/data.txt"), "complete resource\n");
    write(
        &build_root(&home).join("lock.json"),
        &json!({"_looseSkills": [{"name":"p1-fixture", "source":"fixture", "origin": source}]})
            .to_string(),
    );
    set_targets(&home, r#""codex""#);
    source
}

fn p1_pending(
    home: &Path,
    boundary: u8,
) -> ccync_foundation::provider::StandalonePublicationIntent {
    use ccync_foundation::provider::{
        PathRoot, ProviderLifecycleJournal, RelativePath, ScopedPath,
    };
    let home = home.canonicalize().unwrap();
    p1_loose_fixture(&home);
    let first = p1_cli(&home);
    assert!(first.status.success(), "{}", p1_output(&first));
    let build = build_root(&home);
    let record = projection::standalone_publication_records(&build.join("lock.json"))
        .unwrap()
        .remove(0);
    // A prior version attribution makes registry-before differ from after,
    // so the fixture exercises a real registry commit rather than a no-op.
    let mut prior: Value =
        serde_json::from_slice(&std::fs::read(build.join("lock.json")).unwrap()).unwrap();
    for proof in prior["_ccyncProjection"]["standalonePublications"]
        .as_object_mut()
        .unwrap()
        .values_mut()
    {
        proof["sourceRevision"] = json!("previous-source-proof");
    }
    write(&build.join("lock.json"), &prior.to_string());
    let request = projection::StandalonePublicationRequest {
        operation_id: "p1-interrupted".into(),
        source_path: home.join("inputs/p1-fixture"),
        source_id: record.source_id.clone(),
        source_revision: record.source_revision,
        destination: ScopedPath {
            root: PathRoot::UserHome,
            path: RelativePath::new(".agents/skills/p1-fixture").unwrap(),
        },
        destination_root: home.clone(),
        build_root: build.clone(),
        stage_path: RelativePath::new("standalone-publications/p1-interrupted/stage").unwrap(),
        backup_path: RelativePath::new("standalone-publications/p1-interrupted/backup").unwrap(),
        discovery_roots: vec![home.join(".agents/skills")],
        targets: vec!["codex".into()],
        reserved_paths: Vec::new(),
        prior_owner: Some(record.source_id.clone()),
        owned_fingerprint: Some(record.tree_fingerprint),
        manifest_proofs: Vec::new(),
    };
    let prepared = projection::prepare_standalone_publication(request).unwrap();
    let intent = prepared.intent().clone();
    let mut journal = ProviderLifecycleJournal::from_json_str(
        &std::fs::read_to_string(build.join("projection.txn.json")).unwrap(),
    )
    .unwrap();
    journal.standalone_intents.push(intent.clone());
    write(
        &build.join("projection.txn.json"),
        &journal.to_json_string().unwrap(),
    );
    projection::stage_standalone_publication(&prepared, &intent).unwrap();
    if boundary >= 1 {
        projection::publish_standalone_publication(&prepared, &intent).unwrap();
    }
    if boundary >= 2 {
        projection::complete_standalone_publication(&prepared, &intent).unwrap();
    }
    intent
}

fn p1_assert_recovered(home: &Path) {
    let result = p1_cli(home);
    assert!(result.status.success(), "{}", p1_output(&result));
    assert!(
        !p1_output(&result).contains("recovery blocked"),
        "{}",
        p1_output(&result)
    );
    let journal = ccync_foundation::provider::ProviderLifecycleJournal::from_json_str(
        &std::fs::read_to_string(build_root(home).join("projection.txn.json")).unwrap(),
    )
    .unwrap();
    assert!(journal.standalone_intents.is_empty());
    assert!(!build_root(home)
        .join("standalone-publications/p1-interrupted/backup")
        .exists());
    let destination = home.join(".agents/skills/p1-fixture");
    assert!(
        !ccync_foundation::platform::is_symlink_or_junction(&destination),
        "later machine phase must not recreate render link"
    );
    assert_eq!(
        std::fs::read_to_string(destination.join("resources/data.txt")).unwrap(),
        "complete resource\n"
    );
    let records =
        projection::standalone_publication_records(&build_root(home).join("lock.json")).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        projection::standalone_source_fingerprint(&destination).unwrap(),
        records[0].tree_fingerprint
    );
}

fn committed_provider_build(home: &Path) -> PathBuf {
    let (config, state, cache) = if cfg!(windows) {
        let appdata = home.join("AppData/Roaming");
        let local = home.join("AppData/Local");
        std::fs::create_dir_all(&appdata).unwrap();
        std::fs::create_dir_all(&local).unwrap();
        (
            appdata.join("ccync"),
            local.join("ccync/state"),
            local.join("ccync/cache"),
        )
    } else {
        let config = home.join(".config");
        let state = home.join(".local/state");
        let cache = home.join(".cache");
        std::env::set_var("XDG_CONFIG_HOME", &config);
        std::env::set_var("XDG_STATE_HOME", &state);
        std::env::set_var("XDG_CACHE_HOME", &cache);
        (
            config.join("ccync"),
            state.join("ccync"),
            cache.join("ccync"),
        )
    };
    let portable = home.join(".agents/ccync");
    let roots = [&config, &state, &cache, &portable];
    for root in roots {
        std::fs::create_dir_all(root).unwrap();
    }
    let commit = ccync_foundation::provider::StorageLayoutCommit {
        layout_version: 1,
        roots: vec![
            ccync_foundation::provider::LayoutRootIdentity {
                name: "portable".into(),
                path: portable,
            },
            ccync_foundation::provider::LayoutRootIdentity {
                name: "config".into(),
                path: config.clone(),
            },
            ccync_foundation::provider::LayoutRootIdentity {
                name: "state".into(),
                path: state.clone(),
            },
            ccync_foundation::provider::LayoutRootIdentity {
                name: "cache".into(),
                path: cache,
            },
        ],
    };
    let text = serde_json::to_string(&commit).unwrap();
    write(&home.join(".ccync/migration.json"), &text);
    write(&state.join("layout.json"), &text);
    let config_text = r#"{"selectedProjectionTargets":["codex"]}"#;
    write(&config.join("config.json"), config_text);
    state.join("build")
}

#[test]
fn provider_state_authority_standalone_publication_uses_committed_build_root() {
    let (home, _lock) = isolated();
    let build = committed_provider_build(home.root());
    use ccync_foundation::provider::{PathRoot, RelativePath, ScopedPath};
    let destination = home.root().join(".agents/skills/committed-skill");
    let source = home.root().join("inputs/committed-skill");
    std::fs::create_dir_all(home.root().join(".agents/skills")).unwrap();
    write(
        &source.join("SKILL.md"),
        "---\nname: committed-skill\ndescription: Committed fixture.\n---\nRead this skill.\n",
    );
    write(&build.join("lock.json"), "{}");
    let request = projection::StandalonePublicationRequest {
        operation_id: "committed-publication".into(),
        source_path: source,
        source_id: "committed-fixture".into(),
        source_revision: "fixture-revision".into(),
        destination: ScopedPath {
            root: PathRoot::UserHome,
            path: RelativePath::new(".agents/skills/committed-skill").unwrap(),
        },
        destination_root: home.root().to_path_buf(),
        build_root: build.clone(),
        stage_path: RelativePath::new("standalone-publications/committed-publication/stage")
            .unwrap(),
        backup_path: RelativePath::new("standalone-publications/committed-publication/backup")
            .unwrap(),
        discovery_roots: vec![home.root().join(".agents/skills")],
        targets: vec!["codex".into()],
        reserved_paths: Vec::new(),
        prior_owner: None,
        owned_fingerprint: None,
        manifest_proofs: Vec::new(),
    };
    let prepared = projection::prepare_standalone_publication(request).unwrap();
    let intent = prepared.intent().clone();
    let mut journal = ccync_foundation::provider::ProviderLifecycleJournal::default();
    journal.standalone_intents.push(intent.clone());
    write(
        &build.join("projection.txn.json"),
        &journal.to_json_string().unwrap(),
    );
    projection::stage_standalone_publication(&prepared, &intent).unwrap();
    projection::publish_standalone_publication(&prepared, &intent).unwrap();
    projection::complete_standalone_publication(&prepared, &intent).unwrap();
    journal.standalone_intents.clear();
    write(
        &build.join("projection.txn.json"),
        &journal.to_json_string().unwrap(),
    );

    assert!(destination.join("SKILL.md").is_file());
    assert_eq!(
        projection::standalone_publication_records(&build.join("lock.json"))
            .unwrap()
            .len(),
        1
    );
    assert!(build.join("projection.txn.json").is_file());
    assert!(!home.root().join(".ccync/build/lock.json").exists());
    assert!(!home
        .root()
        .join(".ccync/build/standalone-publications")
        .exists());
}

#[test]
fn p1_standalone_repeated_real_cli_sync_preserves_resources_and_independent_tree() {
    let (home, _lock) = isolated();
    p1_loose_fixture(home.root());
    p1_assert_recovered(home.root());
    let before =
        projection::standalone_publication_records(&build_root(home.root()).join("lock.json"))
            .unwrap();
    p1_assert_recovered(home.root());
    assert_eq!(
        before,
        projection::standalone_publication_records(&build_root(home.root()).join("lock.json"))
            .unwrap()
    );
}

#[test]
fn p1_standalone_real_cli_recovers_staged_before_switch() {
    let (home, _lock) = isolated();
    p1_pending(home.root(), 0);
    p1_assert_recovered(home.root());
}

#[test]
fn p1_standalone_real_cli_recovers_after_switch_before_registry() {
    let (home, _lock) = isolated();
    p1_pending(home.root(), 1);
    p1_assert_recovered(home.root());
}

#[test]
fn final_route_publication_commits_proof_before_clearing_intent() {
    let (home, _lock) = isolated();
    let intent = p1_pending(home.root(), 2);
    let journal = p1_journal(home.root());
    assert!(
        journal.standalone_intents.contains(&intent),
        "the intent remains journaled at the publication completion boundary"
    );
    let records =
        projection::standalone_publication_records(&build_root(home.root()).join("lock.json"))
            .unwrap();
    assert_eq!(
        records.len(),
        1,
        "the committed proof is durable before recovery"
    );
    assert_eq!(
        projection::standalone_source_fingerprint(&home.root().join(".agents/skills/p1-fixture"))
            .unwrap(),
        records[0].tree_fingerprint,
        "the committed proof matches the published bytes while the intent remains"
    );

    p1_assert_recovered(home.root());
    assert!(
        p1_journal(home.root()).standalone_intents.is_empty(),
        "recovery clears the intent after the proof is committed"
    );
}

#[test]
fn p1_standalone_real_cli_recovers_interrupted_destination_rename() {
    let (home, _lock) = isolated();
    let intent = p1_pending(home.root(), 0);
    let destination = home.root().join(".agents/skills/p1-fixture");
    let backup = build_root(home.root()).join(intent.backup_path.as_str());
    std::fs::rename(destination, &backup).unwrap();
    p1_assert_recovered(home.root());
}

#[test]
fn p1_standalone_real_cli_foreign_drift_preserves_journal_backup_and_user_bytes() {
    let (home, _lock) = isolated();
    let intent = p1_pending(home.root(), 1);
    let destination = home.root().join(".agents/skills/p1-fixture");
    write(&destination.join("SKILL.md"), "user-owned drift\n");
    let journal_path = build_root(home.root()).join("projection.txn.json");
    let output = p1_cli(home.root());
    assert!(
        p1_output(&output).contains("recovery blocked"),
        "{}",
        p1_output(&output)
    );
    assert_eq!(
        std::fs::read_to_string(destination.join("SKILL.md")).unwrap(),
        "user-owned drift\n"
    );
    let journal = ccync_foundation::provider::ProviderLifecycleJournal::from_json_str(
        &std::fs::read_to_string(journal_path).unwrap(),
    )
    .unwrap();
    assert!(journal.standalone_intents.contains(&intent));
    assert!(build_root(home.root())
        .join(intent.backup_path.as_str())
        .exists());
    assert!(!destination.is_symlink());
    assert!(
        !home.root().join(".agents/skills/ccync").exists(),
        "failure must not create umbrella fallback"
    );
}

#[test]
fn p1_standalone_real_cli_registry_namespace_drift_blocks_recovery() {
    let (home, _lock) = isolated();
    let intent = p1_pending(home.root(), 1);
    let lockfile = build_root(home.root()).join("lock.json");
    let mut value: Value = serde_json::from_slice(&std::fs::read(&lockfile).unwrap()).unwrap();
    let ancestor = home.root().canonicalize().unwrap().join(".agents/skills");
    let key = projection::normalize_managed_artifact_path(&ancestor);
    value["_ccyncProjection"]["directAgentProjectionPaths"] = json!([key]);
    write(&lockfile, &value.to_string());
    let output = p1_cli(home.root());
    assert!(
        p1_output(&output).contains("managed path collision"),
        "{}",
        p1_output(&output)
    );
    let pending = ccync_foundation::provider::ProviderLifecycleJournal::from_json_str(
        &std::fs::read_to_string(build_root(home.root()).join("projection.txn.json")).unwrap(),
    )
    .unwrap();
    assert!(pending.standalone_intents.contains(&intent));
    assert!(build_root(home.root())
        .join(intent.backup_path.as_str())
        .exists());
}

#[test]
#[cfg(windows)]
fn p1_standalone_real_cli_recovers_after_actual_registry_persistence_failure() {
    use ccync_foundation::provider::RelativePath;
    use std::os::windows::fs::OpenOptionsExt;
    let (home, _lock) = isolated();
    let intent = p1_pending(home.root(), 1);
    let home_path = home.root().canonicalize().unwrap();
    let build = build_root(&home_path);
    let proof: Value = serde_json::from_str(&intent.source).unwrap();
    let request = projection::StandalonePublicationRequest {
        operation_id: intent.operation_id.clone(),
        source_path: home_path.join("inputs/p1-fixture"),
        source_id: proof["origin"].as_str().unwrap().into(),
        source_revision: proof["revision"].as_str().unwrap().into(),
        destination: intent.destination.clone(),
        destination_root: home_path.clone(),
        build_root: build.clone(),
        stage_path: RelativePath::new(intent.stage_path.as_str()).unwrap(),
        backup_path: intent.backup_path.clone(),
        discovery_roots: vec![home_path.join(".agents/skills")],
        targets: intent.targets.clone(),
        reserved_paths: Vec::new(),
        prior_owner: intent
            .registry_before
            .as_ref()
            .and_then(|row| row["source"].as_str())
            .map(str::to_string),
        owned_fingerprint: intent
            .preimage_fingerprint
            .as_ref()
            .map(|digest| digest.to_string()),
        manifest_proofs: Vec::new(),
    };
    let prepared = projection::resume_standalone_publication(request, &intent).unwrap();
    // Allow reads but deny replacement/deletion of lock.json. This fails the
    // actual registry atomic rename after publication, without a product switch.
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(build.join("lock.json"))
        .unwrap();
    assert!(projection::complete_standalone_publication(&prepared, &intent).is_err());
    assert!(build.join(intent.backup_path.as_str()).exists());
    assert!(
        ccync_foundation::provider::ProviderLifecycleJournal::from_json_str(
            &std::fs::read_to_string(build.join("projection.txn.json")).unwrap()
        )
        .unwrap()
        .standalone_intents
        .contains(&intent)
    );
    drop(held);
    p1_assert_recovered(home.root());
}

fn p1_lock_value(home: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(build_root(home).join("lock.json")).unwrap()).unwrap()
}

fn p1_loose_rows(home: &Path) -> Value {
    p1_lock_value(home)["_looseSkills"].clone()
}

/// Edit only the loose-source namespace, leaving the ownership registry as the CLI wrote it.
fn p1_write_loose_rows(home: &Path, rows: Option<Value>) {
    let mut lock = p1_lock_value(home);
    match rows {
        Some(rows) => lock["_looseSkills"] = rows,
        None => {
            lock.as_object_mut().unwrap().remove("_looseSkills");
        }
    }
    write(&build_root(home).join("lock.json"), &lock.to_string());
}

fn p1_records(home: &Path) -> Vec<projection::StandaloneProjectionRecord> {
    projection::standalone_publication_records(&build_root(home).join("lock.json")).unwrap()
}

fn p1_journal(home: &Path) -> ccync_foundation::provider::ProviderLifecycleJournal {
    ccync_foundation::provider::ProviderLifecycleJournal::from_json_str(
        &std::fs::read_to_string(build_root(home).join("projection.txn.json")).unwrap(),
    )
    .unwrap()
}

fn p1_assert_absent_and_unowned(home: &Path) {
    let destination = home.join(".agents/skills/p1-fixture");
    assert!(
        destination.symlink_metadata().is_err(),
        "withdrawn path must be genuinely absent: {}",
        destination.display()
    );
    assert!(p1_records(home).is_empty());
    let registry = p1_lock_value(home)["_ccyncProjection"]
        .to_string()
        .to_lowercase();
    assert!(
        !registry.contains("p1-fixture"),
        "ownership row must be removed: {registry}"
    );
    assert!(p1_journal(home).standalone_withdrawal_intents.is_empty());
    assert!(
        !home.join(".agents/skills/ccync").exists(),
        "withdrawal must not create an umbrella fallback"
    );
}

#[test]
fn p1_standalone_real_cli_withdraws_removed_source_then_reenable_publishes_once() {
    let (home, _lock) = isolated();
    let home = home.root().canonicalize().unwrap();
    p1_loose_fixture(&home);
    p1_select_every_shared_root_reader(&home);
    p1_assert_recovered(&home);
    let published = p1_records(&home);
    let rows = p1_loose_rows(&home);
    p1_write_loose_rows(&home, None);
    for _ in 0..2 {
        let result = p1_cli(&home);
        assert!(result.status.success(), "{}", p1_output(&result));
        assert!(
            !p1_output(&result).contains("recovery blocked"),
            "{}",
            p1_output(&result)
        );
        p1_assert_absent_and_unowned(&home);
        assert!(
            !build_root(&home)
                .join("standalone-withdrawals")
                .read_dir()
                .is_ok_and(|mut entries| entries
                    .any(|entry| entry.is_ok_and(|entry| entry.path().join("backup").exists()))),
            "completed withdrawal must clean its private backup"
        );
    }
    p1_write_loose_rows(&home, Some(rows));
    for _ in 0..2 {
        p1_assert_recovered(&home);
        assert_eq!(p1_records(&home), published);
    }
}

#[test]
fn p1_standalone_real_cli_missing_source_without_removal_intent_keeps_committed_proof() {
    let (home, _lock) = isolated();
    let home = home.root().canonicalize().unwrap();
    p1_loose_fixture(&home);
    p1_assert_recovered(&home);
    let published = p1_records(&home);
    // The source row remains desired, only its bytes are gone.
    std::fs::remove_dir_all(home.join("inputs/p1-fixture")).unwrap();
    for _ in 0..2 {
        let _ = p1_cli(&home);
        let destination = home.join(".agents/skills/p1-fixture");
        assert_eq!(
            std::fs::read_to_string(destination.join("resources/data.txt")).unwrap(),
            "complete resource\n"
        );
        assert_eq!(p1_records(&home), published);
        assert!(p1_journal(&home).standalone_withdrawal_intents.is_empty());
    }
}

#[test]
fn p1_standalone_real_cli_unselected_reader_keeps_shared_path_and_proof() {
    let (home, _lock) = isolated();
    let home = home.root().canonicalize().unwrap();
    p1_loose_fixture(&home);
    set_targets(&home, r#""codex","copilot""#);
    p1_assert_recovered(&home);
    let published = p1_records(&home);
    assert_eq!(published[0].targets.len(), 2, "fixture must share the path");
    set_targets(&home, r#""codex""#);
    p1_write_loose_rows(&home, None);
    for _ in 0..2 {
        let output = p1_cli(&home);
        assert!(
            p1_output(&output).contains("withdrawal blocked"),
            "{}",
            p1_output(&output)
        );
        let destination = home.join(".agents/skills/p1-fixture");
        assert_eq!(
            std::fs::read_to_string(destination.join("resources/data.txt")).unwrap(),
            "complete resource\n"
        );
        assert_eq!(p1_records(&home), published);
        assert!(p1_journal(&home).standalone_withdrawal_intents.is_empty());
    }
}

#[test]
fn p1_standalone_never_selected_shared_root_reader_keeps_path_and_proof() {
    let (home, _lock) = isolated();
    let home = home.root().canonicalize().unwrap();
    p1_loose_fixture(&home);
    p1_assert_recovered(&home);
    let published = p1_records(&home);
    assert_eq!(
        published[0].targets,
        vec!["codex"],
        "only codex is ever selected"
    );
    p1_write_loose_rows(&home, None);
    for _ in 0..2 {
        let output = p1_cli(&home);
        assert!(
            p1_output(&output).contains("withdrawal blocked"),
            "{}",
            p1_output(&output)
        );
        let destination = home.join(".agents/skills/p1-fixture");
        assert_eq!(
            std::fs::read_to_string(destination.join("resources/data.txt")).unwrap(),
            "complete resource\n"
        );
        assert_eq!(p1_records(&home), published);
        assert!(p1_journal(&home).standalone_withdrawal_intents.is_empty());
    }
}

#[test]
fn p1_standalone_real_cli_commits_new_readers_across_upgrades_before_withdrawal() {
    let (home, _lock) = isolated();
    let home = home.root().canonicalize().unwrap();
    let source = p1_loose_fixture(&home);
    let first = p1_cli(&home);
    assert!(first.status.success(), "{}", p1_output(&first));

    set_targets(&home, r#""codex","copilot""#);
    let added_reader = p1_cli(&home);
    assert!(
        added_reader.status.success(),
        "{}",
        p1_output(&added_reader)
    );
    let record = p1_records(&home).remove(0);
    assert_eq!(record.targets, vec!["codex", "copilot"]);

    write(&source.join("resources/data.txt"), "upgraded resource\n");
    let upgraded = p1_cli(&home);
    assert!(upgraded.status.success(), "{}", p1_output(&upgraded));
    let upgraded_record = p1_records(&home).remove(0);
    assert_eq!(upgraded_record.targets, vec!["codex", "copilot"]);
    assert_eq!(
        std::fs::read_to_string(home.join(".agents/skills/p1-fixture/resources/data.txt")).unwrap(),
        "upgraded resource\n"
    );

    set_targets(&home, r#""codex""#);
    p1_write_loose_rows(&home, None);
    for _ in 0..2 {
        let result = p1_cli(&home);
        assert!(p1_output(&result).contains("withdrawal blocked"));
        assert!(home.join(".agents/skills/p1-fixture").exists());
        assert_eq!(p1_records(&home).len(), 1);
        assert!(p1_journal(&home).standalone_withdrawal_intents.is_empty());
    }
}

#[test]
fn p1_standalone_real_cli_self_origin_reconciles_readers_before_withdrawal() {
    let (home, _lock) = isolated();
    let home = home.root().canonicalize().unwrap();
    let source = p1_loose_fixture(&home);
    let first = p1_cli(&home);
    assert!(first.status.success(), "{}", p1_output(&first));

    let published = home.join(".agents/skills/p1-fixture");
    set_targets(&home, r#""codex","copilot""#);
    let mut lock = p1_lock_value(&home);
    lock["_looseSkills"][0]["origin"] = json!(published);
    write(&build_root(&home).join("lock.json"), &lock.to_string());
    let self_origin = p1_cli(&home);
    assert!(self_origin.status.success(), "{}", p1_output(&self_origin));
    assert_eq!(p1_records(&home)[0].targets, vec!["codex", "copilot"]);

    set_targets(&home, r#""codex""#);
    p1_write_loose_rows(&home, None);
    let withdrawal = p1_cli(&home);
    assert!(
        p1_output(&withdrawal).contains("withdrawal blocked"),
        "{}",
        p1_output(&withdrawal)
    );
    assert!(published.join("resources/data.txt").exists());
    assert_eq!(p1_records(&home)[0].targets, vec!["codex", "copilot"]);
    let _ = source;
}

#[test]
fn p1_standalone_real_cli_malformed_desired_collections_retain_publications() {
    for key in ["_looseSkills", "resolvedPlugins", "_personalPlugins"] {
        for malformed in [json!({"unexpected": true}), Value::Null] {
            let (home, _lock) = isolated();
            let home = home.root().canonicalize().unwrap();
            p1_loose_fixture(&home);
            let first = p1_cli(&home);
            assert!(first.status.success(), "{}", p1_output(&first));
            let published = p1_records(&home);
            let mut lock = p1_lock_value(&home);
            if key == "_looseSkills" {
                lock.as_object_mut().unwrap().remove("_looseSkills");
            }
            lock[key] = malformed;
            write(&build_root(&home).join("lock.json"), &lock.to_string());

            let _result = p1_cli(&home);
            assert_eq!(
                p1_records(&home),
                published,
                "malformed {key} changed ownership"
            );
            assert_eq!(
                std::fs::read_to_string(home.join(".agents/skills/p1-fixture/resources/data.txt"))
                    .unwrap(),
                "complete resource\n",
                "malformed {key} authorized withdrawal"
            );
        }
    }
}

/// Selects every client that reads the shared `.agents/skills` root.
fn p1_select_every_shared_root_reader(home: &Path) {
    set_targets(home, r#""codex","copilot","opencode""#);
}

fn p1_withdrawal_pending(
    home: &Path,
    boundary: u8,
) -> ccync_foundation::provider::StandaloneWithdrawalIntent {
    use ccync_foundation::provider::{
        PathRoot, RelativePath, ScopedPath, StandaloneWithdrawalReaderIntent,
        StandaloneWithdrawalReaderProof,
    };
    let home = home.canonicalize().unwrap();
    p1_loose_fixture(&home);
    p1_select_every_shared_root_reader(&home);
    let first = p1_cli(&home);
    assert!(first.status.success(), "{}", p1_output(&first));
    let build = build_root(&home);
    let record = p1_records(&home).remove(0);
    p1_write_loose_rows(&home, None);
    let request = projection::StandaloneWithdrawalRequest {
        operation_id: "p1-withdraw".into(),
        readers: ["codex", "copilot", "opencode"]
            .into_iter()
            .map(|reader| StandaloneWithdrawalReaderProof {
                reader: reader.into(),
                intent: StandaloneWithdrawalReaderIntent::Disabled,
                evidence: format!("source-removed:{}", record.source_id),
            })
            .collect(),
        committed_record: record,
        destination: ScopedPath {
            root: PathRoot::UserHome,
            path: RelativePath::new(".agents/skills/p1-fixture").unwrap(),
        },
        destination_root: home.clone(),
        build_root: build.clone(),
        backup_path: RelativePath::new("standalone-withdrawals/p1-withdraw/backup").unwrap(),
        discovery_roots: vec![home.join(".agents/skills")],
        reserved_paths: Vec::new(),
    };
    let prepared = projection::prepare_standalone_withdrawal(request).unwrap();
    let intent = prepared.intent().clone();
    let mut journal = p1_journal(&home);
    journal.standalone_withdrawal_intents.push(intent.clone());
    let journal_path = build.join("projection.txn.json");
    write(&journal_path, &journal.to_json_string().unwrap());
    if boundary >= 1 {
        projection::apply_standalone_withdrawal(&prepared, &intent).unwrap();
    }
    if boundary >= 2 {
        projection::complete_standalone_withdrawal(&prepared, &intent).unwrap();
    }
    if boundary >= 3 {
        journal.standalone_withdrawal_intents.clear();
        write(&journal_path, &journal.to_json_string().unwrap());
    }
    intent
}

fn p1_assert_withdrawal_recovered(home: &Path, backup_retained: bool) {
    let backup = build_root(&home).join("standalone-withdrawals/p1-withdraw/backup");
    for _ in 0..2 {
        let result = p1_cli(home);
        assert!(result.status.success(), "{}", p1_output(&result));
        assert!(
            !p1_output(&result).contains("recovery blocked"),
            "{}",
            p1_output(&result)
        );
        p1_assert_absent_and_unowned(home);
        assert_eq!(
            backup.exists(),
            backup_retained,
            "private backup state differs after recovery"
        );
    }
}

#[test]
fn p1_standalone_real_cli_recovers_withdrawal_after_intent_before_rename() {
    let (home, _lock) = isolated();
    p1_withdrawal_pending(home.root(), 0);
    p1_assert_withdrawal_recovered(&home.root().canonicalize().unwrap(), false);
}

#[test]
fn p1_standalone_real_cli_recovers_withdrawal_after_rename_before_registry() {
    let (home, _lock) = isolated();
    p1_withdrawal_pending(home.root(), 1);
    p1_assert_withdrawal_recovered(&home.root().canonicalize().unwrap(), false);
}

#[test]
fn p1_standalone_real_cli_recovers_withdrawal_after_registry_before_clear() {
    let (home, _lock) = isolated();
    p1_withdrawal_pending(home.root(), 2);
    p1_assert_withdrawal_recovered(&home.root().canonicalize().unwrap(), false);
}

#[test]
fn p1_standalone_real_cli_keeps_orphan_backup_after_withdrawal_intent_cleared() {
    let (home, _lock) = isolated();
    p1_withdrawal_pending(home.root(), 3);
    p1_assert_withdrawal_recovered(&home.root().canonicalize().unwrap(), true);
}

#[test]
fn p1_standalone_real_cli_withdrawal_foreign_drift_preserves_evidence_and_user_bytes() {
    let (home, _lock) = isolated();
    let intent = p1_withdrawal_pending(home.root(), 1);
    let home = home.root().canonicalize().unwrap();
    let destination = home.join(".agents/skills/p1-fixture");
    write(&destination.join("SKILL.md"), "user-owned drift\n");
    for _ in 0..2 {
        let output = p1_cli(&home);
        assert!(
            p1_output(&output).contains("recovery blocked"),
            "{}",
            p1_output(&output)
        );
        assert_eq!(
            std::fs::read_to_string(destination.join("SKILL.md")).unwrap(),
            "user-owned drift\n"
        );
        assert!(p1_journal(&home)
            .standalone_withdrawal_intents
            .iter()
            .any(|pending| pending.operation_id == intent.operation_id));
        assert!(build_root(&home).join(intent.backup_path.as_str()).exists());
        assert!(!home.join(".agents/skills/ccync").exists());
    }
}

fn p1_exit_ok(output: &std::process::Output) -> bool {
    output.status.success()
}

fn p1_second_loose_skill(home: &Path) {
    let other = home.join("inputs/p1-other");
    write(
        &other.join("SKILL.md"),
        "---\nname: p1-other\ndescription: Second isolated fixture.\n---\nKeep me.\n",
    );
    let mut rows = p1_loose_rows(home);
    rows.as_array_mut()
        .unwrap()
        .push(json!({"name": "p1-other", "source": "fixture", "origin": other}));
    p1_write_loose_rows(home, Some(rows));
}

#[test]
fn p1_standalone_real_cli_withdrawal_removes_only_the_withdrawn_ownership_row() {
    let (home, _lock) = isolated();
    let home = home.root().canonicalize().unwrap();
    p1_loose_fixture(&home);
    p1_select_every_shared_root_reader(&home);
    p1_second_loose_skill(&home);
    let first = p1_cli(&home);
    assert!(first.status.success(), "{}", p1_output(&first));
    assert_eq!(p1_records(&home).len(), 2);
    let other_before: Vec<_> = p1_records(&home)
        .into_iter()
        .filter(|record| record.path.ends_with("p1-other"))
        .collect();
    let mut rows = p1_loose_rows(&home);
    rows.as_array_mut()
        .unwrap()
        .retain(|row| row["name"] == "p1-other");
    p1_write_loose_rows(&home, Some(rows));
    for _ in 0..2 {
        let result = p1_cli(&home);
        assert!(result.status.success(), "{}", p1_output(&result));
        assert!(home
            .join(".agents/skills/p1-fixture")
            .symlink_metadata()
            .is_err());
        assert!(
            std::fs::read_to_string(home.join(".agents/skills/p1-other/SKILL.md"))
                .unwrap()
                .contains("Keep me.")
        );
        let remaining = p1_records(&home);
        assert_eq!(remaining, other_before, "unrelated ownership row changed");
    }
}

#[test]
fn p1_standalone_real_cli_blocked_withdrawal_reports_exit_status_and_keeps_proof() {
    let (home, _lock) = isolated();
    let home = home.root().canonicalize().unwrap();
    p1_loose_fixture(&home);
    set_targets(&home, r#""codex","copilot""#);
    p1_assert_recovered(&home);
    set_targets(&home, r#""codex""#);
    p1_write_loose_rows(&home, None);
    let blocked = p1_cli(&home);
    let again = p1_cli(&home);
    assert_eq!(
        p1_exit_ok(&blocked),
        p1_exit_ok(&again),
        "blocked withdrawal status must be stable"
    );
    assert!(
        p1_output(&blocked).contains("withdrawal blocked"),
        "{}",
        p1_output(&blocked)
    );
    assert_eq!(p1_records(&home).len(), 1);
}

#[test]
fn p1_standalone_real_cli_readded_source_after_registry_removal_forward_completes_then_publishes() {
    let (home, _lock) = isolated();
    p1_withdrawal_pending(home.root(), 2);
    let home = home.root().canonicalize().unwrap();
    // Reader evidence changes: the source is desired again.
    let origin = home.join("inputs/p1-fixture");
    p1_write_loose_rows(
        &home,
        Some(json!([{"name": "p1-fixture", "source": "fixture", "origin": origin}])),
    );
    for _ in 0..2 {
        p1_assert_recovered(&home);
    }
    assert!(p1_journal(&home).standalone_withdrawal_intents.is_empty());
    assert!(!build_root(&home)
        .join("standalone-withdrawals/p1-withdraw")
        .exists());
    assert_eq!(p1_records(&home).len(), 1);
}

#[test]
fn p1_standalone_real_cli_readded_source_before_rename_abandons_the_intent_without_moving_bytes() {
    let (home, _lock) = isolated();
    p1_withdrawal_pending(home.root(), 0);
    let home = home.root().canonicalize().unwrap();
    let origin = home.join("inputs/p1-fixture");
    p1_write_loose_rows(
        &home,
        Some(json!([{"name": "p1-fixture", "source": "fixture", "origin": origin}])),
    );
    let published = p1_records(&home);
    for _ in 0..2 {
        p1_assert_recovered(&home);
        assert_eq!(p1_records(&home), published);
    }
    assert!(p1_journal(&home).standalone_withdrawal_intents.is_empty());
}

#[test]
fn p1_standalone_real_cli_unreadable_registry_blocks_withdrawal_then_recovers() {
    let (home, _lock) = isolated();
    let intent = p1_withdrawal_pending(home.root(), 1);
    let home = home.root().canonicalize().unwrap();
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Reads stay allowed, replacement of the registry is denied.
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(build_root(&home).join("lock.json"))
            .unwrap();
        let _ = p1_cli(&home);
        assert!(
            p1_journal(&home)
                .standalone_withdrawal_intents
                .iter()
                .any(|pending| pending.operation_id == intent.operation_id),
            "failed registry persistence must keep the intent"
        );
        assert!(build_root(&home).join(intent.backup_path.as_str()).exists());
        assert_eq!(p1_records(&home).len(), 1, "registry row must remain");
        drop(held);
    }
    p1_assert_withdrawal_recovered(&home, false);
}

#[test]
fn p1_standalone_real_cli_uninstall_refuses_a_pending_withdrawal_and_keeps_evidence() {
    let (home, _lock) = isolated();
    let intent = p1_withdrawal_pending(home.root(), 1);
    let home = home.root().canonicalize().unwrap();
    let error = run_uninstall_using(&UnavailableNativeRunner)
        .expect_err("pending withdrawal must block uninstall");
    assert!(error.to_string().contains("recovery blocked"), "{error}");
    assert!(p1_journal(&home)
        .standalone_withdrawal_intents
        .iter()
        .any(|pending| pending.operation_id == intent.operation_id));
    assert!(build_root(&home).join(intent.backup_path.as_str()).exists());
}

#[test]
fn p1_standalone_real_cli_changed_legacy_link_referent_is_never_captured() {
    changed_legacy_link_referent_is_never_captured(false);
}

#[cfg(not(windows))]
#[test]
fn standalone_legacy_lock_key_changed_link_referent_is_never_captured() {
    changed_legacy_link_referent_is_never_captured(true);
}

fn changed_legacy_link_referent_is_never_captured(use_legacy_key: bool) {
    let (home, _lock) = isolated();
    let home = home.root().canonicalize().unwrap();
    p1_loose_fixture(&home);
    let first = p1_cli(&home);
    assert!(first.status.success(), "{}", p1_output(&first));
    let build = build_root(&home);
    let destination = home.join(".agents/skills/p1-fixture");
    let referent = ccync_foundation::paths::canonical_plugin_root()
        .unwrap()
        .join("skills/p1-fixture");
    // Rebuild the pre-standalone state: a legacy link and legacy registry rows.
    let mut lock = p1_lock_value(&home);
    let key = {
        let proofs = lock["_ccyncProjection"]["standalonePublications"]
            .as_object_mut()
            .unwrap();
        let key = proofs.keys().next().unwrap().clone();
        proofs.remove(&key);
        key
    };
    let section = lock["_ccyncProjection"].as_object_mut().unwrap();
    for paths in section.values_mut().filter_map(Value::as_array_mut) {
        paths.retain(|path| path.as_str() != Some(key.as_str()));
    }
    let key = if use_legacy_key {
        key.replace('/', "\\")
    } else {
        key
    };
    section
        .entry("sourceAttribution")
        .or_insert_with(|| json!({}))[&key] = json!("fixture");
    std::fs::remove_dir_all(&destination).unwrap();
    ccync_foundation::platform::create_dir_link(&referent, &destination).unwrap();
    let raw_target = std::fs::read_link(&destination).unwrap();
    section.insert("sourceLinkProjectionPaths".into(), json!([key]));
    write(&build.join("lock.json"), &lock.to_string());
    // The render now changes the very content the link points at.
    write(
        &home.join("inputs/p1-fixture/SKILL.md"),
        "---\nname: p1-fixture\ndescription: Isolated fixture.\n---\nChanged body.\n",
    );
    let second = p1_cli(&home);
    assert!(
        p1_output(&second).contains("legacy link referent changed"),
        "{}",
        p1_output(&second)
    );
    assert!(ccync_foundation::platform::is_symlink_or_junction(
        &destination
    ));
    assert_eq!(std::fs::read_link(&destination).unwrap(), raw_target);
    assert!(p1_journal(&home).standalone_intents.is_empty());
    assert!(!build
        .join("standalone-publications")
        .read_dir()
        .is_ok_and(|mut entries| entries
            .any(|entry| entry.is_ok_and(|entry| entry.path().join("backup").exists()))));
    // The render is now stable, so the proof holds and publication proceeds.
    let third = p1_cli(&home);
    assert!(third.status.success(), "{}", p1_output(&third));
    assert!(p1_journal(&home).standalone_intents.is_empty());
    assert!(!ccync_foundation::platform::is_symlink_or_junction(
        &destination
    ));
    assert!(std::fs::read_to_string(destination.join("SKILL.md"))
        .unwrap()
        .contains("Changed body."));
}
