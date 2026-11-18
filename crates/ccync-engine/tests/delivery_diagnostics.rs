//! Diagnostic route evidence shows origin, route, and loader state
//! as separate facts. A published, owned route whose loader did not run must
//! read as NotRun with a repair action, never as loaded and never as a
//! warning. A foreign same-name artifact must show its own path behind an
//! alias.

use ccync_engine::delivery::{
    plan_delivery, ComponentRouteChoice, DeliveryComponent, DeliveryInventory, ForeignComponent,
};
use ccync_engine::doctor::{
    route_evidence_lines, route_findings, route_findings_with_home, Severity,
};
use ccync_engine::managed_view::{
    layout_evidence_lines, route_evidence_line, LayoutEvidence, RouteEvidence, RouteRegistration,
    StorageFaceEvidence, VerificationScope,
};
use ccync_foundation::provider::{ComponentIdentity, ComponentKind, ProviderSourceKey};
use ccync_foundation::target::ProjectionTargetId as Target;

use ccync_engine::delivery::NativeRouteStatus;
use ccync_engine::doctor::{
    native_diagnostics, native_findings, native_section_findings, NativeObservations,
};
use ccync_engine::managed_view::{
    native_diagnostic_line, NativeDiagnostic, NativeIdentity, NativeState,
};
use ccync_engine::plugin_native::{NativeCommandRunner, NativeExecutable, NativeRegistration};
use ccync_foundation::provider::{NativeObservation, NativeSelector};

mod support;

struct LayoutTestHome {
    _home: support::IsolatedHome,
    prior: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl LayoutTestHome {
    fn new() -> Self {
        let (home, _) = support::isolated();
        let values = [
            ("APPDATA", home.root().join("AppData/Roaming")),
            ("LOCALAPPDATA", home.root().join("AppData/Local")),
            ("XDG_CONFIG_HOME", home.root().join("xdg/config")),
            ("XDG_STATE_HOME", home.root().join("xdg/state")),
            ("XDG_CACHE_HOME", home.root().join("xdg/cache")),
        ];
        let prior = values
            .iter()
            .map(|(key, _)| (*key, std::env::var_os(key)))
            .collect();
        for (key, value) in values {
            std::env::set_var(key, value);
        }
        Self { _home: home, prior }
    }

    fn root(&self) -> &std::path::Path {
        self._home.root()
    }
}

impl Drop for LayoutTestHome {
    fn drop(&mut self) {
        for (key, value) in self.prior.drain(..) {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}

fn home_snapshot(root: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    fn visit(
        root: &std::path::Path,
        current: &std::path::Path,
        files: &mut Vec<(std::path::PathBuf, Vec<u8>)>,
    ) {
        for entry in std::fs::read_dir(current).unwrap() {
            let path = entry.unwrap().path();
            let kind = std::fs::symlink_metadata(&path).unwrap().file_type();
            if kind.is_dir() {
                files.push((path.strip_prefix(root).unwrap().to_path_buf(), Vec::new()));
                visit(root, &path, files);
            } else if kind.is_file() {
                files.push((
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(path).unwrap(),
                ));
            }
        }
    }

    let mut files = Vec::new();
    visit(root, root, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

fn identity(source: &str, plugin: &str, kind: ComponentKind, id: &str) -> ComponentIdentity {
    ComponentIdentity::new(
        ProviderSourceKey::new(source).unwrap(),
        plugin,
        None,
        kind,
        id,
    )
    .unwrap()
}

fn component(identity: ComponentIdentity) -> DeliveryComponent {
    DeliveryComponent {
        identity,
        provider_hooks_declared: false,
        enabled: true,
        references_preserved: true,
    }
}

#[test]
fn delivery_diagnostics_selected_route_with_loader_not_run_stays_selected_with_repair_and_no_warning(
) {
    let identity = identity("provider", "provider", ComponentKind::Skill, "review");
    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(identity)],
            foreign_components: vec![],
        },
        &[Target::Claude],
        &[],
    );
    let route = plan
        .routes
        .iter()
        .find(|route| route.target == Target::Claude)
        .unwrap();
    assert!(matches!(
        route.choice,
        ComponentRouteChoice::Direct { .. } | ComponentRouteChoice::ServedByRoot { .. }
    ));

    let evidence =
        RouteEvidence::from_route(route, true, true, true, VerificationScope::LoaderNotRun);
    let line = route_evidence_line(&evidence);

    assert!(line.contains("selected"), "{line}");
    assert!(line.contains("published"), "{line}");
    assert!(line.contains("owned by ccync"), "{line}");
    assert!(line.contains("loader NotRun"), "{line}");
    assert!(!line.contains("loader version"), "{line}");
    assert!(line.contains("the loader check did not run"), "{line}");
    assert!(!line.to_lowercase().contains("warning"), "{line}");
    assert!(!line.contains("loaded"), "{line}");
}

#[test]
fn route_health_reports_storage_authority_and_readers_separately_from_publication_and_loading() {
    let _home = LayoutTestHome::new();
    let secret = "seeded-layout-secret-do-not-print";
    let journal = ccync_foundation::paths::machine_state_root()
        .unwrap()
        .join("migration.json");
    std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
    std::fs::write(
        &journal,
        serde_json::json!({"stage":"prepared","token":secret}).to_string(),
    )
    .unwrap();
    let actual_output = route_evidence_lines().unwrap().join("\n");
    assert!(
        actual_output.contains("migration prepared"),
        "{actual_output}"
    );
    assert!(
        !actual_output.contains(secret),
        "secret leaked into doctor output"
    );
    assert!(
        actual_output.contains("layout face Codex: served ~/.agents/skills"),
        "Codex shared-root reader was not reported: {actual_output}"
    );
    assert!(
        actual_output.contains(
            "layout face Claude: served ~/.claude/skills | incidental none | preserved ~/.agents/skills | fallback unsupported roots: ~/.agents/skills"
        ),
        "unsupported shared-root fallback was not reported: {actual_output}"
    );
    assert!(
        actual_output.contains("layout face CopilotCli:")
            && actual_output.contains("layout face CopilotVsCode:"),
        "Copilot client faces must have separate rows: {actual_output}"
    );

    let evidence = LayoutEvidence {
        authority: ccync_foundation::paths::EffectiveLayoutAuthority::MutationBlocked,
        migration_phase: "prepared-recovery".into(),
        effective_roots: vec![("state".into(), std::path::PathBuf::from("/state"))],
        faces: ["Codex", "CopilotCli", "CopilotVsCode"]
            .into_iter()
            .map(|face| StorageFaceEvidence {
                face: face.into(),
                served: vec![std::path::PathBuf::from(format!("/{face}/active"))],
                incidental: vec![std::path::PathBuf::from(format!("/{face}/candidate"))],
                preserved: vec![std::path::PathBuf::from(format!("/{face}/legacy"))],
                fallback_reason: Some("legacy reader retained during recovery".into()),
                block_reason: Some("layout metadata conflicts".into()),
            })
            .collect(),
        retained_live_surfaces: vec![std::path::PathBuf::from("/home/.claude")],
        historical_row_counts: [("historicalOwnership".into(), 2)].into(),
        safe_retry_actions: vec!["rerun `ccync doctor`".into()],
    };
    let lines = layout_evidence_lines(&evidence);
    let output = lines.join("\n");

    assert!(output.contains("authority MutationBlocked"), "{output}");
    assert!(output.contains("migration prepared-recovery"), "{output}");
    assert!(
        output.contains("historical rows historicalOwnership: 2"),
        "{output}"
    );
    assert!(
        output.contains("safe retry: rerun `ccync doctor`"),
        "{output}"
    );
    for face in ["Codex", "CopilotCli", "CopilotVsCode"] {
        assert!(
            output.contains(&format!("layout face {face}: served")),
            "{output}"
        );
        assert!(output.contains("incidental"), "{output}");
        assert!(output.contains("preserved"), "{output}");
        assert!(output.contains("fallback"), "{output}");
        assert!(
            output.contains("blocked layout metadata conflicts"),
            "{output}"
        );
    }
    let mut committed_evidence = evidence.clone();
    committed_evidence.authority = ccync_foundation::paths::EffectiveLayoutAuthority::Committed;
    committed_evidence.migration_phase = "committed".into();
    for face in &mut committed_evidence.faces {
        face.fallback_reason = None;
        face.block_reason = None;
    }
    let committed_output = layout_evidence_lines(&committed_evidence).join("\n");
    assert!(
        committed_output.contains("authority Committed"),
        "{committed_output}"
    );
    assert!(
        committed_output.contains("migration committed"),
        "{committed_output}"
    );

    let identity = identity("provider", "provider", ComponentKind::Skill, "review");
    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(identity)],
            foreign_components: vec![],
        },
        &[Target::Claude],
        &[],
    );
    let route = &plan.routes[0];
    let route = RouteEvidence::from_route(route, true, true, true, VerificationScope::LoaderNotRun);
    let route_line = route_evidence_line(&route);
    assert!(route_line.contains("published"), "{route_line}");
    assert!(route_line.contains("loader NotRun"), "{route_line}");
    assert!(!route_line.contains("loaded"), "{route_line}");
    println!("route_health blocked recovery: {output}");
    println!("route_health storage commit: {committed_output}");
    println!("route_health route: {route_line}");
    assert!(
        !output.contains(secret),
        "secret leaked into storage evidence"
    );
}

