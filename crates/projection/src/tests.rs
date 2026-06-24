use super::*;
use ccync_foundation::health::HealthCheck;
use ccync_foundation::platform::{create_dir_link, is_symlink_or_junction};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

#[cfg(windows)]
#[test]
fn remove_link_removes_directory_junction_not_target() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("keep.txt"), "keep").unwrap();
    let link = tmp.path().join("junction");
    let status = Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            &link.to_string_lossy(),
            &target.to_string_lossy(),
        ])
        .status()
        .unwrap();
    assert!(
        status.success(),
        "mklink /J failed to create the test junction"
    );
    assert!(
        is_symlink_or_junction(&link),
        "test setup: link should be a junction"
    );
    // Regression: before the fix `remove_link` gated on `is_dir()` (false for a
    // Windows junction) and fell to `fs::remove_file` → ERROR_ACCESS_DENIED (os error 5).
    remove_link(&link)
        .expect("remove_link must remove a directory junction (os error 5 regression)");
    assert!(!link.exists(), "junction must be removed");
    assert!(
        target.join("keep.txt").exists(),
        "removing a junction must NOT touch the target directory"
    );
}

// Prune path: a CCYNC-owned real-file residue (retired golem-agent copy without
// a CCYNC header, or a header-stamped file) that is no longer active gets
// removed even when absent from the lockfile (positive content ID = a2 escape
// hatch); a genuine non-CCYNC user file is preserved.
#[test]
fn prune_stale_links_removes_ccync_owned_real_files_keeps_user_file() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("agents");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("golem-analyst.agent.md"),
        "---\nname: golem-analyst\n---\nbody\n",
    )
    .unwrap();
    fs::write(
        root.join("golem-dockeeper.agent.md"),
        "---\nname: golem-dockeeper\n---\nbody\n",
    )
    .unwrap();
    fs::write(
        root.join("stale.toml"),
        format!("{CCYNC_MANAGED_FILE_HEADER}\nx = 1\n"),
    )
    .unwrap();
    fs::write(root.join("my-notes.md"), "personal notes\n").unwrap();

    let mut report = ProjectionReport::default();
    let registry = ManagedArtifactRegistry::load(&tmp.path().join("nolock.json"), &mut report);
    prune_stale_links(
        &root,
        ["golem-analyst.agent.md"],
        false,
        &mut report,
        &registry,
        &[],
    )
    .unwrap();

    assert!(
        root.join("golem-analyst.agent.md").exists(),
        "active agent must be kept"
    );
    assert!(
        !root.join("golem-dockeeper.agent.md").exists(),
        "retired golem-agent real file must be pruned"
    );
    assert!(
        !root.join("stale.toml").exists(),
        "CCYNC-header-stamped stale real file must be pruned"
    );
    assert!(
        root.join("my-notes.md").exists(),
        "non-CCYNC user file must be preserved"
    );
}

// Single source: projection's absent-state fallback for selected runtimes is
// ccync_foundation::runtime::VALID_RUNTIMES — the same set the install side now defaults
// to, so the two paths cannot disagree when install-state.json is missing.
#[test]
fn load_selected_runtimes_absent_state_falls_back_to_valid_runtimes() {
    let tmp = TempDir::new().unwrap();
    let got = load_selected_runtimes(tmp.path()).unwrap();
    let mut expected: Vec<String> = ccync_foundation::runtime::VALID_RUNTIMES
        .iter()
        .map(|r| (*r).to_string())
        .collect();
    expected.sort();
    expected.dedup();
    assert_eq!(got, expected);
}

fn fixture_roots() -> (TempDir, PathBuf, PathBuf, PathBuf) {
    let temp = TempDir::new().unwrap();
    let repo_root = temp.path().join("repo");
    let source_root = repo_root.join("plugins").join("ccync-core");
    let home = temp.path().join("home");
    fs::create_dir_all(source_root.join("skills").join("sample-skill")).unwrap();
    fs::create_dir_all(source_root.join("agents")).unwrap();
    fs::create_dir_all(source_root.join("commands")).unwrap();
    (temp, repo_root, source_root, home)
}

fn fixture_roots_with_appdata() -> (TempDir, PathBuf, PathBuf, PathBuf, PathBuf) {
    let (temp, repo_root, source_root, home) = fixture_roots();
    let appdata = temp.path().join("appdata");
    fs::create_dir_all(&appdata).unwrap();
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo_root)
        .output()
        .unwrap();
    (temp, repo_root, source_root, home, appdata)
}

fn write_skill_fixture(source_root: &Path, name: &str) {
    let skill_root = source_root.join("skills").join(name);
    fs::create_dir_all(&skill_root).unwrap();
    fs::write(skill_root.join("SKILL.md"), format!("# {name}\n")).unwrap();
}

fn write_command_fixture(source_root: &Path, name: &str) {
    let command_root = source_root.join("commands").join(name);
    fs::create_dir_all(&command_root).unwrap();
    fs::write(
        command_root.join("SKILL.template.md"),
        "---\ndescription: sample command\n---\nUse {{CCYNC_ROOT}}\n",
    )
    .unwrap();
    fs::write(command_root.join("SKILL.local.md"), "Local note\n").unwrap();
}

fn write_sync_fixture(repo_root: &Path, source_root: &Path) {
    fs::create_dir_all(repo_root.join(".dev")).unwrap();
    fs::write(
        repo_root.join(".dev").join("project.md"),
        "# Project\n\n| Layer | Technology |\n| --- | --- |\n| Language | Rust |\n",
    )
    .unwrap();
    fs::create_dir_all(source_root.join("conventions")).unwrap();
    fs::create_dir_all(source_root.join("workflows")).unwrap();
    fs::create_dir_all(source_root.join("skills").join("doc-sync")).unwrap();
    fs::write(
        source_root.join("conventions").join("conventions.md"),
        "# Conventions\n",
    )
    .unwrap();
    fs::write(
        source_root.join("conventions").join("token-budget.md"),
        "# Token Budget\n",
    )
    .unwrap();
    fs::write(
        source_root.join("conventions").join("working-hours.md"),
        "# Working Hours\n",
    )
    .unwrap();
    fs::write(source_root.join("conventions").join("rust.md"), "# Rust\n").unwrap();
    fs::write(
        source_root.join("workflows").join("coding.md"),
        "# Coding Flow\n",
    )
    .unwrap();
    fs::write(
        source_root.join("skills").join("doc-sync").join("SKILL.md"),
        "# doc-sync\n",
    )
    .unwrap();
}

