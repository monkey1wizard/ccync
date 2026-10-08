use super::managed_block::{classify, BlockClassification};
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
        BlockClassification::Damaged,
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
        BlockClassification::Dangling
    );
    assert!(fs::symlink_metadata(&path)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(!target.exists());
}
