use super::install::{check_subpath_lexical, split_git_subpath};

#[test]
fn split_git_subpath_returns_non_remote_unchanged() {
    assert_eq!(split_git_subpath("./plugin"), ("./plugin", None));
    assert_eq!(split_git_subpath("./local//x"), ("./local//x", None));
    assert_eq!(
        split_git_subpath("C:/Code/repo//sub"),
        ("C:/Code/repo//sub", None)
    );
}

#[test]
fn split_git_subpath_handles_scheme_urls() {
    assert_eq!(
        split_git_subpath("https://example.com/org/repo//plugins/one"),
        ("https://example.com/org/repo", Some("plugins/one"))
    );
    assert_eq!(
        split_git_subpath("https://host/o/r//plugins/x"),
        ("https://host/o/r", Some("plugins/x"))
    );
}

#[test]
fn split_git_subpath_handles_scheme_urls_without_subpath() {
    assert_eq!(
        split_git_subpath("https://host/o/r"),
        ("https://host/o/r", None)
    );
}

#[test]
fn split_git_subpath_handles_scp_urls() {
    assert_eq!(
        split_git_subpath("git@example.com:org/repo//plugins/one"),
        ("git@example.com:org/repo", Some("plugins/one"))
    );
    assert_eq!(
        split_git_subpath("git@host:o/r//plugins/x"),
        ("git@host:o/r", Some("plugins/x"))
    );
}

#[test]
fn split_git_subpath_preserves_empty_remainder() {
    assert_eq!(
        split_git_subpath("https://example.com/org/repo//"),
        ("https://example.com/org/repo", Some(""))
    );
}

#[test]
fn check_subpath_lexical_accepts_safe_relative_paths() {
    assert_eq!(check_subpath_lexical("plugins/one"), Ok(()));
    assert_eq!(check_subpath_lexical("plugins/claude/context7"), Ok(()));
}

#[test]
fn check_subpath_lexical_rejects_empty_paths() {
    let err = check_subpath_lexical("").unwrap_err();
    assert!(err.contains("empty"), "expected error naming rule: {err}");
}

#[test]
fn check_subpath_lexical_rejects_backslashes() {
    let err = check_subpath_lexical("plugins\\one").unwrap_err();
    assert!(
        err.contains("plugins\\one") && err.contains('\\'),
        "expected error naming subpath and rule: {err}"
    );

    let err2 = check_subpath_lexical("a\\b").unwrap_err();
    assert!(
        err2.contains("a\\b") && err2.contains('\\'),
        "expected error naming subpath and rule: {err2}"
    );
}

#[test]
fn check_subpath_lexical_rejects_empty_and_dot_segments() {
    for subpath in [
        "plugins//one",
        "plugins/./one",
        "plugins/../one",
        "plugins/x/../y",
        "./x",
        "a//b",
    ] {
        let err = check_subpath_lexical(subpath).unwrap_err();
        assert!(
            err.contains(subpath),
            "expected error naming subpath '{subpath}': {err}"
        );
    }
}

#[test]
fn check_subpath_lexical_rejects_absolute_or_prefixed_paths() {
    for subpath in ["/plugins", "C:/plugins", "C:/abs", "/abs"] {
        let err = check_subpath_lexical(subpath).unwrap_err();
        assert!(
            err.contains(subpath),
            "expected error naming subpath '{subpath}': {err}"
        );
    }
}
