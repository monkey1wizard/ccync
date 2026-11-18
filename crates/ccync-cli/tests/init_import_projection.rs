//! End-to-end probes for `ccync init <source>` followed by `ccync sync`.
//!
//! Each probe runs the real `ccync` binary against a temporary home. A loose
//! skill imported from the source must reach a direct target root, the
//! adoption record must name the skill's real directory, a re-init must not
//! import ccync's own published copies, and `config.json` must be the last
//! init write.

use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

/// Committed machine roots relative to the fixture home. Windows uses the
/// APPDATA and LOCALAPPDATA layout, other platforms the XDG defaults.
const STATE_MCP_MANAGED_PATH: &str = if cfg!(windows) {
    "AppData/Local/ccync/state/build/mcp/managed.json"
} else {
    ".local/state/ccync/build/mcp/managed.json"
};
const STATE_LAYOUT_PATH: &str = if cfg!(windows) {
    "AppData/Local/ccync/state/layout.json"
} else {
    ".local/state/ccync/layout.json"
};
const STATE_LOCK_PATH: &str = if cfg!(windows) {
    "AppData/Local/ccync/state/build/lock.json"
} else {
    ".local/state/ccync/build/lock.json"
};
const MACHINE_CONFIG_PATH: &str = if cfg!(windows) {
    "AppData/Roaming/ccync/config.json"
} else {
    ".config/ccync/config.json"
};
const MACHINE_CATALOG_PATH: &str = if cfg!(windows) {
    "AppData/Roaming/ccync/plugins.json"
} else {
    ".config/ccync/plugins.json"
};