#[test]
fn delivery_diagnostics_filesystem_only_scope_never_reads_as_load_success() {
    let identity = identity("provider", "provider", ComponentKind::Skill, "review");
    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(identity)],
            foreign_components: vec![],
        },
        &[Target::Claude],
        &[],
    );
    let route = &plan.routes[0];
    let evidence =
        RouteEvidence::from_route(route, true, true, true, VerificationScope::FilesystemOnly);
    let line = route_evidence_line(&evidence);

    assert!(line.contains("loader FilesystemOnly"), "{line}");
    assert!(!line.contains("loaded"), "{line}");
}

#[test]
fn delivery_diagnostics_unpublished_route_names_the_write_error_as_its_action() {
    let identity = identity("provider", "provider", ComponentKind::Skill, "review");
    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(identity)],
            foreign_components: vec![],
        },
        &[Target::Claude],
        &[],
    );
    let evidence = RouteEvidence::from_route(
        &plan.routes[0],
        true,
        false,
        false,
        VerificationScope::LoaderNotRun,
    );
    let line = route_evidence_line(&evidence);

    assert!(line.contains("not published"), "{line}");
    assert!(line.contains("not owned by ccync"), "{line}");
    assert!(line.contains("fix the reported write error"), "{line}");
}

#[test]
fn delivery_diagnostics_foreign_same_name_artifact_shows_its_path_behind_the_alias() {
    let provider = identity("provider", "provider", ComponentKind::Skill, "review");
    let foreign_identity = identity(
        "native-foreign",
        "native-foreign",
        ComponentKind::Skill,
        "review",
    );
    let foreign_path = "/home/user/.claude/skills/review".to_string();
    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(provider)],
            foreign_components: vec![ForeignComponent {
                identity: foreign_identity,
                path: foreign_path.clone(),
            }],
        },
        &[Target::Claude],
        &[],
    );
    let route = &plan.routes[0];
    assert_eq!(
        route.foreign_alias_path.as_deref(),
        Some(foreign_path.as_str())
    );

    let evidence =
        RouteEvidence::from_route(route, true, true, true, VerificationScope::LoaderNotRun);
    let line = route_evidence_line(&evidence);

    assert!(
        line.contains(&format!("alias caused by foreign path {foreign_path}")),
        "{line}"
    );
    assert_eq!(evidence.original_name, "review");
}

