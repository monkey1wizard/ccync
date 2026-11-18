use ccync_engine::config::CcyncConfig;
use ccync_engine::delivery_apply::UnavailableNativeRunner;
use ccync_engine::install::{run_uninstall, run_update_with_evidence_using};
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

/// Uninstall keeps the persistent lifecycle lock; nothing else may remain in `build/`.
fn build_holds_only_lifecycle_lock(ccync_home: &Path) -> bool {
    fs::read_dir(ccync_home.join("build"))
        .map(|d| d.flatten().all(|e| e.file_name() == "lifecycle.lock"))
        .unwrap_or(true)
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

    let _env_restore = set_home(fake_home.path());

    let ccync_home = fake_home.path().join(".ccync");
    fs::create_dir_all(&ccync_home).unwrap();
    // The two persistent INPUT files that must survive uninstall (new layout).
    fs::write(
        ccync_home.join("config.json"),
        serde_json::json!({
            "selectedProjectionTargets": ["copilot", "codex", "claude"]
        })
        .to_string(),
    )
    .unwrap();
    fs::write(ccync_home.join("plugins.json"), r#"{"plugins":[]}"#).unwrap();

    // Simulate a pre-rename install's leftover dist trees to exercise both
    // legacy-path migrations on the next run (best-effort cleanup):
    // providers→runtimes (older rename) and runtimes→targets (this chain).
    let stale_providers_dist = ccync_home.join("dist").join("providers");
    fs::create_dir_all(stale_providers_dist.join("claude")).unwrap();
    fs::write(
        stale_providers_dist.join("claude").join("managed.json"),
        "{}",
    )
    .unwrap();
    let stale_runtimes_dist = ccync_home.join("dist").join("runtimes");
    fs::create_dir_all(stale_runtimes_dist.join("claude")).unwrap();
    fs::write(
        stale_runtimes_dist.join("claude").join("managed.json"),
        "{}",
    )
    .unwrap();

    let config = CcyncConfig {
        dev_mode: Some(true),
        ccync_root: Some(source.path().display().to_string()),
        install_mode: None,
        selected_runtimes: vec![],
        selected_projection_targets: vec![],
        private_fields: Default::default(),
    };

    run_update_with_evidence_using(&config, &UnavailableNativeRunner).unwrap();

    assert!(ccync_home.join("build").join("render").exists());
    assert!(ccync_home.join("dist").join("targets").exists());
    // New JSON key: the target-state descriptor for claude must say "target",
    // never the retired "runtime" spelling.
    let claude_state: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(
            ccync_home
                .join("dist")
                .join("targets")
                .join("claude")
                .join("managed.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(claude_state["target"], "claude");
    assert!(
        claude_state.get("runtime").is_none(),
        "runtime key must not appear in current serialization"
    );
    assert!(
        !stale_providers_dist.exists(),
        "stale dist/providers must be migrated away on rerun"
    );
    assert!(
        !stale_runtimes_dist.exists(),
        "stale dist/runtimes must be migrated away on rerun"
    );
    // The ordinary sync retired the combined Copilot plugin copy once direct
    // publication succeeded, so it is absent until uninstall runs.
    assert!(!fake_home
        .path()
        .join(".copilot")
        .join("installed-plugins")
        .join("ccync-copilot")
        .join("ccync")
        .exists());
    // The ordinary sync retired the combined Claude link once direct
    // publication succeeded, so uninstall has nothing left to remove there.
    assert!(!fake_home
        .path()
        .join(".claude")
        .join("skills")
        .join("ccync")
        .exists());

    run_uninstall().unwrap();

    assert!(!ccync_home.join("build").join("render").exists());
    assert!(!ccync_home.join("dist").join("targets").exists());
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
    assert!(
        build_holds_only_lifecycle_lock(&ccync_home),
        "build/ must keep only the lifecycle lock"
    );
    let remaining: Vec<String> = fs::read_dir(&ccync_home)
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n != "config.json" && n != "plugins.json")
        .filter(|n| n != "build" || !build_holds_only_lifecycle_lock(&ccync_home))
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
    let _env_restore = set_home(fake_home.path());
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
    // Committed per-host ownership (write-ahead model): Claude owns ccync-tool,
    // so uninstall removes only it and leaves the unowned user-tool alone.
    fs::write(
        ccync.join("build").join("mcp").join("projected-state.json"),
        serde_json::json!({ "version": 1, "hosts": { "claude": ["ccync-tool"] } }).to_string(),
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
    assert!(
        !servers.contains_key("ccync-tool"),
        "managed MCP server removed"
    );
    assert!(
        servers.contains_key("user-tool"),
        "user MCP server preserved"
    );

    let installed: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(plugins_dir.join("installed_plugins.json")).unwrap(),
    )
    .unwrap();
    let plugins = installed["plugins"].as_object().unwrap();
    assert!(
        !plugins.contains_key("ccync@ccync"),
        "ccync plugin record removed"
    );
    assert!(
        plugins.contains_key("other@mkt"),
        "user plugin record preserved"
    );

    assert!(ccync.join("config.json").is_file(), "config.json preserved");
    assert!(
        ccync.join("plugins.json").is_file(),
        "plugins.json preserved"
    );
    assert!(
        build_holds_only_lifecycle_lock(&ccync),
        "build/ keeps only the lifecycle lock"
    );
}

// Idempotency (N-3): a second run_uninstall after the first (a partial-crash
// analogue where derived state is already gone) is safe and converges.
#[test]
fn run_uninstall_is_idempotent() {
    let _env = env_guard();
    let fake_home = TempDir::new().unwrap();
    let _env_restore = set_home(fake_home.path());
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
    assert!(build_holds_only_lifecycle_lock(&ccync));
    assert!(!ccync.join("cache").exists());
    assert!(!ccync.join("ledger.json").exists());

    // Second run over the now-minimal state must not error and must converge.
    run_uninstall().unwrap();
    assert!(
        ccync.join("config.json").is_file(),
        "inputs still preserved after 2nd run"
    );
    assert!(ccync.join("plugins.json").is_file());
    let extra: Vec<String> = fs::read_dir(&ccync)
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n != "config.json" && n != "plugins.json")
        .filter(|n| n != "build" || !build_holds_only_lifecycle_lock(&ccync))
        .collect();
    assert!(
        extra.is_empty(),
        "second run leaves only inputs; found: {extra:?}"
    );
}

/// A partial MCP uninstall must leave behind the committed ownership record, so
/// the retry `run_mcp_remove_managed` promises ("committed ownership retained —
/// re-run `ccync uninstall`") can actually happen.
///
/// Without this the failure is permanent: the ~/.ccync cleanup deletes every
/// child except config.json/plugins.json, taking build/mcp/projected-state.json
/// with it. On a re-run `load_uninstall_prereqs` finds no managed.json, so
/// `run_mcp_remove_managed` is never called, and ccync's entries stay in the
/// live host forever with no record that could authorize removing them.
#[test]
fn run_uninstall_partial_mcp_failure_retains_ownership_record_for_retry() {
    let _env = env_guard();
    let fake_home = TempDir::new().unwrap();
    let _env_restore = set_home(fake_home.path());

    let ccync_home = fake_home.path().join(".ccync");
    let mcp_dir = ccync_home.join("build").join("mcp");
    fs::create_dir_all(&mcp_dir).unwrap();
    fs::write(
        ccync_home.join("config.json"),
        r#"{"selectedProjectionTargets":[]}"#,
    )
    .unwrap();
    fs::write(ccync_home.join("plugins.json"), r#"{"plugins":[]}"#).unwrap();
    fs::write(mcp_dir.join("managed.json"), r#"{"servers":{}}"#).unwrap();
    // ccync committed ownership of `ccync-srv` on copilot.
    fs::write(
        mcp_dir.join("projected-state.json"),
        r#"{"version":1,"hosts":{"copilot":["ccync-srv"]}}"#,
    )
    .unwrap();
    // Derived state under build/ that must still be cleaned up.
    fs::create_dir_all(ccync_home.join("build").join("render")).unwrap();
    fs::write(mcp_dir.join("scratch.json"), "{}").unwrap();

    // Copilot's config is readable but its atomic-write temp sibling path is
    // occupied by a directory, so the removal write fails — the "the host holds
    // a write lock" case.
    let copilot_dir = fake_home.path().join(".copilot");
    fs::create_dir_all(&copilot_dir).unwrap();
    let copilot_cfg = copilot_dir.join("mcp-config.json");
    let original = r#"{"mcpServers":{"ccync-srv":{"command":"npx"}}}"#;
    fs::write(&copilot_cfg, original).unwrap();
    fs::create_dir_all(copilot_dir.join("mcp-config.json.tmp")).unwrap();

    let result = run_uninstall();
    assert!(
        result.is_err(),
        "a failed MCP de-projection is a partial uninstall"
    );

    // The promise: the record that authorizes removal survived.
    assert!(
        mcp_dir.join("projected-state.json").exists(),
        "committed ownership must survive a partial uninstall — it is the only proof \
         that could ever authorize removing ccync's entries from the live host"
    );
    assert!(
        mcp_dir.join("managed.json").exists(),
        "managed.json gates whether the retry calls run_mcp_remove_managed at all"
    );
    // ...and only that: the rest of build/ is still cleaned up.
    assert!(!ccync_home.join("build").join("render").exists());
    assert!(!mcp_dir.join("scratch.json").exists());
    assert_eq!(
        fs::read_to_string(&copilot_cfg).unwrap(),
        original,
        "the entry that could not be removed is untouched"
    );

    // The retry the message promises now actually converges: unblock the host
    // and re-run, exactly as a user would.
    fs::remove_dir(copilot_dir.join("mcp-config.json.tmp")).unwrap();
    run_uninstall().unwrap();

    let copilot_after: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&copilot_cfg).unwrap()).unwrap();
    assert!(
        copilot_after["mcpServers"]["ccync-srv"].is_null(),
        "the retry removed the stranded entry: {copilot_after}"
    );
    let extra: Vec<String> = fs::read_dir(&ccync_home)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n != "config.json" && n != "plugins.json")
        .filter(|n| n != "build" || !build_holds_only_lifecycle_lock(&ccync_home))
        .collect();
    assert!(
        extra.is_empty(),
        "a converged retry leaves only the input files; found: {extra:?}"
    );
}

