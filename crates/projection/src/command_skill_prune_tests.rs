use super::*;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn fixture_roots() -> (TempDir, PathBuf, PathBuf, PathBuf) {
    let temp = TempDir::new().unwrap();
    let repo_root = temp.path().join("repo");
    let source_root = repo_root.join("source");
    let home = temp.path().join("home");
    fs::create_dir_all(source_root.join("commands")).unwrap();
    fs::create_dir_all(source_root.join("skills")).unwrap();
    fs::create_dir_all(&home).unwrap();
    (temp, repo_root, source_root, home)
}

fn options(
    repo_root: PathBuf,
    source_root: PathBuf,
    home: PathBuf,
    dry_run: bool,
) -> CommandUpdateOptions {
    options_with_targets(
        repo_root,
        source_root,
        home,
        vec!["copilot".into()],
        dry_run,
    )
}

fn options_with_targets(
    repo_root: PathBuf,
    source_root: PathBuf,
    home: PathBuf,
    selected_projection_targets: Vec<String>,
    dry_run: bool,
) -> CommandUpdateOptions {
    CommandUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        state_root: home.join(".ccync").join("build"),
        user_home: home,
        selected_projection_targets,
        dry_run,
        replace: true,
    }
}

fn skill_options(
    repo_root: PathBuf,
    source_root: PathBuf,
    home: PathBuf,
    selected_projection_targets: Vec<String>,
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
        selected_projection_targets,
        dry_run: false,
        replace: true,
    }
}

fn write_command(source_root: &Path, name: &str) {
    fs::write(
        source_root.join("commands").join(format!("{name}.md")),
        "---\ndescription: sample command\n---\nbody\n",
    )
    .unwrap();
}

#[test]
fn direct_commands_reject_positional_argument_conversion_without_publication() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    fs::write(
        source_root.join("commands").join("deploy.md"),
        "---\ndescription: deploy\n---\nship release $1\n",
    )
    .unwrap();

    let evidence = run_update_commands(&options_with_targets(
        repo_root,
        source_root,
        home.clone(),
        vec!["codex".into(), "opencode".into()],
        false,
    ))
    .unwrap();

    assert!(!home.join(".agents/skills/deploy").exists());
    assert!(home.join(".config/opencode/commands/deploy.md").is_file());
    let failure = evidence
        .by_target
        .get(&ProjectionTargetId::Codex)
        .and_then(|target| target.failure.as_ref())
        .expect("typed command conversion failure should be reported");
    assert!(failure
        .reason
        .contains("command 'deploy' was not published"));
    assert!(failure.reason.contains("positional command arguments"));
}

#[test]
fn direct_commands_classify_unsupported_invocation_shapes() {
    assert_eq!(
        DirectCommandSkillInput {
            name: "deploy".into(),
            body: "run $@".into(),
        }
        .unsupported_reason(),
        Some(CommandSkillUnsupportedReason::ArgumentExpansion)
    );
    assert_eq!(
        DirectCommandSkillInput {
            name: "ops/deploy".into(),
            body: "run".into(),
        }
        .unsupported_reason(),
        Some(CommandSkillUnsupportedReason::NamespacedCommand)
    );
}

#[test]
fn direct_commands_report_namespaced_commands_without_publication() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    let command = source_root.join("commands").join("ops").join("deploy.md");
    fs::create_dir_all(command.parent().unwrap()).unwrap();
    fs::write(&command, "---\ndescription: deploy\n---\nship release\n").unwrap();

    let evidence = run_update_commands(&options_with_targets(
        repo_root,
        source_root,
        home.clone(),
        vec!["claude".into(), "opencode".into()],
        false,
    ))
    .unwrap();

    assert!(!home.join(".claude/commands/deploy.md").exists());
    assert!(!home.join(".config/opencode/commands/deploy.md").exists());
    assert!(evidence
        .by_target
        .get(&ProjectionTargetId::Claude)
        .and_then(|target| target.failure.as_ref())
        .is_some_and(|failure| failure.reason.contains("namespaced command")));
}

