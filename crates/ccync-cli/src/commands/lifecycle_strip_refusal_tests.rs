//! Acceptance probes for `ccync backup --strip-secrets` refusals, write ordering, and notice.
//!
//! Seam: the refusal, ordering, and notice paths of `ccync backup --strip-secrets` in `cmd_backup`
//! within `crates/ccync-cli/src/commands/lifecycle.rs`.
//!
//! Strip secrets refusal and ordering acceptance contract:
//! - Ordering is the blocking part: resolve, validate, and transform the whole payload in memory,
//!   evaluate both refusals, and only then create the output directory and write.
//! - A refusal raised on a later file must not leave the plaintext-secret file already written.
//! - Two distinct fields in one server whose sanitized names collide refuse with exit `1`
//!   naming both fields, because a shared name would let one re-provisioned value overwrite two tokens.
//! - A `plugins.json` source URL carrying userinfo refuses with exit `1` naming the plugin id,
//!   reusing `roster::has_url_userinfo`.
//! - Either refusal writes nothing at all, including no output directory.
//! - On success, print the complete list of placeholder names introduced.

use super::lifecycle::cmd_backup;
use ccync_engine::ExitCode;
use std::fs;
use tempfile::TempDir;

struct EnvHomeGuard {
    saved: Vec<(String, Option<String>)>,
}

impl EnvHomeGuard {
    fn set(tmp: &std::path::Path) -> Self {
        let keys = [
            "USERPROFILE",
            "HOME",
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
        ];
        let mut saved = Vec::new();
        for key in keys {
            saved.push((key.to_string(), std::env::var(key).ok()));
            unsafe {
                std::env::set_var(key, tmp);
            }
        }
        Self { saved }
    }
}