#[test]
fn tp13_spec_failed_direct_deletion_retains_lock_json_and_rows_until_retry() {
    let _env = env_guard();
    let fake_home = TempDir::new().unwrap();
    let _env_restore = set_home(fake_home.path());
    let h = fake_home.path();
    let ccync = h.join(".ccync");
    let build = ccync.join("build");

    fs::create_dir_all(&build).unwrap();
    fs::write(ccync.join("config.json"), "{}").unwrap();
    fs::write(ccync.join("plugins.json"), r#"{"plugins":[]}"#).unwrap();

    let fn_norm_key = projection::normalize_managed_artifact_path;

    // 1. Setup OpenCode agent and foreign sibling.
    let opencode_dir = h.join(".config").join("opencode").join("agents");
    fs::create_dir_all(&opencode_dir).unwrap();
    let opencode_agent = opencode_dir.join("helper.md");
    fs::write(&opencode_agent, "opencode agent body").unwrap();
    let opencode_foreign = opencode_dir.join("user_agent.md");
    fs::write(&opencode_foreign, "user opencode agent").unwrap();

    // 2. Setup Claude native command and foreign sibling.
    let claude_cmd_dir = h.join(".claude").join("commands");
    fs::create_dir_all(&claude_cmd_dir).unwrap();
    let claude_cmd = claude_cmd_dir.join("audit.md");
    fs::write(&claude_cmd, "claude command body").unwrap();
    let claude_foreign = claude_cmd_dir.join("user_cmd.md");
    fs::write(&claude_foreign, "user claude command").unwrap();

    // 3. Setup Copilot command-derived skill and foreign sibling file/dir.
    let copilot_skill_dir = h.join(".copilot").join("skills").join("cmdskill");
    fs::create_dir_all(&copilot_skill_dir).unwrap();
    let copilot_skill_file = copilot_skill_dir.join("SKILL.md");
    fs::write(&copilot_skill_file, "command-derived skill body").unwrap();
    let copilot_skill_sibling = copilot_skill_dir.join("sibling.txt");
    fs::write(&copilot_skill_sibling, "foreign sibling in skill dir").unwrap();
    let copilot_user_skill = h
        .join(".copilot")
        .join("skills")
        .join("user_skill")
        .join("SKILL.md");
    fs::create_dir_all(copilot_user_skill.parent().unwrap()).unwrap();
    fs::write(&copilot_user_skill, "foreign user skill").unwrap();

    // 4. Setup legacy skill directory and foreign sibling.
    let legacy_skill_dir = h.join(".claude").join("skills").join("legacy_tool");
    fs::create_dir_all(&legacy_skill_dir).unwrap();
    fs::write(legacy_skill_dir.join("SKILL.md"), "legacy skill body").unwrap();
    let legacy_foreign = h
        .join(".claude")
        .join("skills")
        .join("user_tool")
        .join("SKILL.md");
    fs::create_dir_all(legacy_foreign.parent().unwrap()).unwrap();
    fs::write(&legacy_foreign, "foreign user tool").unwrap();

    // 5. Populate lock.json with direct records and legacy records.
    let lock_file = build.join("lock.json");
    fs::write(
        &lock_file,
        serde_json::json!({
            "_ccyncProjection": {
                "directAgentProjectionPaths": [fn_norm_key(&opencode_agent)],
                "directCommandProjectionPaths": [fn_norm_key(&claude_cmd)],
                "directSkillProjectionPaths": [fn_norm_key(&copilot_skill_file)],
                "skillProjectionPaths": [fn_norm_key(&legacy_skill_dir)]
            }
        })
        .to_string(),
    )
    .unwrap();

    // 6. Inject deletion failure on the OpenCode agent.
    #[cfg(windows)]
    let held = {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&opencode_agent)
            .unwrap()
    };
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let parent = opencode_agent.parent().unwrap();
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o555)).unwrap();
    }

    let result = run_uninstall();
    assert!(
        result.is_err(),
        "uninstall with an undeletable file must return error"
    );

    assert!(
        lock_file.is_file(),
        "lock.json must survive when direct deletion fails"
    );
    let surviving_records = projection::direct_records(&lock_file);
    assert!(
        surviving_records
            .iter()
            .any(|r| r.path == Path::new(&fn_norm_key(&opencode_agent))),
        "surviving record for opencode agent must be retained in lock.json: {surviving_records:?}"
    );

    assert!(
        opencode_agent.is_file(),
        "undeletable opencode agent must still be present"
    );
    assert!(
        !claude_cmd.exists(),
        "successful command deletion must remove file"
    );
    assert!(
        !copilot_skill_file.exists(),
        "successful derived skill deletion must remove file"
    );
    assert!(
        !legacy_skill_dir.exists(),
        "successful legacy skill directory deletion must remove dir"
    );

    assert_eq!(
        fs::read_to_string(&opencode_foreign).unwrap(),
        "user opencode agent"
    );
    assert_eq!(
        fs::read_to_string(&claude_foreign).unwrap(),
        "user claude command"
    );
    assert_eq!(
        fs::read_to_string(&copilot_skill_sibling).unwrap(),
        "foreign sibling in skill dir"
    );
    assert_eq!(
        fs::read_to_string(&copilot_user_skill).unwrap(),
        "foreign user skill"
    );
    assert_eq!(
        fs::read_to_string(&legacy_foreign).unwrap(),
        "foreign user tool"
    );

    #[cfg(windows)]
    drop(held);
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let parent = opencode_agent.parent().unwrap();
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let retry_result = run_uninstall();
    assert!(
        retry_result.is_ok(),
        "retry uninstall must succeed: {retry_result:?}"
    );

    assert!(
        !opencode_agent.exists(),
        "retry must delete the remaining direct copy"
    );
    assert!(!lock_file.exists(), "converged uninstall removes lock.json");
    assert!(
        build_holds_only_lifecycle_lock(&ccync),
        "build/ keeps only lifecycle lock"
    );

    assert_eq!(
        fs::read_to_string(&opencode_foreign).unwrap(),
        "user opencode agent"
    );
    assert_eq!(
        fs::read_to_string(&claude_foreign).unwrap(),
        "user claude command"
    );
    assert_eq!(
        fs::read_to_string(&copilot_skill_sibling).unwrap(),
        "foreign sibling in skill dir"
    );
    assert_eq!(
        fs::read_to_string(&copilot_user_skill).unwrap(),
        "foreign user skill"
    );
    assert_eq!(
        fs::read_to_string(&legacy_foreign).unwrap(),
        "foreign user tool"
    );
}