#[test]
fn doctor_layout_paths_reads_committed_fixture_and_preserves_legacy_home() {
    use ccync_engine::doctor::{run_doctor, DoctorOptions};
    use ccync_foundation::paths::{EffectiveLayout, EffectiveLayoutAuthority, StorageRoots};
    use ccync_foundation::provider::{LayoutRootIdentity, StorageLayoutCommit};

    let home = LayoutTestHome::new();
    let roots = StorageRoots::resolve().unwrap();
    let commit = StorageLayoutCommit {
        layout_version: 1,
        roots: vec![
            ("portable", &roots.portable),
            ("config", &roots.config),
            ("state", &roots.state),
            ("cache", &roots.cache),
        ]
        .into_iter()
        .map(|(name, path)| LayoutRootIdentity {
            name: name.to_string(),
            path: path.clone(),
        })
        .collect(),
    };
    let commit_text = serde_json::to_string(&commit).unwrap();
    std::fs::create_dir_all(&roots.state).unwrap();
    std::fs::create_dir_all(home.root().join(".ccync")).unwrap();
    std::fs::write(roots.state.join("layout.json"), &commit_text).unwrap();
    std::fs::write(home.root().join(".ccync/migration.json"), &commit_text).unwrap();

    let layout = EffectiveLayout::load_or_legacy().unwrap();
    assert_eq!(layout.authority(), EffectiveLayoutAuthority::Committed);
    let committed_lock = layout.build_lock_path();
    std::fs::create_dir_all(committed_lock.parent().unwrap()).unwrap();
    let committed_fixture = br#"{"_adoptMaster":"codex"}"#;
    std::fs::write(&committed_lock, committed_fixture).unwrap();

    let legacy_lock = home.root().join(".ccync/build/lock.json");
    std::fs::create_dir_all(legacy_lock.parent().unwrap()).unwrap();
    let legacy_fixture = br#"{}"#;
    std::fs::write(&legacy_lock, legacy_fixture).unwrap();
    let legacy_before = std::fs::read(&legacy_lock).unwrap();
    let before = home_snapshot(home.root());

    let report = run_doctor(&DoctorOptions::default());

    assert!(
        !report
            .findings
            .iter()
            .any(|finding| finding.message.contains("no provider state recorded")),
        "doctor must read the committed lock through the selected layout: {:?}",
        report.findings
    );
    assert_eq!(std::fs::read(&legacy_lock).unwrap(), legacy_before);
    assert_eq!(legacy_before, legacy_fixture);
    assert_eq!(
        home_snapshot(home.root()),
        before,
        "doctor must not mutate the home"
    );
}

#[test]
fn delivery_diagnostics_user_registration_reports_its_uninstall_command() {
    let identity = identity("provider", "provider", ComponentKind::Skill, "review");
    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(identity)],
            foreign_components: vec![],
        },
        &[Target::Claude],
        &[],
    );
    let mut evidence = RouteEvidence::from_route(
        &plan.routes[0],
        true,
        true,
        true,
        VerificationScope::LoaderNotRun,
    );
    evidence.registration = Some(RouteRegistration::RegisteredByUser {
        uninstall_command: "claude plugin uninstall review".to_string(),
    });
    let line = route_evidence_line(&evidence);

    assert!(
        line.contains("registered by user, uninstall with `claude plugin uninstall review`"),
        "{line}"
    );
    assert!(line.contains("remove the user registration"), "{line}");
}

#[test]
fn delivery_diagnostics_route_shows_origin_route_and_original_name() {
    use ccync_foundation::provider::ComponentOrigin;

    let mut provider = identity("provider", "provider", ComponentKind::Skill, "review");
    let origin_path = std::env::temp_dir().join("provider-origin");
    provider.origin = Some(ComponentOrigin {
        structure_version: 1,
        canonical_reference: Some("~/.ccync/plugins/provider".to_string()),
        resolved_path: Some(origin_path.clone()),
        revision: None,
        digest: None,
        enabled: Some(true),
    });
    let foreign = identity(
        "native-foreign",
        "native-foreign",
        ComponentKind::Skill,
        "review",
    );
    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(provider)],
            foreign_components: vec![ForeignComponent {
                identity: foreign,
                path: "/home/user/.claude/skills/review".to_string(),
            }],
        },
        &[Target::Claude],
        &[],
    );
    let evidence = RouteEvidence::from_route(
        &plan.routes[0],
        true,
        true,
        true,
        VerificationScope::LoaderNotRun,
    );
    let line = route_evidence_line(&evidence);

    assert_eq!(evidence.source_key, "provider");
    assert_eq!(evidence.original_name, "review");
    assert!(
        line.contains(&format!("origin provider at {}", origin_path.display())),
        "{line}"
    );
    assert!(line.contains("route "), "{line}");
    assert!(line.starts_with("claude Skill review;"), "{line}");
}

#[test]
fn delivery_diagnostics_route_record_round_trips_every_fact() {
    use ccync_engine::managed_view::{read_route_record, write_route_record};

    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(identity(
                "provider",
                "provider",
                ComponentKind::Command,
                "build",
            ))],
            foreign_components: vec![],
        },
        &[Target::Codex],
        &[],
    );
    let mut evidence = RouteEvidence::from_route(
        &plan.routes[0],
        true,
        false,
        false,
        VerificationScope::LoaderNotRun,
    );
    evidence.failure_reason = Some("disk full".to_string());
    evidence.registration = Some(RouteRegistration::RegisteredByUser {
        uninstall_command: "claude plugin uninstall build".to_string(),
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("build").join("routes.json");

    assert_eq!(read_route_record(&path), Ok(None));
    write_route_record(&path, std::slice::from_ref(&evidence)).unwrap();
    assert_eq!(read_route_record(&path), Ok(Some(vec![evidence])));

    std::fs::write(&path, "{not json").unwrap();
    assert!(read_route_record(&path).is_err());
}

#[test]
fn delivery_diagnostics_unsupported_route_names_its_reason_without_write_error_advice() {
    use ccync_engine::managed_view::{read_route_record, write_route_record};

    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(identity(
                "provider",
                "provider",
                ComponentKind::Command,
                "build",
            ))],
            foreign_components: vec![],
        },
        &[Target::AgyCli],
        &[],
    );
    let unsupported = RouteEvidence::from_route(
        &plan.routes[0],
        true,
        false,
        false,
        VerificationScope::LoaderNotRun,
    );
    let line = route_evidence_line(&unsupported);

    assert!(
        line.contains(
            "not published because unsupported: no command-to-skill surface on this target"
        ),
        "{line}"
    );
    assert!(
        line.contains("action: no write is needed for this unsupported route"),
        "{line}"
    );
    assert!(!line.contains("write error"), "{line}");

    let mut failed = direct_route_evidence(true, false, false);
    failed.failure_reason = Some("disk full".to_string());
    let failed_line = route_evidence_line(&failed);

    assert!(
        failed_line.contains("action: fix the reported write error (disk full)"),
        "{failed_line}"
    );
    assert!(!failed_line.contains("unsupported"), "{failed_line}");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("build").join("routes.json");
    write_route_record(&path, std::slice::from_ref(&unsupported)).unwrap();
    assert_eq!(
        read_route_record(&path),
        Ok(Some(vec![unsupported.clone()]))
    );

    // A record written before the field existed has no key and still parses.
    let mut document: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    for row in document["routes"].as_array_mut().unwrap() {
        row.as_object_mut().unwrap().remove("unsupportedReason");
    }
    std::fs::write(&path, serde_json::to_string(&document).unwrap()).unwrap();
    let legacy = read_route_record(&path).unwrap().unwrap();
    assert_eq!(legacy.len(), 1);
    assert_eq!(legacy[0].unsupported_reason, None);
    assert_eq!(legacy[0].original_name, "build");
}

