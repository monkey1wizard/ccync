use ccync_engine::delivery_apply::UnavailableNativeRunner;
use ccync_engine::install::run_update_with_evidence_using;
use ccync_foundation::provider::ProviderLifecycleJournal;
use std::path::Path;
mod support;
use support::isolated;

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
        &plugin.join("agents/helper.md"),
        "---\nname: helper\ndescription: Helper agent.\n---\nBody.\n",
    );
    write(
        &plugin.join("commands/audit.md"),
        "---\ndescription: Audit command.\n---\nInspect $ARGUMENTS\n",
    );
    write(
        &home.join(".ccync/build/lock.json"),
        r#"{"resolvedPlugins":[{"pluginId":"example","resolvedSource":"example","installStrategy":"bundled-local","sha":"0123456789abcdef0123456789abcdef01234567"}]}"#,
    );
    write(
        &home.join(".ccync/config.json"),
        &format!(r#"{{"selectedProjectionTargets":[{targets}]}}"#),
    );
}

#[test]
fn publish_direct_surfaces_and_records_cutover() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(
        root,
        r#""claude","codex","copilot","opencode","agy-cli","agy-ide","agy-gui""#,
    );
    run_update_with_evidence_using(
        &ccync_engine::config::CcyncConfig::default(),
        &UnavailableNativeRunner,
    )
    .expect("ordinary sync");

    assert!(root.join(".claude/skills/example/SKILL.md").is_file());
    assert!(root.join(".agents/skills/example/SKILL.md").is_file());
    assert!(root.join(".copilot/skills/example/SKILL.md").is_file());
    assert!(!root
        .join(".config/opencode/skills/example/SKILL.md")
        .exists());
    assert!(root
        .join(".gemini/antigravity-cli/skills/example/SKILL.md")
        .is_file());
    assert!(root
        .join(".gemini/config/skills/example/SKILL.md")
        .is_file());
    assert!(root.join(".claude/agents/helper.md").is_file());
    assert!(root.join(".codex/agents/helper.toml").is_file());
    assert!(root.join(".agents/skills/audit/SKILL.md").is_file());

    // All seven targets published and had no combined copy to retire, so
    // retirement finished in this run and cleared the cutover fields.
    let parsed = journal(root);
    assert!(parsed.cutover_targets.is_none());
    assert!(parsed.combined_entries_and_link_to_retire.is_none());
    assert!(parsed.redundant_managed_copy_paths.is_none());
}

#[test]
fn final_route_publication_commits_one_record_per_destination() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""codex","copilot""#);

    let outcome = run_update_with_evidence_using(
        &ccync_engine::config::CcyncConfig::default(),
        &UnavailableNativeRunner,
    )
    .expect("sync publishes the final direct routes");

    let destinations = [
        root.join(".agents/skills/example/SKILL.md"),
        root.join(".copilot/skills/example/SKILL.md"),
    ];
    for destination in &destinations {
        assert!(destination.is_file(), "{}", destination.display());
    }
    use ccync_foundation::target::ProjectionTargetId as Target;
    for target in [Target::Codex, Target::Copilot] {
        let route = route_fact(&outcome.routes, target, "example");
        assert!(route.published && route.owned, "{route:?}");
    }
    assert!(outcome.fault.is_none(), "{:?}", outcome.fault);
}

/// The route fact for one component on one target.
fn route_fact<'a>(
    routes: &'a [ccync_engine::managed_view::RouteEvidence],
    target: ccync_foundation::target::ProjectionTargetId,
    name: &str,
) -> &'a ccync_engine::managed_view::RouteEvidence {
    routes
        .iter()
        .find(|route| route.target == target && route.original_name == name)
        .unwrap_or_else(|| panic!("no route for {target} {name}: {routes:#?}"))
}

#[test]
fn publish_command_to_skill_routes_report_published_with_origin() {
    use ccync_foundation::target::ProjectionTargetId as Target;
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, ALL_TARGETS);

    let outcome = rollout().expect("ordinary sync");

    // Codex is served by the converted copy in the shared skill root, and
    // Copilot gets its own converted copy.
    assert!(root.join(".agents/skills/audit/SKILL.md").is_file());
    assert!(root.join(".copilot/skills/audit/SKILL.md").is_file());
    let codex = route_fact(&outcome.routes, Target::Codex, "audit");
    assert!(codex.command_to_skill, "{codex:?}");
    assert!(codex.published, "{codex:?}");
    assert!(codex.owned, "{codex:?}");
    assert_eq!(codex.served_by_root.as_deref(), Some("~/.agents/skills"));
    let copilot = route_fact(&outcome.routes, Target::Copilot, "audit");
    assert!(copilot.command_to_skill && copilot.published, "{copilot:?}");
    assert_eq!(copilot.path.as_deref(), Some("~/.copilot/skills/audit"));
    assert!(copilot.multi_root, "{copilot:?}");
    // OpenCode keeps its native command and also discovers the converted copy.
    let opencode = route_fact(&outcome.routes, Target::Opencode, "audit");
    assert!(
        !opencode.command_to_skill && opencode.published,
        "{opencode:?}"
    );
    assert!(opencode.multi_root, "{opencode:?}");
    // The AGY faces have no command-to-skill surface: never published, and
    // the unsupported command is reported, not dropped.
    for target in [Target::AgyCli, Target::AgyIde, Target::AgyGui] {
        let agy = route_fact(&outcome.routes, target, "audit");
        assert!(!agy.published && agy.path.is_none(), "{agy:?}");
    }
    let warnings = outcome.evidence.run_warnings.join("\n");
    assert!(
        warnings.contains("agy-cli provider example Command audit: no command-to-skill surface"),
        "{warnings}"
    );
    // Every route carries its origin.
    let line = ccync_engine::managed_view::route_evidence_line(codex);
    assert!(line.contains("origin example at "), "{line}");
    assert!(line.contains("route command-to-skill"), "{line}");
}

#[test]
fn publish_unconvertible_command_is_unsupported_and_writes_no_skill_copy() {
    use ccync_foundation::target::ProjectionTargetId as Target;
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""codex","copilot""#);
    write(
        &root.join(".ccync/plugins/example/commands/audit.md"),
        "---\ndescription: Audit command.\n---\nInspect $1\n",
    );

    let outcome = rollout().expect("ordinary sync");

    assert!(!root.join(".agents/skills/audit").exists());
    assert!(!root.join(".copilot/skills/audit").exists());
    for target in [Target::Codex, Target::Copilot] {
        let route = route_fact(&outcome.routes, target, "audit");
        assert!(!route.published && !route.command_to_skill, "{route:?}");
    }
    let warnings = outcome.evidence.run_warnings.join("\n");
    assert!(
        warnings.contains("codex provider example Command audit: command has no equivalent skill"),
        "{warnings}"
    );
    // The command's own failure does not unpublish the other routes.
    let skill = route_fact(&outcome.routes, Target::Copilot, "example");
    assert!(skill.published, "{skill:?}");
}

#[test]
fn publish_failed_write_unpublishes_only_its_own_route() {
    use ccync_foundation::target::ProjectionTargetId as Target;
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""copilot""#);
    // A regular file at the agents root makes only the agent write fail.
    write(&root.join(".copilot/agents"), "foreign\n");

    let outcome = rollout().expect("ordinary sync");

    let agent = route_fact(&outcome.routes, Target::Copilot, "helper");
    assert!(!agent.published, "{agent:?}");
    assert!(agent.failure_reason.is_some(), "{agent:?}");
    let line = ccync_engine::managed_view::route_evidence_line(agent);
    assert!(
        line.contains("action: fix the reported write error ("),
        "{line}"
    );
    let skill = route_fact(&outcome.routes, Target::Copilot, "example");
    assert!(
        skill.published && skill.failure_reason.is_none(),
        "{skill:?}"
    );
    let command = route_fact(&outcome.routes, Target::Copilot, "audit");
    assert!(command.published, "{command:?}");
}

#[test]
fn final_route_publication_runs_once_per_alias() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude","codex","opencode""#);
    let outcome = run_update_with_evidence_using(
        &ccync_engine::config::CcyncConfig::default(),
        &UnavailableNativeRunner,
    )
    .expect("ordinary sync");
    eprintln!("{}", outcome.evidence.run_warnings.join("\n"));
    assert!(root.join(".claude/skills/example/SKILL.md").is_file());
    assert!(root.join(".agents/skills/example/SKILL.md").is_file());
    assert!(!root.join(".config/opencode/skills/example").exists());
    let records = projection::direct_records(&root.join(".ccync/build/lock.json"))
        .into_iter()
        .filter(|record| {
            record
                .path
                .to_string_lossy()
                .replace('\\', "/")
                .to_ascii_lowercase()
                .contains("/skills/example")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        records.len(),
        2,
        "the two physical destinations have two proofs"
    );
    let mut recorded_paths = records
        .iter()
        .map(|record| record.path.to_string_lossy().to_ascii_lowercase())
        .collect::<Vec<_>>();
    recorded_paths.sort();
    recorded_paths.dedup();
    assert_eq!(
        recorded_paths.len(),
        records.len(),
        "each physical destination is recorded once despite the Codex and OpenCode alias"
    );
}

#[test]
fn publish_ordinary_sync_does_not_stage_provider_generations() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude""#);
    run_update_with_evidence_using(
        &ccync_engine::config::CcyncConfig::default(),
        &UnavailableNativeRunner,
    )
    .expect("ordinary sync");
    // Ordinary sync now publishes direct surfaces and retires the combined
    // Claude copy in the same run.
    assert!(root.join(".claude/skills/example/SKILL.md").is_file());
    assert!(!root.join(".claude/skills/ccync").exists());
    assert!(!claude_plugin_names(root).contains(&"ccync".to_string()));
}

#[test]
fn publish_blocked_claude_route_keeps_opencode_off_the_claude_skill_root() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude","opencode""#);
    // A second source supplies the same plugin name. Claude rejects that
    // namespace, so every Claude route blocks, including the one that would
    // have written into the shared Claude skill root.
    let second = root.join(".ccync/plugins/example-two");
    write(
        &second.join(".claude-plugin/plugin.json"),
        r#"{"name":"example","version":"1.0.0"}"#,
    );
    write(
        &second.join("skills/extra/SKILL.md"),
        "---\nname: extra\ndescription: Extra skill.\n---\nBody.\n",
    );
    write(
        &root.join(".ccync/build/lock.json"),
        r#"{"resolvedPlugins":[{"pluginId":"example","resolvedSource":"example","installStrategy":"bundled-local","sha":"0123456789abcdef0123456789abcdef01234567"},{"pluginId":"example-two","resolvedSource":"example-two","installStrategy":"bundled-local","sha":"89abcdef0123456789abcdef0123456789abcdef"}]}"#,
    );
    run_update_with_evidence_using(
        &ccync_engine::config::CcyncConfig::default(),
        &UnavailableNativeRunner,
    )
    .expect("ordinary sync");

    assert!(!root.join(".claude/skills/example/SKILL.md").exists());
    assert!(root
        .join(".config/opencode/skills/example/SKILL.md")
        .is_file());
}

fn journal(root: &Path) -> ProviderLifecycleJournal {
    let text = std::fs::read_to_string(root.join(".ccync/build/projection.txn.json")).unwrap();
    ProviderLifecycleJournal::from_json_str(&text).unwrap()
}

