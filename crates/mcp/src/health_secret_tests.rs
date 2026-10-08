use crate::McpProjectionHealthCheck;
use ccync_foundation::health::{HealthCheck, Severity};

fn write_projection(content: &str) -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("managed.json"), content).unwrap();
    temp
}

#[test]
fn reports_unresolved_secrets_once_per_server() {
    let _guard = crate::test_support::env_guard();
    let temp = write_projection(
        r#"{
            "servers": {
                "alpha": {"command": "run", "args": ["${ALPHA_API_KEY}", "${ALPHA_TOKEN}"]},
                "beta": {"command": "run", "env": {"secret": "${BETA_PASSWORD}"}},
                "plain": {"command": "run", "args": ["${OTHER_VALUE}"]}
            }
        }"#,
    );
    std::env::remove_var("ALPHA_API_KEY");
    std::env::remove_var("ALPHA_TOKEN");
    std::env::remove_var("BETA_PASSWORD");

    let findings = McpProjectionHealthCheck::with_path(temp.path().join("managed.json")).check();

    assert_eq!(findings.len(), 2);
    assert!(findings
        .iter()
        .all(|finding| finding.severity == Severity::Warning));
    assert!(findings[0].message.contains("alpha"));
    assert!(findings[0].message.contains("ALPHA_API_KEY"));
    assert!(findings[0].message.contains("ALPHA_TOKEN"));
    assert!(findings[0]
        .message
        .contains("cannot be resolved in the current environment"));
    assert!(findings[0].message.contains("config.json#secrets"));
    assert!(findings[0].message.contains("environment"));
    assert!(findings[1].message.contains("beta"));
    assert!(findings[1].message.contains("BETA_PASSWORD"));
}

#[test]
fn valid_json_that_is_not_an_mcp_manifest_has_no_extra_finding() {
    let _guard = crate::test_support::env_guard();
    let temp = write_projection(r#"{"status":"ok"}"#);

    let findings = McpProjectionHealthCheck::with_path(temp.path().join("managed.json")).check();

    assert!(findings.is_empty());
}

#[test]
fn resolved_secret_does_not_report_a_finding() {
    let _guard = crate::test_support::env_guard();
    let temp = write_projection(r#"{"servers":{"alpha":{"command":"${ALPHA_TOKEN}"}}}"#);
    std::env::set_var("ALPHA_TOKEN", "resolved");

    let findings = McpProjectionHealthCheck::with_path(temp.path().join("managed.json")).check();

    std::env::remove_var("ALPHA_TOKEN");
    assert!(findings.is_empty());
}

#[test]
fn fixture_with_one_unresolved_secret_server_yields_exactly_one_warning() {
    let _guard = crate::test_support::env_guard();
    let temp = write_projection(
        r#"{
            "servers": {
                "alpha": {"command": "run", "args": ["${ALPHA_API_KEY}"]}
            }
        }"#,
    );
    std::env::remove_var("ALPHA_API_KEY");

    let findings = McpProjectionHealthCheck::with_path(temp.path().join("managed.json")).check();

    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, Severity::Warning);
    assert!(findings[0].message.contains("alpha"));
    assert!(findings[0].message.contains("ALPHA_API_KEY"));
    assert!(findings[0]
        .message
        .contains("cannot be resolved in the current environment"));
    assert!(findings[0].message.contains("config.json#secrets"));
    assert!(findings[0].message.contains("environment"));
}

#[test]
fn invalid_json_finding_unchanged() {
    let _guard = crate::test_support::env_guard();
    let temp = write_projection("not json");

    let findings = McpProjectionHealthCheck::with_path(temp.path().join("managed.json")).check();

    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, Severity::Error);
    assert!(findings[0].message.contains("MCP projection invalid JSON"));
}