#[test]
fn tp27_spec_unsupported_route_and_failed_direct_route_diagnostics_and_roundtrip() {
    use ccync_engine::managed_view::{read_route_record, write_route_record};

    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(identity(
                "provider",
                "provider",
                ComponentKind::Command,
                "deploy",
            ))],
            foreign_components: vec![],
        },
        &[Target::AgyCli],
        &[],
    );
    assert_eq!(plan.routes.len(), 1);
    let unsupported_evidence = RouteEvidence::from_route(
        &plan.routes[0],
        true,
        false,
        false,
        VerificationScope::LoaderNotRun,
    );
    assert_eq!(
        unsupported_evidence.unsupported_reason.as_deref(),
        Some("no command-to-skill surface on this target")
    );
    assert_eq!(
        unsupported_evidence.repair_action(),
        "no write is needed for this unsupported route"
    );
    let unsupported_line = route_evidence_line(&unsupported_evidence);
    assert!(
        unsupported_line.contains(
            "not published because unsupported: no command-to-skill surface on this target"
        ),
        "expected unsupported reason in line: {unsupported_line}"
    );
    assert!(
        unsupported_line.contains("action: no write is needed for this unsupported route"),
        "expected no-write repair action in line: {unsupported_line}"
    );
    assert!(
        !unsupported_line.contains("write error"),
        "unsupported route must not contain write-error advice: {unsupported_line}"
    );

    let mut failed_direct = direct_route_evidence(true, false, false);
    failed_direct.failure_reason = Some("permission denied".to_string());
    assert_eq!(
        failed_direct.repair_action(),
        "fix the reported write error (permission denied), then rerun `ccync sync`"
    );
    let failed_line = route_evidence_line(&failed_direct);
    assert!(
        failed_line.contains("action: fix the reported write error (permission denied)"),
        "failed direct route must carry write-error advice: {failed_line}"
    );
    assert!(
        !failed_line.contains("unsupported"),
        "failed direct route must not mention unsupported: {failed_line}"
    );

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("build").join("routes.json");
    write_route_record(&path, std::slice::from_ref(&unsupported_evidence)).unwrap();
    let loaded = read_route_record(&path).unwrap().unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(
        loaded[0].unsupported_reason,
        unsupported_evidence.unsupported_reason
    );
    assert_eq!(loaded[0].original_name, "deploy");
}

#[test]
fn delivery_diagnostics_listed_but_not_registered_route_is_labelled_as_such() {
    let identity = identity("provider", "provider", ComponentKind::Skill, "review");
    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(identity)],
            foreign_components: vec![],
        },
        &[Target::Claude],
        &[],
    );
    let mut evidence = RouteEvidence::from_route(
        &plan.routes[0],
        true,
        true,
        true,
        VerificationScope::LoaderNotRun,
    );
    evidence.registration = Some(RouteRegistration::ListedNotRegistered);
    let line = route_evidence_line(&evidence);

    assert!(line.contains("listed, not registered by ccync"), "{line}");
}

fn direct_route_evidence(selected: bool, published: bool, owned: bool) -> RouteEvidence {
    let identity = identity("provider", "provider", ComponentKind::Skill, "review");
    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(identity)],
            foreign_components: vec![],
        },
        &[Target::Claude],
        &[],
    );
    RouteEvidence::from_route(
        &plan.routes[0],
        selected,
        published,
        owned,
        VerificationScope::LoaderNotRun,
    )
}

#[test]
fn doctor_healthy_direct_route_has_no_finding_and_loader_not_run_stays_out_of_findings() {
    let evidence = direct_route_evidence(true, true, true);

    assert!(route_findings(std::slice::from_ref(&evidence)).is_empty());
    assert!(route_evidence_line(&evidence).contains("loader NotRun"));
}

#[test]
fn doctor_missing_required_output_is_a_specific_error_with_repair_action() {
    let evidence = direct_route_evidence(true, false, false);
    let path = evidence
        .path
        .clone()
        .expect("direct route has an output path");

    let findings = route_findings(&[evidence]);

    assert_eq!(findings.len(), 1, "{findings:?}");
    let finding = &findings[0];
    assert_eq!(finding.severity, Severity::Error, "{finding:?}");
    assert!(finding.message.contains("is not published"), "{finding:?}");
    assert!(finding.message.contains(&path), "{finding:?}");
    assert!(
        finding.message.contains("from source provider"),
        "{finding:?}"
    );
    let hint = finding.fix_hint.as_deref().unwrap_or_default();
    assert!(hint.contains("rerun `ccync sync`"), "{finding:?}");
}

#[test]
fn doctor_unowned_required_output_is_a_specific_error() {
    let evidence = direct_route_evidence(true, true, false);

    let findings = route_findings(&[evidence]);

    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].severity, Severity::Error, "{:?}", findings[0]);
    assert!(
        findings[0].message.contains("is not owned by ccync"),
        "{:?}",
        findings[0]
    );
}

#[test]
fn doctor_unselected_or_outputless_route_is_not_a_required_output() {
    assert!(route_findings(&[direct_route_evidence(false, false, false)]).is_empty());

    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![DeliveryComponent {
                identity: identity("provider", "provider", ComponentKind::Skill, "review"),
                provider_hooks_declared: false,
                enabled: false,
                references_preserved: true,
            }],
            foreign_components: vec![],
        },
        &[Target::Claude],
        &[],
    );
    let evidence = RouteEvidence::from_route(
        &plan.routes[0],
        true,
        false,
        false,
        VerificationScope::LoaderNotRun,
    );
    assert_eq!(evidence.path, None);
    assert!(route_findings(&[evidence]).is_empty());
}