/// Names of the plugins listed in the combined Claude marketplace manifest.
fn claude_plugin_names(root: &Path) -> Vec<String> {
    let path = root.join(".ccync/plugins/.claude-plugin/marketplace.json");
    let text = std::fs::read_to_string(path).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    value["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|plugin| plugin["name"].as_str().unwrap().to_string())
        .collect()
}

fn rollout(
) -> Result<ccync_engine::install::CanonicalRunOutcome, ccync_engine::install::InstallError> {
    run_update_with_evidence_using(
        &ccync_engine::config::CcyncConfig::default(),
        &UnavailableNativeRunner,
    )
}

#[test]
fn retire_combined_claude_surface_after_direct_publish() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude""#);
    // A combined copy from before this build, so the first sync has
    // something to retire.
    combined_copy(root, ".claude/skills/ccync");
    write(
        &root.join(".ccync/plugins/.claude-plugin/marketplace.json"),
        r#"{"plugins":[{"name":"ccync"}]}"#,
    );
    assert!(claude_plugin_names(root).contains(&"ccync".to_string()));

    rollout().expect("ordinary sync");

    assert!(root.join(".claude/skills/example/SKILL.md").is_file());
    assert!(!root.join(".claude/skills/ccync").exists());
    assert!(!claude_plugin_names(root).contains(&"ccync".to_string()));
    let parsed = journal(root);
    assert!(parsed.cutover_targets.is_none());
    assert!(parsed.combined_entries_and_link_to_retire.is_none());
    assert!(parsed.redundant_managed_copy_paths.is_none());
}

#[test]
fn retire_keeps_combined_claude_surface_when_user_registers_ccync() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude""#);
    run_update_with_evidence_using(
        &ccync_engine::config::CcyncConfig::default(),
        &UnavailableNativeRunner,
    )
    .expect("ordinary sync");
    let registration = root.join(".claude/plugins/installed_plugins.json");
    write(
        &registration,
        r#"{"version":2,"plugins":{"ccync@ccync":[{"version":"1.0.0"}]}}"#,
    );
    let before = std::fs::read(&registration).unwrap();

    let outcome = rollout().expect("rollout sync");

    assert_eq!(std::fs::read(&registration).unwrap(), before);
    assert!(root.join(".claude/skills/ccync").exists());
    assert!(claude_plugin_names(root).contains(&"ccync".to_string()));
    let warnings = outcome.evidence.run_warnings.join("\n");
    assert!(warnings.contains("registered by the user"), "{warnings}");
    assert!(
        warnings.contains("`claude plugin uninstall ccync@ccync`"),
        "{warnings}"
    );
    // The registration records no scope, so no placeholder is left behind.
    assert!(!warnings.contains("<scope>"), "{warnings}");
    assert!(journal(root).cutover_targets.is_some());
}

#[test]
fn retire_keeps_combined_surface_for_target_with_failed_publication() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude""#);
    run_update_with_evidence_using(
        &ccync_engine::config::CcyncConfig::default(),
        &UnavailableNativeRunner,
    )
    .expect("ordinary sync");
    // A regular file at the agents root makes the direct agent write fail.
    let agents = root.join(".claude/agents");
    if agents.is_dir() {
        std::fs::remove_dir_all(&agents).unwrap();
    }
    write(&agents, "foreign\n");

    let outcome = rollout().expect("rollout sync");

    assert!(root.join(".claude/agents").is_file());
    assert!(root.join(".claude/skills/ccync").exists());
    assert!(claude_plugin_names(root).contains(&"ccync".to_string()));
    let warnings = outcome.evidence.run_warnings.join("\n");
    assert!(warnings.contains("direct publication failed"), "{warnings}");
    assert!(warnings.contains("ccync sync"), "{warnings}");
}

#[test]
fn interrupted_publication_converges_on_next_sync() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude""#);
    run_update_with_evidence_using(
        &ccync_engine::config::CcyncConfig::default(),
        &UnavailableNativeRunner,
    )
    .expect("ordinary sync");
    // The first sync already retired the combined copy, so recreate it to
    // stage the interrupted state.
    combined_copy(root, ".claude/skills/ccync");
    // A directory where the marketplace file belongs makes the combined
    // retirement write fail after the direct copy was published.
    let manifest = root.join(".ccync/plugins/.claude-plugin/marketplace.json");
    std::fs::remove_file(&manifest).ok();
    std::fs::create_dir_all(&manifest).unwrap();

    let first = rollout().expect("interrupted rollout sync");

    assert!(root.join(".claude/skills/example/SKILL.md").is_file());
    assert!(root.join(".claude/skills/ccync").exists());
    assert!(journal(root).cutover_targets.is_some());
    let warnings = first.evidence.run_warnings.join("\n");
    assert!(warnings.contains("may coexist"), "{warnings}");
    assert!(warnings.contains("ccync sync"), "{warnings}");
    assert!(warnings.contains("restarting agents"), "{warnings}");

    std::fs::remove_dir(&manifest).unwrap();
    rollout().expect("converging rollout sync");

    assert!(!root.join(".claude/skills/ccync").exists());
    assert!(!claude_plugin_names(root).contains(&"ccync".to_string()));
    assert!(journal(root).cutover_targets.is_none());
    let copies = std::fs::read_dir(root.join(".claude/skills"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name() == "example")
        .count();
    assert_eq!(copies, 1);
}

const ALL_TARGETS: &str = r#""claude","codex","copilot","opencode","agy-cli","agy-ide","agy-gui""#;

/// The combined `ccync` copy in each skill root the seven targets publish into.
const COMBINED_COPIES: [&str; 6] = [
    ".claude/skills/ccync",
    ".agents/skills/ccync",
    ".copilot/skills/ccync",
    ".config/opencode/skills/ccync",
    ".gemini/antigravity-cli/skills/ccync",
    ".gemini/config/skills/ccync",
];

/// Link a combined `ccync` copy to the canonical render root, the shape a
/// combined copy has on disk.
fn combined_copy(root: &Path, rel: &str) {
    let render = root.join(".ccync").join("build").join("render");
    std::fs::create_dir_all(&render).unwrap();
    // `mklink` reads a `/` in its path as a switch, so use native separators.
    let link = root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    ccync_foundation::platform::create_dir_link(&render, &link).unwrap();
}

#[test]
fn retire_combined_copies_for_all_seven_targets() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, ALL_TARGETS);
    for rel in COMBINED_COPIES {
        combined_copy(root, rel);
    }

    rollout().expect("rollout sync");

    for rel in COMBINED_COPIES {
        assert!(!root.join(rel).exists(), "{rel} must be retired");
    }
    assert!(root.join(".copilot/skills/example/SKILL.md").is_file());
    assert!(root
        .join(".gemini/config/skills/example/SKILL.md")
        .is_file());
    assert!(journal(root).cutover_targets.is_none());
}

#[test]
fn failed_copilot_and_agy_cli_keep_combined_copies_on_private_roots() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, ALL_TARGETS);
    for rel in COMBINED_COPIES {
        combined_copy(root, rel);
    }
    // Regular files at the agents roots make the direct agent writes fail.
    // AGY GUI shares the `.gemini/config/agents` root with AGY CLI, so its
    // write fails too and its combined copy stays.
    write(&root.join(".copilot/agents"), "foreign\n");
    write(&root.join(".gemini/config/agents"), "foreign\n");

    let outcome = rollout().expect("rollout sync");

    // Private roots of the failed targets keep their combined copies.
    assert!(present(&root.join(".copilot/skills/ccync")));
    assert!(present(&root.join(".gemini/antigravity-cli/skills/ccync")));
    // `.gemini/config/skills` is a shared root. Its direct copy was written,
    // so its combined copy retires although AGY GUI failed on its agents.
    assert!(root
        .join(".gemini/config/skills/example/SKILL.md")
        .is_file());
    assert!(!present(&root.join(".gemini/config/skills/ccync")));
    assert!(!present(&root.join(".agents/skills/ccync")));
    assert!(!present(&root.join(".claude/skills/ccync")));
    assert!(!present(&root.join(".config/opencode/skills/ccync")));
    let warnings = outcome.evidence.run_warnings.join("\n");
    assert!(warnings.contains("direct publication failed"), "{warnings}");
    use ccync_engine::install::RouteRetirement;
    use ccync_foundation::target::ProjectionTargetId as Target;
    for target in [Target::Copilot, Target::AgyCli, Target::AgyGui] {
        assert!(
            matches!(
                outcome.route_retirements.get(&target),
                Some(RouteRetirement::Kept { reason }) if reason == "direct publication failed"
            ),
            "{target}: {:?}",
            outcome.route_retirements
        );
    }
    assert_eq!(
        outcome.route_retirements.get(&Target::AgyIde),
        Some(&RouteRetirement::Done)
    );
    assert!(journal(root).cutover_targets.is_some());
}

#[test]
fn retire_keeps_combined_surface_for_target_with_blocked_route() {
    use ccync_engine::install::RouteRetirement;
    use ccync_foundation::target::ProjectionTargetId as Target;
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""copilot","opencode""#);
    combined_copy(root, ".copilot/skills/ccync");
    combined_copy(root, ".config/opencode/skills/ccync");
    owned_copilot_plugin_root(root);
    // A user's same-name entry in Copilot's command-to-skill root forces an
    // alias the converted command cannot take, so the route is blocked.
    write(&root.join(".copilot/skills/audit/SKILL.md"), "user\n");

    let outcome = rollout().expect("ordinary sync");

    assert!(
        outcome
            .route_conflicts
            .iter()
            .any(|conflict| conflict.target == Target::Copilot && conflict.component == "audit"),
        "{:?}",
        outcome.route_conflicts
    );
    // Copilot keeps its combined copy and its combined plugin root.
    assert!(root.join(".copilot/skills/example/SKILL.md").is_file());
    assert!(present(&root.join(".copilot/skills/ccync")));
    assert!(present(&copilot_plugin_root(root)));
    assert_eq!(
        std::fs::read_to_string(root.join(".copilot/skills/audit/SKILL.md")).unwrap(),
        "user\n"
    );
    // OpenCode published everything, so its combined copy retires.
    assert!(!present(&root.join(".config/opencode/skills/ccync")));
    assert_eq!(
        outcome.route_retirements.get(&Target::Opencode),
        Some(&RouteRetirement::Done)
    );
    match outcome.route_retirements.get(&Target::Copilot) {
        Some(RouteRetirement::Kept { reason }) => {
            assert!(
                reason.contains("discoverable without a user step") && reason.contains("audit"),
                "{reason}"
            );
        }
        other => panic!("copilot retirement: {other:?}"),
    }
    let warnings = outcome.evidence.run_warnings.join("\n");
    assert!(
        warnings.contains("copilot: Command audit has a blocked route"),
        "{warnings}"
    );
    assert!(journal(root).cutover_targets.is_some());
}

#[test]
fn retire_reports_matched_registration_key_and_scope_in_uninstall_command() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude""#);
    rollout().expect("ordinary sync");
    let registration = root.join(".claude/plugins/installed_plugins.json");
    write(
        &registration,
        r#"{"version":2,"plugins":{"example@ccync":[{"scope":"user","version":"1.0.0"}],"other@elsewhere":[{"scope":"user"}]}}"#,
    );

    let outcome = rollout().expect("rollout sync");

    let warnings = outcome.evidence.run_warnings.join("\n");
    assert!(
        warnings.contains("`claude plugin uninstall example@ccync --scope user`"),
        "{warnings}"
    );
    assert!(!warnings.contains("<scope>"), "{warnings}");
    assert!(!warnings.contains("<plugin>"), "{warnings}");
    assert!(!warnings.contains("other@elsewhere"), "{warnings}");
}

#[test]
fn publish_reports_unsupported_hooks_for_selected_targets_only() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude","copilot""#);
    write(
        &root.join(".ccync/plugins/example/hooks/hooks.json"),
        r#"{"hooks":{"SessionStart":[]}}"#,
    );

    let outcome = rollout().expect("ordinary sync");

    let warnings = outcome.evidence.run_warnings.join("\n");
    for target in ["claude", "copilot"] {
        assert!(
            warnings.contains(&format!("{target} provider example hooks: unsupported")),
            "{warnings}"
        );
    }
    for target in ["codex", "opencode", "agy-cli", "agy-ide", "agy-gui"] {
        assert!(
            !warnings.contains(&format!("{target} provider example hooks: unsupported")),
            "{warnings}"
        );
    }
}

