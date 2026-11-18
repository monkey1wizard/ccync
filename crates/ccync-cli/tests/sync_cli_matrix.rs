//! Real-binary source and target lifecycle matrix.
//!
//! Each case runs the built `ccync` binary against an isolated home. The home
//! holds one import source's native skill, one or two bundled provider caches,
//! and foreign files beside ccync's outputs. A case runs `init`, two `sync`
//! runs, `doctor`, and `uninstall`. Along the way it checks the exact published
//! files, the ownership rows in `lock.json`, the aliases, and the bytes of the
//! source and foreign inputs.
//!
//! Permitted volatile exclusions. The snapshot ignores nothing else.
//! - The root `generatedAt` and `driftDetection.lastResolvedAt` fields in
//!   resolver locks, and root `generatedAt` in target managed records.
//! - Effective and legacy `lifecycle.lock` files, which are transient locks.
//!
//! Classification filters are not exclusions. The published-file, residue, and
//! alias checks skip the source and foreign inputs, the MCP host files, and
//! everything under `.ccync` except `config.json` and `plugins.json`. The
//! repeat-sync snapshot still compares the bytes of all of them, so a sync that
//! rewrites an MCP host file with different content fails the second-sync check.
//!
//! The first-init MCP ownership collision belongs to a follow-up plan. This
//! file seeds no active MCP entry, so no case here claims that collision is
//! handled.

use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf, MAIN_SEPARATOR_STR},
    process::{Command, Output, Stdio},
};

/// The five import sources.
const SOURCES: [&str; 5] = ["claude", "codex", "copilot", "opencode", "agy"];

/// The seven projection targets, in canonical order.
const TARGETS: [&str; 7] = [
    "claude", "codex", "copilot", "opencode", "agy-cli", "agy-ide", "agy-gui",
];

const SHA_EXAMPLE: &str = "0123456789abcdef0123456789abcdef01234567";
const SHA_EXTRA: &str = "89abcdef0123456789abcdef0123456789abcdef";
const LOCK_PATH: &str = ".ccync/build/lock.json";
const LIFECYCLE_LOCK_PATH: &str = if cfg!(windows) {
    "AppData/Local/ccync/state/lifecycle.lock"
} else {
    ".local/state/ccync/lifecycle.lock"
};
/// Committed machine roots relative to the fixture home. Windows uses the
/// APPDATA and LOCALAPPDATA layout, other platforms the XDG defaults.
const STATE_LOCK_PATH: &str = if cfg!(windows) {
    "AppData/Local/ccync/state/build/lock.json"
} else {
    ".local/state/ccync/build/lock.json"
};
const STATE_LAYOUT_PATH: &str = if cfg!(windows) {
    "AppData/Local/ccync/state/layout.json"
} else {
    ".local/state/ccync/layout.json"
};
const STATE_SOURCES_PREFIX: &str = if cfg!(windows) {
    "AppData/Local/ccync/state/sources/"
} else {
    ".local/state/ccync/sources/"
};
const STATE_SOURCES_PATH: &str = if cfg!(windows) {
    "AppData/Local/ccync/state/sources"
} else {
    ".local/state/ccync/sources"
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
const LEGACY_LIFECYCLE_LOCK_PATH: &str = ".ccync/build/lifecycle.lock";
const CATALOG_PATH: &str = ".ccync/plugins.json";
const CONFIG_PATH: &str = ".ccync/config.json";

#[test]
fn sync_reselects_only_one_held_mcp_server() {
    let home = Home::new();
    home.write(CONFIG_PATH, r#"{"selectedProjectionTargets":["codex"]}"#);
    home.write(CATALOG_PATH, r#"{"plugins":[]}"#);
    home.write(
        ".ccync/mcp.json",
        r#"{"mcpServers":{"alpha":{"url":"https://example.com/alpha","hosts":["codex","claude"]},"beta":{"url":"https://example.com/beta","hosts":["codex","claude"]}}}"#,
    );
    let initial = home.run(&["sync", "--yes"]);
    assert!(
        initial.status.success(),
        "{} {}",
        String::from_utf8_lossy(&initial.stdout),
        String::from_utf8_lossy(&initial.stderr)
    );
    home.write(".codex/config.toml", "");
    home.write(".claude.json", r#"{"mcpServers":{}}"#);
    let held = home.run(&["sync", "--yes"]);
    assert!(!held.status.success());
    let before = snapshot_raw(&home);
    let preview = home.run(&[
        "sync",
        "--reselect-mcp",
        "user",
        "alpha",
        "codex",
        "--dry-run",
    ]);
    assert!(
        preview.status.success(),
        "{} {}",
        String::from_utf8_lossy(&preview.stdout),
        String::from_utf8_lossy(&preview.stderr)
    );
    assert_eq!(before, snapshot_raw(&home));
    let claude_before = home.read(".claude.json");
    let applied = home.run(&["sync", "--reselect-mcp", "user", "alpha", "codex"]);
    assert!(
        applied.status.success(),
        "{} {}",
        String::from_utf8_lossy(&applied.stdout),
        String::from_utf8_lossy(&applied.stderr)
    );
    let codex = home.text(".codex/config.toml").unwrap();
    assert!(codex.contains("alpha"), "{codex}");
    assert!(!codex.contains("beta"), "{codex}");
    assert_eq!(claude_before, home.read(".claude.json"));
    let state_path = if cfg!(windows) {
        "AppData/Local/ccync/state/build/mcp/projected-state.json"
    } else {
        ".local/state/ccync/build/mcp/projected-state.json"
    };
    let state: Value = serde_json::from_slice(&home.read(state_path).unwrap()).unwrap();
    let holds = state["holds"].as_array().unwrap();
    for (server, host) in [("beta", "codex"), ("alpha", "claude"), ("beta", "claude")] {
        assert!(
            holds
                .iter()
                .any(|hold| hold["identity"]["server"] == server && hold["host"] == host),
            "{state}"
        );
    }
    assert!(
        !holds
            .iter()
            .any(|hold| hold["identity"]["server"] == "alpha" && hold["host"] == "codex"),
        "{state}"
    );
    let after = snapshot_raw(&home);
    let not_held = home.run(&["sync", "--reselect-mcp", "user", "alpha", "codex"]);
    assert!(!not_held.status.success());
    assert_eq!(after, snapshot_raw(&home));
    let unknown_host = home.run(&["sync", "--reselect-mcp", "user", "beta", "unknown"]);
    assert!(!unknown_host.status.success());
    assert_eq!(after, snapshot_raw(&home));
}

#[test]
fn sync_reselection_rejects_invalid_arguments_without_writes() {
    let home = Home::new();
    home.write(CONFIG_PATH, "{}");
    home.run(&["sync", "--dry-run"]);
    let before = snapshot_raw(&home);
    for args in [
        vec!["sync", "--reselect-mcp", "user", "alpha"],
        vec!["sync", "--reselect-mcp", "user", "alpha", "codex", "extra"],
        vec![
            "sync",
            "--reselect-mcp",
            "user",
            "alpha",
            "codex",
            "--reselect-mcp",
            "user",
            "beta",
            "claude",
        ],
        vec![
            "sync",
            "--reselect-mcp",
            "user",
            "alpha",
            "codex",
            "--instructions-only",
        ],
    ] {
        let result = home.run(&args);
        assert_eq!(result.status.code(), Some(64), "{args:?}");
        assert_eq!(before, snapshot_raw(&home), "{args:?}");
    }
}

/// Keys stamped with the current time on every run. Permitted volatile exclusion.

/// Host configuration files that sync writes outside the direct layout: MCP host
/// files, and the VS Code setting the copilot target writes. The published-file
/// and residue checks skip them. The repeat-sync snapshot still compares their
/// bytes.
const HOST_CONFIG_FILES: [&str; 6] = [
    ".claude.json",
    ".codex/config.toml",
    ".copilot/mcp-config.json",
    ".gemini/config/mcp_config.json",
    if cfg!(windows) {
        "AppData/Roaming/opencode/opencode.json"
    } else {
        ".config/opencode/opencode.json"
    },
    if cfg!(windows) {
        "AppData/Roaming/Code/User/settings.json"
    } else {
        ".config/Code/User/settings.json"
    },
];

/// Files a user owns beside ccync's outputs. Every step must leave them unchanged.
const FOREIGN_FILES: [&str; 10] = [
    ".claude/skills/user-notes/notes.txt",
    ".agents/skills/user-notes/notes.txt",
    ".copilot/skills/user-notes/notes.txt",
    ".config/opencode/skills/user-notes/notes.txt",
    ".gemini/antigravity-cli/skills/user-notes/notes.txt",
    ".gemini/config/skills/user-notes/notes.txt",
    ".claude/agents/user-agent.md",
    ".config/opencode/agents/user-agent.md",
    ".claude/commands/user-cmd.md",
    ".config/opencode/commands/user-cmd.md",
];

/// Where each source keeps its loose skills.
fn source_root(source: &str) -> &'static str {
    match source {
        "claude" => ".claude/skills",
        "codex" => ".agents/skills",
        "copilot" => ".copilot/skills",
        // The engine scans opencode skills under the platform config directory,
        // which is APPDATA on Windows and XDG_CONFIG_HOME elsewhere.
        "opencode" if cfg!(windows) => "AppData/Roaming/opencode/skills",
        "opencode" => ".config/opencode/skills",
        "agy" => ".gemini/antigravity-cli/skills",
        other => panic!("unknown source {other}"),
    }
}

/// The skill root a target publishes into for this selection. Codex and opencode
/// share `~/.agents/skills`. When both are selected, the surface falls back to
/// that common root, so opencode publishes there rather than into its own root.
fn effective_skill_root(target: &str, targets: &[&str]) -> &'static str {
    match target {
        "opencode" if targets.contains(&"codex") => ".agents/skills",
        "codex" if targets.contains(&"opencode") => ".agents/skills",
        _ => skill_root(target),
    }
}

/// The skill root each target publishes into. AGY IDE and AGY GUI share one root.
fn skill_root(target: &str) -> &'static str {
    match target {
        "claude" => ".claude/skills",
        "codex" => ".agents/skills",
        "copilot" => ".copilot/skills",
        "opencode" => ".config/opencode/skills",
        "agy-cli" => ".gemini/antigravity-cli/skills",
        "agy-ide" | "agy-gui" => ".gemini/config/skills",
        other => panic!("unknown target {other}"),
    }
}

/// The root standalone publication writes for a target. Codex, Copilot and
/// opencode share `~/.agents/skills`, and the AGY IDE and GUI share one root.
fn standalone_root(target: &str) -> &'static str {
    match target {
        "claude" => ".claude/skills",
        "codex" | "copilot" | "opencode" => ".agents/skills",
        "agy-cli" => ".gemini/antigravity-cli/skills",
        "agy-ide" | "agy-gui" => ".gemini/config/skills",
        other => panic!("unknown target {other}"),
    }
}