#[test]
fn direct_commands_preserve_direct_and_equivalent_semantics_and_protect_foreign_files() {
    let (_temp, repo_root, source_root, home) = fixture_roots();

    // 1. Pre-existing foreign unowned directory in ~/.copilot/skills/foreign-skill
    let foreign_skill = home.join(".copilot").join("skills").join("foreign-skill");
    fs::create_dir_all(&foreign_skill).unwrap();
    fs::write(foreign_skill.join("SKILL.md"), "foreign manual skill").unwrap();

    // 2. Equivalent flat command with $ARGUMENTS (no positional args, no shell expansion)
    fs::write(
        source_root.join("commands").join("audit.md"),
        "---\ndescription: perform audit\n---\ninspect project $ARGUMENTS\n",
    )
    .unwrap();

    // 3. Command with argument expansion $@
    fs::write(
        source_root.join("commands").join("batch.md"),
        "---\ndescription: run batch\n---\nprocess files $@\n",
    )
    .unwrap();

    let targets = vec!["copilot".into(), "codex".into(), "opencode".into()];
    let evidence = run_update_commands(&options_with_targets(
        repo_root,
        source_root,
        home.clone(),
        targets,
        false,
    ))
    .unwrap();

    // Foreign unowned file must be untouched
    assert!(foreign_skill.join("SKILL.md").is_file());
    assert_eq!(
        fs::read_to_string(foreign_skill.join("SKILL.md")).unwrap(),
        "foreign manual skill"
    );

    // Equivalent command 'audit' is published as a skill on Copilot and Codex, and as a native command on OpenCode
    let copilot_audit = home.join(".copilot").join("skills").join("audit");
    let codex_audit = home.join(".agents").join("skills").join("audit");
    let opencode_audit = home
        .join(".config")
        .join("opencode")
        .join("commands")
        .join("audit.md");
    assert!(copilot_audit.join("SKILL.md").is_file());
    assert!(codex_audit.join("SKILL.md").is_file());
    assert!(opencode_audit.is_file());

    // Argument-expansion command 'batch' cannot convert to skill: no partial publication on Copilot or Codex
    assert!(!home.join(".copilot").join("skills").join("batch").exists());
    assert!(!home.join(".agents").join("skills").join("batch").exists());

    // But OpenCode has a native command surface, so 'batch' is published there
    let opencode_batch = home
        .join(".config")
        .join("opencode")
        .join("commands")
        .join("batch.md");
    assert!(opencode_batch.is_file());
    let opencode_content = fs::read_to_string(&opencode_batch).unwrap();
    assert!(opencode_content.contains("process files $@"));

    // Copilot reports typed failure naming the command and the expansion reason
    let copilot_failure = evidence
        .by_target
        .get(&ProjectionTargetId::Copilot)
        .and_then(|target| target.failure.as_ref())
        .expect("Copilot must record failure for argument expansion");
    assert!(copilot_failure
        .reason
        .contains("command 'batch' was not published"));
    assert!(copilot_failure
        .reason
        .contains("command argument expansion"));

    // Codex also reports typed failure naming the command and reason
    let codex_failure = evidence
        .by_target
        .get(&ProjectionTargetId::Codex)
        .and_then(|target| target.failure.as_ref())
        .expect("Codex must record failure for argument expansion");
    assert!(codex_failure
        .reason
        .contains("command 'batch' was not published"));
    assert!(codex_failure.reason.contains("command argument expansion"));
}

fn write_skill(source_root: &Path, name: &str) {
    let dir = source_root.join("skills").join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: sample skill\n---\nskill body\n"),
    )
    .unwrap();
}

fn evidence_has_copilot_removal(evidence: &ProjectionEvidence, path: &Path) -> bool {
    evidence
        .by_target
        .get(&ProjectionTargetId::Copilot)
        .into_iter()
        .flat_map(|target| target.paths.iter())
        .any(|change| change.change == ProjectionPathChange::Removed && change.path == path)
}

fn evidence_has_common_removal(evidence: &ProjectionEvidence, path: &Path) -> bool {
    evidence
        .common
        .iter()
        .any(|change| change.change == ProjectionPathChange::Removed && change.path == path)
}

#[test]
fn prunes_stale_real_command_skill_when_no_commands_remain() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command(&source_root, "c");

    run_update_commands(&options(
        repo_root.clone(),
        source_root.clone(),
        home.clone(),
        false,
    ))
    .unwrap();
    let stale = home.join(".copilot").join("skills").join("c");
    assert!(stale.is_dir());

    fs::remove_file(source_root.join("commands").join("c.md")).unwrap();
    let evidence = run_update_commands(&options(repo_root, source_root, home, false)).unwrap();

    assert!(
        !stale.exists(),
        "the removed command's c/ directory still exists"
    );
    assert!(evidence_has_copilot_removal(&evidence, &stale));
}

