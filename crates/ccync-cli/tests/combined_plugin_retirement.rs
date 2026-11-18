//! Full-sync probe: an ordinary `ccync sync` retires the four combined
//! plugin roots and leaves the direct copies in place, on the first run and on
//! a repeat run.
//!
//! Each probe runs the real `ccync` binary against a temporary home with all
//! seven projection targets selected and one bundled provider in the lockfile.
//! The legacy combined roots are seeded as ccync-owned, the way a pre-retirement
//! sync left them, so the first run has something to retire.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

const ALL_TARGETS: &str = r#""claude","codex","copilot","opencode","agy-cli","agy-ide","agy-gui""#;

/// The combined Copilot plugin copy, retired by the first sync.
const COPILOT_PLUGIN_ROOT: &str = ".copilot/installed-plugins/ccync-copilot/ccync";
/// The AGY CLI, IDE, and GUI combined plugin roots, retired by the first sync.
const AGY_PLUGIN_ROOTS: [&str; 3] = [
    ".gemini/antigravity-cli/plugins/ccync",
    ".gemini/antigravity-ide/plugins/ccync",
    ".gemini/config/plugins/ccync",
];
/// The lockfile a sync rewrites, relative to the home directory.
const LOCKFILE: &str = ".ccync/build/lock.json";
/// Direct copies that must exist after every sync.
const DIRECT_COPIES: [&str; 4] = [
    ".claude/skills/example/SKILL.md",
    ".copilot/skills/example/SKILL.md",
    ".gemini/antigravity-cli/skills/example/SKILL.md",
    ".gemini/config/skills/example/SKILL.md",
];

