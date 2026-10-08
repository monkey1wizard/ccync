//! Acceptance probes for `ccync backup --strip-secrets`.
//!
//! Seam: `ccync backup --output <dir> --strip-secrets` in `cmd_backup`
//! within `crates/ccync-cli/src/commands/lifecycle.rs`.
//!
//! Strip secrets acceptance contract:
//! - Plain `ccync backup` applies the same secret stripping as the deprecated flag.
//! - With `--strip-secrets`:
//!   - Omit the top-level `config.json#secrets` map.
//!   - Replace every string under `plugins.lock.json#_mcpServers.<server>.env.<key>`
//!     and `.headers.<key>` that is not already a `${VAR}` placeholder with
//!     `${CCYNC_SECRET_<SERVER>_<KEY>}`.
//!   - Server and key names are upper-cased and sanitized to `[A-Z0-9_]`.
//!   - Existing `${...}` values are left untouched.
//!   - Untouched keys keep their order and formatting.
//!   - `AGENTS.md` and `plugins.json` are preserved.

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
fn lifecycle_seam_backup_registers_strip_secrets_flag() {
    let lifecycle_source = include_str!("lifecycle.rs");

    let (_, backup_fn) = lifecycle_source
        .split_once("pub(crate) fn cmd_backup(")
        .expect("lifecycle.rs must define cmd_backup");
    let (backup_body, _) = backup_fn
        .split_once("pub(crate) fn cmd_restore(")
        .expect("lifecycle.rs must define cmd_restore after cmd_backup");

    assert!(
        backup_body.contains(r#""--strip-secrets""#),
        "--strip-secrets is rejected as unknown today: cmd_backup must include --strip-secrets in allowed_flags"
    );
}

#[test]
fn backup_with_strip_secrets_omits_config_secrets_and_transforms_mcp_tokens() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let out = TempDir::new().unwrap();
    let out_dir = out.path().join("backup");

    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    fs::write(
        ccync.join("config.json"),
        r#"{
  "selectedProjectionTargets": ["claude"],
  "secrets": {
    "API_TOKEN": "supersecret123"
  }
}"#,
    )
    .unwrap();
    fs::write(ccync.join("plugins.json"), r#"{"plugins":[{"id":"demo"}]}"#).unwrap();
    fs::write(
        ccync.join("build").join("lock.json"),
        r#"{
  "_mcpServers": {
    "alpha": {
      "env": {
        "API_TOKEN": "raw_secret_value",
        "PRESERVED": "${MY_TOKEN}"
      },
      "headers": {
        "authorization": "Bearer token_xyz"
      }
    }
  }
}"#,
    )
    .unwrap();
    fs::write(ccync.join("AGENTS.md"), "# Team Rules\n").unwrap();

    let args = vec![
        "backup".to_string(),
        "--output".to_string(),
        out_dir.to_string_lossy().to_string(),
        "--strip-secrets".to_string(),
    ];
    let exit = cmd_backup(&args);

    assert_eq!(
        exit,
        ExitCode::Success,
        "--strip-secrets flag must be accepted and backup must succeed, but exited with {:?}",
        exit
    );

    // Verify config.json: secrets omitted
    let config_content = fs::read_to_string(out_dir.join("config.json")).unwrap();
    let config_val: serde_json::Value = serde_json::from_str(&config_content).unwrap();
    assert!(
        config_val.get("secrets").is_none(),
        "config.json#secrets map must be omitted under --strip-secrets"
    );
    assert_eq!(
        config_val.get("selectedProjectionTargets").unwrap(),
        &serde_json::json!(["claude"])
    );

    // Verify plugins.lock.json: transformed env & headers, preserved ${MY_TOKEN}
    let lock_content = fs::read_to_string(out_dir.join("plugins.lock.json")).unwrap();
    let lock_val: serde_json::Value = serde_json::from_str(&lock_content).unwrap();
    let alpha = &lock_val["_mcpServers"]["alpha"];
    assert_eq!(
        alpha["env"]["API_TOKEN"].as_str().unwrap(),
        "${CCYNC_SECRET_ALPHA_API_TOKEN}",
        "raw env secret must be replaced with ${{CCYNC_SECRET_<SERVER>_<KEY>}}"
    );
    assert_eq!(
        alpha["env"]["PRESERVED"].as_str().unwrap(),
        "${MY_TOKEN}",
        "existing ${{...}} placeholder must remain untouched"
    );
    assert_eq!(
        alpha["headers"]["authorization"].as_str().unwrap(),
        "${CCYNC_SECRET_ALPHA_AUTHORIZATION}",
        "raw header secret must be replaced with ${{CCYNC_SECRET_<SERVER>_<KEY>}}"
    );
}