/// The agent file a target publishes for the `example` provider, if any.
fn agent_path(target: &str) -> Option<&'static str> {
    match target {
        "claude" => Some(".claude/agents/helper.md"),
        "codex" => Some(".codex/agents/helper.toml"),
        "copilot" => Some(".copilot/agents/helper.agent.md"),
        "opencode" => Some(".config/opencode/agents/helper.md"),
        "agy-cli" | "agy-gui" => Some(".gemini/config/agents/helper.md"),
        "agy-ide" => None,
        other => panic!("unknown target {other}"),
    }
}

/// The native command file a target publishes for the `example` provider, if any.
fn native_command_path(target: &str) -> Option<&'static str> {
    match target {
        "claude" => Some(".claude/commands/audit.md"),
        "opencode" => Some(".config/opencode/commands/audit.md"),
        _ => None,
    }
}

/// Whether a target publishes the `audit` command as a command-derived skill.
fn derives_command_skill(target: &str) -> bool {
    matches!(target, "codex" | "copilot")
}

/// Outputs a target should hold for the providers it selects. Keys are lowercase
/// relative paths, and each maps to the provider that owns it.
#[derive(Default)]
struct Expected {
    skills: BTreeMap<String, &'static str>,
    /// Standalone copies of the imported skill. They sit in the registry and on
    /// disk but carry no provider attribution.
    imported: BTreeSet<String>,
    agents: BTreeMap<String, &'static str>,
    commands: BTreeMap<String, &'static str>,
}

impl Expected {
    fn new(source: &str, targets: &[&str], providers: &[&'static str]) -> Self {
        let mut expected = Self::default();
        // The imported skill is standalone content. It is published to every
        // selected reader root except the root that already holds it.
        for target in targets {
            let root = standalone_root(target);
            if root != source_root(source) {
                expected
                    .imported
                    .insert(format!("{root}/imported-{source}"));
            }
        }
        for target in targets {
            let root = effective_skill_root(target, targets);
            for provider in providers {
                match *provider {
                    "example" => {
                        expected.skills.insert(format!("{root}/example"), "example");
                        if let Some(agent) = agent_path(target) {
                            expected.agents.insert(agent.to_string(), "example");
                        }
                        if let Some(command) = native_command_path(target) {
                            expected.commands.insert(command.to_string(), "example");
                        }
                        if derives_command_skill(target) {
                            expected.skills.insert(format!("{root}/audit"), "example");
                        }
                    }
                    "extra" => {
                        expected.skills.insert(format!("{root}/extra"), "extra");
                    }
                    other => panic!("unknown provider {other}"),
                }
            }
        }
        expected
    }

    /// Every expected output path with its owning provider.
    fn all(&self) -> impl Iterator<Item = (&String, &&'static str)> {
        self.skills.iter().chain(&self.agents).chain(&self.commands)
    }

    /// Every file the outputs contain. A skill directory holds its `SKILL.md`.
    fn files(&self) -> BTreeSet<String> {
        let mut files: BTreeSet<String> = self
            .skills
            .keys()
            .map(|dir| format!("{dir}/skill.md"))
            .collect();
        for dir in &self.imported {
            files.insert(format!("{dir}/skill.md"));
            files.insert(format!("{dir}/notes.txt"));
        }
        files.extend(self.agents.keys().cloned());
        files.extend(self.commands.keys().cloned());
        files
    }
}

/// A home directory that every command receives as its only user root.
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

    fn path(&self, relative: &str) -> PathBuf {
        let relative = if self.root.join(STATE_LAYOUT_PATH).is_file() {
            match relative {
                LOCK_PATH => STATE_LOCK_PATH,
                CONFIG_PATH => MACHINE_CONFIG_PATH,
                CATALOG_PATH => MACHINE_CATALOG_PATH,
                _ => relative,
            }
        } else {
            relative
        };
        if relative.starts_with(".ccync/cache/") && self.root.join(STATE_LAYOUT_PATH).is_file() {
            return self.root.join(
                relative
                    .replacen(".ccync/cache/", STATE_SOURCES_PREFIX, 1)
                    .replace('/', MAIN_SEPARATOR_STR),
            );
        }
        if relative == ".ccync/cache" && self.root.join(STATE_LAYOUT_PATH).is_file() {
            return self.root.join(STATE_SOURCES_PATH);
        }
        self.root.join(relative.replace('/', MAIN_SEPARATOR_STR))
    }

    /// The environment roots every command receives, all inside the fixture.
    fn roots(&self) -> [(&'static str, PathBuf); 10] {
        [
            ("HOME", self.root.clone()),
            ("USERPROFILE", self.root.clone()),
            ("APPDATA", self.path("AppData/Roaming")),
            ("LOCALAPPDATA", self.path("AppData/Local")),
            ("XDG_CONFIG_HOME", self.path(".config")),
            ("XDG_STATE_HOME", self.path(".local/state")),
            ("XDG_CACHE_HOME", self.path(".cache")),
            ("CCYNC_HOME", self.path(".ccync")),
            ("CODEX_HOME", self.path(".codex")),
            ("CLAUDE_CONFIG_DIR", self.path(".claude")),
        ]
    }

    /// Refuses a path that leaves the fixture. Every mutation goes through this check.
    fn contain(&self, candidate: &Path) -> Result<(), String> {
        if candidate
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(format!(
                "{} contains a parent-directory step",
                candidate.display()
            ));
        }
        if !candidate.starts_with(&self.root) {
            return Err(format!(
                "{} is outside the fixture {}",
                candidate.display(),
                self.root.display()
            ));
        }
        Ok(())
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_trace(args, None)
    }

    fn run_with_trace(&self, args: &[&str], trace: Option<&Path>) -> Output {
        for (key, value) in self.roots() {
            if let Err(message) = self.contain(&value) {
                panic!("refusing `ccync {}` with {key}: {message}", args.join(" "));
            }
        }
        // An empty PATH directory inside the fixture: no client CLI (claude,
        // codex, copilot, opencode, agy) is reachable from the spawned binary,
        // so no real client can write residue into the fixture home.
        let isolated_bin = self.path("isolated-bin");
        fs::create_dir_all(&isolated_bin).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_ccync"));
        command
            .args(args)
            .envs(self.roots())
            .env("PATH", &isolated_bin)
            .env_remove("CCYNC_ASSUME_YES")
            .env_remove("COPILOT_HOME")
            .env_remove("OPENCODE_CONFIG_DIR")
            .env_remove("OPENCODE_CONFIG")
            .env_remove("OPENCODE_CONFIG_CONTENT")
            .stdin(Stdio::null());
        if let Some(trace) = trace {
            command.env("CCYNC_LOCK_TRACE", trace);
        } else {
            command.env_remove("CCYNC_LOCK_TRACE");
        }
        command.output().unwrap()
    }

    fn write(&self, relative: &str, body: &str) {
        let path = self.path(relative);
        self.contain(&path).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn remove_tree(&self, relative: &str) {
        let path = self.path(relative);
        self.contain(&path).unwrap();
        if path.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
            fs::remove_dir_all(path).unwrap();
        } else if path.symlink_metadata().is_ok() {
            fs::remove_file(path).unwrap();
        }
    }

    fn read(&self, relative: &str) -> Option<Vec<u8>> {
        fs::read(self.path(relative)).ok()
    }

    fn text(&self, relative: &str) -> Option<String> {
        fs::read_to_string(self.path(relative)).ok()
    }

    /// Updates one JSON file in place, so unrelated keys keep their values.
    fn edit_json(&self, relative: &str, edit: impl FnOnce(&mut Value)) {
        let mut value: Value = serde_json::from_str(&self.text(relative).unwrap()).unwrap();
        edit(&mut value);
        self.write(relative, &value.to_string());
    }

