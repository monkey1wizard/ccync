use ccync_engine::delivery_apply::UnavailableNativeRunner;
use ccync_engine::install::run_uninstall_using;
use std::fs;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};
use tempfile::TempDir;

/// Serialize tests that mutate process-global `HOME`/`USERPROFILE`.
fn env_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Point `ccync_home()` / `user_home()` at `home` for the duration of a test.
struct EnvRestore(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            unsafe {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
        }
    }
}

fn set_home(home: &Path) -> EnvRestore {
    let paths = [
        ("HOME", home.to_path_buf()),
        ("USERPROFILE", home.to_path_buf()),
        ("APPDATA", home.join("AppData/Roaming")),
        ("LOCALAPPDATA", home.join("AppData/Local")),
        ("XDG_CONFIG_HOME", home.join("xdg-config")),
        ("XDG_STATE_HOME", home.join("xdg-state")),
        ("XDG_CACHE_HOME", home.join("xdg-cache")),
    ];
    let saved = paths
        .iter()
        .map(|(key, _)| (*key, std::env::var_os(key)))
        .collect();
    for (key, value) in paths {
        unsafe { std::env::set_var(key, value) };
    }
    EnvRestore(saved)
}

fn commit_layout(
    home: &Path,
    legacy_fingerprints: serde_json::Value,
) -> (
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    use ccync_foundation::provider::{LayoutRootIdentity, StorageLayoutCommit};
    let roots = ccync_foundation::paths::EffectiveLayout::load_or_legacy()
        .unwrap()
        .storage_roots()
        .clone();

    let portable = roots.portable.clone();
    let config = roots.config.clone();
    let state = roots.state.clone();
    let cache = roots.cache.clone();
    let commit = StorageLayoutCommit {
        layout_version: 1,
        roots: vec![
            LayoutRootIdentity {
                name: "portable".into(),
                path: portable.clone(),
            },
            LayoutRootIdentity {
                name: "config".into(),
                path: config.clone(),
            },
            LayoutRootIdentity {
                name: "state".into(),
                path: state.clone(),
            },
            LayoutRootIdentity {
                name: "cache".into(),
                path: cache.clone(),
            },
        ],
    };
    let commit_value = serde_json::to_value(&commit).unwrap();
    fs::create_dir_all(&portable).unwrap();
    fs::create_dir_all(&config).unwrap();
    fs::create_dir_all(&state).unwrap();
    fs::create_dir_all(&cache).unwrap();
    let commit_bytes = serde_json::to_vec(&commit).unwrap();
    fs::write(state.join("layout.json"), &commit_bytes).unwrap();
    fs::create_dir_all(home.join(".ccync")).unwrap();
    fs::write(home.join(".ccync/migration.json"), &commit_bytes).unwrap();
    fs::write(
        state.join("migration.json"),
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "stage": "prepared",
            "commit": commit_value,
            "legacyFingerprints": legacy_fingerprints,
            "managedFingerprints": []
        }))
        .unwrap(),
    )
    .unwrap();
    (portable, config, state, cache)
}

