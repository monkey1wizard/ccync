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
    fs::write(&input, "instructions").unwrap();
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
        .contains(fs::canonicalize(&path).unwrap().to_string_lossy().as_ref()));
    assert_eq!(
        findings[0].fix_hint.as_deref(),
        Some("repair or remove the ccync markers in the target file, then run `ccync sync`")
    );
}

#[test]
fn absent_block_is_only_a_finding_when_input_exists() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("AGENTS.md");
    let path = directory.path().join("target.md");
    fs::write(&input, "instructions").unwrap();
    fs::write(&path, "user instructions\n").unwrap();

    let findings = check(
        &input,
        vec![resolution(ProjectionTargetId::Claude, path.clone(), true)],
    );
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, Severity::Error);
    assert!(findings[0].message.contains("missing"));
    assert_eq!(findings[0].fix_hint.as_deref(), Some("run `ccync sync`"));

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
    fs::write(&input, "instructions").unwrap();
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
    fs::write(&input, "instructions").unwrap();
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
        .contains(fs::canonicalize(&deselected).unwrap().to_string_lossy().as_ref()));
    assert_eq!(findings[0].fix_hint, None);
}

#[test]
fn shared_file_stays_current_when_any_of_its_targets_is_still_selected() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("AGENTS.md");
    let path = directory.path().join("shared").join("AGENTS.md");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&input, "instructions").unwrap();
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
    fs::write(&input, "instructions").unwrap();
    fs::write(&codex, managed_block("instructions")).unwrap();
    fs::write(&override_path, "override\n").unwrap();

    let findings = check(
        &input,
        vec![resolution(ProjectionTargetId::Codex, codex, true)],
    );

    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, Severity::Warning);
    assert!(findings[0].message.contains("shadowed"));
    assert_eq!(findings[0].fix_hint, None);
}

#[test]
fn empty_codex_override_does_not_shadow_the_managed_file() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("AGENTS.md");
    let codex = directory.path().join(".codex").join("AGENTS.md");
    let override_path = codex.parent().unwrap().join("AGENTS.override.md");
    fs::create_dir_all(codex.parent().unwrap()).unwrap();
    fs::write(&input, "instructions").unwrap();
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
    fs::write(&input, "instructions").unwrap();
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

fn only(
    findings: &[ccync_foundation::health::DoctorFinding],
) -> &ccync_foundation::health::DoctorFinding {
    assert_eq!(findings.len(), 1, "findings: {:?}", messages(findings));
    &findings[0]
}

#[test]
fn missing_target_file_is_an_error_with_recovery() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("AGENTS.md");
    let path = directory.path().join("target.md");
    fs::write(&input, "instructions").unwrap();

    let findings = check(
        &input,
        vec![resolution(ProjectionTargetId::Claude, path.clone(), true)],
    );

    let finding = only(&findings);
    assert_eq!(finding.severity, Severity::Error);
    assert!(finding.message.contains("missing"));
    assert_eq!(finding.fix_hint.as_deref(), Some("run `ccync sync`"));
    assert!(!path.exists());
}

#[test]
fn drifted_block_is_an_error() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("AGENTS.md");
    let path = directory.path().join("target.md");
    fs::write(&input, "new instructions").unwrap();
    fs::write(&path, managed_block("old instructions")).unwrap();

    let findings = check(
        &input,
        vec![resolution(ProjectionTargetId::Claude, path, true)],
    );

    let finding = only(&findings);
    assert_eq!(finding.severity, Severity::Error);
    assert!(finding.message.contains("differs"));
    assert_eq!(finding.fix_hint.as_deref(), Some("run `ccync sync`"));
}

#[test]
fn crlf_block_equal_to_the_transform_is_not_drift() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("AGENTS.md");
    let path = directory.path().join("target.md");
    fs::write(&input, "line one\nline two").unwrap();
    fs::write(
        &path,
        format!("user\r\n{BEGIN}\r\nline one\r\nline two\r\n{END}\r\n"),
    )
    .unwrap();

    let findings = check(
        &input,
        vec![resolution(ProjectionTargetId::Claude, path, true)],
    );

    assert!(findings.is_empty(), "findings: {:?}", messages(&findings));
}

