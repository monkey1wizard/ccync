//! `ccync doctor` reports route evidence read-only from the route record the
//! last sync committed.
//!
//! The probe runs the real `ccync` binary against a temporary home. OpenCode
//! is selected, but the temporary home holds no OpenCode installation, so the
//! agent is not detected. The target must stay selected, its loader state
//! must read `NotRun`, nothing about the missing agent may become a warning,
//! and doctor must print each route's origin, route, original name, and
//! management state without writing anything.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

/// Committed machine roots relative to the fixture home. Windows uses the
/// APPDATA and LOCALAPPDATA layout, other platforms the XDG defaults.
const STATE_LOCK_PATH: &str = if cfg!(windows) {
    "AppData/Local/ccync/state/build/lock.json"
} else {
    ".local/state/ccync/build/lock.json"
};
const STATE_LAYOUT_PATH: &str = if cfg!(windows) {
    "AppData/Local/ccync/state/layout.json"
} else {
    ".local/state/ccync/layout.json"
};
const STATE_SOURCES_PREFIX: &str = if cfg!(windows) {
    "AppData/Local/ccync/state/sources/"
} else {
    ".local/state/ccync/sources/"
};
const MACHINE_CONFIG_PATH: &str = if cfg!(windows) {
    "AppData/Roaming/ccync/config.json"
} else {
    ".config/ccync/config.json"
};
const MACHINE_CATALOG_PATH: &str = if cfg!(windows) {
    "AppData/Roaming/ccync/plugins.json"
} else {
    ".config/ccync/plugins.json"
};