#[test]
fn tp16_spec_compare_healthy_direct_routes_against_missing_output_and_missing_proof() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let ccync_build = home.join(".ccync").join("build");
    std::fs::create_dir_all(&ccync_build).unwrap();
    let lock_path = ccync_build.join("lock.json");

    let skill_dir = home.join(".claude").join("skills").join("review");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let skill_file = skill_dir.join("SKILL.md");
    std::fs::write(&skill_file, "# Review skill").unwrap();

    let normalized_skill_dir = projection::normalize_managed_artifact_path(&skill_dir);
    let lock_json = serde_json::json!({
        "_ccyncProjection": {
            "directSkillProjectionPaths": [normalized_skill_dir]
        }
    });
    std::fs::write(
        &lock_path,
        serde_json::to_string_pretty(&lock_json).unwrap(),
    )
    .unwrap();

    let identity = identity("provider", "provider", ComponentKind::Skill, "review");
    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(identity)],
            foreign_components: vec![],
        },
        &[Target::Claude],
        &[],
    );
    let initial_evidence = RouteEvidence::from_route(
        &plan.routes[0],
        true,
        true,
        true,
        VerificationScope::LoaderNotRun,
    );

    // 1. Healthy direct route: published and owned
    let verified_healthy = ccync_engine::delivery_apply::verify_recorded_routes(
        vec![initial_evidence.clone()],
        &home,
        &lock_path,
    );
    assert_eq!(verified_healthy.len(), 1);
    assert!(verified_healthy[0].published);
    assert!(verified_healthy[0].owned);

    let healthy_findings = route_findings(&verified_healthy);
    assert!(
        healthy_findings.is_empty(),
        "healthy direct route must have no findings: {healthy_findings:?}"
    );
    assert!(
        !healthy_findings
            .iter()
            .any(|f| f.message.contains("skills/ccync") || f.message.contains("retired")),
        "healthy direct fixture must have no retired-link finding"
    );

    // 2. Missing output: delete the output path
    std::fs::remove_file(&skill_file).unwrap();
    std::fs::remove_dir(&skill_dir).unwrap();

    let verified_missing = ccync_engine::delivery_apply::verify_recorded_routes(
        vec![initial_evidence.clone()],
        &home,
        &lock_path,
    );
    assert_eq!(verified_missing.len(), 1);
    assert!(
        !verified_missing[0].published,
        "missing output must be marked as not published"
    );

    let missing_findings = route_findings(&verified_missing);
    assert_eq!(
        missing_findings.len(),
        1,
        "missing output must yield exactly 1 finding: {missing_findings:?}"
    );
    assert_eq!(missing_findings[0].severity, Severity::Error);
    assert!(
        missing_findings[0].message.contains("is not published"),
        "{:?}",
        missing_findings[0]
    );
    assert!(
        missing_findings[0]
            .fix_hint
            .as_deref()
            .unwrap_or_default()
            .contains("rerun `ccync sync`"),
        "{:?}",
        missing_findings[0]
    );

    // 3. Missing proof: restore file on disk, but clear ownership from lockfile
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(&skill_file, "# Review skill").unwrap();
    std::fs::write(&lock_path, "{}").unwrap();

    let verified_unowned = ccync_engine::delivery_apply::verify_recorded_routes(
        vec![initial_evidence.clone()],
        &home,
        &lock_path,
    );
    assert_eq!(verified_unowned.len(), 1);
    assert!(
        verified_unowned[0].published,
        "restored file must be marked as published"
    );
    assert!(
        !verified_unowned[0].owned,
        "output without lockfile proof must be marked as unowned"
    );

    let unowned_findings = route_findings(&verified_unowned);
    assert_eq!(
        unowned_findings.len(),
        1,
        "unowned output must yield exactly 1 finding: {unowned_findings:?}"
    );
    assert_eq!(unowned_findings[0].severity, Severity::Error);
    assert!(
        unowned_findings[0]
            .message
            .contains("is not owned by ccync"),
        "{:?}",
        unowned_findings[0]
    );
    assert!(
        unowned_findings[0]
            .fix_hint
            .as_deref()
            .unwrap_or_default()
            .contains("move or remove the unowned path"),
        "{:?}",
        unowned_findings[0]
    );
}

/// Routes for the given Copilot skills, all published or all unpublished.
fn copilot_skill_routes(skill_ids: &[&str], published: bool) -> Vec<RouteEvidence> {
    let components = skill_ids
        .iter()
        .map(|id| component(identity("provider", "provider", ComponentKind::Skill, id)))
        .collect();
    let plan = plan_delivery(
        &DeliveryInventory {
            components,
            foreign_components: vec![],
        },
        &[Target::Copilot],
        &[],
    );
    plan.routes
        .iter()
        .map(|route| {
            RouteEvidence::from_route(
                route,
                true,
                published,
                published,
                VerificationScope::LoaderNotRun,
            )
        })
        .collect()
}

/// The Copilot combined copy that stays as the fallback under `home`.
fn copilot_fallback(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".copilot").join("skills").join("ccync")
}

fn fallback_findings(findings: &[ccync_foundation::health::DoctorFinding]) -> Vec<&str> {
    findings
        .iter()
        .map(|finding| finding.message.as_str())
        .filter(|message| message.contains("combined fallback"))
        .collect()
}

#[test]
fn doctor_unpublished_copilot_route_checks_its_fallback_and_reports_its_absence() {
    let home = tempfile::tempdir().unwrap();
    let routes = copilot_skill_routes(&["review"], false);
    assert!(routes[0].path.is_some(), "{:?}", routes[0]);
    assert_eq!(routes[0].unsupported_reason, None);

    std::fs::create_dir_all(copilot_fallback(home.path())).unwrap();
    let with_fallback = route_findings_with_home(&routes, Some(home.path()));
    assert_eq!(with_fallback.len(), 1, "{with_fallback:?}");
    assert!(
        with_fallback[0].message.contains("is not published"),
        "{with_fallback:?}"
    );
    assert!(fallback_findings(&with_fallback).is_empty());

    std::fs::remove_dir_all(copilot_fallback(home.path())).unwrap();
    let without_fallback = route_findings_with_home(&routes, Some(home.path()));
    let fallback = fallback_findings(&without_fallback);
    assert_eq!(fallback.len(), 1, "{without_fallback:?}");
    assert!(
        fallback[0].contains(&Target::Copilot.to_string()),
        "{fallback:?}"
    );
    assert!(
        fallback[0].contains(&copilot_fallback(home.path()).display().to_string()),
        "{fallback:?}"
    );
}

#[test]
fn doctor_missing_copilot_fallback_is_reported_once_per_target() {
    let home = tempfile::tempdir().unwrap();
    let routes = copilot_skill_routes(&["review", "deploy"], false);

    let findings = route_findings_with_home(&routes, Some(home.path()));

    assert_eq!(findings.len(), 3, "{findings:?}");
    assert_eq!(fallback_findings(&findings).len(), 1, "{findings:?}");
}

#[test]
fn doctor_fully_published_route_never_checks_its_fallback() {
    let home = tempfile::tempdir().unwrap();
    let routes = copilot_skill_routes(&["review"], true);

    let findings = route_findings_with_home(&routes, Some(home.path()));

    assert!(findings.is_empty(), "{findings:?}");
}

