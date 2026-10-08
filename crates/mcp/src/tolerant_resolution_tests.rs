use crate::{is_secret_name, McpManifest, McpServer, McpVariableResolver};
use std::collections::{BTreeMap, BTreeSet, HashMap};

fn server(command: &str, args: Vec<&str>, env: Vec<(&str, &str)>) -> McpServer {
    McpServer {
        server_type: Some("stdio".to_string()),
        command: Some(command.to_string()),
        args: Some(args.into_iter().map(str::to_string).collect()),
        env: Some(
            env.into_iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        ),
        cwd: Some("${CWD}".to_string()),
        url: None,
        headers: None,
    }
}

#[test]
fn unresolved_secret_skips_only_server_and_collects_all_fields() {
    let resolver =
        McpVariableResolver::new(HashMap::from([("KNOWN".to_string(), "value".to_string())]));
    let manifest = McpManifest {
        servers: HashMap::from([
            (
                "bad".to_string(),
                server(
                    "${API_KEY}",
                    vec!["${TOKEN}", "${OTHER}"],
                    vec![("x", "${PASSWORD}")],
                ),
            ),
            (
                "good".to_string(),
                server("${KNOWN}", vec!["${OTHER}"], vec![]),
            ),
        ]),
        inputs: None,
    };

    let (resolved, skipped) = resolver.resolve_manifest(&manifest);

    assert!(resolved.servers.contains_key("good"));
    assert!(!resolved.servers.contains_key("bad"));
    assert_eq!(
        skipped["bad"],
        BTreeSet::from([
            "API_KEY".to_string(),
            "PASSWORD".to_string(),
            "TOKEN".to_string(),
        ])
    );
    assert_eq!(resolved.servers["good"].command.as_deref(), Some("value"));
    assert_eq!(
        resolved.servers["good"].args.as_deref(),
        Some(["${OTHER}".to_string()].as_slice())
    );
}

#[test]
fn secret_name_rule_is_case_insensitive_and_does_not_overmatch_names() {
    assert!(is_secret_name("api_key"));
    assert!(is_secret_name("x-secret"));
    assert!(is_secret_name("gh_token"));
    assert!(is_secret_name("db_password"));
    assert!(!is_secret_name("GITHUB_PAT"));
    assert!(!is_secret_name("HOME_DIR"));
}

#[test]
fn unresolved_secret_vars_uses_environment_map_and_is_deterministic() {
    let manifest = McpManifest {
        servers: BTreeMap::from([
            ("z-server".to_string(), server("${Z_TOKEN}", vec![], vec![])),
            ("a-server".to_string(), server("${A_KEY}", vec![], vec![])),
        ])
        .into_iter()
        .collect(),
        inputs: None,
    };

    let resolver = McpVariableResolver::empty();
    let (_, skipped) = resolver.resolve_manifest(&manifest);
    assert_eq!(
        skipped.keys().collect::<Vec<_>>(),
        vec!["a-server", "z-server"]
    );
}