    /// The lockfile as JSON. Its absence is an error for the caller to report.
    fn lock(&self) -> Result<Value, String> {
        let bytes = self
            .read(LOCK_PATH)
            .ok_or_else(|| format!("{LOCK_PATH} is missing"))?;
        serde_json::from_slice(&bytes).map_err(|error| format!("{LOCK_PATH}: {error}"))
    }
}

/// Holds a file open while allowing reads only. A replace or a removal of the
/// file fails with a sharing violation until the handle is dropped. This gives
/// the matrix a deterministic write and delete failure without a product hook.
#[cfg(windows)]
fn hold_for_reads_only(path: &Path) -> std::io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    /// `FILE_SHARE_READ`: other openers may read, but not write or delete.
    const FILE_SHARE_READ: u32 = 0x1;
    fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
}

/// One node in a snapshot of the home.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    File(Vec<u8>),
    Link(String),
}

/// Relative path (forward slashes, original case) to its node.
type Tree = BTreeMap<String, Node>;

fn is_transient_lifecycle_lock(path: &str) -> bool {
    let path = path.to_lowercase();
    path == LIFECYCLE_LOCK_PATH.to_lowercase() || path == LEGACY_LIFECYCLE_LOCK_PATH.to_lowercase()
}

/// Every file and link under the home, minus the transient lifecycle lock.
/// Links are recorded with their targets and not followed.
fn snapshot(home: &Home) -> Tree {
    let mut tree = Tree::new();
    collect(&home.root, &home.root, &mut tree);
    tree.remove(LIFECYCLE_LOCK_PATH);
    tree.remove(LEGACY_LIFECYCLE_LOCK_PATH);
    tree
}

fn snapshot_raw(home: &Home) -> Tree {
    let mut tree = Tree::new();
    collect_raw(&home.root, &home.root, &mut tree);
    tree.remove(LIFECYCLE_LOCK_PATH);
    tree.remove(LEGACY_LIFECYCLE_LOCK_PATH);
    tree
}

fn collect_raw(dir: &Path, root: &Path, tree: &mut Tree) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        let key = relative(root, &path);
        if is_link(&path, &meta) {
            let target = fs::read_link(&path)
                .map(|target| relative_or_raw(root, &target))
                .unwrap_or_default();
            tree.insert(key, Node::Link(target));
        } else if meta.is_dir() {
            collect_raw(&path, root, tree);
        } else if let Ok(bytes) = fs::read(&path) {
            tree.insert(key, Node::File(bytes));
        }
    }
}

fn collect(dir: &Path, root: &Path, tree: &mut Tree) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        let key = relative(root, &path);
        if is_link(&path, &meta) {
            let target = fs::read_link(&path)
                .map(|target| relative_or_raw(root, &target))
                .unwrap_or_default();
            tree.insert(key, Node::Link(target));
        } else if meta.is_dir() {
            collect(&path, root, tree);
        } else if let Ok(bytes) = fs::read(&path) {
            let bytes = normalize(&key, bytes);
            tree.insert(key, Node::File(bytes));
        }
    }
}

fn is_link(path: &Path, meta: &fs::Metadata) -> bool {
    // Junctions report as links through `read_link` but not through `is_symlink`.
    meta.file_type().is_symlink() || fs::read_link(path).is_ok()
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap()
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn relative_or_raw(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(_) => relative(root, path),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

fn normalize(key: &str, bytes: Vec<u8>) -> Vec<u8> {
    let resolver_lock = key == ".agents/ccync/lock.json" || key == STATE_LOCK_PATH;
    let target_record = [
        ".ccync/dist/targets/",
        if cfg!(windows) {
            "AppData/Local/ccync/state/dist/targets/"
        } else {
            ".local/state/ccync/dist/targets/"
        },
    ]
    .iter()
    .any(|prefix| {
        key.strip_prefix(prefix).is_some_and(|rest| {
            rest.strip_suffix("/managed.json")
                .is_some_and(|target| !target.is_empty() && !target.contains('/'))
        })
    });
    if !resolver_lock && !target_record {
        return bytes;
    }
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(mut value) => {
            if let Some(root) = value.as_object_mut() {
                root.remove("generatedAt");
                if resolver_lock {
                    if let Some(drift) = root
                        .get_mut("driftDetection")
                        .and_then(Value::as_object_mut)
                    {
                        drift.remove("lastResolvedAt");
                    }
                }
            }
            serde_json::to_vec(&value).unwrap()
        }
        Err(_) => bytes,
    }
}

#[test]
fn snapshot_exceptions_are_limited_to_resolver_and_target_fields() {
    let original = br#"{"generatedAt":"old","driftDetection":{"lastResolvedAt":"old"},"nested":{"generatedAt":"keep"}}"#;
    let times = br#"{"generatedAt":"new","driftDetection":{"lastResolvedAt":"new"},"nested":{"generatedAt":"keep"}}"#;
    for path in [STATE_LOCK_PATH, ".agents/ccync/lock.json"] {
        assert_eq!(
            normalize(path, original.to_vec()),
            normalize(path, times.to_vec())
        );
        let nested = br#"{"generatedAt":"new","driftDetection":{"lastResolvedAt":"new"},"nested":{"generatedAt":"changed"}}"#;
        assert_ne!(
            normalize(path, original.to_vec()),
            normalize(path, nested.to_vec())
        );
    }
    let target = if cfg!(windows) {
        "AppData/Local/ccync/state/dist/targets/claude/managed.json"
    } else {
        ".local/state/ccync/dist/targets/claude/managed.json"
    };
    assert_eq!(
        normalize(target, br#"{"generatedAt":"old","owner":"a"}"#.to_vec()),
        normalize(target, br#"{"generatedAt":"new","owner":"a"}"#.to_vec())
    );
    assert_ne!(
        normalize(target, br#"{"owner":"a"}"#.to_vec()),
        normalize(target, br#"{"owner":"b"}"#.to_vec())
    );
    assert_ne!(
        normalize(".claude.json", original.to_vec()),
        normalize(".claude.json", times.to_vec())
    );
}

/// The first difference between two snapshots, or `None` when they match.
fn tree_diff(before: &Tree, after: &Tree) -> Option<String> {
    let keys: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    let mut differences = Vec::new();
    for key in keys {
        match (before.get(key), after.get(key)) {
            (Some(a), Some(b)) if a == b => {}
            (Some(_), Some(_)) => differences.push(format!("changed {key}")),
            (Some(_), None) => differences.push(format!("removed {key}")),
            (None, Some(_)) => differences.push(format!("added {key}")),
            (None, None) => {}
        }
    }
    (!differences.is_empty()).then(|| differences.join("; "))
}

/// Lowercase, forward-slash form of an absolute path. The verbatim prefix goes,
/// and a trailing `/skill.md` is reduced to its directory.
fn absolute_key(raw: &str) -> String {
    let text = raw.replace('\\', "/");
    let text = text.strip_prefix("//?/").unwrap_or(&text).to_lowercase();
    match text.strip_suffix("/skill.md") {
        Some(directory) => directory.to_string(),
        None => text,
    }
}

fn home_key(home: &Home) -> String {
    absolute_key(&home.root.to_string_lossy())
}

/// A registry path as a key relative to the home. A path outside the home keeps
/// its absolute form, so a comparison can see it.
fn in_home(key: &str, raw: &str) -> String {
    let absolute = absolute_key(raw);
    match absolute.strip_prefix(&format!("{key}/")) {
        Some(rest) => rest.to_string(),
        None => absolute,
    }
}

/// The paths in one `_ccyncProjection` category, as home-relative keys.
fn registry_set(lock: &Value, category: &str, key: &str) -> BTreeSet<String> {
    lock["_ccyncProjection"][category]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|raw| in_home(key, raw))
        .collect()
}

/// The `sourceAttribution` rows, as home-relative keys.
fn attribution(lock: &Value, key: &str) -> BTreeMap<String, String> {
    let mut rows = BTreeMap::new();
    if let Some(map) = lock["_ccyncProjection"]["sourceAttribution"].as_object() {
        for (raw, value) in map {
            if let Some(provider) = value.as_str() {
                rows.insert(in_home(key, raw), provider.to_string());
            }
        }
    }
    rows
}

/// Every path the lockfile or a target's managed record claims as ccync-owned,
/// as home-relative keys.
fn owned_keys(home: &Home, lock: &Value, key: &str) -> BTreeSet<String> {
    let mut owned = BTreeSet::new();
    if let Some(registry) = lock["_ccyncProjection"].as_object() {
        for value in registry.values() {
            for raw in value
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                owned.insert(in_home(key, raw));
            }
        }
    }
    owned.extend(attribution(lock, key).into_keys());
    if let Ok(entries) = fs::read_dir(home.path(".ccync/dist/targets")) {
        for entry in entries.flatten() {
            let Ok(text) = fs::read_to_string(entry.path().join("managed.json")) else {
                continue;
            };
            let Ok(managed) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            for field in ["installTarget", "shortcutTarget", "projectionRoot"] {
                if let Some(raw) = managed[field].as_str() {
                    owned.insert(in_home(key, raw));
                }
            }
        }
    }
    owned
}

fn ensure(condition: bool, message: impl FnOnce() -> String) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message())
    }
}

fn compare_sets(
    name: &str,
    actual: BTreeSet<String>,
    wanted: BTreeSet<String>,
) -> Result<(), String> {
    let missing: Vec<&String> = wanted.difference(&actual).collect();
    let extra: Vec<&String> = actual.difference(&wanted).collect();
    ensure(missing.is_empty() && extra.is_empty(), || {
        format!("{name}: missing {missing:?}, unexpected {extra:?}")
    })
}