#[test]
fn tp31_spec_doctor_retained_combined_fallback_verification() {
    let home = tempfile::tempdir().unwrap();

    // 1. Fully published route: never checks fallback even if fallback directory does not exist
    let published_routes = copilot_skill_routes(&["review"], true);
    let published_findings = route_findings_with_home(&published_routes, Some(home.path()));
    assert!(
        published_findings.is_empty(),
        "fully published routes should produce no findings: {published_findings:?}"
    );
    assert!(
        fallback_findings(&published_findings).is_empty(),
        "fully published routes must never check fallback"
    );

    // 2. Unsupported route: does not require a fallback
    let unsupported_plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(identity(
                "provider",
                "provider",
                ComponentKind::Command,
                "deploy",
            ))],
            foreign_components: vec![],
        },
        &[Target::AgyCli],
        &[],
    );
    let unsupported_evidence = vec![RouteEvidence::from_route(
        &unsupported_plan.routes[0],
        true,
        false,
        false,
        VerificationScope::LoaderNotRun,
    )];
    let unsupported_findings = route_findings_with_home(&unsupported_evidence, Some(home.path()));
    assert!(
        fallback_findings(&unsupported_findings).is_empty(),
        "unsupported routes must not require a fallback: {unsupported_findings:?}"
    );

    // 3. Unpublished route with fallback present: no fallback finding
    let unpublished_routes = copilot_skill_routes(&["review", "deploy"], false);
    std::fs::create_dir_all(copilot_fallback(home.path())).unwrap();
    let with_fallback = route_findings_with_home(&unpublished_routes, Some(home.path()));
    assert_eq!(
        fallback_findings(&with_fallback).len(),
        0,
        "present fallback must produce no fallback finding"
    );

    // 4. Deleted fallback: exactly 1 fallback finding naming target and path
    std::fs::remove_dir_all(copilot_fallback(home.path())).unwrap();
    let without_fallback = route_findings_with_home(&unpublished_routes, Some(home.path()));
    let missing_fallbacks = fallback_findings(&without_fallback);
    assert_eq!(
        missing_fallbacks.len(),
        1,
        "missing fallback must be reported once per target: {without_fallback:?}"
    );
    assert!(
        missing_fallbacks[0].contains(&Target::Copilot.to_string()),
        "finding must name target: {}",
        missing_fallbacks[0]
    );
    assert!(
        missing_fallbacks[0].contains(&copilot_fallback(home.path()).display().to_string()),
        "finding must name fallback path: {}",
        missing_fallbacks[0]
    );
}

/// Selector and digest of the committed Codex record used by these fixtures.
const T13_SELECTOR: &str = "probe@ccync-marketplace";
const T13_DIGEST: &str = "sha256:abc";

/// What a fake client reports for the committed selector. Its `observe`
/// returns this value; its `run` counts any mutating call.
#[derive(Clone, Copy)]
struct Reported {
    available: bool,
    present: bool,
    enabled: bool,
    broken: bool,
    selector: &'static str,
    digest: Option<&'static str>,
}

impl Reported {
    /// A healthy, enabled registration that matches the committed record.
    fn registered() -> Self {
        Self {
            available: true,
            present: true,
            enabled: true,
            broken: false,
            selector: T13_SELECTOR,
            digest: Some(T13_DIGEST),
        }
    }
}

struct ReadOnlyRunner {
    reported: Reported,
    observes: std::cell::Cell<usize>,
    runs: std::cell::Cell<usize>,
}

impl NativeCommandRunner for ReadOnlyRunner {
    fn run(&self, _executable: NativeExecutable, _args: &[&str]) -> Result<Vec<u8>, String> {
        self.runs.set(self.runs.get() + 1);
        Err("read-only diagnostics must not run a native CLI call".to_string())
    }

    fn observe(
        &self,
        _executable: NativeExecutable,
        _selector: &NativeSelector,
    ) -> Result<NativeRegistration, String> {
        self.observes.set(self.observes.get() + 1);
        let reported = self.reported;
        Ok(NativeRegistration {
            available: reported.available,
            present: reported.present,
            enabled: reported.enabled,
            broken: reported.broken,
            selector: NativeSelector::new(reported.selector).ok(),
            source: None,
            upstream_revision: None,
            package_digest: reported.digest.map(str::to_string),
            capabilities: Default::default(),
        })
    }

    fn marketplace_root(
        &self,
        _executable: NativeExecutable,
        _marketplace: &str,
    ) -> Result<Option<String>, String> {
        Ok(None)
    }
}

/// A lock with one committed Codex record, shaped as `ccync sync` writes it.
fn t13_lock() -> serde_json::Value {
    serde_json::json!({"_providerLifecycle": {"schemaVersion": 1, "committed": [{
        "target": "codex",
        "selector": T13_SELECTOR,
        "pluginName": "probe-provider",
        "upstreamRevision": "rev-1",
        "generation": {"source": "probe-source", "generation": "gen-1", "packageDigest": T13_DIGEST},
        "capabilities": ["skill"]
    }]}})
}

/// Diagnose the committed record against one reported native state. Returns
/// the diagnostics with the number of observations and of mutating calls.
fn t13_diagnose(reported: Reported) -> (Vec<NativeDiagnostic>, usize, usize) {
    let runner = ReadOnlyRunner {
        reported,
        observes: std::cell::Cell::new(0),
        runs: std::cell::Cell::new(0),
    };
    let diagnostics = native_diagnostics(&runner, &t13_lock());
    (diagnostics, runner.observes.get(), runner.runs.get())
}

fn t13_identity() -> NativeIdentity {
    NativeIdentity {
        package: "probe-provider".to_string(),
        management_id: Some("probe-source".to_string()),
        component: T13_SELECTOR.to_string(),
    }
}

#[test]
fn p7_t13_registered_disabled_foreign_drift_and_unavailable_are_distinct_read_only_diagnostics() {
    let registered = Reported::registered();
    let cases = [
        ("registered", registered),
        (
            "disabled",
            Reported {
                enabled: false,
                ..registered
            },
        ),
        (
            "foreign",
            Reported {
                selector: "someone-else@other-market",
                ..registered
            },
        ),
        (
            "drift",
            Reported {
                digest: Some("sha256:changed"),
                ..registered
            },
        ),
        (
            "unavailable",
            Reported {
                available: false,
                ..registered
            },
        ),
    ];

    let mut states = Vec::new();
    for (name, reported) in cases {
        let (diagnostics, observes, runs) = t13_diagnose(reported);
        assert_eq!(diagnostics.len(), 1, "{name} yields one diagnostic");
        assert_eq!(observes, 1, "{name} observes once, read-only");
        assert_eq!(runs, 0, "{name} never runs a native CLI call");
        let line = native_diagnostic_line(&diagnostics[0]);
        assert!(line.contains("route native"), "{name}: {line}");
        assert!(
            !diagnostics[0].is_full_success(),
            "{name} is never full success"
        );
        states.push(diagnostics[0].state);
    }

    assert_eq!(
        states,
        vec![
            NativeState::RegisteredNotLoaded,
            NativeState::Disabled,
            NativeState::ForeignRegistration,
            NativeState::Drifted,
            NativeState::Unavailable,
        ],
        "each reported state maps to its own diagnostic"
    );

    let (absent, _, _) = t13_diagnose(Reported {
        present: false,
        ..registered
    });
    assert_eq!(
        absent[0].state,
        NativeState::Absent,
        "absent is not disabled"
    );
}