#[test]
fn prunes_both_copilot_and_shared_skills_directories_when_command_removed() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command(&source_root, "c");

    let targets = vec!["copilot".into(), "codex".into()];
    run_update_commands(&options_with_targets(
        repo_root.clone(),
        source_root.clone(),
        home.clone(),
        targets.clone(),
        false,
    ))
    .unwrap();

    let copilot_c = home.join(".copilot").join("skills").join("c");
    let shared_c = home.join(".agents").join("skills").join("c");
    assert!(copilot_c.is_dir());
    assert!(shared_c.is_dir());

    fs::remove_file(source_root.join("commands").join("c.md")).unwrap();
    let evidence = run_update_commands(&options_with_targets(
        repo_root,
        source_root,
        home,
        targets,
        false,
    ))
    .unwrap();

    assert!(
        !copilot_c.exists(),
        "the removed command's copilot c/ directory still exists"
    );
    assert!(
        !shared_c.exists(),
        "the removed command's shared c/ directory still exists"
    );
    assert!(evidence_has_copilot_removal(&evidence, &copilot_c));
    assert!(evidence_has_common_removal(&evidence, &shared_c));
}

#[test]
fn preexisting_user_skill_dir_without_registry_row_survives() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command(&source_root, "c");

    // A pre-existing user skill directory with no registry row must survive.
    let user_dir = home.join(".agents").join("skills").join("u");
    fs::create_dir_all(&user_dir).unwrap();
    fs::write(user_dir.join("SKILL.md"), "user's own skill\n").unwrap();
    fs::write(user_dir.join("user.txt"), "user payload\n").unwrap();

    let targets = vec!["copilot".into(), "codex".into()];
    run_update_commands(&options_with_targets(
        repo_root.clone(),
        source_root.clone(),
        home.clone(),
        targets.clone(),
        false,
    ))
    .unwrap();

    assert!(user_dir.is_dir());
    assert_eq!(
        fs::read_to_string(user_dir.join("SKILL.md")).unwrap(),
        "user's own skill\n"
    );
    assert!(user_dir.join("user.txt").exists());

    // When command c is removed, user directory u still survives during prune.
    fs::remove_file(source_root.join("commands").join("c.md")).unwrap();
    let evidence = run_update_commands(&options_with_targets(
        repo_root,
        source_root,
        home.clone(),
        targets,
        false,
    ))
    .unwrap();

    let shared_c = home.join(".agents").join("skills").join("c");
    assert!(!shared_c.exists());
    assert!(evidence_has_common_removal(&evidence, &shared_c));

    assert!(
        user_dir.is_dir(),
        "pre-existing user skill directory u/ must survive prune"
    );
    assert_eq!(
        fs::read_to_string(user_dir.join("SKILL.md")).unwrap(),
        "user's own skill\n"
    );
    assert!(user_dir.join("user.txt").exists());
}

#[test]
fn removing_command_and_adding_same_name_skill_in_same_sync_preserves_skill_link() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command(&source_root, "foo");

    let targets = vec!["copilot".into(), "codex".into()];
    run_update_commands(&options_with_targets(
        repo_root.clone(),
        source_root.clone(),
        home.clone(),
        targets.clone(),
        false,
    ))
    .unwrap();

    let shared_foo = home.join(".agents").join("skills").join("foo");
    assert!(shared_foo.is_dir());
    assert!(shared_foo.join("SKILL.md").exists());

    // Second sync: remove command foo, add skill foo in source.
    fs::remove_file(source_root.join("commands").join("foo.md")).unwrap();
    write_skill(&source_root, "foo");

    // update_skills runs first, creating the skill link in shared_skills and marking it.
    run_update_skills(&skill_options(
        repo_root.clone(),
        source_root.clone(),
        home.clone(),
        targets.clone(),
    ))
    .unwrap();

    // Then update_commands runs.
    let evidence = run_update_commands(&options_with_targets(
        repo_root,
        source_root,
        home,
        targets,
        false,
    ))
    .unwrap();

    // shared_foo must survive because it is a skill projection, not a stale command directory.
    assert!(
        shared_foo.exists(),
        "~/.agents/skills/foo skill link must survive when command foo is removed and skill foo added"
    );
    assert!(!evidence_has_common_removal(&evidence, &shared_foo));
}