#[test]
fn shared_root_combined_copy_retires_once_direct_copy_is_written() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""codex","opencode""#);
    combined_copy(root, ".agents/skills/ccync");

    rollout().expect("rollout sync");

    assert!(root.join(".agents/skills/example/SKILL.md").is_file());
    assert!(!root.join(".agents/skills/ccync").exists());
    assert!(!root.join(".config/opencode/skills/ccync").exists());
    assert!(journal(root).cutover_targets.is_none());
}

#[test]
fn shared_root_retires_despite_unrelated_private_failure() {
    use ccync_engine::install::RouteRetirement;
    use ccync_foundation::target::ProjectionTargetId as Target;
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""codex","copilot""#);
    combined_copy(root, ".agents/skills/ccync");
    combined_copy(root, ".copilot/skills/ccync");
    // A regular file at Copilot's agents root fails only Copilot's agent write.
    // Every route into the shared `~/.agents/skills` root still publishes.
    write(&root.join(".copilot/agents"), "foreign\n");

    let outcome = rollout().expect("rollout sync");

    assert!(root.join(".agents/skills/example/SKILL.md").is_file());
    assert!(!present(&root.join(".agents/skills/ccync")));
    // Copilot's private root keeps its combined copy as the fallback.
    assert!(root.join(".copilot/skills/example/SKILL.md").is_file());
    assert!(present(&root.join(".copilot/skills/ccync")));
    assert!(matches!(
        outcome.route_retirements.get(&Target::Copilot),
        Some(RouteRetirement::Kept { reason }) if reason == "direct publication failed"
    ));
    assert!(journal(root).cutover_targets.is_some());
}

#[test]
fn unowned_required_output_prevents_private_retirement_and_retains_fallback() {
    use ccync_engine::install::RouteRetirement;
    use ccync_foundation::target::ProjectionTargetId as Target;
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""copilot""#);
    owned_copilot_plugin_root(root);
    // Pre-create unowned agent files at the plain name and at the alias the
    // plain-name file forces, so the route's output path is unowned. The
    // writer preserves foreign files without error, so no writer failure
    // occurs, but the output is unowned in the lockfile.
    write(
        &root.join(".copilot/agents/helper.agent.md"),
        "foreign agent\n",
    );
    write(
        &root.join(".copilot/agents/example-example-helper.agent.md"),
        "foreign agent\n",
    );

    let outcome = rollout().expect("rollout sync");

    // The foreign file is preserved untouched.
    assert_eq!(
        std::fs::read_to_string(root.join(".copilot/agents/helper.agent.md")).unwrap(),
        "foreign agent\n"
    );
    // Because the required route output is unowned, direct publication failed
    // and Copilot's combined fallback plugin root is retained.
    assert!(
        present(&copilot_plugin_root(root)),
        "the combined root is retained as fallback when output is unowned"
    );
    assert!(matches!(
        outcome.route_retirements.get(&Target::Copilot),
        Some(RouteRetirement::Kept { reason }) if reason == "direct publication failed"
    ));
    let warnings = outcome.evidence.run_warnings.join("\n");
    assert!(warnings.contains("direct publication failed"), "{warnings}");
    assert!(journal(root).cutover_targets.is_some());
}

#[test]
fn foreign_same_name_directory_is_untouched() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""copilot","codex""#);
    write(&root.join(".copilot/skills/ccync/keep.txt"), "user\n");
    write(&root.join(".agents/skills/ccync/keep.txt"), "user\n");

    rollout().expect("rollout sync");

    assert!(root.join(".copilot/skills/ccync/keep.txt").is_file());
    assert!(root.join(".agents/skills/ccync/keep.txt").is_file());
}

#[test]
fn prune_removes_owned_copy_the_route_plan_no_longer_names() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""copilot""#);
    rollout().expect("first rollout sync");
    assert!(root.join(".copilot/skills/example/SKILL.md").is_file());

    std::fs::remove_dir_all(root.join(".ccync/plugins/example/skills/example")).unwrap();
    rollout().expect("second rollout sync");

    assert!(!root.join(".copilot/skills/example").exists());
}

const AGY_CLI_PLUGIN_ROOT: &str = ".gemini/antigravity-cli/plugins/ccync";
const AGY_IDE_PLUGIN_ROOT: &str = ".gemini/antigravity-ide/plugins/ccync";
const AGY_GUI_PLUGIN_ROOT: &str = ".gemini/config/plugins/ccync";

/// Whether anything, a link included, sits at `path`.
fn present(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

fn copilot_plugin_root(root: &Path) -> std::path::PathBuf {
    root.join(".copilot/installed-plugins/ccync-copilot/ccync")
}

/// Link a plugin root to the canonical render root, the shape a combined
/// plugin root has on disk.
fn plugin_root_link(root: &Path, path: &Path) -> std::path::PathBuf {
    // `mklink` reads a `/` in its path as a switch, so use native separators.
    let path = std::path::PathBuf::from(
        path.to_string_lossy()
            .replace('/', std::path::MAIN_SEPARATOR_STR),
    );
    let render = root.join(".ccync").join("build").join("render");
    std::fs::create_dir_all(&render).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    ccync_foundation::platform::create_dir_link(&render, &path).unwrap();
    path
}

/// A combined AGY plugin root that the lockfile records as ccync-owned.
fn owned_agy_plugin_root(root: &Path, rel: &str) {
    let link = plugin_root_link(root, &root.join(rel));
    projection::AgyRootOwnership::from_lockfile(&root.join(".ccync/build/lock.json"))
        .record_now(&link, false)
        .unwrap();
}

/// The Copilot combined plugin root, with the state file that records it as
/// ccync's own.
fn owned_copilot_plugin_root(root: &Path) {
    let link = plugin_root_link(root, &copilot_plugin_root(root));
    write(
        &root.join(".ccync/dist/targets/copilot/managed.json"),
        &serde_json::json!({ "projectionRoot": link }).to_string(),
    );
}

/// The `projectionRoot` the Copilot state file records, read as JSON.
fn copilot_projection_root_record(root: &Path) -> serde_json::Value {
    let text =
        std::fs::read_to_string(root.join(".ccync/dist/targets/copilot/managed.json")).unwrap();
    serde_json::from_str::<serde_json::Value>(&text).unwrap()["projectionRoot"].clone()
}

#[test]
fn retire_combined_plugin_roots_after_direct_publish_and_stay_gone() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""copilot","agy-cli","agy-ide","agy-gui""#);
    owned_copilot_plugin_root(root);
    owned_agy_plugin_root(root, AGY_CLI_PLUGIN_ROOT);
    owned_agy_plugin_root(root, AGY_IDE_PLUGIN_ROOT);
    owned_agy_plugin_root(root, AGY_GUI_PLUGIN_ROOT);

    let first = rollout().expect("first sync");

    assert!(root.join(".copilot/skills/example/SKILL.md").is_file());
    assert!(!present(&copilot_plugin_root(root)));
    assert!(!present(&root.join(AGY_CLI_PLUGIN_ROOT)));
    assert!(!present(&root.join(AGY_IDE_PLUGIN_ROOT)));
    assert!(!present(&root.join(AGY_GUI_PLUGIN_ROOT)));
    assert!(copilot_projection_root_record(root).is_null());
    let warnings = first.evidence.run_warnings.join("\n");
    assert!(!warnings.contains("plugin root"), "{warnings}");
    assert!(journal(root).cutover_targets.is_none());

    // A repeat sync neither writes nor removes anything on those paths.
    let second = rollout().expect("repeat sync");

    assert!(!present(&copilot_plugin_root(root)));
    assert!(!present(&root.join(AGY_CLI_PLUGIN_ROOT)));
    assert!(!present(&root.join(AGY_IDE_PLUGIN_ROOT)));
    assert!(!present(&root.join(AGY_GUI_PLUGIN_ROOT)));
    let warnings = second.evidence.run_warnings.join("\n");
    assert!(!warnings.contains("plugin root"), "{warnings}");
}

#[test]
fn retire_combined_plugin_keeps_agy_gui_root_when_direct_publish_fails() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""agy-gui""#);
    owned_agy_plugin_root(root, AGY_GUI_PLUGIN_ROOT);
    // A regular file at the shared agents root makes the direct AGY GUI write fail.
    write(&root.join(".gemini/config/agents"), "foreign\n");

    let outcome = rollout().expect("rollout sync");

    assert!(
        present(&root.join(AGY_GUI_PLUGIN_ROOT)),
        "the combined root is the fallback"
    );
    let warnings = outcome.evidence.run_warnings.join("\n");
    assert!(warnings.contains("direct publication failed"), "{warnings}");
    assert!(warnings.contains("ccync sync"), "{warnings}");
    assert!(journal(root).cutover_targets.is_some());
}

#[test]
fn retire_combined_plugin_keeps_unowned_agy_gui_directory_and_reports_it() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""agy-gui""#);
    write(&root.join(AGY_GUI_PLUGIN_ROOT).join("keep.txt"), "user\n");

    let outcome = rollout().expect("rollout sync");

    assert!(root.join(AGY_GUI_PLUGIN_ROOT).join("keep.txt").is_file());
    let warnings = outcome.evidence.run_warnings.join("\n");
    assert!(warnings.contains("not ccync-owned"), "{warnings}");
}

#[test]
fn retire_combined_plugin_keeps_root_when_direct_publish_fails() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""copilot""#);
    owned_copilot_plugin_root(root);
    // A regular file at the agents root makes the direct Copilot agent write fail.
    write(&root.join(".copilot/agents"), "foreign\n");

    let outcome = rollout().expect("rollout sync");

    assert!(
        present(&copilot_plugin_root(root)),
        "the combined root is the fallback"
    );
    let warnings = outcome.evidence.run_warnings.join("\n");
    assert!(warnings.contains("direct publication failed"), "{warnings}");
    assert!(warnings.contains("ccync sync"), "{warnings}");
    assert!(journal(root).cutover_targets.is_some());
}

#[test]
fn retire_combined_plugin_keeps_unowned_agy_directory_and_reports_it() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""agy-cli""#);
    write(&root.join(AGY_CLI_PLUGIN_ROOT).join("keep.txt"), "user\n");

    let outcome = rollout().expect("rollout sync");

    assert!(root.join(AGY_CLI_PLUGIN_ROOT).join("keep.txt").is_file());
    let warnings = outcome.evidence.run_warnings.join("\n");
    assert!(warnings.contains("not ccync-owned"), "{warnings}");
}

#[test]
fn retire_combined_plugin_keeps_copilot_directory_the_state_file_does_not_record() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude""#);
    write(&copilot_plugin_root(root).join("keep.txt"), "user\n");

    let outcome = rollout().expect("rollout sync");

    assert!(copilot_plugin_root(root).join("keep.txt").is_file());
    let warnings = outcome.evidence.run_warnings.join("\n");
    assert!(warnings.contains("not ccync-owned"), "{warnings}");
}

#[test]
fn retire_combined_plugin_removes_owned_surface_of_deselected_face() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude""#);
    owned_copilot_plugin_root(root);
    owned_agy_plugin_root(root, AGY_CLI_PLUGIN_ROOT);

    rollout().expect("deselect sync");

    assert!(!present(&copilot_plugin_root(root)));
    assert!(!present(&root.join(AGY_CLI_PLUGIN_ROOT)));
    assert!(copilot_projection_root_record(root).is_null());
}

// ── native group cutover (p7_t11) ───────────────────────────────────────────
//
// A fake Claude CLI keeps its registrations in memory and loads each enabled
// registration once. No host CLI runs, and every path lives in the isolated home.