#[test]
fn residual_block_after_source_removal_is_an_error() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("AGENTS.md");
    let path = directory.path().join("target.md");
    fs::write(&path, managed_block("instructions")).unwrap();

    let findings = check(
        &input,
        vec![resolution(ProjectionTargetId::Claude, path, true)],
    );

    let finding = only(&findings);
    assert_eq!(finding.severity, Severity::Error);
    assert!(finding.message.contains("remains"));
    assert_eq!(finding.fix_hint.as_deref(), Some("run `ccync sync`"));
}

#[test]
fn zero_byte_source_with_residual_block_is_an_error() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("AGENTS.md");
    let path = directory.path().join("target.md");
    fs::write(&input, b"").unwrap();
    fs::write(&path, managed_block("instructions")).unwrap();

    let findings = check(
        &input,
        vec![resolution(ProjectionTargetId::Claude, path, true)],
    );

    let finding = only(&findings);
    assert_eq!(finding.severity, Severity::Error);
    assert!(finding.message.contains("remains"));
    assert_eq!(finding.fix_hint.as_deref(), Some("run `ccync sync`"));
}

#[test]
fn unreadable_source_is_one_error_and_hides_target_drift() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("AGENTS.md");
    let path = directory.path().join("target.md");
    fs::create_dir(&input).unwrap();
    fs::write(&path, managed_block("anything")).unwrap();

    let findings = check(
        &input,
        vec![resolution(ProjectionTargetId::Claude, path, true)],
    );

    let finding = only(&findings);
    assert_eq!(finding.severity, Severity::Error);
    assert!(finding
        .message
        .contains("instruction source cannot be read"));
    assert_eq!(
        finding.fix_hint.as_deref(),
        Some("check the instruction source file permissions")
    );
}

#[test]
fn invalid_utf8_source_is_an_error() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("AGENTS.md");
    let path = directory.path().join("target.md");
    fs::write(&input, [0xff, 0xfe, 0xfd]).unwrap();

    let findings = check(
        &input,
        vec![resolution(ProjectionTargetId::Claude, path, true)],
    );

    let finding = only(&findings);
    assert_eq!(finding.severity, Severity::Error);
    assert!(finding.message.contains("UTF-8"));
    assert_eq!(
        finding.fix_hint.as_deref(),
        Some("re-save the instruction source as UTF-8")
    );
}

#[test]
fn unreadable_target_is_an_error_and_not_reported_as_damaged() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("AGENTS.md");
    let path = directory.path().join("target.md");
    fs::write(&input, "instructions").unwrap();
    fs::create_dir(&path).unwrap();

    let findings = check(
        &input,
        vec![resolution(ProjectionTargetId::Claude, path, true)],
    );

    let finding = only(&findings);
    assert_eq!(finding.severity, Severity::Error);
    assert!(finding.message.contains("cannot be read"));
    assert!(!finding.message.contains("damaged"));
    assert_eq!(
        finding.fix_hint.as_deref(),
        Some("check the target file permissions")
    );
}

#[test]
fn damaged_deselected_target_is_still_an_error() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("AGENTS.md");
    let path = directory.path().join("target.md");
    fs::write(&input, "instructions").unwrap();
    fs::write(&path, format!("{BEGIN}\nno end\n")).unwrap();

    let findings = check(
        &input,
        vec![resolution(ProjectionTargetId::Claude, path, false)],
    );

    let finding = only(&findings);
    assert_eq!(finding.severity, Severity::Error);
    assert!(finding.message.contains("damaged"));
    assert_eq!(
        finding.fix_hint.as_deref(),
        Some("repair or remove the ccync markers in the target file, then run `ccync sync`")
    );
}

#[test]
fn deselected_target_without_block_is_silent() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("AGENTS.md");
    let path = directory.path().join("target.md");
    fs::write(&input, "instructions").unwrap();
    fs::write(&path, "user instructions\n").unwrap();

    assert!(check(
        &input,
        vec![resolution(ProjectionTargetId::Claude, path, false)]
    )
    .is_empty());
}

