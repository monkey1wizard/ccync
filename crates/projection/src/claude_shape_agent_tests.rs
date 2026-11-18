use super::*;
use std::fs;
use tempfile::TempDir;

fn fixture() -> (
    TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let temp = TempDir::new().unwrap();
    let repo_root = temp.path().join("repo");
    let source_root = repo_root.join("plugins").join("ccync-core");
    let home = temp.path().join("home");
    fs::create_dir_all(source_root.join("skills").join("sample-skill")).unwrap();
    fs::create_dir_all(source_root.join("agents")).unwrap();
    (temp, repo_root, source_root, home)
}

#[test]
fn direct_agents_preserve_foreign_target_files() {
    let (_temp, repo_root, source_root, home) = fixture();
    fs::write(
        source_root.join("agents").join("reviewer.agent.md"),
        "---\nname: reviewer\ndescription: Review\n---\nManaged body\n",
    )
    .unwrap();
    let foreign = [
        (
            home.join(".copilot/agents/reviewer.agent.md"),
            "foreign copilot",
        ),
        (home.join(".codex/agents/reviewer.toml"), "foreign codex"),
    ];
    for (path, content) in &foreign {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    let evidence = run_update_skills(&options(repo_root, source_root, home)).unwrap();

    for (path, content) in foreign {
        assert_eq!(fs::read_to_string(path).unwrap(), content);
    }
    assert!(evidence.by_target[&ProjectionTargetId::Copilot]
        .warnings
        .iter()
        .any(|warning| warning.contains("unowned Copilot agent")));
    assert!(evidence.by_target[&ProjectionTargetId::Codex]
        .warnings
        .iter()
        .any(|warning| warning.contains("unowned Codex agent")));
}

#[test]
fn direct_agents_keep_managed_provenance_and_target_restrictions() {
    let (_temp, repo_root, source_root, home) = fixture();
    fs::write(
        source_root.join("agents").join("reviewer.agent.md"),
        "---\nname: reviewer\ndescription: Review\nmodel: gpt-5.4\nmodel_reasoning_effort: high\n---\nInstructions\n",
    )
    .unwrap();

    run_update_skills(&options(repo_root, source_root, home.clone())).unwrap();

    let codex = fs::read_to_string(home.join(".codex/agents/reviewer.toml")).unwrap();
    assert!(codex.contains("model = \"gpt-5.4\""));
    assert!(codex.contains("model_reasoning_effort = \"high\""));
    assert!(codex.contains("sandbox_mode = \"read-only\""));
    let copilot = fs::read_to_string(home.join(".copilot/agents/reviewer.agent.md")).unwrap();
    assert!(copilot.contains("Instructions"));
    assert!(copilot.contains("model: gpt-5.4"));
}

#[test]
fn direct_agents_publish_supported_fields_to_all_owned_surfaces() {
    let (_temp, _repo_root, _source_root, home) = fixture();
    let content = "---\nname: reviewer\ndescription: Review carefully\nmodel: gpt-5.4\nmodel_reasoning_effort: high\ntools: [read, edit]\ncolor: green\n---\nRead [reference](./guide.md)\n";
    let input = DirectAgentInput {
        source_id: "provider-one".into(),
        name: "reviewer".into(),
        content: content.into(),
    };
    let lockfile = home.join(".ccync/build/lock.json");
    let targets = [
        ProjectionTargetId::Claude,
        ProjectionTargetId::Codex,
        ProjectionTargetId::Copilot,
        ProjectionTargetId::Opencode,
        ProjectionTargetId::AgyGui,
    ];

    for target in targets {
        let evidence = publish_direct_agents(
            target,
            std::slice::from_ref(&input),
            &home,
            &lockfile,
            false,
        );
        assert!(evidence.by_target[&target].failure.is_none());
        if target == ProjectionTargetId::Opencode {
            assert!(evidence.by_target[&target]
                .warnings
                .iter()
                .any(|warning| warning.contains("model_reasoning_effort is unsupported")));
        }
    }

    let claude = fs::read_to_string(home.join(".claude/agents/reviewer.md")).unwrap();
    assert!(claude.contains("model_reasoning_effort: high"));
    let copilot = fs::read_to_string(home.join(".copilot/agents/reviewer.agent.md")).unwrap();
    assert!(copilot.contains("tools: [read, edit]"));
    let agy = fs::read_to_string(home.join(".gemini/config/agents/reviewer.md")).unwrap();
    assert!(agy.contains("Read [reference](./guide.md)"));
    let opencode = fs::read_to_string(home.join(".config/opencode/agents/reviewer.md")).unwrap();
    assert!(opencode.contains("model: gpt-5.4"));
    assert!(opencode.contains("  edit: allow"));
    assert!(opencode.contains("Read [reference](./guide.md)"));
    let codex_path = home.join(".codex/agents/reviewer.toml");
    let codex = fs::read_to_string(&codex_path).unwrap();
    assert!(codex.contains("model = \"gpt-5.4\""));
    assert!(codex.contains("model_reasoning_effort = \"high\""));
    assert!(codex.contains("Read [reference](./guide.md)"));
}

#[test]
fn direct_agents_preserve_unknown_codex_sections_and_reject_foreign_owners() {
    let (_temp, _repo_root, _source_root, home) = fixture();
    let lockfile = home.join(".ccync/build/lock.json");
    let input = DirectAgentInput {
        source_id: "provider-one".into(),
        name: "reviewer".into(),
        content: "---\nname: reviewer\ndescription: Review\ntools: [read, magic]\n---\nBody\n"
            .into(),
    };
    let initial = publish_direct_agents(
        ProjectionTargetId::Codex,
        std::slice::from_ref(&input),
        &home,
        &lockfile,
        false,
    );
    assert!(initial.by_target[&ProjectionTargetId::Codex]
        .warnings
        .iter()
        .any(|warning| warning.contains("tools restriction is unsupported")));
    let codex_path = home.join(".codex/agents/reviewer.toml");
    let mut codex = fs::read_to_string(&codex_path).unwrap();
    codex.push_str("\n[features]\nexperimental = true\n");
    fs::write(&codex_path, codex).unwrap();
    let update = publish_direct_agents(
        ProjectionTargetId::Codex,
        std::slice::from_ref(&input),
        &home,
        &lockfile,
        false,
    );
    assert!(update.by_target[&ProjectionTargetId::Codex]
        .warnings
        .iter()
        .any(|warning| warning.contains("tools restriction is unsupported")));
    assert!(fs::read_to_string(&codex_path)
        .unwrap()
        .contains("[features]\nexperimental = true"));

    let foreign = DirectAgentInput {
        source_id: "provider-two".into(),
        ..input
    };
    let result = publish_direct_agents(
        ProjectionTargetId::Codex,
        &[foreign],
        &home,
        &lockfile,
        false,
    );
    assert!(result.by_target[&ProjectionTargetId::Codex]
        .warnings
        .iter()
        .any(|w| w.contains("unowned agent destination")));
    assert!(fs::read_to_string(codex_path)
        .unwrap()
        .contains("[features]\nexperimental = true"));
}

#[test]
fn direct_agents_preserve_foreign_files_on_every_supported_target() {
    let (_temp, _repo_root, _source_root, home) = fixture();
    let lockfile = home.join(".ccync/build/lock.json");
    let cases = [
        (ProjectionTargetId::Claude, ".claude/agents/reviewer.md"),
        (ProjectionTargetId::Codex, ".codex/agents/reviewer.toml"),
        (
            ProjectionTargetId::Copilot,
            ".copilot/agents/reviewer.agent.md",
        ),
        (
            ProjectionTargetId::Opencode,
            ".config/opencode/agents/reviewer.md",
        ),
        (
            ProjectionTargetId::AgyGui,
            ".gemini/config/agents/reviewer.md",
        ),
    ];
    let input = DirectAgentInput {
        source_id: "provider-one".into(),
        name: "reviewer".into(),
        content: "---\nname: reviewer\ndescription: Review\n---\nBody\n".into(),
    };

    for (target, relative_path) in cases {
        let path = home.join(relative_path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "foreign bytes").unwrap();
        let evidence = publish_direct_agents(
            target,
            std::slice::from_ref(&input),
            &home,
            &lockfile,
            false,
        );
        assert!(evidence.by_target[&target]
            .warnings
            .iter()
            .any(|warning| warning.contains("preserved unowned agent destination")));
        assert_eq!(fs::read_to_string(path).unwrap(), "foreign bytes");
    }
}

#[test]
fn direct_agents_dry_run_and_invalid_inputs_are_handled() {
    let (_temp, _repo_root, _source_root, home) = fixture();
    let lockfile = home.join(".ccync/build/lock.json");

    let input = DirectAgentInput {
        source_id: "provider-one".into(),
        name: "reviewer".into(),
        content: "---\nname: reviewer\ndescription: Review\n---\nBody\n".into(),
    };
    let evidence = publish_direct_agents(
        ProjectionTargetId::Claude,
        std::slice::from_ref(&input),
        &home,
        &lockfile,
        true,
    );
    assert!(evidence
        .by_target
        .get(&ProjectionTargetId::Claude)
        .is_none_or(|target| target.failure.is_none()));
    assert!(!home.join(".claude/agents/reviewer.md").exists());

    let invalid_input = DirectAgentInput {
        source_id: "invalid/source/id".into(),
        name: "reviewer".into(),
        content: "---\nname: reviewer\ndescription: Review\n---\nBody\n".into(),
    };
    let evidence_invalid = publish_direct_agents(
        ProjectionTargetId::Claude,
        &[invalid_input],
        &home,
        &lockfile,
        false,
    );
    assert!(evidence_invalid.by_target[&ProjectionTargetId::Claude]
        .failure
        .is_some());
    assert!(!home.join(".claude/agents/reviewer.md").exists());
}

#[test]
fn direct_agents_reject_agy_ide_and_publish_agy_cli_gui_to_shared_root() {
    let (_temp, _repo_root, _source_root, home) = fixture();
    let lockfile = home.join(".ccync/build/lock.json");
    let input = DirectAgentInput {
        source_id: "provider-one".into(),
        name: "reviewer".into(),
        content: "---\nname: reviewer\ndescription: Review\n---\nBody\n".into(),
    };

    let agy_ide = publish_direct_agents(
        ProjectionTargetId::AgyIde,
        std::slice::from_ref(&input),
        &home,
        &lockfile,
        false,
    );
    let ide_result = &agy_ide.by_target[&ProjectionTargetId::AgyIde];
    assert!(ide_result.paths.is_empty());
    assert_eq!(
        ide_result
            .failure
            .as_ref()
            .map(|failure| failure.reason.as_str()),
        Some("agent definitions are unsupported on agy-ide")
    );
    assert!(!home.join(".gemini/config/agents/reviewer.md").exists());

    for target in [ProjectionTargetId::AgyCli, ProjectionTargetId::AgyGui] {
        let evidence = publish_direct_agents(
            target,
            std::slice::from_ref(&input),
            &home,
            &lockfile,
            false,
        );
        assert!(evidence.by_target[&target].failure.is_none());
        assert_eq!(
            evidence.by_target[&target]
                .paths
                .iter()
                .filter(|path| path.path == home.join(".gemini/config/agents/reviewer.md"))
                .count(),
            1
        );
    }
}

fn options(
    repo_root: std::path::PathBuf,
    source_root: std::path::PathBuf,
    home: std::path::PathBuf,
) -> SkillUpdateOptions {
    SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        state_root: home.join(".ccync").join("build"),
        user_home: home,
        selected_projection_targets: vec!["copilot".into(), "codex".into(), "opencode".into()],
        dry_run: false,
        replace: true,
    }
}

