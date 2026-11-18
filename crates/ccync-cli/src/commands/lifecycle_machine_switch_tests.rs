//! End-to-end coverage for a secret-safe backup restored onto another machine.

use super::lifecycle::{
    cmd_backup, cmd_restore, run_projection_core, windows_stdio_publication_allowed,
};
use ccync_engine::ExitCode;
use ccync_foundation::config::CcyncConfig;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

struct HomeGuard {
    saved: Vec<(String, Option<String>)>,
}

impl HomeGuard {
    fn new(home: &Path) -> Self {
        let keys = [
            "USERPROFILE",
            "HOME",
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
        ];
        let saved = keys
            .into_iter()
            .map(|key| {
                let value = std::env::var(key).ok();
                unsafe { std::env::set_var(key, home) };
                (key.to_string(), value)
            })
            .collect();
        Self { saved }
    }

    fn switch(&mut self, home: &Path) {
        unsafe {
            std::env::set_var("USERPROFILE", home);
            std::env::set_var("HOME", home);
            std::env::set_var("APPDATA", home);
            std::env::set_var("LOCALAPPDATA", home);
            std::env::set_var("XDG_CONFIG_HOME", home);
        }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

fn assert_tree_does_not_contain(root: &Path, secret: &str) {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(&path).unwrap() {
            let entry = entry.unwrap();
            let entry_path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry_path);
            } else if let Ok(content) = fs::read_to_string(&entry_path) {
                assert!(
                    !content.contains(secret),
                    "secret leaked into {}",
                    entry_path.display()
                );
            }
        }
    }
}

#[test]
fn backup_restore_machine_switch_never_copies_literal_secret_and_recovers() {
    let _lock = crate::test_io_lock();
    let home_a = TempDir::new().unwrap();
    let home_b = TempDir::new().unwrap();
    let backup_root = TempDir::new().unwrap();
    let backup = backup_root.path().join("backup");
    let secret = "machine-switch-secret-42";
    let mut home = HomeGuard::new(home_a.path());

    let ccync_a = home_a.path().join(".ccync");
    fs::create_dir_all(ccync_a.join("build")).unwrap();
    fs::write(
        ccync_a.join("config.json"),
        format!(
            r#"{{"selectedProjectionTargets":["claude"],"secrets":{{"API_TOKEN":"{secret}"}}}}"#
        ),
    )
    .unwrap();
    fs::write(ccync_a.join("plugins.json"), r#"{"plugins":[]}"#).unwrap();
    fs::write(
        ccync_a.join("build").join("lock.json"),
        format!(
            r#"{{"_mcpServers":{{"alpha":{{"command":"node","env":{{"API_TOKEN":"{secret}"}}}},"healthy":{{"type":"http","url":"https://mcp.example.test/healthy"}}}},"resolvedPlugins":[]}}"#
        ),
    )
    .unwrap();

    let exit = cmd_backup(&[
        "backup".to_string(),
        "--output".to_string(),
        backup.to_string_lossy().into_owned(),
    ]);
    assert_eq!(exit, ExitCode::Success);
    assert_tree_does_not_contain(&backup, secret);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(
            &fs::read_to_string(backup.join("plugins.lock.json")).unwrap()
        )
        .unwrap()["_mcpServers"]["alpha"]["env"]["API_TOKEN"],
        "${CCYNC_SECRET_ALPHA_API_TOKEN}"
    );

    home.switch(home_b.path());
    let exit = cmd_restore(&[
        "restore".to_string(),
        "--from".to_string(),
        backup.to_string_lossy().into_owned(),
    ]);
    assert_eq!(exit, ExitCode::Success);
    assert_tree_does_not_contain(home_b.path(), secret);

    fs::create_dir_all(home_b.path().join(".agents")).unwrap();
    fs::write(home_b.path().join(".agents/mcp.json"), r#"{"ccyncMcpServers":{},"hostRoutes":[{"source":"adopted","server":"alpha","hosts":["claude","copilot","opencode","codex","agy"]},{"source":"adopted","server":"healthy","hosts":["claude","copilot","opencode","codex","agy"]}]}"#).unwrap();
    let config = CcyncConfig::load();
    let first = run_projection_core(&config, "ccync sync");
    assert_eq!(first, ExitCode::Success);
    let claude_config = home_b.path().join(".claude.json");
    let first_profile = fs::read_to_string(&claude_config).unwrap();
    assert!(first_profile.contains("https://mcp.example.test/healthy"));
    assert!(!first_profile.contains("alpha"));
    assert_tree_does_not_contain(home_b.path(), secret);

    fs::write(
        home_b.path().join(".ccync").join("config.json"),
        format!(
            r#"{{"selectedProjectionTargets":["claude"],"secrets":{{"CCYNC_SECRET_ALPHA_API_TOKEN":"{secret}"}}}}"#
        ),
    )
    .unwrap();
    let config = CcyncConfig::load();
    let second = run_projection_core(&config, "ccync sync");
    let second_profile = fs::read_to_string(&claude_config).unwrap();
    if windows_stdio_publication_allowed() {
        assert_eq!(second, ExitCode::Success);
        assert!(second_profile.contains(secret));
    } else {
        assert_eq!(second, ExitCode::Error);
        assert!(
            !second_profile.contains(secret),
            "unqualified stdio must stay blocked on Windows, so the secret cannot reach the live host"
        );
    }
    assert!(second_profile.contains("https://mcp.example.test/healthy"));
}