#[test]
fn full_tp11_acceptance_matrix() {
    let (_temp, repo_root, source_root, home) = fixture_roots();

    // 1. Initial run: command `c` and command `foo` projected to Copilot and Codex (shared skills).
    write_command(&source_root, "c");
    write_command(&source_root, "foo");

    let targets = vec!["copilot".into(), "codex".into()];
    run_update_commands(&options_with_targets(
        repo_root.clone(),
        source_root.clone(),
        home.clone(),
        targets.clone(),
        false,
    ))
    .unwrap();

    let copilot_c = home.join(".copilot").join("skills").join("c");
    let shared_c = home.join(".agents").join("skills").join("c");
    let copilot_foo = home.join(".copilot").join("skills").join("foo");
    let shared_foo = home.join(".agents").join("skills").join("foo");

    assert!(copilot_c.is_dir());
    assert!(shared_c.is_dir());
    assert!(copilot_foo.is_dir());
    assert!(shared_foo.is_dir());

    // 2. Pre-existing user-owned skill directory `u/` with no registry row.
    let user_u = home.join(".agents").join("skills").join("u");
    fs::create_dir_all(&user_u).unwrap();
    fs::write(user_u.join("SKILL.md"), "user custom skill").unwrap();

    // 3. Next sync:
    // - Remove command `c` (leaving command `c` to be pruned on Copilot and shared-skills)
    // - Remove command `foo` and add skill `foo` in the same sync
    fs::remove_file(source_root.join("commands").join("c.md")).unwrap();
    fs::remove_file(source_root.join("commands").join("foo.md")).unwrap();
    write_skill(&source_root, "foo");

    // update_skills runs first:
    run_update_skills(&skill_options(
        repo_root.clone(),
        source_root.clone(),
        home.clone(),
        targets.clone(),
    ))
    .unwrap();

    // update_commands runs second (with 0 active commands remaining):
    let evidence = run_update_commands(&options_with_targets(
        repo_root,
        source_root,
        home,
        targets,
        false,
    ))
    .unwrap();

    // Acceptance checks:
    // - Removing `c` removes Copilot and shared-skills `c/` directories:
    assert!(!copilot_c.exists(), "Copilot c/ directory must be pruned");
    assert!(
        !shared_c.exists(),
        "shared-skills c/ directory must be pruned"
    );
    assert!(evidence_has_copilot_removal(&evidence, &copilot_c));
    assert!(evidence_has_common_removal(&evidence, &shared_c));

    // - Pre-existing ~/.agents/skills/u/ with no registry row survives:
    assert!(user_u.is_dir(), "Pre-existing u/ directory must survive");
    assert_eq!(
        fs::read_to_string(user_u.join("SKILL.md")).unwrap(),
        "user custom skill"
    );

    // - When command foo is removed and skill foo is added in the same sync, ~/.agents/skills/foo survives:
    assert!(
        shared_foo.exists(),
        "~/.agents/skills/foo skill link must survive"
    );

    // - Copilot foo/ (which has no skill counterpart on Copilot) is pruned:
    assert!(
        !copilot_foo.exists(),
        "Copilot foo/ command directory must be pruned"
    );
}

fn record_key(path: &Path) -> String {
    crate::normalize_managed_artifact_path(path)
}

fn lock_category(lockfile: &Path, category: &str) -> Vec<String> {
    let raw = fs::read_to_string(lockfile).unwrap();
    let root: serde_json::Value = serde_json::from_str(&raw).unwrap();
    root["_ccyncProjection"][category]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(|path| record_key(Path::new(path)))
        .collect()
}

fn publish_copilot_skill(
    home: &Path,
    lockfile: &Path,
    alias: &str,
    content: &str,
) -> ProjectionEvidence {
    publish_direct_command_plan(
        &DirectCommandPublication {
            assignments: vec![DirectCommandAssignment {
                target: ProjectionTargetId::Copilot,
                source_id: "ccync-core".into(),
                alias: alias.into(),
                content: content.into(),
                destination: DirectCommandDestination::CommandSkill,
            }],
        },
        home,
        lockfile,
        false,
    )
}

