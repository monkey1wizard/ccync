use super::*;
use ccync_foundation::health::HealthCheck;
use ccync_foundation::platform::{create_dir_link, is_symlink_or_junction};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

// Recursively snapshot every regular file's bytes and every symlink/junction's
// raw `read_link` target under `root`, keyed by path relative to `root`. Used
// by the idempotence and dry-run regressions below to assert an entire
// projected surface (files, link targets, and the lockfile living inside it)
// is byte-identical across two runs, without hardcoding every path a full
// pipeline touches.
fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let rel = rel.to_path_buf();
            if is_symlink_or_junction(&path) {
                let target = fs::read_link(&path)
                    .map(|t| t.to_string_lossy().into_owned())
                    .unwrap_or_default();
                out.insert(rel, target.into_bytes());
            } else if path.is_dir() {
                walk(root, &path, out);
            } else if path.is_file() {
                out.insert(rel, fs::read(&path).unwrap_or_default());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

// ── ProjectionEvidence test helpers ────────────────────────────────────────
//
// Several older tests were written against the legacy flat `ProjectionReport`
// (`written_files`/`removed_paths`/`created_links`/`warnings`, run-global, no
// target attribution). Now that every producer returns `ProjectionEvidence`
// directly, these helpers flatten `common` + every `by_target` bucket back
// into the same "any path/warning touched, anywhere" shape those tests
// actually need — deliberately collapsing attribution here (unlike the
// evidence-shaped tests elsewhere in this file, which assert attribution
// directly) since that is what these particular tests were checking.
fn evidence_paths(evidence: &ProjectionEvidence) -> impl Iterator<Item = &ProjectionPathResult> {
    evidence
        .common
        .iter()
        .chain(evidence.by_target.values().flat_map(|t| t.paths.iter()))
}

fn evidence_has_change(evidence: &ProjectionEvidence, change: ProjectionPathChange) -> bool {
    evidence_paths(evidence).any(|p| p.change == change)
}

fn evidence_all_warnings(evidence: &ProjectionEvidence) -> Vec<String> {
    let mut warnings = evidence.run_warnings.clone();
    for target in evidence.by_target.values() {
        warnings.extend(target.warnings.iter().cloned());
    }
    warnings
}

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

// Prune path (option B): only the CCYNC content header determines ownership;
// frontmatter alone is not enough. A header-stamped stale file is removed; a
// frontmatter-only file without a header is now treated as a user file and
// preserved even when absent from the active-set.
//
// Also exercises the pre-attribution regression: `stale.toml` has no
// `source_attribution` row at all (a lockfile predating attribution tracking,
// or an entry `mark`-ed without a source). `owning_source_of` returns `None`
// for it, which must fall through to normal authorization rather than being
// treated as foreign-owned — even while the registry carries an unrelated
// attribution row and `sources` is non-empty, matching the shape a real
// invocation with other installed sources would have.
#[test]
fn prune_stale_links_prunes_pre_attribution_entry_with_installed_sources() {
    use crate::support::MANAGED_SKILL_PATHS;

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("agents");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("my-analyst.agent.md"),
        "---\nname: my-analyst\n---\nbody\n",
    )
    .unwrap();
    fs::write(
        root.join("my-dockeeper.agent.md"),
        "---\nname: my-dockeeper\n---\nbody\n",
    )
    .unwrap();
    fs::write(
        root.join("stale.toml"),
        format!("{CCYNC_MANAGED_FILE_HEADER}\nx = 1\n"),
    )
    .unwrap();
    fs::write(root.join("my-notes.md"), "personal notes\n").unwrap();

    let lockfile = tmp.path().join("lock.json");
    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    // Attribution recorded for an unrelated path only — `stale.toml` itself
    // has no attribution row, but the registry and `sources` are non-empty.
    reg.mark_with_source(
        MANAGED_SKILL_PATHS,
        &tmp.path().join("other-skill"),
        "my-bundle",
    );
    reg.persist(&[], false, &mut report).unwrap();
    let reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);

    let sources = vec![
        ProjectionSource {
            root: PathBuf::new(),
            id: "ccync-core".into(),
            persistent: true,
        },
        ProjectionSource {
            root: PathBuf::new(),
            id: "my-bundle".into(),
            persistent: false,
        },
    ];
    prune_stale_links(&root, ["my-analyst.agent.md"], false, &reg2, &sources, None).unwrap();

    assert!(
        root.join("my-analyst.agent.md").exists(),
        "active agent must be kept"
    );
    assert!(
        root.join("my-dockeeper.agent.md").exists(),
        "frontmatter-only file without CCYNC header is now a user file — must be preserved"
    );
    assert!(
        !root.join("stale.toml").exists(),
        "entry with no attribution row must still fall through to normal authorization and be pruned"
    );
    assert!(
        root.join("my-notes.md").exists(),
        "non-CCYNC user file must be preserved"
    );
}

// Skip/failure exclusivity: a target's evidence cannot be built with both a
// skip reason and an expected failure at once.
#[test]
fn projection_target_evidence_rejects_skip_and_failure_together() {
    let result = ProjectionTargetEvidence::new(
        Vec::new(),
        Vec::new(),
        Some("nothing to project".to_string()),
        Some(ProjectionExpectedFailure {
            path: None,
            reason: "boom".to_string(),
        }),
    );
    assert!(result.is_err());

    let skip_only = ProjectionTargetEvidence::new(
        Vec::new(),
        Vec::new(),
        Some("nothing to project".to_string()),
        None,
    )
    .unwrap();
    assert!(skip_only.is_valid());

    let failure_only = ProjectionTargetEvidence::new(
        Vec::new(),
        Vec::new(),
        None,
        Some(ProjectionExpectedFailure {
            path: Some(PathBuf::from("/tmp/x")),
            reason: "boom".to_string(),
        }),
    )
    .unwrap();
    assert!(failure_only.is_valid());
}

// completed_targets and by_target are separate collections: a target can be
// completed with nothing to report (present in completed_targets, absent
// from by_target) versus never having run that phase at all (absent from
// completed_targets entirely). Both must be representable and distinguishable.
#[test]
fn projection_evidence_empty_completed_differs_from_absent() {
    let mut evidence = ProjectionEvidence::default();
    evidence.completed_targets.insert(ProjectionTargetId::Codex);

    assert!(
        evidence
            .completed_targets
            .contains(&ProjectionTargetId::Codex),
        "codex completed this phase"
    );
    assert!(
        !evidence.by_target.contains_key(&ProjectionTargetId::Codex),
        "codex completed with nothing to report — empty, not absent"
    );
    assert!(
        !evidence
            .completed_targets
            .contains(&ProjectionTargetId::Copilot),
        "copilot never ran this phase — absent, not merely empty"
    );
    assert!(!evidence
        .by_target
        .contains_key(&ProjectionTargetId::Copilot));
}

// Single source: projection's empty-selection fallback is the foundation's
// canonical target table — an absent/empty selection (LegacyState::Absent)
// falls back to all eight canonical projection targets.
#[test]
fn selected_projection_targets_empty_config_falls_back_to_canonical_targets() {
    let got = selected_projection_targets_from_config(&CcyncConfig::default());
    let mut expected: Vec<String> = ccync_foundation::target::CANONICAL_PROJECTION_TARGETS
        .iter()
        .map(|t| t.as_str().to_string())
        .collect();
    expected.sort();
    expected.dedup();
    assert_eq!(got, expected);
}

// Regression: machine projection must honor the runtime selection folded into
// config.json by the data-layout flatten, not the removed install-state.json.
#[test]
fn selected_projection_targets_honor_config_selection() {
    let config = CcyncConfig {
        selected_runtimes: vec!["codex".into(), "claude".into()],
        ..Default::default()
    };
    let got = selected_projection_targets_from_config(&config);
    assert_eq!(got, vec!["claude".to_string(), "codex".to_string()]);
}

// Canonical-only selection (LegacyState::Canonical): the canonical
// `selectedProjectionTargets` key must be honored even though the legacy
// `selectedRuntimes` key is empty. Before this delegated to the foundation
// resolver, this function only ever read `selected_runtimes`, so a
// canonical-only config silently fell through to the default-all fallback —
// this fixture proves that bug is fixed.
#[test]
fn selected_projection_targets_honor_canonical_only_selection() {
    let config = CcyncConfig {
        selected_projection_targets: vec!["codex".into(), "claude".into()],
        ..Default::default()
    };
    let got = selected_projection_targets_from_config(&config);
    assert_eq!(got, vec!["claude".to_string(), "codex".to_string()]);
}

// Both keys present (LegacyState::Both): canonical and legacy hold
// genuinely different values, so a passing assertion proves canonical truly
// wins rather than the two fixtures happening to agree.
#[test]
fn selected_projection_targets_both_present_canonical_wins() {
    let config = CcyncConfig {
        selected_projection_targets: vec!["codex".into(), "claude".into()],
        selected_runtimes: vec!["agy-gui".into()],
        ..Default::default()
    };
    let got = selected_projection_targets_from_config(&config);
    assert_eq!(got, vec!["claude".to_string(), "codex".to_string()]);
}

// ManagedDeprojection authority (Protected Path) — registry-owned path
// authorized for removal; non-managed same-named sibling NOT authorized.
#[test]
fn managed_deprojection_authorizes_only_recorded_paths() {
    let tmp = TempDir::new().unwrap();
    let lock = tmp.path().join("build").join("lock.json");
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    let managed = tmp.path().join("skills").join("ccync");
    let sibling = tmp.path().join("skills").join("user-tool");

    // Record `managed` via the registry's own writer so the stored key uses the
    // exact same normalization the authority checks against.
    let mut report = ProjectionReport::default();
    let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
    registry.mark(MANAGED_SKILL_PATHS, &managed);
    registry.persist(&[], false, &mut report).unwrap();

    let authority = ManagedDeprojection::from_lockfile(&lock);
    assert!(!authority.is_empty());
    assert!(
        authority.is_authorized_for_removal(&managed),
        "registry-owned path must be authorized for removal"
    );
    assert!(
        !authority.is_authorized_for_removal(&sibling),
        "a non-managed same-named sibling must NOT be authorized"
    );
}

