use super::*;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

fn roots() -> (TempDir, PathBuf, PathBuf, PathBuf) {
    let temp = TempDir::new().unwrap();
    let repo_root = temp.path().join("repo");
    let source_root = repo_root.join("plugins").join("ccync-core");
    let home = temp.path().join("home");
    fs::create_dir_all(source_root.join("commands")).unwrap();
    (temp, repo_root, source_root, home)
}

fn options(
    repo_root: PathBuf,
    source_root: PathBuf,
    home: PathBuf,
    target: &str,
) -> CommandUpdateOptions {
    CommandUpdateOptions {
        repo_root,
        sources: vec![ProjectionSource {
            root: source_root,
            id: "ccync-core".into(),
            persistent: true,
        }],
        user_home: home,
        selected_projection_targets: vec![target.into()],
        dry_run: false,
        replace: true,
    }
}

#[test]
fn flat_command_files_are_sorted_and_only_direct_markdown_files_are_listed() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    fs::write(root.join("z.md"), "z").unwrap();
    fs::write(root.join("a.md"), "a").unwrap();
    fs::write(root.join("ignore.txt"), "ignore").unwrap();
    fs::create_dir(root.join("nested.md")).unwrap();

    let files = list_flat_command_files(root).unwrap();
    assert_eq!(files, vec![root.join("a.md"), root.join("z.md")]);
}

#[test]
fn ensure_name_frontmatter_adds_name_and_is_idempotent() {
    assert_eq!(
        ensure_name_frontmatter("body\n", "sample"),
        "---\nname: sample\n---\nbody\n"
    );
    assert_eq!(
        ensure_name_frontmatter("---\ndescription: sample\n---\nbody\n", "sample"),
        "---\nname: sample\ndescription: sample\n---\nbody\n"
    );
    let with_name = "---\nname: existing\n---\nbody\n";
    assert_eq!(ensure_name_frontmatter(with_name, "sample"), with_name);
}

#[test]
fn flat_command_projects_copilot_and_codex_without_canonical_bake() {
    let (_temp, repo_root, source_root, home) = roots();
    fs::write(
        source_root.join("commands").join("flat.md"),
        "Run {{CCYNC_ROOT}}\n",
    )
    .unwrap();

    run_update_commands(&options(
        repo_root.clone(),
        source_root.clone(),
        home.clone(),
        "copilot",
    ))
    .unwrap();

    let copilot = home.join(".copilot/skills/flat/SKILL.md");
    assert_eq!(
        fs::read_to_string(copilot).unwrap(),
        format!("---\nname: flat\n---\nRun {}\n", repo_root.display())
    );
    assert!(!home
        .join(".ccync/build/render/commands/flat/SKILL.md")
        .exists());

    run_update_commands(&options(repo_root, source_root, home.clone(), "codex")).unwrap();
    assert!(home.join(".agents/skills/flat/SKILL.md").exists());
}

#[test]
fn directory_command_takes_precedence_over_flat_file() {
    let (_temp, repo_root, source_root, home) = roots();
    let command_root = source_root.join("commands");
    fs::write(command_root.join("same.md"), "flat\n").unwrap();
    fs::create_dir(command_root.join("same")).unwrap();
    fs::write(
        command_root.join("same/SKILL.md"),
        "---\ndescription: directory\n---\ndirectory\n",
    )
    .unwrap();

    let report =
        run_update_commands(&options(repo_root, source_root, home.clone(), "copilot")).unwrap();
    assert!(!home
        .join(".copilot/skills/same/SKILL.md")
        .join("noop")
        .exists());
    assert!(home.join(".copilot/skills/same/SKILL.md").exists());
    assert!(report
        .run_warnings
        .iter()
        .any(|warning| warning == "command 'same.md' skipped: directory command takes precedence"));
}

#[test]
fn flat_command_tp10_acceptance_full_matrix() {
    let (_temp, repo_root, source_root, home) = roots();
    fs::write(
        source_root.join("commands").join("c.md"),
        "---\ndescription: c-desc\n---\nRun {{CCYNC_ROOT}}\n",
    )
    .unwrap();

    let opts = CommandUpdateOptions {
        repo_root: repo_root.clone(),
        sources: vec![ProjectionSource {
            root: source_root.clone(),
            id: "ccync-core".into(),
            persistent: true,
        }],
        user_home: home.clone(),
        selected_projection_targets: vec!["copilot".into(), "codex".into(), "opencode".into()],
        dry_run: false,
        replace: true,
    };

    let report = run_update_commands(&opts).unwrap();
    assert!(report.run_warnings.is_empty());

    let copilot_file = home.join(".copilot/skills/c/SKILL.md");
    let codex_file = home.join(".agents/skills/c/SKILL.md");
    let opencode_file = home.join(".config/opencode/commands/c.md");

    let copilot_content_1 = fs::read_to_string(&copilot_file).unwrap();
    let codex_content_1 = fs::read_to_string(&codex_file).unwrap();
    let opencode_content = fs::read_to_string(&opencode_file).unwrap();

    assert!(copilot_content_1.starts_with("---\nname: c\ndescription: c-desc\n---\n"));
    assert!(codex_content_1.starts_with("---\nname: c\ndescription: c-desc\n---\n"));
    assert!(opencode_content.contains(&repo_root.display().to_string()));
    assert!(!home.join(".ccync/build/render/commands/c").exists());

    // Second run leaves both SKILL.md files byte-identical
    let report2 = run_update_commands(&opts).unwrap();
    assert!(report2.run_warnings.is_empty());

    let copilot_content_2 = fs::read_to_string(&copilot_file).unwrap();
    let codex_content_2 = fs::read_to_string(&codex_file).unwrap();

    assert_eq!(copilot_content_1, copilot_content_2);
    assert_eq!(codex_content_1, codex_content_2);
}