use ccync_engine::delivery_apply::preview_native_cutover;
use ccync_engine::plugin_native::{
    set_native_enablement, NativeCommandRunner, NativeExecutable, NativeRegistration,
};
use ccync_engine::plugin_render::{load_provider_inventory, render_provider_generations};
use ccync_foundation::provider::{CommittedOwnership, NativeSelector, ProviderLifecycleLock};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Default)]
struct FakeClaudeState {
    /// Marketplace name to the root it was registered from.
    marketplaces: BTreeMap<String, String>,
    /// Installed selector to its enabled flag.
    installed: BTreeMap<String, bool>,
    /// Every call that is not a read, in order.
    mutations: Vec<String>,
    /// When set, the next plugin install fails after its call is recorded.
    fail_install: bool,
    /// When set, a successful plugin install is followed by a failed listing,
    /// which is the post-install verification read.
    fail_post_install_listing: bool,
    /// Armed by a successful install; the next listing read fails once.
    listing_fails_next: bool,
    /// When set, the next plugin install replaces this journal file with a
    /// directory first, so a later journal write fails.
    journal_dir_on_install: Option<PathBuf>,
    /// When set, the next plugin install moves this lockfile aside and puts a
    /// directory in its place, so the ownership commit after a successful add fails.
    lock_dir_on_install: Option<PathBuf>,
    /// When set, the next uninstall fails after its call is recorded.
    fail_uninstall: bool,
}

/// Injected Claude adapter. Reads answer from memory, and mutations follow the
/// real argument shapes the adapter uses.
#[derive(Default)]
struct FakeClaude {
    state: RefCell<FakeClaudeState>,
    /// When set, only this adapter is verified, so the other one stays loose.
    only: Option<NativeExecutable>,
}

impl FakeClaude {
    fn mutation_count(&self) -> usize {
        self.state.borrow().mutations.len()
    }

    /// Native loads: one per enabled registration, plus any loose skill copy.
    fn loads(&self, home: &Path) -> usize {
        let native = self
            .state
            .borrow()
            .installed
            .values()
            .filter(|enabled| **enabled)
            .count();
        let loose = count_files(&home.join(".claude/agents"))
            + count_files(&home.join(".claude/commands"))
            + count_files(&home.join(".claude/skills"));
        native + loose
    }

    fn installed_count(&self) -> usize {
        self.state.borrow().installed.len()
    }
}

/// The marketplace document a root publishes, in the Claude or Codex layout.
fn marketplace_document(root: &str) -> PathBuf {
    let claude = PathBuf::from(root).join(".claude-plugin/marketplace.json");
    if claude.is_file() {
        claude
    } else {
        PathBuf::from(root).join(".agents/plugins/marketplace.json")
    }
}

/// Read the marketplace name from the document a root publishes.
fn marketplace_name(root: &str) -> Option<String> {
    let text = std::fs::read_to_string(marketplace_document(root)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value["name"].as_str().map(str::to_string)
}

impl NativeCommandRunner for FakeClaude {
    fn mutation_verified(&self, executable: NativeExecutable) -> bool {
        self.only.is_none_or(|only| only == executable)
    }

    fn run(&self, _executable: NativeExecutable, args: &[&str]) -> Result<Vec<u8>, String> {
        let mut state = self.state.borrow_mut();
        state.mutations.push(args.join(" "));
        match args {
            ["plugin", "marketplace", "add", root, ..] => {
                let name = marketplace_name(root).ok_or("marketplace document is missing")?;
                state.marketplaces.insert(name, (*root).to_string());
                Ok(Vec::new())
            }
            // Codex installs with `plugin add`; Claude with `plugin install`.
            ["plugin", "install" | "add", selector, ..] => {
                if let Some(path) = state.journal_dir_on_install.take() {
                    let _ = std::fs::remove_file(&path);
                    std::fs::create_dir_all(&path).unwrap();
                }
                if state.fail_install {
                    return Err("injected install failure".to_string());
                }
                let (plugin, marketplace) = selector.split_once('@').ok_or("bad selector")?;
                let root = state
                    .marketplaces
                    .get(marketplace)
                    .ok_or("marketplace is not registered")?
                    .clone();
                let text = std::fs::read_to_string(marketplace_document(&root))
                    .map_err(|e| e.to_string())?;
                let value: serde_json::Value =
                    serde_json::from_str(&text).map_err(|e| e.to_string())?;
                let source = value["plugins"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|entry| entry["name"] == plugin)
                    .and_then(|entry| {
                        entry["source"]
                            .as_str()
                            .or_else(|| entry["source"]["path"].as_str())
                    })
                    .ok_or("plugin is not listed")?;
                if !PathBuf::from(&root).join(source).is_dir() {
                    return Err("plugin source is missing".to_string());
                }
                if let Some(lock) = state.lock_dir_on_install.take() {
                    let held = lock.with_file_name("lock.json.held");
                    let _ = std::fs::rename(&lock, &held);
                    std::fs::create_dir_all(&lock).unwrap();
                }
                state.installed.insert((*selector).to_string(), true);
                if state.fail_post_install_listing {
                    state.listing_fails_next = true;
                }
                Ok(Vec::new())
            }
            ["plugin", verb @ ("enable" | "disable"), selector, ..] => {
                let enabled = *verb == "enable";
                match state.installed.get_mut(*selector) {
                    Some(flag) => *flag = enabled,
                    None => return Err("not installed".to_string()),
                }
                Ok(Vec::new())
            }
            ["plugin", "uninstall", selector, ..] => {
                if std::mem::take(&mut state.fail_uninstall) {
                    return Err("injected uninstall failure".to_string());
                }
                match state.installed.remove(*selector) {
                    Some(_) => Ok(Vec::new()),
                    None => Err("not installed".to_string()),
                }
            }
            _ => Err(format!("unexpected call: {}", args.join(" "))),
        }
    }

    fn observe(
        &self,
        _executable: NativeExecutable,
        selector: &NativeSelector,
    ) -> Result<NativeRegistration, String> {
        let mut state = self.state.borrow_mut();
        if std::mem::take(&mut state.listing_fails_next) {
            return Err("injected post-install listing failure".to_string());
        }
        let enabled = state.installed.get(selector.as_str()).copied();
        Ok(NativeRegistration {
            available: true,
            present: enabled.is_some(),
            enabled: enabled.unwrap_or(false),
            broken: false,
            selector: enabled.map(|_| selector.clone()),
            source: None,
            upstream_revision: None,
            package_digest: None,
            capabilities: Default::default(),
        })
    }

    fn marketplace_root(
        &self,
        _executable: NativeExecutable,
        marketplace: &str,
    ) -> Result<Option<String>, String> {
        Ok(self.state.borrow().marketplaces.get(marketplace).cloned())
    }
}

/// Committed Claude ownership rows from the isolated build lock.
fn committed_claude(root: &Path) -> Vec<CommittedOwnership> {
    let text = std::fs::read_to_string(root.join(".ccync/build/lock.json")).unwrap();
    let lock: serde_json::Value = serde_json::from_str(&text).unwrap();
    ProviderLifecycleLock::from_lock(&lock).unwrap().committed
}

/// Bytes of every file under the paths a dry run must leave alone.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if let Ok(bytes) = std::fs::read(&path) {
                out.insert(path, bytes);
            }
        }
    }
    let mut out = BTreeMap::new();
    for part in [
        ".ccync/config.json",
        ".ccync/build",
        ".claude",
        ".config/opencode",
    ] {
        let path = root.join(part);
        if path.is_file() {
            out.insert(path.clone(), std::fs::read(&path).unwrap());
        } else {
            walk(&path, &mut out);
        }
    }
    out
}

#[test]
fn p7_t11_two_isolated_syncs_yield_stable_identity_and_one_enabled_load() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude","opencode""#);
    let fake = FakeClaude::default();

    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("first sync");
    let first = committed_claude(root);
    assert_eq!(first.len(), 1, "one committed Claude registration");
    assert_eq!(fake.installed_count(), 1, "one native registration");
    assert_eq!(fake.loads(root), 1, "one enabled load after the first sync");
    assert!(
        !root.join(".claude/agents/helper.md").exists(),
        "the loose copy is retired once the native group is verified"
    );
    let after_first = fake.mutation_count();

    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("second sync");
    assert_eq!(committed_claude(root), first, "package identity is stable");
    assert_eq!(fake.installed_count(), 1, "still one native registration");
    assert_eq!(fake.loads(root), 1, "still one enabled load");
    assert_eq!(
        fake.mutation_count(),
        after_first,
        "the second sync makes no native mutation"
    );

    set_native_enablement(&fake, NativeExecutable::Claude, &first[0], false)
        .expect("disable the owned registration");
    assert_eq!(fake.loads(root), 0, "zero loads when disabled");
}

#[test]
fn p7_t11_sync_keeps_a_registration_the_user_disabled_out_of_band() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude","opencode""#);
    let fake = FakeClaude::default();

    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("first sync");
    let first = committed_claude(root);
    let selector = first[0].selector.as_str().to_string();

    // The user disables the registration outside ccync, through the native CLI.
    fake.run(
        NativeExecutable::Claude,
        &["plugin", "disable", &selector, "--scope", "user"],
    )
    .expect("out-of-band disable");
    let before_second_sync = fake.mutation_count();

    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("second sync");

    assert_eq!(
        fake.state.borrow().installed.get(&selector),
        Some(&false),
        "the registration stays disabled"
    );
    assert_eq!(fake.loads(root), 0, "zero active loads after the sync");
    let after_second_sync = fake.state.borrow().mutations[before_second_sync..].to_vec();
    assert!(
        after_second_sync
            .iter()
            .all(|call| !call.starts_with("plugin enable")),
        "no enable call was issued: {after_second_sync:?}"
    );
    assert!(
        after_second_sync.is_empty(),
        "the sync made no native mutation: {after_second_sync:?}"
    );
    let recorded = committed_claude(root);
    assert_eq!(
        recorded[0].enablement_intent,
        Some(ccync_foundation::provider::EnablementIntent::Disabled),
        "the committed ownership records the disabled state"
    );
}

#[test]
fn p7_t11_dry_run_writes_nothing_and_calls_no_mutating_host() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude","opencode""#);
    let fake = FakeClaude::default();
    // A first sync establishes the state the dry run must leave untouched.
    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("establishing sync");
    let before = snapshot(root);
    let calls_before = fake.mutation_count();

    let inventory = load_provider_inventory().expect("inventory");
    let rendered = render_provider_generations(&inventory).expect("render");
    let lines = preview_native_cutover(
        &rendered,
        &[
            ccync_foundation::target::ProjectionTargetId::Claude,
            ccync_foundation::target::ProjectionTargetId::Opencode,
        ],
        root,
        &root.join(".ccync/plugins/lock.json"),
        &fake,
    );

    assert!(
        lines
            .iter()
            .any(|line| line.contains("native activation planned")),
        "preview plans the native group: {lines:?}"
    );
    assert_eq!(
        fake.mutation_count(),
        calls_before,
        "the dry run makes no mutating host call"
    );
    assert_eq!(
        snapshot(root),
        before,
        "config, journal and registrations are unchanged"
    );
}

#[test]
fn final_route_publication_interruption_keeps_intent_and_shared_bytes() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude","opencode""#);
    let fake = FakeClaude::default();
    fake.state.borrow_mut().fail_install = true;

    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("sync reports fallback instead of failing");
    assert!(
        committed_claude(root).is_empty(),
        "no ownership is committed after a failed install"
    );
    assert!(
        root.join(".claude/agents/helper.md").is_file(),
        "the failed group keeps its loose route"
    );
    assert!(
        root.join(".claude/skills/example/SKILL.md").is_file(),
        "the shared skill copy survives the failed switch"
    );
    let intents = journal(root).intents;
    assert_eq!(intents.len(), 1, "the intent survives for recovery");
    assert_eq!(intents[0].target.as_str(), "claude");

    // The next sync recovers once the install succeeds.
    fake.state.borrow_mut().fail_install = false;
    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("recovery sync");
    assert_eq!(committed_claude(root).len(), 1, "ownership is committed");
    assert_eq!(fake.loads(root), 1, "one enabled load after recovery");
    assert!(
        journal(root).intents.is_empty(),
        "the intent clears after cleanup"
    );
}

