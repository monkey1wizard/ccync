//! Acceptance probes for the delivery preview that `ccync init` prints before
//! consent. Each probe runs the real `ccync` binary against a temporary home,
//! so the preview reads the same native roots a real run would read.

use std::{
    fs,
    path::PathBuf,
    process::{Command, Output, Stdio},
};

/// Committed machine roots relative to the fixture home. Windows uses the
/// APPDATA and LOCALAPPDATA layout, other platforms the XDG defaults.
const STATE_ROUTES_PATH: &str = if cfg!(windows) {
    "AppData/Local/ccync/state/build/routes.json"
} else {
    ".local/state/ccync/build/routes.json"
};
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
            match relative {
                ".ccync/build/lock.json" => STATE_LOCK_PATH,
                ".ccync/plugins.json" => MACHINE_CATALOG_PATH,
                ".ccync/config.json" => MACHINE_CONFIG_PATH,
                _ => relative,
            }
        } else {
            relative
        };
        if committed && mapped.starts_with(".ccync/cache/") {
            self.root
                .join(mapped.replacen(".ccync/cache/", STATE_SOURCES_PREFIX, 1))
        } else {
            self.root.join(mapped)
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        // An empty PATH directory: no client CLI is reachable from the binary.
        let isolated_bin = self.root.join("isolated-bin");
        fs::create_dir_all(&isolated_bin).unwrap();
        Command::new(env!("CARGO_BIN_EXE_ccync"))
            .args(args)
            .current_dir(&self.root)
            .env("HOME", &self.root)
            .env("USERPROFILE", &self.root)
            .env("APPDATA", self.root.join("AppData").join("Roaming"))
            .env("LOCALAPPDATA", self.root.join("AppData").join("Local"))
            .env("XDG_CONFIG_HOME", self.root.join(".config"))
            .env("PATH", &isolated_bin)
            .env_remove("CCYNC_ASSUME_YES")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env_remove("COPILOT_HOME")
            .env_remove("OPENCODE_CONFIG_DIR")
            .env_remove("OPENCODE_CONFIG")
            .env_remove("OPENCODE_CONFIG_CONTENT")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// A skill directory with a manifest, placed under `relative` from home.
    fn write_skill(&self, relative: &str, name: &str) -> PathBuf {
        let dir = self.root.join(relative).join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: fixture\n---\n"),
        )
        .unwrap();
        dir
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn delivery_output_preview_names_foreign_alias_entry_with_its_foreign_path() {
    let home = Home::new();
    home.write_skill(".claude/skills", "shared-skill");
    // A same-name skill that ccync did not write, at a selected target's root.
    let foreign = home.write_skill(".agents/skills", "shared-skill");

    let output = home.run(&["init", "claude", "--agents", "claude,codex"]);

    let text = stdout(&output);
    assert!(
        text.contains("preview for source claude"),
        "the preview must print before the consent refusal: {text}"
    );
    let line = text
        .lines()
        .find(|line| line.contains("codex: shared-skill aliased to"))
        .unwrap_or_else(|| panic!("no foreign alias line in preview: {text}"));
    assert!(
        line.contains(&foreign.display().to_string()),
        "the alias line must name the foreign path {}: {line}",
        foreign.display()
    );
}

#[test]
fn delivery_output_preview_without_foreign_entry_has_no_alias_line() {
    let home = Home::new();
    home.write_skill(".claude/skills", "shared-skill");

    let output = home.run(&["init", "claude", "--agents", "claude,codex"]);

    assert!(
        !stdout(&output).contains("aliased to"),
        "no foreign entry means no alias line: {}",
        stdout(&output)
    );
}

#[test]
fn delivery_output_preview_is_colorless_when_not_a_terminal() {
    let home = Home::new();
    home.write_skill(".claude/skills", "shared-skill");
    home.write_skill(".agents/skills", "shared-skill");

    let output = home.run(&["init", "claude", "--agents", "claude,codex"]);

    assert!(
        !output.stdout.contains(&0x1b),
        "piped preview must carry no ANSI escape sequences: {}",
        stdout(&output)
    );
}

#[test]
fn delivery_output_partial_route_text_never_claims_full_success() {
    use ccync_engine::managed_view::{route_evidence_line, RouteEvidence, VerificationScope};
    use ccync_foundation::provider::ComponentKind;
    use ccync_foundation::target::ProjectionTargetId::Codex;

    // A route whose write failed: the target stays selected, the loader never ran,
    // and the served-by root names where the target was meant to find the skill.
    let evidence = RouteEvidence {
        target: Codex,
        kind: ComponentKind::Skill,
        original_name: "shared-skill".to_string(),
        source_key: "provider".to_string(),
        origin_path: Some("/home/u/.ccync/cache/provider".to_string()),
        origin_reference: None,
        command_to_skill: false,
        failure_reason: Some("permission denied".to_string()),
        unsupported_reason: None,
        alias: None,
        path: Some("~/.agents/skills/shared-skill".to_string()),
        served_by_root: Some("~/.agents/skills".to_string()),
        multi_root: false,
        foreign_alias_path: None,
        selected: true,
        published: false,
        owned: false,
        loader: VerificationScope::LoaderNotRun,
        loader_version: None,
        registration: None,
    };
    let line = route_evidence_line(&evidence);

    assert!(line.contains("selected"));
    assert!(line.contains("not published"));
    assert!(line.contains("loader NotRun"));
    assert!(line.contains("served by root ~/.agents/skills"));
    assert!(line.contains("retry") || line.contains("action: fix"));
    assert!(
        line.contains("action: fix the reported write error (permission denied)"),
        "the action must name the failure reason: {line}"
    );
    assert!(
        line.contains("origin provider at /home/u/.ccync/cache/provider"),
        "{line}"
    );
    assert!(!line.contains("loaded"), "loader was not run: {line}");
    assert!(
        !line.contains("success"),
        "partial route claims success: {line}"
    );
    assert!(
        !line.contains('\u{1b}'),
        "route text must be colorless: {line}"
    );
}

#[test]
fn portable_cli_evidence() {
    let doctor_source = include_str!("../src/commands/doctor.rs");
    let lifecycle_source = include_str!("../src/commands/lifecycle.rs");
    let helper_count = [doctor_source, lifecycle_source]
        .iter()
        .map(|source| source.matches("fn label_standalone_route").count())
        .sum::<usize>();
    assert_eq!(
        helper_count, 1,
        "the route label helper must have one definition"
    );

    let home = Home::new();
    let source = home.root.join("inputs/portable-evidence");
    fs::create_dir_all(&source).unwrap();
    fs::write(
        source.join("SKILL.md"),
        "---\nname: portable-evidence\ndescription: Evidence fixture.\n---\nBody.\n",
    )
    .unwrap();
    fs::create_dir_all(home.root.join(".ccync/build")).unwrap();
    fs::write(
        home.path(".ccync/build/lock.json"),
        serde_json::json!({"_looseSkills": [{"name": "portable-evidence", "source": "fixture", "origin": source}]}).to_string(),
    )
    .unwrap();
    fs::write(
        home.path(".ccync/config.json"),
        r#"{"selectedProjectionTargets":["codex"]}"#,
    )
    .unwrap();
    let sync = home.run(&["sync", "--yes"]);
    assert_eq!(sync.status.code(), Some(0), "{}", stdout(&sync));
    let sync_stderr = String::from_utf8_lossy(&sync.stderr).into_owned();
    assert!(sync_stderr.contains("layout: authority"), "{sync_stderr}");
    assert!(sync_stderr.contains("layout face Codex:"), "{sync_stderr}");

    let doctor = home.run(&["doctor"]);
    assert_eq!(doctor.status.code(), Some(0), "{}", stdout(&doctor));
    let doctor_stdout = stdout(&doctor);
    for line in sync_stderr
        .lines()
        .filter(|line| line.starts_with("layout "))
    {
        assert!(
            doctor_stdout.lines().any(|doctor_line| doctor_line == line),
            "doctor output is missing sync evidence line `{line}`:\n{doctor_stdout}"
        );
    }

    let record_path = home.root.join(STATE_ROUTES_PATH);
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(record_path).unwrap()).unwrap();
    let route = record["routes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|route| route["originalName"] == "portable-evidence")
        .unwrap();
    assert_eq!(route["loader"], "loaderNotRun");
}

/// A Codex skill route for the route-level text probes. `published` decides
/// whether the write succeeded, and the other facts stay fixed.
fn codex_route(published: bool) -> ccync_engine::managed_view::RouteEvidence {
    use ccync_engine::managed_view::{RouteEvidence, VerificationScope};
    use ccync_foundation::provider::ComponentKind;
    use ccync_foundation::target::ProjectionTargetId::Codex;

    RouteEvidence {
        target: Codex,
        kind: ComponentKind::Skill,
        original_name: "shared-skill".to_string(),
        source_key: "provider".to_string(),
        origin_path: None,
        origin_reference: Some("~/.ccync/plugins/provider".to_string()),
        command_to_skill: false,
        failure_reason: None,
        unsupported_reason: None,
        alias: None,
        path: Some("~/.agents/skills/shared-skill".to_string()),
        served_by_root: Some("~/.agents/skills".to_string()),
        multi_root: false,
        foreign_alias_path: None,
        selected: true,
        published,
        owned: false,
        loader: VerificationScope::LoaderNotRun,
        loader_version: None,
        registration: None,
    }
}

#[test]
fn delivery_output_partial_retirement_shows_done_and_kept_in_colorless_text() {
    use ccync_engine::delivery_apply::route_outcome_line;
    use ccync_engine::install::RouteRetirement;

    // Claude's combined surface retired; Codex's stayed because its direct
    // publication failed. Both labels must appear on their own lines.
    let retired = route_outcome_line(&codex_route(true), Some(&RouteRetirement::Done), None);
    let kept = route_outcome_line(
        &codex_route(false),
        Some(&RouteRetirement::Kept {
            reason: "direct publication failed".to_string(),
        }),
        None,
    );

    assert!(retired.ends_with("; retirement: done"), "{retired}");
    assert!(
        kept.ends_with("; retirement: kept (direct publication failed)"),
        "{kept}"
    );
    for line in [&retired, &kept] {
        assert!(
            !line.contains('\u{1b}'),
            "route text must be colorless: {line}"
        );
        assert!(
            !line.contains("success"),
            "partial run claims success: {line}"
        );
    }
}

#[test]
fn delivery_output_conflict_route_is_labeled_and_never_published_in_colorless_text() {
    use ccync_engine::delivery_apply::route_outcome_line;

    let line = route_outcome_line(
        &codex_route(false),
        None,
        Some("a same-name entry at this target is not owned by ccync"),
    );

    assert!(
        line.contains("conflict: a same-name entry at this target is not owned by ccync"),
        "{line}"
    );
    assert!(line.contains("selected; not published"), "{line}");
    assert!(
        !line.contains("; published;"),
        "conflict route claims publication: {line}"
    );
    assert!(
        !line.contains("write error"),
        "conflict route names a write error: {line}"
    );
    assert!(
        !line.contains('\u{1b}'),
        "route text must be colorless: {line}"
    );
}

#[test]
fn p1_standalone_real_cli_publishes_complete_loose_skill_without_manager_namespace() {
    let home = Home::new();
    let source = home.root.join("inputs").join("standalone-fixture");
    fs::create_dir_all(source.join("resources")).unwrap();
    fs::write(
        source.join("SKILL.md"),
        "---\nname: standalone-fixture\ndescription: Isolated fixture.\n---\nRead resources/data.txt.\n",
    )
    .unwrap();
    fs::write(source.join("resources/data.txt"), "complete resource\n").unwrap();
    fs::create_dir_all(home.root.join(".ccync/build")).unwrap();
    fs::write(
        home.path(".ccync/build/lock.json"),
        serde_json::json!({"_looseSkills": [{"name": "standalone-fixture", "source": "fixture", "origin": source}]}).to_string(),
    )
    .unwrap();
    fs::write(
        home.path(".ccync/config.json"),
        r#"{"selectedProjectionTargets":["codex"]}"#,
    )
    .unwrap();

    let output = home.run(&["sync", "--yes"]);
    let report = stdout(&output);
    let sync_text = format!("{}{}", report, String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.status.code(), Some(0), "{report}");
    let delivered = home.root.join(".agents/skills/standalone-fixture");
    assert_eq!(
        fs::read_to_string(delivered.join("resources/data.txt")).unwrap(),
        "complete resource\n"
    );
    assert!(delivered.join("SKILL.md").is_file());
    let sync_route = sync_text
        .lines()
        .find(|line| line.starts_with("route: codex Skill standalone-fixture;"))
        .unwrap_or_else(|| panic!("sync did not report standalone route evidence: {sync_text}"));
    assert!(sync_route.contains("standalone published"), "{sync_route}");
    assert!(
        !report.contains("ccync:standalone-fixture"),
        "standalone delivery must not claim a manager namespace: {report}"
    );
    assert!(
        !home.root.join(".agents/skills/ccync").exists(),
        "standalone delivery must not create an umbrella namespace"
    );
    let doctor = home.run(&["doctor"]);
    let doctor_text = format!(
        "{}{}",
        stdout(&doctor),
        String::from_utf8_lossy(&doctor.stderr)
    );
    let route = doctor_text
        .lines()
        .find(|line| line.starts_with("route: codex Skill standalone-fixture;"))
        .unwrap_or_else(|| {
            panic!("doctor did not report standalone route evidence: {doctor_text}")
        });
    assert!(route.contains("standalone published"), "{route}");
    assert!(route.contains("origin loose-origin:"), "{route}");
    assert!(route.contains("loader NotRun"), "{route}");
    assert!(
        !route.contains("native"),
        "standalone route claimed native delivery: {route}"
    );
}

#[test]
fn p1_standalone_real_cli_withdraws_and_restores_without_legacy_rewrite() {
    let home = Home::new();
    let source = home.root.join("inputs").join("withdraw-fixture");
    fs::create_dir_all(source.join("resources")).unwrap();
    fs::write(
        source.join("SKILL.md"),
        "---\nname: withdraw-fixture\ndescription: Withdrawal fixture.\n---\nUse resources/data.txt.\n",
    )
    .unwrap();
    fs::write(source.join("resources/data.txt"), "resource survives\n").unwrap();
    let mut lock = home.path(".ccync/build/lock.json");
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    fs::write(
        &lock,
        serde_json::json!({"_looseSkills": [{"name": "withdraw-fixture", "source": "fixture", "origin": source}]}).to_string(),
    )
    .unwrap();
    let config = home.path(".ccync/config.json");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    // Every client that reads the shared root must be selected for withdrawal.
    fs::write(
        &config,
        r#"{"selectedProjectionTargets":["codex","copilot","opencode"]}"#,
    )
    .unwrap();

    let first = home.run(&["sync", "--yes"]);
    assert_eq!(first.status.code(), Some(0), "{}", stdout(&first));
    lock = home.path(".ccync/build/lock.json");
    let destination = home.root.join(".agents/skills/withdraw-fixture");
    assert!(destination.join("resources/data.txt").is_file());

    let mut lock_state: serde_json::Value =
        serde_json::from_slice(&fs::read(&lock).unwrap()).unwrap();
    lock_state["_looseSkills"] = serde_json::json!([]);
    fs::write(&lock, lock_state.to_string()).unwrap();
    let withdrawn = home.run(&["sync", "--yes"]);
    assert_eq!(withdrawn.status.code(), Some(0), "{}", stdout(&withdrawn));
    assert!(
        !destination.exists(),
        "withdrawal left the standalone route\nwithdrawal: {}{}\nfirst: {}{}",
        stdout(&withdrawn),
        String::from_utf8_lossy(&withdrawn.stderr),
        stdout(&first),
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        !home
            .root
            .join(".agents/skills/ccync/withdraw-fixture")
            .exists(),
        "the machine phase recreated a legacy umbrella fallback"
    );

    fs::write(
        &lock,
        serde_json::json!({"_looseSkills": [{"name": "withdraw-fixture", "source": "fixture", "origin": source}]}).to_string(),
    )
    .unwrap();
    let restored = home.run(&["sync", "--yes"]);
    assert_eq!(restored.status.code(), Some(0), "{}", stdout(&restored));
    assert_eq!(
        fs::read_to_string(destination.join("resources/data.txt")).unwrap(),
        "resource survives\n"
    );
}

#[test]
fn p1_standalone_real_cli_foreign_destination_is_unresolved_and_not_success() {
    let home = Home::new();
    let source = home.root.join("inputs/foreign-fixture");
    fs::create_dir_all(&source).unwrap();
    fs::write(
        source.join("SKILL.md"),
        "---\nname: foreign-fixture\ndescription: Foreign fixture.\n---\nBody.\n",
    )
    .unwrap();
    let destination = home.root.join(".agents/skills/foreign-fixture");
    fs::create_dir_all(&destination).unwrap();
    fs::write(destination.join("user.txt"), "user-owned\n").unwrap();
    fs::create_dir_all(home.root.join(".ccync/build")).unwrap();
    fs::write(
        home.path(".ccync/build/lock.json"),
        serde_json::json!({"_looseSkills": [{"name": "foreign-fixture", "source": "fixture", "origin": source}]}).to_string(),
    )
    .unwrap();
    fs::create_dir_all(home.root.join(".ccync")).unwrap();
    fs::write(
        home.path(".ccync/config.json"),
        r#"{"selectedProjectionTargets":["codex"]}"#,
    )
    .unwrap();

    let sync = home.run(&["sync", "--yes"]);
    let report = format!("{}{}", stdout(&sync), String::from_utf8_lossy(&sync.stderr));
    assert_eq!(sync.status.code(), Some(1), "{report}");
    assert!(report.contains("identity unresolved"), "{report}");
    assert!(!stdout(&sync).contains("Successfully rebuilt"), "{report}");
    assert_eq!(
        fs::read_to_string(destination.join("user.txt")).unwrap(),
        "user-owned\n"
    );
    assert!(!destination.join("SKILL.md").exists());
}

/// A machine whose personal plugin has a verified package name `Impeccable`
/// under the management ID `plugin`: the catalog entry and the lock record
/// both carry the management ID as their selector.
fn seed_impeccable_management_id(home: &Home) {
    const SHA: &str = "d631a8827f99414d2b6daba4ef08b7f8701751d7";
    let write = |relative: &str, body: &str| {
        let path = home.root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    };
    write(
        ".ccync/plugins.json",
        r#"{"plugins":[{"pluginId":"plugin","displayName":"plugin","packageName":"Impeccable","source":"https://github.com/pbakaus/impeccable.git","installStrategy":"personal-git-clone","pin":"d631a8827f99414d2b6daba4ef08b7f8701751d7"}]}"#,
    );
    write(
        ".ccync/build/lock.json",
        &format!(
            r#"{{"_personalPlugins":[{{"pluginId":"plugin","source":"https://github.com/pbakaus/impeccable.git","pin":"{SHA}"}}]}}"#
        ),
    );
}

#[test]
fn p7_t14_show_names_canonical_package_before_management_id() {
    let home = Home::new();
    seed_impeccable_management_id(&home);

    let output = home.run(&["show", "plugin"]);

    assert!(
        output.status.success(),
        "show by management ID must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = stdout(&output);
    assert!(
        text.starts_with("Plugin: Impeccable | management ID: plugin\n"),
        "package identity must lead, management ID follows: {text}"
    );
}

#[test]
fn p7_t14_show_keeps_management_lookup_and_refuses_the_package_name() {
    let home = Home::new();
    seed_impeccable_management_id(&home);

    // Lookup stays keyed by the management ID; the display name is not a selector.
    let output = home.run(&["show", "Impeccable"]);

    assert!(
        !output.status.success(),
        "a display name must not select the plugin: {}",
        stdout(&output)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("'Impeccable' not found"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn p7_t14_list_suggests_the_management_id_beside_the_package_name() {
    let home = Home::new();
    seed_impeccable_management_id(&home);

    let output = home.run(&["list"]);

    assert!(output.status.success(), "{}", stdout(&output));
    let text = stdout(&output);
    assert!(
        text.contains(
            "Package: Impeccable | management ID: plugin (remove with: ccync remove plugin)"
        ),
        "list must name the package and suggest the management ID: {text}"
    );
    assert!(
        !text.contains("remove with: ccync remove Impeccable"),
        "a display name must never become a selector: {text}"
    );
}

#[test]
fn p7_t14_remove_accepts_the_management_id_and_refuses_the_package_name() {
    let home = Home::new();
    seed_impeccable_management_id(&home);
    let catalog = home.path(".ccync/plugins.json");
    let before = fs::read(&catalog).unwrap();

    // Without --yes the run asks for confirmation and stdin is closed, so
    // nothing is removed; the only thing under test is which selector resolves.
    let by_id = home.run(&["remove", "plugin"]);
    let by_id_err = String::from_utf8_lossy(&by_id.stderr).into_owned();
    assert!(
        !by_id_err.contains("not found"),
        "remove must resolve the management ID: {by_id_err}"
    );

    let by_package = home.run(&["remove", "Impeccable"]);
    assert!(
        !by_package.status.success(),
        "a display name must not select the plugin for removal"
    );
    assert_eq!(
        fs::read(&catalog).unwrap(),
        before,
        "an unconfirmed remove must leave the catalog unchanged"
    );
}

#[test]
fn p7_t14_unsupported_tool_upgrade_stays_blocked_and_mutates_nothing() {
    // D-09: ccync does not upgrade external packages. An untracked tool has
    // no adapter to run, so `upgrade tool:<id>` exits 3 (Blocked) and claims
    // no upgrade.
    let home = Home::new();

    let output = home.run(&["upgrade", "tool:graphify"]);

    assert_eq!(output.status.code(), Some(3), "{}", stdout(&output));
    let text = format!(
        "{}{}",
        stdout(&output),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!text.contains("upgraded"), "{text}");
    assert!(!text.contains("success"), "{text}");
}

/// The public verbs `ccync --help` lists. Native delivery adds none, so this set is the
/// contract the help output must keep.
const PUBLIC_VERBS: &[&str] = &[
    "init",
    "sync",
    "add",
    "remove",
    "list",
    "doctor",
    "backup",
    "restore",
    "uninstall",
    "update",
    "upgrade",
    "show",
    "cleanup",
    "pin",
    "search",
    "repair",
    "roster",
    "tool",
    "outdated",
];

/// The migrated record: the canonical native selector for the management ID,
/// written through the engine's lifecycle section the way a committed migration
/// leaves it. The legacy `plugin@ccync-plugin` record is already gone.
fn seed_migrated_record(home: &Home) {
    use ccync_foundation::provider::{
        CapabilityInventory, ClientScope, CommittedOwnership, EnablementIntent, GenerationId,
        NativeSelector, PackageDigest, ProviderGeneration, ProviderLifecycleLock,
    };
    use ccync_foundation::target::ProjectionTargetId;

    seed_impeccable_management_id(home);
    let path = home.path(".ccync/build/lock.json");
    let mut lock: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let digest = |fill: char| PackageDigest::new(format!("sha256:{}", fill.to_string().repeat(64)));
    let record = CommittedOwnership {
        target: ProjectionTargetId::Claude,
        selector: NativeSelector::new("Impeccable@ccync-Impeccable").unwrap(),
        plugin_name: "Impeccable".to_string(),
        upstream_revision: Some("d631a8827f99414d2b6daba4ef08b7f8701751d7".to_string()),
        generation: ProviderGeneration {
            source: ccync_foundation::provider::ProviderSourceKey::new("plugin").unwrap(),
            generation: GenerationId::new("impeccable-gen").unwrap(),
            package_digest: digest('a').unwrap(),
        },
        capabilities: CapabilityInventory::new(),
        client_scope: Some(ClientScope::new("ccync-claude").unwrap()),
        source_digest: Some(digest('b').unwrap()),
        view_digest: Some(digest('c').unwrap()),
        observed_preimage: None,
        enablement_intent: Some(EnablementIntent::Enabled),
        group_substep: None,
    };
    let section = ProviderLifecycleLock {
        committed: vec![record],
        ..Default::default()
    };
    section.write_to_lock(&mut lock).unwrap();
    fs::write(path, serde_json::to_string_pretty(&lock).unwrap()).unwrap();
    // An initialized machine, so remove reaches its confirmed path.
    fs::write(
        home.path(".ccync/config.json"),
        r#"{"selectedProjectionTargets":[]}"#,
    )
    .unwrap();
}

#[test]
fn p7_t14_public_verb_set_is_unchanged() {
    let home = Home::new();

    let output = home.run(&["--help"]);

    assert!(output.status.success(), "{}", stdout(&output));
    let text = stdout(&output);
    let verbs: Vec<&str> = text
        .lines()
        .skip_while(|line| *line != "Commands:")
        .skip(1)
        .take_while(|line| !line.trim().is_empty())
        .map(str::trim)
        .collect();
    assert_eq!(verbs, PUBLIC_VERBS, "the public verb set changed: {text}");
}

#[test]
fn p7_t14_migrated_record_keeps_management_id_for_list_show_and_remove() {
    let home = Home::new();
    seed_migrated_record(&home);

    let list = home.run(&["list"]);
    assert!(list.status.success(), "{}", stdout(&list));
    assert!(
        stdout(&list).contains(
            "Package: Impeccable | management ID: plugin (remove with: ccync remove plugin)"
        ),
        "list must suggest the management ID for the migrated record: {}",
        stdout(&list)
    );
    assert!(
        stdout(&list).contains("loader: Impeccable@ccync-Impeccable"),
        "list must show the migrated native selector: {}",
        stdout(&list)
    );

    let show = home.run(&["show", "plugin"]);
    assert!(show.status.success(), "{}", stdout(&show));
    assert!(
        stdout(&show).contains("Plugin: Impeccable | management ID: plugin\n"),
        "show must keep the management ID beside the package: {}",
        stdout(&show)
    );

    let remove = home.run(&["remove", "plugin", "--yes"]);
    assert!(remove.status.success(), "{}", stdout(&remove));
    assert!(
        stdout(&remove).contains("Plugin 'plugin' removed"),
        "a confirmed remove by management ID must remove it: {}",
        stdout(&remove)
    );
}