#[test]
fn p7_t13_findings_block_drift_and_foreign_and_never_flag_registration_or_disable() {
    let registered = Reported::registered();
    let finding_for = |reported: Reported| {
        let (diagnostics, _, _) = t13_diagnose(reported);
        native_findings(&diagnostics)
    };

    assert!(
        finding_for(registered).is_empty(),
        "a registered route is NotRun, not a finding"
    );
    assert!(
        finding_for(Reported {
            enabled: false,
            ..registered
        })
        .is_empty(),
        "a deliberate disable is not a finding"
    );

    let drift = finding_for(Reported {
        digest: Some("sha256:changed"),
        ..registered
    });
    assert_eq!(drift.len(), 1);
    assert_eq!(drift[0].severity, Severity::Error);
    assert!(
        drift[0].message.contains("no longer matches"),
        "{:?}",
        drift[0]
    );

    let foreign = finding_for(Reported {
        selector: "someone-else@other-market",
        ..registered
    });
    assert_eq!(foreign[0].severity, Severity::Error);

    let unavailable = finding_for(Reported {
        available: false,
        ..registered
    });
    assert_eq!(unavailable[0].severity, Severity::Warning);
    assert!(
        unavailable[0].message.contains("NotRun"),
        "{:?}",
        unavailable[0]
    );
}

#[test]
fn p7_t13_fallback_route_reports_its_reason_and_never_merges_into_success() {
    let native = NativeDiagnostic::from_observation(
        Target::Codex,
        t13_identity(),
        NativeRouteStatus::Native,
        NativeObservation::Desired,
        Some(true),
        Some(T13_DIGEST.to_string()),
    );
    let fallback = NativeDiagnostic::from_observation(
        Target::Codex,
        t13_identity(),
        NativeRouteStatus::Fallback {
            reason: "unverified adapter version 9.9".to_string(),
        },
        NativeObservation::Desired,
        Some(true),
        Some(T13_DIGEST.to_string()),
    );

    let line = native_diagnostic_line(&fallback);
    assert!(
        line.contains("route fallback, loose route active: unverified adapter version 9.9"),
        "{line}"
    );

    for merged in [native.merge(&fallback), fallback.merge(&native)] {
        assert!(
            matches!(merged.route, NativeRouteStatus::Fallback { .. }),
            "a fallback survives the merge: {merged:?}"
        );
        assert!(!merged.is_full_success());
    }
}

#[test]
fn p7_t13_failures_survive_every_evidence_merge_order() {
    let observe = |observation: NativeObservation, enabled: Option<bool>| {
        NativeDiagnostic::from_observation(
            Target::Codex,
            t13_identity(),
            NativeRouteStatus::Native,
            observation,
            enabled,
            None,
        )
    };
    let registered = observe(NativeObservation::Desired, Some(true));
    let disabled = observe(NativeObservation::Disabled, Some(false));
    let drifted = observe(NativeObservation::Drifted, Some(true));
    let foreign = observe(NativeObservation::External, Some(true));

    for merged in [registered.merge(&drifted), drifted.merge(&registered)] {
        assert_eq!(merged.state, NativeState::Drifted, "drift survives");
        assert!(merged.reason.contains("no longer matches"), "{merged:?}");
    }
    for merged in [disabled.merge(&foreign), foreign.merge(&disabled)] {
        assert_eq!(merged.state, NativeState::ForeignRegistration);
    }

    let merged_all = [&disabled, &drifted, &foreign]
        .into_iter()
        .fold(registered.clone(), |acc, next| acc.merge(next));
    assert_eq!(merged_all.state, NativeState::ForeignRegistration);
    assert!(!merged_all.is_full_success());

    assert_eq!(
        registered.merge(&disabled).enabled,
        None,
        "disagreeing enabled states become unknown"
    );
}

#[test]
fn p7_t13_native_diagnostic_line_names_identity_route_enabled_state_and_generation() {
    let (diagnostics, _, _) = t13_diagnose(Reported::registered());
    let line = native_diagnostic_line(&diagnostics[0]);
    for expected in [
        "codex package probe-provider",
        "(management ID probe-source)",
        "component probe@ccync-marketplace",
        "route native",
        "; enabled;",
        "generation sha256:abc",
    ] {
        assert!(line.contains(expected), "{expected} missing from: {line}");
    }

    let (disabled, _, _) = t13_diagnose(Reported {
        enabled: false,
        ..Reported::registered()
    });
    let disabled_line = native_diagnostic_line(&disabled[0]);
    assert!(disabled_line.contains("; disabled;"), "{disabled_line}");
    assert!(
        disabled_line.contains("generation sha256:abc"),
        "{disabled_line}"
    );
}

#[test]
fn p7_t13_unparseable_or_legacy_lifecycle_section_warns_and_never_reports_full_success() {
    let unreadable_shapes = [
        serde_json::json!({"_providerLifecycle": "not-an-object"}),
        serde_json::json!({"_providerLifecycle": {"schemaVersion": 999, "committed": []}}),
        serde_json::json!({"_providerLifecycle": {"schemaVersion": 1, "committed": "legacy"}}),
    ];
    for lock in &unreadable_shapes {
        let findings = native_section_findings(lock);
        assert_eq!(findings.len(), 1, "{lock} yields one warning");
        assert_eq!(findings[0].severity, Severity::Warning, "{lock}");
        assert!(
            findings[0]
                .message
                .contains("native route state is unknown"),
            "{}",
            findings[0].message
        );
        assert!(
            findings[0].message.contains("never full success"),
            "{}",
            findings[0].message
        );

        let runner = ReadOnlyRunner {
            reported: Reported::registered(),
            observes: std::cell::Cell::new(0),
            runs: std::cell::Cell::new(0),
        };
        assert!(
            native_diagnostics(&runner, lock).is_empty(),
            "no record can be named from {lock}"
        );
        assert_eq!(runner.observes.get(), 0, "{lock} observes nothing");
        assert_eq!(runner.runs.get(), 0, "{lock} runs no native CLI call");
    }

    assert!(
        native_section_findings(&serde_json::json!({})).is_empty(),
        "an absent section is not a finding"
    );
    assert!(
        native_section_findings(&t13_lock()).is_empty(),
        "a readable section is not a finding"
    );
}