struct Home {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        // tempfile may return an 8.3 path on Windows; the CLI canonicalizes
        // discovered paths, so build expectations from the same root.
        let root = dir.path().canonicalize().unwrap();
        Self { _dir: dir, root }
    }

    fn path(&self, relative: &str) -> PathBuf {
        let committed = self.root.join(STATE_LAYOUT_PATH).is_file();
        let mapped = if committed {
            match relative {
                ".ccync/build/lock.json" => STATE_LOCK_PATH,
                ".ccync/plugins.json" => MACHINE_CATALOG_PATH,
                ".ccync/config.json" => MACHINE_CONFIG_PATH,
                _ => relative,
            }
        } else {
            relative
        };
        if committed && mapped.starts_with(".ccync/cache/") {
            self.root
                .join(mapped.replacen(".ccync/cache/", STATE_SOURCES_PREFIX, 1))
        } else {
            self.root.join(mapped)
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        // An empty PATH directory: no client CLI is reachable from the binary.
        let isolated_bin = self.root.join("isolated-bin");
        fs::create_dir_all(&isolated_bin).unwrap();
        Command::new(env!("CARGO_BIN_EXE_ccync"))
            .args(args)
            .current_dir(&self.root)
            .env("HOME", &self.root)
            .env("USERPROFILE", &self.root)
            .env("APPDATA", self.root.join("AppData").join("Roaming"))
            .env("LOCALAPPDATA", self.root.join("AppData").join("Local"))
            .env("XDG_CONFIG_HOME", self.root.join(".config"))
            .env("PATH", &isolated_bin)
            .env_remove("CCYNC_ASSUME_YES")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env_remove("COPILOT_HOME")
            .env_remove("OPENCODE_CONFIG_DIR")
            .env_remove("OPENCODE_CONFIG")
            .env_remove("OPENCODE_CONFIG_CONTENT")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn transcript(output: &Output) -> String {
    format!(
        "exit: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn doctor_reads_only_named_mcp_evidence_and_reports_uncertainty() {
    let home = Home::new();
    let event = |seq, kind, actor, parent, attempt| serde_json::json!({"seq":seq,"time":format!("2026-01-01T00:00:{seq:02}Z"),"host":"codex","hostVersion":"1.0","session":"s1","server":"server1","operation":"op1","attempt":attempt,"parent":parent,"kind":kind,"source":"native-config","actor":actor,"result":"ok","digest":null});
    let evidence = serde_json::json!({"version":1,"run":"run1","events":[
        event(1, "observe-start", "unknown", None, "0"),
        event(2, "dispatch", "ccync", None, "1"),
        event(3, "failure", "ccync", Some(2), "1"),
        event(4, "dispatch", "agent", Some(3), "2"),
        event(5, "failure", "agent", Some(4), "2")
    ]});
    write(&home.path("events.json"), &evidence.to_string());
    write(&home.root.join(STATE_LAYOUT_PATH), "invalid layout");
    let first = home.run(&["doctor", "--mcp-evidence", "events.json"]);
    let second = home.run(&["doctor", "--mcp-evidence=events.json"]);
    assert_eq!(first.status.code(), Some(0), "{}", transcript(&first));
    assert_eq!(first.stdout, second.stdout);
    let output = String::from_utf8_lossy(&first.stdout);
    for expected in [
        "unproven",
        "codex",
        "server1",
        "observed attempts: 2",
        "Ccync",
        "Agent",
        "AgentRetry",
        "Truncated",
        "event IDs",
        "observation interval incomplete",
    ] {
        assert!(output.contains(expected), "missing {expected}: {output}");
    }
    for args in [
        vec!["doctor", "--mcp-evidence", "missing.json"],
        vec!["doctor", "--mcp-evidence", "bad.json"],
    ] {
        write(&home.path("bad.json"), "{\"secret\":\"raw-secret-value\"}");
        let result = home.run(&args);
        assert_eq!(result.status.code(), Some(1), "{}", transcript(&result));
        assert!(!transcript(&result).contains("raw-secret-value"));
    }
    for args in [
        vec!["doctor", "--mcp-evidence"],
        vec!["doctor", "--mcp-evidence="],
        vec!["doctor", "--mcp-evidence", "--dry-run"],
        vec!["doctor", "--mcp-evidence=a", "--mcp-evidence=b"],
    ] {
        let result = home.run(&args);
        assert_eq!(result.status.code(), Some(64), "{}", transcript(&result));
    }
}

#[test]
fn doctor_refuses_oversized_evidence_without_native_calls() {
    use ccync_foundation::provider::{
        CapabilityInventory, ClientScope, CommittedOwnership, EnablementIntent, GenerationId,
        NativeSelector, PackageDigest, ProviderGeneration, ProviderLifecycleLock,
        ProviderSourceKey,
    };
    use ccync_foundation::target::ProjectionTargetId;

    let home = Home::new();
    let bin = home.root.join("isolated-bin");
    fs::create_dir_all(&bin).unwrap();
    let source = home.root.join("stub.rs");
    write(
        &source,
        r#"use std::{env,fs::OpenOptions,io::Write}; fn main(){ let mut f=OpenOptions::new().create(true).append(true).open(env::var_os("PROBE_CALLS").unwrap()).unwrap(); writeln!(f,"call").unwrap(); println!("[]"); }"#,
    );
    let stub = bin.join(if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    });
    assert!(Command::new("rustc")
        .args(["--edition=2021"])
        .arg(&source)
        .arg("-o")
        .arg(&stub)
        .status()
        .unwrap()
        .success());
    let calls = home.root.join("calls.txt");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_ccync"))
            .args(args)
            .current_dir(&home.root)
            .env("HOME", &home.root)
            .env("USERPROFILE", &home.root)
            .env("APPDATA", home.root.join("AppData/Roaming"))
            .env("LOCALAPPDATA", home.root.join("AppData/Local"))
            .env("XDG_CONFIG_HOME", home.root.join(".config"))
            .env("PATH", &bin)
            .env("PROBE_CALLS", &calls)
            .env("CCYNC_ASSUME_YES", "1")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let init = run(&["init", "claude", "--agents", "claude", "--yes"]);
    assert!(init.status.success(), "{}", transcript(&init));
    let lock_path = home.path(".ccync/build/lock.json");
    let mut lock = serde_json::from_slice(&fs::read(&lock_path).unwrap()).unwrap();
    let digest =
        |c: char| PackageDigest::new(format!("sha256:{}", c.to_string().repeat(64))).unwrap();
    let record = CommittedOwnership {
        target: ProjectionTargetId::Claude,
        selector: NativeSelector::new("Example@ccync-Example").unwrap(),
        plugin_name: "Example".into(),
        upstream_revision: None,
        generation: ProviderGeneration {
            source: ProviderSourceKey::new("example").unwrap(),
            generation: GenerationId::new("example-gen").unwrap(),
            package_digest: digest('a'),
        },
        capabilities: CapabilityInventory::new(),
        client_scope: Some(ClientScope::new("ccync-claude").unwrap()),
        source_digest: Some(digest('b')),
        view_digest: Some(digest('c')),
        observed_preimage: None,
        enablement_intent: Some(EnablementIntent::Enabled),
        group_substep: None,
    };
    ProviderLifecycleLock {
        committed: vec![record],
        ..Default::default()
    }
    .write_to_lock(&mut lock)
    .unwrap();
    fs::write(lock_path, serde_json::to_vec(&lock).unwrap()).unwrap();
    fs::write(&calls, "").unwrap();
    let ordinary = run(&["doctor"]);
    assert!(
        !fs::read(&calls).unwrap().is_empty(),
        "{}",
        transcript(&ordinary)
    );

    fs::write(home.root.join("oversized.json"), vec![b'x'; 256 * 1024 + 1]).unwrap();
    fs::write(&calls, "").unwrap();
    let result = run(&["doctor", "--mcp-evidence", "oversized.json"]);
    assert_eq!(result.status.code(), Some(1), "{}", transcript(&result));
    assert!(result.stdout.is_empty(), "{}", transcript(&result));
    assert!(String::from_utf8_lossy(&result.stderr).contains("256 KiB limit"));
    assert!(
        fs::read(&calls).unwrap().is_empty(),
        "{}",
        transcript(&result)
    );
}

/// The inputs a real sync reads: the machine config selecting `targets`, a
/// personal catalog entry for one bundled-local provider, and the provider's
/// cache copy under the `<id>@<sha>` name sync resolves.
fn seed_machine(home: &Home, targets: &str) {
    let plugin = home.path(&format!(".ccync/cache/example@{SHA}"));
    write(
        &plugin.join(".claude-plugin/plugin.json"),
        r#"{"name":"example","version":"1.0.0"}"#,
    );
    write(
        &plugin.join("skills/example/SKILL.md"),
        "---\nname: example\ndescription: Example skill.\n---\nBody.\n",
    );
    write(
        &plugin.join("commands/audit.md"),
        "---\ndescription: Audit command.\n---\nInspect $ARGUMENTS\n",
    );
    write(
        &home.path(".ccync/plugins.json"),
        r#"{"plugins":[{"pluginId":"example","displayName":"Example","supportTier":"local","sourceType":"local","license":"proprietary","checksumPolicy":"none","componentMap":{"skills":true,"mcp":false,"agents":true},"supportedProviders":["claude","codex","copilot","opencode","agy"],"installStrategy":"bundled-local","source":"~/.ccync/plugins/example","defaultProfiles":[],"allowAutoUpdate":false,"localOverridePolicy":"always"}]}"#,
    );
    write(
        &home.path(".ccync/config.json"),
        &format!(r#"{{"selectedProjectionTargets":[{targets}]}}"#),
    );
}

/// Every regular file under `root` with its bytes, keyed by relative path.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() {
                let bytes = fs::read(&path).unwrap_or_default();
                files.insert(path.strip_prefix(root).unwrap().to_path_buf(), bytes);
            }
        }
    }
    files
}