#[test]
fn p7_t12_crash_at_verifying_substep_converges_on_retry() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude","opencode""#);
    let fake = FakeClaude::default();
    // The native add succeeds, then the post-install listing fails: the crash
    // point is between the native side effect and the ownership commit.
    fake.state.borrow_mut().fail_post_install_listing = true;

    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("sync reports fallback instead of failing");
    assert_eq!(fake.installed_count(), 1, "the native add ran");
    assert!(
        committed_claude(root).is_empty(),
        "no ownership is committed before verification"
    );
    assert!(
        root.join(".claude/agents/helper.md").is_file(),
        "the loose route survives the unverified switch"
    );
    assert_eq!(journal(root).intents.len(), 1, "the intent survives");

    // The retry verifies the registration that already exists and converges to
    // the new recorded state.
    fake.state.borrow_mut().fail_post_install_listing = false;
    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("recovery sync");
    assert_eq!(
        committed_claude(root).len(),
        1,
        "the retry commits the verified ownership"
    );
    assert_eq!(fake.installed_count(), 1, "still one native registration");
    assert_eq!(fake.loads(root), 1, "one enabled load after recovery");
    assert!(journal(root).intents.is_empty(), "the intent clears");
}

#[test]
fn p7_t12_present_registration_without_pending_intent_blocks() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude","opencode""#);
    let fake = FakeClaude::default();
    // A same-name registration that no journal intent records. ccync did not
    // install it, so it is external: it is neither adopted nor replaced.
    fake.state
        .borrow_mut()
        .installed
        .insert("example@ccync-example".to_string(), true);

    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("sync reports fallback instead of failing");
    assert_eq!(fake.mutation_count(), 0, "no native call replaces it");
    assert_eq!(fake.installed_count(), 1, "the foreign registration stays");
    assert!(
        committed_claude(root).is_empty(),
        "no ownership is committed for an unproven registration"
    );
    assert_eq!(
        journal(root).intents.len(),
        1,
        "the attempt's intent is kept as evidence"
    );
    assert!(
        root.join(".claude/agents/helper.md").is_file(),
        "the loose route survives the blocked switch"
    );
}

#[test]
fn final_route_publication_stale_evidence_blocks_retirement() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude","opencode""#);
    let fake = FakeClaude::default();
    fake.state.borrow_mut().fail_post_install_listing = true;
    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("sync reports fallback instead of failing");
    assert_eq!(fake.installed_count(), 1, "the native add ran");

    // The recorded intent proves a view digest that the installed view does
    // not produce, so the registration cannot be attributed to this intent.
    let mut recorded = journal(root);
    recorded.intents[0].view_digest = Some(
        ccync_foundation::provider::PackageDigest::new(format!("sha256:{}", "0".repeat(64)))
            .unwrap(),
    );
    std::fs::write(
        root.join(".ccync/build/projection.txn.json"),
        recorded.to_json_string().unwrap(),
    )
    .unwrap();

    let earlier_intent = journal(root).intents[0].clone();
    let earlier_view_digest = earlier_intent.view_digest.clone();
    let earlier_preimages = earlier_intent.preimage_refs.clone();
    let journal_bytes_before =
        std::fs::read(root.join(".ccync/build/projection.txn.json")).unwrap();

    fake.state.borrow_mut().fail_post_install_listing = false;
    let before = fake.mutation_count();
    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("sync reports fallback instead of failing");
    assert_eq!(fake.mutation_count(), before, "no native call on the retry");
    let retried = journal(root);
    assert_eq!(
        retried.intents.len(),
        1,
        "the blocked retry adds no second intent"
    );
    assert_eq!(
        retried.intents[0], earlier_intent,
        "the earlier intent survives the blocked retry unchanged"
    );
    assert_eq!(
        retried.intents[0].view_digest, earlier_view_digest,
        "the earlier intent keeps its view digest"
    );
    assert_eq!(
        retried.intents[0].preimage_refs, earlier_preimages,
        "the earlier intent keeps its preimage references"
    );
    assert_eq!(
        std::fs::read(root.join(".ccync/build/projection.txn.json")).unwrap(),
        journal_bytes_before,
        "the journal file is byte-identical after the blocked retry"
    );
    assert_eq!(
        fake.installed_count(),
        1,
        "the registration is left as found"
    );
    assert!(
        committed_claude(root).is_empty(),
        "a digest mismatch commits no ownership"
    );
    assert_eq!(
        journal(root).intents.len(),
        1,
        "the intent is kept as evidence"
    );
    assert!(
        root.join(".claude/agents/helper.md").is_file(),
        "the loose route survives the blocked switch"
    );
}

/// Rewrite the first journal intent so it names no preimage references and
/// its view digest differs from the digest the attempt will record. The two
/// intents then describe different operations, so the earlier one is ambiguous.
fn make_first_intent_ambiguous(root: &Path) {
    let mut recorded = journal(root);
    recorded.intents[0].preimage_refs = Vec::new();
    recorded.intents[0].view_digest = Some(
        ccync_foundation::provider::PackageDigest::new(format!("sha256:{}", "0".repeat(64)))
            .unwrap(),
    );
    std::fs::write(
        root.join(".ccync/build/projection.txn.json"),
        recorded.to_json_string().unwrap(),
    )
    .unwrap();
}

/// Rewrite the first journal intent so it names no preimage references and
/// otherwise matches the attempt's operation.
fn drop_first_intent_preimages(root: &Path) {
    let mut recorded = journal(root);
    recorded.intents[0].preimage_refs = Vec::new();
    std::fs::write(
        root.join(".ccync/build/projection.txn.json"),
        recorded.to_json_string().unwrap(),
    )
    .unwrap();
}

#[test]
fn p7_t12_ambiguous_earlier_intent_survives_a_failed_install() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude","opencode""#);
    let fake = FakeClaude::default();
    fake.state.borrow_mut().fail_install = true;
    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("sync reports fallback instead of failing");
    make_first_intent_ambiguous(root);
    let earlier_intent = journal(root).intents[0].clone();
    assert!(
        earlier_intent.preimage_refs.is_empty(),
        "the earlier intent names no preimage references"
    );

    // The retry records a replacement for a different view digest, the native
    // install fails, and the earlier intent is put back unchanged.
    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("retry reports fallback instead of failing");
    let retried = journal(root);
    assert_eq!(retried.intents.len(), 1, "the retry adds no second intent");
    assert_eq!(
        retried.intents[0], earlier_intent,
        "the ambiguous earlier intent survives the failed install unchanged"
    );
    assert!(
        committed_claude(root).is_empty(),
        "a failed install commits no ownership"
    );
    assert!(
        root.join(".claude/agents/helper.md").is_file(),
        "the loose route survives the failed install"
    );
}

#[test]
fn p7_t12_same_operation_earlier_intent_is_superseded_on_failed_install() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude","opencode""#);
    let fake = FakeClaude::default();
    fake.state.borrow_mut().fail_install = true;
    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("sync reports fallback instead of failing");
    drop_first_intent_preimages(root);
    let earlier_intent = journal(root).intents[0].clone();

    // The earlier intent describes the same operation, so the failed retry's
    // intent, which adds the saved preimages, supersedes it and is kept.
    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("retry reports fallback instead of failing");
    let retried = journal(root);
    assert_eq!(retried.intents.len(), 1, "the retry adds no second intent");
    let superseding = &retried.intents[0];
    assert!(
        !superseding.preimage_refs.is_empty(),
        "the superseding intent names the saved preimages"
    );
    let mut expected = earlier_intent.clone();
    expected.preimage_refs = superseding.preimage_refs.clone();
    assert_eq!(
        *superseding, expected,
        "only the preimage references differ from the earlier intent"
    );
    for preimage in &superseding.preimage_refs {
        assert!(
            root.join(".ccync/build")
                .join(preimage.saved.as_str())
                .exists(),
            "saved preimage {} is kept for recovery",
            preimage.saved.as_str()
        );
    }
}

#[test]
fn p7_t12_ambiguous_earlier_intent_survives_a_refused_adoption() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude","opencode""#);
    let fake = FakeClaude::default();
    fake.state.borrow_mut().fail_post_install_listing = true;
    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("sync reports fallback instead of failing");
    assert_eq!(fake.installed_count(), 1, "the native add ran");

    // The earlier intent names no preimages and proves a view digest that the
    // installed view does not produce, so adoption is refused.
    make_first_intent_ambiguous(root);
    let earlier_intent = journal(root).intents[0].clone();

    fake.state.borrow_mut().fail_post_install_listing = false;
    let before = fake.mutation_count();
    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("retry reports fallback instead of failing");
    assert_eq!(fake.mutation_count(), before, "no native call on the retry");
    let retried = journal(root);
    assert_eq!(retried.intents.len(), 1, "the retry adds no second intent");
    assert_eq!(
        retried.intents[0], earlier_intent,
        "the ambiguous earlier intent survives the refused adoption unchanged"
    );
    assert!(
        committed_claude(root).is_empty(),
        "a refused adoption commits no ownership"
    );
}

#[test]
fn p7_t12_failed_write_back_reports_both_errors() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude","opencode""#);
    let fake = FakeClaude::default();
    fake.state.borrow_mut().fail_install = true;
    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("sync reports fallback instead of failing");
    make_first_intent_ambiguous(root);

    // The install fails, and the journal turns into a directory before the
    // earlier intent is written back, so the write-back fails too.
    let journal_path = root.join(".ccync/build/projection.txn.json");
    fake.state.borrow_mut().journal_dir_on_install = Some(journal_path);
    let outcome =
        run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
            .expect("sync reports fallback instead of failing");
    let note = outcome
        .evidence
        .run_warnings
        .iter()
        .find(|line| line.contains("claude plugin"))
        .expect("the group reports its activation failure");
    assert!(
        note.contains("injected install failure"),
        "the note keeps the original install error: {note}"
    );
    assert!(
        note.contains("restoring the earlier pending intent also failed"),
        "the note reports the write-back failure: {note}"
    );
}