fn transcript(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Collects every failed check of one case, so one failure does not hide the rest.
struct Checks {
    label: String,
    failures: Vec<String>,
}

impl Checks {
    fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            failures: Vec::new(),
        }
    }

    fn check(&mut self, name: &str, result: Result<(), String>) {
        if let Err(message) = result {
            self.failures
                .push(format!("{} | {name}: {message}", self.label));
        }
    }

    fn exit(&mut self, name: &str, output: &Output, expected: i32) {
        let code = output.status.code();
        self.check(
            name,
            ensure(code == Some(expected), || {
                format!("exit {code:?}, expected {expected}\n{}", transcript(output))
            }),
        );
    }

    fn finish(self) -> Vec<String> {
        self.failures
    }
}

/// The skill, agent, and command the `example` provider carries.
fn example_skill(body: &str) -> String {
    format!("---\nname: example\ndescription: Example skill.\n---\n{body}\n")
}

fn catalog_entry(id: &str) -> Value {
    json!({
        "pluginId": id,
        "displayName": id,
        "supportTier": "local",
        "sourceType": "local",
        "license": "proprietary",
        "checksumPolicy": "none",
        "componentMap": {"skills": true, "mcp": false, "agents": true},
        "supportedProviders": ["claude", "codex", "copilot", "opencode", "agy"],
        "installStrategy": "bundled-local",
        "source": format!("~/.ccync/plugins/{id}"),
        "defaultProfiles": [],
        "allowAutoUpdate": false,
        "localOverridePolicy": "always"
    })
}

/// One fixture: a source's native skill, the selected providers, foreign files,
/// and the targets the configuration selects.
struct Fixture {
    home: Home,
    source: &'static str,
    targets: Vec<&'static str>,
    providers: Vec<&'static str>,
    example_body: &'static str,
    /// Bytes of the source and foreign inputs as they were before `init`.
    inputs: BTreeMap<String, Vec<u8>>,
}

impl Fixture {
    fn new(source: &'static str, targets: &[&'static str], providers: &[&'static str]) -> Self {
        let mut fixture = Self {
            home: Home::new(),
            source,
            targets: targets.to_vec(),
            providers: providers.to_vec(),
            example_body: "Body.",
            inputs: BTreeMap::new(),
        };
        let skill_dir = format!("{}/imported-{source}", source_root(source));
        fixture.home.write(
            &format!("{skill_dir}/SKILL.md"),
            &format!(
                "---\nname: imported-{source}\ndescription: Imported skill from {source}.\n---\n# imported from {source}\n"
            ),
        );
        fixture
            .home
            .write(&format!("{skill_dir}/notes.txt"), "resource\n");
        for relative in FOREIGN_FILES {
            fixture
                .home
                .write(relative, &format!("foreign {relative}\n"));
        }
        for provider in providers.iter().copied() {
            fixture.seed_cache(provider);
        }
        fixture.write_catalog();
        fixture.inputs = fixture
            .input_paths()
            .into_iter()
            .map(|relative| {
                let bytes = fixture.home.read(&relative).unwrap();
                (relative, bytes)
            })
            .collect();
        fixture
    }

    /// Source and foreign files, relative to the home.
    fn input_paths(&self) -> Vec<String> {
        let skill_dir = format!("{}/imported-{}", source_root(self.source), self.source);
        let mut paths = vec![
            format!("{skill_dir}/SKILL.md"),
            format!("{skill_dir}/notes.txt"),
        ];
        paths.extend(FOREIGN_FILES.iter().map(|path| path.to_string()));
        paths
    }

    fn seed_cache(&self, provider: &str) {
        match provider {
            "example" => {
                let base = format!(".ccync/cache/example@{SHA_EXAMPLE}");
                self.home.write(
                    &format!("{base}/.claude-plugin/plugin.json"),
                    r#"{"name":"example","version":"1.0.0"}"#,
                );
                self.home.write(
                    &format!("{base}/skills/example/SKILL.md"),
                    &example_skill(self.example_body),
                );
                self.home.write(
                    &format!("{base}/agents/helper.md"),
                    "---\nname: helper\ndescription: Helper agent.\n---\nBody.\n",
                );
                self.home.write(
                    &format!("{base}/commands/audit.md"),
                    "---\ndescription: Audit command.\n---\nInspect $ARGUMENTS\n",
                );
            }
            "extra" => {
                let base = format!(".ccync/cache/extra@{SHA_EXTRA}");
                self.home.write(
                    &format!("{base}/.claude-plugin/plugin.json"),
                    r#"{"name":"extra","version":"1.0.0"}"#,
                );
                self.home.write(
                    &format!("{base}/skills/extra/SKILL.md"),
                    "---\nname: extra\ndescription: Extra skill.\n---\nExtra body.\n",
                );
            }
            other => panic!("unknown provider {other}"),
        }
    }

    /// Writes the catalog for the selected providers, keeping nothing else.
    fn write_catalog(&self) {
        let plugins: Vec<Value> = self.providers.iter().map(|id| catalog_entry(id)).collect();
        self.home
            .write(CATALOG_PATH, &json!({ "plugins": plugins }).to_string());
    }