#[test]
fn backup_with_strip_secrets_sanitizes_server_and_key_identifiers() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let out = TempDir::new().unwrap();
    let out_dir = out.path().join("backup");

    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    fs::write(
        ccync.join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();
    fs::write(
        ccync.join("build").join("lock.json"),
        r#"{
  "_mcpServers": {
    "my-custom.server": {
      "env": {
        "api-key": "secret_1",
        "already_var": "${ALREADY_SET}"
      },
      "headers": {
        "x-custom-token": "secret_2"
      }
    }
  }
}"#,
    )
    .unwrap();

    let args = vec![
        "backup".to_string(),
        "--output".to_string(),
        out_dir.to_string_lossy().to_string(),
        "--strip-secrets".to_string(),
    ];
    let exit = cmd_backup(&args);

    assert_eq!(exit, ExitCode::Success);
    let lock_content = fs::read_to_string(out_dir.join("plugins.lock.json")).unwrap();
    let lock_val: serde_json::Value = serde_json::from_str(&lock_content).unwrap();
    let server = &lock_val["_mcpServers"]["my-custom.server"];
    assert_eq!(
        server["env"]["api-key"].as_str().unwrap(),
        "${CCYNC_SECRET_MY_CUSTOM_SERVER_API_KEY}",
        "sanitization must uppercase and replace non-alphanumeric chars with underscore"
    );
    assert_eq!(
        server["env"]["already_var"].as_str().unwrap(),
        "${ALREADY_SET}",
        "existing ${{...}} placeholder must not be changed"
    );
    assert_eq!(
        server["headers"]["x-custom-token"].as_str().unwrap(),
        "${CCYNC_SECRET_MY_CUSTOM_SERVER_X_CUSTOM_TOKEN}",
        "header key must be sanitized and replaced"
    );
}