struct Home {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        // tempfile may return an 8.3 path on Windows; the CLI canonicalizes
        // discovered paths, so build expectations from the same root.
        let root = dir.path().canonicalize().unwrap();
        Self { _dir: dir, root }
    }

    fn ccync_dir(&self) -> PathBuf {
        self.root.join(".ccync")
    }

    fn committed(&self) -> bool {
        self.root.join(STATE_LAYOUT_PATH).is_file()
    }

    fn config_path(&self) -> PathBuf {
        if self.committed() {
            self.root.join(MACHINE_CONFIG_PATH)
        } else {
            self.root.join(".ccync/config.json")
        }
    }

    fn catalog_path(&self) -> PathBuf {
        if self.committed() {
            self.root.join(MACHINE_CATALOG_PATH)
        } else {
            self.root.join(".ccync/plugins.json")
        }
    }

    fn lock_path(&self) -> PathBuf {
        if self.committed() {
            self.root.join(STATE_LOCK_PATH)
        } else {
            self.root.join(".ccync/build/lock.json")
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ccync"))
            .args(args)
            .env("HOME", &self.root)
            .env("USERPROFILE", &self.root)
            .env("APPDATA", self.root.join("AppData").join("Roaming"))
            .env("LOCALAPPDATA", self.root.join("AppData").join("Local"))
            .env("XDG_CONFIG_HOME", self.root.join(".config"))
            .env("XDG_STATE_HOME", self.root.join(".local").join("state"))
            .env("XDG_CACHE_HOME", self.root.join(".cache"))
            .env("CCYNC_HOME", self.ccync_dir())
            .env("CODEX_HOME", self.root.join(".codex"))
            .env("CLAUDE_CONFIG_DIR", self.root.join(".claude"))
            .env_remove("CCYNC_ASSUME_YES")
            .env_remove("COPILOT_HOME")
            .env_remove("OPENCODE_CONFIG_DIR")
            .env_remove("OPENCODE_CONFIG")
            .env_remove("OPENCODE_CONFIG_CONTENT")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn run_ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "`ccync {}` failed: stdout: {}\nstderr: {}",
            args.join(" "),
            stdout(&output),
            stderr(&output)
        );
        output
    }

    /// Write a loose skill directory with a `SKILL.md` and one resource.
    fn write_skill(&self, root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: probe skill {name}\n---\n# {name}\n"),
        )
        .unwrap();
        fs::write(dir.join("notes.txt"), "resource\n").unwrap();
        dir
    }

    fn json(&self, path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    /// The `_adoption.records` rows in `plugins.json`.
    fn records(&self) -> Vec<Value> {
        self.json(&self.catalog_path())["_adoption"]["records"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Strip the Windows verbatim prefix so two spellings of one path compare.
fn plain(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text).to_string())
}

/// Assert that `name` is recorded in `plugins.json` with a path origin equal
/// to `expected`, not with the source agent's name.
fn assert_path_origin(home: &Home, source: &str, name: &str, expected: &Path) {
    let records = home.records();
    let record = records
        .iter()
        .find(|r| r["name"] == name && r["kind"] == "skill")
        .unwrap_or_else(|| panic!("no skill record for {name}: {records:?}"));
    assert_eq!(record["source"], source);
    let origin = record["origin"].as_str().expect("origin must be recorded");
    assert_ne!(origin, source, "origin must not be the agent name");
    assert_eq!(
        plain(&fs::canonicalize(origin).unwrap()),
        plain(&fs::canonicalize(expected).unwrap()),
        "origin must be the skill's own directory"
    );
}

/// The skill directory under a direct root, readable as a published copy.
fn assert_published(root: &Path, name: &str) {
    let skill = root.join(name).join("SKILL.md");
    let body = fs::read_to_string(&skill)
        .unwrap_or_else(|e| panic!("{} must be published: {e}", skill.display()));
    assert!(body.contains(&format!("# {name}")), "{body}");
    assert_eq!(
        fs::read_to_string(root.join(name).join("notes.txt")).unwrap(),
        "resource\n",
        "the skill's resources travel with it"
    );
}

/// The adopted rows (`_adoptedItems`) in the lock, by name.
fn adopted_names(home: &Home) -> Vec<String> {
    home.json(&home.lock_path())["_adoptedItems"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| row["name"].as_str().map(str::to_string))
        .collect()
}

#[test]
fn init_import_claude_loose_skill_reaches_a_direct_root_after_sync() {
    let home = Home::new();
    let origin = home.write_skill(&home.root.join(".claude").join("skills"), "hello");

    home.run_ok(&["init", "claude", "--agents", "claude,codex", "--yes"]);
    assert_path_origin(&home, "claude", "hello", &origin);

    let loose = home.json(&home.lock_path())["_looseSkills"].clone();
    let row = loose
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["name"] == "hello"))
        .unwrap_or_else(|| panic!("hello must be a loose skill: {loose}"));
    assert!(
        row["origin"].as_str().is_some(),
        "the loose skill must carry its origin: {row}"
    );

    let sync = home.run_ok(&["sync", "--yes"]);
    assert!(
        !stderr(&sync).contains("has no available recorded origin"),
        "sync must find the recorded origin: {}",
        stderr(&sync)
    );
    // Codex reads the shared ~/.agents/skills root.
    assert_published(&home.root.join(".agents").join("skills"), "hello");
}

#[test]
fn init_import_agy_loose_skill_reaches_a_direct_root_after_sync() {
    let home = Home::new();
    let origin = home.write_skill(
        &home
            .root
            .join(".gemini")
            .join("antigravity-cli")
            .join("skills"),
        "agy-hello",
    );

    home.run_ok(&["init", "agy", "--agents", "codex,agy", "--yes"]);
    assert_path_origin(&home, "agy", "agy-hello", &origin);

    let sync = home.run_ok(&["sync", "--yes"]);
    assert!(
        !stderr(&sync).contains("has no available recorded origin"),
        "sync must find the recorded origin: {}",
        stderr(&sync)
    );
    // Codex reads the shared ~/.agents/skills root.
    assert_published(&home.root.join(".agents").join("skills"), "agy-hello");
}