#[test]
fn p7_t12_newer_generation_replaces_an_older_interrupted_intent_and_converges() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude","opencode""#);
    let fake = FakeClaude::default();
    fake.state.borrow_mut().fail_install = true;

    // Two failed switches leave an interrupted intent for generation G1 that
    // names saved preimages of the loose route.
    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("sync reports fallback instead of failing");
    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("sync reports fallback instead of failing");
    let older = journal(root).intents[0].clone();
    assert!(
        !older.preimage_refs.is_empty(),
        "the older intent names saved preimages"
    );

    // The plugin moves to generation G2.
    write(
        &root.join(".ccync/plugins/example/.claude-plugin/plugin.json"),
        r#"{"name":"example","version":"2.0.0"}"#,
    );
    write(
        &root.join(".ccync/plugins/example/skills/example/SKILL.md"),
        "---\nname: example\ndescription: Example skill.\n---\nBody v2.\n",
    );

    // G2's native add succeeds, then the ownership commit fails, so the run
    // ends after the native change without committing it.
    let lock = root.join(".ccync/build/lock.json");
    {
        let mut state = fake.state.borrow_mut();
        state.fail_install = false;
        state.lock_dir_on_install = Some(lock.clone());
    }
    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("sync reports fallback instead of failing");
    assert_eq!(fake.installed_count(), 1, "the G2 add ran");

    // The journal names the generation being installed, and it keeps the
    // older intent's saved preimages.
    let during = journal(root).intents;
    assert_eq!(during.len(), 1, "one intent names the operation");
    assert_ne!(
        during[0].desired_generation, older.desired_generation,
        "the intent names generation G2"
    );
    assert_eq!(
        during[0].preimage_refs, older.preimage_refs,
        "the saved preimages carry over unchanged"
    );
    let build = root.join(".ccync/build");
    for preimage in &during[0].preimage_refs {
        assert!(
            build.join(preimage.saved.as_str()).exists(),
            "saved preimage {} is kept",
            preimage.saved.as_str()
        );
    }

    // Put the lockfile back and retry. The retry adopts the registration that
    // the G2 intent proves, and it converges without a second native add.
    std::fs::remove_dir(&lock).unwrap();
    std::fs::rename(lock.with_file_name("lock.json.held"), &lock).unwrap();
    let adds_before = fake
        .state
        .borrow()
        .mutations
        .iter()
        .filter(|call| call.starts_with("plugin install"))
        .count();
    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("retry reports fallback instead of failing");
    let adds_after = fake
        .state
        .borrow()
        .mutations
        .iter()
        .filter(|call| call.starts_with("plugin install"))
        .count();
    assert_eq!(adds_after, adds_before, "the retry adds nothing");
    let committed = committed_claude(root);
    assert_eq!(committed.len(), 1, "ownership is committed: {committed:?}");
    assert_eq!(committed[0].generation, during[0].desired_generation);
    assert!(
        journal(root).intents.is_empty(),
        "the intent is cleared after convergence"
    );
}

#[test]
fn p7_t12_failed_post_install_verification_keeps_the_newer_intent_and_converges() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude","opencode""#);
    let fake = FakeClaude::default();
    fake.state.borrow_mut().fail_install = true;

    // Two failed switches leave an interrupted G1 intent with saved preimages.
    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("sync reports fallback instead of failing");
    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("sync reports fallback instead of failing");
    let older = journal(root).intents[0].clone();
    assert!(
        !older.preimage_refs.is_empty(),
        "the older intent has preimages"
    );

    // The plugin moves to G2. Its native add succeeds, then the post-install
    // verification read fails, so the run ends after a registration change.
    write(
        &root.join(".ccync/plugins/example/.claude-plugin/plugin.json"),
        r#"{"name":"example","version":"2.0.0"}"#,
    );
    write(
        &root.join(".ccync/plugins/example/skills/example/SKILL.md"),
        "---\nname: example\ndescription: Example skill.\n---\nBody v2.\n",
    );
    {
        let mut state = fake.state.borrow_mut();
        state.fail_install = false;
        state.fail_post_install_listing = true;
    }
    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("sync reports fallback instead of failing");
    assert_eq!(fake.installed_count(), 1, "the G2 add ran");

    // The attempt intent is kept for G2 and the earlier G1 intent is not restored.
    let during = journal(root).intents;
    assert_eq!(during.len(), 1, "the journal names one operation");
    assert_ne!(
        during[0].desired_generation, older.desired_generation,
        "the journal names generation G2"
    );
    assert_eq!(
        during[0].preimage_refs, older.preimage_refs,
        "the saved preimages carry over unchanged"
    );
    assert!(
        committed_claude(root).is_empty(),
        "no ownership is committed yet"
    );

    // The retry adopts the G2 registration that the intent proves and converges.
    fake.state.borrow_mut().fail_post_install_listing = false;
    let adds_before = fake
        .state
        .borrow()
        .mutations
        .iter()
        .filter(|call| call.starts_with("plugin install"))
        .count();
    let outcome =
        run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
            .expect("retry reports fallback instead of failing");
    let adds_after = fake
        .state
        .borrow()
        .mutations
        .iter()
        .filter(|call| call.starts_with("plugin install"))
        .count();
    assert_eq!(adds_after, adds_before, "the retry adds nothing");
    assert!(
        outcome
            .evidence
            .run_warnings
            .iter()
            .all(|line| !line.contains("External")),
        "no Conflict(External) on retry: {:?}",
        outcome.evidence.run_warnings
    );
    let committed = committed_claude(root);
    assert_eq!(committed.len(), 1, "ownership is committed: {committed:?}");
    assert_eq!(committed[0].generation, during[0].desired_generation);
    assert!(
        journal(root).intents.is_empty(),
        "the intent is cleared after convergence"
    );
}

/// Bytes of every file under `dir`, keyed by path.
fn files_under(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if let Ok(bytes) = std::fs::read(&path) {
                out.insert(path, bytes);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, &mut out);
    out
}

/// Copy a saved preimage back to its live path: a tree when `from` is a
/// directory, a file otherwise.
fn restore_preimage(from: &Path, to: &Path) {
    if from.is_dir() {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            restore_preimage(&entry.path(), &to.join(entry.file_name()));
        }
    } else {
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(from, to).unwrap();
    }
}

#[test]
fn p7_t11_interrupted_switch_saves_preimages_that_restore_the_prior_route() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude","opencode""#);
    let fake = FakeClaude::default();
    fake.state.borrow_mut().fail_install = true;

    // The first failed switch writes the loose route. Native activation runs
    // before any loose write, so that sync has no live copy to save yet.
    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("sync reports fallback instead of failing");
    assert!(root.join(".claude/agents/helper.md").is_file());

    // The second failed switch finds the loose copies and saves them before its intent.
    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("second sync reports fallback instead of failing");
    let prior_route = files_under(&root.join(".claude"));
    assert!(!prior_route.is_empty(), "the loose route exists");

    let build = root.join(".ccync/build");
    let intents = journal(root).intents;
    assert_eq!(intents.len(), 1, "the intent survives for recovery");
    let refs = intents[0].preimage_refs.clone();
    assert!(
        !refs.is_empty(),
        "the intent names each replaced loose copy"
    );
    for preimage in &refs {
        assert!(
            build.join(preimage.saved.as_str()).exists(),
            "saved preimage {} exists",
            preimage.saved.as_str()
        );
    }

    // Lose the live copies, then rebuild the prior route from saved preimages alone.
    for preimage in &refs {
        let live = preimage.path.resolve().expect("user home resolves");
        if live.is_dir() {
            std::fs::remove_dir_all(&live).unwrap();
        } else {
            std::fs::remove_file(&live).unwrap();
        }
    }
    for preimage in &refs {
        let live = preimage.path.resolve().expect("user home resolves");
        restore_preimage(&build.join(preimage.saved.as_str()), &live);
    }
    assert_eq!(
        files_under(&root.join(".claude")),
        prior_route,
        "the saved preimages restore the prior route byte for byte"
    );

    // Recovery completes the switch and drops the saved copies it no longer needs.
    fake.state.borrow_mut().fail_install = false;
    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("recovery sync");
    assert!(journal(root).intents.is_empty(), "the intent clears");
    for preimage in &refs {
        assert!(
            !build.join(preimage.saved.as_str()).exists(),
            "saved preimage {} is removed after cleanup",
            preimage.saved.as_str()
        );
    }
}

/// Count the regular files under `dir`, or zero when it is absent.
fn count_files(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            if entry.path().is_dir() {
                count_files(&entry.path())
            } else {
                1
            }
        })
        .sum()
}

#[test]
fn p7_t11_shared_skill_without_opencode_keeps_loose_route_and_reports_fallback() {
    let (home, _lock) = isolated();
    let root = home.root();
    // OpenCode is not selected, so nothing replaces the shared skill copy it reads.
    fixture(root, r#""claude""#);
    let fake = FakeClaude::default();

    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("sync with an unselected shared reader");
    assert_eq!(
        fake.installed_count(),
        0,
        "a group whose skill is read by an unselected OpenCode never activates natively"
    );
    assert!(
        root.join(".claude/skills/example/SKILL.md").is_file(),
        "the loose skill route is retained"
    );
    assert!(
        committed_claude(root).is_empty(),
        "no ownership is committed"
    );
    assert_eq!(fake.loads(root), 3, "agent, command and skill stay loose");

    let inventory = load_provider_inventory().expect("inventory");
    let rendered = render_provider_generations(&inventory).expect("render");
    let lines = preview_native_cutover(
        &rendered,
        &[ccync_foundation::target::ProjectionTargetId::Claude],
        root,
        &root.join(".ccync/plugins/lock.json"),
        &fake,
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("loose route kept") && line.contains("opencode")),
        "the fallback names the unselected reader: {lines:?}"
    );
}

#[test]
fn p7_t11_opencode_finds_the_skill_through_its_private_root_after_the_switch() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude","opencode""#);
    write(
        &root.join(".ccync/plugins/example/.mcp.json"),
        r#"{"mcpServers":{"demo":{"command":"demo-server"}}}"#,
    );
    let fake = FakeClaude::default();
    let managed = root.join(".ccync/build/mcp/managed.json");

    // A loose sync records the MCP ownership the native switch must keep.
    run_update_with_evidence_using(
        &ccync_engine::config::CcyncConfig::default(),
        &UnavailableNativeRunner,
    )
    .expect("loose sync");
    let mcp_before = std::fs::read(&managed).expect("managed MCP ownership exists");
    assert!(
        String::from_utf8_lossy(&mcp_before).contains("demo"),
        "the fixture server is owned"
    );
    assert!(root.join(".claude/skills/example/SKILL.md").is_file());

    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("native switch");
    assert_eq!(fake.installed_count(), 1, "one native registration");
    assert_eq!(fake.loads(root), 1, "one enabled load, no loose copy");
    assert!(
        !root.join(".claude/skills/example").exists(),
        "the shared skill copy is retired"
    );
    assert!(
        root.join(".config/opencode/skills/example/SKILL.md")
            .is_file(),
        "OpenCode finds the skill through its private root"
    );
    assert_eq!(
        std::fs::read(&managed).expect("managed MCP ownership exists"),
        mcp_before,
        "MCP ownership is unchanged by the native switch"
    );

    let calls = fake.mutation_count();
    let before = snapshot(root);
    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("second sync");
    assert_eq!(
        fake.mutation_count(),
        calls,
        "no second-run native mutation"
    );
    assert_eq!(snapshot(root), before, "no second-run file mutation");
    assert_eq!(fake.loads(root), 1, "still one enabled load");
}

// ── legacy native selector migration (p7_t14) ───────────────────────────────
//
// A legacy registration is an owned record for the same management ID under a
// selector other than the canonical one. The sync that follows a manifest
// rename is the migration: the new registration is installed, the legacy one
// is removed with fresh proof, and only then is the new ownership committed.

use ccync_engine::plugin_native::committed_view_path;

/// Rewrite the native manifest name of the fixture plugin. The management ID
/// (the lockfile plugin id) stays the same, so only the selector changes.
fn rename_manifest(root: &Path, name: &str) {
    write(
        &root.join(".ccync/plugins/example/.claude-plugin/plugin.json"),
        &format!(r#"{{"name":"{name}","version":"1.0.0"}}"#),
    );
}

fn run_sync(fake: &FakeClaude) -> ccync_engine::install::CanonicalRunOutcome {
    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), fake)
        .expect("sync")
}

/// Sync once under the legacy manifest name and return the committed legacy row.
fn legacy_claude_sync(root: &Path, fake: &FakeClaude) -> CommittedOwnership {
    rename_manifest(root, "plugin");
    run_sync(fake);
    let rows = committed_claude(root);
    assert_eq!(
        rows.len(),
        1,
        "one legacy registration after the legacy sync"
    );
    assert_eq!(rows[0].selector.as_str(), "plugin@ccync-plugin");
    rows[0].clone()
}