#[test]
fn p7_t13_registered_but_not_loaded_route_reports_that_loading_is_not_proven() {
    let (diagnostics, _, _) = t13_diagnose(Reported::registered());
    let diagnostic = &diagnostics[0];
    assert_eq!(diagnostic.state, NativeState::RegisteredNotLoaded);
    assert!(
        diagnostic.reason.contains("not proven loaded"),
        "{}",
        diagnostic.reason
    );
    assert!(
        diagnostic.reason.contains("the loader did not run"),
        "{}",
        diagnostic.reason
    );

    let line = native_diagnostic_line(diagnostic);
    assert!(line.contains("loader NotRun"), "{line}");
    assert!(line.contains("not proven loaded"), "{line}");
    assert!(!diagnostic.is_full_success());
    assert!(
        native_findings(&diagnostics).is_empty(),
        "registered-not-loaded is not a finding"
    );
}

/// A committed record for `target` and `selector`, shaped as `ccync sync` writes it.
fn t13_record(target: &str, selector: &str) -> serde_json::Value {
    serde_json::json!({
        "target": target,
        "selector": selector,
        "pluginName": "probe-provider",
        "upstreamRevision": "rev-1",
        "generation": {"source": "probe-source", "generation": "gen-1", "packageDigest": T13_DIGEST},
        "capabilities": ["skill"]
    })
}

/// A lock with several committed records across Codex and Claude Code.
fn t13_multi_lock() -> serde_json::Value {
    serde_json::json!({"_providerLifecycle": {"schemaVersion": 1, "committed": [
        t13_record("codex", "a@ccync-marketplace"),
        t13_record("codex", "b@ccync-marketplace"),
        t13_record("codex", "c@ccync-marketplace"),
        t13_record("claude", "a@ccync-marketplace"),
        t13_record("claude", "b@ccync-marketplace"),
    ]}})
}

/// A fake client whose native listing fails with `error` for every call. Its
/// `run` counts any CLI call that is not an observation, which must not occur.
struct ListingFailureRunner {
    error: &'static str,
    observed: std::cell::RefCell<Vec<NativeExecutable>>,
}

impl NativeCommandRunner for ListingFailureRunner {
    fn run(&self, _executable: NativeExecutable, _args: &[&str]) -> Result<Vec<u8>, String> {
        Err("read-only diagnostics must not run a native CLI call".to_string())
    }

    fn observe(
        &self,
        executable: NativeExecutable,
        _selector: &NativeSelector,
    ) -> Result<NativeRegistration, String> {
        self.observed.borrow_mut().push(executable);
        Err(self.error.to_string())
    }

    fn marketplace_root(
        &self,
        _executable: NativeExecutable,
        _marketplace: &str,
    ) -> Result<Option<String>, String> {
        Ok(None)
    }
}

fn t13_listing_failure(error: &'static str) -> (Vec<NativeDiagnostic>, Vec<NativeExecutable>) {
    let runner = ListingFailureRunner {
        error,
        observed: std::cell::RefCell::new(Vec::new()),
    };
    let diagnostics = native_diagnostics(&runner, &t13_multi_lock());
    (diagnostics, runner.observed.into_inner())
}

#[test]
fn p7_t13_timed_out_listing_is_called_once_per_executable_across_records() {
    let (diagnostics, observed) =
        t13_listing_failure("claude plugin list --json: exceeded the 60s time bound");

    assert_eq!(diagnostics.len(), 5, "every committed record is named");
    assert_eq!(
        observed,
        vec![NativeExecutable::Codex, NativeExecutable::Claude],
        "each executable is observed once, not once per record"
    );
    for diagnostic in &diagnostics {
        assert_eq!(
            diagnostic.state,
            NativeState::Unreadable,
            "a timed-out listing is not observable: {diagnostic:?}"
        );
        assert!(!diagnostic.is_full_success());
    }
    let findings = native_findings(&diagnostics);
    assert_eq!(findings.len(), 5);
    assert!(findings
        .iter()
        .all(|finding| finding.severity == Severity::Warning));
}

#[test]
fn p7_t13_missing_executable_is_unavailable_and_observed_once() {
    let (diagnostics, observed) = t13_listing_failure(
        "codex plugin list --json: could not start executable: program not found",
    );

    assert_eq!(observed.len(), 2, "one observation per executable");
    assert_eq!(diagnostics.len(), 5);
    for diagnostic in &diagnostics {
        assert_eq!(
            diagnostic.state,
            NativeState::Unavailable,
            "a CLI that cannot start is Unavailable: {diagnostic:?}"
        );
        assert_ne!(diagnostic.state, NativeState::Unreadable);
    }
    let line = native_diagnostic_line(&diagnostics[0]);
    assert!(line.contains("unavailable, NotRun"), "{line}");
    assert!(!diagnostics[0].is_full_success());
}

#[test]
fn p7_t13_shared_observation_is_computed_once_for_check_and_evidence_lines() {
    let runner = ReadOnlyRunner {
        reported: Reported::registered(),
        observes: std::cell::Cell::new(0),
        runs: std::cell::Cell::new(0),
    };
    let observations = NativeObservations::observe(&runner, &t13_lock());
    assert_eq!(
        runner.observes.get(),
        1,
        "one committed record, one observation"
    );

    let findings = observations.findings();
    let evidence = observations.evidence_lines();
    let diagnostics = observations.diagnostics();

    assert_eq!(
        runner.observes.get(),
        1,
        "reading the shared value observes nothing new"
    );
    assert_eq!(runner.runs.get(), 0, "no native CLI call");
    assert_eq!(diagnostics.len(), 1);
    assert!(
        findings.is_empty(),
        "registered-not-loaded is not a finding"
    );
    assert_eq!(evidence.len(), 1, "{evidence:?}");
    assert!(evidence[0].starts_with("native: "), "{}", evidence[0]);
    assert!(evidence[0].contains("route native"), "{}", evidence[0]);
    assert!(!diagnostics[0].is_full_success());
}
