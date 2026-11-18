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
        state_root: home.join(".ccync").join("build"),
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
        state_root: home.join(".ccync").join("build"),
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

fn prepared_command(
    target: ProjectionTargetId,
    alias: &str,
    content: &str,
    destination: DirectCommandDestination,
) -> DirectCommandAssignment {
    DirectCommandAssignment {
        target,
        source_id: "ccync-core".into(),
        alias: alias.into(),
        content: content.into(),
        destination,
    }
}

fn record_key(path: &Path) -> String {
    crate::normalize_managed_artifact_path(path)
}

#[test]
fn direct_command_plan_publishes_exact_prepared_bytes_for_every_provider() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let lockfile = temp.path().join("lock.json");
    let claude_body = "---\ndescription: deploy\n---\nship release\n";
    let opencode_body = "---\ndescription: deploy\n---\nship release via opencode\n";
    let skill_body = "---\nname: deploy\n---\nship release\n";
    let publication = DirectCommandPublication {
        assignments: vec![
            prepared_command(
                ProjectionTargetId::Claude,
                "deploy",
                claude_body,
                DirectCommandDestination::NativeCommand,
            ),
            prepared_command(
                ProjectionTargetId::Opencode,
                "deploy",
                opencode_body,
                DirectCommandDestination::NativeCommand,
            ),
            prepared_command(
                ProjectionTargetId::Copilot,
                "deploy",
                skill_body,
                DirectCommandDestination::CommandSkill,
            ),
            prepared_command(
                ProjectionTargetId::Codex,
                "deploy",
                skill_body,
                DirectCommandDestination::CommandSkill,
            ),
        ],
    };

    let evidence = publish_direct_command_plan(&publication, &home, &lockfile, false);

    assert!(evidence.run_warnings.is_empty());
    assert!(evidence.by_target.values().all(|t| t.failure.is_none()));
    assert_eq!(
        fs::read_to_string(home.join(".claude/commands/deploy.md")).unwrap(),
        claude_body
    );
    assert_eq!(
        fs::read_to_string(home.join(".config/opencode/commands/deploy.md")).unwrap(),
        opencode_body
    );
    assert_eq!(
        fs::read_to_string(home.join(".copilot/skills/deploy/SKILL.md")).unwrap(),
        skill_body
    );
    assert_eq!(
        fs::read_to_string(home.join(".agents/skills/deploy/SKILL.md")).unwrap(),
        skill_body
    );
    // Nothing outside the plan is written, including commands never assigned.
    assert!(!home.join(".claude/commands/hidden.md").exists());
    assert!(!home.join(".copilot/skills/hidden").exists());
    assert!(!home.join(".ccync/build/render/commands").exists());

    let records = direct_records(&lockfile);
    for (path, kind) in [
        (
            home.join(".claude/commands/deploy.md"),
            DirectArtifactKind::Command,
        ),
        (
            home.join(".config/opencode/commands/deploy.md"),
            DirectArtifactKind::Command,
        ),
        (
            home.join(".copilot/skills/deploy/SKILL.md"),
            DirectArtifactKind::Skill,
        ),
        (
            home.join(".agents/skills/deploy/SKILL.md"),
            DirectArtifactKind::Skill,
        ),
    ] {
        assert!(
            records.iter().any(|record| record.kind == kind
                && record_key(&record.path) == record_key(&path)
                && record.source_id.as_deref() == Some("ccync-core")),
            "missing removable ownership row for {}",
            path.display()
        );
    }

    // A later batch naming one provider keeps every other provider's proof.
    let second = publish_direct_command_plan(
        &DirectCommandPublication {
            assignments: vec![prepared_command(
                ProjectionTargetId::Claude,
                "deploy",
                claude_body,
                DirectCommandDestination::NativeCommand,
            )],
        },
        &home,
        &lockfile,
        false,
    );
    assert!(second.by_target.values().all(|t| t.paths.is_empty()));
    let records = direct_records(&lockfile);
    assert_eq!(records.len(), 4);
    assert!(records.iter().any(|record| record_key(&record.path)
        == record_key(&home.join(".agents/skills/deploy/SKILL.md"))));
}

