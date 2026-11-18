//! Direct projection through the ordinary sync path.
//!
//! `baseline_matrix` runs the 35 combinations of five import sources and seven
//! projection targets. Each combination uses its own isolated home: it seeds
//! the source's native roots with a loose skill (and an MCP server where the
//! source has an MCP config), imports them through the engine functions
//! `ccync init` runs after consent, and then runs ordinary sync twice. Each
//! combination checks the recorded origins of the imported components, one
//! published copy per physical root for the bundled provider, retirement of a
//! real combined copy, and a second sync that changes nothing.
//!
//! `lifecycle_matrix` walks two selected providers and one unselected consumer
//! that shares a skill root with one of them through add, deselect, re-add,
//! upgrade, remove, and uninstall. Each step asserts the exact provider outputs
//! it changed.

use ccync_engine::adopt::{
    adopt_items, adopt_loose_skills, adopt_mcp_definitions, read_adoption_records,
    read_mcp_definitions, scan_source, store_adoption_records, AdoptionRecord, AgentKind, ItemKind,
    SourceState,
};
use ccync_engine::config::CcyncConfig;
use ccync_engine::delivery_apply::UnavailableNativeRunner;
use ccync_engine::install::{run_uninstall, run_update_with_evidence_using};
use ccync_foundation::target::AgentId;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
mod support;
use support::isolated;

/// The five import sources.
const SOURCES: [AgentId; 5] = [
    AgentId::Claude,
    AgentId::Codex,
    AgentId::Copilot,
    AgentId::Opencode,
    AgentId::Agy,
];

/// Name of the loose skill each source fixture holds.
const IMPORTED_SKILL: &str = "imported";

/// Name of the MCP server each source fixture with an MCP config holds.
const IMPORTED_MCP: &str = "imported-mcp";

/// Every physical skill root a target can publish into.
const SKILL_ROOTS: [&str; 6] = [
    ".claude/skills",
    ".agents/skills",
    ".copilot/skills",
    ".config/opencode/skills",
    ".gemini/antigravity-cli/skills",
    ".gemini/config/skills",
];

/// The seven projection targets, in canonical order.
const TARGETS: [&str; 7] = [
    "claude", "codex", "copilot", "opencode", "agy-cli", "agy-ide", "agy-gui",
];

/// The skill path each target's direct route writes for the fixture skill
/// when that target is the only one selected.
fn direct_skill_path(target: &str) -> &'static str {
    match target {
        "claude" => ".claude/skills/example/SKILL.md",
        "codex" => ".agents/skills/example/SKILL.md",
        "opencode" => ".config/opencode/skills/example/SKILL.md",
        "copilot" => ".copilot/skills/example/SKILL.md",
        "agy-cli" => ".gemini/antigravity-cli/skills/example/SKILL.md",
        "agy-gui" => ".gemini/config/skills/example/SKILL.md",
        // AGY IDE shares the `~/.gemini/config/skills` root with AGY GUI.
        "agy-ide" => ".gemini/config/skills/example/SKILL.md",
        other => panic!("no direct skill path for target {other}"),
    }
}

/// The combined `ccync` copy in the skill root each target publishes into.
/// AGY IDE and AGY GUI both publish into `~/.gemini/config/skills`.
fn combined_copy_path(target: &str) -> &'static str {
    match target {
        "claude" => ".claude/skills/ccync",
        "codex" => ".agents/skills/ccync",
        "opencode" => ".config/opencode/skills/ccync",
        "copilot" => ".copilot/skills/ccync",
        "agy-cli" => ".gemini/antigravity-cli/skills/ccync",
        "agy-gui" | "agy-ide" => ".gemini/config/skills/ccync",
        other => panic!("no combined copy path for target {other}"),
    }
}

/// Link a combined `ccync` copy to the canonical render root, the shape a
/// combined copy has on disk before retirement.
fn seed_combined_copy(root: &Path, rel: &str) -> PathBuf {
    let render = root.join(".ccync").join("build").join("render");
    std::fs::create_dir_all(&render).unwrap();
    // `mklink` reads a `/` in its path as a switch, so use native separators.
    let link = root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    ccync_foundation::platform::create_dir_link(&render, &link).unwrap();
    link
}

