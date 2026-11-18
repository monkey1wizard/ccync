use ccync_engine::install::is_remote_url;

#[test]
fn is_remote_url_classifies_http_git_and_ssh_prefixes() {
    // Remote prefixes should return true
    assert!(is_remote_url("http://example.com/repo.git"));
    assert!(is_remote_url("https://github.com/org/repo.git"));
    assert!(is_remote_url("git@github.com:org/repo.git"));
    assert!(is_remote_url("git://github.com/org/repo.git"));

    // Local paths and catalog ids should return false
    assert!(!is_remote_url("./local/path"));
    assert!(!is_remote_url("/absolute/local/path"));
    assert!(!is_remote_url("catalog-id"));
}