/// The `route:` line for one target and component.
fn route_line<'a>(text: &'a str, target: &str, kind: &str, name: &str) -> &'a str {
    let prefix = format!("route: {target} {kind} {name};");
    text.lines()
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_else(|| panic!("no `{prefix}` line in:\n{text}"))
}

#[test]
fn doctor_reports_route_evidence_with_origin_for_a_target_whose_agent_is_not_installed() {
    let home = Home::new();
    seed_machine(&home, r#""codex","opencode""#);
    assert!(
        !home.path(".opencode").exists() && !home.path(".config/opencode").exists(),
        "the temporary home must hold no OpenCode installation"
    );

    let sync = home.run(&["sync", "--yes"]);
    assert_eq!(sync.status.code(), Some(0), "{}", transcript(&sync));
    let sync_err = String::from_utf8_lossy(&sync.stderr).into_owned();

    // The target stays selected, and sync reports its loader as NotRun.
    let config = fs::read_to_string(home.path(".ccync/config.json")).unwrap();
    assert!(config.contains("\"opencode\""), "{config}");
    let sync_route = route_line(&sync_err, "opencode", "Skill", "example");
    assert!(sync_route.contains("; selected;"), "{sync_route}");
    assert!(sync_route.contains("loader NotRun"), "{sync_route}");
    // An undetected agent never escalates to a warning.
    for line in sync_err.lines().filter(|line| line.starts_with("warning:")) {
        let lower = line.to_lowercase();
        assert!(
            !(lower.contains("opencode")
                && (lower.contains("not installed")
                    || lower.contains("not detected")
                    || lower.contains("loader"))),
            "undetected agent escalated to a warning: {line}\n{}",
            transcript(&sync)
        );
    }

    let before = snapshot(&home.root);
    let doctor = home.run(&["doctor"]);
    let after = snapshot(&home.root);
    assert_eq!(
        before,
        after,
        "doctor must write nothing\n{}",
        transcript(&doctor)
    );

    let report = String::from_utf8_lossy(&doctor.stdout).into_owned();
    let origin = home
        .path(&format!(".ccync/cache/example@{SHA}"))
        .display()
        .to_string();

    // The skill: OpenCode is served by the shared root Codex writes.
    let skill = route_line(&report, "opencode", "Skill", "example");
    // Ownership is re-read from the lockfile, so the management status is
    // whatever the lockfile records now; the line must state it either way.
    for fact in [
        "; selected;",
        "; published;",
        "owned by ccync;",
        "loader NotRun",
        "route served by root",
        "served by root ~/.agents/skills",
        "path ~/.agents/skills/example",
    ] {
        assert!(skill.contains(fact), "missing `{fact}`: {skill}");
    }
    assert!(
        skill.contains(&format!("origin example at {origin}")),
        "missing origin {origin}: {skill}"
    );
    assert!(
        skill.contains("action: start opencode and confirm `example` is listed"),
        "{skill}"
    );

    // The command: Codex publishes it as its equivalent skill, and OpenCode
    // discovers that copy as well as its own native command.
    let codex_command = route_line(&report, "codex", "Command", "audit");
    for fact in [
        "; published;",
        "route command-to-skill",
        "served by root ~/.agents/skills",
    ] {
        assert!(
            codex_command.contains(fact),
            "missing `{fact}`: {codex_command}"
        );
    }
    let opencode_command = route_line(&report, "opencode", "Command", "audit");
    assert!(
        opencode_command.contains("discovered in multiple roots"),
        "{opencode_command}"
    );

    // Nothing about the not-installed agent is a doctor warning.
    for line in report.lines().filter(|line| line.starts_with("[WARNING]")) {
        let lower = line.to_lowercase();
        assert!(
            !lower.contains("opencode") && !lower.contains("notrun") && !lower.contains("route"),
            "doctor escalated route or agent state to a warning: {line}\n{report}"
        );
    }
}

#[test]
fn doctor_reports_a_route_copy_removed_after_sync_as_not_published() {
    let home = Home::new();
    seed_machine(&home, r#""codex""#);
    let sync = home.run(&["sync", "--yes"]);
    assert_eq!(sync.status.code(), Some(0), "{}", transcript(&sync));

    fs::remove_dir_all(home.path(".agents/skills/example")).unwrap();
    let doctor = home.run(&["doctor"]);
    let report = String::from_utf8_lossy(&doctor.stdout).into_owned();

    let skill = route_line(&report, "codex", "Skill", "example");
    assert!(skill.contains("; not published;"), "{skill}");
    assert!(
        skill.contains("action: fix the reported write error ("),
        "{skill}"
    );
    assert!(skill.contains("is gone since the last sync"), "{skill}");
}

#[test]
fn p1_standalone_doctor_preserves_loose_identity_and_does_not_claim_native() {
    let home = Home::new();
    let source = home.path("inputs/loose-skill");
    write(
        &source.join("SKILL.md"),
        "---\nname: loose-skill\ndescription: Loose fixture.\n---\nBody.\n",
    );
    write(
        &home.path(".ccync/build/lock.json"),
        &serde_json::json!({"_looseSkills": [{"name": "loose-skill", "source": "fixture", "origin": source}]}).to_string(),
    );
    write(
        &home.path(".ccync/config.json"),
        r#"{"selectedProjectionTargets":["codex"]}"#,
    );

    let sync = home.run(&["sync", "--yes"]);
    assert_eq!(sync.status.code(), Some(0), "{}", transcript(&sync));
    let sync_text = format!(
        "{}{}",
        String::from_utf8_lossy(&sync.stdout),
        String::from_utf8_lossy(&sync.stderr)
    );
    let sync_route = route_line(&sync_text, "codex", "Skill", "loose-skill");
    assert!(sync_route.contains("standalone published"), "{sync_route}");

    let doctor = home.run(&["doctor"]);
    assert_eq!(doctor.status.code(), Some(0), "{}", transcript(&doctor));
    let report = String::from_utf8_lossy(&doctor.stdout).into_owned();
    let doctor_route = route_line(&report, "codex", "Skill", "loose-skill");
    assert!(
        doctor_route.contains("standalone published"),
        "{doctor_route}"
    );
    assert!(
        doctor_route.contains("origin loose-origin:"),
        "{doctor_route}"
    );
    assert!(doctor_route.contains("loader NotRun"), "{doctor_route}");
    assert!(!doctor_route.contains("native"), "{doctor_route}");
}

#[test]
fn p1_standalone_doctor_reports_foreign_destination_as_unresolved() {
    let home = Home::new();
    let source = home.path("inputs/doctor-foreign");
    write(
        &source.join("SKILL.md"),
        "---\nname: doctor-foreign\ndescription: Foreign destination fixture.\n---\nBody.\n",
    );
    let destination = home.path(".agents/skills/doctor-foreign");
    write(&destination.join("user.txt"), "preserve me\n");
    write(
        &home.path(".ccync/build/lock.json"),
        &serde_json::json!({"_looseSkills": [{"name": "doctor-foreign", "source": "fixture", "origin": source}]}).to_string(),
    );
    write(
        &home.path(".ccync/config.json"),
        r#"{"selectedProjectionTargets":["codex"]}"#,
    );

    let sync = home.run(&["sync", "--yes"]);
    assert_eq!(sync.status.code(), Some(1), "{}", transcript(&sync));
    let doctor = home.run(&["doctor"]);
    let report = String::from_utf8_lossy(&doctor.stdout).into_owned();
    let route = route_line(&report, "codex", "Skill", "doctor-foreign");
    assert!(route.contains("identity unresolved"), "{route}");
    assert!(!route.contains("native"), "{route}");
    assert_eq!(
        fs::read_to_string(destination.join("user.txt")).unwrap(),
        "preserve me\n"
    );
}

#[test]
fn doctor_reports_unsupported_and_disabled_routes_without_false_failure() {
    let home = Home::new();
    let plugin = home.path(&format!(".ccync/cache/example@{SHA}"));
    write(
        &plugin.join(".claude-plugin/plugin.json"),
        r#"{"name":"example","version":"1.0.0"}"#,
    );
    write(
        &plugin.join("skills/example/SKILL.md"),
        "---\nname: example\ndescription: Example skill.\n---\nBody.\n",
    );
    write(
        &plugin.join("agents/helper.md"),
        "---\nname: helper\ndescription: Helper agent.\n---\nBody.\n",
    );

    const SHA_DIS: &str = "1111222233334444555566667777888899990000";
    let plugin_dis = home.path(&format!(".ccync/cache/disabled-plugin@{SHA_DIS}"));
    write(
        &plugin_dis.join(".claude-plugin/plugin.json"),
        r#"{"name":"disabled-plugin","version":"1.0.0"}"#,
    );
    write(
        &plugin_dis.join("skills/dis-skill/SKILL.md"),
        "---\nname: dis-skill\ndescription: Dis skill.\n---\nBody.\n",
    );
    write(
        &plugin_dis.join("agents/dis-agent.md"),
        "---\nname: dis-agent\ndescription: Dis agent.\n---\nBody.\n",
    );

    write(
        &home.path(".ccync/plugins.json"),
        r#"{"plugins":[
            {"pluginId":"example","displayName":"Example","supportTier":"local","sourceType":"local","license":"proprietary","checksumPolicy":"none","componentMap":{"skills":true,"mcp":false,"agents":true},"supportedProviders":["claude","codex","copilot","opencode","agy"],"installStrategy":"bundled-local","source":"~/.ccync/plugins/example","defaultProfiles":[],"allowAutoUpdate":false,"localOverridePolicy":"always"},
            {"pluginId":"disabled-plugin","displayName":"DisabledPlugin","supportTier":"local","sourceType":"local","license":"proprietary","checksumPolicy":"none","componentMap":{"skills":true,"mcp":false,"agents":false},"supportedProviders":["claude","codex","copilot","opencode","agy"],"installStrategy":"bundled-local","source":"~/.ccync/plugins/disabled-plugin","defaultProfiles":[],"allowAutoUpdate":false,"localOverridePolicy":"always"}
        ]}"#,
    );
    // Select agy-ide which does not support direct agents.
    write(
        &home.path(".ccync/config.json"),
        r#"{"selectedProjectionTargets":["agy-ide"]}"#,
    );

    let sync = home.run(&["sync", "--yes"]);
    assert_eq!(
        sync.status.code(),
        Some(0),
        "sync must exit 0 for healthy unsupported and disabled routes\n{}",
        transcript(&sync)
    );

    let sync_err = String::from_utf8_lossy(&sync.stderr).into_owned();
    // Unsupported agent on agy-ide
    let agent_route = route_line(&sync_err, "agy-ide", "Agent", "helper");
    assert!(
        agent_route.contains("not published because unsupported:"),
        "{agent_route}"
    );
    assert!(
        agent_route.contains("no write is needed for this unsupported route"),
        "{agent_route}"
    );
    assert!(agent_route.contains("route none"), "{agent_route}");
    assert!(agent_route.contains("retirement: done"), "{agent_route}");

    // Disabled agent from componentMap
    let dis_route = route_line(&sync_err, "agy-ide", "Agent", "dis-agent");
    assert!(
        dis_route.contains("not published because unsupported:"),
        "{dis_route}"
    );
    assert!(
        dis_route.contains("no write is needed for this unsupported route"),
        "{dis_route}"
    );
    assert!(dis_route.contains("route none"), "{dis_route}");

    let doctor = home.run(&["doctor"]);
    let report = String::from_utf8_lossy(&doctor.stdout).into_owned();
    let doc_agent = route_line(&report, "agy-ide", "Agent", "helper");
    assert!(
        doc_agent.contains("not published because unsupported:"),
        "{doc_agent}"
    );
    assert!(
        doc_agent.contains("no write is needed for this unsupported route"),
        "{doc_agent}"
    );
    assert!(doc_agent.contains("route none"), "{doc_agent}");

    let doc_dis = route_line(&report, "agy-ide", "Agent", "dis-agent");
    assert!(
        doc_dis.contains("not published because unsupported:"),
        "{doc_dis}"
    );
    assert!(
        doc_dis.contains("no write is needed for this unsupported route"),
        "{doc_dis}"
    );
    assert!(doc_dis.contains("route none"), "{doc_dis}");

    // Doctor must not escalate healthy unsupported or disabled routes to warnings.
    for line in report.lines().filter(|line| line.starts_with("[WARNING]")) {
        let lower = line.to_lowercase();
        assert!(
            !lower.contains("helper")
                && !lower.contains("dis-agent")
                && !lower.contains("unsupported"),
            "doctor escalated unsupported or disabled route to warning: {line}\n{report}"
        );
    }
}