#[test]
fn init_import_reinit_does_not_adopt_ccync_published_copies() {
    let home = Home::new();
    home.write_skill(&home.root.join(".claude").join("skills"), "hello");
    home.run_ok(&["init", "claude", "--agents", "claude,codex", "--yes"]);
    home.run_ok(&["sync", "--yes"]);
    let published = home.root.join(".agents").join("skills").join("hello");
    assert!(published.join("SKILL.md").is_file(), "precondition");

    // Codex imports from ~/.agents/skills, where ccync published its copy.
    let reinit = home.run_ok(&["init", "codex", "--agents", "claude,codex", "--yes"]);
    let out = stdout(&reinit);
    assert!(
        out.contains("excluded (ccync-owned copy, not imported): hello"),
        "the preview must report the exclusion: {out}"
    );
    assert!(
        out.contains("Excluded ccync-owned skill copies (not imported): hello"),
        "the adoption summary must report the exclusion: {out}"
    );
    assert!(
        !home
            .records()
            .iter()
            .any(|r| r["source"] == "codex" && r["name"] == "hello"),
        "a ccync-owned copy must never become a codex adoption record: {:?}",
        home.records()
    );
    let loose = home.json(&home.lock_path())["_looseSkills"].clone();
    assert!(
        !loose
            .as_array()
            .into_iter()
            .flatten()
            .any(|row| row["origin"]
                .as_str()
                .is_some_and(|origin| plain(Path::new(origin)) == plain(&published))),
        "the published copy must not become a loose-skill origin: {loose}"
    );
}

#[test]
fn init_import_config_json_is_the_last_init_write() {
    // Ordering on success: config.json is written no earlier than plugins.json.
    let home = Home::new();
    home.write_skill(&home.root.join(".claude").join("skills"), "hello");
    home.run_ok(&["init", "claude", "--agents", "claude", "--yes"]);
    let modified = |path: PathBuf| fs::metadata(path).unwrap().modified().unwrap();
    assert!(
        modified(home.config_path()) >= modified(home.catalog_path()),
        "config.json must be written after plugins.json"
    );

    // Ordering on failure: when config.json cannot be written, every other
    // adoption write has already landed, so the lock holds the adopted rows,
    // the loose skill with its origin, and the recorded source.
    let blocked = Home::new();
    blocked.write_skill(&blocked.root.join(".claude").join("skills"), "hello");
    fs::create_dir_all(blocked.root.join(MACHINE_CONFIG_PATH)).unwrap();
    let failed = blocked.run(&["init", "claude", "--agents", "claude", "--yes"]);
    assert_eq!(failed.status.code(), Some(1), "stderr: {}", stderr(&failed));
    assert!(
        stderr(&failed).contains("could not write the target selection to config.json"),
        "{}",
        stderr(&failed)
    );
    assert!(!blocked.config_path().is_file());
    let lock = blocked.json(&blocked.lock_path());
    assert_eq!(lock["_adoptMaster"], "claude", "{lock}");
    assert!(adopted_names(&blocked).contains(&"hello".to_string()));
    assert!(
        lock["_looseSkills"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|row| row["name"] == "hello" && row["origin"].is_string()),
        "{lock}"
    );
}

#[test]
fn init_import_copilot_mcp_server_reaches_managed_state_after_sync() {
    let home = Home::new();
    let copilot = home.root.join(".copilot");
    fs::create_dir_all(&copilot).unwrap();
    fs::write(
        copilot.join("mcp-config.json"),
        r#"{"mcpServers":{"copilot-fetch":{"type":"local","command":"uvx","args":["mcp-server-fetch"],"tools":["*"]}}}"#,
    )
    .unwrap();

    // The init-time projection meets the source's own same-name live entry
    // before ccync owns it, so its exit status is not asserted here; the next
    // sync claims the identical entry and must succeed.
    let init = home.run(&["init", "copilot", "--agents", "claude", "--yes"]);
    assert!(
        stdout(&init).contains("copilot-fetch"),
        "init must import the Copilot MCP server: {}
{}",
        stdout(&init),
        stderr(&init)
    );
    assert!(home.catalog_path().is_file() && home.config_path().is_file());
    home.run_ok(&["sync", "--yes"]);

    let managed = home.json(&home.root.join(STATE_MCP_MANAGED_PATH));
    assert!(
        managed.to_string().contains("copilot-fetch"),
        "the Copilot MCP server must reach the managed MCP state: {managed}"
    );
}

/// The lines that describe the restricted MCP server, trimmed for comparison.
fn restricted_lines(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|line| line.starts_with("restricted:"))
        .map(str::to_string)
        .collect()
}

