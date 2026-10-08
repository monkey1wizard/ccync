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
