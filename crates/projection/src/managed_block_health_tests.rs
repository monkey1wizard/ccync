use super::managed_block::{ManagedBlockHealthCheck, TargetResolution};
use ccync_foundation::health::{HealthCheck, Severity};
use ccync_foundation::target::ProjectionTargetId;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

const BEGIN: &str = "<!-- ccync:managed:begin -->";
const END: &str = "<!-- ccync:managed:end -->";

fn managed_block(body: &str) -> String {
    format!("{BEGIN}\n{body}\n{END}\n")
}

fn resolution(target: ProjectionTargetId, path: PathBuf, selected: bool) -> TargetResolution {
    TargetResolution {
        target,
        path,
        selected,
    }
}

fn check(
    input: &Path,
    targets: Vec<TargetResolution>,
) -> Vec<ccync_foundation::health::DoctorFinding> {
    ManagedBlockHealthCheck::new(input.to_path_buf(), targets).check()
}

fn messages(findings: &[ccync_foundation::health::DoctorFinding]) -> Vec<&str> {
    findings
        .iter()
        .map(|finding| finding.message.as_str())
        .collect()
}

#[test]
fn damaged_file_is_an_error_and_is_reported_once() {
    let directory = tempdir().unwrap();
    let input = directory.path().join(".ccync").join("AGENTS.md");
    fs::create_dir_all(input.parent().unwrap()).unwrap();
    fs::write(&input, "instructions\n").unwrap();
    let path = directory.path().join("AGENTS.md");
    fs::write(&path, format!("{BEGIN}\nmissing end\n")).unwrap();

    let findings = check(
        &input,
        vec![resolution(ProjectionTargetId::Codex, path.clone(), true)],
    );

    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, Severity::Error);
    assert!(findings[0]
        .message
        .contains(path.to_string_lossy().as_ref()));
}

#[test]
fn absent_block_is_only_a_finding_when_input_exists() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("AGENTS.md");
    let path = directory.path().join("target.md");
    fs::write(&input, "instructions\n").unwrap();
    fs::write(&path, "user instructions\n").unwrap();

    let findings = check(
        &input,
        vec![resolution(ProjectionTargetId::Claude, path.clone(), true)],
    );
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, Severity::Error);

    fs::remove_file(&input).unwrap();
    assert!(check(
        &input,
        vec![resolution(ProjectionTargetId::Claude, path, true)]
    )
    .is_empty());
}

#[test]
fn shared_file_is_current_when_one_selected_target_still_resolves_to_it() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("AGENTS.md");
    let path = directory.path().join("shared").join("AGENTS.md");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&input, "instructions\n").unwrap();
    fs::write(&path, managed_block("instructions")).unwrap();

    let findings = check(
        &input,
        vec![
            resolution(ProjectionTargetId::AgyCli, path.clone(), false),
            resolution(ProjectionTargetId::AgyIde, path.clone(), false),
            resolution(ProjectionTargetId::AgyGui, path, true),
        ],
    );

    assert!(findings.is_empty(), "findings: {:?}", messages(&findings));
}

#[test]
fn stale_block_is_keyed_by_resolved_path_not_target_name() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("AGENTS.md");
    let deselected = directory.path().join("codex-agents.md");
    let selected = directory.path().join("claude.md");
    fs::write(&input, "instructions\n").unwrap();
    fs::write(&deselected, managed_block("instructions")).unwrap();
    fs::write(&selected, managed_block("instructions")).unwrap();

    let findings = check(
        &input,
        vec![
            resolution(ProjectionTargetId::Claude, selected, true),
            resolution(ProjectionTargetId::Codex, deselected.clone(), false),
        ],
    );

    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, Severity::Warning);
    assert!(findings[0]
        .message
        .contains(deselected.to_string_lossy().as_ref()));
}

#[test]
fn shared_file_stays_current_when_any_of_its_targets_is_still_selected() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("AGENTS.md");
    let path = directory.path().join("shared").join("AGENTS.md");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&input, "instructions\n").unwrap();
    fs::write(&path, managed_block("instructions")).unwrap();

    // Deselecting one alias while another alias to the same resolved path
    // stays selected must not produce a stale finding.
    let findings = check(
        &input,
        vec![
            resolution(ProjectionTargetId::AgyCli, path.clone(), false),
            resolution(ProjectionTargetId::AgyIde, path, true),
        ],
    );

    assert!(findings.is_empty(), "findings: {:?}", messages(&findings));
}

#[test]
fn non_empty_codex_override_is_a_warning() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("AGENTS.md");
    let codex = directory.path().join(".codex").join("AGENTS.md");
    let override_path = codex.parent().unwrap().join("AGENTS.override.md");
    fs::create_dir_all(codex.parent().unwrap()).unwrap();
    fs::write(&input, "instructions\n").unwrap();
    fs::write(&codex, managed_block("instructions")).unwrap();
    fs::write(&override_path, "override\n").unwrap();

    let findings = check(
        &input,
        vec![resolution(ProjectionTargetId::Codex, codex, true)],
    );

    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, Severity::Warning);
    assert!(findings[0].message.contains("shadowed"));
}

#[test]
fn empty_codex_override_does_not_shadow_the_managed_file() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("AGENTS.md");
    let codex = directory.path().join(".codex").join("AGENTS.md");
    let override_path = codex.parent().unwrap().join("AGENTS.override.md");
    fs::create_dir_all(codex.parent().unwrap()).unwrap();
    fs::write(&input, "instructions\n").unwrap();
    fs::write(&codex, managed_block("instructions")).unwrap();
    fs::write(&override_path, b"").unwrap();

    assert!(check(
        &input,
        vec![resolution(ProjectionTargetId::Codex, codex, true)]
    )
    .is_empty());
}

#[test]
fn health_check_does_not_modify_target_or_override_files() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("AGENTS.md");
    let codex = directory.path().join(".codex").join("AGENTS.md");
    let override_path = codex.parent().unwrap().join("AGENTS.override.md");
    fs::create_dir_all(codex.parent().unwrap()).unwrap();
    fs::write(&input, "instructions\n").unwrap();
    fs::write(&codex, "user instructions\n").unwrap();
    fs::write(&override_path, "override\n").unwrap();
    let before_target = fs::read(&codex).unwrap();
    let before_override = fs::read(&override_path).unwrap();

    let _ = check(
        &input,
        vec![resolution(ProjectionTargetId::Codex, codex.clone(), true)],
    );

    assert_eq!(fs::read(&codex).unwrap(), before_target);
    assert_eq!(fs::read(&override_path).unwrap(), before_override);
}