#[test]
fn plain_backup_strips_secrets_like_strip_flag() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let out = TempDir::new().unwrap();
    let out_dir = out.path().join("backup");

    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    let config_raw = "{\n  \"selectedProjectionTargets\": [\"claude\"],\n  \"secrets\": {\n    \"RAW_KEY\": \"raw_secret_value\"\n  }\n}";
    let lock_raw = "{\n  \"_mcpServers\": {\n    \"alpha\": {\n      \"env\": {\n        \"RAW\": \"secret\"\n      }\n    }\n  }\n}";
    fs::write(ccync.join("config.json"), config_raw).unwrap();
    fs::write(ccync.join("plugins.json"), r#"{"plugins":[]}"#).unwrap();
    fs::write(ccync.join("build").join("lock.json"), lock_raw).unwrap();
    fs::write(ccync.join("AGENTS.md"), "# Team Rules\n").unwrap();

    let args = vec![
        "backup".to_string(),
        "--output".to_string(),
        out_dir.to_string_lossy().to_string(),
    ];
    let exit = cmd_backup(&args);

    assert_eq!(exit, ExitCode::Success);
    let config_content = fs::read_to_string(out_dir.join("config.json")).unwrap();
    let config_val: serde_json::Value = serde_json::from_str(&config_content).unwrap();
    assert!(
        config_val.get("secrets").is_none(),
        "plain backup must omit config.json#secrets"
    );
    assert_ne!(
        config_content, config_raw,
        "plain backup must re-serialize config.json after stripping secrets"
    );

    let lock_content = fs::read_to_string(out_dir.join("plugins.lock.json")).unwrap();
    let lock_val: serde_json::Value = serde_json::from_str(&lock_content).unwrap();
    assert_eq!(
        lock_val["_mcpServers"]["alpha"]["env"]["RAW"],
        "${CCYNC_SECRET_ALPHA_RAW}"
    );
    assert_ne!(
        lock_content, lock_raw,
        "plain backup must re-serialize plugins.lock.json after stripping secrets"
    );

    // --strip-secrets run to second output of same fixture writes byte-identical profile files
    let out_flag = TempDir::new().unwrap();
    let out_dir_flag = out_flag.path().join("backup");
    let args_flag = vec![
        "backup".to_string(),
        "--output".to_string(),
        out_dir_flag.to_string_lossy().to_string(),
        "--strip-secrets".to_string(),
    ];
    let exit_flag = cmd_backup(&args_flag);
    assert_eq!(exit_flag, ExitCode::Success);

    assert_eq!(
        fs::read_to_string(out_dir.join("config.json")).unwrap(),
        fs::read_to_string(out_dir_flag.join("config.json")).unwrap(),
        "plain backup and --strip-secrets must write byte-identical config.json"
    );
    assert_eq!(
        fs::read_to_string(out_dir.join("plugins.lock.json")).unwrap(),
        fs::read_to_string(out_dir_flag.join("plugins.lock.json")).unwrap(),
        "plain backup and --strip-secrets must write byte-identical plugins.lock.json"
    );
    assert_eq!(
        fs::read_to_string(out_dir.join("plugins.json")).unwrap(),
        fs::read_to_string(out_dir_flag.join("plugins.json")).unwrap(),
        "plain backup and --strip-secrets must write byte-identical plugins.json"
    );
    assert_eq!(
        fs::read_to_string(out_dir.join("AGENTS.md")).unwrap(),
        fs::read_to_string(out_dir_flag.join("AGENTS.md")).unwrap(),
        "plain backup and --strip-secrets must write byte-identical AGENTS.md"
    );
}

#[test]
fn backup_with_strip_secrets_preserves_agents_md_and_plugins_json() {
    let _lock = crate::test_io_lock();
    let home = TempDir::new().unwrap();
    let _env = EnvHomeGuard::set(home.path());
    let out = TempDir::new().unwrap();
    let out_dir = out.path().join("backup");

    let ccync = home.path().join(".ccync");
    fs::create_dir_all(ccync.join("build")).unwrap();
    fs::write(
        ccync.join("config.json"),
        r#"{"selectedProjectionTargets":["claude"]}"#,
    )
    .unwrap();
    let plugins_raw = "{\n  \"plugins\": [\n    {\n      \"id\": \"demo\"\n    }\n  ]\n}";
    fs::write(ccync.join("plugins.json"), plugins_raw).unwrap();
    fs::write(ccync.join("build").join("lock.json"), r#"{}"#).unwrap();
    let agents_raw = "# Team Rules\nLine 2\n";
    fs::write(ccync.join("AGENTS.md"), agents_raw).unwrap();

    let args = vec![
        "backup".to_string(),
        "--output".to_string(),
        out_dir.to_string_lossy().to_string(),
        "--strip-secrets".to_string(),
    ];
    let exit = cmd_backup(&args);

    assert_eq!(exit, ExitCode::Success);
    assert_eq!(
        fs::read_to_string(out_dir.join("AGENTS.md")).unwrap(),
        agents_raw,
        "AGENTS.md must be copied unaltered under --strip-secrets"
    );
    assert_eq!(
        fs::read_to_string(out_dir.join("plugins.json")).unwrap(),
        plugins_raw,
        "plugins.json must be copied unaltered under --strip-secrets"
    );
}