#[test]
fn init_import_preview_and_completion_agree_on_mcp_diagnostics_without_values() {
    let home = Home::new();
    let skill_origin = home.write_skill(&home.root.join(".claude").join("skills"), "hello");
    fs::write(
        home.root.join(".claude.json"),
        // No plain server here: a live same-name entry collides with its managed
        // copy, which is the separate source-host MCP plan, not this probe.
        r#"{"mcpServers":{"disabled":{"command":"node"},"restricted":{"command":"node","allowedTools":["read"],"env":{"API_KEY":"probe-secret-value"}}},"disabledMcpServers":["disabled"]}"#,
    )
    .unwrap();

    let preview = home.run(&["init", "claude", "--agents", "claude"]);
    let preview_out = stdout(&preview);
    assert_eq!(
        preview.status.code(),
        Some(64),
        "stderr: {}",
        stderr(&preview)
    );
    assert!(!home.ccync_dir().exists(), "the preview must write nothing");
    assert!(
        preview_out.contains("skill: hello"),
        "preview must include unrelated valid skill: {preview_out}"
    );

    let completion = home.run(&["init", "claude", "--agents", "claude", "--yes"]);
    assert!(
        completion.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&completion),
        stderr(&completion)
    );
    let completion_out = stdout(&completion);

    for out in [&preview_out, &completion_out] {
        assert!(
            out.contains("mcp blocked: 1 server(s), not imported; mcp disabled: 1 server(s)"),
            "{out}"
        );
        assert!(out.contains("disabled by the source: disabled"), "{out}");
        assert!(!out.contains("probe-secret-value"), "{out}");
    }
    // The `--yes` run prints its own preview before the completion summary, so
    // the completion block is the text after the adoption summary begins.
    let (_, completion_summary) = completion_out
        .split_once("Imported ")
        .expect("completion must print the adoption summary");
    let preview_restricted = restricted_lines(&preview_out);
    assert_eq!(preview_restricted.len(), 1, "{preview_out}");
    assert_eq!(
        restricted_lines(completion_summary),
        preview_restricted,
        "completion must name the same blocked server as the preview: {completion_out}"
    );
    assert_path_origin(&home, "claude", "hello", &skill_origin);
}

#[test]
fn init_import_preview_reports_loose_conflict_and_legacy_rescan_without_writing() {
    let home = Home::new();
    home.write_skill(&home.root.join(".claude").join("skills"), "hello");
    fs::create_dir_all(home.lock_path().parent().unwrap()).unwrap();
    fs::write(
        home.lock_path(),
        r#"{"_looseSkills":[{"name":"hello","source":"claude","origin":"/elsewhere/hello"}]}"#,
    )
    .unwrap();
    fs::write(
        home.catalog_path(),
        r#"{"_adoption":{"records":[{"source":"claude","kind":"skill","componentId":"hello","name":"hello"}],"targetSnapshot":["claude"]}}"#,
    )
    .unwrap();
    let before = [home.lock_path(), home.catalog_path()].map(|path| fs::read(&path).unwrap());

    let preview = home.run(&["init", "claude", "--agents", "claude"]);
    let out = stdout(&preview);
    assert_eq!(
        preview.status.code(),
        Some(64),
        "stderr: {}",
        stderr(&preview)
    );
    assert!(
        out.contains("loose conflict: hello has 2 origins:"),
        "{out}"
    );
    assert!(out.contains("/elsewhere/hello"), "{out}");
    assert!(
        out.contains("rescan required: hello (skill) has no recorded origin"),
        "{out}"
    );
    assert_eq!(
        [home.lock_path(), home.catalog_path()].map(|path| fs::read(&path).unwrap()),
        before,
        "the preview must leave the lock and the catalog unchanged"
    );
}