#[test]
fn p7_t14_unsupported_route_is_named_and_never_reported_as_full_success() {
    let home = Home::new();
    let plugin = home.path(&format!(".ccync/cache/example@{SHA}"));
    write(
        &plugin.join(".claude-plugin/plugin.json"),
        r#"{"name":"example","version":"1.0.0"}"#,
    );
    write(
        &plugin.join("skills/example/SKILL.md"),
        "---\nname: example\ndescription: Example skill.\n---\nBody.\n",
    );
    write(
        &plugin.join("agents/helper.md"),
        "---\nname: helper\ndescription: Helper agent.\n---\nBody.\n",
    );
    write(
        &home.path(".ccync/plugins.json"),
        r#"{"plugins":[{"pluginId":"example","displayName":"Example","supportTier":"local","sourceType":"local","license":"proprietary","checksumPolicy":"none","componentMap":{"skills":true,"mcp":false,"agents":true},"supportedProviders":["claude","codex","copilot","opencode","agy"],"installStrategy":"bundled-local","source":"~/.ccync/plugins/example","defaultProfiles":[],"allowAutoUpdate":false,"localOverridePolicy":"always"}]}"#,
    );
    // agy-ide has no direct agent route, so the agent is unsupported there.
    write(
        &home.path(".ccync/config.json"),
        r#"{"selectedProjectionTargets":["agy-ide"]}"#,
    );

    let sync = home.run(&["sync", "--yes"]);
    assert_eq!(sync.status.code(), Some(0), "{}", transcript(&sync));
    let stdout = String::from_utf8_lossy(&sync.stdout).into_owned();

    assert!(
        !stdout.contains("Successfully rebuilt"),
        "an unsupported route must never read as full success: {stdout}"
    );
    assert!(
        stdout.contains("Not fully applied: agy-ide Agent helper — unsupported:"),
        "the unsupported route must be named with its reason: {stdout}"
    );
}

