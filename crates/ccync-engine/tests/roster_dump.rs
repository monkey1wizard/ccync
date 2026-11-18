use ccync_engine::roster::dump_roster;
use ccync_engine::roster::parse_roster;
use serde_json::json;
use tempfile::tempdir;

#[test]
fn dump_roster_skips_local_credential_and_pinless_entries() {
    let dir = tempdir().expect("tempdir");
    let out_path = dir.path().join("ccync.roster.json");

    let catalog = json!({
        "schemaVersion": 1,
        "plugins": [
            {
                "pluginId": "qualifying-plugin",
                "displayName": "Qualifying Plugin",
                "sourceType": "curated-upstream",
                "installStrategy": "personal-git-clone",
                "source": "https://github.com/example/qualifying.git",
                "pin": "0123456789abcdef0123456789abcdef01234567",
                "held": true,
                "selectedProjectionTargets": ["claude"]
            },
            {
                "pluginId": "local-rel-plugin",
                "displayName": "Local Rel Plugin",
                "source": "./local/path",
                "pin": "0123456789abcdef0123456789abcdef01234567"
            },
            {
                "pluginId": "local-abs-plugin",
                "displayName": "Local Abs Plugin",
                "source": "/absolute/local/path",
                "pin": "0123456789abcdef0123456789abcdef01234567"
            },
            {
                "pluginId": "credential-plugin",
                "displayName": "Credential Plugin",
                "source": "https://user:token@github.com/example/repo.git",
                "pin": "0123456789abcdef0123456789abcdef01234567"
            },
            {
                "pluginId": "pinless-plugin",
                "displayName": "Pinless Plugin",
                "source": "https://github.com/example/pinless.git"
            },
            {
                "pluginId": "short-pin-plugin",
                "displayName": "Short Pin Plugin",
                "source": "https://github.com/example/short.git",
                "pin": "1234567"
            }
        ]
    });

    let report = dump_roster(&catalog, &out_path).expect("dump_roster should succeed");

    assert_eq!(report.written.len(), 1, "expected 1 written plugin");
    assert_eq!(report.written, vec!["qualifying-plugin".to_string()]);

    assert_eq!(report.skipped.len(), 5, "expected 5 skipped plugins");
    assert_eq!(
        report.skipped,
        vec![
            (
                "local-rel-plugin".to_string(),
                "source is not a remote URL".to_string()
            ),
            (
                "local-abs-plugin".to_string(),
                "source is not a remote URL".to_string()
            ),
            (
                "credential-plugin".to_string(),
                "source carries URL userinfo".to_string()
            ),
            (
                "pinless-plugin".to_string(),
                "no full 40-hex pin recorded".to_string()
            ),
            (
                "short-pin-plugin".to_string(),
                "no full 40-hex pin recorded".to_string()
            ),
        ]
    );

    let content = std::fs::read_to_string(&out_path).expect("read roster output");
    let doc: serde_json::Value =
        serde_json::from_str(&content).expect("valid json in roster output");
    assert_eq!(doc["schemaVersion"], 1);

    let plugins = doc["plugins"].as_array().expect("plugins must be an array");
    assert_eq!(plugins.len(), 1);

    let entry = &plugins[0];
    let obj = entry.as_object().expect("entry must be an object");
    assert_eq!(obj.len(), 6, "written entry must have exactly 6 keys");

    assert_eq!(entry["pluginId"], "qualifying-plugin");
    assert_eq!(entry["displayName"], "Qualifying Plugin");
    assert_eq!(entry["sourceType"], "curated-upstream");
    assert_eq!(entry["installStrategy"], "personal-git-clone");
    assert_eq!(entry["source"], "https://github.com/example/qualifying.git");
    assert_eq!(entry["pin"], "0123456789abcdef0123456789abcdef01234567");

    assert_eq!(entry.get("held"), None, "held must not be in roster output");
    assert_eq!(
        entry.get("selectedProjectionTargets"),
        None,
        "selectedProjectionTargets must not be in roster output"
    );
}

#[test]
fn dump_and_parse_roster_preserves_git_subpath_selector() {
    let dir = tempdir().expect("tempdir");
    let out_path = dir.path().join("ccync.roster.json");
    let selector = "https://github.com/o/r//plugins/x";
    let catalog = json!({
        "schemaVersion": 1,
        "plugins": [{
            "pluginId": "x",
            "displayName": "X",
            "source": selector,
            "pin": "0123456789abcdef0123456789abcdef01234567"
        }]
    });

    dump_roster(&catalog, &out_path).expect("dump_roster should succeed");

    let content = std::fs::read_to_string(&out_path).expect("read roster output");
    let dumped: serde_json::Value = serde_json::from_str(&content).expect("valid roster output");
    assert_eq!(dumped["plugins"][0]["source"], selector);

    let entries = parse_roster(&dumped).expect("dumped roster should parse");
    assert_eq!(entries[0].source, selector);
}
