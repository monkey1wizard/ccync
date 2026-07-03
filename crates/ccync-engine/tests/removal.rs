use ccync_engine::config::CcyncConfig;
use ccync_engine::install::{run_install, run_uninstall};
use std::fs;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};
use tempfile::TempDir;

/// Serialize tests that mutate process-global `HOME`/`USERPROFILE`. `cargo test`
/// runs a crate's integration tests multithreaded in one process, so two tests
/// pointing `ccync_home()` at different temp dirs would race. Poison-tolerant.
fn env_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Point `ccync_home()` / `user_home()` at `home` for the duration of a test.
fn set_home(home: &Path) {
    #[cfg(windows)]
    unsafe {
        std::env::set_var("USERPROFILE", home);
        std::env::set_var("HOME", home);
    }
    #[cfg(not(windows))]
    unsafe {
        std::env::set_var("HOME", home);
    }
}

fn make_ccync_source_tree_with_content(root: &Path) {
    fs::create_dir_all(root.join("agents")).unwrap();
    fs::create_dir_all(root.join("skills").join("doc-sync")).unwrap();
    fs::create_dir_all(root.join("commands").join("ccync")).unwrap();

    fs::write(
        root.join("agents").join("sample-agent.agent.md"),
        "---\nname: sample-agent\ndescription: doc keeper\n---\n# body\n",
    )
    .unwrap();
    fs::write(
        root.join("skills").join("doc-sync").join("SKILL.md"),
        "# doc-sync skill\n",
    )
    .unwrap();
    fs::write(
        root.join("commands").join("ccync").join("SKILL.md"),
        "# ccync command\n",
    )
    .unwrap();
}

#[test]
fn run_uninstall_removes_rust_managed_provider_outputs() {
    let _env = env_guard();
    let fake_home = TempDir::new().unwrap();
    let source = TempDir::new().unwrap();
    make_ccync_source_tree_with_content(source.path());

    set_home(fake_home.path());

    let ccync_home = fake_home.path().join(".ccync");
    fs::create_dir_all(&ccync_home).unwrap();
    // The two persistent INPUT files that must survive uninstall (new layout).
    fs::write(
        ccync_home.join("config.json"),
        serde_json::json!({
            "selectedRuntimes": ["copilot", "codex", "claude"],
            "primaryRuntime": "copilot"
        })
        .to_string(),
    )
    .unwrap();
    fs::write(ccync_home.join("plugins.json"), r#"{"plugins":[]}"#).unwrap();

    // Simulate a pre-rename install's leftover dist tree to exercise the
    // providers→runtimes path migration on the next run (best-effort cleanup).
    let stale_dist = ccync_home.join("dist").join("providers");
    fs::create_dir_all(stale_dist.join("claude")).unwrap();
    fs::write(stale_dist.join("claude").join("managed.json"), "{}").unwrap();

    let config = CcyncConfig {
        dev_mode: Some(true),
        ccync_root: Some(source.path().display().to_string()),
        install_mode: None,
        selected_runtimes: vec![],
        primary_runtime: None,
    };

    run_install(&config).unwrap();

    assert!(ccync_home.join("build").join("render").exists());
    assert!(ccync_home.join("dist").join("runtimes").exists());
    assert!(
        !stale_dist.exists(),
        "stale dist/providers must be migrated away on rerun"
    );
    assert!(fake_home
        .path()
        .join(".copilot")
        .join("installed-plugins")
        .join("ccync-copilot")
        .join("ccync")
        .exists());
    assert!(fake_home
        .path()
        .join(".claude")
        .join("skills")
        .join("ccync")
        .exists());

    run_uninstall().unwrap();

    assert!(!ccync_home.join("build").join("render").exists());
    assert!(!ccync_home.join("dist").join("runtimes").exists());
    assert!(!fake_home
        .path()
        .join(".copilot")
        .join("installed-plugins")
        .join("ccync-copilot")
        .join("ccync")
        .exists());
    assert!(!fake_home
        .path()
        .join(".claude")
        .join("skills")
        .join("ccync")
        .exists());

    // New preserve-set + no-ledger contract: only the two INPUT files survive;
    // build/ and any ledger.json are gone, and no uninstall ledger is written.
    assert!(
        ccync_home.join("config.json").is_file(),
        "config.json (INPUT) must be preserved"
    );
    assert!(
        ccync_home.join("plugins.json").is_file(),
        "plugins.json (INPUT) must be preserved"
    );
    assert!(
        !ccync_home.join("ledger.json").exists(),
        "ledger.json must NOT exist after uninstall (uninstall ledger write removed)"
    );
    assert!(!ccync_home.join("build").exists(), "build/ must be removed");
    let remaining: Vec<String> = fs::read_dir(&ccync_home)
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n != "config.json" && n != "plugins.json")
        .collect();
    assert!(
        remaining.is_empty(),
        "only config.json + plugins.json may remain; found extra: {remaining:?}"
    );
}