impl Drop for EnvHomeGuard {
    fn drop(&mut self) {
        for (key, val) in &self.saved {
            unsafe {
                match val {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

#[test]
fn backup_strip_secrets_refuses_colliding_field_names_and_writes_nothing() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let out = TempDir::new().unwrap();
    let out_dir = out.path().join("backup_colliding");

    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    fs::write(
        ccync.join("config.json"),
        r#"{"selectedProjectionTargets":["claude"],"secrets":{"RAW_SECRET":"sensitive"}}"#,
    )
    .unwrap();
    fs::write(ccync.join("plugins.json"), r#"{"plugins":[]}"#).unwrap();
    // Two distinct fields under the same server whose sanitized names collide:
    // "api-key" -> "API_KEY"
    // "api_key" -> "API_KEY"
    fs::write(
        ccync.join("build").join("lock.json"),
        r#"{
  "_mcpServers": {
    "myserver": {
      "env": {
        "api-key": "secret_val_1",
        "api_key": "secret_val_2"
      }
    }
  }
}"#,
    )
    .unwrap();
    fs::write(ccync.join("AGENTS.md"), "# Rules\n").unwrap();

    let args = vec![
        "backup".to_string(),
        "--output".to_string(),
        out_dir.to_string_lossy().to_string(),
        "--strip-secrets".to_string(),
    ];
    let exit = cmd_backup(&args);

    assert_eq!(
        exit,
        ExitCode::Error,
        "refusal on colliding fields (${{CCYNC_SECRET_...}}) must exit 1, but got {:?}",
        exit
    );

    // Either refusal writes nothing at all, including no output directory.
    assert!(
        !out_dir.exists(),
        "EF-01 refusal must write nothing at all (including no output directory), but out_dir exists; CCYNC_SECRET_ collision refusal fired after plaintext write"
    );
}

#[test]
fn backup_strip_secrets_refuses_userinfo_url_and_writes_nothing() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let out = TempDir::new().unwrap();
    let out_dir = out.path().join("backup_userinfo");

    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    fs::write(
        ccync.join("config.json"),
        r#"{"selectedProjectionTargets":["claude"],"secrets":{"RAW_SECRET":"sensitive"}}"#,
    )
    .unwrap();
    // plugins.json with userinfo in source URL
    fs::write(
        ccync.join("plugins.json"),
        r#"{
  "plugins": [
    {
      "id": "leaked-credentials-plugin",
      "source": "https://user:pass123@github.com/example/repo.git"
    }
  ]
}"#,
    )
    .unwrap();
    fs::write(
        ccync.join("build").join("lock.json"),
        r#"{
  "_mcpServers": {
    "alpha": {
      "env": {
        "TOKEN": "val"
      }
    }
  }
}"#,
    )
    .unwrap();
    fs::write(ccync.join("AGENTS.md"), "# Rules\n").unwrap();

    let args = vec![
        "backup".to_string(),
        "--output".to_string(),
        out_dir.to_string_lossy().to_string(),
        "--strip-secrets".to_string(),
    ];
    let exit = cmd_backup(&args);

    assert_eq!(
        exit,
        ExitCode::Error,
        "plugins.json source URL carrying userinfo must refuse with exit 1 (ExitCode::Error)"
    );

    // Must write nothing at all, including no output directory
    assert!(
        !out_dir.exists(),
        "userinfo refusal must write nothing at all (including no output directory), but out_dir exists"
    );
}

fn run_mcp_server_backup(lock_payload: &str, output_name: &str) -> (ExitCode, bool) {
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let out = TempDir::new().unwrap();
    let out_dir = out.path().join(output_name);
    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    fs::write(
        ccync.join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();
    fs::write(ccync.join("plugins.json"), r#"{"plugins":[]}"#).unwrap();
    fs::write(ccync.join("build").join("lock.json"), lock_payload).unwrap();

    let args = vec![
        "backup".to_string(),
        "--output".to_string(),
        out_dir.to_string_lossy().to_string(),
        "--strip-secrets".to_string(),
    ];
    let exit = cmd_backup(&args);
    (exit, out_dir.exists())
}

#[test]
fn backup_strip_secrets_refuses_literal_mcp_command_args_and_url() {
    let _lock = crate::test_io_lock();
    for (field, payload) in [
        (
            "command",
            r#"{"_mcpServers":{"alpha":{"command":"TOKEN=literal-secret"}}}"#,
        ),
        (
            "args",
            r#"{"_mcpServers":{"alpha":{"command":"node","args":["--token","literal-secret"]}}}"#,
        ),
        (
            "args equals",
            r#"{"_mcpServers":{"alpha":{"command":"node","args":["--API-KEY=literal-secret"]}}}"#,
        ),
        (
            "url",
            r#"{"_mcpServers":{"alpha":{"url":"https://TOKEN=literal-secret@example.com/mcp"}}}"#,
        ),
    ] {
        let (exit, wrote_output) = run_mcp_server_backup(payload, "backup_literal");
        assert_eq!(exit, ExitCode::Error, "literal MCP {field} must refuse");
        assert!(
            !wrote_output,
            "literal MCP {field} refusal must write nothing"
        );
    }
}

#[test]
fn backup_strip_secrets_refuses_literal_secret_in_url_without_userinfo() {
    let _lock = crate::test_io_lock();
    let payload =
        r#"{"_mcpServers":{"alpha":{"url":"https://example.com/mcp?TOKEN=literal-secret"}}}"#;
    let (exit, wrote_output) = run_mcp_server_backup(payload, "backup_url_secret");
    assert_eq!(
        exit,
        ExitCode::Error,
        "a literal secret in url query text must refuse even without userinfo"
    );
    assert!(!wrote_output, "url secret refusal must write nothing");
}

#[test]
fn backup_strip_secrets_refuses_userinfo_embedded_in_an_arg() {
    let _lock = crate::test_io_lock();
    let payload = r#"{"_mcpServers":{"alpha":{"command":"node","args":["--endpoint=https://user:pass@example.com/mcp"]}}}"#;
    let (exit, wrote_output) = run_mcp_server_backup(payload, "backup_arg_userinfo");
    assert_eq!(
        exit,
        ExitCode::Error,
        "an arg carrying a URL with userinfo after a non-scheme prefix must refuse"
    );
    assert!(!wrote_output, "arg userinfo refusal must write nothing");
}

#[test]
fn backup_strip_secrets_allows_mcp_placeholders_and_non_secret_fields() {
    let _lock = crate::test_io_lock();
    let payload = r#"{
  "_mcpServers": {
    "alpha": {
      "command": "GH_TOKEN=${GH_TOKEN}",
      "args": ["--token", "${TOKEN}", "--api-key=${API_KEY}", "https://example.com/mcp"],
      "url": "https://example.com/mcp"
    }
  }
}"#;
    let (exit, wrote_output) = run_mcp_server_backup(payload, "backup_allowed");
    assert_eq!(exit, ExitCode::Success);
    assert!(
        wrote_output,
        "placeholder-only MCP fields must remain allowed"
    );
}

#[test]
fn backup_strip_secrets_ordering_in_memory_before_filesystem_creation() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let out = TempDir::new().unwrap();
    let out_dir = out.path().join("backup_order");

    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    fs::write(
        ccync.join("config.json"),
        r#"{"selectedProjectionTargets":["claude"],"secrets":{"LEAKABLE":"plaintext_secret_value"}}"#,
    )
    .unwrap();
    fs::write(
        ccync.join("plugins.json"),
        r#"{"plugins":[{"id":"bad_url_plugin","source":"https://token:xyz@example.com/repo.git"}]}"#,
    )
    .unwrap();
    fs::write(ccync.join("build").join("lock.json"), r#"{}"#).unwrap();

    let args = vec![
        "backup".to_string(),
        "--output".to_string(),
        out_dir.to_string_lossy().to_string(),
        "--strip-secrets".to_string(),
    ];
    let exit = cmd_backup(&args);

    assert_eq!(exit, ExitCode::Error);
    assert!(
        !out_dir.join("config.json").exists(),
        "plaintext config.json must never be written before evaluating refusals"
    );
    assert!(
        !out_dir.exists(),
        "output directory must not exist when refusal fires"
    );
}

fn run_declared_mcp_backup(payload: &[u8], output_name: &str) -> (ExitCode, bool, Vec<u8>) {
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let out = TempDir::new().unwrap();
    let out_dir = out.path().join(output_name);
    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    fs::write(
        ccync.join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();
    fs::write(ccync.join("plugins.json"), r#"{"plugins":[]}"#).unwrap();
    fs::write(ccync.join("mcp.json"), payload).unwrap();
    fs::write(ccync.join("build").join("lock.json"), r#"{}"#).unwrap();

    let args = vec![
        "backup".to_string(),
        "--output".to_string(),
        out_dir.to_string_lossy().to_string(),
        "--strip-secrets".to_string(),
    ];
    let exit = cmd_backup(&args);
    let source = fs::read(ccync.join("mcp.json")).unwrap();
    (exit, out_dir.exists(), source)
}

#[test]
fn mcp_input_backup_secret_scans_every_declared_mcp_carrier_before_output_creation() {
    let _lock = crate::test_io_lock();
    for (field, server) in [
        ("command", r#""command":"TOKEN=literal-secret""#),
        (
            "args",
            r#""command":"node","args":["--token","literal-secret"]"#,
        ),
        (
            "url",
            r#""url":"https://example.com/mcp?TOKEN=literal-secret""#,
        ),
        (
            "env",
            r#""command":"node","env":{"TOKEN":"literal-secret"}"#,
        ),
        (
            "headers",
            r#""url":"https://example.com/mcp","headers":{"X-API-KEY":"literal-secret"}"#,
        ),
    ] {
        let payload = format!(r#"{{"mcpServers":{{"alpha":{{{server}}}}}}}"#);
        let (exit, wrote_output, _) =
            run_declared_mcp_backup(payload.as_bytes(), "declared_refusal");
        assert_eq!(exit, ExitCode::Error, "declared MCP {field} must refuse");
        assert!(
            !wrote_output,
            "declared MCP {field} refusal must not create output"
        );
    }
}

#[test]
fn mcp_input_backup_secret_scans_declared_mcp_without_changing_safe_source_bytes() {
    let _lock = crate::test_io_lock();
    let payload = b"{\n  \"mcpServers\": {\n    \"alpha\": {\n      \"command\": \"node\",\n      \"args\": [\"--mode\", \"safe\"],\n      \"env\": {\"MODE\": \"safe\"},\n      \"headers\": {\"Accept\": \"application/json\"}\n    }\n  }\n}\n";
    let (exit, wrote_output, source) = run_declared_mcp_backup(payload, "declared_safe");
    assert_eq!(exit, ExitCode::Success);
    assert!(wrote_output);
    assert_eq!(source, payload, "scanning must preserve source bytes");
}

#[test]
fn backup_preserves_selected_input_bytes_and_refuses_unknown_values() {
    let _lock = crate::test_io_lock();
    for personal in [false, true] {
        let home = TempDir::new().unwrap();
        let _env = EnvHomeGuard::set(home.path());
        let ccync = home.path().join(".ccync");
        fs::create_dir_all(&ccync).unwrap();
        fs::write(ccync.join("config.json"), "{}").unwrap();
        let input = if personal {
            home.path().join(".agents/mcp.json")
        } else {
            ccync.join("mcp.json")
        };
        fs::create_dir_all(input.parent().unwrap()).unwrap();
        let key = if personal {
            "ccyncMcpServers"
        } else {
            "mcpServers"
        };
        let safe = format!("{{\n  \"{key}\": {{\"alpha\": {{\"command\": \"node\"}}}}\n}}\n");
        fs::write(&input, &safe).unwrap();
        let output = home.path().join("backup");
        let args = vec![
            "backup".into(),
            "--output".into(),
            output.to_string_lossy().into_owned(),
        ];
        assert_eq!(cmd_backup(&args), ExitCode::Success);
        assert_eq!(fs::read(output.join("mcp.json")).unwrap(), safe.as_bytes());
        fs::write(
            &input,
            format!("{{\"{key}\":{{\"alpha\":{{\"command\":\"node\",\"args\":[\"opaque\"]}}}}}}"),
        )
        .unwrap();
        let refused = home.path().join("refused");
        let args = vec![
            "backup".into(),
            "--output".into(),
            refused.to_string_lossy().into_owned(),
        ];
        assert_eq!(cmd_backup(&args), ExitCode::Error);
        assert!(!refused.exists());
        fs::write(
            &input,
            format!("{{\"{key}\":{{\"alpha\":{{\"command\":\"node\",\"env\":{{\"TOKEN\":\"literal-secret\"}}}}}}}}"),
        )
        .unwrap();
        assert_eq!(cmd_backup(&args), ExitCode::Error);
        assert!(!refused.exists());
        let references = format!("{{\"{key}\":{{\"alpha\":{{\"command\":\"node\",\"args\":[\"TOKEN=${{ONE}}${{TWO}}\"]}}}}}}");
        fs::write(&input, &references).unwrap();
        assert_eq!(cmd_backup(&args), ExitCode::Success);
        assert_eq!(
            fs::read(refused.join("mcp.json")).unwrap(),
            references.as_bytes()
        );
    }
}

#[test]
fn backup_exports_migrated_route_and_refuses_reserved_config_field() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let ccync = home.path().join(".ccync");
    fs::create_dir_all(&ccync).unwrap();
    fs::write(ccync.join("config.json"), "{}").unwrap();
    let managed = ccync.join("build/mcp/managed.json");
    let route = mcp::MigratedRoute::new(
        ccync_foundation::mcp::DeclarationIdentity {
            source: "user".into(),
            server: "alpha".into(),
        },
        vec!["codex".into(), "claude".into()],
    );
    mcp::commit_restored_routes(&managed, &[route]).unwrap();
    let output = home.path().join("backup");
    let args = vec![
        "backup".into(),
        "--output".into(),
        output.to_string_lossy().into_owned(),
    ];
    assert_eq!(cmd_backup(&args), ExitCode::Success);
    let config: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("config.json")).unwrap()).unwrap();
    assert_eq!(
        config["_ccyncBackupMcpRoutes"],
        serde_json::json!({"version": 1, "routes": [{"source":"user", "server":"alpha", "hosts":["claude", "codex"]}]})
    );
    fs::write(ccync.join("config.json"), r#"{"_ccyncBackupMcpRoutes":{}}"#).unwrap();
    let refused = home.path().join("refused");
    let args = vec![
        "backup".into(),
        "--output".into(),
        refused.to_string_lossy().into_owned(),
    ];
    assert_eq!(cmd_backup(&args), ExitCode::Error);
    assert!(!refused.exists());
}

#[test]
fn backup_preserves_lockfile_references_and_rejects_placeholder_collisions() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    fs::write(ccync.join("config.json"), "{}").unwrap();
    let lock = ccync.join("build/lock.json");
    fs::write(&lock, r#"{"_mcpServers":{"alpha":{"command":"${ONE}${TWO}","args":["TOKEN=${ONE}${TWO}"],"env":{"TOKEN":"TOKEN=${ONE}${TWO}","KEY":"literal"}}}}"#).unwrap();
    let output = home.path().join("backup");
    let args = vec![
        "backup".into(),
        "--output".into(),
        output.to_string_lossy().into_owned(),
    ];
    assert_eq!(cmd_backup(&args), ExitCode::Success);
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("plugins.lock.json")).unwrap()).unwrap();
    assert_eq!(
        saved["_mcpServers"]["alpha"]["env"]["TOKEN"],
        "TOKEN=${ONE}${TWO}"
    );
    assert_eq!(
        saved["_mcpServers"]["alpha"]["env"]["KEY"],
        "${CCYNC_SECRET_ALPHA_KEY}"
    );

    fs::write(&lock, r#"{"_mcpServers":{"alpha":{"env":{"KEY":"literal","OTHER":"${CCYNC_SECRET_ALPHA_KEY}"}}}}"#).unwrap();
    let refused = home.path().join("refused");
    let args = vec![
        "backup".into(),
        "--output".into(),
        refused.to_string_lossy().into_owned(),
    ];
    assert_eq!(cmd_backup(&args), ExitCode::Error);
    assert!(!refused.exists());
}
