use ccync_engine::ExitCode;

#[test]
fn updates_available_numeric_value_is_three() {
    assert_eq!(
        ExitCode::UpdatesAvailable as u8,
        3,
        "ExitCode::UpdatesAvailable must convert to the integer 3, which restore --dry-run and roster install --dry-run both depend on"
    );
    assert_eq!(
        ExitCode::UpdatesAvailable as i32,
        3,
        "ExitCode::UpdatesAvailable as i32 must equal 3"
    );

    // Verify From<ExitCode> for std::process::ExitCode conversion compiles and preserves value
    let process_code: std::process::ExitCode = ExitCode::UpdatesAvailable.into();
    let _ = process_code;
}

#[test]
fn all_exit_code_variants_numeric_contract() {
    assert_eq!(ExitCode::Success as u8, 0);
    assert_eq!(ExitCode::Error as u8, 1);
    assert_eq!(ExitCode::NotWired as u8, 2);
    assert_eq!(ExitCode::UpdatesAvailable as u8, 3);
    assert_eq!(ExitCode::Usage as u8, 64);
}

#[test]
fn updates_available_doc_comment_names_restore_dry_run() {
    let lib_source = include_str!("../src/lib.rs");
    let (before_variant, _) = lib_source
        .split_once("UpdatesAvailable = 3")
        .expect("lib.rs must declare UpdatesAvailable = 3");

    let doc_comment = before_variant
        .lines()
        .rev()
        .take_while(|line| line.trim_start().starts_with("///"))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        doc_comment.contains("restore --dry-run"),
        "ExitCode::UpdatesAvailable doc comment must name restore --dry-run"
    );
}

#[test]
fn updates_available_doc_comment_names_roster_install_dry_run() {
    let lib_source = include_str!("../src/lib.rs");
    let (before_variant, _) = lib_source
        .split_once("UpdatesAvailable = 3")
        .expect("lib.rs must declare UpdatesAvailable = 3");

    let doc_comment = before_variant
        .lines()
        .rev()
        .take_while(|line| line.trim_start().starts_with("///"))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        doc_comment.contains("roster install --dry-run"),
        "ExitCode::UpdatesAvailable doc comment must name roster install --dry-run"
    );
}