#[test]
fn projects_claude_shape_agents_to_all_targets_and_strips_bom() {
    let (_temp, repo_root, source_root, home) = fixture();
    let raw = "\u{feff}---\nname: alpha\ndescription: Alpha agent\nmodel: ignored\nmodel_reasoning_effort: high\n---\nBody\n";
    fs::write(source_root.join("agents").join("alpha.md"), raw).unwrap();

    run_update_skills(&options(repo_root, source_root, home.clone())).unwrap();

    let copilot = fs::read_to_string(home.join(".copilot/agents/alpha.agent.md")).unwrap();
    assert!(copilot.contains("name: alpha"));
    assert!(!copilot.contains("model:"));
    assert!(!copilot.starts_with('\u{feff}'));

    let codex = fs::read_to_string(home.join(".codex/agents/alpha.toml")).unwrap();
    assert!(codex.contains("name = \"alpha\""));
    assert!(!codex.contains("model ="));
    assert!(!codex.contains("model_reasoning_effort"));

    let opencode = fs::read_to_string(home.join(".config/opencode/agents/alpha.md")).unwrap();
    assert!(opencode.contains("description: |\n  Alpha agent"));
    assert!(opencode.contains("Body"));
    assert!(!opencode.contains("name: alpha"));
}

#[test]
fn claude_shape_agents_precedence_and_recovery_lifecycle() {
    let (_temp, repo_root, source_root, home) = fixture();
    fs::write(
        source_root.join("agents").join("alpha.md"),
        "---\nname: alpha\ndescription: Claude shape agent\nmodel: claude-3-5-sonnet\n---\nClaude Body\n",
    )
    .unwrap();

    // Initial run: alpha.md projects to Copilot with model removed
    run_update_skills(&options(
        repo_root.clone(),
        source_root.clone(),
        home.clone(),
    ))
    .unwrap();
    let copilot_v1 = fs::read_to_string(home.join(".copilot/agents/alpha.agent.md")).unwrap();
    assert!(copilot_v1.contains("name: alpha"));
    assert!(!copilot_v1.contains("model:"));

    // Adding alpha.agent.md: .agent.md takes precedence with warning
    fs::write(
        source_root.join("agents").join("alpha.agent.md"),
        "---\nname: alpha\ndescription: Legacy agent\nmodel: gpt-4o\n---\nLegacy Body\n",
    )
    .unwrap();

    let evidence = run_update_skills(&options(
        repo_root.clone(),
        source_root.clone(),
        home.clone(),
    ))
    .unwrap();
    assert!(evidence.run_warnings.iter().any(|w| w.contains("alpha.md")
        && w.contains("alpha.agent.md")
        && w.contains("takes precedence")));

    let copilot_v2 = fs::read_to_string(home.join(".copilot/agents/alpha.agent.md")).unwrap();
    assert!(
        copilot_v2.contains("Legacy Body"),
        ".agent.md source content must win when both shapes exist"
    );
    assert!(
        copilot_v2.contains("model: gpt-4o"),
        ".agent.md retains its model line on Copilot"
    );

    // Removing alpha.agent.md: next run rewrites Copilot alpha.agent.md from alpha.md
    fs::remove_file(source_root.join("agents").join("alpha.agent.md")).unwrap();
    run_update_skills(&options(repo_root, source_root, home.clone())).unwrap();

    let copilot_v3 = fs::read_to_string(home.join(".copilot/agents/alpha.agent.md")).unwrap();
    assert!(
        copilot_v3.contains("Claude Body"),
        "Copilot alpha.agent.md must be rewritten from alpha.md after alpha.agent.md is removed"
    );
    assert!(
        !copilot_v3.contains("model:"),
        "Rewritten Copilot alpha.agent.md must have model removed"
    );
}

