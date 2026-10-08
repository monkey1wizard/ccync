use super::managed_block::{write_block, BlockOutcome};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

const BEGIN: &str = "<!-- ccync:managed:begin -->";
const END: &str = "<!-- ccync:managed:end -->";

fn managed_block(payload: &str) -> String {
    format!("{BEGIN}\n{payload}\n{END}\n")
}

fn write(path: &Path, contents: &str) {
    fs::write(path, contents).expect("fixture write should succeed");
}

#[test]
fn dangling_symlink_fails_closed_without_writing() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let target = directory.path().join("missing");
        let link = directory.path().join("instructions");
        symlink(&target, &link).unwrap();

        let result = write_block(&link, Some("managed"));

        assert!(result.is_err());
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!target.exists());
    }
}

#[test]
fn creates_a_missing_file() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("instructions");

    assert_eq!(
        write_block(&path, Some("managed")),
        Ok(BlockOutcome::Create)
    );
    assert_eq!(fs::read_to_string(path).unwrap(), managed_block("managed"));
}

#[test]
fn creates_missing_parent_directories_before_writing() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("nested").join("instructions");

    assert_eq!(
        write_block(&path, Some("managed")),
        Ok(BlockOutcome::Create)
    );
    assert_eq!(fs::read_to_string(path).unwrap(), managed_block("managed"));
}

#[test]
fn appends_when_the_file_has_no_managed_block() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("instructions");
    write(&path, "user content\n");

    assert_eq!(
        write_block(&path, Some("managed")),
        Ok(BlockOutcome::Append)
    );
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        format!("user content\n{}", managed_block("managed"))
    );
}

#[test]
fn rewrites_an_existing_managed_block() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("instructions");
    write(&path, &format!("before\n{}after\n", managed_block("old")));

    assert_eq!(write_block(&path, Some("new")), Ok(BlockOutcome::Rewrite));
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        format!("before\n{}after\n", managed_block("new"))
    );
}

#[test]
fn prunes_an_existing_managed_block() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("instructions");
    write(&path, &format!("before\n{}after\n", managed_block("old")));

    assert_eq!(write_block(&path, None), Ok(BlockOutcome::Prune));
    assert_eq!(fs::read_to_string(path).unwrap(), "before\nafter\n");
}

#[test]
fn absent_payload_is_unchanged_without_a_managed_block() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("instructions");
    write(&path, "user content\n");

    assert_eq!(write_block(&path, None), Ok(BlockOutcome::Unchanged));
    assert_eq!(fs::read_to_string(path).unwrap(), "user content\n");
}

#[test]
fn damaged_file_is_not_written() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("instructions");
    let original = format!("before\n{BEGIN}\nmanaged");
    write(&path, &original);

    assert!(write_block(&path, Some("replacement")).is_err());
    assert_eq!(fs::read_to_string(path).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn writing_through_a_symlink_preserves_the_link_and_updates_the_target() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().unwrap();
    let target = directory.path().join("target");
    let link = directory.path().join("instructions");
    write(&target, "user content\n");
    symlink(&target, &link).unwrap();

    assert_eq!(
        write_block(&link, Some("managed")),
        Ok(BlockOutcome::Append)
    );
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        fs::read_to_string(target).unwrap(),
        format!("user content\n{}", managed_block("managed"))
    );
}
