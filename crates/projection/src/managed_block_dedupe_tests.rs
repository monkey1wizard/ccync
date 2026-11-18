use super::managed_block::dedupe_resolved;
use std::fs;
use tempfile::tempdir;

#[test]
fn existing_paths_with_the_same_resolved_parent_and_name_are_deduped() {
    let directory = tempdir().unwrap();
    let nested = directory.path().join("nested");
    fs::create_dir(&nested).unwrap();

    let first = directory.path().join("instructions");
    let alias = nested.join("..").join("instructions");
    let other = directory.path().join("other");
    fs::write(&first, "first").unwrap();
    fs::write(&other, "other").unwrap();

    assert_eq!(
        dedupe_resolved(vec![first.clone(), alias, other.clone()]),
        vec![first, other]
    );
}

#[test]
fn missing_files_with_the_same_resolved_parent_and_name_are_deduped() {
    let directory = tempdir().unwrap();
    let nested = directory.path().join("nested");
    fs::create_dir(&nested).unwrap();

    let first = directory.path().join("not-yet-created");
    let alias = nested.join("..").join("not-yet-created");

    assert_eq!(dedupe_resolved(vec![first.clone(), alias]), vec![first]);
}

#[test]
fn different_file_names_in_one_parent_are_not_deduped() {
    let directory = tempdir().unwrap();
    let first = directory.path().join("instructions");
    let second = directory.path().join("other-instructions");

    assert_eq!(
        dedupe_resolved(vec![first.clone(), second.clone()]),
        vec![first, second]
    );
}

#[test]
fn uncanonicalizable_parents_fall_back_to_literal_paths() {
    let directory = tempdir().unwrap();
    let first = directory.path().join("missing-a").join("instructions");
    let second = directory.path().join("missing-b").join("instructions");

    assert_eq!(
        dedupe_resolved(vec![first.clone(), second.clone()]),
        vec![first, second]
    );
}

#[test]
fn dedupe_preserves_the_first_seen_path_and_order() {
    let directory = tempdir().unwrap();
    let nested = directory.path().join("nested");
    fs::create_dir(&nested).unwrap();

    let first_target = directory.path().join("first");
    let first_alias = nested.join("..").join("first");
    let second_target = directory.path().join("second");

    let paths = vec![first_alias.clone(), second_target.clone(), first_target];

    assert_eq!(dedupe_resolved(paths), vec![first_alias, second_target]);
}

#[test]
fn identical_literal_paths_are_deduped_when_parent_resolution_fails() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("missing-parent").join("instructions");

    assert_eq!(
        dedupe_resolved(vec![path.clone(), path.clone()]),
        vec![path]
    );
}