#[test]
fn claude_shape_agents_plain_file_without_frontmatter_skipped_silently() {
    let (_temp, repo_root, source_root, home) = fixture();
    fs::write(
        source_root.join("agents").join("README.md"),
        "# Agent Docs\nJust documentation, no frontmatter delimiter.\n",
    )
    .unwrap();

    let evidence = run_update_skills(&options(repo_root, source_root, home.clone())).unwrap();
    assert!(!home.join(".copilot/agents/README.md").exists());
    assert!(!home.join(".copilot/agents/README.agent.md").exists());
    assert!(!home.join(".codex/agents/README.toml").exists());
    assert!(!home.join(".config/opencode/agents/README.md").exists());
    assert!(
        !evidence
            .run_warnings
            .iter()
            .any(|w| w.contains("README.md")),
        "File without frontmatter must be silently skipped with no warnings"
    );
}

#[test]
fn claude_shape_agents_unnamed_frontmatter_skipped_with_warning() {
    let (_temp, repo_root, source_root, home) = fixture();
    fs::write(
        source_root.join("agents").join("unnamed.md"),
        "---\ndescription: Has description but no name\n---\nBody\n",
    )
    .unwrap();

    let evidence = run_update_skills(&options(repo_root, source_root, home.clone())).unwrap();
    assert!(!home.join(".copilot/agents/unnamed.agent.md").exists());
    assert!(!home.join(".codex/agents/unnamed.toml").exists());
    assert!(!home.join(".config/opencode/agents/unnamed.md").exists());
    assert!(evidence.run_warnings.iter().any(|w| w
        .contains("agent 'unnamed.md' from source 'ccync-core' skipped: frontmatter has no name")));
}

