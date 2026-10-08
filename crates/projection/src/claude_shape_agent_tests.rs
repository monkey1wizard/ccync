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