#[test]
fn update_skills_projects_links_and_agents() {
    // Projection materializes directory junctions (no privilege) and copies agent
    // files via `write_text` — it never creates per-file symlinks, so this needs no
    // file-symlink privilege guard (cf. `agents_projected_verbatim_with_codex_toml_and_no_discuss`).
    let (_temp, repo_root, source_root, home) = fixture_roots();
    fs::write(
        source_root
            .join("skills")
            .join("sample-skill")
            .join("SKILL.md"),
        "# Skill",
    )
    .unwrap();
    fs::write(
        source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\ncolor: green\ntools: [read, edit, execute]\n---\nBody\n",
    )
    .unwrap();

    let report = run_update_skills(&SkillUpdateOptions {
        repo_root: repo_root.clone(),
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_runtimes: vec![
            "copilot".into(),
            "gemini-cli".into(),
            "agy-cli".into(),
            "opencode".into(),
        ],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(!report.created_links.is_empty());
    assert!(home
        .join(".agents")
        .join("skills")
        .join("sample-skill")
        .exists());
    assert!(home
        .join(".copilot")
        .join("agents")
        .join("helper.agent.md")
        .exists());
    assert!(home.join(".copilot").join("ccync").exists());
    assert!(home.join(".gemini").join("ccync").exists());
    assert!(home
        .join(".gemini")
        .join("antigravity-cli")
        .join("ccync")
        .exists());
    let rendered = fs::read_to_string(
        home.join(".config")
            .join("opencode")
            .join("agents")
            .join("helper.md"),
    )
    .unwrap();
    assert!(rendered.starts_with(CCYNC_MANAGED_FILE_HEADER));
    assert!(rendered.contains("color: green"));
    assert!(rendered.contains("  read: allow"));
    assert!(rendered.contains("  list: allow"));
    assert!(rendered.contains("  edit: allow"));
    assert!(rendered.contains("  bash: allow"));
}

#[test]
fn agents_projected_verbatim_with_codex_toml_and_no_discuss() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    fs::write(
        source_root
            .join("skills")
            .join("sample-skill")
            .join("SKILL.md"),
        "# Skill",
    )
    .unwrap();
    // Agent with abstract tools + a frontmatter model. Generic projection must
    // NOT rewrite tools and must source the Codex model from the frontmatter.
    let raw = "---\nname: golem-x\ndescription: d\nmodel: gpt-5.4\ntools: [read, edit]\n---\nCharter body\n";
    fs::write(source_root.join("agents").join("golem-x.agent.md"), raw).unwrap();

    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_runtimes: vec!["copilot".into(), "codex".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    // (1) Claude/copilot agent projected VERBATIM — abstract tools unchanged.
    let claude_agent = home
        .join(".copilot")
        .join("agents")
        .join("golem-x.agent.md");
    let projected = fs::read_to_string(&claude_agent).unwrap();
    assert_eq!(
        projected, raw,
        "Claude agent must be copied verbatim (no tool rewrite)"
    );
    assert!(
        projected.contains("tools: [read, edit]"),
        "abstract tools must NOT be rewritten"
    );

    // (2) Codex TOML projected, model sourced from frontmatter.
    let codex_toml = home.join(".codex").join("agents").join("golem-x.toml");
    let toml = fs::read_to_string(&codex_toml).unwrap();
    assert!(toml.contains("[agent]"));
    assert!(toml.contains("name = \"golem-x\""));
    assert!(
        toml.contains("model = \"gpt-5.4\""),
        "Codex model must come from frontmatter"
    );

    // (3) No discuss-skill residue.
    assert!(
        !home
            .join(".agents")
            .join("skills")
            .join("discuss-golem-x")
            .exists(),
        "no discuss-<role> skill must be projected"
    );
}

#[test]
fn update_skills_skips_shared_projection_without_codex_or_opencode() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    fs::write(
        source_root
            .join("skills")
            .join("sample-skill")
            .join("SKILL.md"),
        "# Skill",
    )
    .unwrap();

    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_runtimes: vec!["copilot".into(), "gemini-cli".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(!home
        .join(".agents")
        .join("skills")
        .join("sample-skill")
        .exists());
}

#[test]
fn healthcheck_warns_when_projection_missing() {
    let temp = TempDir::new().unwrap();
    let check = SkillsProjectionHealthCheck::with_path(temp.path().join("missing"));
    let findings = check.check();
    assert_eq!(findings.len(), 1);
    assert_eq!(
        findings[0].severity,
        ccync_foundation::health::Severity::Warning
    );
}

#[test]
fn healthcheck_accepts_existing_directory() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("skills");
    fs::create_dir_all(&path).unwrap();
    let check = SkillsProjectionHealthCheck::with_path(path);
    assert!(check.check().is_empty());
}

#[test]
fn render_opencode_agent_matches_legacy_shape() {
    let rendered = render_opencode_agent(
        "helper",
        &Frontmatter {
            description: Some("helper line 1\nhelper line 2".into()),
            color: Some("green".into()),
            tools: Vec::new(),
        },
        "Body",
    );

    assert_eq!(
            rendered,
            "# Generated by CCYNC Setup-Machine. Do not edit manually.\n---\ndescription: |\n  helper line 1\n  helper line 2\nmode: subagent\ncolor: green\npermission:\n  read: allow\n  list: allow\n---\n\nBody\n"
        );
}

#[test]
fn render_opencode_agent_expands_search_permissions() {
    let rendered = render_opencode_agent(
        "helper",
        &Frontmatter {
            description: Some("helper".into()),
            color: None,
            tools: vec!["search".into()],
        },
        "Body",
    );

    assert!(rendered.contains("  read: allow"));
    assert!(rendered.contains("  list: allow"));
    assert!(rendered.contains("  grep: allow"));
    assert!(rendered.contains("  glob: allow"));
}

#[test]
fn update_commands_bakes_and_projects_outputs() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "sample-command");

    let report = run_update_commands(&CommandUpdateOptions {
        repo_root: repo_root.clone(),
        sources: vec![ProjectionSource {
            root: source_root.clone(),
            id: "ccync-core".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_runtimes: vec!["gemini-cli".into(), "codex".into(), "opencode".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    let baked_path = home
        .join(".ccync")
        .join("plugins")
        .join("ccync")
        .join("commands")
        .join("sample-command")
        .join("SKILL.md");
    let baked = fs::read_to_string(&baked_path).unwrap();
    assert!(baked.contains(&repo_root.display().to_string()));
    assert!(baked.contains("CCYNC LOCAL OVERRIDE START"));
    assert!(!source_root
        .join("commands")
        .join("sample-command")
        .join("SKILL.md")
        .exists());

    // Codex command skill now lives at the shared ~/.agents/skills (official
    // path), not the legacy ~/.codex/skills link.
    let codex_skill = home
        .join(".agents")
        .join("skills")
        .join("sample-command")
        .join("SKILL.md");
    assert!(
        codex_skill.exists(),
        "codex command projected as a shared skill"
    );
    assert!(
        !home
            .join(".codex")
            .join("skills")
            .join("sample-command")
            .exists(),
        "legacy ~/.codex/skills command target removed"
    );

    let gemini = fs::read_to_string(
        home.join(".gemini")
            .join("commands")
            .join("sample-command.toml"),
    )
    .unwrap();
    assert!(gemini.starts_with(CCYNC_MANAGED_FILE_HEADER));
    assert!(gemini.contains("User command arguments, if any: {{args}}"));

    let opencode = fs::read_to_string(
        home.join(".config")
            .join("opencode")
            .join("commands")
            .join("sample-command.md"),
    )
    .unwrap();
    assert!(opencode.starts_with(CCYNC_MANAGED_FILE_HEADER));
    assert!(opencode.contains("User command arguments, if any: $ARGUMENTS"));
    assert!(!report.written_files.is_empty());
}

#[test]
fn update_commands_projects_copilot_command_skill() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "ccync-status");

    run_update_commands(&CommandUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_runtimes: vec!["copilot".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    let skill = home
        .join(".copilot")
        .join("skills")
        .join("ccync-status")
        .join("SKILL.md");
    assert!(
        skill.exists(),
        "copilot command must be projected as a skill (~/.copilot/skills/<name>/SKILL.md)"
    );
    let body = fs::read_to_string(&skill).unwrap();
    assert!(
        body.contains("sample command"),
        "projected skill carries the baked command body"
    );
}

#[test]
fn update_commands_cleans_command_skill_when_runtime_deselected() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "ccync-status");

    run_update_commands(&CommandUpdateOptions {
        repo_root: repo_root.clone(),
        sources: vec![ProjectionSource {
            root: source_root.clone(),
            id: "ccync-core".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_runtimes: vec!["codex".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();
    let shared = home.join(".agents").join("skills").join("ccync-status");
    assert!(shared.exists(), "codex command-skill written when selected");

    // Deselect codex: the CCYNC-written command-skill must be cleaned so it does
    // not linger in the shared dir and double-load for opencode/copilot.
    run_update_commands(&CommandUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_runtimes: vec!["gemini-cli".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();
    assert!(
        !shared.exists(),
        "command-skill removed when its runtime is deselected"
    );
}

#[test]
fn update_commands_opencode_only_gets_no_shared_command_skill() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "ccync-status");

    run_update_commands(&CommandUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_runtimes: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    // Double-load guard: opencode keeps its native command; the shared
    // ~/.agents/skills must NOT also carry a command-skill (that is gated on
    // codex being selected), so the same command does not appear twice.
    assert!(
        home.join(".config")
            .join("opencode")
            .join("commands")
            .join("ccync-status.md")
            .exists(),
        "opencode keeps its native command projection"
    );
    assert!(
        !home
            .join(".agents")
            .join("skills")
            .join("ccync-status")
            .exists(),
        "no shared command-skill when codex is not selected (no double-load)"
    );
}

#[test]
fn update_commands_removes_managed_outputs_for_unselected_runtimes() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "sample-command");

    let gemini_target = home
        .join(".gemini")
        .join("commands")
        .join("sample-command.toml");
    let opencode_target = home
        .join(".config")
        .join("opencode")
        .join("commands")
        .join("sample-command.md");
    let claude_target = home
        .join(".claude")
        .join("commands")
        .join("sample-command.md");
    fs::create_dir_all(gemini_target.parent().unwrap()).unwrap();
    fs::create_dir_all(opencode_target.parent().unwrap()).unwrap();
    fs::create_dir_all(claude_target.parent().unwrap()).unwrap();
    fs::write(
        &gemini_target,
        format!("{CCYNC_MANAGED_FILE_HEADER}\nold\n"),
    )
    .unwrap();
    fs::write(
        &opencode_target,
        format!("{CCYNC_MANAGED_FILE_HEADER}\nold\n"),
    )
    .unwrap();
    fs::write(
        &claude_target,
        format!("{CCYNC_MANAGED_FILE_HEADER}\nold\n"),
    )
    .unwrap();

    run_update_commands(&CommandUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_runtimes: vec!["copilot".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(!gemini_target.exists());
    assert!(!opencode_target.exists());
    assert!(!claude_target.exists());
}

#[test]
fn update_commands_prunes_obsolete_managed_files() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "sample-command");

    let gemini_commands = home.join(".gemini").join("commands");
    let opencode_commands = home.join(".config").join("opencode").join("commands");
    fs::create_dir_all(&gemini_commands).unwrap();
    fs::create_dir_all(&opencode_commands).unwrap();
    fs::write(
        gemini_commands.join("old-command.toml"),
        format!("{CCYNC_MANAGED_FILE_HEADER}\nold\n"),
    )
    .unwrap();
    fs::write(
        opencode_commands.join("old-command.md"),
        format!("{CCYNC_MANAGED_FILE_HEADER}\nold\n"),
    )
    .unwrap();

    run_update_commands(&CommandUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_runtimes: vec!["gemini-cli".into(), "opencode".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(!gemini_commands.join("old-command.toml").exists());
    assert!(!opencode_commands.join("old-command.md").exists());
    assert!(gemini_commands.join("sample-command.toml").exists());
    assert!(opencode_commands.join("sample-command.md").exists());
}

#[test]
fn update_commands_is_idempotent_and_preserves_untracked_tools() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "sample-command");

    let options = CommandUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_runtimes: vec!["gemini-cli".into()],
        dry_run: false,
        replace: true,
    };

    let first_report = run_update_commands(&options).unwrap();
    assert!(
        !first_report.written_files.is_empty(),
        "first sync should materialize managed artifacts"
    );

    let lockfile = home.join(".ccync").join("state").join("plugins.lock.json");
    let first_lockfile = fs::read_to_string(&lockfile).unwrap();
    assert!(
        first_lockfile.contains("\"_ccyncProjection\""),
        "projection marker must be persisted as the sync source of truth"
    );
    assert!(
        first_lockfile.contains("sample-command"),
        "managed command paths must be recorded in the projection marker"
    );

    let stale_managed_looking = home
        .join(".gemini")
        .join("commands")
        .join("old-command.toml");
    fs::write(
        &stale_managed_looking,
        format!("{CCYNC_MANAGED_FILE_HEADER}\nuser-owned\n"),
    )
    .unwrap();
    let unrelated_claude_tool = home.join(".claude").join("plugins").join("my-tool");
    fs::create_dir_all(&unrelated_claude_tool).unwrap();

    let second_report = run_update_commands(&options).unwrap();

    assert!(
        second_report.written_files.is_empty(),
        "second sync should be idempotent when inputs do not change"
    );
    assert!(
        second_report.removed_paths.is_empty(),
        "second sync must not delete untracked surfaces"
    );
    assert!(
        second_report.warnings.is_empty(),
        "stable rerun should not emit cleanup warnings"
    );
    assert!(
        stale_managed_looking.exists(),
        "files not tracked in plugins.lock.json must be preserved even if they look CCYNC-managed"
    );
    assert!(
        unrelated_claude_tool.exists(),
        "sync must not delete unrelated non-CCYNC Claude tools"
    );
    assert_eq!(
        fs::read_to_string(&lockfile).unwrap(),
        first_lockfile,
        "idempotent sync must leave the projection marker unchanged"
    );
}

#[test]
fn update_commands_preserves_user_owned_shared_skill_link() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "sample-command");

    let external_target = home.join("external-command");
    fs::create_dir_all(&external_target).unwrap();
    let shared_target = home.join(".agents").join("skills").join("sample-command");
    fs::create_dir_all(shared_target.parent().unwrap()).unwrap();
    create_dir_link(&external_target, &shared_target).unwrap();

    run_update_commands(&CommandUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_runtimes: vec!["gemini-cli".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(shared_target.exists());
    assert_eq!(
        fs::canonicalize(&shared_target).unwrap(),
        fs::canonicalize(&external_target).unwrap()
    );
}

#[test]
fn update_commands_preserves_user_owned_shared_command_skill_dir() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "sample-command");

    // A genuine user directory at the shared skills target (same name as a
    // command) must be preserved, not clobbered by the command-skill write.
    let shared_target = home.join(".agents").join("skills").join("sample-command");
    fs::create_dir_all(&shared_target).unwrap();
    fs::write(shared_target.join("SKILL.md"), "user's own skill\n").unwrap();
    fs::write(shared_target.join("user.txt"), "keep").unwrap();

    let report = run_update_commands(&CommandUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_runtimes: vec!["codex".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert_eq!(
        fs::read_to_string(shared_target.join("SKILL.md")).unwrap(),
        "user's own skill\n",
        "user's SKILL.md must not be overwritten"
    );
    assert!(shared_target.join("user.txt").exists());
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.message.contains("preserved user-owned path")));
}

#[test]
fn update_personalization_writes_context_bridges_and_routing() {
    let (_temp, repo_root, source_root, home, appdata) = fixture_roots_with_appdata();
    write_skill_fixture(&source_root, "sample-skill");
    fs::create_dir_all(
        repo_root
            .join("plugins")
            .join("ccync-core")
            .join("templates"),
    )
    .unwrap();
    fs::write(
        repo_root
            .join("plugins")
            .join("ccync-core")
            .join("templates")
            .join("executor-routing.example.json"),
        "{\n  \"coder\": \"gpt-5.4\"\n}\n",
    )
    .unwrap();
    fs::create_dir_all(home.join(".ccync").join("config")).unwrap();
    fs::write(
        home.join(".ccync").join("config").join("config.json"),
        "{\n  \"devMode\": true\n}\n",
    )
    .unwrap();
    fs::create_dir_all(home.join(".gemini")).unwrap();
    fs::write(
        home.join(".gemini").join("settings.json"),
        "{\n  \"context\": {\"fileName\": [\"README.md\"]}\n}\n",
    )
    .unwrap();
    fs::create_dir_all(appdata.join("Code").join("User")).unwrap();

    run_update_personalization(&PersonalizationUpdateOptions {
        repo_root: repo_root.clone(),
        source_root: source_root.clone(),
        user_home: home.clone(),
        appdata_root: appdata.clone(),
        selected_runtimes: vec!["copilot".into(), "gemini-cli".into()],
        dry_run: false,
        replace: true,
        uninstall: false,
    })
    .unwrap();

    let context = fs::read_to_string(home.join(".gemini").join("ccync-context.md")).unwrap();
    assert!(context.contains(&format!(
        "@{}",
        source_root
            .join("skills")
            .join("sample-skill")
            .join("SKILL.md")
            .display()
    )));

    let settings: Value = serde_json::from_str(
        &fs::read_to_string(home.join(".gemini").join("settings.json")).unwrap(),
    )
    .unwrap();
    let file_names = settings["context"]["fileName"].as_array().unwrap();
    assert!(file_names.iter().any(|value| value == "README.md"));
    assert!(file_names.iter().any(|value| value == "AGENTS.md"));
    assert!(file_names.iter().any(|value| value == "GEMINI.md"));

    let vscode: Value = serde_json::from_str(
        &fs::read_to_string(appdata.join("Code").join("User").join("settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        vscode["chat.agentSkillsLocations"]["~/.agents/skills"],
        Value::Bool(false)
    );

    // D-08: routing-template copy + source-repo git config-filter registration
    // were removed (dead ccync-workflow); no longer asserted.
    let _ = &repo_root;
}

#[test]
fn update_personalization_uninstall_removes_gemini_context() {
    let (_temp, repo_root, source_root, home, appdata) = fixture_roots_with_appdata();
    write_skill_fixture(&source_root, "sample-skill");
    fs::create_dir_all(home.join(".gemini").join("antigravity-cli").join("plugins")).unwrap();
    fs::create_dir_all(home.join(".ccync").join("config")).unwrap();
    fs::write(
        home.join(".ccync").join("config").join("config.json"),
        "{\n  \"devMode\": true\n}\n",
    )
    .unwrap();
    fs::create_dir_all(home.join(".gemini")).unwrap();
    fs::write(home.join(".gemini").join("ccync-context.md"), "old\n").unwrap();
    fs::create_dir_all(
        home.join(".gemini")
            .join("antigravity-cli")
            .join("plugins")
            .join("ccync"),
    )
    .unwrap();

    run_update_personalization(&PersonalizationUpdateOptions {
        repo_root,
        source_root,
        user_home: home.clone(),
        appdata_root: appdata,
        selected_runtimes: vec!["gemini-cli".into(), "agy-cli".into()],
        dry_run: false,
        replace: true,
        uninstall: true,
    })
    .unwrap();

    assert!(!home.join(".gemini").join("ccync-context.md").exists());
    // D-08: AGY plugin lifecycle is owned by install.rs `AgyProjection`, not
    // personalization — no longer asserted here.
}

#[test]
fn update_personalization_preserves_existing_vscode_skill_locations() {
    let (_temp, repo_root, source_root, home, appdata) = fixture_roots_with_appdata();
    write_skill_fixture(&source_root, "sample-skill");
    fs::create_dir_all(appdata.join("Code").join("User")).unwrap();
    fs::write(
        appdata.join("Code").join("User").join("settings.json"),
        "{\n  \"chat.agentSkillsLocations\": {\n    \"/custom/path\": true\n  }\n}\n",
    )
    .unwrap();

    run_update_personalization(&PersonalizationUpdateOptions {
        repo_root,
        source_root,
        user_home: home,
        appdata_root: appdata.clone(),
        selected_runtimes: vec!["copilot".into()],
        dry_run: false,
        replace: true,
        uninstall: false,
    })
    .unwrap();

    let vscode: Value = serde_json::from_str(
        &fs::read_to_string(appdata.join("Code").join("User").join("settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        vscode["chat.agentSkillsLocations"]["/custom/path"],
        Value::Bool(true)
    );
    assert_eq!(
        vscode["chat.agentSkillsLocations"]["~/.agents/skills"],
        Value::Bool(false)
    );
}

#[test]
fn update_personalization_dry_run_does_not_write_agy_projection() {
    // AGY projection is owned by `crate::agy::AgyProjection`
    // inside `build_agy_plugin`, not a spawned build script. Dry-run must record
    // the would-be target without touching disk — assert the user-visible
    // invariant (the AGY install target is never created under --dry-run).
    let (_temp, repo_root, source_root, home, appdata) = fixture_roots_with_appdata();
    write_skill_fixture(&source_root, "sample-skill");
    fs::create_dir_all(home.join(".ccync").join("config")).unwrap();
    fs::write(
        home.join(".ccync").join("config").join("config.json"),
        "{\n  \"devMode\": true\n}\n",
    )
    .unwrap();

    let agy_install_target = home
        .join(".gemini")
        .join("antigravity-cli")
        .join("plugins")
        .join("ccync");

    run_update_personalization(&PersonalizationUpdateOptions {
        repo_root,
        source_root,
        user_home: home,
        appdata_root: appdata,
        selected_runtimes: vec!["agy-cli".into()],
        dry_run: true,
        replace: true,
        uninstall: false,
    })
    .unwrap();

    assert!(
        !agy_install_target.exists(),
        "dry-run must not create the AGY projection target on disk"
    );
}

#[test]
fn registry_mark_with_source_records_and_persists_attribution() {
    use crate::support::{ManagedArtifactRegistry, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let lockfile = tmp.path().join("plugins.lock.json");
    let path_a = tmp.path().join("skill-a");
    let path_b = tmp.path().join("skill-b");

    // Write an initial lockfile with no attribution so the first load is a no-attribution baseline.
    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);

    reg.mark(MANAGED_SKILL_PATHS, &path_a);
    reg.mark_with_source(MANAGED_SKILL_PATHS, &path_b, "my-bundle");
    reg.persist(false, &mut report).unwrap();

    // Reload and verify attribution is present for path_b, absent for path_a.
    let reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    assert_eq!(
        reg2.owning_source_of(&path_b),
        Some("my-bundle"),
        "reload must preserve source attribution for mark_with_source path"
    );
    assert_eq!(
        reg2.owning_source_of(&path_a),
        None,
        "paths marked without source must have no attribution"
    );
}

// ── ManagedArtifactRegistry — personal plugin projection safety ──

/// Non-CCYNC paths are NOT in the registry → can_mutate() must return false
/// once a lockfile has been written (i.e. prior is non-empty).
/// This guarantees that personal plugin projection never deletes a user-owned
/// file at an unrelated path.
#[test]
fn registry_can_mutate_false_for_non_managed_path_with_populated_prior() {
    use crate::support::{ManagedArtifactRegistry, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let lockfile = tmp.path().join("plugins.lock.json");
    let ccync_managed_path = tmp.path().join("ccync-managed-skill");
    let user_path = tmp.path().join("user-owned-file");

    // Write a lockfile recording ccync_managed_path.
    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg.mark(MANAGED_SKILL_PATHS, &ccync_managed_path);
    reg.persist(false, &mut report).unwrap();

    // Reload — prior is now non-empty.
    let reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);

    // CCYNC-managed path → can_mutate() = true.
    assert!(
        reg2.can_mutate(&ccync_managed_path),
        "CCYNC-managed path must be mutable"
    );
    // User-owned path (never in the registry) → can_mutate() = false.
    assert!(
        !reg2.can_mutate(&user_path),
        "non-CCYNC path must NOT be mutable (prune safety)"
    );
}

/// Personal plugin artifacts are marked via mark() during projection.
/// After persist + reload, those paths satisfy can_mutate() = true
/// (they are CCYNC-managed), while a different user path remains false.
#[test]
fn registry_personal_plugin_artifacts_are_managed_after_mark() {
    use crate::support::{ManagedArtifactRegistry, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let lockfile = tmp.path().join("plugins.lock.json");
    let personal_skill = tmp.path().join("personal-skill-projected");
    let unrelated_user_file = tmp.path().join("claude-plugins-external-never-touched");

    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    // Simulates the projection of a personal plugin skill artifact.
    reg.mark(MANAGED_SKILL_PATHS, &personal_skill);
    reg.persist(false, &mut report).unwrap();

    let reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    assert!(
        reg2.can_mutate(&personal_skill),
        "personal plugin projected artifact must be mutable (CCYNC owns it)"
    );
    assert!(
        !reg2.can_mutate(&unrelated_user_file),
        "unrelated user file must NOT be mutable — prune must never delete it"
    );
}

#[test]
fn update_skills_projects_two_sources_to_shared_skills() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    // ccync-core: skill-a
    write_skill_fixture(&source_root, "skill-a");
    // bundle source: skill-b (different name → no collision)
    let bundle_root = _temp.path().join("bundle");
    fs::create_dir_all(bundle_root.join("skills").join("skill-b")).unwrap();
    fs::write(
        bundle_root.join("skills").join("skill-b").join("SKILL.md"),
        "# skill-b\n",
    )
    .unwrap();
    fs::create_dir_all(bundle_root.join("agents")).unwrap();

    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![
            ProjectionSource {
                root: source_root.clone(),
                id: "ccync-core".into(),
                persistent: true,
            },
            ProjectionSource {
                root: bundle_root,
                id: "my-bundle".into(),
                persistent: false,
            },
        ],
        user_home: home.clone(),
        selected_runtimes: vec!["codex".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    let shared = home.join(".agents").join("skills");
    assert!(
        shared.join("skill-a").exists(),
        "ccync-core skill must be projected"
    );
    assert!(
        shared.join("skill-b").exists(),
        "bundle skill must be projected"
    );
}

#[test]
fn update_skills_single_source_output_unchanged() {
    // Single-source (1-element Vec) must be byte-identical to pre-change behavior.
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "sample-skill");

    let report = run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_runtimes: vec!["codex".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(home
        .join(".agents")
        .join("skills")
        .join("sample-skill")
        .exists());
    assert!(
        report.warnings.is_empty(),
        "single-source must emit no warnings"
    );
}

#[test]
fn agy_gui_projects_decomposed_skills_and_mcp_config() {
    // Probe: agy-gui native projection — decomposed managed skills into
    // ~/.gemini/antigravity/skills/<name> + MCP servers into mcp_config.json
    // (NOT a whole-plugin junction).
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "doc-sync");

    // The canonical .mcp.json (engine-rendered) is the GUI MCP source.
    let canonical = home.join(".ccync").join("plugins").join("ccync");
    fs::create_dir_all(&canonical).unwrap();
    fs::write(
        canonical.join(".mcp.json"),
        r#"{"servers":{"memory":{"command":"npx","args":["-y","srv"]}}}"#,
    )
    .unwrap();

    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_runtimes: vec!["agy-gui".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    let gui_skill = home
        .join(".gemini")
        .join("antigravity")
        .join("skills")
        .join("doc-sync");
    assert!(
        gui_skill.exists(),
        "managed skill must be projected into ~/.gemini/antigravity/skills/"
    );

    let mcp_config = home
        .join(".gemini")
        .join("antigravity")
        .join("mcp_config.json");
    assert!(
        mcp_config.exists(),
        "MCP must be written to ~/.gemini/antigravity/mcp_config.json"
    );
    let body = fs::read_to_string(&mcp_config).unwrap();
    assert!(
        body.contains("mcpServers"),
        "GUI config uses the mcpServers key"
    );
    assert!(
        body.contains("memory"),
        "managed server must appear in the GUI MCP config"
    );
}

#[test]
fn agy_gui_not_selected_writes_no_gui_surface() {
    // Negative: when agy-gui is absent from the selected set the GUI skills
    // dir stays empty and no mcp_config.json is written.
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "doc-sync");

    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_runtimes: vec!["codex".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    let gui_skill = home
        .join(".gemini")
        .join("antigravity")
        .join("skills")
        .join("doc-sync");
    assert!(
        !gui_skill.exists(),
        "no GUI skill link without agy-gui selected"
    );
    let mcp_config = home
        .join(".gemini")
        .join("antigravity")
        .join("mcp_config.json");
    assert!(
        !mcp_config.exists(),
        "no GUI mcp_config.json without agy-gui selected"
    );
}

#[test]
fn registry_attribution_round_trips_multiple_sources() {
    use crate::support::{ManagedArtifactRegistry, MANAGED_AGENT_PATHS, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let lockfile = tmp.path().join("plugins.lock.json");

    let paths = [
        (tmp.path().join("skill-core"), "ccync-core"),
        (tmp.path().join("skill-bundle"), "my-bundle"),
        (tmp.path().join("agent-bundle"), "my-bundle"),
    ];

    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);

    reg.mark_with_source(MANAGED_SKILL_PATHS, &paths[0].0, paths[0].1);
    reg.mark_with_source(MANAGED_SKILL_PATHS, &paths[1].0, paths[1].1);
    reg.mark_with_source(MANAGED_AGENT_PATHS, &paths[2].0, paths[2].1);
    reg.persist(false, &mut report).unwrap();

    let reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    for (path, expected_source) in &paths {
        assert_eq!(
            reg2.owning_source_of(path),
            Some(*expected_source),
            "source attribution for {} must survive round-trip",
            path.display()
        );
    }
}

// Collision policy: core-wins + additive-only + warn
#[test]
fn update_skills_collision_core_wins_emits_warning() {
    // ccync-core and bundle both declare "shared-skill" → core item preserved, bundle skipped + warning.
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "shared-skill");
    write_skill_fixture(&source_root, "core-only");

    let bundle_root = _temp.path().join("bundle-collision");
    fs::create_dir_all(bundle_root.join("skills").join("shared-skill")).unwrap();
    fs::write(
        bundle_root
            .join("skills")
            .join("shared-skill")
            .join("SKILL.md"),
        "# bundle version\n",
    )
    .unwrap();
    fs::create_dir_all(bundle_root.join("agents")).unwrap();

    let report = run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![
            ProjectionSource {
                root: source_root.clone(),
                id: "ccync-core".into(),
                persistent: true,
            },
            ProjectionSource {
                root: bundle_root,
                id: "my-bundle".into(),
                persistent: false,
            },
        ],
        user_home: home.clone(),
        selected_runtimes: vec!["codex".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    // Core item must be projected.
    let shared_dir = home.join(".agents").join("skills").join("shared-skill");
    assert!(
        shared_dir.exists(),
        "core skill must be projected even when bundle collides"
    );

    // Exactly one warning must mention the collision.
    assert_eq!(
        report.warnings.len(),
        1,
        "exactly one collision warning expected"
    );
    let msg = &report.warnings[0].message;
    assert!(
        msg.contains("shared-skill") && msg.contains("my-bundle") && msg.contains("ccync-core"),
        "warning must name the skill and both sources; got: {msg}"
    );
}

#[test]
fn update_skills_collision_non_core_first_wins_emits_warning() {
    // Two non-core sources collide → first-loaded wins, second skipped + warning.
    let (_temp, repo_root, source_root, home) = fixture_roots();
    // ccync-core has no "conflict-skill"
    fs::create_dir_all(source_root.join("skills")).unwrap();
    fs::create_dir_all(source_root.join("agents")).unwrap();

    let bundle_a = _temp.path().join("bundle-a");
    fs::create_dir_all(bundle_a.join("skills").join("conflict-skill")).unwrap();
    fs::write(
        bundle_a
            .join("skills")
            .join("conflict-skill")
            .join("SKILL.md"),
        "# A\n",
    )
    .unwrap();
    fs::create_dir_all(bundle_a.join("agents")).unwrap();

    let bundle_b = _temp.path().join("bundle-b");
    fs::create_dir_all(bundle_b.join("skills").join("conflict-skill")).unwrap();
    fs::write(
        bundle_b
            .join("skills")
            .join("conflict-skill")
            .join("SKILL.md"),
        "# B\n",
    )
    .unwrap();
    fs::create_dir_all(bundle_b.join("agents")).unwrap();

    let report = run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![
            ProjectionSource {
                root: source_root.clone(),
                id: "ccync-core".into(),
                persistent: true,
            },
            ProjectionSource {
                root: bundle_a,
                id: "bundle-a".into(),
                persistent: false,
            },
            ProjectionSource {
                root: bundle_b,
                id: "bundle-b".into(),
                persistent: false,
            },
        ],
        user_home: home.clone(),
        selected_runtimes: vec!["codex".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    // First non-core source wins → skill exists.
    assert!(
        home.join(".agents")
            .join("skills")
            .join("conflict-skill")
            .exists(),
        "first non-core source skill must be projected"
    );

    // Exactly one warning for the second source collision.
    assert_eq!(
        report.warnings.len(),
        1,
        "exactly one collision warning expected"
    );
    let msg = &report.warnings[0].message;
    assert!(
        msg.contains("conflict-skill") && msg.contains("bundle-b") && msg.contains("bundle-a"),
        "warning must name skill and both non-core sources; got: {msg}"
    );
}

// Per-source prune safety: source still installed → skip even if absent from invocation
#[test]
fn prune_stale_links_skips_items_from_installed_source() {
    use crate::support::{prune_stale_links, ManagedArtifactRegistry, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();

    // Simulate two previously-projected skills: one from ccync-core, one from my-bundle.
    let core_skill = root.join("core-skill");
    let bundle_skill = root.join("bundle-skill");
    fs::create_dir_all(&core_skill).unwrap();
    fs::create_dir_all(&bundle_skill).unwrap();

    let lockfile = tmp.path().join("lock.json");
    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg.mark_with_source(MANAGED_SKILL_PATHS, &core_skill, "ccync-core");
    reg.mark_with_source(MANAGED_SKILL_PATHS, &bundle_skill, "my-bundle");
    reg.persist(false, &mut report).unwrap();

    // Reload (simulate a fresh invocation).
    let reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);

    // Only ccync-core in keep_names (bundle-skill not projected this invocation).
    // installed_source_ids includes my-bundle → bundle-skill must NOT be pruned.
    prune_stale_links(
        &root,
        ["core-skill"],
        false,
        &mut report,
        &reg2,
        &["ccync-core", "my-bundle"],
    )
    .unwrap();

    assert!(core_skill.exists(), "core skill in keep list must be kept");
    assert!(
        bundle_skill.exists(),
        "bundle skill from installed source must NOT be pruned even when absent from keep list"
    );
}

#[test]
fn prune_stale_links_removes_items_from_removed_source() {
    use crate::support::{prune_stale_links, ManagedArtifactRegistry, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();

    let core_skill = root.join("core-skill");
    let bundle_skill = root.join("bundle-skill");
    fs::create_dir_all(&core_skill).unwrap();
    fs::create_dir_all(&bundle_skill).unwrap();

    // Write a CCYNC-managed file inside the dir so is_ccync_owned_real_path authorizes removal.
    // Reference the constant (not a hardcoded string) so renaming the marker breaks
    // this fixture at compile time instead of silently desyncing.
    fs::write(
        bundle_skill.join("SKILL.md"),
        format!("{CCYNC_MANAGED_FILE_HEADER}\n# bundle-skill\n"),
    )
    .unwrap();

    let lockfile = tmp.path().join("lock.json");
    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg.mark_with_source(MANAGED_SKILL_PATHS, &core_skill, "ccync-core");
    reg.mark_with_source(MANAGED_SKILL_PATHS, &bundle_skill, "my-bundle");
    reg.persist(false, &mut report).unwrap();

    let reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);

    // my-bundle removed from installed_source_ids → bundle-skill IS prunable.
    prune_stale_links(
        &root,
        ["core-skill"],
        false,
        &mut report,
        &reg2,
        &["ccync-core"],
    )
    .unwrap();

    assert!(core_skill.exists(), "core skill must survive");
    assert!(
        !bundle_skill.exists(),
        "bundle skill from removed source must be pruned"
    );
}

// Source assembly: collect_projection_sources returns ccync-core 1-element list by default
#[test]
fn collect_projection_sources_returns_ccync_core_single_source() {
    use crate::collect_projection_sources;

    let tmp = TempDir::new().unwrap();
    let source_root = tmp.path().to_path_buf();

    let sources = collect_projection_sources(&source_root);
    assert_eq!(
        sources.len(),
        1,
        "default assembly must return exactly 1 source"
    );
    assert_eq!(sources[0].id, "ccync");
    assert_eq!(sources[0].root, source_root);
    assert!(sources[0].persistent, "ccync source must be persistent");
}

// Verifies that persist() does not write ccync-self.json when the registry is empty
// (byte-identical guarantee for the no-writer stage). An empty registry fires the
// equality guard (prior == next, both empty BTreeMaps) and returns early without
// calling write_text — the file stays absent.
#[test]
fn registry_empty_ccync_self_no_write_on_persist() {
    use crate::support::ManagedArtifactRegistry;

    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("ccync-self.json");

    let mut report = ProjectionReport::default();
    let reg = ManagedArtifactRegistry::load(&path, &mut report);
    reg.persist(false, &mut report).unwrap();

    assert!(
        !path.exists(),
        "empty ccync-self registry must not write ccync-self.json (persist() non-empty guard)"
    );
}

// Interleaved coexistence: two registries at distinct lockfile paths (one
// plugins.lock.json-like for "ccync", one ccync-self.json for "ccync-self") share a
// projection output directory. Each registry owns a disjoint subset of artifacts
// via mark_with_source with a distinct source-id. Prune from each registry's side
// must not delete the other registry's artifacts, and a non-CCYNC external path must
// be untouched by both prune operations.
#[test]
fn registry_interleaved_coexistence_isolation() {
    use crate::support::{prune_stale_links, ManagedArtifactRegistry, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let shared_dir = tmp.path().join("skills");
    fs::create_dir_all(&shared_dir).unwrap();

    // Three entries in the shared output directory — directories (no CCYNC header)
    // so is_ccync_owned_real_path cannot authorize accidental removal.
    let ccync_artifact = shared_dir.join("ccync-skill");
    let ccync_self_artifact = shared_dir.join("ccync-self-skill");
    let external_path = shared_dir.join("user-external");
    fs::create_dir_all(&ccync_artifact).unwrap();
    fs::create_dir_all(&ccync_self_artifact).unwrap();
    fs::create_dir_all(&external_path).unwrap();

    let mut report = ProjectionReport::default();

    // Registry A (plugins.lock.json-like): records ccync-artifact as owned by "ccync".
    let lock_a = tmp.path().join("plugins.lock.json");
    let mut reg_a = ManagedArtifactRegistry::load(&lock_a, &mut report);
    reg_a.mark_with_source(MANAGED_SKILL_PATHS, &ccync_artifact, "ccync");
    reg_a.persist(false, &mut report).unwrap();

    // Registry B (ccync-self.json): records ccync-self-artifact as owned by "ccync-self".
    let lock_b = tmp.path().join("ccync-self.json");
    let mut reg_b = ManagedArtifactRegistry::load(&lock_b, &mut report);
    reg_b.mark_with_source(MANAGED_SKILL_PATHS, &ccync_self_artifact, "ccync-self");
    reg_b.persist(false, &mut report).unwrap();

    // Reload both registries to simulate a fresh invocation on each side.
    let reg_a2 = ManagedArtifactRegistry::load(&lock_a, &mut report);
    let reg_b2 = ManagedArtifactRegistry::load(&lock_b, &mut report);

    // --- Prune from registry A's perspective (source "ccync" installed) ---
    // ccync-skill is in keep_names → always kept.
    // ccync-self-skill is NOT in reg_a's prior → can_mutate = false → not authorized.
    // external_path is not in any registry → can_mutate = false → not authorized.
    prune_stale_links(
        &shared_dir,
        ["ccync-skill"],
        false,
        &mut report,
        &reg_a2,
        &["ccync"],
    )
    .unwrap();

    assert!(
        ccync_artifact.exists(),
        "ccync artifact must survive its own registry prune"
    );
    assert!(
        ccync_self_artifact.exists(),
        "ccync-self artifact must NOT be deleted by ccync-side prune (absent from ccync registry)"
    );
    assert!(
        external_path.exists(),
        "external non-CCYNC path must be untouched by ccync-side prune"
    );

    // --- Prune from registry B's perspective (source "ccync-self" installed) ---
    // ccync-self-skill is in keep_names → always kept.
    // ccync-skill is NOT in reg_b's prior → can_mutate = false → not authorized.
    // external_path is not in any registry → can_mutate = false → not authorized.
    prune_stale_links(
        &shared_dir,
        ["ccync-self-skill"],
        false,
        &mut report,
        &reg_b2,
        &["ccync-self"],
    )
    .unwrap();

    assert!(
        ccync_self_artifact.exists(),
        "ccync-self artifact must survive its own registry prune"
    );
    assert!(
            ccync_artifact.exists(),
            "ccync artifact must NOT be deleted by ccync-self-side prune (absent from ccync-self registry)"
        );
    assert!(
        external_path.exists(),
        "external non-CCYNC path must be untouched by ccync-self-side prune"
    );
}

// ── decompose_plugin (projection-input materialization) ────────────────────

#[test]
fn decompose_plugin_yields_component_lists() {
    let tmp = TempDir::new().unwrap();
    let plugin = tmp.path().join("my-plugin");
    // skills/<name>/SKILL.md
    for s in ["doc-sync", "git-commits"] {
        let d = plugin.join("skills").join(s);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("SKILL.md"), "x").unwrap();
    }
    // commands/<name>/
    fs::create_dir_all(plugin.join("commands").join("ccync-status")).unwrap();
    // agents/<name>.agent.md
    fs::create_dir_all(plugin.join("agents")).unwrap();
    fs::write(plugin.join("agents").join("golem-architect.agent.md"), "x").unwrap();
    // .mcp.json
    fs::write(
        plugin.join(".mcp.json"),
        r#"{"servers":{"memory":{},"fetch":{}}}"#,
    )
    .unwrap();

    let c = decompose_plugin(&plugin).unwrap();
    assert_eq!(c.skills, vec!["doc-sync", "git-commits"]);
    assert_eq!(c.commands, vec!["ccync-status"]);
    assert_eq!(c.agents, vec!["golem-architect.agent.md"]);
    assert_eq!(c.mcp_servers, vec!["fetch", "memory"]); // sorted
}

#[test]
fn decompose_plugin_absent_subtrees_are_empty() {
    let tmp = TempDir::new().unwrap();
    let plugin = tmp.path().join("empty-plugin");
    fs::create_dir_all(&plugin).unwrap();
    let c = decompose_plugin(&plugin).unwrap();
    assert!(c.skills.is_empty());
    assert!(c.commands.is_empty());
    assert!(c.agents.is_empty());
    assert!(c.mcp_servers.is_empty());
}

// ── single-mode resolve_runtime_roots (D-07) ───────────────────────────────

#[test]
fn resolve_runtime_roots_returns_canonical_root_no_err() {
    // Only env-mutating test in this crate's test binary (per-crate process).
    let tmp = TempDir::new().unwrap();
    #[cfg(windows)]
    unsafe {
        std::env::set_var("USERPROFILE", tmp.path());
    }
    #[cfg(not(windows))]
    unsafe {
        std::env::set_var("HOME", tmp.path());
    }

    // Single mode: no ccync-core on disk, default config → must NOT error and
    // must return the ccync canonical plugin root for both roots.
    let (repo_root, source_root) = resolve_runtime_roots(&CcyncConfig::default()).unwrap();
    assert_eq!(repo_root, source_root);
    assert!(
        repo_root.ends_with(Path::new("plugins").join("ccync")),
        "resolve_runtime_roots must return ~/.ccync/plugins/ccync, got {repo_root:?}"
    );
}