#[test]
fn claude_shape_agents_legacy_agent_without_name_reaches_copilot_and_opencode() {
    let (_temp, repo_root, source_root, home) = fixture();
    fs::write(
        source_root.join("agents").join("legacy.agent.md"),
        "---\ndescription: Legacy agent without name\nmodel: gpt-4\n---\nLegacy Body\n",
    )
    .unwrap();

    run_update_skills(&options(repo_root, source_root, home.clone())).unwrap();

    // Copilot gets legacy.agent.md verbatim
    let copilot = fs::read_to_string(home.join(".copilot/agents/legacy.agent.md")).unwrap();
    assert!(copilot.contains("description: Legacy agent without name"));
    assert!(copilot.contains("model: gpt-4"));

    // OpenCode gets legacy.md
    let opencode = fs::read_to_string(home.join(".config/opencode/agents/legacy.md")).unwrap();
    assert!(opencode.contains("Legacy agent without name"));

    // Codex skips because parse_agent_meta returns None without name
    assert!(!home.join(".codex/agents/legacy.toml").exists());
}

#[test]
fn claude_shape_agents_bom_stripping_on_opencode_body() {
    let (_temp, repo_root, source_root, home) = fixture();
    fs::write(
        source_root.join("agents").join("e.md"),
        "\u{feff}---\nname: e\ndescription: Agent E with BOM\n---\nBody E content\n",
    )
    .unwrap();

    run_update_skills(&options(repo_root, source_root, home.clone())).unwrap();

    let opencode = fs::read_to_string(home.join(".config/opencode/agents/e.md")).unwrap();
    assert!(opencode.contains("description: |\n  Agent E with BOM"));
    let body_part = opencode.rsplit_once("---").unwrap().1;
    assert!(
        !body_part.contains("---"),
        "OpenCode agent body must not contain frontmatter delimiters"
    );
    assert!(body_part.contains("Body E content"));
    assert!(!body_part.contains("name: e"));
}

#[test]
fn claude_shape_agents_missing_description_skipped_with_warning() {
    let (_temp, repo_root, source_root, home) = fixture();
    fs::write(
        source_root.join("agents").join("b.md"),
        "---\nname: b\n---\nBody without description\n",
    )
    .unwrap();

    let evidence = run_update_skills(&options(repo_root, source_root, home.clone())).unwrap();

    assert!(
        !home.join(".copilot/agents/b.agent.md").exists(),
        "b.md (name only, missing description) must NOT be projected to Copilot"
    );
    assert!(
        !home.join(".codex/agents/b.toml").exists(),
        "b.md (name only, missing description) must NOT be projected to Codex"
    );
    assert!(
        !home.join(".config/opencode/agents/b.md").exists(),
        "b.md (name only, missing description) must NOT be projected to OpenCode"
    );

    let warnings: Vec<_> = evidence
        .run_warnings
        .iter()
        .filter(|w| w.contains("b.md"))
        .collect();
    assert_eq!(
        warnings.len(),
        1,
        "Expected exactly 1 warning for b.md, got {warnings:?}"
    );
    assert!(
        warnings[0].contains("description"),
        "Warning for b.md must indicate description issue, got: {}",
        warnings[0]
    );
}

#[test]
fn claude_shape_agents_block_scalar_description_skipped_with_warning() {
    let (_temp, repo_root, source_root, home) = fixture();
    fs::write(
        source_root.join("agents").join("c.md"),
        "---\nname: c\ndescription: |\n  multi line block description\n---\nBody C\n",
    )
    .unwrap();

    let evidence = run_update_skills(&options(repo_root, source_root, home.clone())).unwrap();

    assert!(
        !home.join(".copilot/agents/c.agent.md").exists(),
        "c.md (block-scalar description) must NOT be projected to Copilot"
    );
    assert!(
        !home.join(".codex/agents/c.toml").exists(),
        "c.md (block-scalar description) must NOT be projected to Codex"
    );
    assert!(
        !home.join(".config/opencode/agents/c.md").exists(),
        "c.md (block-scalar description) must NOT be projected to OpenCode"
    );

    let warnings: Vec<_> = evidence
        .run_warnings
        .iter()
        .filter(|w| w.contains("c.md"))
        .collect();
    assert_eq!(
        warnings.len(),
        1,
        "Expected exactly 1 warning for c.md, got {warnings:?}"
    );
    assert!(
        warnings[0].contains("description") && warnings[0].contains("|"),
        "Warning for c.md must indicate unsupported block scalar '|', got: {}",
        warnings[0]
    );
}