#[test]
fn p7_t14_owned_legacy_selector_migrates_and_loads_once() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude","opencode""#);
    let fake = FakeClaude::default();
    legacy_claude_sync(root, &fake);

    rename_manifest(root, "example");
    let outcome = run_sync(&fake);

    let rows = committed_claude(root);
    assert_eq!(rows.len(), 1, "the legacy record is dropped");
    assert_eq!(rows[0].selector.as_str(), "example@ccync-example");
    {
        let state = fake.state.borrow();
        assert!(
            !state.installed.contains_key("plugin@ccync-plugin"),
            "the legacy registration is removed"
        );
        assert!(
            state
                .mutations
                .iter()
                .any(|call| call == "plugin uninstall plugin@ccync-plugin --scope user"),
            "the legacy removal ran with its own selector: {:?}",
            state.mutations
        );
    }
    assert_eq!(fake.installed_count(), 1, "one native registration");
    assert_eq!(fake.loads(root), 1, "the plugin loads once");
    assert!(
        journal(root).intents.is_empty(),
        "the migration intent clears after cleanup"
    );
    assert!(
        !outcome
            .evidence
            .run_warnings
            .iter()
            .any(|warning| warning.contains("legacy native route kept")),
        "the migration reports no blocked group: {:?}",
        outcome.evidence.run_warnings
    );
}

#[test]
fn p7_t14_unverifiable_legacy_selector_blocks_and_keeps_both() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude","opencode""#);
    let fake = FakeClaude::default();
    let legacy = legacy_claude_sync(root, &fake);
    // A file ccync did not write makes the legacy view digest differ from its
    // committed record, so the registration is no longer provably owned.
    let view = committed_view_path(&legacy).expect("legacy view path");
    std::fs::write(view.join("tampered.txt"), "not written by ccync").unwrap();

    rename_manifest(root, "example");
    let outcome = run_sync(&fake);

    assert_eq!(
        committed_claude(root),
        vec![legacy],
        "the legacy record is kept"
    );
    {
        let state = fake.state.borrow();
        assert_eq!(
            state.installed.get("plugin@ccync-plugin"),
            Some(&true),
            "the legacy registration stays enabled"
        );
        assert!(
            !state.installed.contains_key("example@ccync-example"),
            "no new registration is installed over an unproven one"
        );
    }
    // The shared skill copy that the opencode route writes under the Claude root
    // is outside this migration, so only the native registrations are asserted.
    assert_eq!(
        fake.installed_count(),
        1,
        "only the kept registration is installed"
    );
    assert!(
        journal(root).intents.is_empty(),
        "no migration intent is written before the proof"
    );
    assert!(
        outcome.evidence.run_warnings.iter().any(|warning| {
            warning.contains("legacy native route kept") && warning.contains("not provably owned")
        }),
        "the blocked group reports its reason: {:?}",
        outcome.evidence.run_warnings
    );
}

#[test]
fn p7_t14_crash_between_new_install_and_legacy_removal_converges_on_retry() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude","opencode""#);
    let fake = FakeClaude::default();
    legacy_claude_sync(root, &fake);

    // The new install succeeds, then the legacy removal fails: the crash point
    // is between the new registration and the removal of the legacy one.
    fake.state.borrow_mut().fail_uninstall = true;
    rename_manifest(root, "example");
    run_sync(&fake);
    {
        let state = fake.state.borrow();
        assert!(
            state.installed.contains_key("plugin@ccync-plugin"),
            "the legacy registration is kept after the failed removal"
        );
        assert!(
            state.installed.contains_key("example@ccync-example"),
            "the new registration was installed before the removal"
        );
    }
    assert_eq!(
        committed_claude(root)[0].selector.as_str(),
        "plugin@ccync-plugin",
        "the legacy record is kept until its removal is verified"
    );
    let intents = journal(root).intents;
    assert_eq!(intents.len(), 1, "one intent records the whole migration");
    assert_eq!(
        intents[0].ownership_fingerprints.len(),
        2,
        "the intent names the new and the legacy selector"
    );

    // The retry adopts the new registration from the intent, removes the
    // legacy one with fresh proof, and commits the new ownership.
    run_sync(&fake);
    let rows = committed_claude(root);
    assert_eq!(rows.len(), 1, "the retry leaves one committed record");
    assert_eq!(rows[0].selector.as_str(), "example@ccync-example");
    assert_eq!(
        fake.installed_count(),
        1,
        "one native registration after the retry"
    );
    assert_eq!(fake.loads(root), 1, "the plugin loads once after the retry");
    assert!(
        journal(root).intents.is_empty(),
        "the intent clears after the retry"
    );
}

#[test]
fn p7_t14_generic_plugin_name_alone_never_triggers_migration() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude","opencode""#);
    rename_manifest(root, "example");
    let fake = FakeClaude::default();
    // A registration named like the generic management ID, with no committed
    // record. Name equality alone is never ownership, so it is left alone.
    fake.state
        .borrow_mut()
        .installed
        .insert("plugin@ccync-plugin".to_string(), true);

    run_sync(&fake);

    let state = fake.state.borrow();
    assert_eq!(
        state.installed.get("plugin@ccync-plugin"),
        Some(&true),
        "the same-name registration is untouched"
    );
    assert!(
        state
            .mutations
            .iter()
            .all(|call| !call.contains("uninstall")),
        "no removal was attempted: {:?}",
        state.mutations
    );
    assert!(
        state.installed.contains_key("example@ccync-example"),
        "the canonical registration is installed"
    );
}

// ── shared-root reader coverage through sync ────────────────────────────────
//
// A native switch retires a shared copy only after every other reader of that
// root has a verified copy elsewhere: a shared root whose copy stays, or its
// private root. The covering copies are written before any native change.

/// A provider with one skill and nothing else, so no command-skill shares a
/// root with it.
fn skill_only_fixture(home: &Path, targets: &str) {
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
        &plugin.join("skills/example/references/notes.md"),
        "Notes.\n",
    );
    write(
        &home.join(".ccync/build/lock.json"),
        r#"{"resolvedPlugins":[{"pluginId":"example","resolvedSource":"example","installStrategy":"bundled-local","sha":"0123456789abcdef0123456789abcdef01234567"}]}"#,
    );
    write(
        &home.join(".ccync/config.json"),
        &format!(r#"{{"selectedProjectionTargets":[{targets}]}}"#),
    );
}

