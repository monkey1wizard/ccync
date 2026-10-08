use crate::{inspect_mcp_projection, run_mcp_projection, McpInspectReport, McpProjectionReport};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;

#[test]
fn projection_report_carries_secret_skips() {
    let report = McpProjectionReport {
        hosts_updated: Vec::new(),
        recovery_actions: Vec::new(),
        secret_skips: BTreeMap::from([(
            "github".to_string(),
            BTreeSet::from(["GITHUB_TOKEN".to_string()]),
        )]),
        host_failures: Vec::new(),
    };

    assert_eq!(
        report.secret_skips["github"],
        BTreeSet::from(["GITHUB_TOKEN".to_string()])
    );
}

#[test]
fn inspect_report_carries_prior_secret_skips() {
    let _guard = crate::test_support::env_guard();
    let temp = tempfile::tempdir().unwrap();
    let managed_json = temp.path().join("managed.json");
    fs::write(
        &managed_json,
        serde_json::to_string(&json!({
            "servers": {
                "github": {
                    "type": "stdio",
                    "command": "github",
                    "env": {"GITHUB_TOKEN": "${CCYNC_REPORT_SKIP_TEST_TOKEN}"}
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let report = inspect_mcp_projection(&managed_json).unwrap();

    assert_eq!(
        report.prior_secret_skips["github"],
        BTreeSet::from(["CCYNC_REPORT_SKIP_TEST_TOKEN".to_string()])
    );
}

#[test]
fn inspect_report_default_has_no_prior_secret_skips() {
    let report = McpInspectReport::default();
    assert!(report.prior_secret_skips.is_empty());
}

#[test]
fn run_mcp_projection_carries_desired_secret_skips() {
    let _guard = crate::test_support::env_guard();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let appdata = tmp.path().join("appdata");
    let xdg = tmp.path().join("xdg-config");
    let managed_json = tmp.path().join("mcp").join("managed.json");
    fs::create_dir_all(managed_json.parent().unwrap()).unwrap();
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::create_dir_all(home.join(".copilot")).unwrap();
    fs::create_dir_all(home.join(".gemini").join("config")).unwrap();
    fs::create_dir_all(appdata.join("opencode")).unwrap();
    fs::create_dir_all(xdg.join("opencode")).unwrap();
    fs::write(home.join(".claude.json"), r#"{"mcpServers":{}}"#).unwrap();
    fs::write(
        home.join(".copilot").join("mcp-config.json"),
        r#"{"mcpServers":{}}"#,
    )
    .unwrap();
    fs::write(xdg.join("opencode").join("opencode.json"), r#"{"mcp":{}}"#).unwrap();
    fs::write(
        appdata.join("opencode").join("opencode.json"),
        r#"{"mcp":{}}"#,
    )
    .unwrap();
    fs::write(
        home.join(".gemini").join("config").join("mcp_config.json"),
        r#"{"mcpServers":{}}"#,
    )
    .unwrap();
    fs::write(home.join(".codex").join("config.toml"), "").unwrap();

    let prior_userprofile = std::env::var_os("USERPROFILE");
    let prior_home = std::env::var_os("HOME");
    let prior_appdata = std::env::var_os("APPDATA");
    let prior_xdg = std::env::var_os("XDG_CONFIG_HOME");
    #[cfg(windows)]
    unsafe {
        std::env::set_var("USERPROFILE", &home);
        std::env::set_var("HOME", &home);
        std::env::set_var("APPDATA", &appdata);
        std::env::remove_var("XDG_CONFIG_HOME");
    }
    #[cfg(not(windows))]
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", &xdg);
        std::env::remove_var("APPDATA");
    }

    let result = run_mcp_projection(&managed_json, || {
        let desired = json!({
            "servers": {
                "gh": {
                    "type": "stdio",
                    "command": "gh",
                    "env": {"GH_TOKEN": "${CCYNC_REPORT_SKIP_TEST_GH_TOKEN}"}
                }
            }
        });
        fs::write(&managed_json, serde_json::to_string(&desired).unwrap())?;
        Ok(())
    });

    #[cfg(windows)]
    unsafe {
        match prior_userprofile {
            Some(v) => std::env::set_var("USERPROFILE", v),
            None => std::env::remove_var("USERPROFILE"),
        }
        match prior_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match prior_appdata {
            Some(v) => std::env::set_var("APPDATA", v),
            None => std::env::remove_var("APPDATA"),
        }
        match prior_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }
    #[cfg(not(windows))]
    unsafe {
        match prior_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match prior_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match prior_appdata {
            Some(v) => std::env::set_var("APPDATA", v),
            None => std::env::remove_var("APPDATA"),
        }
    }

    let report = result.unwrap();
    assert_eq!(
        report.secret_skips.get("gh"),
        Some(&BTreeSet::from([
            "CCYNC_REPORT_SKIP_TEST_GH_TOKEN".to_string()
        ])),
        "run_mcp_projection must carry the desired-manifest secret skip: {:?}",
        report.secret_skips
    );
    let claude_json = fs::read_to_string(home.join(".claude.json")).unwrap();
    assert!(
        !claude_json.contains("\"gh\""),
        "skipped desired server must not reach host render: {claude_json}"
    );
}