/// Committed machine roots relative to the fixture home. Windows uses the
/// APPDATA and LOCALAPPDATA layout, other platforms the XDG defaults.
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
const STATE_BUILD_PREFIX: &str = if cfg!(windows) {
    "AppData/Local/ccync/state/build/"
} else {
    ".local/state/ccync/build/"
};
const STATE_DIST_PREFIX: &str = if cfg!(windows) {
    "AppData/Local/ccync/state/dist/"
} else {
    ".local/state/ccync/dist/"
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

    fn path(&self, relative: &str) -> PathBuf {
        let committed = self.root.join(STATE_LAYOUT_PATH).is_file();
        let mapped = if committed {
            if relative == ".ccync/config.json" {
                MACHINE_CONFIG_PATH.to_owned()
            } else if relative == ".ccync/plugins.json" {
                MACHINE_CATALOG_PATH.to_owned()
            } else if relative.starts_with(".ccync/cache/") {
                relative.replacen(".ccync/cache/", STATE_SOURCES_PREFIX, 1)
            } else if relative.starts_with(".ccync/build/") {
                relative.replacen(".ccync/build/", STATE_BUILD_PREFIX, 1)
            } else if relative.starts_with(".ccync/dist/") {
                relative.replacen(".ccync/dist/", STATE_DIST_PREFIX, 1)
            } else {
                relative.to_owned()
            }
        } else {
            relative.to_owned()
        };
        self.root.join(mapped)
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
}

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

/// The inputs a real sync reads: the machine config with every target
/// selected, a personal catalog entry for one bundled-local provider, and the
/// provider's cache copy under the `<id>@<sha>` name sync resolves.
fn seed_machine(home: &Home) {
    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    let plugin = home.path(&format!(".ccync/cache/example@{SHA}"));
    write(
        &plugin.join(".claude-plugin/plugin.json"),
        r#"{"name":"example","version":"1.0.0"}"#,
    );
    write(
        &plugin.join("skills/example/SKILL.md"),
        "---\nname: example\ndescription: Example skill.\n---\nBody.\n",
    );
    write(
        &plugin.join("agents/helper.md"),
        "---\nname: helper\ndescription: Helper agent.\n---\nBody.\n",
    );
    write(
        &plugin.join("commands/audit.md"),
        "---\ndescription: Audit command.\n---\nInspect $ARGUMENTS\n",
    );
    write(
        &home.path(".ccync/plugins.json"),
        r#"{"plugins":[{"pluginId":"example","displayName":"Example","supportTier":"local","sourceType":"local","license":"proprietary","checksumPolicy":"none","componentMap":{"skills":true,"mcp":false,"agents":true},"supportedProviders":["claude","codex","copilot","opencode","agy"],"installStrategy":"bundled-local","source":"~/.ccync/plugins/example","defaultProfiles":[],"allowAutoUpdate":false,"localOverridePolicy":"always"}]}"#,
    );
    write(
        &home.path(".ccync/config.json"),
        &format!(r#"{{"selectedProjectionTargets":[{ALL_TARGETS}]}}"#),
    );
}

/// Leave the combined roots a pre-retirement sync would have written: ccync
/// owns each one, so the first sync may retire it.
fn seed_legacy_combined_roots(home: &Home) {
    let render = home.path(".ccync/build/render");
    fs::create_dir_all(&render).unwrap();

    let lockfile = home.path(".ccync/build/lock.json");
    let ownership = projection::AgyRootOwnership::from_lockfile(&lockfile);
    for relative in AGY_PLUGIN_ROOTS {
        let link = home.path(relative);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        ccync_foundation::platform::create_dir_link(&render, &link).unwrap();
        ownership.record_now(&link, false).unwrap();
    }

    let copilot_root = home.path(COPILOT_PLUGIN_ROOT);
    fs::create_dir_all(copilot_root.parent().unwrap()).unwrap();
    ccync_foundation::platform::create_dir_link(&render, &copilot_root).unwrap();
    write(
        &home.path(".ccync/dist/targets/copilot/managed.json"),
        &serde_json::json!({ "projectionRoot": copilot_root }).to_string(),
    );
}

/// Whether anything, a link included, sits at `path`.
fn present(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

/// Assert AGY roots without current ownership remain and direct copies exist.
fn assert_kept_layout(home: &Home, label: &str, sync: &Output) {
    let transcript = format!(
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&sync.stdout),
        String::from_utf8_lossy(&sync.stderr)
    );
    assert!(
        !present(&home.path(COPILOT_PLUGIN_ROOT)),
        "{label}: the Copilot combined root has current ownership and should retire\n{transcript}"
    );
    for relative in AGY_PLUGIN_ROOTS {
        assert!(
            present(&home.path(relative)),
            "{label}: combined plugin root must remain without current ownership: {relative}\n{transcript}"
        );
    }
    assert!(
        transcript.contains("not ccync-owned"),
        "{label}: output must explain why the combined roots stay\n{transcript}"
    );
    for relative in DIRECT_COPIES {
        assert!(
            home.path(relative).is_file(),
            "{label}: direct copy must exist: {relative}\n{transcript}"
        );
    }
}

#[test]
fn retire_combined_plugin_roots_through_full_sync_twice() {
    let home = Home::new();
    seed_machine(&home);
    seed_legacy_combined_roots(&home);
    for relative in std::iter::once(COPILOT_PLUGIN_ROOT).chain(AGY_PLUGIN_ROOTS) {
        assert!(
            present(&home.path(relative)),
            "the seeded legacy root must exist before the first sync: {relative}"
        );
    }

    let first = home.run(&["sync", "--yes"]);
    assert_eq!(
        first.status.code(),
        Some(0),
        "first sync must complete after demoting fingerprint-less ownership\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    assert_kept_layout(&home, "first sync", &first);
    assert!(
        home.path(LOCKFILE).is_file(),
        "first sync must leave the lockfile observable: {LOCKFILE}"
    );
    let first_lock: serde_json::Value =
        serde_json::from_slice(&fs::read(home.path(LOCKFILE)).unwrap()).unwrap();
    assert!(
        first_lock
            .pointer("/_ccyncProjection/historicalOwnership")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|rows| !rows.is_empty()),
        "migration must retain the fingerprint-less ownership rows as historical: {first_lock}"
    );
    let first_bytes: Vec<Vec<u8>> = DIRECT_COPIES
        .iter()
        .map(|relative| fs::read(home.path(relative)).unwrap())
        .collect();

    let second = home.run(&["sync", "--yes"]);
    assert_eq!(
        second.status.code(),
        Some(0),
        "repeat sync must complete while retaining roots without current ownership\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert_kept_layout(&home, "repeat sync", &second);
    assert!(
        home.path(LOCKFILE).is_file(),
        "repeat sync must keep the lockfile observable: {LOCKFILE}"
    );
    for (relative, before) in DIRECT_COPIES.iter().zip(&first_bytes) {
        assert_eq!(
            &fs::read(home.path(relative)).unwrap(),
            before,
            "repeat sync must leave the direct copy's bytes unchanged: {relative}"
        );
    }
}

fn seed_two_providers(home: &Home) {
    const SHA_A: &str = "0123456789abcdef0123456789abcdef01234567";
    const SHA_B: &str = "89abcdef0123456789abcdef0123456789abcdef";

    let plugin_a = home.path(&format!(".ccync/cache/alpha@{SHA_A}"));
    write(
        &plugin_a.join(".claude-plugin/plugin.json"),
        r#"{"name":"alpha","version":"1.0.0"}"#,
    );
    write(
        &plugin_a.join("skills/alpha-skill/SKILL.md"),
        "---\nname: alpha-skill\ndescription: Alpha skill.\n---\nAlpha body.\n",
    );
    write(
        &plugin_a.join("agents/alpha-agent.md"),
        "---\nname: alpha-agent\ndescription: Alpha agent.\n---\nAlpha agent body.\n",
    );
    write(
        &plugin_a.join("commands/alpha-cmd.md"),
        "---\ndescription: Alpha command.\n---\nInspect $ARGUMENTS\n",
    );

    let plugin_b = home.path(&format!(".ccync/cache/beta@{SHA_B}"));
    write(
        &plugin_b.join(".claude-plugin/plugin.json"),
        r#"{"name":"beta","version":"1.0.0"}"#,
    );
    write(
        &plugin_b.join("skills/beta-skill/SKILL.md"),
        "---\nname: beta-skill\ndescription: Beta skill.\n---\nBeta body.\n",
    );
    write(
        &plugin_b.join("agents/beta-agent.md"),
        "---\nname: beta-agent\ndescription: Beta agent.\n---\nBeta agent body.\n",
    );
    write(
        &plugin_b.join("commands/beta-cmd.md"),
        "---\ndescription: Beta command.\n---\nInspect $ARGUMENTS\n",
    );

    write(
        &home.path(".ccync/plugins.json"),
        r#"{"plugins":[
            {"pluginId":"alpha","displayName":"Alpha","supportTier":"local","sourceType":"local","license":"proprietary","checksumPolicy":"none","componentMap":{"skills":true,"mcp":false,"agents":true},"supportedProviders":["claude","codex","copilot","opencode","agy"],"installStrategy":"bundled-local","source":"~/.ccync/plugins/alpha","defaultProfiles":[],"allowAutoUpdate":false,"localOverridePolicy":"always"},
            {"pluginId":"beta","displayName":"Beta","supportTier":"local","sourceType":"local","license":"proprietary","checksumPolicy":"none","componentMap":{"skills":true,"mcp":false,"agents":true},"supportedProviders":["claude","codex","copilot","opencode","agy"],"installStrategy":"bundled-local","source":"~/.ccync/plugins/beta","defaultProfiles":[],"allowAutoUpdate":false,"localOverridePolicy":"always"}
        ]}"#,
    );
    write(
        &home.path(".ccync/config.json"),
        &format!(r#"{{"selectedProjectionTargets":[{ALL_TARGETS}]}}"#),
    );
}

#[test]
fn two_phase_sync_twice_with_two_providers_preserves_artifacts_and_ownership() {
    let home = Home::new();
    seed_two_providers(&home);

    let first = home.run(&["sync", "--yes"]);
    assert_eq!(
        first.status.code(),
        Some(0),
        "first sync failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );

    let lockfile = home.path(LOCKFILE);
    assert!(lockfile.is_file(), "lock.json must be observable");

    let first_records = projection::direct_records(&lockfile);
    assert!(
        !first_records.is_empty(),
        "direct categories must record ownership rows"
    );

    let sources: std::collections::BTreeSet<_> = first_records
        .iter()
        .filter_map(|r| r.source_id.as_deref())
        .collect();
    assert!(
        sources.contains("alpha") && sources.contains("beta"),
        "both alpha and beta providers must have attributed ownership rows in lock.json: {sources:?}"
    );

    assert!(
        first_records
            .iter()
            .any(|r| r.kind == projection::DirectArtifactKind::Skill)
            && first_records
                .iter()
                .any(|r| r.kind == projection::DirectArtifactKind::Agent)
            && first_records
                .iter()
                .any(|r| r.kind == projection::DirectArtifactKind::Command),
        "direct categories for skill, agent, and command must all be present in lock.json"
    );

    // Every direct path recorded in lockfile must exist on disk and carry no extra alias.
    let mut first_file_bytes = std::collections::BTreeMap::new();
    for rec in &first_records {
        let entry_path = home.path(&rec.path.to_string_lossy());
        assert!(
            entry_path.exists(),
            "direct record path must exist on disk: {}",
            entry_path.display()
        );
        let path_str = rec.path.to_string_lossy();
        assert!(
            !path_str.contains("alpha-skill-") && !path_str.contains("beta-skill-"),
            "no unwanted alias may be created for direct components: {path_str}"
        );
        if entry_path.is_file() {
            first_file_bytes.insert(rec.path.clone(), fs::read(&entry_path).unwrap());
        } else if entry_path.is_dir() {
            let skill_file = entry_path.join("SKILL.md");
            if skill_file.is_file() {
                first_file_bytes.insert(rec.path.join("SKILL.md"), fs::read(&skill_file).unwrap());
            }
        }
    }

    // Repeat sync: second run must converge, preserve bytes, and keep ownership unchanged.
    let second = home.run(&["sync", "--yes"]);
    assert_eq!(
        second.status.code(),
        Some(0),
        "second sync failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );

    assert!(lockfile.is_file(), "lock.json must remain observable");
    let second_records = projection::direct_records(&lockfile);
    assert_eq!(
        first_records, second_records,
        "direct categories and source attributions must be identical across two syncs"
    );

    for (rel_path, before_bytes) in &first_file_bytes {
        let file_path = home.path(&rel_path.to_string_lossy());
        assert!(
            file_path.is_file(),
            "direct artifact must still exist after second sync: {}",
            file_path.display()
        );
        let after_bytes = fs::read(&file_path).unwrap();
        assert_eq!(
            &after_bytes,
            before_bytes,
            "artifact bytes must be stable across two syncs: {}",
            file_path.display()
        );
    }
}