/// A provider that is a single skill, with its `SKILL.md` at the source root.
fn single_skill_fixture(home: &Path, targets: &str) {
    let plugin = home.join(".ccync/plugins/single");
    write(
        &plugin.join("SKILL.md"),
        "---\nname: single\ndescription: Single skill.\n---\nBody.\n",
    );
    write(&plugin.join("LICENSE"), "MIT\n");
    write(&plugin.join("references/notes.md"), "Notes.\n");
    write(
        &home.join(".ccync/build/lock.json"),
        r#"{"resolvedPlugins":[{"pluginId":"single","resolvedSource":"single","installStrategy":"bundled-local","sha":"0123456789abcdef0123456789abcdef01234567"}]}"#,
    );
    write(
        &home.join(".ccync/config.json"),
        &format!(r#"{{"selectedProjectionTargets":[{targets}]}}"#),
    );
}

/// True when both skill copies hold the same `SKILL.md` and notes bytes.
fn same_skill(left: &Path, right: &Path) -> bool {
    ["SKILL.md", "references/notes.md"].iter().all(|file| {
        let read = |dir: &Path| std::fs::read(dir.join(file)).ok();
        read(left).is_some() && read(left) == read(right)
    })
}

#[test]
fn shared_reader_coverage_claude_switch_keeps_the_agents_copy_for_opencode() {
    let (home, _lock) = isolated();
    let root = home.root();
    skill_only_fixture(root, r#""claude","codex","opencode""#);
    let fake = FakeClaude {
        only: Some(NativeExecutable::Claude),
        ..FakeClaude::default()
    };

    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("sync");

    assert_eq!(fake.installed_count(), 1, "the Claude group activates");
    assert!(
        !root.join(".claude/skills/example").exists(),
        "the shared Claude copy retires"
    );
    let source = root.join(".ccync/plugins/example/skills/example");
    assert!(
        same_skill(&root.join(".agents/skills/example"), &source),
        "OpenCode keeps the identical copy in ~/.agents/skills"
    );
    assert!(
        !root.join(".config/opencode/skills/example").exists(),
        "no private OpenCode copy is needed while the shared one stays"
    );
    assert_eq!(fake.loads(root), 1, "Claude loads the skill once");
}

#[test]
fn shared_reader_coverage_codex_switch_writes_reader_copies_before_retiring() {
    let (home, _lock) = isolated();
    let root = home.root();
    skill_only_fixture(root, r#""codex","copilot","opencode""#);
    let fake = FakeClaude {
        only: Some(NativeExecutable::Codex),
        ..FakeClaude::default()
    };

    let inventory = load_provider_inventory().expect("inventory");
    let rendered = render_provider_generations(&inventory).expect("render");
    use ccync_foundation::target::ProjectionTargetId as Target;
    let lines = preview_native_cutover(
        &rendered,
        &[Target::Codex, Target::Copilot, Target::Opencode],
        root,
        &root.join(".ccync/plugins/lock.json"),
        &fake,
    );
    assert!(
        lines
            .iter()
            .any(|line| line == "codex plugin example: native activation planned"),
        "{lines:?}"
    );

    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("sync");

    assert_eq!(fake.installed_count(), 1, "the Codex group activates");
    assert!(
        !root.join(".agents/skills/example").exists(),
        "the shared ~/.agents/skills copy retires"
    );
    let source = root.join(".ccync/plugins/example/skills/example");
    assert!(
        same_skill(&root.join(".copilot/skills/example"), &source),
        "Copilot keeps an identical copy in its private root"
    );
    assert!(
        same_skill(&root.join(".config/opencode/skills/example"), &source),
        "OpenCode gets an identical copy in its private root"
    );
}

#[test]
fn single_skill_provider_publishes_its_source_root_as_the_skill() {
    let (home, _lock) = isolated();
    let root = home.root();
    single_skill_fixture(root, r#""copilot""#);

    run_update_with_evidence_using(
        &ccync_engine::config::CcyncConfig::default(),
        &UnavailableNativeRunner,
    )
    .expect("sync");

    let copy = root.join(".copilot/skills/single");
    assert!(
        same_skill(&copy, &root.join(".ccync/plugins/single")),
        "the whole single-skill source becomes the skill directory"
    );
    assert!(copy.join("LICENSE").is_file());
}

#[test]
fn single_skill_provider_covers_copilot_before_the_codex_switch() {
    let (home, _lock) = isolated();
    let root = home.root();
    single_skill_fixture(root, r#""codex","copilot","opencode""#);
    let fake = FakeClaude {
        only: Some(NativeExecutable::Codex),
        ..FakeClaude::default()
    };

    run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
        .expect("sync");

    assert_eq!(fake.installed_count(), 1, "the Codex group activates");
    assert!(!root.join(".agents/skills/single").exists());
    let source = root.join(".ccync/plugins/single");
    assert!(same_skill(&root.join(".copilot/skills/single"), &source));
    assert!(same_skill(
        &root.join(".config/opencode/skills/single"),
        &source
    ));
}

// Unowned copies remain in place even when their bytes match the provider.

/// Write an unowned copy of the `example` skill at `dir`, with `notes` as the
/// body of its notes file.
fn legacy_example_copy(dir: &Path, notes: &str) {
    write(
        &dir.join("SKILL.md"),
        "---\nname: example\ndescription: Example skill.\n---\nBody.\n",
    );
    write(&dir.join("references/notes.md"), notes);
}

#[test]
fn equal_content_not_owned_skill_directory_stays_and_blocks_native_group() {
    let (home, _lock) = isolated();
    let root = home.root();
    skill_only_fixture(root, r#""claude""#);
    let copy = root.join(".claude/skills/example");
    legacy_example_copy(&copy, "Notes.\n");

    let fake = FakeClaude::default();
    let _outcome =
        run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
            .expect("sync");

    assert!(same_skill(
        &copy,
        &root.join(".ccync/plugins/example/skills/example")
    ));
    assert!(!root.join(".ccync/backups").exists());
    let inventory = load_provider_inventory().expect("inventory");
    let rendered = render_provider_generations(&inventory).expect("rendered providers");
    let native_lines = preview_native_cutover(
        &rendered,
        &[ccync_foundation::target::ProjectionTargetId::Claude],
        root,
        &root.join(".ccync/plugins/lock.json"),
        &fake,
    );
    assert!(
        native_lines.iter().any(|line| line.contains("blocked")),
        "foreign equal-byte skill blocks its native group: {native_lines:?}"
    );
    assert!(
        copy.is_dir(),
        "the unowned equal-byte directory stays in place"
    );
}

#[test]
fn legacy_changed_copy_stays_and_ccync_publishes_beside_it() {
    let (home, _lock) = isolated();
    let root = home.root();
    skill_only_fixture(root, r#""copilot""#);
    let copy = root.join(".copilot/skills/example");
    legacy_example_copy(&copy, "Edited by the user.\n");

    run_update_with_evidence_using(
        &ccync_engine::config::CcyncConfig::default(),
        &UnavailableNativeRunner,
    )
    .expect("sync");

    assert!(
        !root.join(".ccync/backups").exists(),
        "no backup is created"
    );
    assert_eq!(
        std::fs::read_to_string(copy.join("references/notes.md")).unwrap(),
        "Edited by the user.\n"
    );
    let aliased = std::fs::read_dir(root.join(".copilot/skills"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path != &copy && path.join("SKILL.md").is_file())
        .collect::<Vec<_>>();
    assert_eq!(
        aliased.len(),
        1,
        "ccync publishes under an alias: {aliased:?}"
    );
}

#[test]
fn foreign_same_name_skill_does_not_block_an_agent_that_names_it_in_paths() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude""#);
    write(
        &root.join(".ccync/plugins/example/agents/helper.md"),
        "---\nname: helper\ndescription: Helper agent.\n---\nRead ../skills/example/notes.md, then run /example now.\n",
    );
    // A user's own `example` skill with other content forces an alias.
    write(&root.join(".claude/skills/example/SKILL.md"), "user\n");

    let outcome = rollout().expect("sync");

    assert!(
        outcome
            .route_conflicts
            .iter()
            .all(|conflict| conflict.component != "helper"),
        "{:?}",
        outcome.route_conflicts
    );
    let agent = std::fs::read_to_string(root.join(".claude/agents/helper.md")).expect("agent");
    assert!(agent.contains("../skills/example/notes.md"), "{agent}");
    assert!(
        !agent.contains("run /example now"),
        "the command token is aliased: {agent}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join(".claude/skills/example/SKILL.md")).unwrap(),
        "user\n"
    );
}

/// Agent files under `~/.config/opencode/agents` whose name ends with
/// `helper.md`.
fn opencode_helper_files(root: &Path) -> Vec<String> {
    let mut names = std::fs::read_dir(root.join(".config/opencode/agents"))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with("helper.md"))
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn equal_content_not_owned_agent_file_stays_and_blocks_native_group() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""claude""#);
    let plain = root.join(".claude/agents/helper.md");
    let source = root.join(".ccync/plugins/example/agents/helper.md");
    std::fs::create_dir_all(plain.parent().unwrap()).unwrap();
    std::fs::copy(&source, &plain).unwrap();

    let fake = FakeClaude::default();
    let _outcome =
        run_update_with_evidence_using(&ccync_engine::config::CcyncConfig::default(), &fake)
            .expect("sync");

    assert_eq!(
        std::fs::read(&plain).unwrap(),
        std::fs::read(&source).unwrap()
    );
    assert!(!root.join(".ccync/backups").exists());
    let inventory = load_provider_inventory().expect("inventory");
    let rendered = render_provider_generations(&inventory).expect("rendered providers");
    let native_lines = preview_native_cutover(
        &rendered,
        &[ccync_foundation::target::ProjectionTargetId::Claude],
        root,
        &root.join(".ccync/plugins/lock.json"),
        &fake,
    );
    assert!(
        native_lines.iter().any(|line| line.contains("blocked")),
        "foreign equal-byte agent blocks its native group: {native_lines:?}"
    );
    assert!(
        plain.is_file(),
        "the unowned equal-byte file stays in place"
    );
}

#[test]
fn legacy_changed_agent_stays_and_sync_names_its_removal_command() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""opencode""#);
    let plain = root.join(".config/opencode/agents/helper.md");
    write(&plain, "the owner's own helper\n");

    let first = rollout().expect("first sync");
    let second = rollout().expect("second sync");

    assert_eq!(
        std::fs::read_to_string(&plain).unwrap(),
        "the owner's own helper\n"
    );
    assert!(!root.join(".ccync/backups/legacy-copies").exists());
    for outcome in [first, second] {
        let warnings = outcome.evidence.run_warnings.join("\n");
        assert!(
            warnings.contains("kept unowned agent")
                && warnings.contains(&home_path_display(
                    root,
                    ".config/opencode/agents/helper.md"
                )),
            "{warnings}"
        );
        let command = if cfg!(windows) {
            "Remove-Item -LiteralPath"
        } else {
            "rm '"
        };
        assert!(warnings.contains(command), "{warnings}");
        assert!(!warnings.contains("moved legacy copy"), "{warnings}");
    }
}

/// Display form of a home-relative path as the engine prints it, with the
/// platform separator between every segment.
fn home_path_display(root: &Path, relative: &str) -> String {
    relative
        .split('/')
        .fold(root.to_path_buf(), |path, segment| path.join(segment))
        .display()
        .to_string()
}

#[test]
fn unowned_copilot_agent_file_aliases_the_provider_agent() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""copilot""#);
    let plain = root.join(".copilot/agents/helper.agent.md");
    write(
        &plain,
        "the owner's own helper
",
    );

    let outcome = rollout().expect("sync");

    let failures = outcome
        .evidence
        .by_target
        .values()
        .filter_map(|target| target.failure.as_ref())
        .map(|failure| failure.reason.clone())
        .collect::<Vec<_>>();
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(
        std::fs::read_to_string(&plain).unwrap(),
        "the owner's own helper
"
    );
    let aliased = std::fs::read_dir(root.join(".copilot/agents"))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "helper.agent.md" && name.ends_with("-helper.agent.md"))
        .collect::<Vec<_>>();
    assert_eq!(
        aliased.len(),
        1,
        "ccync publishes under an alias: {aliased:?}"
    );
}

#[test]
fn aliased_agent_keeps_one_copy_per_target_and_converges_without_flapping() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""opencode","copilot""#);
    let copilot = root.join(".copilot/agents/helper.agent.md");
    // Only Copilot holds the plain name, so OpenCode's plain name stays free
    // while the agent is aliased on every target.
    write(&copilot, "the owner's own helper\n");

    for round in 0..2 {
        rollout().expect("sync while Copilot holds the name");
        let files = opencode_helper_files(root);
        assert_eq!(
            files.len(),
            1,
            "round {round}: one OpenCode copy: {files:?}"
        );
        assert_ne!(files[0], "helper.md", "round {round}: the copy is aliased");
    }

    // The owner removes the changed Copilot file, as the sync note suggests.
    std::fs::remove_file(&copilot).unwrap();
    for round in 0..3 {
        let outcome = rollout().expect("sync after the owner's removal");
        assert_eq!(
            opencode_helper_files(root),
            vec!["helper.md".to_string()],
            "round {round}"
        );
        let copilot_files = std::fs::read_dir(root.join(".copilot/agents"))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with("helper.agent.md"))
            .collect::<Vec<_>>();
        assert_eq!(
            copilot_files,
            vec!["helper.agent.md".to_string()],
            "round {round}"
        );
        let failures = outcome
            .evidence
            .by_target
            .values()
            .filter_map(|target| target.failure.as_ref())
            .map(|failure| failure.reason.clone())
            .collect::<Vec<_>>();
        assert!(failures.is_empty(), "round {round}: {failures:?}");
        let warnings = outcome.evidence.run_warnings.join("\n");
        assert!(!warnings.contains("kept unowned agent"), "{warnings}");
    }
}

#[test]
fn equal_content_unowned_agent_stays_while_another_target_keeps_the_alias() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""opencode","copilot""#);
    let plain = root.join(".config/opencode/agents/helper.md");
    let copilot = root.join(".copilot/agents/helper.agent.md");
    write(&plain, "placeholder\n");
    write(&copilot, "the owner's own helper\n");
    rollout().expect("first sync");
    let aliased = opencode_helper_files(root)
        .into_iter()
        .find(|name| name != "helper.md")
        .expect("ccync publishes the agent under an alias");
    let payload =
        std::fs::read(root.join(".config/opencode/agents").join(&aliased)).expect("alias");
    std::fs::write(&plain, &payload).unwrap();

    let outcome = rollout().expect("sync while Copilot holds the name");

    let warnings = outcome.evidence.run_warnings.join("\n");
    assert!(warnings.contains("kept unowned agent"), "{warnings}");
    assert!(plain.is_file(), "the equal-byte unowned agent remains");
    assert!(!root.join(".ccync/backups").exists());
    assert_eq!(
        opencode_helper_files(root),
        vec![aliased, "helper.md".to_string()]
    );
    assert_eq!(
        std::fs::read_to_string(&copilot).unwrap(),
        "the owner's own helper\n"
    );
}

/// Record `path` in the lockfile as output of the plain-name Codex agent
/// writer, in the form the projection registry records paths.
fn record_plain_writer_codex_agent(root: &Path, path: &Path) {
    let lock_path = root.join(".ccync/build/lock.json");
    let mut lock: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&lock_path).unwrap()).unwrap();
    let key = projection::normalize_managed_artifact_path(path);
    let projection = lock
        .as_object_mut()
        .unwrap()
        .entry("_ccyncProjection")
        .or_insert_with(|| serde_json::json!({"schemaVersion": 1}));
    projection
        .as_object_mut()
        .unwrap()
        .insert("codexAgentProjectionPaths".into(), serde_json::json!([key]));
    std::fs::write(&lock_path, serde_json::to_string(&lock).unwrap()).unwrap();
}

#[test]
fn plain_writer_agent_file_stays_when_the_agent_is_aliased() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""opencode","copilot","codex""#);
    write(
        &root.join(".copilot/agents/helper.agent.md"),
        "the owner's own helper\n",
    );
    rollout().expect("first sync aliases the agent");
    let codex = root.join(".codex/agents/helper.toml");
    write(&codex, "name = \"helper\"\n");
    record_plain_writer_codex_agent(root, &codex);

    let outcome = rollout().expect("sync with the plain-name writer's file");

    let warnings = outcome.evidence.run_warnings.join("\n");
    assert!(codex.is_file());
    assert!(!root.join(".ccync/backups").exists());
    assert!(!warnings.contains("moved legacy copy"), "{warnings}");
}

#[test]
fn plain_writer_agent_file_stays_when_the_agent_is_not_aliased() {
    let (home, _lock) = isolated();
    let root = home.root();
    fixture(root, r#""opencode","codex""#);
    rollout().expect("first sync publishes under the plain name");
    let codex = root.join(".codex/agents/helper.toml");
    write(&codex, "name = \"helper\"\n");
    record_plain_writer_codex_agent(root, &codex);

    let outcome = rollout().expect("second sync");

    let warnings = outcome.evidence.run_warnings.join("\n");
    assert!(!warnings.contains("moved legacy copy"), "{warnings}");
    assert!(!root.join(".ccync/backups/legacy-copies").exists());
}