#[test]
fn uninstall_sweep_preserves_agents_md_input_file() {
    let _guard = env_guard();
    let fake_home = TempDir::new().unwrap();
    let _env_restore = set_home(fake_home.path());

    let ccync_home = fake_home.path().join(".ccync");
    fs::create_dir_all(ccync_home.join("build").join("render")).unwrap();
    fs::create_dir_all(ccync_home.join("cache")).unwrap();
    fs::write(ccync_home.join("config.json"), "{}").unwrap();
    fs::write(ccync_home.join("plugins.json"), r#"{"plugins":[]}"#).unwrap();

    let agents_md = ccync_home.join("AGENTS.md");
    let agents_content = "# User Instruction Content\nKeep this file across uninstall.\n";
    fs::write(&agents_md, agents_content).unwrap();

    run_uninstall_using(&UnavailableNativeRunner).expect("uninstall should succeed");

    assert!(
        agents_md.exists(),
        "uninstall sweep must preserve ~/.ccync/AGENTS.md"
    );
    assert_eq!(
        fs::read_to_string(&agents_md).unwrap(),
        agents_content,
        "AGENTS.md content must be preserved unchanged"
    );
    assert!(
        ccync_home.join("config.json").exists(),
        "config.json must be preserved"
    );
    assert!(
        ccync_home.join("plugins.json").exists(),
        "plugins.json must be preserved"
    );
    let build_left: Vec<_> = fs::read_dir(ccync_home.join("build"))
        .map(|d| d.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(
        build_left.iter().all(|n| n == "lifecycle.lock"),
        "build must keep only the lifecycle lock, found {build_left:?}"
    );
    assert!(
        !ccync_home.join("cache").exists(),
        "cache directory must be removed"
    );
}

#[test]
fn uninstall_sweep_preserves_agents_md_when_other_inputs_absent() {
    let _guard = env_guard();
    let fake_home = TempDir::new().unwrap();
    let _env_restore = set_home(fake_home.path());

    let ccync_home = fake_home.path().join(".ccync");
    fs::create_dir_all(ccync_home.join("build")).unwrap();

    let agents_md = ccync_home.join("AGENTS.md");
    let agents_content = "# Standalone AGENTS.md\n";
    fs::write(&agents_md, agents_content).unwrap();

    run_uninstall_using(&UnavailableNativeRunner).expect("uninstall should succeed");

    assert!(
        agents_md.exists(),
        "uninstall sweep must preserve ~/.ccync/AGENTS.md when config/plugins absent"
    );
    assert_eq!(
        fs::read_to_string(&agents_md).unwrap(),
        agents_content,
        "AGENTS.md content must be preserved unchanged"
    );
}

#[test]
fn uninstall_sweep_preserves_mcp_json_byte_for_byte_when_other_inputs_absent() {
    let _guard = env_guard();
    let fake_home = TempDir::new().unwrap();
    let _env_restore = set_home(fake_home.path());

    let ccync_home = fake_home.path().join(".ccync");
    fs::create_dir_all(ccync_home.join("build")).unwrap();

    let mcp_json = ccync_home.join("mcp.json");
    let mcp_content = b"{\n  \"mcpServers\": {\"user-server\": {\"command\": \"tool\"}}\n}\n";
    fs::write(&mcp_json, mcp_content).unwrap();

    run_uninstall_using(&UnavailableNativeRunner).expect("uninstall should succeed");

    assert!(
        mcp_json.exists(),
        "uninstall sweep must preserve ~/.ccync/mcp.json"
    );
    assert_eq!(
        fs::read(&mcp_json).unwrap(),
        mcp_content,
        "mcp.json bytes must be preserved unchanged"
    );
}

#[test]
fn uninstall_sweep_preserves_all_inputs_including_mcp_json_byte_for_byte() {
    let _guard = env_guard();
    let fake_home = TempDir::new().unwrap();
    let _env_restore = set_home(fake_home.path());

    let ccync_home = fake_home.path().join(".ccync");
    fs::create_dir_all(ccync_home.join("build").join("render")).unwrap();
    fs::create_dir_all(ccync_home.join("cache")).unwrap();

    let config_path = ccync_home.join("config.json");
    let config_content = b"{\"master\": \"claude\"}\n";
    fs::write(&config_path, config_content).unwrap();

    let plugins_path = ccync_home.join("plugins.json");
    let plugins_content = b"{\"plugins\": [\"test-plugin\"]}\n";
    fs::write(&plugins_path, plugins_content).unwrap();

    let agents_path = ccync_home.join("AGENTS.md");
    let agents_content = b"# All Inputs Instruction Content\n";
    fs::write(&agents_path, agents_content).unwrap();

    let mcp_path = ccync_home.join("mcp.json");
    let mcp_content = b"{\n  \"mcpServers\": {\"custom\": {\"command\": \"mcp-server\"}}\n}\n";
    fs::write(&mcp_path, mcp_content).unwrap();

    run_uninstall_using(&UnavailableNativeRunner).expect("uninstall should succeed");

    assert!(config_path.exists(), "config.json must be preserved");
    assert_eq!(
        fs::read(&config_path).unwrap(),
        config_content,
        "config.json bytes must be preserved unchanged"
    );

    assert!(plugins_path.exists(), "plugins.json must be preserved");
    assert_eq!(
        fs::read(&plugins_path).unwrap(),
        plugins_content,
        "plugins.json bytes must be preserved unchanged"
    );

    assert!(agents_path.exists(), "AGENTS.md must be preserved");
    assert_eq!(
        fs::read(&agents_path).unwrap(),
        agents_content,
        "AGENTS.md bytes must be preserved unchanged"
    );

    assert!(mcp_path.exists(), "mcp.json must be preserved");
    assert_eq!(
        fs::read(&mcp_path).unwrap(),
        mcp_content,
        "mcp.json bytes must be preserved unchanged"
    );

    let build_left: Vec<_> = fs::read_dir(ccync_home.join("build"))
        .map(|d| d.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(
        build_left.iter().all(|n| n == "lifecycle.lock"),
        "build must keep only the lifecycle lock, found {build_left:?}"
    );
    assert!(
        !ccync_home.join("cache").exists(),
        "cache directory must be removed"
    );
}

#[test]
fn lifecycle_private_paths_committed_layout_keeps_authority_and_prunes_private_state() {
    let _guard = env_guard();
    let home = TempDir::new().unwrap();
    let _env_restore = set_home(home.path());
    let legacy_input = home.path().join(".ccync/legacy-input.json");
    fs::create_dir_all(legacy_input.parent().unwrap()).unwrap();
    fs::write(&legacy_input, "legacy input").unwrap();
    let legacy_fingerprint = projection::shared_path_fingerprint(&legacy_input).unwrap();
    let evidence = serde_json::json!([{
        "path": legacy_input,
        "fingerprint": legacy_fingerprint
    }]);
    let (portable, config, state, cache) = commit_layout(home.path(), evidence);
    fs::write(portable.join("plugins.json"), "portable input").unwrap();
    fs::write(config.join("config.json"), "machine input").unwrap();
    fs::create_dir_all(state.join("build/mcp")).unwrap();
    fs::write(state.join("build/mcp/managed.json"), "{}").unwrap();
    fs::create_dir_all(state.join("dist/targets")).unwrap();
    fs::write(state.join("dist/targets/ccync-private"), "derived").unwrap();
    fs::write(cache.join("source.tar"), "derived").unwrap();
    fs::create_dir_all(portable.join("plugins/.agents")).unwrap();
    fs::write(portable.join("plugins/.agents/marker"), "marker").unwrap();
    let legacy_lock = home.path().join(".ccync/build/lifecycle.lock");
    fs::create_dir_all(legacy_lock.parent().unwrap()).unwrap();
    fs::write(&legacy_lock, "").unwrap();
    let layout_path = state.join("layout.json");
    let journal_path = state.join("migration.json");
    let sentinel_path = home.path().join(".ccync/migration.json");

    run_uninstall_using(&UnavailableNativeRunner).expect("committed uninstall should succeed");
    assert!(!legacy_input.exists());
    run_uninstall_using(&UnavailableNativeRunner)
        .expect("committed uninstall should succeed after legacy input pruning");

    assert!(portable.join("plugins.json").is_file());
    assert!(config.join("config.json").is_file());
    assert!(layout_path.is_file());
    assert!(journal_path.is_file());
    assert!(sentinel_path.is_file());
    assert!(legacy_lock.is_file());
    assert!(state.join("lifecycle.lock").is_file());
    assert!(!state.join("build").exists());
    assert!(!state.join("dist").exists());
    assert!(!cache.exists());
    assert!(!portable.join("plugins/.agents").exists());
}

#[test]
fn lifecycle_private_paths_mutation_blocked_leaves_files_unchanged() {
    let _guard = env_guard();
    let home = TempDir::new().unwrap();
    let _env_restore = set_home(home.path());
    let (portable, config, state, cache) = commit_layout(home.path(), serde_json::json!([]));
    let marker = home.path().join(".ccync/migration.json");
    fs::write(&marker, "invalid conflicting marker").unwrap();
    fs::write(portable.join("private"), "portable").unwrap();
    fs::write(config.join("private"), "config").unwrap();
    fs::write(state.join("private"), "state").unwrap();
    fs::write(cache.join("private"), "cache").unwrap();
    fs::write(state.join("lifecycle.lock"), "state lock").unwrap();
    fs::create_dir_all(home.path().join(".ccync/build")).unwrap();
    fs::write(
        home.path().join(".ccync/build/lifecycle.lock"),
        "rendezvous lock",
    )
    .unwrap();
    let before = [
        fs::read(portable.join("private")).unwrap(),
        fs::read(config.join("private")).unwrap(),
        fs::read(state.join("private")).unwrap(),
        fs::read(cache.join("private")).unwrap(),
        fs::read(&marker).unwrap(),
        fs::read(state.join("lifecycle.lock")).unwrap(),
        fs::read(home.path().join(".ccync/build/lifecycle.lock")).unwrap(),
    ];

    let result = run_uninstall_using(&UnavailableNativeRunner);

    assert!(result.is_err(), "MutationBlocked must stop uninstall");
    assert_eq!(fs::read(portable.join("private")).unwrap(), before[0]);
    assert_eq!(fs::read(config.join("private")).unwrap(), before[1]);
    assert_eq!(fs::read(state.join("private")).unwrap(), before[2]);
    assert_eq!(fs::read(cache.join("private")).unwrap(), before[3]);
    assert_eq!(fs::read(&marker).unwrap(), before[4]);
    assert_eq!(fs::read(state.join("lifecycle.lock")).unwrap(), before[5]);
    assert_eq!(
        fs::read(home.path().join(".ccync/build/lifecycle.lock")).unwrap(),
        before[6]
    );
}

#[test]
fn lifecycle_private_paths_migration_drift_leaves_files_unchanged() {
    let _guard = env_guard();
    let home = TempDir::new().unwrap();
    let _env_restore = set_home(home.path());
    let legacy_input = home.path().join(".ccync/config.json");
    fs::create_dir_all(legacy_input.parent().unwrap()).unwrap();
    fs::write(&legacy_input, "original legacy input").unwrap();
    let original_fingerprint = projection::shared_path_fingerprint(&legacy_input).unwrap();
    fs::write(&legacy_input, "edited legacy input").unwrap();
    let evidence = serde_json::json!([{
        "path": legacy_input,
        "fingerprint": original_fingerprint
    }]);
    let (portable, config, state, cache) = commit_layout(home.path(), evidence);
    fs::write(portable.join("private"), "portable").unwrap();
    fs::write(config.join("private"), "config").unwrap();
    fs::write(state.join("private"), "state").unwrap();
    fs::write(cache.join("private"), "cache").unwrap();
    let journal = state.join("migration.json");
    let before_journal = fs::read(&journal).unwrap();

    let result = run_uninstall_using(&UnavailableNativeRunner);

    assert!(result.is_err(), "legacy drift must stop uninstall");
    assert!(portable.join("private").is_file());
    assert!(config.join("private").is_file());
    assert!(state.join("private").is_file());
    assert!(cache.join("private").is_file());
    assert_eq!(fs::read(&journal).unwrap(), before_journal);
    assert_eq!(fs::read(&legacy_input).unwrap(), b"edited legacy input");
}

#[test]
fn lifecycle_private_paths_changed_managed_path_does_not_block_uninstall() {
    let _guard = env_guard();
    let home = TempDir::new().unwrap();
    let _env_restore = set_home(home.path());
    let (portable, config, state, cache) = commit_layout(home.path(), serde_json::json!([]));
    let managed_live_path = home.path().join(".claude/managed.json");
    fs::create_dir_all(managed_live_path.parent().unwrap()).unwrap();
    fs::write(&managed_live_path, "changed managed path").unwrap();
    let journal = state.join("migration.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
    manifest["managedFingerprints"] = serde_json::json!([{
        "path": managed_live_path,
        "fingerprint": "old managed fingerprint"
    }]);
    fs::write(&journal, serde_json::to_vec(&manifest).unwrap()).unwrap();
    fs::write(portable.join("private"), "portable").unwrap();
    fs::write(config.join("private"), "config").unwrap();
    fs::write(state.join("private"), "state").unwrap();
    fs::write(cache.join("private"), "cache").unwrap();

    run_uninstall_using(&UnavailableNativeRunner)
        .expect("managed live-path changes must not block uninstall");
}

#[test]
fn lifecycle_private_paths_legacy_layout_keeps_existing_uninstall_behavior() {
    let _guard = env_guard();
    let home = TempDir::new().unwrap();
    let _env_restore = set_home(home.path());
    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    fs::create_dir_all(ccync.join("cache")).unwrap();
    fs::write(ccync.join("config.json"), "legacy input").unwrap();
    fs::write(ccync.join("cache/source"), "legacy cache").unwrap();

    run_uninstall_using(&UnavailableNativeRunner).expect("legacy uninstall should succeed");

    assert_eq!(
        fs::read(ccync.join("config.json")).unwrap(),
        b"legacy input"
    );
    assert!(!ccync.join("cache").exists());
}