// Broader regression: live de-projection preserves user-owned MCP servers and
// Claude plugin records while removing ccync-owned ones, through the full
// run_uninstall flow.
#[test]
fn run_uninstall_preserves_user_mcp_and_plugin_records() {
    let _env = env_guard();
    let fake_home = TempDir::new().unwrap();
    set_home(fake_home.path());
    let h = fake_home.path();
    let ccync = h.join(".ccync");

    fs::create_dir_all(ccync.join("build").join("mcp")).unwrap();
    fs::write(ccync.join("config.json"), "{}").unwrap();
    fs::write(ccync.join("plugins.json"), r#"{"plugins":[]}"#).unwrap();
    fs::write(ccync.join("build").join("lock.json"), "{}").unwrap();
    // Previous managed MCP manifest naming the ccync-owned server.
    fs::write(
        ccync.join("build").join("mcp").join("managed.json"),
        serde_json::json!({ "mcpServers": { "ccync-tool": { "command": "x", "args": [] } } })
            .to_string(),
    )
    .unwrap();
    // Live Claude config with ccync-owned + user MCP servers.
    fs::write(
        h.join(".claude.json"),
        serde_json::json!({
            "mcpServers": {
                "ccync-tool": { "command": "x", "args": [] },
                "user-tool": { "command": "y", "args": [] }
            }
        })
        .to_string(),
    )
    .unwrap();
    // Claude plugin state with a ccync record + a user record.
    let plugins_dir = h.join(".claude").join("plugins");
    fs::create_dir_all(&plugins_dir).unwrap();
    fs::write(
        plugins_dir.join("installed_plugins.json"),
        serde_json::json!({
            "plugins": {
                "ccync@ccync": [{ "version": "1.0.0" }],
                "other@mkt": [{ "version": "2.0.0" }]
            }
        })
        .to_string(),
    )
    .unwrap();

    run_uninstall().unwrap();

    let claude: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(h.join(".claude.json")).unwrap()).unwrap();
    let servers = claude["mcpServers"].as_object().unwrap();
    assert!(!servers.contains_key("ccync-tool"), "managed MCP server removed");
    assert!(servers.contains_key("user-tool"), "user MCP server preserved");

    let installed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(plugins_dir.join("installed_plugins.json")).unwrap())
            .unwrap();
    let plugins = installed["plugins"].as_object().unwrap();
    assert!(!plugins.contains_key("ccync@ccync"), "ccync plugin record removed");
    assert!(plugins.contains_key("other@mkt"), "user plugin record preserved");

    assert!(ccync.join("config.json").is_file(), "config.json preserved");
    assert!(ccync.join("plugins.json").is_file(), "plugins.json preserved");
    assert!(!ccync.join("build").exists(), "build/ removed");
}

// Idempotency (N-3): a second run_uninstall after the first (a partial-crash
// analogue where derived state is already gone) is safe and converges.
#[test]
fn run_uninstall_is_idempotent() {
    let _env = env_guard();
    let fake_home = TempDir::new().unwrap();
    set_home(fake_home.path());
    let ccync = fake_home.path().join(".ccync");

    fs::create_dir_all(ccync.join("build").join("mcp")).unwrap();
    fs::create_dir_all(ccync.join("cache")).unwrap();
    fs::write(ccync.join("config.json"), "{}").unwrap();
    fs::write(ccync.join("plugins.json"), r#"{"plugins":[]}"#).unwrap();
    fs::write(ccync.join("build").join("lock.json"), "{}").unwrap();
    fs::write(ccync.join("build").join("mcp").join("managed.json"), "{}").unwrap();
    fs::write(ccync.join("ledger.json"), "{}").unwrap();

    run_uninstall().unwrap();
    assert!(ccync.join("config.json").is_file());
    assert!(ccync.join("plugins.json").is_file());
    assert!(!ccync.join("build").exists());
    assert!(!ccync.join("cache").exists());
    assert!(!ccync.join("ledger.json").exists());

    // Second run over the now-minimal state must not error and must converge.
    run_uninstall().unwrap();
    assert!(ccync.join("config.json").is_file(), "inputs still preserved after 2nd run");
    assert!(ccync.join("plugins.json").is_file());
    let extra: Vec<String> = fs::read_dir(&ccync)
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n != "config.json" && n != "plugins.json")
        .collect();
    assert!(extra.is_empty(), "second run leaves only inputs; found: {extra:?}");
}