#[test]
fn tp09_spec_claude_shape_agents_full_matrix() {
    let (_temp, repo_root, source_root, home) = fixture();

    // Render root with:
    // a.md (name, description, model: sonnet)
    fs::write(
        source_root.join("agents").join("a.md"),
        "---\nname: a\ndescription: Agent A\nmodel: sonnet\n---\nBody A\n",
    )
    .unwrap();

    // README.md (no frontmatter)
    fs::write(
        source_root.join("agents").join("README.md"),
        "# Agent documentation\nThis file has no frontmatter.\n",
    )
    .unwrap();

    // b.md (name only)
    fs::write(
        source_root.join("agents").join("b.md"),
        "---\nname: b\n---\nBody B without description\n",
    )
    .unwrap();

    // c.md (block-scalar description)
    fs::write(
        source_root.join("agents").join("c.md"),
        "---\nname: c\ndescription: |\n  multi line description\n---\nBody C\n",
    )
    .unwrap();

    // d.agent.md (no name)
    fs::write(
        source_root.join("agents").join("d.agent.md"),
        "---\ndescription: Agent D\nmodel: claude-3-opus\n---\nBody D\n",
    )
    .unwrap();

    // Run projection update
    let evidence = run_update_skills(&options(
        repo_root.clone(),
        source_root.clone(),
        home.clone(),
    ))
    .unwrap();

    // Copilot gets a.agent.md without model line and d.agent.md verbatim
    let copilot_a = fs::read_to_string(home.join(".copilot/agents/a.agent.md"))
        .expect("Copilot must project a.agent.md");
    assert!(copilot_a.contains("name: a"));
    assert!(!copilot_a.contains("model:"));

    let copilot_d = fs::read_to_string(home.join(".copilot/agents/d.agent.md"))
        .expect("Copilot must project d.agent.md verbatim");
    assert!(copilot_d.contains("description: Agent D"));
    assert!(copilot_d.contains("model: claude-3-opus"));

    // Copilot: b, c, README must not exist
    assert!(
        !home.join(".copilot/agents/b.agent.md").exists(),
        "b.md must be skipped"
    );
    assert!(
        !home.join(".copilot/agents/c.agent.md").exists(),
        "c.md must be skipped"
    );
    assert!(!home.join(".copilot/agents/README.md").exists());
    assert!(!home.join(".copilot/agents/README.agent.md").exists());

    // Codex gets a.toml without model; d.agent.md has no name
    let codex_a =
        fs::read_to_string(home.join(".codex/agents/a.toml")).expect("Codex must project a.toml");
    assert!(codex_a.contains("name = \"a\""));
    assert!(!codex_a.contains("model ="));
    assert!(!home.join(".codex/agents/b.toml").exists());
    assert!(!home.join(".codex/agents/c.toml").exists());
    assert!(!home.join(".codex/agents/d.toml").exists());

    // OpenCode gets a.md and d.md
    let opencode_a = fs::read_to_string(home.join(".config/opencode/agents/a.md"))
        .expect("OpenCode must project a.md");
    assert!(opencode_a.contains("Agent A"));
    let opencode_d = fs::read_to_string(home.join(".config/opencode/agents/d.md"))
        .expect("OpenCode must project d.md");
    assert!(opencode_d.contains("Agent D"));
    assert!(!home.join(".config/opencode/agents/b.md").exists());
    assert!(!home.join(".config/opencode/agents/c.md").exists());
    assert!(!home.join(".config/opencode/agents/README.md").exists());

    // Evidence warnings name b.md and c.md once each, never README.md
    let b_warnings: Vec<_> = evidence
        .run_warnings
        .iter()
        .filter(|w| w.contains("b.md"))
        .collect();
    assert_eq!(
        b_warnings.len(),
        1,
        "Expected 1 warning for b.md, got {b_warnings:?}"
    );
    let c_warnings: Vec<_> = evidence
        .run_warnings
        .iter()
        .filter(|w| w.contains("c.md"))
        .collect();
    assert_eq!(
        c_warnings.len(),
        1,
        "Expected 1 warning for c.md, got {c_warnings:?}"
    );
    assert!(!evidence
        .run_warnings
        .iter()
        .any(|w| w.contains("README.md")));

    // Adding a.agent.md yields one precedence warning and Copilot a.agent.md equals .agent.md source
    fs::write(
        source_root.join("agents").join("a.agent.md"),
        "---\nname: a\ndescription: Agent A from agent.md\nmodel: gpt-4o\n---\nBody A from agent.md\n",
    )
    .unwrap();

    let evidence2 = run_update_skills(&options(
        repo_root.clone(),
        source_root.clone(),
        home.clone(),
    ))
    .unwrap();
    let precedence_warnings: Vec<_> = evidence2
        .run_warnings
        .iter()
        .filter(|w| w.contains("a.md") && w.contains("a.agent.md") && w.contains("precedence"))
        .collect();
    assert_eq!(precedence_warnings.len(), 1);
    let copilot_a_p2 = fs::read_to_string(home.join(".copilot/agents/a.agent.md")).unwrap();
    assert!(copilot_a_p2.contains("Agent A from agent.md"));
    assert!(copilot_a_p2.contains("model: gpt-4o"));

    // Removing a.agent.md rewrites Copilot a.agent.md from a.md
    fs::remove_file(source_root.join("agents").join("a.agent.md")).unwrap();
    run_update_skills(&options(repo_root, source_root, home.clone())).unwrap();
    let copilot_a_p3 = fs::read_to_string(home.join(".copilot/agents/a.agent.md")).unwrap();
    assert!(copilot_a_p3.contains("Agent A"));
    assert!(!copilot_a_p3.contains("model:"));
}

fn agent_input(source_id: &str, name: &str, content: &str) -> DirectAgentInput {
    DirectAgentInput {
        source_id: source_id.into(),
        name: name.into(),
        content: content.into(),
    }
}

fn assignment(target: ProjectionTargetId, input: DirectAgentInput) -> DirectAgentAssignment {
    DirectAgentAssignment { target, input }
}

fn assert_direct_owner(lockfile: &Path, path: &Path, source_id: &str) {
    let mut report = ProjectionReport::default();
    let registry = support::ManagedArtifactRegistry::load(lockfile, &mut report);
    assert!(
        registry.is_managed(path),
        "missing ownership proof for {}",
        path.display()
    );
    assert_eq!(
        registry.owning_source_of(path),
        Some(source_id),
        "wrong owner recorded for {}",
        path.display()
    );
}

