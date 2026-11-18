use super::managed_block::{
    classify, read_instruction_source, BlockClassification, InstructionSource,
    InstructionSourceError,
};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

const BEGIN: &str = "<!-- ccync:managed:begin -->";
const END: &str = "<!-- ccync:managed:end -->";

fn managed_block(payload: &str) -> String {
    format!("{BEGIN}\n{payload}\n{END}\n")
}

fn assert_unchanged(
    path: &Path,
    before: Option<&[u8]>,
    payload: Option<&str>,
    expected: BlockClassification,
) {
    let result = classify(path, payload);

    assert_eq!(result, expected);
    assert_eq!(fs::read(path).ok().as_deref(), before);
}

#[test]
fn classifies_a_missing_file_as_create_without_writing() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("instructions");

    assert_eq!(
        classify(&path, Some("managed")),
        BlockClassification::Create
    );
    assert!(!path.exists());
}

#[test]
fn classifies_a_file_without_a_block_as_append_without_writing() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("instructions");
    let original = b"user content\n";
    fs::write(&path, original).unwrap();

    assert_unchanged(
        &path,
        Some(original),
        Some("managed"),
        BlockClassification::Append,
    );
}

#[test]
fn classifies_a_different_existing_block_as_rewrite_without_writing() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("instructions");
    let original = managed_block("old");
    fs::write(&path, &original).unwrap();

    assert_unchanged(
        &path,
        Some(original.as_bytes()),
        Some("new"),
        BlockClassification::Rewrite,
    );
}

#[test]
fn classifies_an_existing_block_with_no_payload_as_prune_without_writing() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("instructions");
    let original = managed_block("old");
    fs::write(&path, &original).unwrap();

    assert_unchanged(
        &path,
        Some(original.as_bytes()),
        None,
        BlockClassification::Prune,
    );
}

#[test]
fn classifies_no_block_and_no_payload_as_unchanged_without_writing() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("instructions");
    let original = b"user content\n";
    fs::write(&path, original).unwrap();

    assert_unchanged(&path, Some(original), None, BlockClassification::Unchanged);
}

#[test]
fn classifies_a_damaged_file_without_writing() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("instructions");
    let original = format!("before\n{BEGIN}\nmanaged");
    fs::write(&path, &original).unwrap();

    assert_unchanged(
        &path,
        Some(original.as_bytes()),
        Some("replacement"),
        BlockClassification::MalformedMarker,
    );
}

#[cfg(unix)]
#[test]
fn classifies_a_dangling_symlink_without_writing() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().unwrap();
    let target = directory.path().join("missing");
    let path = directory.path().join("instructions");
    symlink(&target, &path).unwrap();

    assert_eq!(
        classify(&path, Some("managed")),
        BlockClassification::DanglingLink
    );
    assert!(fs::symlink_metadata(&path)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(!target.exists());
}

#[test]
fn classifies_transform_equivalent_managed_content_as_unchanged_without_writing() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("instructions");
    let original = managed_block("managed");
    fs::write(&path, &original).unwrap();

    assert_unchanged(
        &path,
        Some(original.as_bytes()),
        Some("managed"),
        BlockClassification::Unchanged,
    );
}

#[test]
fn classifies_a_target_read_failure_without_writing() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("target-directory");
    fs::create_dir(&path).unwrap();

    assert_eq!(classify(&path, Some("managed")), BlockClassification::TargetRead);
    assert!(path.is_dir());
    assert_eq!(fs::read_dir(path).unwrap().count(), 0);
}

#[test]
fn managed_block_source_reader_distinguishes_absence_and_content() {
    let directory = tempdir().unwrap();
    let missing = directory.path().join("missing");
    let empty = directory.path().join("empty");
    let content = directory.path().join("content");
    let whitespace = directory.path().join("whitespace");
    fs::write(&empty, []).unwrap();
    fs::write(&content, "managed instructions\n").unwrap();
    fs::write(&whitespace, " \n\t").unwrap();

    assert_eq!(read_instruction_source(&missing), Ok(InstructionSource::Absent));
    assert_eq!(read_instruction_source(&empty), Ok(InstructionSource::Absent));
    assert_eq!(
        read_instruction_source(&content),
        Ok(InstructionSource::Content("managed instructions\n".to_owned()))
    );
    assert_eq!(
        read_instruction_source(&whitespace),
        Ok(InstructionSource::Content(" \n\t".to_owned()))
    );
}

#[test]
fn managed_block_source_reader_reports_invalid_utf8() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("invalid-utf8");
    fs::write(&path, [0xff]).unwrap();

    assert!(matches!(
        read_instruction_source(&path),
        Err(InstructionSourceError::Utf8(_))
    ));
}

#[test]
fn managed_block_source_reader_reports_read_errors() {
    let directory = tempdir().unwrap();

    assert!(matches!(
        read_instruction_source(directory.path()),
        Err(InstructionSourceError::Read(_))
    ));
}

#[cfg(unix)]
#[test]
fn managed_block_source_reader_reports_dangling_links() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().unwrap();
    let target = directory.path().join("missing-target");
    let path = directory.path().join("instruction-link");
    symlink(target, &path).unwrap();

    assert_eq!(
        read_instruction_source(&path),
        Err(InstructionSourceError::DanglingLink)
    );
}
