use serde_json::{json, Value};
use std::{fs, process::Command};

fn event(seq: u64, kind: &str, attempt: &str) -> Value {
    json!({"seq":seq,"time":format!("2026-01-01T00:00:{seq:02}Z"),"host":"codex","hostVersion":"1.0","session":"s1","server":"server1","operation":"op1","attempt":attempt,"parent":null,"kind":kind,"source":"native-config","actor":"agent","result":"ok","digest":null})
}

fn doctor(events: Vec<Value>) -> String {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("events.json");
    fs::write(
        &path,
        json!({"version":1,"run":"run1","events":events}).to_string(),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ccync"))
        .args(["doctor", "--mcp-evidence"])
        .arg(&path)
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("APPDATA", home.path().join("AppData/Roaming"))
        .env("LOCALAPPDATA", home.path().join("AppData/Local"))
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("PATH", home.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn reports_attempt_counts_and_unknown_stop() {
    for (events, count) in [
        (
            vec![event(1, "observe-start", "0"), event(2, "observe-end", "0")],
            0,
        ),
        (
            vec![
                event(1, "observe-start", "0"),
                event(2, "use", "1"),
                event(3, "observe-end", "0"),
            ],
            1,
        ),
        (
            vec![
                event(1, "observe-start", "0"),
                event(2, "use", "1"),
                event(3, "use", "1"),
                event(4, "use", "2"),
                event(5, "dispatch", "2"),
                event(6, "observe-end", "0"),
            ],
            2,
        ),
    ] {
        let output = doctor(events);
        assert!(
            output.contains(&format!("observed attempts: {count}")),
            "{output}"
        );
        assert!(output.contains("stop state: Unknown"), "{output}");
        assert!(
            output.contains("gap: stop evidence not established"),
            "{output}"
        );
    }
}

#[test]
fn reports_explicit_and_incomplete_stop_states() {
    for (kind, expected) in [
        ("terminate", "Terminated"),
        ("bound-exceeded", "OutOfBounds"),
        ("manual-stop", "ManualIntervention"),
        ("failure", "StillRetrying"),
        ("launch-request", "StillRetrying"),
    ] {
        let output = doctor(vec![
            event(1, "observe-start", "0"),
            event(2, kind, "1"),
            event(3, "observe-end", "0"),
        ]);
        assert!(
            output.contains(&format!("stop state: {expected}")),
            "{output}"
        );
        assert!(
            !output.contains("stop evidence not established"),
            "{output}"
        );
    }
    let output = doctor(vec![event(1, "observe-start", "0"), event(2, "use", "1")]);
    assert!(output.contains("observed attempts: 1"), "{output}");
    assert!(output.contains("stop state: Truncated"), "{output}");
}