#[test]
fn direct_command_skill_adopts_legacy_skill_record_without_touching_foreign_siblings() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    write_command(&source_root, "c");
    run_update_commands(&options(repo_root, source_root, home.clone(), false)).unwrap();

    let lockfile = home.join(".ccync").join("build").join("lock.json");
    let skill_dir = home.join(".copilot").join("skills").join("c");
    let legacy_skill = skill_dir.join("SKILL.md");
    let foreign = skill_dir.join("notes.md");
    fs::write(&foreign, "user notes\n").unwrap();
    assert!(lock_category(&lockfile, MANAGED_COMMAND_PATHS).contains(&record_key(&legacy_skill)));

    let content = "---\nname: c\ndescription: refreshed\n---\nrefreshed body\n";
    let evidence = publish_copilot_skill(&home, &lockfile, "c", content);

    assert!(evidence.run_warnings.is_empty());
    assert_eq!(fs::read_to_string(&legacy_skill).unwrap(), content);
    assert_eq!(fs::read_to_string(&foreign).unwrap(), "user notes\n");

    // The legacy row moves to the direct skill category, so the removable
    // scope is the SKILL.md file. The directory holding foreign siblings is
    // never recorded.
    assert!(!lock_category(&lockfile, MANAGED_COMMAND_PATHS).contains(&record_key(&legacy_skill)));
    let records = direct_records(&lockfile);
    assert!(records
        .iter()
        .any(|record| record.kind == DirectArtifactKind::Skill
            && record_key(&record.path) == record_key(&legacy_skill)
            && record.source_id.as_deref() == Some("ccync-core")));
    assert!(!records
        .iter()
        .any(|record| record_key(&record.path) == record_key(&skill_dir)));
}

#[test]
fn direct_command_skill_preserves_unrecorded_skill_file_and_its_siblings() {
    let (_temp, _repo_root, _source_root, home) = fixture_roots();
    let lockfile = home.join(".ccync").join("build").join("lock.json");
    let user_dir = home.join(".copilot").join("skills").join("u");
    fs::create_dir_all(&user_dir).unwrap();
    fs::write(user_dir.join("SKILL.md"), "user's own skill\n").unwrap();
    fs::write(user_dir.join("notes.md"), "user notes\n").unwrap();

    let evidence = publish_copilot_skill(&home, &lockfile, "u", "---\nname: u\n---\nccync body\n");

    assert_eq!(
        fs::read_to_string(user_dir.join("SKILL.md")).unwrap(),
        "user's own skill\n"
    );
    assert_eq!(
        fs::read_to_string(user_dir.join("notes.md")).unwrap(),
        "user notes\n"
    );
    assert!(evidence
        .by_target
        .get(&ProjectionTargetId::Copilot)
        .is_some_and(|target| target
            .warnings
            .iter()
            .any(|warning| warning.contains("preserved unowned command destination"))));
    assert!(direct_records(&lockfile).is_empty());
}