    /// Changes the selected targets through `init`, the only supported path. Sync
    /// refuses a hand-edited selection, so the harness does not edit config.json.
    fn reinit(&mut self, targets: &[&'static str], checks: &mut Checks) {
        let csv = targets.join(",");
        checks.exit(
            "re-init",
            &self
                .home
                .run(&["init", self.source, "--agents", csv.as_str(), "--yes"]),
            0,
        );
        self.targets = targets.to_vec();
    }

    /// Runs `init`, then two syncs, and checks that the second sync changes nothing.
    fn install_and_sync(&self, checks: &mut Checks) {
        let csv = self.targets.join(",");
        checks.exit(
            "init",
            &self
                .home
                .run(&["init", self.source, "--agents", csv.as_str(), "--yes"]),
            0,
        );
        let first = self.home.run(&["sync", "--yes"]);
        checks.exit("first sync", &first, 0);
        checks.check(
            "unsupported route is not reported as a write failure",
            unsupported_route_check(&first, &self.targets),
        );
        let before = snapshot(&self.home);
        let second = self.home.run(&["sync", "--yes"]);
        checks.exit("second sync", &second, 0);
        checks.check(
            "second sync changes nothing",
            tree_diff(&before, &snapshot(&self.home)).map_or(Ok(()), Err),
        );
    }

    /// Checks the registry rows, the exact published files, the aliases, and
    /// the registry's containment against the expected outputs.
    fn check_projection(&self, checks: &mut Checks) {
        let expected = Expected::new(self.source, &self.targets, &self.providers);
        let lock = match self.home.lock() {
            Ok(lock) => lock,
            Err(message) => {
                checks.check("lock.json readable", Err(message));
                return;
            }
        };
        let key = home_key(&self.home);
        checks.check(
            "registry direct rows and attribution",
            registry_matches(&lock, &key, &expected),
        );
        checks.check("published files exactly", self.files_match(&expected));
        checks.check("no extra alias", alias_check(&self.home, &lock, &key));
        checks.check(
            "registry paths stay inside the fixture",
            inside_fixture(&lock, &key),
        );
    }

    fn files_match(&self, expected: &Expected) -> Result<(), String> {
        let tree = snapshot(&self.home);
        let inputs: BTreeSet<String> = self
            .input_paths()
            .iter()
            .map(|p| p.to_lowercase())
            .collect();
        let host: BTreeSet<String> = HOST_CONFIG_FILES.iter().map(|p| p.to_lowercase()).collect();
        let actual: BTreeSet<String> = tree
            .iter()
            .filter(|(path, node)| {
                matches!(node, Node::File(_))
                    && !path.starts_with(".ccync/")
                    && !is_portable_ccync_path(path)
                    && !is_transient_lifecycle_lock(path)
            })
            .map(|(path, _)| path.to_lowercase())
            .filter(|path| !inputs.contains(path) && !host.contains(path))
            .collect();
        compare_sets("published files", actual, expected.files())
    }

    /// Source and foreign bytes must match their state before `init`.
    fn check_inputs(&self) -> Result<(), String> {
        let mut changed = Vec::new();
        for (relative, before) in &self.inputs {
            if self.home.read(relative).as_deref() != Some(before.as_slice()) {
                changed.push(relative.clone());
            }
        }
        ensure(changed.is_empty(), || {
            format!("changed input bytes: {changed:?}")
        })
    }

    /// Checks the state after `uninstall`: no owned output, no alias, only the
    /// inputs and MCP host files, and only the configuration under `.ccync`.
    fn check_uninstalled(&self, checks: &mut Checks) {
        let expected = Expected::new(self.source, &self.targets, &self.providers);
        let tree = snapshot(&self.home);
        let leftovers: Vec<String> = expected
            .files()
            .into_iter()
            .filter(|file| self.home.path(file).symlink_metadata().is_ok())
            .collect();
        checks.check(
            "uninstall removes every owned output",
            ensure(leftovers.is_empty(), || {
                format!("still present: {leftovers:?}")
            }),
        );

        let links: Vec<&String> = tree
            .iter()
            .filter(|(path, node)| matches!(node, Node::Link(_)) && !path.starts_with(".ccync/"))
            .map(|(path, _)| path)
            .collect();
        checks.check(
            "uninstall leaves no alias",
            ensure(links.is_empty(), || format!("links left: {links:?}")),
        );

        let inputs: BTreeSet<String> = self
            .input_paths()
            .iter()
            .map(|p| p.to_lowercase())
            .collect();
        let host: BTreeSet<String> = HOST_CONFIG_FILES.iter().map(|p| p.to_lowercase()).collect();
        let residue: Vec<&String> = tree
            .iter()
            .filter(|(path, node)| {
                matches!(node, Node::File(_))
                    && !path.starts_with(".ccync/")
                    && !is_portable_ccync_path(path)
                    && !is_transient_lifecycle_lock(path)
            })
            .map(|(path, _)| path)
            .filter(|path| {
                let lower = path.to_lowercase();
                !inputs.contains(&lower) && !host.contains(&lower)
            })
            .collect();
        checks.check(
            "uninstall leaves only inputs and MCP host files",
            ensure(residue.is_empty(), || format!("residue: {residue:?}")),
        );

        let ccync_residue: Vec<&String> = tree
            .keys()
            .filter(|path| path.starts_with(".ccync/"))
            .filter(|path| {
                path.as_str() != CONFIG_PATH
                    && path.as_str() != CATALOG_PATH
                    && path.as_str() != ".ccync/migration.json"
                    && !is_transient_lifecycle_lock(path)
            })
            .collect();
        checks.check(
            "uninstall keeps only configuration under .ccync",
            ensure(ccync_residue.is_empty(), || {
                format!("kept: {ccync_residue:?}")
            }),
        );

        checks.check("source and foreign bytes unchanged", self.check_inputs());
    }

    /// The text of a file, which must contain `needle`.
    fn expect_contains(&self, relative: &str, needle: &str) -> Result<(), String> {
        let text = self.home.text(relative).unwrap_or_default();
        ensure(text.contains(needle), || {
            format!("{relative} does not contain {needle:?}: {text:?}")
        })
    }
}

fn is_portable_ccync_path(path: &str) -> bool {
    path.starts_with("AppData/Local/ccync/")
        || path.starts_with("AppData/Roaming/ccync/")
        || path.starts_with(".local/state/ccync/")
        || path.starts_with(".config/ccync/")
        || path.starts_with(".cache/ccync/")
        || path.starts_with(".agents/ccync/")
}

/// The registry rows for every expected category, and each row's provider.
fn registry_matches(lock: &Value, key: &str, expected: &Expected) -> Result<(), String> {
    compare_sets(
        "directSkillProjectionPaths",
        registry_set(lock, "directSkillProjectionPaths", key),
        expected
            .skills
            .keys()
            .chain(&expected.imported)
            .cloned()
            .collect(),
    )?;
    compare_sets(
        "directAgentProjectionPaths",
        registry_set(lock, "directAgentProjectionPaths", key),
        expected.agents.keys().cloned().collect(),
    )?;
    compare_sets(
        "directCommandProjectionPaths",
        registry_set(lock, "directCommandProjectionPaths", key),
        expected.commands.keys().cloned().collect(),
    )?;
    let rows = attribution(lock, key);
    for (path, provider) in expected.all() {
        let got = rows.get(path).map(String::as_str);
        ensure(got == Some(provider), || {
            format!("{path} is attributed to {got:?}, expected {provider}")
        })?;
    }
    Ok(())
}

/// Every link outside `.ccync` must be a path the lockfile or a managed record owns.
fn alias_check(home: &Home, lock: &Value, key: &str) -> Result<(), String> {
    let owned = owned_keys(home, lock, key);
    let tree = snapshot(home);
    let extra: Vec<String> = tree
        .iter()
        .filter(|(path, node)| matches!(node, Node::Link(_)) && !path.starts_with(".ccync/"))
        .map(|(path, _)| path.to_lowercase())
        .filter(|path| !owned.contains(path))
        .collect();
    ensure(extra.is_empty(), || {
        format!("links outside the registry: {extra:?}")
    })
}

/// Every registry path must sit under the fixture root.
fn inside_fixture(lock: &Value, key: &str) -> Result<(), String> {
    let mut outside = Vec::new();
    if let Some(registry) = lock["_ccyncProjection"].as_object() {
        for value in registry.values() {
            for raw in value
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if !absolute_key(raw).starts_with(&format!("{key}/")) {
                    outside.push(raw.to_string());
                }
            }
        }
    }
    ensure(outside.is_empty(), || {
        format!("outside the fixture: {outside:?}")
    })
}