#[test]
fn p7_t14_blocked_route_makes_sync_exit_nonzero_and_is_never_full_success() {
    let home = Home::new();
    seed_machine(&home, r#""codex""#);
    // A regular file where Codex's shared skill root must be: the route
    // cannot be written, so the target is blocked.
    write(&home.path(".agents/skills"), "not a directory");

    let sync = home.run(&["sync", "--yes"]);
    let stdout = String::from_utf8_lossy(&sync.stdout).into_owned();

    // docs/commands.md documents `not published` routes, blocked or written in
    // error, as exit 1.
    assert_eq!(sync.status.code(), Some(1), "{}", transcript(&sync));
    assert!(
        !stdout.contains("Successfully rebuilt"),
        "a blocked route must never read as full success: {stdout}"
    );
}

#[test]
fn p7_t14_release_gate_and_normal_doctor_share_exit_and_report() {
    let home = Home::new();
    seed_machine(&home, r#""codex""#);
    let sync = home.run(&["sync", "--yes"]);
    assert_eq!(sync.status.code(), Some(0), "{}", transcript(&sync));

    // docs/setup.md documents that --release-gate adds no independent checks,
    // so the two runs must agree on exit and on every printed line.
    let normal = home.run(&["doctor"]);
    let gate = home.run(&["doctor", "--release-gate"]);

    assert_eq!(
        normal.status.code(),
        gate.status.code(),
        "{}",
        transcript(&gate)
    );
    assert_eq!(normal.stdout, gate.stdout, "{}", transcript(&gate));
}