#[test]
fn direct_agent_plan_keeps_every_providers_proof_in_one_batch() {
    let (_temp, _repo_root, _source_root, home) = fixture();
    let lockfile = home.join(".ccync/build/lock.json");
    let content = "---\nname: reviewer\ndescription: Review\n---\nBody\n";
    let publication = DirectAgentPublication {
        assignments: vec![
            assignment(
                ProjectionTargetId::Claude,
                agent_input("provider-one", "reviewer", content),
            ),
            assignment(
                ProjectionTargetId::Codex,
                agent_input("provider-one", "reviewer", content),
            ),
            assignment(
                ProjectionTargetId::Copilot,
                agent_input("provider-two", "auditor", content),
            ),
        ],
    };

    let evidence = publish_direct_agent_plan(&publication, &home, &lockfile, false);

    assert!(evidence
        .by_target
        .values()
        .all(|target| target.failure.is_none()));
    assert_direct_owner(
        &lockfile,
        &home.join(".claude/agents/reviewer.md"),
        "provider-one",
    );
    assert_direct_owner(
        &lockfile,
        &home.join(".codex/agents/reviewer.toml"),
        "provider-one",
    );
    assert_direct_owner(
        &lockfile,
        &home.join(".copilot/agents/auditor.agent.md"),
        "provider-two",
    );

    // A later batch that names only one provider keeps the other proofs.
    let second = DirectAgentPublication {
        assignments: vec![assignment(
            ProjectionTargetId::Copilot,
            agent_input(
                "provider-two",
                "auditor",
                "---\nname: auditor\ndescription: Audit\n---\nRevised\n",
            ),
        )],
    };
    publish_direct_agent_plan(&second, &home, &lockfile, false);

    assert_direct_owner(
        &lockfile,
        &home.join(".claude/agents/reviewer.md"),
        "provider-one",
    );
    assert_direct_owner(
        &lockfile,
        &home.join(".codex/agents/reviewer.toml"),
        "provider-one",
    );
    assert_direct_owner(
        &lockfile,
        &home.join(".copilot/agents/auditor.agent.md"),
        "provider-two",
    );
}

#[test]
fn direct_agent_plan_publishes_shared_agy_destination_once() {
    let (_temp, _repo_root, _source_root, home) = fixture();
    let lockfile = home.join(".ccync/build/lock.json");
    let content = "---\nname: reviewer\ndescription: Review\n---\nBody\n";
    let publication = DirectAgentPublication {
        assignments: vec![
            assignment(
                ProjectionTargetId::AgyCli,
                agent_input("provider-one", "reviewer", content),
            ),
            assignment(
                ProjectionTargetId::AgyGui,
                agent_input("provider-one", "reviewer", content),
            ),
        ],
    };

    let evidence = publish_direct_agent_plan(&publication, &home, &lockfile, false);

    let agy_path = home.join(".gemini/config/agents/reviewer.md");
    for target in [ProjectionTargetId::AgyCli, ProjectionTargetId::AgyGui] {
        let result = &evidence.by_target[&target];
        assert!(result.failure.is_none());
        assert_eq!(
            result
                .paths
                .iter()
                .filter(|path| path.path == agy_path)
                .count(),
            1,
            "{target:?} must receive one entry for the shared destination"
        );
    }
    assert!(fs::read_to_string(&agy_path).unwrap().contains("Body"));
    assert_direct_owner(&lockfile, &agy_path, "provider-one");
}

#[test]
fn direct_agent_plan_blocks_incompatible_shared_destination_before_writing() {
    let (_temp, _repo_root, _source_root, home) = fixture();
    let lockfile = home.join(".ccync/build/lock.json");
    let agy_path = home.join(".gemini/config/agents/reviewer.md");
    let publication = DirectAgentPublication {
        assignments: vec![
            assignment(
                ProjectionTargetId::AgyCli,
                agent_input(
                    "provider-one",
                    "reviewer",
                    "---\nname: reviewer\ndescription: Review\n---\nFirst\n",
                ),
            ),
            assignment(
                ProjectionTargetId::AgyGui,
                agent_input(
                    "provider-one",
                    "reviewer",
                    "---\nname: reviewer\ndescription: Review\n---\nSecond\n",
                ),
            ),
            assignment(
                ProjectionTargetId::Claude,
                agent_input(
                    "provider-one",
                    "auditor",
                    "---\nname: auditor\ndescription: Audit\n---\nBody\n",
                ),
            ),
        ],
    };

    let evidence = publish_direct_agent_plan(&publication, &home, &lockfile, false);

    assert!(!agy_path.exists());
    for target in [ProjectionTargetId::AgyCli, ProjectionTargetId::AgyGui] {
        let failure = evidence.by_target[&target]
            .failure
            .as_ref()
            .expect("conflicting shared destination must fail on every claimant");
        assert!(
            failure.reason.contains("incompatible"),
            "{}",
            failure.reason
        );
        assert_eq!(failure.path.as_deref(), Some(agy_path.as_path()));
    }
    assert!(evidence.by_target[&ProjectionTargetId::Claude]
        .failure
        .is_none());
    assert!(home.join(".claude/agents/auditor.md").exists());
}

#[test]
fn direct_agent_plan_blocks_one_destination_claimed_by_two_sources() {
    let (_temp, _repo_root, _source_root, home) = fixture();
    let lockfile = home.join(".ccync/build/lock.json");
    let publication = DirectAgentPublication {
        assignments: vec![
            assignment(
                ProjectionTargetId::Claude,
                agent_input(
                    "provider-one",
                    "reviewer",
                    "---\nname: reviewer\ndescription: One\n---\nOne\n",
                ),
            ),
            assignment(
                ProjectionTargetId::Claude,
                agent_input(
                    "provider-two",
                    "reviewer",
                    "---\nname: reviewer\ndescription: Two\n---\nTwo\n",
                ),
            ),
        ],
    };

    let evidence = publish_direct_agent_plan(&publication, &home, &lockfile, false);

    assert!(!home.join(".claude/agents/reviewer.md").exists());
    let failure = evidence.by_target[&ProjectionTargetId::Claude]
        .failure
        .as_ref()
        .expect("two sources for one destination must block it");
    assert!(failure.reason.contains("'provider-one' and 'provider-two'"));
}