/// The AGY CLI command route is an approved unsupported output. Init reports
/// that the target has no command-to-skill surface, so the sync route must not
/// say that a write failed.
fn unsupported_route_check(output: &Output, targets: &[&str]) -> Result<(), String> {
    if !targets.contains(&"agy-cli") {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    let masquerading: Vec<&str> = text
        .lines()
        .filter(|line| {
            line.contains("agy-cli Command") && line.contains("fix the reported write error")
        })
        .collect();
    ensure(masquerading.is_empty(), || {
        format!("unsupported agy-cli command reported as a write failure: {masquerading:?}")
    })
}

/// Runs one case from init through uninstall, with every check collected.
fn run_case(
    source: &'static str,
    targets: &[&'static str],
    providers: &[&'static str],
    label: &str,
) -> Vec<String> {
    let fixture = Fixture::new(source, targets, providers);
    let mut checks = Checks::new(label);
    fixture.install_and_sync(&mut checks);
    checks.exit("doctor", &fixture.home.run(&["doctor"]), 0);
    fixture.check_projection(&mut checks);
    checks.exit("uninstall", &fixture.home.run(&["uninstall"]), 0);
    fixture.check_uninstalled(&mut checks);
    checks.finish()
}

fn report(failures: Vec<String>) {
    assert!(
        failures.is_empty(),
        "{} failed check(s):\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn single_target_matrix_covers_all_thirty_five_source_target_pairs() {
    let mut failures = Vec::new();
    for source in SOURCES {
        for target in TARGETS {
            let label = format!("{source} -> {target}");
            failures.extend(run_case(source, &[target], &["example"], &label));
        }
    }
    report(failures);
}

#[test]
fn all_targets_and_shared_consumer_subsets_with_two_providers_converge() {
    let subsets: [(&str, &[&'static str]); 3] = [
        ("all seven targets", &TARGETS[..]),
        ("agy-ide and agy-gui share a root", &["agy-ide", "agy-gui"]),
        ("codex and opencode share a root", &["codex", "opencode"]),
    ];
    let mut failures = Vec::new();
    for source in SOURCES {
        for (name, targets) in subsets {
            let label = format!("{source} / {name}");
            failures.extend(run_case(source, targets, &["example", "extra"], &label));
        }
    }
    report(failures);
}

/// Add, deselect, re-add, upgrade, remove, cache loss, write failure, and
/// delete failure, each followed by its own convergence or retention check.
#[test]
fn lifecycle_steps_converge_and_retain_owned_state_on_failure() {
    let mut fixture = Fixture::new("claude", &TARGETS, &["example"]);
    let mut checks = Checks::new("lifecycle");

    // Add.
    fixture.install_and_sync(&mut checks);
    fixture.check_projection(&mut checks);
    let added = snapshot(&fixture.home);

    // Deselect the source's own target, then re-select it.
    let without_claude: Vec<&'static str> = TARGETS[1..].to_vec();
    fixture.reinit(&without_claude, &mut checks);
    checks.exit("deselect sync", &fixture.home.run(&["sync", "--yes"]), 0);
    fixture.check_projection(&mut checks);
    fixture.reinit(&TARGETS, &mut checks);
    checks.exit("re-select sync", &fixture.home.run(&["sync", "--yes"]), 0);
    fixture.check_projection(&mut checks);
    checks.check(
        "re-add restores the added bytes",
        tree_diff(&added, &snapshot(&fixture.home)).map_or(Ok(()), Err),
    );

    // Upgrade the provider body.
    fixture.example_body = "Upgraded body.";
    fixture.seed_cache("example");
    checks.exit("upgrade sync", &fixture.home.run(&["sync", "--yes"]), 0);
    checks.check(
        "upgrade publishes the new body",
        fixture.expect_contains(".claude/skills/example/SKILL.md", "Upgraded body."),
    );
    fixture.check_projection(&mut checks);

    // Remove the provider from the catalog, then add it back.
    let catalog_before_removal = fixture.home.read(CATALOG_PATH).unwrap();
    fixture.home.edit_json(CATALOG_PATH, |catalog| {
        catalog["plugins"] = json!([]);
    });
    fixture.providers.clear();
    checks.exit("remove sync", &fixture.home.run(&["sync", "--yes"]), 0);
    fixture.check_projection(&mut checks);
    checks.check("removal keeps the input bytes", fixture.check_inputs());
    fixture.home.write(
        CATALOG_PATH,
        &String::from_utf8_lossy(&catalog_before_removal),
    );
    fixture.providers = vec!["example"];
    checks.exit("re-add sync", &fixture.home.run(&["sync", "--yes"]), 0);
    fixture.check_projection(&mut checks);

    // Cache loss: the sync refuses and keeps the owned outputs.
    let before_loss = snapshot(&fixture.home);
    fixture.home.remove_tree(".ccync/cache");
    let loss = fixture.home.run(&["sync", "--yes"]);
    checks.exit("cache-loss sync", &loss, 1);
    checks.check(
        "cache loss names the missing plugin directory",
        ensure(transcript(&loss).contains("missing"), || {
            format!("no missing-directory message: {}", transcript(&loss))
        }),
    );
    let outside_cache = |tree: &Tree| -> Tree {
        tree.iter()
            .filter(|(path, _)| {
                !path.starts_with(".ccync/cache/") && !path.starts_with(STATE_SOURCES_PREFIX)
            })
            .map(|(path, node)| (path.clone(), node.clone()))
            .collect()
    };
    checks.check(
        "cache loss keeps the owned state",
        tree_diff(
            &outside_cache(&before_loss),
            &outside_cache(&snapshot(&fixture.home)),
        )
        .map_or(Ok(()), Err),
    );
    fixture.seed_cache("example");
    checks.exit(
        "cache restored sync",
        &fixture.home.run(&["sync", "--yes"]),
        0,
    );
    fixture.check_projection(&mut checks);

    // Write failure: a read-only owned file cannot be replaced, so the previous
    // bytes survive, and the retry publishes the new body.
    #[cfg(windows)]
    {
        fixture.example_body = "Upgraded again.";
        fixture.seed_cache("example");
        let locked = fixture.home.path(".claude/skills/example/SKILL.md");
        let held = hold_for_reads_only(&locked);
        checks.check(
            "hold the owned file for reads only",
            held.as_ref().map(|_| ()).map_err(|error| error.to_string()),
        );
        checks.exit(
            "write-failure sync",
            &fixture.home.run(&["sync", "--yes"]),
            1,
        );
        checks.check(
            "write failure keeps the previous owned bytes",
            fixture.expect_contains(".claude/skills/example/SKILL.md", "Upgraded body."),
        );
        drop(held);
        checks.exit("write-retry sync", &fixture.home.run(&["sync", "--yes"]), 0);
        checks.check(
            "write retry publishes the new body",
            fixture.expect_contains(".claude/skills/example/SKILL.md", "Upgraded again."),
        );
        fixture.check_projection(&mut checks);
    }

    // Uninstall. On Windows a read-only owned file makes the first attempt fail,
    // so the lock, the row, and the file must survive until the retry.
    #[cfg(windows)]
    {
        let locked = fixture.home.path(".copilot/skills/example/SKILL.md");
        let held = hold_for_reads_only(&locked);
        checks.check(
            "hold the owned file for reads only",
            held.as_ref().map(|_| ()).map_err(|error| error.to_string()),
        );
        checks.exit(
            "delete-failure uninstall",
            &fixture.home.run(&["uninstall"]),
            1,
        );
        checks.check(
            "delete failure keeps the lockfile and the registry row",
            fixture.home.lock().and_then(|lock| {
                let key = home_key(&fixture.home);
                ensure(
                    registry_set(&lock, "directSkillProjectionPaths", &key)
                        .contains(".copilot/skills/example"),
                    || "copilot skill row is gone".to_string(),
                )
            }),
        );
        checks.check(
            "delete failure keeps the locked file",
            ensure(locked.is_file(), || format!("{} is gone", locked.display())),
        );
        checks.check("delete failure keeps foreign bytes", fixture.check_inputs());
        drop(held);
    }
    checks.exit("uninstall", &fixture.home.run(&["uninstall"]), 0);
    fixture.check_uninstalled(&mut checks);

    report(checks.finish());
}

/// The machine source links that sync creates for the all-targets selection.
const SOURCE_LINKS: [&str; 3] = [
    ".copilot/ccync",
    ".gemini/ccync",
    ".gemini/antigravity-cli/ccync",
];

/// Whether `relative` is a symlink or junction. Junctions report as links only
/// through `read_link`, which `is_link` already covers.
fn link_at(home: &Home, relative: &str) -> bool {
    let path = home.path(relative);
    path.symlink_metadata()
        .is_ok_and(|meta| is_link(&path, &meta))
}

/// The `sourceLinkProjectionPaths` rows as home-relative keys. A missing lockfile
/// holds no rows, which is the state after uninstall.
fn recorded_source_links(home: &Home) -> Result<BTreeSet<String>, String> {
    if home.read(LOCK_PATH).is_none() {
        return Ok(BTreeSet::new());
    }
    let lock = home.lock()?;
    Ok(registry_set(
        &lock,
        "sourceLinkProjectionPaths",
        &home_key(home),
    ))
}

/// Sync records each source link and creates it. Uninstall removes every recorded
/// link, leaves no link behind, and forgets the rows.
#[test]
fn source_links_are_recorded_then_removed_by_uninstall() {
    let fixture = Fixture::new("claude", &TARGETS, &["example"]);
    let mut checks = Checks::new("source links");
    fixture.install_and_sync(&mut checks);

    let wanted: BTreeSet<String> = SOURCE_LINKS.iter().map(|p| p.to_string()).collect();
    checks.check(
        "sync records the three source links",
        recorded_source_links(&fixture.home)
            .and_then(|rows| compare_sets("sourceLinkProjectionPaths", rows, wanted)),
    );
    let missing: Vec<&str> = SOURCE_LINKS
        .iter()
        .copied()
        .filter(|path| !link_at(&fixture.home, path))
        .collect();
    checks.check(
        "sync creates the three source links",
        ensure(missing.is_empty(), || format!("not links: {missing:?}")),
    );

    checks.exit("uninstall", &fixture.home.run(&["uninstall"]), 0);
    let kept: Vec<&str> = SOURCE_LINKS
        .iter()
        .copied()
        .filter(|path| fixture.home.path(path).symlink_metadata().is_ok())
        .collect();
    checks.check(
        "uninstall removes the three source links",
        ensure(kept.is_empty(), || format!("still present: {kept:?}")),
    );
    checks.check(
        "uninstall forgets the source link rows",
        recorded_source_links(&fixture.home)
            .and_then(|rows| compare_sets("sourceLinkProjectionPaths", rows, BTreeSet::new())),
    );
    fixture.check_uninstalled(&mut checks);
    report(checks.finish());
}

/// The containment check refuses paths that leave the fixture, including paths
/// that climb out with `..`.
#[test]
fn containment_refuses_paths_outside_the_fixture() {
    let home = Home::new();
    assert!(home.contain(&home.path(".claude/skills")).is_ok());
    assert!(home
        .contain(&home.root.parent().unwrap().join("elsewhere"))
        .is_err());
    assert!(home.contain(&home.path("..").join("escape")).is_err());
    assert!(home.contain(Path::new(r"C:\Windows\System32")).is_err());
}

#[test]
fn portable_sync_dry_run_keeps_legacy_and_older_sublayout_homes_byte_identical() {
    for older_sublayout in [false, true] {
        let home = Home::new();
        let config = if older_sublayout {
            ".ccync/config/config.json"
        } else {
            ".ccync/config.json"
        };
        home.write(config, concat!(r#"{"selectedRunt"#, r#"imes":["claude"]}"#));
        let before = snapshot(&home);

        let output = home.run(&["sync", "--dry-run"]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "dry run failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("Storage migration inventory"), "{stdout}");
        assert_eq!(
            stdout.contains("Pending older sublayout fixup"),
            older_sublayout,
            "unexpected fixup status: {stdout}"
        );
        assert!(
            tree_diff(&before, &snapshot(&home)).is_none(),
            "dry run changed the fixture home"
        );
    }
}

#[test]
fn mutation_authority_gate_blocks_corrupt_layout_before_command_writes() {
    let home = Home::new();
    home.write(".ccync/migration.json", "{");
    let trace = home.path("lock-trace.log");

    let output = home.run_with_trace(&["sync", "--yes"], Some(&trace));

    assert_eq!(
        output.status.code(),
        Some(1),
        "corrupt layout must block sync: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(message.contains("MutationBlocked"), "{message}");
    assert!(home.read(CONFIG_PATH).is_none());
    assert!(home.read(".ccync/plugins.json").is_none());
    assert!(home.read(LOCK_PATH).is_none());
    let trace = fs::read_to_string(trace).unwrap();
    let rendezvous = trace.find("acquire rendezvous").unwrap();
    let state = trace.find("acquire state").unwrap();
    assert!(rendezvous < state, "{trace}");
}

#[test]
fn mutation_authority_gate_commits_fresh_once_and_skips_read_only_lock() {
    let home = Home::new();
    for (index, args) in [
        &["list"][..],
        &["show"],
        &["outdated"],
        &["update", "--check"],
        &["pin", "--list"],
        &["search", "term", "--no-add"],
        &["tool", "discover"],
        &["sync", "--dry-run"],
        &["--help"],
    ]
    .iter()
    .enumerate()
    {
        let trace = home.path(&format!("read-only-trace-{index}.log"));
        let _ = home.run_with_trace(args, Some(&trace));
        assert!(!trace.exists(), "ccync {args:?} acquired a lock");
    }

    let trace = home.path("lock-trace.log");
    let first = home.run_with_trace(&["sync", "--yes"], Some(&trace));
    assert!(first.status.code().is_some());
    let layout = home.read(STATE_LAYOUT_PATH).unwrap();
    let second = home.run_with_trace(&["sync", "--yes"], Some(&trace));
    assert!(second.status.code().is_some());
    assert_eq!(
        home.read(STATE_LAYOUT_PATH).as_deref(),
        Some(layout.as_slice()),
        "a second mutation changed the fresh layout commit"
    );
    let trace = fs::read_to_string(trace).unwrap();
    let acquisitions = trace.matches("acquire rendezvous").count();
    assert_eq!(acquisitions, 2, "{trace}");
    assert_eq!(trace.matches("acquire state").count(), 2, "{trace}");
    for chunk in trace.split("acquire rendezvous").skip(1) {
        let state = chunk.find("acquire state").unwrap();
        let next = chunk.find("acquire rendezvous");
        assert!(next.is_none_or(|next| state < next), "{trace}");
    }
}

#[test]
fn mutation_authority_gate_blocks_committed_legacy_drift_before_writes() {
    let home = Home::new();
    let original = concat!(
        r#"{"selectedRunt"#,
        r#"imes":["claude"],"driftSentinel":"original"}"#
    );
    home.write(CONFIG_PATH, original);
    let initial = home.run(&["sync", "--yes"]);
    assert_eq!(
        initial.status.code(),
        Some(0),
        "initial sync failed: {}",
        String::from_utf8_lossy(&initial.stderr)
    );
    assert!(home.read(STATE_LAYOUT_PATH).is_some());

    fs::write(
        home.root.join(CONFIG_PATH),
        concat!(
            r#"{"selectedRunt"#,
            r#"imes":["claude"],"driftSentinel":"changed"}"#
        ),
    )
    .unwrap();
    let before = snapshot_raw(&home);
    let output = home.run(&["sync", "--yes"]);
    let message = String::from_utf8_lossy(&output.stderr);
    assert_ne!(output.status.code(), Some(0), "{message}");
    assert!(
        message.contains("legacy storage drift blocks mutation"),
        "{message}"
    );
    assert_eq!(
        tree_diff(&before, &snapshot_raw(&home)),
        None,
        "drift gate changed file bytes or links"
    );
}

#[test]
fn mutation_authority_gate_traces_every_mutator_and_self_locking_command() {
    let mutators: &[&[&str]] = &[
        &["sync", "--bad-flag"],
        &["add", "--bad-flag"],
        &["remove", "--bad-flag"],
        &["cleanup", "--bad-flag"],
        &["repair", "--bad-flag"],
        &["refresh", "--bad-flag"],
        &["rollback", "--bad-flag"],
        &["upgrade", "--bad-flag"],
        &["update", "--bad-flag"],
        &["pin", "--bad-flag"],
        &["search", "term"],
        &["roster", "install", "--bad-flag"],
        &["tool", "track", "--bad-flag"],
        &["tool", "untrack", "--bad-flag"],
        &["tool", "refresh", "--bad-flag"],
    ];
    let home = Home::new();
    for (index, args) in mutators.iter().enumerate() {
        let trace = home.path(&format!("trace-{index}.log"));
        let _ = home.run_with_trace(args, Some(&trace));
        let trace = fs::read_to_string(trace).unwrap_or_else(|error| {
            panic!(
                "ccync {} did not trace lock acquisition: {error}",
                args.join(" ")
            )
        });
        assert_lock_order(&trace, 1, args);
    }

    let init_trace = home.path("trace-init.log");
    let init = home.run_with_trace(
        &["init", "claude", "--agents", "claude", "--yes"],
        Some(&init_trace),
    );
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    assert_lock_order(&fs::read_to_string(init_trace).unwrap(), 2, &["init"]);

    let backup_dir = home.path("backup");
    let backup_dir = backup_dir.to_str().unwrap();
    let backup = home.run(&["backup", "--output", backup_dir]);
    assert!(
        backup.status.success(),
        "backup failed: {}",
        String::from_utf8_lossy(&backup.stderr)
    );
    let restore_trace = home.path("trace-restore.log");
    let restore = home.run_with_trace(&["restore", "--from", backup_dir], Some(&restore_trace));
    assert!(
        restore.status.success(),
        "restore failed: {}",
        String::from_utf8_lossy(&restore.stderr)
    );
    assert_lock_order(&fs::read_to_string(restore_trace).unwrap(), 1, &["restore"]);

    let invalid_restore_trace = home.path("trace-invalid-restore.log");
    let before = snapshot_raw(&home);
    let invalid_restore =
        home.run_with_trace(&["restore", "--bad-flag"], Some(&invalid_restore_trace));
    assert!(!invalid_restore.status.success());
    assert!(!invalid_restore_trace.exists());
    assert_eq!(before, snapshot_raw(&home));

    let uninstall_trace = home.path("trace-uninstall.log");
    let uninstall = home.run_with_trace(&["uninstall"], Some(&uninstall_trace));
    assert!(
        uninstall.status.success(),
        "uninstall failed: {}",
        String::from_utf8_lossy(&uninstall.stderr)
    );
    assert_lock_order(
        &fs::read_to_string(uninstall_trace).unwrap(),
        1,
        &["uninstall"],
    );
}

fn assert_lock_order(trace: &str, acquisitions: usize, args: &[&str]) {
    let rendezvous_count = trace.matches("acquire rendezvous").count();
    let state_count = trace.matches("acquire state").count();
    assert_eq!(
        rendezvous_count,
        acquisitions,
        "ccync {} rendezvous acquisitions: {trace}",
        args.join(" ")
    );
    assert_eq!(
        state_count,
        acquisitions,
        "ccync {} state acquisitions: {trace}",
        args.join(" ")
    );
    let mut remainder = trace;
    for _ in 0..acquisitions {
        let rendezvous = remainder.find("acquire rendezvous").unwrap();
        let after_rendezvous = &remainder[rendezvous + "acquire rendezvous".len()..];
        let state = after_rendezvous.find("acquire state").unwrap();
        remainder = &after_rendezvous[state + "acquire state".len()..];
    }
}

#[test]
fn portable_sync_migrates_legacy_data_and_second_sync_is_byte_stable() {
    let home = Home::new();
    let legacy_config = concat!(
        r#"{"selectedRunt"#,
        r#"imes":["claude"],"migrationMarker":"keep-me"}"#
    );
    home.write(CONFIG_PATH, legacy_config);

    let first = home.run(&["sync", "--yes"]);
    assert_eq!(
        first.status.code(),
        Some(0),
        "legacy sync failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(home.text(CONFIG_PATH).as_deref(), Some(legacy_config));
    let migrated_config = home
        .text(MACHINE_CONFIG_PATH)
        .expect("migration must commit the old config to the portable layout");
    assert!(migrated_config.contains("claude"), "{migrated_config}");
    assert!(migrated_config.contains("keep-me"), "{migrated_config}");
    assert!(home.read(STATE_LAYOUT_PATH).is_some());

    let after_first_sync = snapshot(&home);
    let second = home.run(&["sync", "--yes"]);
    assert_eq!(
        second.status.code(),
        Some(0),
        "second sync failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        tree_diff(&after_first_sync, &snapshot(&home)),
        None,
        "a second sync must leave the home byte-stable"
    );
}

#[test]
fn portable_sync_committed_layout_skips_older_sublayout_fixup() {
    let home = Home::new();
    home.write(
        CONFIG_PATH,
        concat!(r#"{"selectedRunt"#, r#"imes":["claude"]}"#),
    );
    let migrated = home.run(&["sync", "--yes"]);
    assert_eq!(migrated.status.code(), Some(0));

    let old_sublayout = ".ccync/config/config.json";
    let sentinel = b"leave committed home sublayout untouched";
    home.write(old_sublayout, std::str::from_utf8(sentinel).unwrap());
    let sync = home.run(&["sync", "--yes"]);

    assert_eq!(
        sync.status.code(),
        Some(0),
        "committed sync failed: {}",
        String::from_utf8_lossy(&sync.stderr)
    );
    assert_eq!(home.read(old_sublayout).as_deref(), Some(&sentinel[..]));
}

#[test]
fn portable_sync_refresh_does_not_migrate_legacy_layout() {
    let home = Home::new();
    let old_config = concat!(r#"{"selectedRunt"#, r#"imes":["claude"]}"#);
    home.write(CONFIG_PATH, old_config);

    let refresh = home.run(&["refresh"]);

    assert_eq!(
        refresh.status.code(),
        Some(0),
        "legacy refresh failed: {}",
        String::from_utf8_lossy(&refresh.stderr)
    );
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&refresh.stdout),
        String::from_utf8_lossy(&refresh.stderr)
    );
    assert!(output.contains("ccync sync"), "{output}");
    assert_eq!(home.text(CONFIG_PATH).as_deref(), Some(old_config));
    assert!(home.read(STATE_LAYOUT_PATH).is_none());
}

#[test]
fn portable_sync_required_blocked_route_fails_after_storage_commit() {
    let home = Home::new();
    home.write(
        CONFIG_PATH,
        concat!(r#"{"selectedRunt"#, r#"imes":["claude"]}"#),
    );
    fs::create_dir_all(home.path(".claude.json")).unwrap();

    let sync = home.run(&["sync", "--yes"]);

    assert_eq!(
        sync.status.code(),
        Some(1),
        "blocked required route must fail sync: {}",
        String::from_utf8_lossy(&sync.stderr)
    );
    assert!(
        home.read(STATE_LAYOUT_PATH).is_some(),
        "portable storage must be committed before projection reports the blocked route"
    );
}

#[test]
fn portable_sync_init_then_sync_commits_fresh_ownership_under_state_root() {
    let home = Home::new();
    let init = home.run(&["init", "claude", "--agents", "claude", "--yes"]);
    assert_eq!(
        init.status.code(),
        Some(0),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    let sync = home.run(&["sync", "--yes"]);
    assert_eq!(
        sync.status.code(),
        Some(0),
        "sync failed: {}",
        String::from_utf8_lossy(&sync.stderr)
    );
    assert!(home.read(STATE_LOCK_PATH).is_some());
    assert!(
        !home.root.join(LOCK_PATH).is_file(),
        "legacy ownership path must be absent"
    );
}

#[test]
fn portable_sync_legacy_init_then_sync_keeps_init_ownership_under_state_root() {
    let home = Home::new();
    home.write(
        CONFIG_PATH,
        concat!(r#"{"selectedRunt"#, r#"imes":["claude"]}"#),
    );
    home.write(
        ".claude/skills/legacy-init/SKILL.md",
        "---\nname: legacy-init\ndescription: legacy init fixture\n---\nbody\n",
    );

    let init = home.run(&["init", "claude", "--agents", "claude", "--yes"]);
    assert_eq!(
        init.status.code(),
        Some(0),
        "legacy init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let state_lock = STATE_LOCK_PATH;
    assert!(
        home.read(state_lock).is_some(),
        "init ownership must be in S"
    );
    assert!(
        !home.root.join(LOCK_PATH).is_file(),
        "legacy ownership path must be absent"
    );

    let sync = home.run(&["sync", "--yes"]);
    assert_eq!(
        sync.status.code(),
        Some(0),
        "sync after legacy init failed: {}",
        String::from_utf8_lossy(&sync.stderr)
    );
    assert!(home.read(state_lock).is_some());
    assert!(!home.root.join(LOCK_PATH).is_file());
}

#[test]
fn portable_sync_demotes_legacy_ownership_without_fingerprint() {
    let home = Home::new();
    home.write(
        LOCK_PATH,
        &json!({
            "_ccyncProjection": {
                "directSkillProjectionPaths": [home.path(".claude/skills/example").to_string_lossy()]
            }
        })
        .to_string(),
    );
    home.write(
        CONFIG_PATH,
        concat!(r#"{"selectedRunt"#, r#"imes":["claude"]}"#),
    );
    home.write(
        ".claude/skills/example/SKILL.md",
        "---\nname: example\ndescription: seeded legacy ownership\n---\nbody\n",
    );

    let sync = home.run(&["sync", "--yes"]);
    assert_eq!(
        sync.status.code(),
        Some(0),
        "sync must migrate and retain unverified legacy ownership: {}",
        String::from_utf8_lossy(&sync.stderr)
    );
    let lock_bytes = home.read(STATE_LOCK_PATH).unwrap_or_else(|| {
        panic!(
            "legacy lock must migrate to committed state; stdout: {}; stderr: {}",
            String::from_utf8_lossy(&sync.stdout),
            String::from_utf8_lossy(&sync.stderr)
        )
    });
    let lock: serde_json::Value = serde_json::from_slice(&lock_bytes).unwrap();
    assert!(
        lock.pointer("/_ccyncProjection/historicalOwnership")
            .is_some(),
        "{lock}"
    );
}

/// The snapshot oracle ignores the named volatile keys and flags a planted alias
/// and a registry edit.
#[test]
fn snapshot_oracle_flags_planted_aliases_and_registry_edits_but_ignores_volatile_keys() {
    let fixture = Fixture::new("claude", &["claude"], &["example"]);
    let mut checks = Checks::new("oracle");
    fixture.install_and_sync(&mut checks);
    report(checks.finish());
    let base = snapshot(&fixture.home);

    // The named volatile keys change on every run and must not count.
    fixture.home.edit_json(LOCK_PATH, |lock| {
        lock["generatedAt"] = json!("2030-01-01T00:00:00Z");
        lock["driftDetection"]["lastResolvedAt"] = json!("2030-01-01T00:00:00Z");
    });
    assert!(
        tree_diff(&base, &snapshot(&fixture.home)).is_none(),
        "volatile keys must be ignored"
    );

    // A planted alias outside the registry must count.
    let link = fixture.home.path(".copilot/extra-alias");
    fixture.home.contain(&link).unwrap();
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    ccync_foundation::platform::create_dir_link(
        &fixture.home.path(".claude/skills/example"),
        &link,
    )
    .unwrap();
    let lock = fixture.home.lock().unwrap();
    let key = home_key(&fixture.home);
    assert!(
        alias_check(&fixture.home, &lock, &key).is_err(),
        "a planted alias must be reported"
    );
    assert!(
        tree_diff(&base, &snapshot(&fixture.home)).is_some(),
        "a planted alias must change the snapshot"
    );
    if cfg!(windows) {
        fs::remove_dir(&link).unwrap();
    } else {
        fs::remove_file(&link).unwrap();
    }

    // A registry edit must change the snapshot.
    fixture.home.edit_json(LOCK_PATH, |lock| {
        lock["_ccyncProjection"]["directSkillProjectionPaths"]
            .as_array_mut()
            .unwrap()
            .pop();
    });
    assert!(
        tree_diff(&base, &snapshot(&fixture.home)).is_some(),
        "a registry edit must change the snapshot"
    );
}

#[test]
fn lifecycle_layout_paths() {
    let fixture = Fixture::new("claude", &["claude"], &[]);
    let home = &fixture.home;
    let portable = home.path(".agents/ccync");
    let config = if cfg!(windows) {
        home.path("AppData/Roaming/ccync")
    } else {
        home.path(".config/ccync")
    };
    let state = if cfg!(windows) {
        home.path("AppData/Local/ccync/state")
    } else {
        home.path(".local/state/ccync")
    };
    let cache = if cfg!(windows) {
        home.path("AppData/Local/ccync/cache")
    } else {
        home.path(".cache/ccync")
    };
    for root in [&portable, &config, &state, &cache] {
        fs::create_dir_all(root).unwrap();
    }
    let commit = json!({
        "layoutVersion": 1,
        "roots": [
            {"name": "portable", "path": portable},
            {"name": "config", "path": config},
            {"name": "state", "path": state},
            {"name": "cache", "path": cache}
        ]
    });
    let commit_bytes = serde_json::to_vec(&commit).unwrap();
    fs::write(home.path(".ccync/migration.json"), &commit_bytes).unwrap();
    fs::write(state.join("layout.json"), &commit_bytes).unwrap();
    fs::write(
        config.join("plugins.json"),
        json!({"plugins": []}).to_string(),
    )
    .unwrap();
    fs::write(
        config.join("config.json"),
        json!({"selectedProjectionTargets": ["claude"]}).to_string(),
    )
    .unwrap();

    let legacy_config = home.root.join(".ccync/config.json");
    let legacy_lock = home.root.join(".ccync/build/lock.json");
    fs::create_dir_all(legacy_lock.parent().unwrap()).unwrap();
    fs::write(
        &legacy_config,
        br#"{"selectedProjectionTargets":["claude"],"legacySentinel":"keep-me"}"#,
    )
    .unwrap();
    fs::write(&legacy_lock, b"legacy lock sentinel").unwrap();

    fn legacy_files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn visit(dir: &Path, root: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.is_dir() {
                    visit(&path, root, files);
                } else {
                    let relative = path.strip_prefix(root).unwrap().to_path_buf();
                    // Host-registered live surfaces stay in ~/.ccync, so
                    // only private state is compared here.
                    let live_surface = [
                        "plugins",
                        "source",
                        "build/render",
                        "build/active",
                        "data",
                        "install-receipts",
                    ]
                    .iter()
                    .any(|surface| relative.starts_with(surface));
                    if relative != Path::new("build/lifecycle.lock")
                        && relative != Path::new("config.json")
                        && !live_surface
                    {
                        files.insert(relative, fs::read(path).unwrap());
                    }
                }
            }
        }

        let mut files = BTreeMap::new();
        if root.exists() {
            visit(root, root, &mut files);
        }
        files
    }

    let legacy_root = home.path(".ccync");
    let legacy_before = legacy_files(&legacy_root);

    let init = home.run(&["init", "claude", "--agents", "claude", "--yes"]);
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let sync = home.run(&["sync", "--yes"]);
    assert!(
        sync.status.success(),
        "sync failed: {}",
        String::from_utf8_lossy(&sync.stderr)
    );
    assert!(
        state.join("build/lock.json").is_file(),
        "committed projection lock was not written"
    );
    assert!(config.join("config.json").is_file());
    assert!(portable.is_dir());
    assert!(
        fs::read_dir(&cache).unwrap().next().is_none(),
        "committed lifecycle fixture must not write private state under cache"
    );
    let preserved_config: serde_json::Value =
        serde_json::from_slice(&fs::read(&legacy_config).unwrap()).unwrap();
    assert_eq!(preserved_config["legacySentinel"], "keep-me");
    assert!(preserved_config["selectedProjectionTargets"]
        .as_array()
        .is_some_and(|targets| targets.iter().any(|target| target == "claude")));
    assert_eq!(fs::read(&legacy_lock).unwrap(), b"legacy lock sentinel");
    let legacy_after = legacy_files(&legacy_root);
    assert_eq!(
        legacy_after.keys().collect::<Vec<_>>(),
        legacy_before.keys().collect::<Vec<_>>(),
        "init and sync must create no files under legacy ~/.ccync except the transient lifecycle lock"
    );
    for (path, bytes) in &legacy_before {
        assert_eq!(
            legacy_after.get(path),
            Some(bytes),
            "init and sync changed legacy ~/.ccync/{}",
            path.display()
        );
    }
}
