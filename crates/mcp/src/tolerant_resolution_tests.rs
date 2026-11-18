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
        provenance: Default::default(),
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
    assert!(is_secret_name("GITHUB_PAT"));
    assert!(!is_secret_name("PATH"));
    assert!(!is_secret_name("PATTERN"));
    assert!(!is_secret_name("XPATH"));
    assert!(!is_secret_name("HOME_DIR"));
}

#[test]
fn unresolved_secret_vars_uses_environment_map_and_is_deterministic() {
    let manifest = McpManifest {
        provenance: Default::default(),
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

#[test]
fn variable_reports_collect_both_kinds_across_fields() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.json");
    std::fs::write(&config, "{}").unwrap();
    let manifest: McpManifest = serde_json::from_str(
        r#"{
        "servers": {
            "mixed": {
                "command": "${CCYNC_MISSING_PAT}",
                "args": ["${CCYNC_MISSING_PATH}", "${CCYNC_MISSING_PAT}"],
                "env": {"a": "${CCYNC_MISSING_PATTERN}"},
                "url": "${CCYNC_MISSING_XPATH}",
                "headers": {"a": "${CCYNC_MISSING_TOKEN}"}
            },
            "healthy": {"command": "run"}
        }
    }"#,
    )
    .unwrap();
    let reports = crate::server_variable_reports_at(&manifest, &config);
    assert_eq!(reports.len(), 1);
    let report = &reports[0];
    assert_eq!(report.server(), "mixed");
    assert_eq!(
        report.secret_names(),
        &BTreeSet::from(["CCYNC_MISSING_PAT".into(), "CCYNC_MISSING_TOKEN".into()])
    );
    assert_eq!(
        report.ordinary_names(),
        &BTreeSet::from([
            "CCYNC_MISSING_PATH".into(),
            "CCYNC_MISSING_PATTERN".into(),
            "CCYNC_MISSING_XPATH".into()
        ])
    );
    assert_eq!(
        report.field_locations(),
        &BTreeSet::from([
            "command".into(),
            "args".into(),
            "env".into(),
            "url".into(),
            "headers".into()
        ])
    );
    let (resolved, skipped) = McpVariableResolver::empty().resolve_manifest(&manifest);
    assert!(resolved.servers.contains_key("healthy"));
    assert!(!resolved.servers.contains_key("mixed"));
    assert_eq!(&skipped["mixed"], report.secret_names());
}
