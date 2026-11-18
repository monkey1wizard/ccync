use std::process::Command;

#[test]
fn sync_help_includes_reselection_and_does_not_touch_home() {
    let home = tempfile::tempdir().unwrap();
    let cases: &[&[&str]] = &[
        &["sync", "--help"],
        &["sync", "-h"],
        &["sync", "--reselect-mcp", "--help"],
        &[
            "sync",
            "--dry-run",
            "--reselect-mcp",
            "user",
            "server",
            "codex",
            "-h",
        ],
    ];
    for args in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_ccync"))
            .args(*args)
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("APPDATA", home.path().join("appdata"))
            .env("LOCALAPPDATA", home.path().join("local"))
            .env("XDG_CONFIG_HOME", home.path().join("config"))
            .env("XDG_STATE_HOME", home.path().join("state"))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("ccync sync --reselect-mcp SOURCE SERVER HOST [--dry-run]"));
        assert!(stdout.contains("ccync sync [--dry-run] [--yes] [--instructions-only]"));
        assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
    }
}

#[test]
fn reselect_invalid_arguments_return_usage() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join(".ccync")).unwrap();
    std::fs::write(home.path().join(".ccync/config.json"), "{}").unwrap();
    let cases: &[&[&str]] = &[
        &["sync", "--reselect-mcp", "user", "server"],
        &["sync", "--reselect-mcp", "user", "server", "codex", "extra"],
        &["sync", "--reselect-mcp", "user", "server", "--unknown"],
        &[
            "sync",
            "--dry-run",
            "--reselect-mcp",
            "user",
            "server",
            "unknown",
        ],
    ];
    for args in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_ccync"))
            .args(*args)
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("APPDATA", home.path().join("appdata"))
            .env("LOCALAPPDATA", home.path().join("local"))
            .env("XDG_CONFIG_HOME", home.path().join("config"))
            .env("XDG_STATE_HOME", home.path().join("state"))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(64), "{args:?}: {output:?}");
    }
}