// Empty/missing prior authorizes nothing — proves is_managed semantics, NOT
// can_mutate's allow-all-on-empty-prior (which would delete user files during a
// common partial-state uninstall).
#[test]
fn managed_deprojection_empty_or_missing_prior_authorizes_nothing() {
    let tmp = TempDir::new().unwrap();
    let any = tmp.path().join("skills").join("ccync");

    let authority = ManagedDeprojection::from_lockfile(&tmp.path().join("nope.json"));
    assert!(authority.is_empty(), "missing lockfile → empty authority");
    assert!(
        !authority.is_authorized_for_removal(&any),
        "empty/missing prior must authorize nothing (no can_mutate allow-all)"
    );
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
        selected_projection_targets: vec![
            "copilot".into(),
            "gemini-cli".into(),
            "agy-cli".into(),
            "opencode".into(),
        ],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(evidence_has_change(&report, ProjectionPathChange::Linked));
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
    let raw = "---\nname: my-agent\ndescription: d\nmodel: gpt-5.4\ntools: [read, edit]\n---\nCharter body\n";
    fs::write(source_root.join("agents").join("my-agent.agent.md"), raw).unwrap();

    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec!["copilot".into(), "codex".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    // (1) Claude/copilot agent projected VERBATIM — abstract tools unchanged.
    let claude_agent = home
        .join(".copilot")
        .join("agents")
        .join("my-agent.agent.md");
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
    let codex_toml = home.join(".codex").join("agents").join("my-agent.toml");
    let toml = fs::read_to_string(&codex_toml).unwrap();
    assert!(toml.contains("[agent]"));
    assert!(toml.contains("name = \"my-agent\""));
    assert!(
        toml.contains("model = \"gpt-5.4\""),
        "Codex model must come from frontmatter"
    );

    // (3) No discuss-skill residue.
    assert!(
        !home
            .join(".agents")
            .join("skills")
            .join("discuss-my-agent")
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
        selected_projection_targets: vec!["copilot".into(), "gemini-cli".into()],
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
fn update_skills_gemini_cli_only_creates_no_gemini_ccync_source_link() {
    // gemini-cli is retired: the `~/.gemini/ccync` source link is re-gated on
    // agy-* selection alone. A gemini-cli-only selection must create neither
    // the link nor its parent `~/.gemini/` directory.
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
        selected_projection_targets: vec!["gemini-cli".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(
        !home.join(".gemini").exists(),
        "gemini-cli-only selection must not create ~/.gemini or the ccync source link"
    );
}

// ── OpenCode agent write-side ownership regressions ─────────────────────────
//
// The OpenCode agents directory used to be published via a whole-directory
// staged swap: anything the user had placed in `~/.config/opencode/agents/`
// was silently deleted or overwritten. These regressions lock the guarded
// per-file replacement in place — absent-root non-creation, foreign
// survival, ownership-guarded collision handling, and atomic-failure safety.

#[test]
fn opencode_agent_publication_absent_root_does_not_create_agents_dir() {
    // Zero-agent fixture: the source's `agents/` dir is empty, so the desired
    // set is empty and the per-file write loop never runs. Unlike the prior
    // whole-directory swap (which staged/created the live directory
    // unconditionally), the guarded per-file loop must not create
    // `~/.config/opencode/agents` when nothing is ever written into it.
    let (_temp, repo_root, source_root, home) = fixture_roots();

    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(
        !home
            .join(".config")
            .join("opencode")
            .join("agents")
            .exists(),
        "zero desired agents must not create the OpenCode agents root"
    );
}

#[test]
fn opencode_agent_publication_zero_agents_preserves_foreign_sibling() {
    // Pre-existing foreign OpenCode agent, zero ccync agents desired: the
    // guarded per-file loop has nothing to write, so a pre-existing
    // user-owned sibling must survive byte-for-byte.
    let (_temp, repo_root, source_root, home) = fixture_roots();
    let opencode_agents = home.join(".config").join("opencode").join("agents");
    fs::create_dir_all(&opencode_agents).unwrap();
    let foreign = opencode_agents.join("foreign.md");
    fs::write(&foreign, "---\nname: foreign\n---\nnot ccync\n").unwrap();
    let before = fs::read(&foreign).unwrap();

    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert_eq!(
        fs::read(&foreign).unwrap(),
        before,
        "foreign OpenCode agent must survive byte-for-byte with zero ccync agents desired"
    );
}

#[test]
fn opencode_agent_publication_writes_authorized_agent_beside_foreign_sibling() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    fs::write(
        source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\ncolor: green\n---\nBody\n",
    )
    .unwrap();

    let opencode_agents = home.join(".config").join("opencode").join("agents");
    fs::create_dir_all(&opencode_agents).unwrap();
    let foreign = opencode_agents.join("foreign.md");
    fs::write(&foreign, "---\nname: foreign\n---\nnot ccync\n").unwrap();
    let before = fs::read(&foreign).unwrap();

    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    let helper = fs::read_to_string(opencode_agents.join("helper.md")).unwrap();
    assert!(helper.starts_with(CCYNC_MANAGED_FILE_HEADER));
    assert!(helper.contains("color: green"));
    assert_eq!(
        fs::read(&foreign).unwrap(),
        before,
        "a foreign sibling must remain untouched by an authorized write elsewhere in the same directory"
    );
}

#[test]
fn opencode_agent_publication_preserves_unproven_same_name_collision() {
    use crate::support::ManagedArtifactRegistry;

    let (_temp, repo_root, source_root, home) = fixture_roots();
    fs::write(
        source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\n---\nBody\n",
    )
    .unwrap();

    let opencode_agents = home.join(".config").join("opencode").join("agents");
    fs::create_dir_all(&opencode_agents).unwrap();
    let target = opencode_agents.join("helper.md");
    fs::write(&target, "user's own helper, not ccync-managed\n").unwrap();
    let before = fs::read(&target).unwrap();

    let evidence = run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert_eq!(
        fs::read(&target).unwrap(),
        before,
        "an unproven same-name target must be preserved, not overwritten"
    );
    assert!(
        evidence_all_warnings(&evidence)
            .iter()
            .any(|w| w.contains("preserved user-owned path")),
        "the collision must be surfaced as a warning"
    );

    let lockfile = home.join(".ccync").join("build").join("lock.json");
    let mut report = ProjectionReport::default();
    let registry = ManagedArtifactRegistry::load(&lockfile, &mut report);
    assert!(
        !registry.is_managed(&target),
        "an unproven collision must never be claimed as ccync-owned"
    );
}

#[test]
fn opencode_agent_publication_absent_source_collision_then_present_owner_writes() {
    // Round 1: source "owner-src" installs helper.agent.md; ownership recorded.
    let (_temp, repo_root, source_root, home) = fixture_roots();
    fs::write(
        source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\n---\nBody v1\n",
    )
    .unwrap();

    let owner_opts = SkillUpdateOptions {
        repo_root: repo_root.clone(),
        sources: vec![ProjectionSource {
            root: source_root.clone(),
            id: "owner-src".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    };
    run_update_skills(&owner_opts).unwrap();

    let target = home
        .join(".config")
        .join("opencode")
        .join("agents")
        .join("helper.md");
    assert!(fs::read_to_string(&target).unwrap().contains("Body v1"));

    // Round 2: a run where "owner-src" is no longer among installed sources —
    // a different source ("other-src") happens to publish an agent file with
    // the same file name and different content. Because the registry still
    // attributes `helper.md` to "owner-src", which is absent from this run's
    // installed set, the target is foreign to this invocation and must be
    // preserved untouched, regardless of the new content.
    let other_source_root = repo_root.join("plugins").join("other-src");
    fs::create_dir_all(other_source_root.join("agents")).unwrap();
    fs::write(
        other_source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\n---\nBody v2\n",
    )
    .unwrap();

    let foreign_run_opts = SkillUpdateOptions {
        repo_root: repo_root.clone(),
        sources: vec![ProjectionSource {
            root: other_source_root,
            id: "other-src".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    };
    let evidence = run_update_skills(&foreign_run_opts).unwrap();

    assert!(
        fs::read_to_string(&target).unwrap().contains("Body v1"),
        "a target owned by a source absent from this run's installed set must be preserved"
    );
    assert!(
        evidence_all_warnings(&evidence)
            .iter()
            .any(|w| w.contains("preserved foreign-owned OpenCode agent")),
        "the absent-source collision must be surfaced as a warning"
    );

    // Round 3 (present-owner control): re-run with "owner-src" installed again
    // and new content — this time the write must succeed, proving round 2's
    // block was specifically about the absent-source guard, not a permanent
    // lock on the target.
    fs::write(
        source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\n---\nBody v3\n",
    )
    .unwrap();
    run_update_skills(&owner_opts).unwrap();

    assert!(
        fs::read_to_string(&target).unwrap().contains("Body v3"),
        "a present-owner rerun must be able to update its own previously written target"
    );
}

#[test]
fn opencode_agent_publication_atomic_failure_leaves_no_registry_claim() {
    use crate::support::ManagedArtifactRegistry;

    let (_temp, repo_root, source_root, home) = fixture_roots();
    fs::write(
        source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\n---\nBody\n",
    )
    .unwrap();

    let opencode_agents = home.join(".config").join("opencode").join("agents");
    fs::create_dir_all(&opencode_agents).unwrap();
    // Deterministically block the atomic writer's own `<target>.tmp`
    // sibling by pre-occupying that exact path with a directory.
    fs::create_dir_all(opencode_agents.join("helper.md.tmp")).unwrap();

    let failure = run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap_err();

    assert!(matches!(failure.error, ProjectionError::Io(_)));

    let target = opencode_agents.join("helper.md");
    assert!(
        !target.exists(),
        "a failed atomic write must not leave a partially written target"
    );

    let lockfile = home.join(".ccync").join("build").join("lock.json");
    let mut report = ProjectionReport::default();
    let registry = ManagedArtifactRegistry::load(&lockfile, &mut report);
    assert!(
        !registry.is_managed(&target),
        "a target must never be claimed ccync-owned unless its write actually succeeded"
    );
}

// ── OpenCode agent prune/dry-run regressions ─────────────────────────────────
//
// The write-side guard (above) proves a single run never clobbers foreign
// content. These regressions prove the follow-on `prune_stale_links` call
// removes only what this run actually owns, leaves an absent-source stale
// entry alone, never runs at all when OpenCode is deselected, and reports
// accurate `Written`/`Removed`/warning evidence under `--dry-run` without
// touching disk.

#[test]
fn opencode_agent_publication_prunes_stale_ccync_agent_beside_foreign_content() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    fs::write(
        source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\n---\nBody\n",
    )
    .unwrap();

    let opencode_agents = home.join(".config").join("opencode").join("agents");
    fs::create_dir_all(&opencode_agents).unwrap();
    let foreign = opencode_agents.join("foreign.md");
    fs::write(&foreign, "---\nname: foreign\n---\nnot ccync\n").unwrap();
    let foreign_before = fs::read(&foreign).unwrap();

    let mut opts = SkillUpdateOptions {
        repo_root: repo_root.clone(),
        sources: vec![ProjectionSource {
            root: source_root.clone(),
            id: "ccync".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    };
    run_update_skills(&opts).unwrap();

    let helper = opencode_agents.join("helper.md");
    assert!(helper.exists(), "round 1 must publish the ccync agent");

    // Round 2: helper.agent.md is no longer part of the desired set — it must
    // be pruned as stale, while the foreign sibling survives untouched.
    fs::remove_file(source_root.join("agents").join("helper.agent.md")).unwrap();
    opts.sources = vec![ProjectionSource {
        root: source_root,
        id: "ccync".into(),
        persistent: true,
    }];
    run_update_skills(&opts).unwrap();

    assert!(
        !helper.exists(),
        "a stale ccync-owned OpenCode agent must be pruned once absent from the desired set"
    );
    assert_eq!(
        fs::read(&foreign).unwrap(),
        foreign_before,
        "a foreign sibling must survive stale-ccync pruning byte-for-byte"
    );
}

#[test]
fn opencode_agent_publication_preserves_absent_source_stale_entry() {
    // Round 1: "owner-src" installs helper.agent.md; ownership recorded.
    let (_temp, repo_root, source_root, home) = fixture_roots();
    fs::write(
        source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\n---\nBody\n",
    )
    .unwrap();

    run_update_skills(&SkillUpdateOptions {
        repo_root: repo_root.clone(),
        sources: vec![ProjectionSource {
            root: source_root,
            id: "owner-src".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    let target = home
        .join(".config")
        .join("opencode")
        .join("agents")
        .join("helper.md");
    let before = fs::read(&target).unwrap();

    // Round 2: "owner-src" is entirely absent from this run's installed
    // sources — a different source with zero agents of its own runs instead.
    // `helper.md` is not in the current desired set (stale), but it is
    // attributed to a source foreign to this invocation and must be
    // preserved rather than pruned.
    let other_source_root = repo_root.join("plugins").join("other-src");
    fs::create_dir_all(other_source_root.join("agents")).unwrap();

    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: other_source_root,
            id: "other-src".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert_eq!(
        fs::read(&target).unwrap(),
        before,
        "a stale entry owned by a source absent from this run's installed set must be preserved, not pruned"
    );
}

#[test]
fn opencode_agent_publication_deselection_preserves_byte_identity() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    fs::write(
        source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\n---\nBody\n",
    )
    .unwrap();

    run_update_skills(&SkillUpdateOptions {
        repo_root: repo_root.clone(),
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    let target = home
        .join(".config")
        .join("opencode")
        .join("agents")
        .join("helper.md");
    let before = fs::read(&target).unwrap();

    // Re-run with "opencode" absent from the selected targets: this hits the
    // early-return deselection branch before either the publish loop or the
    // prune call ever executes, so the previously published agent must
    // survive byte-for-byte.
    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![],
        user_home: home.clone(),
        selected_projection_targets: vec!["copilot".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert_eq!(
        fs::read(&target).unwrap(),
        before,
        "deselecting OpenCode must leave a previously published agent byte-identical"
    );
}

#[test]
fn opencode_agent_publication_dry_run_reports_written_removed_and_warning_without_mutation() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    fs::write(
        source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\n---\nBody\n",
    )
    .unwrap();

    let mut opts = SkillUpdateOptions {
        repo_root: repo_root.clone(),
        sources: vec![ProjectionSource {
            root: source_root.clone(),
            id: "ccync".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    };
    run_update_skills(&opts).unwrap();

    let opencode_agents = home.join(".config").join("opencode").join("agents");
    let helper = opencode_agents.join("helper.md");
    let helper_before = fs::read(&helper).unwrap();

    // Set up round 2's fixture: `helper.agent.md` goes stale (removed from
    // source), `another.agent.md` is a fresh desired agent with no
    // collision, and `newagent.agent.md` collides with a pre-existing
    // unproven user file at its target name.
    fs::remove_file(source_root.join("agents").join("helper.agent.md")).unwrap();
    fs::write(
        source_root.join("agents").join("another.agent.md"),
        "---\ndescription: another\n---\nBody\n",
    )
    .unwrap();
    fs::write(
        source_root.join("agents").join("newagent.agent.md"),
        "---\ndescription: newagent\n---\nBody\n",
    )
    .unwrap();
    let collision = opencode_agents.join("newagent.md");
    fs::write(&collision, "user's own newagent, not ccync-managed\n").unwrap();
    let collision_before = fs::read(&collision).unwrap();

    let another = opencode_agents.join("another.md");
    opts.dry_run = true;
    let evidence = run_update_skills(&opts).unwrap();

    let opencode_evidence = evidence
        .by_target
        .get(&ProjectionTargetId::Opencode)
        .expect("dry-run must still record OpenCode-scoped evidence");
    assert!(
        opencode_evidence
            .paths
            .iter()
            .any(|p| p.path == another && p.change == ProjectionPathChange::Written),
        "dry-run must preview writing the fresh, non-colliding agent"
    );
    assert!(
        opencode_evidence
            .paths
            .iter()
            .any(|p| p.path == helper && p.change == ProjectionPathChange::Removed),
        "dry-run must preview pruning the now-stale ccync agent"
    );
    assert!(
        opencode_evidence
            .warnings
            .iter()
            .any(|w| w.contains("preserved user-owned path")),
        "dry-run must still surface the unproven collision warning"
    );

    assert!(
        helper.exists(),
        "dry-run must not actually prune the stale agent from disk"
    );
    assert_eq!(
        fs::read(&helper).unwrap(),
        helper_before,
        "dry-run must not mutate the stale agent's bytes"
    );
    assert!(
        !another.exists(),
        "dry-run must not actually write the previewed fresh agent"
    );
    assert_eq!(
        fs::read(&collision).unwrap(),
        collision_before,
        "dry-run must not mutate the collision target's bytes"
    );
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
        selected_projection_targets: vec!["gemini-cli".into(), "codex".into(), "opencode".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    let baked_path = home
        .join(".ccync")
        .join("build")
        .join("render")
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

    assert!(
        !home
            .join(".gemini")
            .join("commands")
            .join("sample-command.toml")
            .exists(),
        "gemini-cli command projection is retired; no .toml file is ever written, even when selected"
    );

    let opencode = fs::read_to_string(
        home.join(".config")
            .join("opencode")
            .join("commands")
            .join("sample-command.md"),
    )
    .unwrap();
    assert!(opencode.starts_with(CCYNC_MANAGED_FILE_HEADER));
    assert!(opencode.contains("User command arguments, if any: $ARGUMENTS"));
    assert!(evidence_has_change(&report, ProjectionPathChange::Written));
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
        selected_projection_targets: vec!["copilot".into()],
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
        selected_projection_targets: vec!["codex".into()],
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
        selected_projection_targets: vec!["gemini-cli".into()],
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
        selected_projection_targets: vec!["opencode".into()],
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

    let opencode_target = home
        .join(".config")
        .join("opencode")
        .join("commands")
        .join("sample-command.md");
    let claude_target = home
        .join(".claude")
        .join("commands")
        .join("sample-command.md");
    fs::create_dir_all(opencode_target.parent().unwrap()).unwrap();
    fs::create_dir_all(claude_target.parent().unwrap()).unwrap();
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
        selected_projection_targets: vec!["copilot".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(!opencode_target.exists());
    assert!(!claude_target.exists());
}

#[test]
fn update_commands_prunes_obsolete_managed_files() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "sample-command");

    // gemini-cli command projection is retired: a pre-existing `.toml` under
    // `~/.gemini/commands/` — however it got there — is a surface ccync no
    // longer manages at all, so it must be left byte-identical even with
    // gemini-cli selected. Only opencode's own stale-file pruning applies.
    let gemini_commands = home.join(".gemini").join("commands");
    let opencode_commands = home.join(".config").join("opencode").join("commands");
    fs::create_dir_all(&gemini_commands).unwrap();
    fs::create_dir_all(&opencode_commands).unwrap();
    let stale_gemini_content = format!("{CCYNC_MANAGED_FILE_HEADER}\nold\n");
    fs::write(
        gemini_commands.join("old-command.toml"),
        &stale_gemini_content,
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
        selected_projection_targets: vec!["gemini-cli".into(), "opencode".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert_eq!(
        fs::read_to_string(gemini_commands.join("old-command.toml")).unwrap(),
        stale_gemini_content,
        "gemini-cli command projection is retired; a pre-existing .toml is never pruned"
    );
    assert!(!gemini_commands.join("sample-command.toml").exists());
    assert!(!opencode_commands.join("old-command.md").exists());
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
        selected_projection_targets: vec!["opencode".into()],
        dry_run: false,
        replace: true,
    };

    let first_report = run_update_commands(&options).unwrap();
    assert!(
        evidence_has_change(&first_report, ProjectionPathChange::Written),
        "first sync should materialize managed artifacts"
    );

    let lockfile = home.join(".ccync").join("build").join("lock.json");
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
        .join(".config")
        .join("opencode")
        .join("commands")
        .join("old-command.md");
    fs::write(
        &stale_managed_looking,
        format!("{CCYNC_MANAGED_FILE_HEADER}\nuser-owned\n"),
    )
    .unwrap();
    let unrelated_claude_tool = home.join(".claude").join("plugins").join("my-tool");
    fs::create_dir_all(&unrelated_claude_tool).unwrap();

    let second_report = run_update_commands(&options).unwrap();

    assert!(
        !evidence_has_change(&second_report, ProjectionPathChange::Written),
        "second sync should be idempotent when inputs do not change"
    );
    assert!(
        !evidence_has_change(&second_report, ProjectionPathChange::Removed),
        "second sync must not delete untracked surfaces"
    );
    assert!(
        evidence_all_warnings(&second_report).is_empty(),
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
        selected_projection_targets: vec!["gemini-cli".into()],
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
        selected_projection_targets: vec!["codex".into()],
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
    assert!(evidence_all_warnings(&report)
        .iter()
        .any(|warning| warning.contains("preserved user-owned path")));
}

#[test]
fn update_personalization_selecting_gemini_cli_writes_no_context_bridge_or_settings_merge() {
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
    let settings_before = "{\n  \"context\": {\"fileName\": [\"README.md\"]}\n}\n";
    fs::write(home.join(".gemini").join("settings.json"), settings_before).unwrap();
    fs::create_dir_all(appdata.join("Code").join("User")).unwrap();

    run_update_personalization(&PersonalizationUpdateOptions {
        repo_root: repo_root.clone(),
        source_root: source_root.clone(),
        user_home: home.clone(),
        appdata_root: appdata.clone(),
        selected_projection_targets: vec!["copilot".into(), "gemini-cli".into()],
        dry_run: false,
        replace: true,
        uninstall: false,
    })
    .unwrap();

    assert!(
        !home.join(".gemini").join("ccync-context.md").exists(),
        "the Gemini context bridge must never be written, even with gemini-cli selected"
    );
    assert_eq!(
        fs::read_to_string(home.join(".gemini").join("settings.json")).unwrap(),
        settings_before,
        "~/.gemini/settings.json must be byte-identical — no merge is performed"
    );

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
fn update_personalization_uninstall_does_not_touch_gemini_settings_or_context() {
    let (_temp, repo_root, source_root, home, appdata) = fixture_roots_with_appdata();
    fs::create_dir_all(home.join(".gemini").join("antigravity-cli").join("plugins")).unwrap();
    fs::create_dir_all(home.join(".ccync").join("config")).unwrap();
    fs::write(
        home.join(".ccync").join("config").join("config.json"),
        "{\n  \"devMode\": true\n}\n",
    )
    .unwrap();
    fs::create_dir_all(home.join(".gemini")).unwrap();
    let settings_before = "{\n  \"context\": {\"fileName\": [\"README.md\"]}\n}\n";
    fs::write(home.join(".gemini").join("settings.json"), settings_before).unwrap();
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
        selected_projection_targets: vec!["gemini-cli".into(), "agy-cli".into()],
        dry_run: false,
        replace: true,
        uninstall: true,
    })
    .unwrap();

    assert!(!home.join(".gemini").join("ccync-context.md").exists());
    assert_eq!(
        fs::read_to_string(home.join(".gemini").join("settings.json")).unwrap(),
        settings_before,
        "uninstall must not touch ~/.gemini/settings.json"
    );
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
        selected_projection_targets: vec!["copilot".into()],
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
        selected_projection_targets: vec!["agy-cli".into()],
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
fn personalization_runtime_dirs_gated_by_selection() {
    // Claude-only fixture: neither the Gemini nor the Antigravity agent-home
    // directory should be created; `~/.ccync` is unconditional.
    let (_temp, repo_root, source_root, home, appdata) = fixture_roots_with_appdata();
    write_skill_fixture(&source_root, "sample-skill");
    let gemini_root = home.join(".gemini");
    let antigravity_root = gemini_root.join("antigravity-cli");
    let ccync_home = home.join(".ccync");

    run_update_personalization(&PersonalizationUpdateOptions {
        repo_root,
        source_root,
        user_home: home,
        appdata_root: appdata,
        selected_projection_targets: vec!["claude".into()],
        dry_run: false,
        replace: true,
        uninstall: false,
    })
    .unwrap();

    assert!(
        !gemini_root.exists(),
        "Claude-only selection must not create the Gemini agent-home directory"
    );
    assert!(
        !antigravity_root.exists(),
        "Claude-only selection must not create the Antigravity agent-home directory"
    );
    assert!(
        ccync_home.is_dir(),
        "ccync_home must always be created regardless of runtime selection"
    );
}

#[test]
fn personalization_runtime_dirs_created_for_selected_runtime() {
    // agy-cli selected: both the Gemini agent-home directory and the
    // Antigravity agent-home directory are created — re-gated on agy-*
    // selection alone now that gemini-cli is retired.
    let (_temp, repo_root, source_root, home, appdata) = fixture_roots_with_appdata();
    write_skill_fixture(&source_root, "sample-skill");
    let gemini_root = home.join(".gemini");
    let antigravity_root = gemini_root.join("antigravity-cli");

    run_update_personalization(&PersonalizationUpdateOptions {
        repo_root,
        source_root,
        user_home: home,
        appdata_root: appdata,
        selected_projection_targets: vec!["agy-cli".into()],
        dry_run: false,
        replace: true,
        uninstall: false,
    })
    .unwrap();

    assert!(
        gemini_root.is_dir(),
        "agy-cli selection must create the Gemini agent-home directory"
    );
    assert!(
        antigravity_root.is_dir(),
        "agy-cli selection must create the Antigravity agent-home directory"
    );
}

#[test]
fn personalization_agent_home_dirs_not_created_for_gemini_cli_only_selection() {
    // gemini-cli is retired: even while the target still parses, selecting it
    // alone must not create the Gemini agent-home directory. Agent-home
    // creation is re-gated on agy-* selection alone.
    let (_temp, repo_root, source_root, home, appdata) = fixture_roots_with_appdata();
    write_skill_fixture(&source_root, "sample-skill");
    let gemini_root = home.join(".gemini");
    let antigravity_root = gemini_root.join("antigravity-cli");

    run_update_personalization(&PersonalizationUpdateOptions {
        repo_root,
        source_root,
        user_home: home,
        appdata_root: appdata,
        selected_projection_targets: vec!["gemini-cli".into()],
        dry_run: false,
        replace: true,
        uninstall: false,
    })
    .unwrap();

    assert!(
        !gemini_root.exists(),
        "gemini-cli-only selection must not create the Gemini agent-home directory"
    );
    assert!(
        !antigravity_root.exists(),
        "gemini-cli-only selection must not create the Antigravity agent-home directory"
    );
}

// ── MachineUpdateOptions::new / selected_projection_targets accessors ──────

#[test]
fn machine_update_options_new_getter_setter_round_trip_every_value() {
    let repo_root = PathBuf::from("/repo");
    let source_root = PathBuf::from("/repo/plugins/ccync-core");
    let user_home = PathBuf::from("/home/user");
    let appdata_root = PathBuf::from("/home/user/appdata");
    let initial_targets = vec!["claude".to_string(), "codex".to_string()];

    let mut opts = MachineUpdateOptions::new(
        repo_root.clone(),
        source_root.clone(),
        user_home.clone(),
        appdata_root.clone(),
        initial_targets.clone(),
        true,
        true,
        false,
        true,
    );

    // Every constructor value round-trips onto the struct exactly.
    assert_eq!(opts.repo_root, repo_root);
    assert_eq!(opts.source_root, source_root);
    assert_eq!(opts.user_home, user_home);
    assert_eq!(opts.appdata_root, appdata_root);
    assert_eq!(
        opts.selected_projection_targets(),
        initial_targets.as_slice()
    );
    assert_eq!(opts.selected_projection_targets, initial_targets);
    assert!(opts.dry_run);
    assert!(opts.replace);
    assert!(!opts.uninstall);
    assert!(opts.include_source_projections);

    let updated_targets = vec!["copilot".to_string(), "opencode".to_string()];
    opts.set_selected_projection_targets(updated_targets.clone());
    assert_eq!(
        opts.selected_projection_targets(),
        updated_targets.as_slice()
    );
    assert_eq!(opts.selected_projection_targets, updated_targets);

    // Every other option value is untouched by the setter.
    assert_eq!(opts.repo_root, repo_root);
    assert_eq!(opts.source_root, source_root);
    assert_eq!(opts.user_home, user_home);
    assert_eq!(opts.appdata_root, appdata_root);
    assert!(opts.dry_run);
    assert!(opts.replace);
    assert!(!opts.uninstall);
    assert!(opts.include_source_projections);
}

// The round-trip test above uses a single bool pattern (true, true, false,
// true) for (dry_run, replace, uninstall, include_source_projections), which
// leaves `dry_run`/`replace`/`include_source_projections` pairwise
// indistinguishable from each other (all three share the value `true`) — a
// positional mix-up between any two of them would go undetected. This case
// uses a complementary pattern so the two runs together resolve most of
// those pairs, and also exercises the empty-selection edge case: an empty
// `selected_projection_targets` must round-trip to an empty slice rather
// than silently retaining a stale or panicking value.
#[test]
fn machine_update_options_new_empty_selection_and_alternate_bool_pattern() {
    let repo_root = PathBuf::from("/repo2");
    let source_root = PathBuf::from("/repo2/plugins/ccync-core");
    let user_home = PathBuf::from("/home/user2");
    let appdata_root = PathBuf::from("/home/user2/appdata");

    let opts = MachineUpdateOptions::new(
        repo_root.clone(),
        source_root.clone(),
        user_home.clone(),
        appdata_root.clone(),
        Vec::new(),
        false,
        false,
        true,
        false,
    );

    assert_eq!(opts.repo_root, repo_root);
    assert_eq!(opts.source_root, source_root);
    assert_eq!(opts.user_home, user_home);
    assert_eq!(opts.appdata_root, appdata_root);
    assert!(opts.selected_projection_targets().is_empty());
    assert!(opts.selected_projection_targets.is_empty());
    assert!(!opts.dry_run);
    assert!(!opts.replace);
    assert!(opts.uninstall);
    assert!(!opts.include_source_projections);
}

// ── planned_machine_touches (pure, read-only projection touch planner) ─────

// Builds a completely empty source tree (no skill/agent/command subdirectories)
// so `run_machine_update`'s per-item projection loops contribute nothing
// to the dry-run report — isolating exactly the content-independent live paths
// that `planned_machine_touches` must also enumerate purely from `opts`.
fn empty_machine_options(
    selected_projection_targets: Vec<String>,
) -> (TempDir, MachineUpdateOptions) {
    let temp = TempDir::new().unwrap();
    let repo_root = temp.path().join("repo");
    let source_root = repo_root.join("plugins").join("ccync-core");
    let home = temp.path().join("home");
    let appdata = temp.path().join("appdata");
    fs::create_dir_all(source_root.join("skills")).unwrap();
    fs::create_dir_all(source_root.join("agents")).unwrap();
    fs::create_dir_all(source_root.join("commands")).unwrap();

    let opts = MachineUpdateOptions {
        repo_root,
        source_root,
        user_home: home,
        appdata_root: appdata,
        selected_projection_targets,
        dry_run: true,
        replace: true,
        uninstall: false,
        include_source_projections: true,
    };
    (temp, opts)
}

fn observed_dry_run_touch_paths(opts: &MachineUpdateOptions) -> Vec<PathBuf> {
    let evidence = run_machine_update(opts).unwrap();
    let mut observed: Vec<PathBuf> = evidence_paths(&evidence)
        .filter(|p| {
            matches!(
                p.change,
                ProjectionPathChange::Written | ProjectionPathChange::Linked
            )
        })
        .map(|p| p.path.clone())
        .collect();
    observed.sort();
    observed.dedup();
    observed
}

#[test]
fn planned_machine_touches_matches_real_run_for_full_selection() {
    // Full canonical-target selection (all seven projection targets), run for
    // real (dry_run: false) rather than dry-run: `planned_machine_touches` is
    // dry-run-agnostic by contract, and its Antigravity GUI touch can only be
    // proven against a real run — the GUI junction target is unconditional on
    // selection, no content-gating involved. This fixture has zero OpenCode
    // agents, so OpenCode's per-file guarded write loop has nothing to publish
    // and touches no path at all — content-dependent per-agent writes are
    // deliberately out of scope for this content-independent planner (see its
    // own doc comment), so neither `planned` nor `observed` should carry an
    // OpenCode entry here.
    let (_temp, mut opts) = empty_machine_options(
        [
            "agy-cli", "agy-ide", "agy-gui", "claude", "codex", "copilot", "opencode",
        ]
        .into_iter()
        .map(String::from)
        .collect(),
    );
    assert_eq!(
        opts.selected_projection_targets.len(),
        ccync_foundation::target::CANONICAL_PROJECTION_TARGETS.len(),
        "fixture selection must cover the true canonical projection-target set, not a stale subset"
    );

    // Canonical root must exist for the whole-plugin junction targets (agy-*)
    // to resolve to a real directory.
    let canonical = opts.user_home.join(".ccync").join("build").join("render");
    fs::create_dir_all(&canonical).unwrap();
    opts.dry_run = false;

    let observed = observed_dry_run_touch_paths(&opts);

    let planned = planned_machine_touches(&opts);
    let mut planned_paths: Vec<PathBuf> = planned.iter().map(|(_, path)| path.clone()).collect();
    planned_paths.sort();
    planned_paths.dedup();

    assert_eq!(
        planned_paths, observed,
        "pure planner must exactly match the real run's touched path set"
    );
    assert!(
        !planned_paths.is_empty(),
        "sanity: the full-selection fixture must actually touch something"
    );

    for (runtime, _path) in &planned {
        assert!(
            runtime == "common"
                || runtime == "agy"
                || opts
                    .selected_projection_targets
                    .iter()
                    .any(|rt| rt == runtime),
            "planned touch labeled '{runtime}' must correspond to a selected runtime"
        );
    }

    let opencode_path = opts
        .user_home
        .join(".config")
        .join("opencode")
        .join("agents");
    assert!(
        !planned.iter().any(|(_, path)| path == &opencode_path),
        "the content-independent planner must not claim an OpenCode agents-directory touch \
         it cannot prove without per-agent content"
    );
    assert!(
        !observed.contains(&opencode_path),
        "a real run with opencode selected but zero agents must not touch the agents directory at all"
    );

    let agy_gui_junction = opts
        .user_home
        .join(".gemini")
        .join("config")
        .join("plugins")
        .join("ccync");
    assert!(
        planned
            .iter()
            .any(|(rt, path)| rt == "agy-gui" && path == &agy_gui_junction),
        "planned touches must report the Antigravity GUI whole-plugin junction"
    );
    assert!(
        observed.contains(&agy_gui_junction),
        "a real run with agy-gui selected must create the GUI junction"
    );
}

#[test]
fn planned_machine_touches_excludes_unselected_runtime_paths() {
    // Claude-only selection: none of the gemini/copilot/agy-gated touches apply —
    // only the selection-independent lockfile marker remains, matching the
    // "selected runtimes only" gating already used for the same class of live surface.
    let (_temp, opts) = empty_machine_options(vec!["claude".to_string()]);

    let observed = observed_dry_run_touch_paths(&opts);
    let planned = planned_machine_touches(&opts);
    let mut planned_paths: Vec<PathBuf> = planned.iter().map(|(_, path)| path.clone()).collect();
    planned_paths.sort();

    assert_eq!(
        planned_paths, observed,
        "pure planner must exactly match the real dry-run report for a narrow selection"
    );
    assert_eq!(
        planned.len(),
        1,
        "claude-only selection must yield only the selection-independent lockfile touch"
    );
    assert_eq!(planned[0].0, "common");
    assert!(
        planned[0].1.ends_with(Path::new("build").join("lock.json")),
        "the sole claude-only touch must be the projection lockfile marker"
    );
}

#[test]
fn machine_update_claude_only_creates_no_unselected_agent_homes() {
    // FINDING-11-1/11-2 regression: a Claude-only `run_machine_update`
    // with a command AND a skill present used to `ensure_dir` `~/.gemini/commands`,
    // `~/.copilot/agents`, and `~/.config/opencode/commands` unconditionally —
    // creating agent homes for runtimes the user never selected and that the
    // first-projection consent gate never named. Pre-fix this test fails with
    // `~/.gemini exists = true`.
    let (_temp, mut opts) = empty_machine_options(vec!["claude".to_string()]);
    write_skill_fixture(&opts.source_root, "sample-skill");
    write_command_fixture(&opts.source_root, "sample-command");
    opts.dry_run = false;

    run_machine_update(&opts).unwrap();

    let home = &opts.user_home;
    assert!(
        !home.join(".gemini").exists(),
        "Claude-only run must not create the Gemini agent home"
    );
    assert!(
        !home.join(".copilot").exists(),
        "Claude-only run must not create the Copilot agent home"
    );
    assert!(
        !home.join(".config").join("opencode").exists(),
        "Claude-only run must not create the OpenCode agent home"
    );
}

#[test]
fn machine_update_gemini_cli_target_never_gets_command_projection() {
    // gemini-cli command projection is retired: even with gemini-cli selected
    // and a command present, `~/.gemini/commands` must never be created — no
    // evidence entry is attributed to a command file under `~/.gemini/`.
    let (_temp, mut opts) = empty_machine_options(vec!["gemini-cli".to_string()]);
    write_command_fixture(&opts.source_root, "sample-command");
    opts.dry_run = false;

    run_machine_update(&opts).unwrap();

    assert!(
        !opts.user_home.join(".gemini").join("commands").exists(),
        "gemini-cli command projection is retired; ~/.gemini/commands is never created"
    );
}

// ── run_machine_update — mid-run fault attribution ──

/// Fixture for `run_machine_update_mid_fault_preserves_exact_partial_attribution`:
/// a copilot-only source tree, plus a plain file sitting exactly where
/// `update_skills`'s copilot-agent block needs to create a directory
/// (`ensure_dir(&agents_target, ...)` inside the `has_target("copilot")`
/// block), so `fs::create_dir_all` faults mid-run with a real io error — an
/// unexpected fault, not an owned-path failure any branch already handles as
/// a value.
fn fault_mid_machine_options() -> (TempDir, MachineUpdateOptions) {
    let temp = TempDir::new().unwrap();
    let repo_root = temp.path().join("repo");
    let source_root = repo_root.join("plugins").join("ccync-core");
    let home = temp.path().join("home");
    let appdata = temp.path().join("appdata");
    fs::create_dir_all(source_root.join("skills")).unwrap();
    fs::create_dir_all(source_root.join("agents")).unwrap();
    fs::create_dir_all(source_root.join("commands")).unwrap();

    fs::create_dir_all(home.join(".copilot")).unwrap();
    fs::write(home.join(".copilot").join("agents"), "blocking file").unwrap();

    let opts = MachineUpdateOptions {
        repo_root,
        source_root,
        user_home: home,
        appdata_root: appdata,
        selected_projection_targets: vec!["copilot".to_string()],
        dry_run: false,
        replace: true,
        uninstall: false,
        include_source_projections: true,
    };
    (temp, opts)
}

// A mid-machine fault returns byte-equal partial evidence with exact
// common/target/warning attribution — proven by running the identical
// fixture twice, independently (separate temp roots so neither run can
// observe files the other already wrote), and asserting the two faults'
// relativized partial evidence are byte-equal to each other. This proves
// `run_machine_update`'s own partial-evidence contract is deterministic,
// not just that it happens to not crash.
#[test]
fn run_machine_update_mid_fault_preserves_exact_partial_attribution() {
    fn relativize(evidence: ProjectionEvidence, base: &Path) -> ProjectionEvidence {
        let fix_path = |p: PathBuf| p.strip_prefix(base).unwrap_or(&p).to_path_buf();
        let fix_result = |r: ProjectionPathResult| ProjectionPathResult {
            path: fix_path(r.path),
            change: r.change,
        };
        ProjectionEvidence {
            common: evidence.common.into_iter().map(fix_result).collect(),
            by_target: evidence
                .by_target
                .into_iter()
                .map(|(target, target_evidence)| {
                    (
                        target,
                        ProjectionTargetEvidence {
                            paths: target_evidence.paths.into_iter().map(fix_result).collect(),
                            ..target_evidence
                        },
                    )
                })
                .collect(),
            completed_targets: evidence.completed_targets,
            run_warnings: evidence.run_warnings,
        }
    }

    let (temp, opts) = fault_mid_machine_options();
    let failure = run_machine_update(&opts)
        .expect_err("a file blocking the copilot agents directory must fault, not succeed");

    // Independent reconstruction: a fresh fixture instance (separate temp
    // root, so this second run cannot observe files the first run already
    // wrote — `write_text`'s identical-content skip would otherwise make the
    // second run silently under-report).
    let (temp2, opts2) = fault_mid_machine_options();
    let failure2 = run_machine_update(&opts2)
        .expect_err("a file blocking the copilot agents directory must fault, not succeed");

    let actual = relativize(failure.partial, temp.path());
    let expected = relativize(failure2.partial, temp2.path());
    assert_eq!(
        actual, expected,
        "partial evidence at the fault must be byte-equal across two independent runs of the \
         identical fixture"
    );
    assert!(
        expected
            .by_target
            .contains_key(&ProjectionTargetId::Copilot),
        "sanity: the fixture must actually attribute real pre-fault work to the copilot target"
    );
    assert!(
        !expected.by_target[&ProjectionTargetId::Copilot]
            .paths
            .is_empty(),
        "sanity: copilot target evidence must contain at least the pre-fault ccync link"
    );
    assert!(
        expected.completed_targets.is_empty(),
        "sanity: a faulted run must not mark any target completed"
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
    reg.persist(&[], false, &mut report).unwrap();

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
    reg.persist(&[], false, &mut report).unwrap();

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
    reg.persist(&[], false, &mut report).unwrap();

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
        selected_projection_targets: vec!["codex".into()],
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
        selected_projection_targets: vec!["codex".into()],
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
        evidence_all_warnings(&report).is_empty(),
        "single-source must emit no warnings"
    );
}

#[test]
fn agy_gui_projects_whole_plugin_junction() {
    // Probe: agy-gui now projects the same whole-plugin junction mechanism
    // as agy-cli/agy-ide, at ~/.gemini/config/plugins/ccync — the former
    // per-skill decomposition + dedicated MCP writer are retired.
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "doc-sync");

    let canonical = home.join(".ccync").join("build").join("render");
    fs::create_dir_all(&canonical).unwrap();

    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec!["agy-gui".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    let gui_junction = home
        .join(".gemini")
        .join("config")
        .join("plugins")
        .join("ccync");
    assert!(
        gui_junction.exists(),
        "agy-gui must project a whole-plugin junction at ~/.gemini/config/plugins/ccync"
    );

    let gui_skill = home
        .join(".gemini")
        .join("antigravity")
        .join("skills")
        .join("doc-sync");
    assert!(
        !gui_skill.exists(),
        "the retired per-skill decomposition must never write here again"
    );
    let mcp_config = home
        .join(".gemini")
        .join("antigravity")
        .join("mcp_config.json");
    assert!(
        !mcp_config.exists(),
        "the retired dedicated GUI mcp_config.json writer must never write here again"
    );
}

#[test]
fn agy_gui_not_selected_writes_no_gui_surface() {
    // Negative: when agy-gui is absent from the selected set neither the
    // junction nor any retired GUI surface is written.
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
        selected_projection_targets: vec!["codex".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    let gui_junction = home
        .join(".gemini")
        .join("config")
        .join("plugins")
        .join("ccync");
    assert!(
        !gui_junction.exists(),
        "no GUI junction without agy-gui selected"
    );
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

// Regression for the audit's FINDING-001: a user-owned real directory at the
// agy-gui target with no ownership row must resolve to `skipped`, not an
// ordinary `Unchanged` completion. The preserve branch in `update_skills`
// once pushed only raw warning strings into evidence and recorded no `paths`
// entry, which left the target indistinguishable from `Unchanged` at the
// `ProjectionTargetEvidence` level even though nothing was actually
// published.
#[test]
fn agy_gui_preserves_user_owned_directory_and_reports_skipped_not_unchanged() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "doc-sync");

    let canonical = home.join(".ccync").join("build").join("render");
    fs::create_dir_all(&canonical).unwrap();

    let gui_target = home
        .join(".gemini")
        .join("config")
        .join("plugins")
        .join("ccync");
    fs::create_dir_all(&gui_target).unwrap();
    fs::write(gui_target.join("users-own-file.txt"), "do not touch").unwrap();

    let evidence = run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec!["agy-gui".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(
        gui_target.join("users-own-file.txt").exists(),
        "an unowned real directory must survive untouched"
    );

    let target_evidence = evidence
        .by_target
        .get(&ProjectionTargetId::AgyGui)
        .expect("agy-gui must record its own target evidence");
    assert_eq!(
        target_evidence.skipped,
        Some("preserved user-owned path".to_string()),
        "a preserved unowned directory must report Skipped, not Unchanged: {:?}",
        target_evidence
    );
    assert!(
        target_evidence.paths.is_empty(),
        "a preserved path must not be recorded as a path change"
    );
}

// Reported-status-level proof of the plan's own "neither run reports a
// spurious change" Test Case for the agy-gui root: `publish_agy_root`'s
// `wrote` flag (DEV-04) is exercised at the `support.rs` unit level
// already, but nothing above that confirmed the flag actually reaches
// `update_skills`'s evidence. A second consecutive publish with no
// upstream change must record no path event for agy-gui, so a stable
// rerun reports `Unchanged` rather than a spurious `Applied`.
#[test]
fn agy_gui_second_consecutive_publish_records_no_path_event() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "doc-sync");

    let canonical = home.join(".ccync").join("build").join("render");
    fs::create_dir_all(&canonical).unwrap();

    let options = SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec!["agy-gui".into()],
        dry_run: false,
        replace: true,
    };

    let first = run_update_skills(&options).unwrap();
    let first_target = first
        .by_target
        .get(&ProjectionTargetId::AgyGui)
        .expect("first publish must record agy-gui target evidence");
    assert!(
        !first_target.paths.is_empty(),
        "the initial publish must report a real path change"
    );

    let second = run_update_skills(&options).unwrap();
    let second_target = second.by_target.get(&ProjectionTargetId::AgyGui);
    assert!(
        second_target.map(|t| t.paths.is_empty()).unwrap_or(true),
        "an unchanged republish must record no path event for agy-gui, \
         or every sync reports a spurious change: {:?}",
        second_target
    );
}

// A deselected `agy-gui` root that was published as a link is unlinked, and
// its ownership row must go with it. A row whose artifact is gone is standing
// `remove_dir_all` authority over whatever later occupies that path, and
// nothing else collects it — `persist` re-serializes this category verbatim
// from disk. A surviving directory copy keeps its row on purpose: that row is
// the proof `ccync uninstall` needs to remove it later.
#[test]
fn agy_gui_deselection_releases_the_ownership_row_with_the_link() {
    use crate::support::AgyRootOwnership;

    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "doc-sync");

    let canonical = home.join(".ccync").join("build").join("render");
    fs::create_dir_all(&canonical).unwrap();

    let gui_target = home
        .join(".gemini")
        .join("config")
        .join("plugins")
        .join("ccync");
    fs::create_dir_all(gui_target.parent().unwrap()).unwrap();
    if create_dir_link(&canonical, &gui_target).is_err() {
        return; // host permits neither symlink nor junction here
    }

    let lockfile = home.join(".ccync").join("build").join("lock.json");
    let ownership = AgyRootOwnership::from_lockfile(&lockfile);
    ownership.record_now(&gui_target, false).unwrap();
    assert!(
        AgyRootOwnership::from_lockfile(&lockfile).is_owned(&gui_target),
        "precondition: the published link is recorded as owned"
    );

    run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec!["codex".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(!gui_target.exists(), "the link itself must be unlinked");
    assert!(
        !AgyRootOwnership::from_lockfile(&lockfile).is_owned(&gui_target),
        "the row must not outlive the artifact it authorizes removing"
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
    reg.persist(&[], false, &mut report).unwrap();

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

// persist() GC: an attribution row whose category row went missing (the path
// is no longer marked in any category) is dropped once its owning source is
// installed this invocation — an installed source has authority to judge its
// own rows stale. A row owned by a source absent from `installed_source_ids`
// (foreign to this invocation) survives untouched, matching the foreign-source
// protection `prune_stale_links` applies at prune time.
#[test]
fn registry_persist_gc_drops_orphan_attribution_for_installed_source_only() {
    use crate::support::{ManagedArtifactRegistry, MANAGED_AGENT_PATHS, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let lockfile = tmp.path().join("plugins.lock.json");
    let installed_path = tmp.path().join("skill-installed");
    let foreign_path = tmp.path().join("skill-foreign");

    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg.mark_with_source(MANAGED_SKILL_PATHS, &installed_path, "my-bundle");
    reg.mark_with_source(MANAGED_SKILL_PATHS, &foreign_path, "other-bundle");
    // Keep a row alive in a different category so the marker survives once
    // MANAGED_SKILL_PATHS is cleared below.
    reg.mark_with_source(
        MANAGED_AGENT_PATHS,
        &tmp.path().join("keep-alive.md"),
        "keep-alive",
    );
    reg.persist(&[], false, &mut report).unwrap();

    // Simulate both attribution rows becoming orphans: their category row
    // goes missing while attribution survives.
    let mut reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg2.replace_category(MANAGED_SKILL_PATHS);
    // "my-bundle" is installed this invocation, "other-bundle" is not.
    reg2.persist(&["my-bundle"], false, &mut report).unwrap();

    let reg3 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    assert_eq!(
        reg3.owning_source_of(&installed_path),
        None,
        "orphan row owned by an installed source must be GC'd on persist"
    );
    assert_eq!(
        reg3.owning_source_of(&foreign_path),
        Some("other-bundle"),
        "orphan row owned by a source foreign to this invocation must survive"
    );
}

// The projection marker (`_ccyncProjection`) must not be dropped merely
// because every managed-artifact category is empty — a retained attribution
// row (e.g. a foreign-source orphan the GC above preserves) still needs the
// marker present so it round-trips on the next load.
#[test]
fn registry_persist_retains_marker_when_only_attribution_remains() {
    use crate::support::{ManagedArtifactRegistry, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let lockfile = tmp.path().join("plugins.lock.json");
    let foreign_path = tmp.path().join("skill-foreign");

    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg.mark_with_source(MANAGED_SKILL_PATHS, &foreign_path, "other-bundle");
    reg.persist(&[], false, &mut report).unwrap();

    // Every category empties out, but the foreign-source attribution row
    // survives the GC (its owner is absent from `installed_source_ids`).
    let mut reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg2.replace_category(MANAGED_SKILL_PATHS);
    reg2.persist(&["my-bundle"], false, &mut report).unwrap();

    assert!(
        lockfile.exists(),
        "lockfile must still be written while an attribution row is retained"
    );
    let raw = fs::read_to_string(&lockfile).unwrap();
    assert!(
        raw.contains("_ccyncProjection"),
        "projection marker must be retained when a retained attribution row remains"
    );

    let reg3 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    assert_eq!(
        reg3.owning_source_of(&foreign_path),
        Some("other-bundle"),
        "retained attribution row must survive the round-trip"
    );
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
        selected_projection_targets: vec!["codex".into()],
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
        report.run_warnings.len(),
        1,
        "exactly one collision warning expected"
    );
    let msg = &report.run_warnings[0];
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
        selected_projection_targets: vec!["codex".into()],
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
        report.run_warnings.len(),
        1,
        "exactly one collision warning expected"
    );
    let msg = &report.run_warnings[0];
    assert!(
        msg.contains("conflict-skill") && msg.contains("bundle-b") && msg.contains("bundle-a"),
        "warning must name skill and both non-core sources; got: {msg}"
    );
}

// Per-source prune safety: an owner present in `sources` (still installed,
// even mid-teardown, per the bundle-uninstall final-pass forward contract)
// is NOT foreign — a stale entry it owns falls through to normal
// authorization and is pruned, rather than being protected forever merely
// because its source is known.
#[test]
fn prune_stale_links_prunes_stale_item_owned_by_installed_source() {
    use crate::support::{prune_stale_links, ManagedArtifactRegistry, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();

    // Simulate two previously-projected skills: one from ccync-core, one from my-bundle.
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
    reg.persist(&[], false, &mut report).unwrap();

    // Reload (simulate a fresh invocation).
    let reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);

    // Only ccync-core in keep_names (bundle-skill not projected this invocation),
    // but my-bundle is still present in `sources` — its own stale item must be pruned.
    let sources = vec![
        ProjectionSource {
            root: PathBuf::new(),
            id: "ccync-core".into(),
            persistent: true,
        },
        ProjectionSource {
            root: PathBuf::new(),
            id: "my-bundle".into(),
            persistent: false,
        },
    ];
    prune_stale_links(&root, ["core-skill"], false, &reg2, &sources, None).unwrap();

    assert!(core_skill.exists(), "core skill in keep list must be kept");
    assert!(
        !bundle_skill.exists(),
        "stale item owned by a still-installed source must be pruned, not protected forever"
    );
}

// Per-source prune safety: an owner entirely absent from `sources` (foreign to
// this invocation) is protected even when normal authorization would otherwise
// allow removal — this invocation has no authority to judge whether it's stale.
#[test]
fn prune_stale_links_preserves_item_owned_by_foreign_source() {
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
    reg.persist(&[], false, &mut report).unwrap();

    let reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);

    // my-bundle is absent from `sources` entirely — foreign, so bundle-skill
    // must be preserved even though its CCYNC marker would otherwise authorize removal.
    let sources = vec![ProjectionSource {
        root: PathBuf::new(),
        id: "ccync-core".into(),
        persistent: true,
    }];
    prune_stale_links(&root, ["core-skill"], false, &reg2, &sources, None).unwrap();

    assert!(core_skill.exists(), "core skill must survive");
    assert!(
        bundle_skill.exists(),
        "item owned by a source foreign to this invocation must be preserved"
    );
}

// An orphan symlink whose category row went missing (e.g. lost to a
// prior GC bug) but whose attribution row survived is still pruned, live or
// dangling, once its target is proven to resolve inside the owning
// installed source's own root.
#[test]
fn prune_stale_links_orphan_symlink_with_in_root_attribution_is_pruned() {
    use crate::support::{
        prune_stale_links, ManagedArtifactRegistry, MANAGED_AGENT_PATHS, MANAGED_SKILL_PATHS,
    };

    let tmp = TempDir::new().unwrap();
    let owner_root = tmp.path().join("owned-source");
    fs::create_dir_all(&owner_root).unwrap();

    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();

    // Live orphan: the target still exists inside owner_root.
    let live_target = owner_root.join("live-target");
    fs::create_dir_all(&live_target).unwrap();
    let live_link = root.join("live-skill");
    create_dir_link(&live_target, &live_link).unwrap();

    // Dangling orphan: the target existed inside owner_root at link creation
    // time, then was removed — the link's own stored path still resolves
    // inside the root even though the target no longer exists.
    let dangling_target = owner_root.join("dangling-target");
    fs::create_dir_all(&dangling_target).unwrap();
    let dangling_link = root.join("dangling-skill");
    create_dir_link(&dangling_target, &dangling_link).unwrap();
    fs::remove_dir_all(&dangling_target).unwrap();

    let lockfile = tmp.path().join("lock.json");
    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg.mark_with_source(MANAGED_SKILL_PATHS, &live_link, "my-bundle");
    reg.mark_with_source(MANAGED_SKILL_PATHS, &dangling_link, "my-bundle");
    // Keep a row alive in a different category, untouched by the
    // `replace_category` below — `persist` drops the whole marker (and with
    // it every attribution row) once all categories go empty, which would
    // silently defeat this test's premise rather than exercise it.
    reg.mark_with_source(
        MANAGED_AGENT_PATHS,
        &tmp.path().join("keep-alive.md"),
        "keep-alive",
    );
    reg.persist(&[], false, &mut report).unwrap();

    // Simulate the category row going missing while attribution survives.
    let mut reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg2.replace_category(MANAGED_SKILL_PATHS);
    reg2.persist(&[], false, &mut report).unwrap();

    let reg3 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    let sources = vec![ProjectionSource {
        root: owner_root.clone(),
        id: "my-bundle".into(),
        persistent: false,
    }];

    prune_stale_links(&root, [], false, &reg3, &sources, None).unwrap();

    assert!(
        fs::symlink_metadata(&live_link).is_err(),
        "live orphan symlink with in-root attributed target must be pruned"
    );
    assert!(
        fs::symlink_metadata(&dangling_link).is_err(),
        "dangling orphan symlink with in-root attributed target must be pruned"
    );
}

// The same missing-category-row-plus-attribution setup as the preceding
// in-root-orphan test must NOT authorize removal of a real (non-symlink)
// directory, a live symlink whose target resolves outside the owner's root,
// or a dangling symlink whose stored target carries a `..` (ParentDir)
// component — none of these prove ownership of an in-root artifact, so all
// three are preserved.
#[test]
fn prune_stale_links_preserves_real_dir_external_target_and_parent_dir_target() {
    use crate::support::{
        prune_stale_links, ManagedArtifactRegistry, MANAGED_AGENT_PATHS, MANAGED_SKILL_PATHS,
    };

    let tmp = TempDir::new().unwrap();
    let owner_root = tmp.path().join("owned-source");
    fs::create_dir_all(&owner_root).unwrap();
    let outside_dir = tmp.path().join("outside-source");
    fs::create_dir_all(&outside_dir).unwrap();

    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();

    // Case 1: a real (non-symlink) directory — never authorized via the
    // symlink-only source-root path.
    let real_dir = root.join("real-skill");
    fs::create_dir_all(&real_dir).unwrap();

    // Case 2: a live symlink whose target lives outside owner_root.
    let external_target = outside_dir.join("external-target");
    fs::create_dir_all(&external_target).unwrap();
    let external_link = root.join("external-skill");
    create_dir_link(&external_target, &external_link).unwrap();

    let lockfile = tmp.path().join("lock.json");
    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg.mark_with_source(MANAGED_SKILL_PATHS, &real_dir, "my-bundle");
    reg.mark_with_source(MANAGED_SKILL_PATHS, &external_link, "my-bundle");
    // Keep a row alive in a different category so `persist` retains the
    // marker (and the attribution rows under it) once MANAGED_SKILL_PATHS
    // is cleared below — see the matching comment in the in-root-orphan test above.
    reg.mark_with_source(
        MANAGED_AGENT_PATHS,
        &tmp.path().join("keep-alive.md"),
        "keep-alive",
    );
    reg.persist(&[], false, &mut report).unwrap();

    let mut reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg2.replace_category(MANAGED_SKILL_PATHS);
    reg2.persist(&[], false, &mut report).unwrap();

    let reg3 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    let sources = vec![ProjectionSource {
        root: owner_root.clone(),
        id: "my-bundle".into(),
        persistent: false,
    }];

    prune_stale_links(&root, [], false, &reg3, &sources, None).unwrap();

    assert!(
        real_dir.exists(),
        "a real directory must never be authorized via the symlink-only source-root path"
    );
    assert!(
        fs::symlink_metadata(&external_link).is_ok(),
        "a live symlink resolving outside the owner's root must be preserved"
    );

    // Case 3: a dangling symlink whose raw stored target contains `..` —
    // exercised directly against the target-proof helper, since Windows
    // junctions (the unprivileged reparse mechanism this crate relies on
    // elsewhere) always store an absolute, already-resolved target and
    // cannot carry a literal `..` component to round-trip through
    // `read_link`. A real symlink can, but creating one on Windows needs
    // Developer Mode or admin — skip gracefully when unavailable, matching
    // the existing privilege-aware pattern in this workspace.
    let parent_dir_link = root.join("parent-dir-skill");
    let traversal_target = root.join("..").join("owned-source").join("live-target");
    let made_link = {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&traversal_target, &parent_dir_link).is_ok()
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(&traversal_target, &parent_dir_link).is_ok()
        }
    };
    if made_link {
        assert!(
            !crate::support::is_symlink_target_inside_root(&parent_dir_link, &owner_root),
            "a dangling target containing a ParentDir component must never prove in-root ownership"
        );
    }
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
    let mut reg = ManagedArtifactRegistry::load(&path, &mut report);
    reg.persist(&[], false, &mut report).unwrap();

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
    reg_a.persist(&[], false, &mut report).unwrap();

    // Registry B (ccync-self.json): records ccync-self-artifact as owned by "ccync-self".
    let lock_b = tmp.path().join("ccync-self.json");
    let mut reg_b = ManagedArtifactRegistry::load(&lock_b, &mut report);
    reg_b.mark_with_source(MANAGED_SKILL_PATHS, &ccync_self_artifact, "ccync-self");
    reg_b.persist(&[], false, &mut report).unwrap();

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
        &reg_a2,
        &[ProjectionSource {
            root: PathBuf::new(),
            id: "ccync".into(),
            persistent: true,
        }],
        None,
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
        &reg_b2,
        &[ProjectionSource {
            root: PathBuf::new(),
            id: "ccync-self".into(),
            persistent: true,
        }],
        None,
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
    fs::write(plugin.join("agents").join("my-architect.agent.md"), "x").unwrap();
    // .mcp.json
    fs::write(
        plugin.join(".mcp.json"),
        r#"{"servers":{"memory":{},"fetch":{}}}"#,
    )
    .unwrap();

    let c = decompose_plugin(&plugin).unwrap();
    assert_eq!(c.skills, vec!["doc-sync", "git-commits"]);
    assert_eq!(c.commands, vec!["ccync-status"]);
    assert_eq!(c.agents, vec!["my-architect.agent.md"]);
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

// ── single-mode resolve_projection_roots (D-07) ─────────────────────────────

#[test]
fn resolve_projection_roots_returns_canonical_root_no_err() {
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
    let (repo_root, source_root) = resolve_projection_roots(&CcyncConfig::default()).unwrap();
    assert_eq!(repo_root, source_root);
    assert!(
        repo_root.ends_with(Path::new("build").join("render")),
        "resolve_projection_roots must return ~/.ccync/build/render, got {repo_root:?}"
    );
}

// ── name-agnostic ownership regression (option B) ──────────────────────────────

#[test]
fn arbitrary_name_agent_managed_set_membership_allows_prune() {
    use crate::support::{prune_stale_links, ManagedArtifactRegistry, MANAGED_AGENT_PATHS};

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("agents");
    fs::create_dir_all(&root).unwrap();

    let managed_agent = root.join("my-tool.agent.md");
    let user_agent = root.join("user-agent.agent.md");

    fs::write(&managed_agent, "---\nname: my-tool\n---\nbody\n").unwrap();
    fs::write(&user_agent, "---\nname: user-tool\n---\nbody\n").unwrap();

    let lockfile = tmp.path().join("build").join("lock.json");
    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg.mark(MANAGED_AGENT_PATHS, &managed_agent);
    reg.persist(&[], false, &mut report).unwrap();

    let reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    prune_stale_links(&root, [], false, &reg2, &[], None).unwrap();

    assert!(
        !managed_agent.exists(),
        "managed-set member must be pruned when no longer active (option B)"
    );
    assert!(
        user_agent.exists(),
        "non-managed user-owned agent must be preserved"
    );
}

#[test]
fn arbitrary_name_agent_first_run_preserve_user_file() {
    use crate::support::{prune_stale_links, ManagedArtifactRegistry};

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("agents");
    fs::create_dir_all(&root).unwrap();

    let user_agent = root.join("my-custom-agent.agent.md");
    fs::write(&user_agent, "---\nname: my-custom-agent\n---\nbody\n").unwrap();

    let mut report = ProjectionReport::default();
    let registry = ManagedArtifactRegistry::load(&tmp.path().join("nolock.json"), &mut report);

    prune_stale_links(&root, [], false, &registry, &[], None).unwrap();

    assert!(
        user_agent.exists(),
        "user-owned agent must survive first-run prune (empty prior lockfile)"
    );
}

// ── evidence-shaped residual producers (skills/commands/personalization) ────

#[test]
fn update_skills_with_evidence_attributes_copilot_link_to_owner() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "sample-skill");

    let evidence = run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec!["copilot".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    let copilot = evidence
        .by_target
        .get(&ProjectionTargetId::Copilot)
        .expect("copilot must own the ~/.copilot/ccync junction");
    assert!(copilot.paths.contains(&ProjectionPathResult {
        path: home.join(".copilot").join("ccync"),
        change: ProjectionPathChange::Linked,
    }));
    assert!(
        !evidence.by_target.contains_key(&ProjectionTargetId::Codex),
        "codex was not selected and produced no evidence, so it must have no entry at all"
    );
}

#[test]
fn update_skills_with_evidence_shared_skills_surface_falls_back_to_common() {
    // codex/opencode share `~/.agents/skills` — not uniquely owned by one
    // target, so the producer must attribute it to `common`.
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "sample-skill");

    let evidence = run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec!["codex".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(evidence.common.contains(&ProjectionPathResult {
        path: home.join(".agents").join("skills").join("sample-skill"),
        change: ProjectionPathChange::Linked,
    }));
    assert!(
        evidence.by_target.is_empty(),
        "codex's own agents block wrote nothing (no agents in the fixture), so no target entry should exist: {:?}",
        evidence.by_target
    );
}

#[test]
fn update_skills_with_evidence_second_identical_run_is_a_true_no_op() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "sample-skill");
    let opts = SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        user_home: home,
        selected_projection_targets: vec!["copilot".into()],
        dry_run: false,
        replace: true,
    };

    run_update_skills(&opts).unwrap();
    let second = run_update_skills(&opts).unwrap();

    assert_eq!(
        second,
        ProjectionEvidence::default(),
        "an already-converged second run must record no further evidence at all"
    );
}

#[test]
fn update_skills_with_evidence_mid_run_fault_preserves_exact_partial_attribution() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "sample-skill");
    // Block codex's own `ensure_dir(&codex_agents_target(), ..)` with a plain
    // file occupying the directory's own path — codex's block is the last
    // one to run in `update_skills_with_evidence`, so copilot's own evidence
    // (processed earlier) must already be present in the fault's partial.
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(home.join(".codex").join("agents"), "blocking file").unwrap();

    let failure = run_update_skills(&SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec!["copilot".into(), "codex".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap_err();

    let copilot = failure
        .partial
        .by_target
        .get(&ProjectionTargetId::Copilot)
        .expect("copilot's own block ran to completion before codex's own block faulted");
    assert!(copilot.paths.contains(&ProjectionPathResult {
        path: home.join(".copilot").join("ccync"),
        change: ProjectionPathChange::Linked,
    }));
    assert!(
        !failure.partial.by_target.contains_key(&ProjectionTargetId::Codex),
        "codex's own block faulted on its very first statement (ensure_dir), before writing anything"
    );
}

#[test]
fn update_commands_with_evidence_selecting_gemini_cli_records_no_owned_paths() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "status");

    let evidence = run_update_commands(&CommandUpdateOptions {
        repo_root: repo_root.clone(),
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec!["gemini-cli".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(
        evidence.by_target.is_empty(),
        "gemini-cli no longer parses to any target, so no path may be owned in update_commands: {:?}",
        evidence
    );
}

#[test]
fn update_commands_with_evidence_canonical_bake_falls_back_to_common() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "status");

    let evidence = run_update_commands(&CommandUpdateOptions {
        repo_root: repo_root.clone(),
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec![],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert!(evidence.common.iter().any(|p| p.path
        == home
            .join(".ccync")
            .join("build")
            .join("render")
            .join("commands")
            .join("status")
            .join("SKILL.md")
        && p.change == ProjectionPathChange::Written));
    assert!(
        evidence.by_target.is_empty(),
        "no runtime was selected, so nothing should be attributed to a target: {:?}",
        evidence.by_target
    );
}

#[test]
fn update_commands_with_evidence_no_command_dirs_records_empty_evidence() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    // fixture_roots() creates an empty `commands/` dir with no children.

    let evidence = run_update_commands(&CommandUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        user_home: home,
        selected_projection_targets: vec!["gemini-cli".into()],
        dry_run: false,
        replace: true,
    })
    .unwrap();

    assert_eq!(evidence, ProjectionEvidence::default());
}

#[test]
fn update_commands_with_evidence_mid_run_fault_preserves_exact_partial_attribution() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command_fixture(&source_root, "cmd-a");
    write_command_fixture(&source_root, "cmd-b");
    // Block the second command's own canonical directory (`cmd-a` sorts first)
    // with a plain file occupying its own path, forcing `ensure_dir` to fault
    // only once `cmd-a`'s own write has already completed.
    let canonical_commands_root = home
        .join(".ccync")
        .join("build")
        .join("render")
        .join("commands");
    fs::create_dir_all(&canonical_commands_root).unwrap();
    fs::write(canonical_commands_root.join("cmd-b"), "blocking file").unwrap();

    let failure = run_update_commands(&CommandUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec![],
        dry_run: false,
        replace: true,
    })
    .unwrap_err();

    assert!(failure.partial.common.iter().any(|p| p.path
        == canonical_commands_root.join("cmd-a").join("SKILL.md")
        && p.change == ProjectionPathChange::Written));
    assert!(
        !failure
            .partial
            .common
            .iter()
            .any(|p| p.path.starts_with(canonical_commands_root.join("cmd-b"))),
        "cmd-b's own block faulted on its own ensure_dir, before writing anything"
    );
}

#[test]
fn update_personalization_with_evidence_selecting_gemini_cli_records_no_owned_paths() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "sample-skill");
    let appdata = home.join("appdata");
    fs::create_dir_all(&appdata).unwrap();

    let evidence = run_update_personalization(&PersonalizationUpdateOptions {
        repo_root,
        source_root,
        user_home: home.clone(),
        appdata_root: appdata,
        selected_projection_targets: vec!["gemini-cli".into()],
        dry_run: false,
        replace: true,
        uninstall: false,
    })
    .unwrap();

    assert!(
        evidence.by_target.is_empty(),
        "gemini-cli no longer parses to any target, so no path may be owned in update_personalization: {:?}",
        evidence
    );
}

#[test]
fn update_personalization_with_evidence_preserved_repo_rule_warning_falls_back_to_common() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    let appdata = home.join("appdata");
    fs::create_dir_all(&appdata).unwrap();
    let rules_dir = repo_root.join(".agents").join("rules");
    fs::create_dir_all(&rules_dir).unwrap();
    fs::write(
        rules_dir.join("ccync.md"),
        "# user-owned, no ccync header\n",
    )
    .unwrap();

    let evidence = run_update_personalization(&PersonalizationUpdateOptions {
        repo_root,
        source_root,
        user_home: home,
        appdata_root: appdata,
        selected_projection_targets: vec![],
        dry_run: false,
        replace: true,
        uninstall: false,
    })
    .unwrap();

    assert!(
        evidence
            .run_warnings
            .iter()
            .any(|w| w.contains("preserved user-owned repo rule")),
        "the warning has no single owning target, so it must land in run_warnings (common), not by_target: {:?}",
        evidence
    );
    assert!(evidence.by_target.is_empty());
}

#[test]
fn update_personalization_with_evidence_no_selection_records_empty_evidence() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    let appdata = home.join("appdata");
    fs::create_dir_all(&appdata).unwrap();

    let evidence = run_update_personalization(&PersonalizationUpdateOptions {
        repo_root,
        source_root,
        user_home: home,
        appdata_root: appdata,
        selected_projection_targets: vec![],
        dry_run: false,
        replace: true,
        uninstall: false,
    })
    .unwrap();

    assert_eq!(evidence, ProjectionEvidence::default());
}

// ensure_dir_link: a genuine user-owned real directory at the link path (no
// CCYNC content marker) is preserved untouched — returns `false` so the
// caller must not record it in the managed-artifact registry, and warns.
#[test]
fn ensure_dir_link_preserves_user_owned_path_returns_false_with_warning() {
    use crate::support::ensure_dir_link;

    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("target");
    fs::create_dir_all(&target).unwrap();
    let link = tmp.path().join("user-dir");
    fs::create_dir_all(&link).unwrap();
    fs::write(link.join("notes.md"), "personal notes\n").unwrap();

    let mut report = ProjectionReport::default();
    let linked = ensure_dir_link(&target, &link, false, false, &mut report).unwrap();

    assert!(!linked, "genuine user-owned directory must not be linked");
    assert!(
        link.join("notes.md").exists(),
        "user content must survive untouched"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.message.contains("preserved user-owned path")),
        "preserve-user-path must emit a warning"
    );
    assert!(
        report.created_links.is_empty(),
        "preserved path must not be recorded as a created link"
    );
}

// ensure_dir_link: fresh creation returns `true`. Every production call site
// now pairs the return value with `mark_with_source`, guarded on `true` — this
// exercises that pairing directly and confirms the correct owning source id
// round-trips through the registry.
#[test]
fn ensure_dir_link_fresh_creation_returns_true_and_records_source_id() {
    use crate::support::{ensure_dir_link, ManagedArtifactRegistry, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("target");
    fs::create_dir_all(&target).unwrap();
    let link = tmp.path().join("skills").join("my-skill");

    let mut report = ProjectionReport::default();
    let linked = ensure_dir_link(&target, &link, false, false, &mut report).unwrap();
    assert!(linked, "fresh creation must return true");
    assert!(is_symlink_or_junction(&link));

    let lockfile = tmp.path().join("lock.json");
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    if linked {
        reg.mark_with_source(MANAGED_SKILL_PATHS, &link, "my-bundle");
    }
    reg.persist(&[], false, &mut report).unwrap();

    let reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    assert_eq!(
        reg2.owning_source_of(&link),
        Some("my-bundle"),
        "ensured link must record the correct owning source id"
    );
}

// Regression for the false-attribution bug fixed alongside `ensure_dir_link`'s
// `Ok(bool)` split: a preserve-user-path outcome must never reach the
// registry, so the user's own directory disappears from both the
// managed-artifact categories and `sourceAttribution` — mirroring every
// production call site's `if linked { mark_with_source(...) }` guard.
#[test]
fn ensure_dir_link_preserved_path_never_recorded_in_registry() {
    use crate::support::{ensure_dir_link, ManagedArtifactRegistry, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("target");
    fs::create_dir_all(&target).unwrap();
    let link = tmp.path().join("skills").join("user-owned");
    fs::create_dir_all(&link).unwrap();
    fs::write(link.join("SKILL.md"), "# not ccync\n").unwrap();

    let mut report = ProjectionReport::default();
    let linked = ensure_dir_link(&target, &link, false, false, &mut report).unwrap();
    assert!(!linked);

    let lockfile = tmp.path().join("lock.json");
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    if linked {
        reg.mark_with_source(MANAGED_SKILL_PATHS, &link, "my-bundle");
    }
    reg.persist(&[], false, &mut report).unwrap();

    assert!(
        !lockfile.exists(),
        "nothing was ever marked, so persist on an empty prior/next pair must not write a lockfile"
    );
    let reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    assert_eq!(
        reg2.owning_source_of(&link),
        None,
        "preserved user path must never carry source attribution"
    );
}

// GC: an orphan attribution row (category row missing, attribution row
// alive) owned by a now-installed source is dropped on the first persist
// that names that source in `installed_source_ids` — the lockfile rewrites.
// A second persist over the now-unchanged registry state is a true no-op:
// byte-identical output, proving GC does not re-fire or otherwise perturb
// output once the orphan is already gone.
#[test]
fn registry_persist_orphan_rewrites_once_then_reruns_as_noop() {
    use crate::support::{ManagedArtifactRegistry, MANAGED_AGENT_PATHS, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let lockfile = tmp.path().join("lock.json");
    let orphan_path = tmp.path().join("skill-orphan");

    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg.mark_with_source(MANAGED_SKILL_PATHS, &orphan_path, "my-bundle");
    // Keep a row alive in a different category so the marker survives once
    // MANAGED_SKILL_PATHS is cleared below.
    reg.mark_with_source(
        MANAGED_AGENT_PATHS,
        &tmp.path().join("keep-alive.md"),
        "keep-alive",
    );
    reg.persist(&[], false, &mut report).unwrap();
    let before = fs::read_to_string(&lockfile).unwrap();
    assert!(before.contains("skill-orphan"));

    // Orphan the row: category row goes missing, attribution row survives.
    let mut reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg2.replace_category(MANAGED_SKILL_PATHS);
    reg2.persist(&["my-bundle"], false, &mut report).unwrap();
    let after_gc = fs::read_to_string(&lockfile).unwrap();
    assert_ne!(
        before, after_gc,
        "GC of a live orphan owned by an installed source must rewrite the lockfile"
    );
    assert!(
        !after_gc.contains("skill-orphan"),
        "orphan row owned by an installed source must be gone after the rewrite"
    );

    // Rerun with an unchanged registry state: prior == next, nothing left to
    // GC — must be a true no-op.
    let mut reg3 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg3.persist(&["my-bundle"], false, &mut report).unwrap();
    let after_rerun = fs::read_to_string(&lockfile).unwrap();
    assert_eq!(
        after_gc, after_rerun,
        "identical rerun with nothing left to GC must leave the lockfile byte-identical"
    );
}

// Foreign-only attribution (owner absent from `installed_source_ids`)
// survives an orphaning GC pass untouched, and `persist` — which only ever
// writes the lockfile itself — never touches an unrelated sibling path
// living beside the projected artifacts on disk.
#[test]
fn registry_persist_foreign_attribution_survives_sibling_immutable() {
    use crate::support::{ManagedArtifactRegistry, MANAGED_AGENT_PATHS, MANAGED_SKILL_PATHS};

    let tmp = TempDir::new().unwrap();
    let lockfile = tmp.path().join("lock.json");
    let foreign_path = tmp.path().join("skill-foreign");
    let sibling = tmp.path().join("skill-sibling.md");
    fs::write(&sibling, "unrelated user content\n").unwrap();
    let sibling_before = fs::read(&sibling).unwrap();

    let mut report = ProjectionReport::default();
    let mut reg = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg.mark_with_source(MANAGED_SKILL_PATHS, &foreign_path, "other-bundle");
    reg.mark_with_source(
        MANAGED_AGENT_PATHS,
        &tmp.path().join("keep-alive.md"),
        "keep-alive",
    );
    reg.persist(&[], false, &mut report).unwrap();

    // Orphan the row while "other-bundle" stays absent from this
    // invocation's installed set — foreign, so GC must not touch it.
    let mut reg2 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    reg2.replace_category(MANAGED_SKILL_PATHS);
    reg2.persist(&["my-bundle"], false, &mut report).unwrap();

    let reg3 = ManagedArtifactRegistry::load(&lockfile, &mut report);
    assert_eq!(
        reg3.owning_source_of(&foreign_path),
        Some("other-bundle"),
        "foreign-owned orphan row must survive persist and reload"
    );
    assert_eq!(
        fs::read(&sibling).unwrap(),
        sibling_before,
        "an unrecorded sibling path must remain byte-identical — persist touches only the lockfile"
    );
}

// End-to-end idempotence: rerunning the full skill/agent projection pipeline
// with identical inputs must leave every projected surface — files, link
// targets, and the lockfile that lives inside the same tree — byte-identical.
#[test]
fn run_update_skills_identical_rerun_is_byte_identical() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "sample-skill");
    fs::write(
        source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\ncolor: green\ntools: [read, edit, execute]\n---\nBody\n",
    )
    .unwrap();

    let options = SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec![
            "copilot".into(),
            "gemini-cli".into(),
            "agy-cli".into(),
            "opencode".into(),
        ],
        dry_run: false,
        replace: true,
    };

    run_update_skills(&options).unwrap();
    let first = snapshot_tree(&home);
    assert!(
        !first.is_empty(),
        "test setup: the pipeline must project something to snapshot"
    );

    run_update_skills(&options).unwrap();
    let second = snapshot_tree(&home);

    assert_eq!(
        first, second,
        "identical rerun must leave every projected surface and the lockfile byte-identical"
    );
}

// Dry-run over already-projected state must leave disk and the lockfile
// completely untouched — every write path (`write_text`, `ensure_dir_link`,
// `prune_stale_links`, `ManagedArtifactRegistry::persist`) threads `dry_run`
// through to its final I/O, not just its evidence bookkeeping.
#[test]
fn run_update_skills_dry_run_rerun_leaves_disk_byte_identical() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_skill_fixture(&source_root, "sample-skill");
    fs::write(
        source_root.join("agents").join("helper.agent.md"),
        "---\ndescription: helper\ncolor: green\ntools: [read, edit, execute]\n---\nBody\n",
    )
    .unwrap();

    let mut options = SkillUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec![
            "copilot".into(),
            "gemini-cli".into(),
            "agy-cli".into(),
            "opencode".into(),
        ],
        dry_run: false,
        replace: true,
    };

    run_update_skills(&options).unwrap();
    let before = snapshot_tree(&home);

    options.dry_run = true;
    run_update_skills(&options).unwrap();
    let after = snapshot_tree(&home);

    assert_eq!(
        before, after,
        "a dry-run rerun over already-projected state must leave disk and the lockfile byte-identical"
    );
}