#[test]
fn unreadable_codex_override_is_an_error() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("AGENTS.md");
    let codex = directory.path().join(".codex").join("AGENTS.md");
    let override_path = codex.parent().unwrap().join("AGENTS.override.md");
    fs::create_dir_all(&override_path).unwrap();
    fs::write(&input, "instructions").unwrap();
    fs::write(&codex, managed_block("instructions")).unwrap();

    let findings = check(
        &input,
        vec![resolution(ProjectionTargetId::Codex, codex, true)],
    );

    let finding = only(&findings);
    assert_eq!(finding.severity, Severity::Error);
    assert!(finding.message.contains("override cannot be read"));
    assert_eq!(
        finding.fix_hint.as_deref(),
        Some("check the override file permissions or remove it")
    );
}

#[test]
fn matching_block_is_healthy_without_findings() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("AGENTS.md");
    let path = directory.path().join("target.md");
    fs::write(&input, "instructions").unwrap();
    fs::write(&path, managed_block("instructions")).unwrap();

    let findings = check(
        &input,
        vec![resolution(ProjectionTargetId::Claude, path, true)],
    );

    assert!(findings.is_empty(), "findings: {:?}", messages(&findings));
}

#[cfg(unix)]
#[test]
fn dangling_source_symlink_is_an_error_with_recovery() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().unwrap();
    let missing_input = directory.path().join("missing-input.md");
    let input = directory.path().join("AGENTS.md");
    let path = directory.path().join("target.md");
    symlink(missing_input, &input).unwrap();
    fs::write(&path, managed_block("instructions")).unwrap();

    let findings = check(
        &input,
        vec![resolution(ProjectionTargetId::Claude, path, true)],
    );

    let finding = only(&findings);
    assert_eq!(finding.severity, Severity::Error);
    assert!(finding
        .message
        .contains("instruction source is a dangling symbolic link"));
    assert_eq!(
        finding.fix_hint.as_deref(),
        Some("remove or repair the symbolic link")
    );
}

#[cfg(unix)]
#[test]
fn dangling_target_symlink_is_an_error_with_recovery() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().unwrap();
    let input = directory.path().join("AGENTS.md");
    let missing_target = directory.path().join("missing-target.md");
    let path = directory.path().join("target.md");
    fs::write(&input, "instructions").unwrap();
    symlink(missing_target, &path).unwrap();

    let findings = check(
        &input,
        vec![resolution(ProjectionTargetId::Claude, path, true)],
    );

    let finding = only(&findings);
    assert_eq!(finding.severity, Severity::Error);
    assert!(finding
        .message
        .contains("managed block target is a dangling symbolic link"));
    assert_eq!(
        finding.fix_hint.as_deref(),
        Some("remove or repair the symbolic link, then run `ccync sync`")
    );
}

#[test]
fn health_check_leaves_every_matrix_file_byte_identical() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("AGENTS.md");
    let drifted = directory.path().join("drifted.md");
    let missing_block = directory.path().join("plain.md");
    let stale = directory.path().join("stale.md");
    fs::write(&input, "new").unwrap();
    fs::write(&drifted, managed_block("old")).unwrap();
    fs::write(&missing_block, "user\n").unwrap();
    fs::write(&stale, managed_block("old")).unwrap();
    let paths = [&input, &drifted, &missing_block, &stale];
    let before: Vec<Vec<u8>> = paths.iter().map(|path| fs::read(path).unwrap()).collect();

    let findings = check(
        &input,
        vec![
            resolution(ProjectionTargetId::Claude, drifted.clone(), true),
            resolution(ProjectionTargetId::Copilot, missing_block.clone(), true),
            resolution(ProjectionTargetId::Opencode, stale.clone(), false),
        ],
    );

    assert_eq!(findings.len(), 3, "findings: {:?}", messages(&findings));
    let after: Vec<Vec<u8>> = paths.iter().map(|path| fs::read(path).unwrap()).collect();
    assert_eq!(before, after);
}
