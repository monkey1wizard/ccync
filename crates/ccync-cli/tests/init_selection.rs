//! Acceptance probes for `ccync init` consent, exit codes, and write ordering.
//!
//! Each probe runs the real `ccync` binary against a temporary home. Stdin is
//! not a terminal in these runs, so every prompt is answered by a flag or by
//! refusal. The terminal-only cancel paths (Esc at each `dialoguer` prompt) are
//! not reachable here; they are covered by the `prompt_*` unit tests in
//! `init.rs` and by the source-text probe below.

use serde_json::Value;
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output, Stdio},
};

/// Committed machine roots relative to the fixture home. Windows uses the
/// APPDATA layout, other platforms the XDG defaults.
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

    fn config_path(&self) -> PathBuf {
        self.root.join(MACHINE_CONFIG_PATH)
    }

    fn catalog_path(&self) -> PathBuf {
        self.root.join(MACHINE_CATALOG_PATH)
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
            .env_remove("CCYNC_ASSUME_YES")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// The `selectedProjectionTargets` array in `config.json`.
    fn selected_targets(&self) -> Vec<String> {
        let text = fs::read_to_string(self.config_path()).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        value["selectedProjectionTargets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    /// The `_adoption.targetSnapshot` array in `plugins.json`.
    fn snapshot(&self) -> Vec<String> {
        let text = fs::read_to_string(self.catalog_path()).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        value["_adoption"]["targetSnapshot"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn code(output: &Output) -> Option<i32> {
    output.status.code()
}

#[test]
fn init_selection_init_without_consent_prints_preview_writes_nothing_and_exits_64() {
    let home = Home::new();
    let output = home.run(&["init", "claude", "--agents", "claude"]);

    assert_eq!(code(&output), Some(64), "stderr: {}", stderr(&output));
    assert!(
        stdout(&output).contains("preview for source claude"),
        "the preview must print before the consent refusal: {}",
        stdout(&output)
    );
    assert!(stderr(&output).contains("nothing written"));
    assert!(
        !home.ccync_dir().exists(),
        "a refused init must not create ~/.ccync"
    );
}

#[test]
fn init_selection_init_with_empty_agents_list_exits_64_as_missing_target_list() {
    let home = Home::new();
    let output = home.run(&["init", "claude", "--agents", "", "--yes"]);

    assert_eq!(code(&output), Some(64), "stderr: {}", stderr(&output));
    assert!(stderr(&output).contains("missing target list"));
    assert!(!home.ccync_dir().exists());
}

#[test]
fn init_selection_init_without_source_exits_64_when_not_on_a_terminal() {
    let home = Home::new();
    let output = home.run(&["init", "--agents", "claude", "--yes"]);

    assert_eq!(code(&output), Some(64), "stderr: {}", stderr(&output));
    assert!(stderr(&output).contains("missing source"));
    assert!(!home.ccync_dir().exists());
}

#[test]
fn init_selection_init_with_unknown_target_key_exits_64_and_writes_nothing() {
    let home = Home::new();
    let output = home.run(&["init", "claude", "--agents", "frobnicate", "--yes"]);

    assert_eq!(code(&output), Some(64), "stderr: {}", stderr(&output));
    assert!(stderr(&output).contains("unknown target 'frobnicate'"));
    assert!(!home.ccync_dir().exists());
}

#[test]
fn init_selection_consented_init_stores_the_exact_target_list() {
    let home = Home::new();
    let output = home.run(&["init", "claude", "--agents", "claude,codex", "--yes"]);

    assert!(
        output.status.success(),
        "consented init must succeed: stderr: {}",
        stderr(&output)
    );
    assert_eq!(home.selected_targets(), vec!["claude", "codex"]);
    assert_eq!(home.snapshot(), vec!["claude", "codex"]);
}

#[test]
fn init_selection_consented_init_keeps_a_partial_agy_subset() {
    let home = Home::new();
    let output = home.run(&["init", "claude", "--agents", "claude,agy-cli", "--yes"]);

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(home.selected_targets(), vec!["claude", "agy-cli"]);
}

#[test]
fn init_selection_agy_agent_name_expands_to_all_three_surfaces() {
    let home = Home::new();
    let output = home.run(&["init", "claude", "--agents", "agy", "--yes"]);

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(
        home.selected_targets(),
        vec!["agy-cli", "agy-ide", "agy-gui"]
    );
}

#[test]
fn init_selection_write_failure_after_plugins_json_leaves_fresh_machine_uninitialized_and_repeat_converges(
) {
    let home = Home::new();
    // A directory where config.json belongs makes the second write fail after
    // plugins.json has already recorded the snapshot.
    fs::create_dir_all(home.config_path()).unwrap();

    let failed = home.run(&["init", "claude", "--agents", "claude", "--yes"]);
    assert_eq!(code(&failed), Some(1), "stderr: {}", stderr(&failed));
    assert!(stderr(&failed).contains("could not write the target selection to config.json"));
    assert_eq!(home.snapshot(), vec!["claude"]);
    assert!(
        !home.config_path().is_file(),
        "a machine with no config.json stays uninitialized"
    );

    fs::remove_dir(home.config_path()).unwrap();
    let repeat = home.run(&["init", "claude", "--agents", "claude", "--yes"]);
    assert!(
        repeat.status.success(),
        "the repeat must converge: stderr: {}",
        stderr(&repeat)
    );
    assert_eq!(home.selected_targets(), vec!["claude"]);
    assert_eq!(home.snapshot(), vec!["claude"]);
}

#[cfg(windows)]
#[test]
fn init_selection_write_failure_on_reinit_keeps_previous_config_and_sync_refuses_on_snapshot_mismatch(
) {
    let home = Home::new();
    let first = home.run(&["init", "claude", "--agents", "claude", "--yes"]);
    assert!(first.status.success(), "stderr: {}", stderr(&first));

    // On Windows, a rename cannot replace a read-only file, so the config
    // write fails after plugins.json already records the new snapshot.
    let writable = fs::metadata(home.config_path()).unwrap().permissions();
    let mut permissions = writable.clone();
    permissions.set_readonly(true);
    fs::set_permissions(home.config_path(), permissions).unwrap();

    let failed = home.run(&["init", "codex", "--agents", "codex", "--yes"]);
    assert_eq!(code(&failed), Some(1), "stderr: {}", stderr(&failed));
    assert_eq!(
        home.selected_targets(),
        vec!["claude"],
        "the previous config.json must survive the failed write"
    );
    assert_eq!(home.snapshot(), vec!["codex"]);

    let sync = home.run(&["sync", "--yes"]);
    assert_eq!(code(&sync), Some(1), "stderr: {}", stderr(&sync));
    assert!(
        stderr(&sync).contains("changed outside `ccync init`"),
        "sync must refuse on the snapshot mismatch: {}",
        stderr(&sync)
    );

    fs::set_permissions(home.config_path(), writable).unwrap();
}

#[test]
fn init_selection_retired_silent_paths_are_absent_from_the_init_source() {
    // The four legacy silent paths: multi-select cancel, empty selection,
    // best-effort selection write, and the skipped consent gate. Their
    // fallback wording must not return to the init source.
    let source = include_str!("../src/commands/init.rs");
    for retired in [
        ["falling back to ", "master"].concat(),
        ["vec![master.to_string()]"].concat(),
        ["(best-effort): {e}"].concat(),
        ["safe fallback: master"].concat(),
    ] {
        assert!(
            !source.contains(&retired),
            "retired init fallback reappeared: {retired}"
        );
    }
}

#[test]
fn init_selection_preview_names_foreign_alias_and_leaves_the_foreign_root_untouched() {
    let home = Home::new();
    let source = home.root.join(".claude").join("skills").join("hello");
    fs::create_dir_all(&source).unwrap();
    fs::write(
        source.join("SKILL.md"),
        "---\nname: hello\ndescription: probe skill hello\n---\n# hello\n",
    )
    .unwrap();
    // A same-name skill that ccync does not own sits in the codex skill root.
    let foreign = home.root.join(".agents").join("skills").join("hello");
    fs::create_dir_all(&foreign).unwrap();
    fs::write(foreign.join("SKILL.md"), "foreign\n").unwrap();
    let before = fs::read(foreign.join("SKILL.md")).unwrap();

    let preview = home.run(&["init", "claude", "--agents", "codex"]);
    let out = stdout(&preview);
    assert_eq!(code(&preview), Some(64), "stderr: {}", stderr(&preview));
    assert!(
        out.contains("aliased to") && out.contains("is not owned by ccync"),
        "the preview must name the alias and its foreign path: {out}"
    );
    assert_eq!(
        fs::read(foreign.join("SKILL.md")).unwrap(),
        before,
        "the foreign root must stay untouched by the preview"
    );
    assert!(!home.ccync_dir().exists(), "the preview must write nothing");
}