#[test]
fn direct_agent_plan_leaves_agy_ide_unchanged_and_publishes_other_targets() {
    let (_temp, _repo_root, _source_root, home) = fixture();
    let lockfile = home.join(".ccync/build/lock.json");
    let content = "---\nname: reviewer\ndescription: Review\n---\nBody\n";
    let publication = DirectAgentPublication {
        assignments: vec![
            assignment(
                ProjectionTargetId::AgyIde,
                agent_input("provider-one", "reviewer", content),
            ),
            assignment(
                ProjectionTargetId::Claude,
                agent_input("provider-one", "reviewer", content),
            ),
        ],
    };

    let evidence = publish_direct_agent_plan(&publication, &home, &lockfile, false);

    let ide = &evidence.by_target[&ProjectionTargetId::AgyIde];
    assert!(ide.paths.is_empty());
    assert_eq!(
        ide.failure.as_ref().map(|failure| failure.reason.as_str()),
        Some("agent definitions are unsupported on agy-ide")
    );
    assert!(!home.join(".gemini/config/agents/reviewer.md").exists());
    assert!(home.join(".claude/agents/reviewer.md").exists());
}

#[test]
fn direct_agent_plan_failed_write_keeps_previous_bytes_and_proof() {
    let (_temp, _repo_root, _source_root, home) = fixture();
    let lockfile = home.join(".ccync/build/lock.json");
    let v1 = "---\nname: reviewer\ndescription: Review\n---\nVersion one\n";
    let v2 = "---\nname: reviewer\ndescription: Review\n---\nVersion two\n";
    let first = DirectAgentPublication {
        assignments: vec![
            assignment(
                ProjectionTargetId::Claude,
                agent_input("provider-one", "reviewer", v1),
            ),
            assignment(
                ProjectionTargetId::Codex,
                agent_input("provider-one", "reviewer", v1),
            ),
        ],
    };
    publish_direct_agent_plan(&first, &home, &lockfile, false);
    let claude_path = home.join(".claude/agents/reviewer.md");
    let foreign_path = home.join(".copilot/agents/foreign.agent.md");
    fs::create_dir_all(foreign_path.parent().unwrap()).unwrap();
    fs::write(&foreign_path, "foreign bytes").unwrap();
    // A directory at the temporary sibling makes the Claude replacement fail.
    fs::create_dir_all(home.join(".claude/agents/reviewer.md.tmp")).unwrap();

    let second = DirectAgentPublication {
        assignments: vec![
            assignment(
                ProjectionTargetId::Claude,
                agent_input("provider-one", "reviewer", v2),
            ),
            assignment(
                ProjectionTargetId::Codex,
                agent_input("provider-one", "reviewer", v2),
            ),
            assignment(
                ProjectionTargetId::Copilot,
                agent_input(
                    "provider-one",
                    "foreign",
                    "---\nname: foreign\ndescription: Foreign\n---\nBody\n",
                ),
            ),
        ],
    };

    let evidence = publish_direct_agent_plan(&second, &home, &lockfile, false);

    let claude_failure = evidence.by_target[&ProjectionTargetId::Claude]
        .failure
        .as_ref()
        .expect("blocked temporary sibling must fail the Claude write");
    assert_eq!(claude_failure.path.as_deref(), Some(claude_path.as_path()));
    assert!(evidence.by_target[&ProjectionTargetId::Codex]
        .failure
        .is_none());
    assert!(evidence.by_target[&ProjectionTargetId::Copilot]
        .warnings
        .iter()
        .any(|warning| warning.contains("preserved unowned agent destination")));
    assert!(fs::read_to_string(&claude_path)
        .unwrap()
        .contains("Version one"));
    assert_direct_owner(&lockfile, &claude_path, "provider-one");
    assert!(fs::read_to_string(home.join(".codex/agents/reviewer.toml"))
        .unwrap()
        .contains("Version two"));
    assert_eq!(fs::read_to_string(&foreign_path).unwrap(), "foreign bytes");
}