#[test]
fn direct_command_plan_rejects_unsupported_forms_and_conflicting_shared_paths() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let lockfile = temp.path().join("lock.json");
    let publication = DirectCommandPublication {
        assignments: vec![
            prepared_command(
                ProjectionTargetId::Claude,
                "x",
                "---\ndescription: x\n---\nbody\n",
                DirectCommandDestination::CommandSkill,
            ),
            prepared_command(
                ProjectionTargetId::Copilot,
                "shared",
                "---\nname: shared\n---\nfirst\n",
                DirectCommandDestination::CommandSkill,
            ),
            DirectCommandAssignment {
                source_id: "other-source".into(),
                ..prepared_command(
                    ProjectionTargetId::Copilot,
                    "shared",
                    "---\nname: shared\n---\nsecond\n",
                    DirectCommandDestination::CommandSkill,
                )
            },
        ],
    };

    let evidence = publish_direct_command_plan(&publication, &home, &lockfile, false);

    assert!(!home.join(".claude/commands/x.md").exists());
    assert!(!home.join(".copilot/skills/shared/SKILL.md").exists());
    assert!(evidence
        .by_target
        .get(&ProjectionTargetId::Claude)
        .and_then(|target| target.failure.as_ref())
        .is_some_and(|failure| failure.reason.contains("was not published")));
    assert!(evidence
        .by_target
        .get(&ProjectionTargetId::Copilot)
        .and_then(|target| target.failure.as_ref())
        .is_some_and(|failure| failure.reason.contains("claimed by sources")));
}