#[test]
fn tp06_spec_command_skill_removable_scope_and_legacy_records() {
    let (_temp, repo_root, source_root, home) = fixture_roots();
    let lockfile = home.join(".ccync").join("build").join("lock.json");

    // 1. Removable scope is smallest owned artifact (SKILL.md file, not directory)
    let copilot_skill_dir = home.join(".copilot/skills/cmd-skill");
    let copilot_skill_md = copilot_skill_dir.join("SKILL.md");
    let codex_skill_dir = home.join(".agents/skills/cmd-skill");
    let codex_skill_md = codex_skill_dir.join("SKILL.md");

    let initial_pub = DirectCommandPublication {
        assignments: vec![
            DirectCommandAssignment {
                target: ProjectionTargetId::Copilot,
                source_id: "provider-alpha".into(),
                alias: "cmd-skill".into(),
                content: "---\nname: cmd-skill\n---\nbody v1\n".into(),
                destination: DirectCommandDestination::CommandSkill,
            },
            DirectCommandAssignment {
                target: ProjectionTargetId::Codex,
                source_id: "provider-alpha".into(),
                alias: "cmd-skill".into(),
                content: "---\nname: cmd-skill\n---\nbody v1\n".into(),
                destination: DirectCommandDestination::CommandSkill,
            },
        ],
    };

    let evidence = publish_direct_command_plan(&initial_pub, &home, &lockfile, false);
    assert!(evidence.run_warnings.is_empty());
    assert!(copilot_skill_md.is_file());
    assert!(codex_skill_md.is_file());

    let records = direct_records(&lockfile);
    // Exact file paths recorded as DirectArtifactKind::Skill
    assert!(records.iter().any(|r| r.kind == DirectArtifactKind::Skill
        && record_key(&r.path) == record_key(&copilot_skill_md)
        && r.source_id.as_deref() == Some("provider-alpha")));
    assert!(records.iter().any(|r| r.kind == DirectArtifactKind::Skill
        && record_key(&r.path) == record_key(&codex_skill_md)
        && r.source_id.as_deref() == Some("provider-alpha")));
    // Directory is NEVER recorded as owned artifact
    assert!(!records
        .iter()
        .any(|r| record_key(&r.path) == record_key(&copilot_skill_dir)));
    assert!(!records
        .iter()
        .any(|r| record_key(&r.path) == record_key(&codex_skill_dir)));

    // 2. Foreign siblings in the directory survive untouched across update
    let foreign_sibling_1 = copilot_skill_dir.join("helper.py");
    let foreign_sibling_2 = copilot_skill_dir.join("sub").join("data.txt");
    fs::create_dir_all(foreign_sibling_2.parent().unwrap()).unwrap();
    fs::write(&foreign_sibling_1, "print('foreign')\n").unwrap();
    fs::write(&foreign_sibling_2, "nested foreign data\n").unwrap();

    let update_pub = DirectCommandPublication {
        assignments: vec![DirectCommandAssignment {
            target: ProjectionTargetId::Copilot,
            source_id: "provider-alpha".into(),
            alias: "cmd-skill".into(),
            content: "---\nname: cmd-skill\n---\nbody v2 updated\n".into(),
            destination: DirectCommandDestination::CommandSkill,
        }],
    };
    let update_evidence = publish_direct_command_plan(&update_pub, &home, &lockfile, false);
    assert!(update_evidence.run_warnings.is_empty());

    assert_eq!(
        fs::read_to_string(&copilot_skill_md).unwrap(),
        "---\nname: cmd-skill\n---\nbody v2 updated\n"
    );
    assert_eq!(
        fs::read_to_string(&foreign_sibling_1).unwrap(),
        "print('foreign')\n"
    );
    assert_eq!(
        fs::read_to_string(&foreign_sibling_2).unwrap(),
        "nested foreign data\n"
    );

    // 3. Support legacy SKILL.md records without deleting foreign siblings
    let legacy_cmd_dir = home.join(".copilot/skills/legacy-cmd");
    let legacy_skill_md = legacy_cmd_dir.join("SKILL.md");
    let legacy_foreign = legacy_cmd_dir.join("user_notes.txt");
    write_command(&source_root, "legacy-cmd");
    run_update_commands(&options(repo_root, source_root, home.clone(), false)).unwrap();
    fs::write(&legacy_foreign, "legacy foreign notes\n").unwrap();

    assert!(lock_category(&lockfile, MANAGED_COMMAND_PATHS).contains(&record_key(&legacy_skill_md)));

    // Direct command publication adopts legacy record
    let adopt_pub = DirectCommandPublication {
        assignments: vec![DirectCommandAssignment {
            target: ProjectionTargetId::Copilot,
            source_id: "ccync-core".into(),
            alias: "legacy-cmd".into(),
            content: "---\nname: legacy-cmd\n---\nadopted body\n".into(),
            destination: DirectCommandDestination::CommandSkill,
        }],
    };
    let adopt_evidence = publish_direct_command_plan(&adopt_pub, &home, &lockfile, false);
    assert!(adopt_evidence.run_warnings.is_empty());
    assert_eq!(
        fs::read_to_string(&legacy_skill_md).unwrap(),
        "---\nname: legacy-cmd\n---\nadopted body\n"
    );
    assert_eq!(
        fs::read_to_string(&legacy_foreign).unwrap(),
        "legacy foreign notes\n"
    );

    // Legacy row moved to directSkillProjectionPaths
    assert!(
        !lock_category(&lockfile, MANAGED_COMMAND_PATHS).contains(&record_key(&legacy_skill_md))
    );
    let records = direct_records(&lockfile);
    let legacy_rec = records
        .iter()
        .find(|r| record_key(&r.path) == record_key(&legacy_skill_md))
        .expect("legacy skill row should migrate to direct records");
    assert_eq!(legacy_rec.kind, DirectArtifactKind::Skill);

    // Removing owned record removes authority only over SKILL.md, not foreign siblings
    forget_direct_record(
        &lockfile,
        DirectArtifactKind::Skill,
        &legacy_skill_md,
        false,
    )
    .unwrap();
    let records_after_forget = direct_records(&lockfile);
    assert!(!records_after_forget
        .iter()
        .any(|r| record_key(&r.path) == record_key(&legacy_skill_md)));
    // foreign sibling is untouched
    assert_eq!(
        fs::read_to_string(&legacy_foreign).unwrap(),
        "legacy foreign notes\n"
    );
}