#[test]
fn tp04_spec_publish_multiple_agent_providers_shared_agy_codex_foreign_and_failure() {
    let (_temp, _repo_root, _source_root, home) = fixture();
    let lockfile = home.join(".ccync/build/lock.json");

    // Phase 1: Setup baseline with multiple providers, shared AGY, and Codex unknown sections.
    let agy_content = "---\nname: agy-agent\ndescription: AGY Shared Agent\n---\nAGY Body\n";
    let codex_content_v1 =
        "---\nname: codex-agent\ndescription: Codex Agent\nmodel: gpt-5.4\n---\nCodex Body V1\n";
    let claude_content_v1 =
        "---\nname: claude-agent\ndescription: Claude Agent\n---\nClaude Body V1\n";

    let initial_publication = DirectAgentPublication {
        assignments: vec![
            // Provider A targets shared AGY path via both AgyCli and AgyGui
            assignment(
                ProjectionTargetId::AgyCli,
                agent_input("provider-a", "agy-agent", agy_content),
            ),
            assignment(
                ProjectionTargetId::AgyGui,
                agent_input("provider-a", "agy-agent", agy_content),
            ),
            // Provider B targets Codex and Claude
            assignment(
                ProjectionTargetId::Codex,
                agent_input("provider-b", "codex-agent", codex_content_v1),
            ),
            assignment(
                ProjectionTargetId::Claude,
                agent_input("provider-b", "claude-agent", claude_content_v1),
            ),
        ],
    };

    let initial_evidence = publish_direct_agent_plan(&initial_publication, &home, &lockfile, false);

    // 1. Shared AGY destination published once across AgyCli and AgyGui
    let agy_path = home.join(".gemini/config/agents/agy-agent.md");
    assert!(agy_path.exists());
    assert!(fs::read_to_string(&agy_path).unwrap().contains("AGY Body"));
    assert_eq!(
        initial_evidence.by_target[&ProjectionTargetId::AgyCli]
            .paths
            .iter()
            .filter(|p| p.path == agy_path)
            .count(),
        1
    );
    assert_eq!(
        initial_evidence.by_target[&ProjectionTargetId::AgyGui]
            .paths
            .iter()
            .filter(|p| p.path == agy_path)
            .count(),
        1
    );
    assert_direct_owner(&lockfile, &agy_path, "provider-a");

    // Multiple providers retain proof in one lockfile
    let codex_path = home.join(".codex/agents/codex-agent.toml");
    let claude_path = home.join(".claude/agents/claude-agent.md");
    assert_direct_owner(&lockfile, &codex_path, "provider-b");
    assert_direct_owner(&lockfile, &claude_path, "provider-b");

    let records = direct_records(&lockfile);
    assert!(records.iter().any(
        |r| r.source_id.as_deref() == Some("provider-a") && r.kind == DirectArtifactKind::Agent
    ));
    assert!(records.iter().any(
        |r| r.source_id.as_deref() == Some("provider-b") && r.kind == DirectArtifactKind::Agent
    ));

    // 2. Add unknown sections to Codex agent file
    let mut codex_text = fs::read_to_string(&codex_path).unwrap();
    codex_text.push_str("\n[features]\nexperimental = true\n[custom_sec]\nkey = \"value\"\n");
    fs::write(&codex_path, codex_text).unwrap();

    // 3. Create a foreign (unowned) file on Copilot
    let foreign_path = home.join(".copilot/agents/foreign-agent.agent.md");
    fs::create_dir_all(foreign_path.parent().unwrap()).unwrap();
    fs::write(&foreign_path, "foreign agent content").unwrap();

    // 4. Inject write failure for Claude by making .tmp a directory
    fs::create_dir_all(home.join(".claude/agents/claude-agent.md.tmp")).unwrap();

    // Phase 2: Publish update with:
    // - Updated Codex agent content (Provider B)
    // - Updated Claude agent content (Provider B, should fail write)
    // - Attempt to publish to foreign Copilot path (Provider C, should be rejected)
    // - New Provider C agent on OpenCode (should succeed)
    let codex_content_v2 = "---\nname: codex-agent\ndescription: Codex Agent Updated\nmodel: gpt-5.4\n---\nCodex Body V2\n";
    let claude_content_v2 =
        "---\nname: claude-agent\ndescription: Claude Agent Updated\n---\nClaude Body V2\n";
    let foreign_claim = "---\nname: foreign-agent\ndescription: Foreign Claim\n---\nClaim Body\n";
    let opencode_content =
        "---\nname: opencode-agent\ndescription: OpenCode Agent\n---\nOpenCode Body\n";

    let update_publication = DirectAgentPublication {
        assignments: vec![
            assignment(
                ProjectionTargetId::Codex,
                agent_input("provider-b", "codex-agent", codex_content_v2),
            ),
            assignment(
                ProjectionTargetId::Claude,
                agent_input("provider-b", "claude-agent", claude_content_v2),
            ),
            assignment(
                ProjectionTargetId::Copilot,
                agent_input("provider-c", "foreign-agent", foreign_claim),
            ),
            assignment(
                ProjectionTargetId::Opencode,
                agent_input("provider-c", "opencode-agent", opencode_content),
            ),
        ],
    };

    let update_evidence = publish_direct_agent_plan(&update_publication, &home, &lockfile, false);

    // Codex unknown sections preserved while content updated
    let updated_codex_text = fs::read_to_string(&codex_path).unwrap();
    assert!(updated_codex_text.contains("Codex Body V2"));
    assert!(
        updated_codex_text.contains("[features]\nexperimental = true"),
        "Codex unknown section [features] must be preserved"
    );
    assert!(
        updated_codex_text.contains("[custom_sec]\nkey = \"value\""),
        "Codex unknown section [custom_sec] must be preserved"
    );
    assert_direct_owner(&lockfile, &codex_path, "provider-b");

    // Foreign file preserved and unowned write rejected
    assert_eq!(
        fs::read_to_string(&foreign_path).unwrap(),
        "foreign agent content"
    );
    assert!(update_evidence.by_target[&ProjectionTargetId::Copilot]
        .warnings
        .iter()
        .any(|w| w.contains("preserved unowned agent destination")));

    // Failed write on Claude retains previous bytes and prior proof
    let claude_failure = update_evidence.by_target[&ProjectionTargetId::Claude]
        .failure
        .as_ref()
        .expect("Claude write must fail due to directory collision");
    assert_eq!(claude_failure.path.as_deref(), Some(claude_path.as_path()));
    assert!(fs::read_to_string(&claude_path)
        .unwrap()
        .contains("Claude Body V1"));
    assert_direct_owner(&lockfile, &claude_path, "provider-b");

    // Provider A's shared AGY proof still intact from initial batch
    assert_direct_owner(&lockfile, &agy_path, "provider-a");

    // OpenCode succeeded and has proof for provider-c
    let opencode_path = home.join(".config/opencode/agents/opencode-agent.md");
    assert!(opencode_path.exists());
    assert_direct_owner(&lockfile, &opencode_path, "provider-c");
}