/// The loose skill directory a source fixture seeds in that source's native
/// skill root.
fn source_skill_dir(root: &Path, source: AgentId) -> PathBuf {
    let skills = match source {
        AgentId::Claude => root.join(".claude/skills"),
        AgentId::Codex => root.join(".agents/skills"),
        AgentId::Copilot => root.join(".copilot/skills"),
        AgentId::Opencode => opencode_config_dir().join("skills"),
        AgentId::Agy => root.join(".gemini/antigravity-cli/skills"),
    };
    skills.join(IMPORTED_SKILL)
}

/// OpenCode's config directory in the isolated home.
fn opencode_config_dir() -> PathBuf {
    ccync_foundation::paths::config_dir()
        .expect("isolated config dir")
        .join("opencode")
}

/// Seed one source's native roots: a loose skill, and an MCP server in the
/// source's own MCP config when the source has one. Returns the MCP config
/// path, or `None` for a source without an MCP config.
fn seed_source(root: &Path, source: AgentId) -> Option<PathBuf> {
    write(
        &source_skill_dir(root, source).join("SKILL.md"),
        &format!(
            "---\nname: {IMPORTED_SKILL}\ndescription: Imported skill.\n---\nFrom {source}.\n"
        ),
    );
    let server = format!(r#"{{"{IMPORTED_MCP}":{{"command":"node","args":["server.js"]}}}}"#);
    let (path, body) = match source {
        AgentId::Claude => (
            root.join(".claude.json"),
            format!(r#"{{"mcpServers":{server}}}"#),
        ),
        AgentId::Codex => (
            root.join(".codex/config.toml"),
            format!("[mcp_servers.{IMPORTED_MCP}]\ncommand = 'node'\nargs = ['server.js']\n"),
        ),
        AgentId::Copilot => (
            root.join(".copilot/mcp-config.json"),
            format!(r#"{{"mcpServers":{server}}}"#),
        ),
        AgentId::Opencode => (
            opencode_config_dir().join("opencode.json"),
            format!(
                r#"{{"mcp":{{"{IMPORTED_MCP}":{{"type":"local","command":["node","server.js"]}}}}}}"#
            ),
        ),
        AgentId::Agy => return None,
    };
    write(&path, &body);
    Some(path)
}

/// Import `source` through the engine functions `ccync init` runs after
/// consent, in the same order: the `_adoption` record with the target snapshot
/// into `plugins.json`, then the adopted items, the MCP definitions, and the
/// loose skills into the lock.
fn import_source(source: AgentId, targets: &[&str]) {
    let scan = scan_source(source);
    assert_eq!(
        scan.state,
        SourceState::Ready,
        "{source}: {:?}",
        scan.reason
    );
    let records = scan
        .adoptable
        .items
        .iter()
        .map(|item| AdoptionRecord {
            source: source.as_str().to_string(),
            kind: match item.kind {
                ItemKind::Plugin => "plugin",
                ItemKind::McpServer => "mcp-server",
                ItemKind::Skill => "skill",
            }
            .to_string(),
            component_id: item.name.clone(),
            name: item.name.clone(),
            origin: Some(
                item.source
                    .clone()
                    .unwrap_or_else(|| panic!("{source}: {} has no origin", item.name)),
            ),
            revision: None,
            digest: None,
            enabled: None,
            policy: scan.adoptable.policy.get(&item.name).cloned(),
        })
        .collect::<Vec<_>>();
    let keys = targets
        .iter()
        .map(|target| target.to_string())
        .collect::<Vec<_>>();
    let catalog = ccync_foundation::paths::personal_catalog_path().expect("catalog path");
    store_adoption_records(&catalog, &records, &keys).expect("adoption record");
    let lock = ccync_foundation::paths::plugins_lock_path().expect("lock path");
    let items = scan.adoptable.items.clone();
    adopt_items(items.clone(), &lock).expect("adopt items");
    let kind = match source {
        AgentId::Claude => AgentKind::Claude,
        AgentId::Codex => AgentKind::Codex,
        AgentId::Copilot => AgentKind::Copilot,
        AgentId::Opencode => AgentKind::OpenCode,
        AgentId::Agy => AgentKind::Agy,
    };
    adopt_mcp_definitions(read_mcp_definitions(&kind).store, &lock).expect("adopt MCP");
    adopt_loose_skills(&items, &HashSet::new(), &lock).expect("adopt loose skills");
}

/// The lockfile as JSON.
fn read_lock(root: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(root.join(".ccync/build/lock.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// Whether a recorded origin names `expected`. Both sides are resolved, so
/// separator spelling and the Windows verbatim prefix do not matter.
fn same_path(recorded: &str, expected: &Path) -> bool {
    match (
        std::fs::canonicalize(recorded),
        std::fs::canonicalize(expected),
    ) {
        (Ok(recorded), Ok(expected)) => recorded == expected,
        _ => false,
    }
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// One bundled plugin with a skill, an agent, and a command, selecting `targets`.
fn fixture(home: &Path, targets: &[&str]) {
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
        &plugin.join("agents/helper.md"),
        "---\nname: helper\ndescription: Helper agent.\n---\nBody.\n",
    );
    write(
        &plugin.join("commands/audit.md"),
        "---\ndescription: Audit command.\n---\nInspect $ARGUMENTS\n",
    );
    write_lock(home, &["example"]);
    let selected = targets
        .iter()
        .map(|target| format!("\"{target}\""))
        .collect::<Vec<_>>()
        .join(",");
    write(
        &home.join(".ccync/config.json"),
        &format!(r#"{{"selectedProjectionTargets":[{selected}]}}"#),
    );
}

/// Sets the lock's `resolvedPlugins` to exactly the bundled plugins `plugins`.
/// Every other key stays as the last sync wrote it, including the
/// `_ccyncProjection` registry that records which outputs ccync owns.
/// Replacing the whole file would drop that registry, and the next sync would
/// then read the provider's own copies as foreign.
fn write_lock(home: &Path, plugins: &[&str]) {
    let path = home.join(".ccync/build/lock.json");
    let mut lock: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let entries = plugins
        .iter()
        .map(|id| {
            serde_json::json!({
                "pluginId": id,
                "resolvedSource": id,
                "installStrategy": "bundled-local",
                "sha": "0123456789abcdef0123456789abcdef01234567",
            })
        })
        .collect::<Vec<_>>();
    lock["resolvedPlugins"] = serde_json::Value::Array(entries);
    write(&path, &lock.to_string());
}

/// The second bundled provider: one skill named `extra`, whose body is `body`.
/// It has no agent or command, so its outputs are the skill copies only.
fn write_extra_provider(home: &Path, body: &str) {
    let plugin = home.join(".ccync/plugins/extra");
    write(
        &plugin.join(".claude-plugin/plugin.json"),
        r#"{"name":"extra","version":"1.0.0"}"#,
    );
    write(
        &plugin.join("skills/extra/SKILL.md"),
        &format!("---\nname: extra\ndescription: Extra skill.\n---\n{body}\n"),
    );
}

/// Drops the `generatedAt` lines that the managed-state writers stamp with the
/// current time on every run. Those lines are the only byte-level churn this
/// matrix tolerates, and the receipt records that gap.
fn without_generated_at(bytes: Vec<u8>) -> Vec<u8> {
    match String::from_utf8(bytes) {
        Ok(text) if text.contains("\"generatedAt\"") => text
            .lines()
            .filter(|line| !line.contains("\"generatedAt\""))
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes(),
        Ok(text) => text.into_bytes(),
        Err(error) => error.into_bytes(),
    }
}

/// Every file under `root` with its bytes, keyed by relative path. The lifecycle
/// lock lives under `.ccync/build` and is excluded because it is not output.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, root, out);
            } else if let Ok(bytes) = std::fs::read(&path) {
                let rel = path.strip_prefix(root).unwrap().to_path_buf();
                out.insert(rel, without_generated_at(bytes));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out.retain(|rel, _| {
        let key = rel.to_string_lossy().replace('\\', "/");
        !key.starts_with(".ccync/build/lock")
    });
    out
}

/// Baseline matrix: 5 sources × 7 targets = 35 combinations. Each imports
/// its source through the production adoption functions, then takes one
/// ordinary sync that publishes the target's direct skill once per physical
/// root with its origin, retires a real combined copy, and changes nothing on
/// a second sync.
#[test]
fn baseline_matrix() {
    use ccync_foundation::target::ProjectionTargetId;
    let mut combinations = 0;
    for source in SOURCES {
        for target in TARGETS {
            combinations += 1;
            let label = format!("source={source} target={target}");
            let (home, _lock) = isolated();
            let root = home.root();

            // Import the source's own native content.
            let mcp_config = seed_source(root, source);
            let skill_dir = source_skill_dir(root, source);
            let skill_bytes = std::fs::read(skill_dir.join("SKILL.md")).unwrap();
            import_source(source, &[target]);
            let lock = read_lock(root);
            let loose = lock["_looseSkills"].as_array().cloned().unwrap_or_default();
            let loose_origins = loose
                .iter()
                .filter(|entry| entry["name"] == IMPORTED_SKILL)
                .filter_map(|entry| entry["origin"].as_str())
                .collect::<Vec<_>>();
            assert_eq!(loose_origins.len(), 1, "{label}: {loose:?}");
            assert!(
                same_path(loose_origins[0], &skill_dir),
                "{label}: loose skill origin {} is not {skill_dir:?}",
                loose_origins[0]
            );
            let catalog = ccync_foundation::paths::personal_catalog_path().unwrap();
            let records = read_adoption_records(&catalog).unwrap();
            let origin_of = |kind: &str, name: &str| {
                records
                    .iter()
                    .find(|record| {
                        record.source == source.as_str()
                            && record.kind == kind
                            && record.component_id == name
                    })
                    .and_then(|record| record.origin.clone())
                    .unwrap_or_else(|| panic!("{label}: no {kind} record for {name}"))
            };
            assert!(
                same_path(&origin_of("skill", IMPORTED_SKILL), &skill_dir),
                "{label}: adoption record origin"
            );
            if let Some(config) = &mcp_config {
                assert!(
                    same_path(&origin_of("mcp-server", IMPORTED_MCP), config),
                    "{label}: MCP adoption record origin"
                );
            }

            // The bundled provider, plus a real combined copy for the sync to retire.
            fixture(root, &[target]);
            let combined = seed_combined_copy(root, combined_copy_path(target));
            assert!(combined.symlink_metadata().is_ok(), "{label}: seeded copy");

            let outcome =
                run_update_with_evidence_using(&CcyncConfig::default(), &UnavailableNativeRunner)
                    .unwrap_or_else(|error| panic!("{label}: first sync failed: {error}"));
            let direct = root.join(direct_skill_path(target));
            assert!(
                direct.is_file(),
                "{label}: direct skill missing at {direct:?}"
            );
            // One copy per physical root: a single target publishes into one root.
            let copies = SKILL_ROOTS
                .iter()
                .filter(|skills| root.join(skills).join("example/SKILL.md").is_file())
                .count();
            assert_eq!(copies, 1, "{label}: the bundled skill must be written once");
            let target_id: ProjectionTargetId = target.parse().unwrap();
            let route = outcome
                .routes
                .iter()
                .find(|route| {
                    route.target == target_id
                        && route.original_name == "example"
                        && route.kind == ccync_foundation::provider::ComponentKind::Skill
                })
                .unwrap_or_else(|| panic!("{label}: no skill route: {:#?}", outcome.routes));
            assert!(route.published && route.owned, "{label}: {route:?}");
            let bundle = root.join(".ccync/plugins/example");
            assert!(
                route
                    .origin_path
                    .as_deref()
                    .is_some_and(|origin| same_path(origin, &bundle)),
                "{label}: route origin {:?} is not {bundle:?}",
                route.origin_path
            );
            assert!(
                combined.symlink_metadata().is_err(),
                "{label}: combined copy must be retired in the same sync"
            );
            // The imported skill is materialized from its recorded origin, and
            // the origin itself is left byte-identical.
            assert_eq!(
                std::fs::read(root.join(".ccync/build/render/skills/imported/SKILL.md")).ok(),
                Some(skill_bytes.clone()),
                "{label}: imported skill must come from its recorded origin"
            );
            assert_eq!(
                std::fs::read(skill_dir.join("SKILL.md")).unwrap(),
                skill_bytes
            );
            // The imported MCP server reaches the managed MCP set for every
            // source whose MCP definitions the import reads.
            if matches!(source, AgentId::Claude | AgentId::Codex | AgentId::Opencode) {
                let managed = std::fs::read_to_string(root.join(".ccync/build/mcp/managed.json"))
                    .unwrap_or_default();
                assert!(managed.contains(IMPORTED_MCP), "{label}: {managed}");
            }
            let first = snapshot(root);

            run_update_with_evidence_using(&CcyncConfig::default(), &UnavailableNativeRunner)
                .unwrap_or_else(|error| panic!("{label}: second sync failed: {error}"));
            let after = snapshot(root);
            let changed: Vec<&PathBuf> = first
                .iter()
                .filter(|(path, bytes)| after.get(*path) != Some(*bytes))
                .map(|(path, _)| path)
                .chain(after.keys().filter(|path| !first.contains_key(*path)))
                .collect();
            assert!(
                changed.is_empty(),
                "{label}: a second sync must change no output bytes, changed: {changed:?}"
            );
        }
    }
    assert_eq!(combinations, 35);
}

/// Relative path of `path` with forward slashes, so one expectation string
/// names the same output on every platform.
fn rel_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Relative paths whose bytes differ between two snapshots, excluding ccync's
/// own state under `.ccync/`, which is not provider output.
fn changed_outputs(
    before: &BTreeMap<PathBuf, Vec<u8>>,
    after: &BTreeMap<PathBuf, Vec<u8>>,
) -> BTreeSet<String> {
    let mut changed = BTreeSet::new();
    for (path, bytes) in before {
        if after.get(path) != Some(bytes) {
            changed.insert(rel_key(path));
        }
    }
    for path in after.keys() {
        if !before.contains_key(path) {
            changed.insert(rel_key(path));
        }
    }
    changed.retain(|key| !key.starts_with(".ccync/"));
    changed
}

/// Writes the selection, keeping the fixture plugin in place.
fn select(home: &Path, targets: &[&str]) {
    let selected = targets
        .iter()
        .map(|target| format!("\"{target}\""))
        .collect::<Vec<_>>()
        .join(",");
    write(
        &home.join(".ccync/config.json"),
        &format!(r#"{{"selectedProjectionTargets":[{selected}]}}"#),
    );
}

/// Replaces the fixture skill body, which an upgrade changes in place.
fn write_skill(home: &Path, body: &str) {
    write(
        &home.join(".ccync/plugins/example/skills/example/SKILL.md"),
        &format!(
            "---
name: example
description: Example skill.
---
{body}
"
        ),
    );
}

/// Runs one lifecycle step and asserts that exactly `expected` provider outputs
/// changed. `act` performs the step's sync, uninstall, or source edit.
fn step(root: &Path, label: &str, expected: &[&str], act: impl FnOnce()) {
    let before = snapshot(root);
    act();
    let changed = changed_outputs(&before, &snapshot(root));
    let expected: BTreeSet<String> = expected.iter().map(|path| path.to_string()).collect();
    assert_eq!(
        changed, expected,
        "{label}: changed provider outputs differ from the plan"
    );
}

/// Lifecycle matrix. Two selected providers, `claude` and `agy-gui`,
/// and one unselected consumer, `agy-ide`, which reads the same
/// `.gemini/config/skills` root as `agy-gui`. Copilot joins and leaves during
/// the walk. Each step runs one ordinary sync, uninstall, or source edit, and
/// asserts that only the paths the step plans changed.
#[test]
fn lifecycle_matrix() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, &["claude", "agy-gui"]);
    let claude = ".claude/skills/example/SKILL.md";
    let agy_gui = ".gemini/config/skills/example/SKILL.md";
    let copilot = ".copilot/skills/example/SKILL.md";
    // The Copilot direct outputs for the fixture plugin: its agent and its
    // command as a skill, next to the skill itself.
    let copilot_direct = [
        ".copilot/agents/helper.agent.md",
        ".copilot/skills/audit/SKILL.md",
        copilot,
    ];

    run_update_with_evidence_using(&CcyncConfig::default(), &UnavailableNativeRunner)
        .expect("first sync");
    assert!(root.join(claude).is_file(), "claude skill after first sync");
    assert!(
        root.join(agy_gui).is_file(),
        "agy-gui skill after first sync"
    );
    assert!(
        !root.join(".gemini/antigravity-ide/skills/example").exists(),
        "unselected agy-ide must receive no copy"
    );

    // Repeat sync: no output changes.
    step(root, "repeat sync", &[], || {
        run_update_with_evidence_using(&CcyncConfig::default(), &UnavailableNativeRunner)
            .expect("repeat sync");
    });

    // Add Copilot. Only its own output appears; the shared root is untouched.
    step(root, "add copilot", &copilot_direct, || {
        select(root, &["claude", "agy-gui", "copilot"]);
        run_update_with_evidence_using(&CcyncConfig::default(), &UnavailableNativeRunner)
            .expect("add sync");
    });
    assert!(root.join(copilot).is_file());

    // Deselect Copilot. Its copy is pruned; the other providers keep theirs.
    step(root, "deselect copilot", &copilot_direct, || {
        select(root, &["claude", "agy-gui"]);
        run_update_with_evidence_using(&CcyncConfig::default(), &UnavailableNativeRunner)
            .expect("deselect sync");
    });
    assert!(
        !root.join(copilot).exists(),
        "deselected copilot copy must be pruned"
    );

    // Re-add Copilot after the deselect.
    step(root, "re-add copilot", &copilot_direct, || {
        select(root, &["claude", "agy-gui", "copilot"]);
        run_update_with_evidence_using(&CcyncConfig::default(), &UnavailableNativeRunner)
            .expect("re-add sync");
    });

    // Upgrade the source skill. Each selected copy rewrites in place.
    step(root, "upgrade skill", &[claude, agy_gui, copilot], || {
        write_skill(root, "Upgraded body.");
        run_update_with_evidence_using(&CcyncConfig::default(), &UnavailableNativeRunner)
            .expect("upgrade sync");
    });
    assert!(
        std::fs::read_to_string(root.join(claude))
            .unwrap()
            .contains("Upgraded body."),
        "claude copy must carry the upgraded body"
    );

    // Remove the skill from the source. Every selected copy is pruned.
    step(root, "remove skill", &[claude, agy_gui, copilot], || {
        std::fs::remove_dir_all(root.join(".ccync/plugins/example/skills/example")).unwrap();
        run_update_with_evidence_using(&CcyncConfig::default(), &UnavailableNativeRunner)
            .expect("remove sync");
    });
    assert!(
        !root.join(claude).exists(),
        "removed skill leaves no claude copy"
    );

    // Restore the skill for the rest of the walk.
    step(root, "restore skill", &[claude, agy_gui, copilot], || {
        write_skill(root, "Upgraded body.");
        run_update_with_evidence_using(&CcyncConfig::default(), &UnavailableNativeRunner)
            .expect("restore sync");
    });

    // Provider-level steps for the second provider, `extra`. Its outputs are the
    // skill copies on every selected target (claude, agy-gui, copilot). Each
    // step below changes only these, so the `example` provider's outputs stay
    // byte-identical.
    let extra_claude = ".claude/skills/extra/SKILL.md";
    let extra_agy_gui = ".gemini/config/skills/extra/SKILL.md";
    let extra_copilot = ".copilot/skills/extra/SKILL.md";
    let extra_all = [extra_claude, extra_agy_gui, extra_copilot];

    // Add the second provider. Its copies appear on every selected target.
    step(root, "add extra provider", &extra_all, || {
        write_extra_provider(root, "Extra body.");
        write_lock(root, &["example", "extra"]);
        run_update_with_evidence_using(&CcyncConfig::default(), &UnavailableNativeRunner)
            .expect("add extra sync");
    });
    assert!(root.join(extra_claude).is_file());

    // Repeat sync with both providers present: no output changes.
    step(root, "repeat sync with two providers", &[], || {
        run_update_with_evidence_using(&CcyncConfig::default(), &UnavailableNativeRunner)
            .expect("repeat two-provider sync");
    });

    // Remove the second provider from the source. Its copies go; `example` keeps its own.
    step(root, "remove extra provider", &extra_all, || {
        write_lock(root, &["example"]);
        std::fs::remove_dir_all(root.join(".ccync/plugins/extra")).unwrap();
        run_update_with_evidence_using(&CcyncConfig::default(), &UnavailableNativeRunner)
            .expect("remove extra sync");
    });
    assert!(!root.join(extra_claude).exists());

    // Re-add the second provider. Its copies come back on every selected target.
    step(root, "re-add extra provider", &extra_all, || {
        write_extra_provider(root, "Extra body.");
        write_lock(root, &["example", "extra"]);
        run_update_with_evidence_using(&CcyncConfig::default(), &UnavailableNativeRunner)
            .expect("re-add extra sync");
    });

    // Upgrade the second provider's skill. Only its copies are rewritten.
    step(root, "upgrade extra provider", &extra_all, || {
        write_extra_provider(root, "Upgraded extra body.");
        run_update_with_evidence_using(&CcyncConfig::default(), &UnavailableNativeRunner)
            .expect("upgrade extra sync");
    });
    assert!(
        std::fs::read_to_string(root.join(extra_claude))
            .unwrap()
            .contains("Upgraded extra body."),
        "extra copy must carry the upgraded body"
    );

    // Deselect agy-gui. The shared root's copy goes, since agy-ide is not selected.
    // Copilot's direct routes are outside this step, so it leaves them exactly
    // as they were.
    let agy_gui_agent = ".gemini/config/agents/helper.md";
    let deselect_both = [
        &[agy_gui, agy_gui_agent, extra_agy_gui][..],
        &copilot_direct[..],
        &[extra_copilot][..],
    ]
    .concat();
    step(root, "deselect agy-gui and copilot", &deselect_both, || {
        select(root, &["claude"]);
        run_update_with_evidence_using(&CcyncConfig::default(), &UnavailableNativeRunner)
            .expect("deselect agy-gui sync");
    });
    assert!(
        !root.join(agy_gui).exists(),
        "unselected shared root copy must be pruned"
    );
    assert!(
        !root.join(".gemini/antigravity-ide/skills/example").exists(),
        "unselected agy-ide must stay without a copy"
    );

    // Uninstall removes every remaining ccync-owned direct output.
    let claude_agent = ".claude/agents/helper.md";
    let claude_command = ".claude/commands/audit.md";
    let uninstalled = [claude, claude_agent, claude_command, extra_claude];
    step(root, "uninstall", &uninstalled, || {
        run_uninstall().expect("uninstall");
    });
    assert!(
        !root.join(claude).exists(),
        "uninstall removes the claude copy"
    );
    assert!(
        !root.join(claude_command).exists(),
        "uninstall removes the recorded native claude command"
    );
    assert!(
        !snapshot(root)
            .keys()
            .any(|path| path.starts_with(".copilot/installed-plugins/ccync-copilot")),
        "uninstall removes every file of the Copilot combined plugin copy"
    );
}