#[test]
fn source_add_persistence() {
    fn source(home: &Home, name: &str) -> PathBuf {
        let root = home.root.join(name);
        fs::create_dir_all(root.join("skills")).unwrap();
        fs::write(root.join("skills").join("demo.md"), "# demo\n").unwrap();
        root
    }

    let home = Home::new();
    home.write_skill(&home.root.join(".claude").join("skills"), "imported");
    home.run_ok(&["init", "claude", "--agents", "claude", "--yes"]);
    let added_source = source(&home, "added-plugin");
    let added_path = added_source.to_string_lossy().into_owned();
    let added = home.run_ok(&["add", &added_path, "--no-sync"]);
    assert!(stdout(&added).contains("added"), "{}", stdout(&added));

    let catalog = home.json(&home.catalog_path());
    assert_eq!(catalog["plugins"].as_array().unwrap().len(), 1);
    assert_eq!(catalog["_adoption"]["records"][0]["name"], "imported");
    let reloaded: Value = serde_json::from_slice(&fs::read(home.catalog_path()).unwrap()).unwrap();
    assert_eq!(reloaded["plugins"].as_array().unwrap().len(), 1);
    assert_eq!(reloaded["_adoption"]["records"][0]["name"], "imported");

    let missing_array = Home::new();
    fs::create_dir_all(missing_array.ccync_dir()).unwrap();
    fs::write(missing_array.catalog_path(), r#"{"private":"kept"}"#).unwrap();
    let missing_source = source(&missing_array, "missing-array-plugin");
    let missing_path = missing_source.to_string_lossy().into_owned();
    missing_array.run_ok(&["add", &missing_path, "--no-sync"]);
    let catalog = missing_array.json(&missing_array.catalog_path());
    assert_eq!(catalog["private"], "kept");
    assert_eq!(catalog["plugins"].as_array().unwrap().len(), 1);

    let wrong_type = Home::new();
    fs::create_dir_all(wrong_type.ccync_dir()).unwrap();
    let original = br#"{"plugins":"invalid","private":"kept"}"#;
    fs::write(wrong_type.catalog_path(), original).unwrap();
    let wrong_source = source(&wrong_type, "wrong-type-plugin");
    let wrong_path = wrong_source.to_string_lossy().into_owned();
    let failed = wrong_type.run(&["add", &wrong_path, "--no-sync"]);
    assert!(!failed.status.success());
    assert!(!stdout(&failed).contains("added"), "{}", stdout(&failed));
    assert_eq!(fs::read(wrong_type.catalog_path()).unwrap(), original);

    #[cfg(windows)]
    {
        let write_failure = Home::new();
        fs::create_dir_all(write_failure.ccync_dir()).unwrap();
        let original = br#"{"plugins":[],"private":"kept"}"#;
        let catalog_path = write_failure.catalog_path();
        fs::write(&catalog_path, original).unwrap();
        let failing_source = source(&write_failure, "write-failure-plugin");
        let failing_path = failing_source.to_string_lossy().into_owned();

        use std::os::windows::fs::OpenOptionsExt;

        // Allow the CLI to read the catalog while denying replacement or
        // deletion of the destination during its atomic write.
        let _catalog_replace_guard = fs::OpenOptions::new()
            .read(true)
            .share_mode(0x0000_0001)
            .open(&catalog_path)
            .unwrap();
        let failed = write_failure.run(&["add", &failing_path, "--no-sync"]);
        assert!(!failed.status.success());
        assert!(!stdout(&failed).contains("added"), "{}", stdout(&failed));
        assert_eq!(fs::read(&catalog_path).unwrap(), original);
    }

    #[cfg(not(windows))]
    {
        let write_failure = Home::new();
        fs::create_dir_all(write_failure.ccync_dir()).unwrap();
        let original = br#"{"plugins":[],"private":"kept"}"#;
        fs::write(write_failure.catalog_path(), original).unwrap();
        // The atomic writer stages through `<catalog>.tmp`, so a directory
        // there makes the temporary write fail.
        let mut temporary_path = write_failure.catalog_path().as_os_str().to_os_string();
        temporary_path.push(".tmp");
        fs::create_dir(PathBuf::from(temporary_path)).unwrap();
        let failing_source = source(&write_failure, "write-failure-plugin");
        let failing_path = failing_source.to_string_lossy().into_owned();
        let failed = write_failure.run(&["add", &failing_path, "--no-sync"]);
        assert!(!failed.status.success());
        assert!(!stdout(&failed).contains("added"), "{}", stdout(&failed));
        assert_eq!(fs::read(write_failure.catalog_path()).unwrap(), original);
    }
}
