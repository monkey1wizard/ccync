//! Regression coverage for tolerant MCP secret resolution in the projection core.

use super::lifecycle::{
    mcp_secret_skip_lines, render_mcp_inspect, run_projection_core, McpInspection,
};
use ccync_engine::ExitCode;
use ccync_foundation::config::CcyncConfig;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

struct EnvGuard(Vec<(String, Option<String>)>);

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

fn isolate_home(tmp: &Path) -> EnvGuard {
    std::fs::create_dir_all(tmp.join(".agents")).unwrap();
    std::fs::write(tmp.join(".agents/mcp.json"), r#"{"ccyncMcpServers":{},"hostRoutes":[{"source":"adopted","server":"alpha","hosts":["claude","copilot","opencode","codex","agy"]},{"source":"adopted","server":"healthy","hosts":["claude","copilot","opencode","codex","agy"]}]}"#).unwrap();
    let keys = [
        "USERPROFILE",
        "HOME",
        "APPDATA",
        "LOCALAPPDATA",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ];
    let mut saved = Vec::new();
    for key in keys {
        saved.push((key.to_string(), std::env::var(key).ok()));
        unsafe {
            std::env::set_var(key, tmp);
        }
    }
    EnvGuard(saved)
}

#[test]
fn mcp_secret_skip_lines_are_empty_without_skips() {
    assert!(mcp_secret_skip_lines("ccync sync", &BTreeMap::new()).is_empty());
}

#[test]
fn mcp_secret_skip_lines_are_deterministic_and_byte_exact() {
    let skips = BTreeMap::from([
        ("zeta".to_string(), BTreeSet::from(["TOKEN".to_string()])),
        (
            "alpha".to_string(),
            BTreeSet::from(["API_KEY".to_string(), "PASSWORD".to_string()]),
        ),
    ]);

    assert_eq!(
        mcp_secret_skip_lines("ccync sync", &skips),
        vec![
            "ccync sync: MCP server failed: alpha — missing secret API_KEY, PASSWORD — add to config.json#secrets or the environment".to_string(),
            "ccync sync: MCP server failed: zeta — missing secret TOKEN — add to config.json#secrets or the environment".to_string(),
        ]
    );
}

#[test]
fn sync_with_unresolved_secret_placeholder_skips_server_and_projects_rest() {
    let _guard = crate::test_io_lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let _saved = isolate_home(tmp.path());

    let ccync = tmp.path().join(".ccync");
    std::fs::create_dir_all(ccync.join("build")).unwrap();
    std::fs::write(
        ccync.join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();
    std::fs::write(ccync.join("AGENTS.md"), "# Team rules\n").unwrap();
    std::fs::write(
        ccync.join("build").join("lock.json"),
        r#"{"_mcpServers":{"alpha":{"command":"npx","env":{"API_TOKEN":"${CCYNC_SECRET_ALPHA_API_TOKEN}"}},"healthy":{"type":"http","url":"https://mcp.example.test/healthy"}},"resolvedPlugins":[]}"#,
    )
    .unwrap();

    let claude_target = tmp.path().join(".claude").join("CLAUDE.md");
    std::fs::create_dir_all(claude_target.parent().unwrap()).unwrap();
    std::fs::write(
        &claude_target,
        "user-authored heading\n<!-- ccync:managed:begin -->\nold\n<!-- ccync:managed:end -->\n",
    )
    .unwrap();

    let config = CcyncConfig::load();
    let result = run_projection_core(&config, "ccync sync");

    let render_dir_exists = ccync.join("build").join("render").is_dir();
    let claude_after = std::fs::read_to_string(&claude_target).unwrap();
    let claude_mcp: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(tmp.path().join(".claude.json")).unwrap())
            .unwrap();

    assert_eq!(
        result,
        ExitCode::Success,
        "a secret-only server skip must not select a non-zero exit"
    );
    assert!(
        render_dir_exists,
        "the canonical render must complete despite the skipped server"
    );
    assert!(
        !claude_after.contains("<!-- ccync:managed:begin -->"),
        "the Machine and managed-block phases must continue"
    );
    assert!(claude_after.starts_with("user-authored heading\n"));
    assert_eq!(
        claude_mcp["mcpServers"]["healthy"]["url"], "https://mcp.example.test/healthy",
        "the healthy server must reach the live host"
    );
    assert!(
        claude_mcp["mcpServers"].get("alpha").is_none(),
        "the unresolved server must not reach the live host"
    );
}

#[test]
fn sync_with_unresolved_secret_and_malformed_host_file_still_exits_error() {
    let _guard = crate::test_io_lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let _saved = isolate_home(tmp.path());

    let ccync = tmp.path().join(".ccync");
    std::fs::create_dir_all(ccync.join("build")).unwrap();
    std::fs::write(
        ccync.join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();
    std::fs::write(ccync.join("AGENTS.md"), "# Team rules\n").unwrap();
    std::fs::write(
        ccync.join("build").join("lock.json"),
        r#"{"_mcpServers":{"alpha":{"command":"npx","env":{"API_TOKEN":"${CCYNC_SECRET_ALPHA_API_TOKEN}"}},"healthy":{"type":"http","url":"https://mcp.example.test/healthy"}},"resolvedPlugins":[]}"#,
    )
    .unwrap();

    let claude_target = tmp.path().join(".claude").join("CLAUDE.md");
    std::fs::create_dir_all(claude_target.parent().unwrap()).unwrap();
    std::fs::write(
        &claude_target,
        "user-authored heading\n<!-- ccync:managed:begin -->\nold\n<!-- ccync:managed:end -->\n",
    )
    .unwrap();

    // Malformed .claude.json so that reading live host configuration fails
    std::fs::write(tmp.path().join(".claude.json"), "{ invalid json syntax").unwrap();

    let config = CcyncConfig::load();
    let result = run_projection_core(&config, "ccync sync");

    assert_eq!(
        result,
        ExitCode::Error,
        "a malformed host configuration must still cause run_projection_core to exit Error"
    );
    let claude_after = std::fs::read_to_string(&claude_target).unwrap();
    assert!(claude_after.starts_with("user-authored heading\n"));
    assert!(!claude_after.contains("<!-- ccync:managed:begin -->"));
}

#[test]
fn sync_with_server_unresolved_in_both_prior_and_desired_yields_one_skip_entry() {
    let _guard = crate::test_io_lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let _saved = isolate_home(tmp.path());

    let ccync = tmp.path().join(".ccync");
    let managed_mcp_dir = ccync.join("build").join("mcp");
    std::fs::create_dir_all(&managed_mcp_dir).unwrap();
    std::fs::write(
        ccync.join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();
    std::fs::write(ccync.join("AGENTS.md"), "# Team rules\n").unwrap();

    // Prior managed.json has alpha with an unresolved secret placeholder
    std::fs::write(
        managed_mcp_dir.join("managed.json"),
        r#"{"servers":{"alpha":{"command":"npx","env":{"API_TOKEN":"${CCYNC_SECRET_ALPHA_API_TOKEN}"}}}}"#,
    )
    .unwrap();

    // Desired lock.json also has alpha with the unresolved secret placeholder, plus a healthy server
    std::fs::write(
        ccync.join("build").join("lock.json"),
        r#"{"_mcpServers":{"alpha":{"command":"npx","env":{"API_TOKEN":"${CCYNC_SECRET_ALPHA_API_TOKEN}"}},"healthy":{"type":"http","url":"https://mcp.example.test/healthy"}},"resolvedPlugins":[]}"#,
    )
    .unwrap();

    let claude_target = tmp.path().join(".claude").join("CLAUDE.md");
    std::fs::create_dir_all(claude_target.parent().unwrap()).unwrap();
    std::fs::write(
        &claude_target,
        "user-authored heading\n<!-- ccync:managed:begin -->\nold\n<!-- ccync:managed:end -->\n",
    )
    .unwrap();

    let config = CcyncConfig::load();
    let result = run_projection_core(&config, "ccync sync");

    let claude_mcp: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(tmp.path().join(".claude.json")).unwrap())
            .unwrap();
    let claude_after = std::fs::read_to_string(&claude_target).unwrap();
    assert!(claude_after.starts_with("user-authored heading\n"));
    assert!(!claude_after.contains("<!-- ccync:managed:begin -->"));

    // Verify directly that run_mcp_projection with both prior and desired secret skips
    // reports only the desired skip (exactly one entry for alpha).
    let managed_json = managed_mcp_dir.join("managed.json");
    let layout = ccync_foundation::paths::EffectiveLayout::load_or_legacy().unwrap();
    let report = mcp::run_mcp_projection_at(&managed_json, &layout.machine_config_path(), || {
        // canonical render overwrites managed.json with desired manifest
        std::fs::write(
            &managed_json,
            r#"{"servers":{"alpha":{"command":"npx","env":{"API_TOKEN":"${CCYNC_SECRET_ALPHA_API_TOKEN}"}},"healthy":{"type":"http","url":"https://mcp.example.test/healthy"}}}"#,
        )
        .map_err(mcp::McpUpdateError::Io)
    })
    .unwrap();

    let skip_lines = mcp_secret_skip_lines("ccync sync", &report.secret_skips);

    assert_eq!(
        result,
        ExitCode::Success,
        "a secret-only skip in prior and desired must still exit Success"
    );
    assert_eq!(
        claude_mcp["mcpServers"]["healthy"]["url"], "https://mcp.example.test/healthy",
        "the healthy server must reach the live host"
    );
    assert!(
        claude_mcp["mcpServers"].get("alpha").is_none(),
        "the unresolved server must not reach the live host"
    );
    assert_eq!(
        report.secret_skips.len(),
        1,
        "server unresolved in both prior and desired must produce exactly one skip entry"
    );
    assert_eq!(
        skip_lines,
        vec![
            "ccync sync: MCP server failed: alpha — missing secret CCYNC_SECRET_ALPHA_API_TOKEN — add to config.json#secrets or the environment".to_string()
        ],
        "exactly one skip line must be rendered for the skipped server"
    );
}

#[test]
fn dry_run_lists_prior_secret_skips_from_last_rendered_manifest() {
    let report = mcp::McpInspectReport {
        prior_secret_skips: BTreeMap::from([(
            "alpha".to_string(),
            BTreeSet::from(["API_TOKEN".to_string(), "PASSWORD".to_string()]),
        )]),
        ..Default::default()
    };

    let lines = render_mcp_inspect(&McpInspection::Attempted(Ok(report)));

    let skip_lines: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("from last rendered manifest"))
        .collect();
    assert_eq!(skip_lines.len(), 1);
    assert!(skip_lines[0].contains("alpha"));
    assert!(skip_lines[0].contains("API_TOKEN"));
    assert!(skip_lines[0].contains("PASSWORD"));
}

#[test]
fn dry_run_does_not_list_prior_secret_skips_when_none_exist() {
    let lines = render_mcp_inspect(&McpInspection::Attempted(Ok(Default::default())));

    assert!(!lines
        .iter()
        .any(|line| line.contains("from last rendered manifest")));
}

#[test]
fn lifecycle_source_no_longer_contains_old_lockout_test() {
    let lifecycle_source = include_str!("lifecycle.rs");
    let old_test_name = concat!(
        "sync_with_unresolved_secret_placeholder_",
        "stops_before_machine_and_managed_block"
    );
    assert!(
        !lifecycle_source.contains(old_test_name),
        "lifecycle.rs must no longer contain the obsolete lockout test"
    );
}
