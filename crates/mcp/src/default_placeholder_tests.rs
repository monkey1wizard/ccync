use crate::{build_variable_map_from, McpVariableResolver};
use std::collections::BTreeSet;

const VAR: &str = "CCYNC_TEST_DEFAULT_PLACEHOLDER_KEY";

#[test]
fn default_placeholder_unset_uses_empty_default() {
    let _env_guard = crate::test_support::env_guard();
    std::env::remove_var(VAR);

    let resolver = McpVariableResolver::new(build_variable_map_from(None));
    let (value, unresolved) = resolver.resolve_string("${CCYNC_TEST_DEFAULT_PLACEHOLDER_KEY:-}");

    assert_eq!(value, "");
    assert!(unresolved.is_empty());
}

#[test]
fn default_placeholder_unset_uses_text_default() {
    let _env_guard = crate::test_support::env_guard();
    std::env::remove_var(VAR);

    let resolver = McpVariableResolver::new(build_variable_map_from(None));
    let (value, unresolved) = resolver.resolve_string("${CCYNC_TEST_DEFAULT_PLACEHOLDER_KEY:-abc}");

    assert_eq!(value, "abc");
    assert!(unresolved.is_empty());
}

#[test]
fn default_placeholder_set_uses_non_empty_variable() {
    let _env_guard = crate::test_support::env_guard();
    std::env::set_var(VAR, "real");

    let resolver = McpVariableResolver::new(build_variable_map_from(None));
    let (value, unresolved) = resolver.resolve_string("${CCYNC_TEST_DEFAULT_PLACEHOLDER_KEY:-abc}");

    assert_eq!(value, "real");
    assert!(unresolved.is_empty());
    std::env::remove_var(VAR);
}

#[test]
fn default_placeholder_empty_variable_uses_text_default() {
    let _env_guard = crate::test_support::env_guard();
    std::env::set_var(VAR, "");

    let resolver = McpVariableResolver::new(build_variable_map_from(None));
    let (value, unresolved) = resolver.resolve_string("${CCYNC_TEST_DEFAULT_PLACEHOLDER_KEY:-abc}");

    assert_eq!(value, "abc");
    assert!(unresolved.is_empty());
    std::env::remove_var(VAR);
}

#[test]
fn default_placeholder_without_default_stays_unresolved() {
    let resolver = McpVariableResolver::empty();
    let (value, unresolved) = resolver.resolve_string("${API_KEY}");

    assert_eq!(value, "${API_KEY}");
    assert_eq!(unresolved, BTreeSet::from(["API_KEY".to_string()]));
}

#[test]
fn default_placeholder_unresolved_secret_vars_ignores_default() {
    use crate::{unresolved_secret_vars, McpManifest, McpServer};
    use std::collections::HashMap;

    let manifest = McpManifest {
            provenance: Default::default(),
        servers: HashMap::from([
            (
                "with_default".to_string(),
                McpServer {
                    server_type: Some("stdio".to_string()),
                    command: Some("${API_KEY:-}".to_string()),
                    args: None,
                    env: None,
                    cwd: None,
                    url: None,
                    headers: None,
                },
            ),
            (
                "without_default".to_string(),
                McpServer {
                    server_type: Some("stdio".to_string()),
                    command: Some("${API_KEY}".to_string()),
                    args: None,
                    env: None,
                    cwd: None,
                    url: None,
                    headers: None,
                },
            ),
        ]),
        inputs: None,
    };
    let unresolved = unresolved_secret_vars(&manifest);
    assert!(!unresolved.contains_key("with_default"));
    assert_eq!(
        unresolved.get("without_default"),
        Some(&BTreeSet::from(["API_KEY".to_string()]))
    );
}