#[test]
fn tp05_spec_publish_exact_prepared_native_commands_and_multiprovider_proof() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let lockfile = temp.path().join("lock.json");

    // Multiple providers: provider-alpha and provider-beta
    let alpha_claude_body = "---\ndescription: alpha claude command\n---\nalpha claude execution\n";
    let alpha_opencode_body = "---\ndescription: alpha opencode command\n---\nalpha opencode execution\n";
    let alpha_copilot_body = "---\nname: alpha-cmd\ndescription: alpha copilot skill\n---\nalpha copilot execution\n";
    let beta_claude_body = "---\ndescription: beta claude command\n---\nbeta claude execution\n";
    let beta_codex_body = "---\nname: beta-cmd\ndescription: beta codex skill\n---\nbeta codex execution\n";

    let publication = DirectCommandPublication {
        assignments: vec![
            // provider-alpha assignments
            DirectCommandAssignment {
                target: ProjectionTargetId::Claude,
                source_id: "provider-alpha".into(),
                alias: "alpha-cmd".into(),
                content: alpha_claude_body.into(),
                destination: DirectCommandDestination::NativeCommand,
            },
            DirectCommandAssignment {
                target: ProjectionTargetId::Opencode,
                source_id: "provider-alpha".into(),
                alias: "alpha-cmd".into(),
                content: alpha_opencode_body.into(),
                destination: DirectCommandDestination::NativeCommand,
            },
            DirectCommandAssignment {
                target: ProjectionTargetId::Copilot,
                source_id: "provider-alpha".into(),
                alias: "alpha-cmd".into(),
                content: alpha_copilot_body.into(),
                destination: DirectCommandDestination::CommandSkill,
            },
            // provider-beta assignments
            DirectCommandAssignment {
                target: ProjectionTargetId::Claude,
                source_id: "provider-beta".into(),
                alias: "beta-cmd".into(),
                content: beta_claude_body.into(),
                destination: DirectCommandDestination::NativeCommand,
            },
            DirectCommandAssignment {
                target: ProjectionTargetId::Codex,
                source_id: "provider-beta".into(),
                alias: "beta-cmd".into(),
                content: beta_codex_body.into(),
                destination: DirectCommandDestination::CommandSkill,
            },
        ],
    };

    let evidence = publish_direct_command_plan(&publication, &home, &lockfile, false);
    assert!(evidence.run_warnings.is_empty());
    assert!(evidence.by_target.values().all(|t| t.failure.is_none()));

    // Verify Claude commands exist at exact planned paths with unchanged prepared bytes
    let claude_alpha_path = home.join(".claude/commands/alpha-cmd.md");
    let claude_beta_path = home.join(".claude/commands/beta-cmd.md");
    assert_eq!(fs::read_to_string(&claude_alpha_path).unwrap(), alpha_claude_body);
    assert_eq!(fs::read_to_string(&claude_beta_path).unwrap(), beta_claude_body);

    // Verify OpenCode command exists at exact planned path
    let opencode_alpha_path = home.join(".config/opencode/commands/alpha-cmd.md");
    assert_eq!(fs::read_to_string(&opencode_alpha_path).unwrap(), alpha_opencode_body);

    // Verify command-derived skills exist at exact planned paths
    let copilot_alpha_path = home.join(".copilot/skills/alpha-cmd/SKILL.md");
    let codex_beta_path = home.join(".agents/skills/beta-cmd/SKILL.md");
    assert_eq!(fs::read_to_string(&copilot_alpha_path).unwrap(), alpha_copilot_body);
    assert_eq!(fs::read_to_string(&codex_beta_path).unwrap(), beta_codex_body);

    // Verify excluded commands are absent
    assert!(!home.join(".claude/commands/excluded-cmd.md").exists());
    assert!(!home.join(".config/opencode/commands/excluded-cmd.md").exists());
    assert!(!home.join(".copilot/skills/excluded-cmd").exists());
    assert!(!home.join(".agents/skills/excluded-cmd").exists());

    // Verify multiple providers retain removable ownership in lockfile
    let records = direct_records(&lockfile);
    assert_eq!(records.len(), 5);

    for (path, kind, source) in [
        (&claude_alpha_path, DirectArtifactKind::Command, "provider-alpha"),
        (&opencode_alpha_path, DirectArtifactKind::Command, "provider-alpha"),
        (&copilot_alpha_path, DirectArtifactKind::Skill, "provider-alpha"),
        (&claude_beta_path, DirectArtifactKind::Command, "provider-beta"),
        (&codex_beta_path, DirectArtifactKind::Skill, "provider-beta"),
    ] {
        assert!(
            records.iter().any(|r| r.kind == kind
                && record_key(&r.path) == record_key(path)
                && r.source_id.as_deref() == Some(source)),
            "missing direct record for {} with source {}",
            path.display(),
            source
        );
    }

    // A subsequent publish containing only provider-alpha retains provider-beta's ownership records
    let second_pub = DirectCommandPublication {
        assignments: vec![
            DirectCommandAssignment {
                target: ProjectionTargetId::Claude,
                source_id: "provider-alpha".into(),
                alias: "alpha-cmd".into(),
                content: alpha_claude_body.into(),
                destination: DirectCommandDestination::NativeCommand,
            },
        ],
    };
    let second_evidence = publish_direct_command_plan(&second_pub, &home, &lockfile, false);
    assert!(second_evidence.by_target.values().all(|t| t.failure.is_none()));

    let records_after = direct_records(&lockfile);
    assert_eq!(records_after.len(), 5);
    assert!(records_after.iter().any(|r| r.source_id.as_deref() == Some("provider-beta")
        && record_key(&r.path) == record_key(&claude_beta_path)));
    assert!(records_after.iter().any(|r| r.source_id.as_deref() == Some("provider-beta")
        && record_key(&r.path) == record_key(&codex_beta_path)));
    assert_eq!(fs::read_to_string(&claude_beta_path).unwrap(), beta_claude_body);

    // Foreign unowned file protection:
    // Put an unowned file at .claude/commands/unowned-tool.md
    let foreign_file = home.join(".claude/commands/unowned-tool.md");
    fs::write(&foreign_file, "user manual command\n").unwrap();
    let foreign_claim = DirectCommandPublication {
        assignments: vec![
            DirectCommandAssignment {
                target: ProjectionTargetId::Claude,
                source_id: "provider-gamma".into(),
                alias: "unowned-tool".into(),
                content: "attempt overwrite\n".into(),
                destination: DirectCommandDestination::NativeCommand,
            },
        ],
    };
    let foreign_evidence = publish_direct_command_plan(&foreign_claim, &home, &lockfile, false);
    assert_eq!(fs::read_to_string(&foreign_file).unwrap(), "user manual command\n");
    assert!(foreign_evidence.by_target[&ProjectionTargetId::Claude]
        .warnings
        .iter()
        .any(|w| w.contains("preserved unowned command destination")));

    // Replacement failure preserves prior bytes and prior ownership
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_NONE: u32 = 0;

        let _held = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_NONE)
            .open(&claude_alpha_path)
            .unwrap();

        let failing_pub = DirectCommandPublication {
            assignments: vec![
                DirectCommandAssignment {
                    target: ProjectionTargetId::Claude,
                    source_id: "provider-alpha".into(),
                    alias: "alpha-cmd".into(),
                    content: "corrupted new content\n".into(),
                    destination: DirectCommandDestination::NativeCommand,
                },
            ],
        };
        let fail_evidence = publish_direct_command_plan(&failing_pub, &home, &lockfile, false);
        assert!(fail_evidence.by_target[&ProjectionTargetId::Claude].failure.is_some());
        drop(_held);

        // Previous bytes intact
        assert_eq!(fs::read_to_string(&claude_alpha_path).unwrap(), alpha_claude_body);
        // Ownership record intact
        let records_fail = direct_records(&lockfile);
        assert!(records_fail.iter().any(|r| r.source_id.as_deref() == Some("provider-alpha")
            && record_key(&r.path) == record_key(&claude_alpha_path)));
    }
}

