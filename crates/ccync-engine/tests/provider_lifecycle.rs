use ccync_engine::delivery_apply::UnavailableNativeRunner;
use ccync_engine::install::{
    apply_agy_evidence, clear_native_intent, commit_native_ownership, record_native_intent,
    run_update_with_evidence, run_update_with_evidence_using,
};
use ccync_engine::plugin_native::{
    install_native_registration, set_native_enablement, NativeExecutable, NativeInstall,
    ProcessNativeRunner,
};
use ccync_foundation::provider::{
    CapabilityInventory, ClientScope, CommittedOwnership, EnablementIntent, GenerationId,
    IntentPhase, LayoutRootIdentity, NativeSelector, OwnershipFingerprint, PackageDigest, PathRoot,
    PendingIntent, PreimageRef, ProviderGeneration, ProviderLifecycleJournal, ProviderSourceKey,
    RelativePath, ScopedPath, StorageLayoutCommit,
};
use ccync_foundation::target::{ProjectionTargetId, CANONICAL_PROJECTION_TARGETS};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;
mod support;
use support::isolated;

const UPSTREAM_REVISION: &str = "0123456789abcdef0123456789abcdef01234567";

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(path, text).expect("write");
}

/// Skill names that `seed_demo_provider` publishes.
const FIXTURE_SKILLS: &[&str] = &["demo-pipeline"];

/// Seed one DEMO-shaped bundled provider and select the given targets.
fn seed_demo_provider(home: &Path, targets: &[&str]) {
    let payload = home.join(".ccync/plugins/demo");
    write(
        &payload.join(".claude-plugin/plugin.json"),
        r#"{"name":"demo","version":"0.3.0","description":"DEMO workflow plugin"}"#,
    );
    for skill in FIXTURE_SKILLS {
        write(
            &payload.join(format!("skills/{skill}/SKILL.md")),
            &format!(
                "---\nname: {skill}\ndescription: Run the DEMO pipeline. Iterates tasks.\n---\n\nBody.\n"
            ),
        );
    }
    write(
        &payload.join("hooks/hooks.json"),
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo ready"}]}]}}"#,
    );
    write(
        &home.join(".ccync/build/lock.json"),
        &format!(
            r#"{{"resolvedPlugins":[{{"pluginId":"demo","resolvedSource":"demo","installStrategy":"bundled-local","sha":"{UPSTREAM_REVISION}"}}]}}"#
        ),
    );
    let selected = targets
        .iter()
        .map(|target| format!("\"{target}\""))
        .collect::<Vec<_>>()
        .join(",");
    write(
        &home.join(".ccync/config.json"),
        &format!(r#"{{"selectedProjectionTargets":[{selected}]}}"#),
    );
}

#[test]
fn legacy_demo_shape_sync_is_repeatable_and_keeps_provider_generations() {
    let (home, _lock) = isolated();
    let combined = home.root().join(".ccync/build/render");
    let legacy = combined.join(".claude-plugin/plugin.json");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::write(&legacy, r#"{"name":"ccync"}"#).unwrap();
    let retained = combined.join("providers/demo/g-old/retained");
    std::fs::create_dir_all(retained.parent().unwrap()).unwrap();
    std::fs::write(&retained, "generation").unwrap();
    std::fs::create_dir_all(home.root().join(".ccync/build")).unwrap();
    std::fs::write(
        home.root().join(".ccync/build/lock.json"),
        r#"{"resolvedPlugins":[]}"#,
    )
    .unwrap();

    for _ in 0..2 {
        let _ = run_update_with_evidence_using(
            &ccync_engine::config::CcyncConfig::default(),
            &UnavailableNativeRunner,
        )
        .expect("isolated sync");
    }
    assert!(retained.is_file());
    let copilot_root = home
        .root()
        .join(".copilot/installed-plugins/ccync-copilot/ccync");
    assert!(
        !copilot_root.exists(),
        "Copilot's combined plugin root is retired after direct publication"
    );
    assert_eq!(CANONICAL_PROJECTION_TARGETS.len(), 7);
    assert!(home.root().join(".codex").starts_with(home.root()));
}

#[test]
fn target_vocabulary_covers_all_seven_independent_surfaces() {
    let expected = [
        "claude", "codex", "copilot", "opencode", "agy-cli", "agy-ide", "agy-gui",
    ];
    let actual: Vec<_> = CANONICAL_PROJECTION_TARGETS
        .iter()
        .map(|target| target.as_str())
        .collect();
    assert_eq!(actual, expected);
    assert_eq!(ProjectionTargetId::AgyGui.as_str(), "agy-gui");
}

#[test]
fn provider_lifecycle_sync_reuses_the_cli_held_lock_without_deadlock() {
    let (home, _lock) = isolated();
    seed_demo_provider(home.root(), &["claude"]);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _guard = ccync_engine::tool_manager::lock::LifecycleLock::acquire().expect("cli guard");
        let result = run_update_with_evidence_using(
            &ccync_engine::config::CcyncConfig::default(),
            &UnavailableNativeRunner,
        )
        .map(|_| ());
        let _ = done_tx.send(result.map_err(|error| error.to_string()));
    });
    let result = done_rx
        .recv_timeout(std::time::Duration::from_secs(120))
        .expect("sync under a held CLI guard must not wait on its own lock");
    result.expect("sync under held guard");
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Print the single stdout line that a live probe reports.
fn probe_line(name: &str, body: &str) {
    println!("live-probe {name}: {body}");
}

/// Every reported root must lie under the isolated home, never the user's real home.
fn assert_roots_isolated(home: &Path, roots: &[&Path]) {
    for root in roots {
        assert!(
            root.starts_with(home),
            "probe root {} escapes the isolated home {}",
            root.display(),
            home.display()
        );
    }
}

/// The launcher name that starts, trying the bare name before the Windows
/// `.cmd` shim that an npm install may provide. `None` means the CLI is absent.
fn launcher(program: &str) -> Option<String> {
    [program.to_string(), format!("{program}.cmd")]
        .into_iter()
        .find(|candidate| {
            Command::new(candidate)
                .arg("--version")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok()
        })
}

fn cli_version(launcher: &str) -> String {
    let output = Command::new(launcher)
        .arg("--version")
        .output()
        .expect("probe CLI reports its version");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Start the probe CLI with piped stdio, pointed at an isolated profile.
fn spawn(launcher: &str, args: &[&str], env: &[(&str, &Path)], cwd: &Path) -> Child {
    Command::new(launcher)
        .args(args)
        .envs(env.iter().copied())
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("probe CLI starts")
}

/// Read stdout on a thread so the probe can bound each wait by a timeout.
fn read_lines(stdout: std::process::ChildStdout) -> Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    receiver
}

/// Wait for the first JSON line that satisfies `matches`, skipping others.
fn wait_for(lines: &Receiver<String>, what: &str, matches: impl Fn(&Value) -> bool) -> Value {
    loop {
        let line = lines
            .recv_timeout(PROBE_TIMEOUT)
            .unwrap_or_else(|error| panic!("no {what} within {PROBE_TIMEOUT:?}: {error}"));
        if let Ok(value) = serde_json::from_str::<Value>(&line) {
            if matches(&value) {
                return value;
            }
        }
    }
}

fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Claude Code loader probe. It installs the owned view through the adapter,
/// reads the `system` `init` event, disables the registration through the
/// adapter, and reads the init event again.
#[test]
#[ignore = "live Claude loader probe requires an installed claude CLI"]
fn provider_live_claude() {
    run_claude_live_probe();
}

#[test]
#[ignore = "live Claude loader probe requires an installed claude CLI"]
fn p7_live_claude() {
    run_claude_live_probe();
}

/// Skill published by the Claude probe's native marketplace. The name is unique
/// so that no skill already on the machine can satisfy the count.
const CLAUDE_PROBE_SKILL: &str = "ccync-native-probe";

/// Exact selector of the Claude probe's registration: plugin `demo` in the
/// `ccync` marketplace, installed in the user scope.
const CLAUDE_PROBE_SELECTOR: &str = "demo@ccync";

/// Render the owned native package view: a directory marketplace named `ccync`
/// whose single plugin `demo` carries one user-invocable skill.
fn render_claude_view(view: &Path) {
    write(
        &view.join(".claude-plugin/marketplace.json"),
        r#"{"name":"ccync","owner":{"name":"ccync"},"plugins":[{"name":"demo","source":"./demo","description":"ccync native loader probe"}]}"#,
    );
    write(
        &view.join("demo/.claude-plugin/plugin.json"),
        r#"{"name":"demo","version":"0.3.0","description":"ccync native loader probe"}"#,
    );
    write(
        &view.join(format!("demo/skills/{CLAUDE_PROBE_SKILL}/SKILL.md")),
        &format!(
            "---\nname: {CLAUDE_PROBE_SKILL}\ndescription: Fixture skill for the native loader probe.\n---\n\nBody.\n"
        ),
    );
}

/// Run one bounded Claude Code session in the isolated profile and return its
/// `system` `init` event. Built-in tools and MCP servers are denied, active MCP
/// and hooks are removed, and prompt suggestions are off. The probe stops
/// reading after `init`. That stop does not prove that no model request began,
/// so the probe runs only in an authorized isolated environment.
fn claude_init_event(launcher: &str, home: &Path, profile: &Path) -> Value {
    let mut child = spawn(
        launcher,
        &[
            "-p",
            "ccync live probe",
            "--output-format",
            "stream-json",
            "--verbose",
            "--tools",
            "",
            "--strict-mcp-config",
            "--settings",
            r#"{"disableAllHooks":true}"#,
            "--prompt-suggestions",
            "false",
            "--no-session-persistence",
        ],
        &[("CLAUDE_CONFIG_DIR", profile)],
        home,
    );
    drop(child.stdin.take());
    let lines = read_lines(child.stdout.take().expect("piped stdout"));
    let init = wait_for(&lines, "claude system init event", |value| {
        value["type"] == "system" && value["subtype"] == "init"
    });
    stop(&mut child);
    init
}

/// Count the probe plugin entries that the init event places under the owned view.
fn claude_identity_count(init: &Value, view: &Path) -> usize {
    init["plugins"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|plugin| {
            plugin["name"] == "demo"
                && plugin["path"]
                    .as_str()
                    .is_some_and(|path| Path::new(path).starts_with(view))
        })
        .count()
}

/// Count the probe skill that the init event reports under the probe plugin.
fn claude_skill_count(init: &Value) -> usize {
    let fixture = format!("demo:{CLAUDE_PROBE_SKILL}");
    init["skills"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|skill| skill.as_str() == Some(fixture.as_str()))
        .count()
}

fn run_claude_live_probe() {
    let Some(launcher) = launcher("claude") else {
        return probe_line("claude", "NotRun");
    };
    let (home, _lock) = isolated();
    let profile = home.root().join(".claude");
    let view = home.root().join(".ccync").join("native").join("claude");
    render_claude_view(&view);
    let view_root = view.to_string_lossy().into_owned();
    let version = cli_version(&launcher);

    let selector = NativeSelector::new(CLAUDE_PROBE_SELECTOR).expect("probe selector");
    let generation = p7_t05_generation("sha256:probe");
    // Claude Code reports no capability inventory, so the probe requires none.
    let capabilities = CapabilityInventory::new();
    install_native_registration(
        &ProcessNativeRunner,
        NativeExecutable::Claude,
        &NativeInstall {
            selector: &selector,
            marketplace_root: &view_root,
            generation: &generation,
            upstream_revision: None,
            capabilities: &capabilities,
        },
    )
    .expect("claude install through the adapter");
    let committed = CommittedOwnership {
        target: ProjectionTargetId::Claude,
        selector: selector.clone(),
        plugin_name: "demo".to_string(),
        upstream_revision: None,
        generation: generation.clone(),
        capabilities: CapabilityInventory::new(),
        client_scope: Some(ClientScope::new("demo-client").expect("scope")),
        source_digest: Some(PackageDigest::new("sha256:probe").expect("digest")),
        view_digest: Some(PackageDigest::new("sha256:view").expect("digest")),
        observed_preimage: None,
        enablement_intent: Some(EnablementIntent::Enabled),
        group_substep: None,
    };

    let enabled = claude_init_event(&launcher, home.root(), &profile);
    let identity = claude_identity_count(&enabled, &view);
    let count = claude_skill_count(&enabled);
    let disabled = set_native_enablement(
        &ProcessNativeRunner,
        NativeExecutable::Claude,
        &committed,
        false,
    )
    .map_err(|error| error.to_string())
    .map(|()| claude_skill_count(&claude_init_event(&launcher, home.root(), &profile)));
    let disabled_text = match &disabled {
        Ok(count) => count.to_string(),
        Err(reason) => format!("Unsupported reason={reason:?}"),
    };
    assert_roots_isolated(home.root(), &[profile.as_path(), view.as_path()]);
    probe_line(
        "claude",
        &format!(
            "identity={identity} count={count} disabled={disabled_text} version={version} roots={}",
            profile.display()
        ),
    );
    assert_eq!(identity, 1, "native identity must bind the owned view");
    assert_eq!(count, 1, "the enabled native skill must load once");
    let disabled = disabled.expect("claude disable through the adapter");
    assert_eq!(disabled, 0, "the disabled native skill must not load");
}

/// Codex app-server speaks JSON-RPC lines over stdio. The probe initializes,
/// then asks for the skill list of the isolated home.
#[test]
#[ignore = "live Codex discovery probe requires an installed codex CLI"]
fn provider_live_codex() {
    run_codex_live_probe();
}

#[test]
#[ignore = "live Codex discovery probe requires an installed codex CLI"]
fn p7_live_codex() {
    run_codex_live_probe();
}

/// Skill published by the Codex probe's native marketplace. The name is unique
/// so that no skill already on the machine can satisfy the count.
const CODEX_PROBE_SKILL: &str = "ccync-native-probe";

/// Exact selector of the probe's native registration: plugin `demo` in the
/// `ccync` marketplace.
const CODEX_PROBE_SELECTOR: &str = "demo@ccync";

/// Render the owned native package view: a local marketplace named `ccync`
/// whose single plugin `demo` carries one user-invocable skill.
fn render_codex_view(view: &Path) {
    write(
        &view.join(".agents/plugins/marketplace.json"),
        r#"{"name":"ccync","plugins":[{"name":"demo","source":{"source":"local","path":"./demo"},"policy":{"installation":"AVAILABLE","authentication":"ON_INSTALL"},"category":"Productivity"}]}"#,
    );
    write(
        &view.join("demo/.codex-plugin/plugin.json"),
        r#"{"name":"demo","version":"0.3.0","description":"ccync native loader probe","skills":"./skills/"}"#,
    );
    write(
        &view.join(format!("demo/skills/{CODEX_PROBE_SKILL}/SKILL.md")),
        &format!(
            "---\nname: {CODEX_PROBE_SKILL}\ndescription: Fixture skill for the native loader probe.\n---\n\nBody.\n"
        ),
    );
}

/// Count the probe's native skill that `skills/list` reports under the isolated
/// home. The app-server session is read-only and no turn starts.
fn codex_skill_count(launcher: &str, home: &Path, codex_home: &Path) -> usize {
    let mut child = spawn(
        launcher,
        &["app-server"],
        &[("CODEX_HOME", codex_home)],
        home,
    );
    let mut stdin: ChildStdin = child.stdin.take().expect("piped stdin");
    let lines = read_lines(child.stdout.take().expect("piped stdout"));
    let send = |stdin: &mut ChildStdin, message: Value| {
        writeln!(stdin, "{message}").expect("send request");
        stdin.flush().expect("flush request");
    };
    let is_response = |id: u64| move |value: &Value| value.get("id") == Some(&json!(id));

    send(
        &mut stdin,
        json!({"id": 0, "method": "initialize", "params": {"clientInfo": {
            "name": "ccync-live-probe",
            "title": "ccync live probe",
            "version": env!("CARGO_PKG_VERSION"),
        }}}),
    );
    let initialized = wait_for(&lines, "codex initialize response", is_response(0));
    assert!(
        initialized.get("result").is_some(),
        "codex initialize failed: {initialized}"
    );
    send(&mut stdin, json!({"method": "initialized", "params": {}}));
    let root = home.to_string_lossy().into_owned();
    send(
        &mut stdin,
        json!({"id": 1, "method": "skills/list", "params": {"cwds": [root]}}),
    );
    let listed = wait_for(&lines, "codex skills/list response", is_response(1));
    stop(&mut child);

    let data = listed
        .pointer("/result/data")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("codex skills/list returned no data: {listed}"));
    let fixture = format!("demo:{CODEX_PROBE_SKILL}");
    data.iter()
        .filter_map(|entry| entry.get("skills")?.as_array())
        .flatten()
        .filter(|skill| {
            // Codex also reads the user's real ~/.agents/skills, which the isolated
            // HOME does not redirect, so a skill counts only under the isolated home
            // and only when the registration that provides it is the probe's.
            let under_home = skill
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|path| Path::new(path).starts_with(home));
            under_home
                && skill["pluginId"] == CODEX_PROBE_SELECTOR
                && skill["name"] == fixture.as_str()
        })
        .count()
}

/// Installed Codex entries for the probe selector, from `plugin list --json`.
fn codex_installed_entries(launcher: &str, home: &Path, codex_home: &Path) -> Vec<Value> {
    let output = Command::new(launcher)
        .args(["plugin", "list", "--json"])
        .env("CODEX_HOME", codex_home)
        .current_dir(home)
        .output()
        .expect("codex plugin list runs");
    let listing: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    listing["installed"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter(|entry| entry["pluginId"] == CODEX_PROBE_SELECTOR)
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Count installed registrations whose source path is the owned native view.
fn codex_owned_identity(launcher: &str, home: &Path, codex_home: &Path, view: &Path) -> usize {
    codex_installed_entries(launcher, home, codex_home)
        .iter()
        .filter(|entry| {
            entry["source"]["path"]
                .as_str()
                .is_some_and(|path| Path::new(path).starts_with(view))
        })
        .count()
}

/// Switch the probe registration off through the config key that Codex 0.163.0
/// honors, then confirm `plugin list` reports it disabled. Returns the reason
/// when the switch is not supported.
fn disable_codex_plugin(launcher: &str, home: &Path, codex_home: &Path) -> Result<(), String> {
    let config = codex_home.join("config.toml");
    let text = std::fs::read_to_string(&config).map_err(|error| error.to_string())?;
    let section = text
        .find(&format!("[plugins.\"{CODEX_PROBE_SELECTOR}\"]"))
        .ok_or_else(|| "config.toml has no plugin section for the probe selector".to_string())?;
    let (head, tail) = text.split_at(section);
    if !tail.contains("enabled = true") {
        return Err("config.toml has no enabled = true key to switch".into());
    }
    let switched = format!(
        "{head}{}",
        tail.replacen("enabled = true", "enabled = false", 1)
    );
    std::fs::write(&config, switched).map_err(|error| error.to_string())?;
    let reported = codex_installed_entries(launcher, home, codex_home)
        .first()
        .and_then(|entry| entry["enabled"].as_bool());
    match reported {
        Some(false) => Ok(()),
        _ => Err("plugin list still reports the registration enabled".into()),
    }
}

fn run_codex_live_probe() {
    let Some(launcher) = launcher("codex") else {
        return probe_line("codex", "NotRun");
    };
    let (home, _lock) = isolated();
    let codex_home = home.root().join(".codex");
    let view = home.root().join(".ccync").join("native").join("codex");
    render_codex_view(&view);
    let view_root = view.to_string_lossy().into_owned();
    let version = cli_version(&launcher);

    let selector = NativeSelector::new(CODEX_PROBE_SELECTOR).expect("probe selector");
    let generation = p7_t05_generation("sha256:probe");
    // Codex reports no capability inventory, so the probe requires none.
    let capabilities = CapabilityInventory::new();
    install_native_registration(
        &ProcessNativeRunner,
        NativeExecutable::Codex,
        &NativeInstall {
            selector: &selector,
            marketplace_root: &view_root,
            generation: &generation,
            upstream_revision: None,
            capabilities: &capabilities,
        },
    )
    .expect("codex install through the adapter");

    let identity = codex_owned_identity(&launcher, home.root(), &codex_home, &view);
    let count = codex_skill_count(&launcher, home.root(), &codex_home);
    let disabled = disable_codex_plugin(&launcher, home.root(), &codex_home)
        .map(|()| codex_skill_count(&launcher, home.root(), &codex_home));
    let disabled_text = match &disabled {
        Ok(count) => count.to_string(),
        Err(reason) => format!("Unsupported reason={reason:?}"),
    };
    assert_roots_isolated(home.root(), &[codex_home.as_path(), view.as_path()]);
    probe_line(
        "codex",
        &format!(
            "identity={identity} count={count} disabled={disabled_text} version={version} roots={}",
            codex_home.display()
        ),
    );
    assert_eq!(identity, 1, "native identity must bind the owned view");
    assert_eq!(count, 1, "the enabled native skill must load once");
    if let Ok(disabled) = disabled {
        assert_eq!(disabled, 0, "the disabled native skill must not load");
    }
}

#[test]
#[ignore = "live OpenCode skill discovery probe requires an installed opencode CLI"]
fn p7_live_opencode() {
    run_opencode_live_probe();
}

/// Skill that the OpenCode probe delivers through the component route. The name
/// is unique so that no skill already on the machine can satisfy the count.
const OPENCODE_PROBE_SKILL: &str = "ccync-opencode-probe";

/// Plugin that carries the OpenCode probe skill as a bundled component.
const OPENCODE_PROBE_PLUGIN: &str = "opencode-probe";

/// Find the OpenCode CLI on PATH first, then in the Scoop shim directory of the
/// real profile. `None` means the CLI is absent from both places.
fn opencode_program() -> Option<String> {
    if let Some(program) = launcher("opencode") {
        return Some(program);
    }
    let shim = Path::new(&std::env::var_os("USERPROFILE")?)
        .join("scoop")
        .join("shims")
        .join("opencode.exe");
    shim.is_file().then(|| shim.to_string_lossy().into_owned())
}

/// Environment that points every OpenCode profile root into the isolated home.
fn opencode_env(home: &Path) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("HOME", home.to_path_buf()),
        ("USERPROFILE", home.to_path_buf()),
        ("XDG_CONFIG_HOME", home.join(".config")),
        ("XDG_DATA_HOME", home.join(".local/share")),
        ("XDG_STATE_HOME", home.join(".local/state")),
        ("XDG_CACHE_HOME", home.join(".cache")),
    ]
}

/// Run one `opencode` command in the isolated profile and return its output.
fn opencode_output(
    program: &str,
    home: &Path,
    project: &Path,
    args: &[&str],
) -> std::process::Output {
    Command::new(program)
        .args(args)
        .envs(
            opencode_env(home)
                .iter()
                .map(|(key, value)| (*key, value.as_path())),
        )
        .current_dir(project)
        .output()
        .expect("opencode runs")
}

/// Confirm that `opencode debug paths` places every profile root under the
/// isolated home. The `tmp` entry is the OS temp directory, not a profile root.
fn assert_opencode_paths_isolated(program: &str, home: &Path, project: &Path) {
    let output = opencode_output(program, home, project, &["debug", "paths"]);
    assert!(output.status.success(), "opencode debug paths failed");
    let text = String::from_utf8_lossy(&output.stdout);
    let mut checked = 0;
    for (key, path) in text
        .lines()
        .filter_map(|line| line.trim().split_once(char::is_whitespace))
    {
        if key == "tmp" {
            continue;
        }
        assert!(
            Path::new(path.trim()).starts_with(home),
            "opencode {key} path {path} escapes the isolated home {}",
            home.display()
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "opencode debug paths listed no profile root:\n{text}"
    );
}

/// Lower-case a path and use `/` separators, so that OpenCode's mixed
/// separators compare equal to the paths that ccync writes.
fn normalized_location(path: &str) -> String {
    path.replace('\\', "/").to_lowercase()
}

fn run_opencode_live_probe() {
    // Resolve the CLI before isolation rewrites USERPROFILE, so the Scoop shim
    // fallback still points at the real profile.
    let Some(program) = opencode_program() else {
        return probe_line("opencode", "NotRun");
    };
    let (home, _lock) = isolated();
    let project = home.root().join("project");
    std::fs::create_dir_all(&project).expect("probe project");
    assert_opencode_paths_isolated(&program, home.root(), &project);

    let payload = home
        .root()
        .join(".ccync/plugins")
        .join(OPENCODE_PROBE_PLUGIN);
    let skill_dir = payload.join("skills").join(OPENCODE_PROBE_SKILL);
    write(
        &payload.join(".claude-plugin/plugin.json"),
        r#"{"name":"opencode-probe","version":"0.3.0","description":"ccync OpenCode loader probe"}"#,
    );
    write(
        &skill_dir.join("SKILL.md"),
        &format!(
            "---\nname: {OPENCODE_PROBE_SKILL}\ndescription: Fixture skill for the OpenCode loader probe.\n---\n\nBody.\n"
        ),
    );
    write(
        &home.root().join(".ccync/build/lock.json"),
        &format!(
            r#"{{"resolvedPlugins":[{{"pluginId":"{OPENCODE_PROBE_PLUGIN}","resolvedSource":"{OPENCODE_PROBE_PLUGIN}","installStrategy":"bundled-local","sha":"{UPSTREAM_REVISION}"}}]}}"#
        ),
    );
    write(
        &home.root().join(".ccync/config.json"),
        r#"{"selectedProjectionTargets":["opencode"]}"#,
    );
    // With only the `opencode` target selected, the component route serves the
    // skill at OpenCode's own skill root, `~/.config/opencode/skills`. The probe
    // home is isolated, so this path is never the real profile's root.
    let skills_root = home.root().join(".config/opencode/skills");
    let delivered = skills_root.join(OPENCODE_PROBE_SKILL).join("SKILL.md");

    run_update_with_evidence(&ccync_engine::config::CcyncConfig::default())
        .expect("opencode sync delivers the component");
    assert!(delivered.is_file(), "opencode skill must be delivered");
    let version = cli_version(&program);
    probe_line(
        "opencode",
        &format!("version={version} roots={}", skills_root.display()),
    );
    probe_line(
        "opencode",
        "loader count not observed on this OpenCode version",
    );

    // Withdraw the component and sync again, so ccync retires its delivered copy.
    std::fs::remove_dir_all(&skill_dir).expect("withdraw probe skill");
    run_update_with_evidence(&ccync_engine::config::CcyncConfig::default())
        .expect("opencode sync retires the withdrawn component");
    assert!(
        !delivered.exists(),
        "withdrawn opencode skill must be removed"
    );
}

fn p7_t05_generation(digest: &str) -> ProviderGeneration {
    ProviderGeneration {
        source: ProviderSourceKey::new("demo").expect("source key"),
        generation: GenerationId::new("g-0001").expect("generation"),
        package_digest: PackageDigest::new(digest).expect("digest"),
    }
}

fn p7_t05_intent(target: ProjectionTargetId, selector: &str, digest: &str) -> PendingIntent {
    PendingIntent {
        target,
        ownership_fingerprints: vec![OwnershipFingerprint {
            selector: NativeSelector::new(selector).expect("selector"),
            package_digest: PackageDigest::new(digest).expect("digest"),
        }],
        scoped_paths: vec![ScopedPath {
            root: PathRoot::UserHome,
            path: RelativePath::new(".claude/skills/demo").expect("path"),
        }],
        preimage_refs: Vec::new(),
        previous_generation: None,
        desired_generation: p7_t05_generation(digest),
        phase: IntentPhase::Staged,
        client_scope: Some(ClientScope::new("demo-client").expect("scope")),
        source_digest: Some(PackageDigest::new(digest).expect("digest")),
        view_digest: Some(PackageDigest::new("sha256:view").expect("digest")),
        observed_preimage: None,
        enablement_intent: Some(EnablementIntent::Enabled),
        group_substep: None,
    }
}

fn p7_t05_ownership() -> CommittedOwnership {
    CommittedOwnership {
        target: ProjectionTargetId::Claude,
        selector: NativeSelector::new("demo@ccync").expect("selector"),
        plugin_name: "demo".to_string(),
        upstream_revision: Some(UPSTREAM_REVISION.to_string()),
        generation: p7_t05_generation("sha256:bb"),
        capabilities: Default::default(),
        client_scope: Some(ClientScope::new("demo-client").expect("scope")),
        source_digest: Some(PackageDigest::new("sha256:bb").expect("digest")),
        view_digest: Some(PackageDigest::new("sha256:view").expect("digest")),
        observed_preimage: None,
        enablement_intent: Some(EnablementIntent::Enabled),
        group_substep: None,
    }
}

fn commit_provider_state_layout(home: &Path) -> PathBuf {
    let (config, state, cache) = if cfg!(windows) {
        let appdata = home.join("AppData/Roaming");
        let local = home.join("AppData/Local");
        std::env::set_var("APPDATA", &appdata);
        std::env::set_var("LOCALAPPDATA", &local);
        (
            appdata.join("ccync"),
            local.join("ccync/state"),
            local.join("ccync/cache"),
        )
    } else {
        let config = home.join("xdg-config");
        let state = home.join("xdg-state");
        let cache = home.join("xdg-cache");
        std::env::set_var("XDG_CONFIG_HOME", &config);
        std::env::set_var("XDG_STATE_HOME", &state);
        std::env::set_var("XDG_CACHE_HOME", &cache);
        (
            config.join("ccync"),
            state.join("ccync"),
            cache.join("ccync"),
        )
    };
    let portable = home.join(".agents/ccync");
    for root in [&config, &state, &cache, &portable] {
        std::fs::create_dir_all(root).unwrap();
    }
    let commit = StorageLayoutCommit {
        layout_version: 1,
        roots: vec![
            LayoutRootIdentity {
                name: "portable".into(),
                path: portable,
            },
            LayoutRootIdentity {
                name: "config".into(),
                path: config,
            },
            LayoutRootIdentity {
                name: "state".into(),
                path: state.clone(),
            },
            LayoutRootIdentity {
                name: "cache".into(),
                path: cache,
            },
        ],
    };
    let bytes = serde_json::to_vec(&commit).unwrap();
    write(
        &home.join(".ccync/migration.json"),
        std::str::from_utf8(&bytes).unwrap(),
    );
    write(
        &state.join("layout.json"),
        std::str::from_utf8(&bytes).unwrap(),
    );
    state.join("build")
}

#[test]
fn provider_state_authority_routes_journal_and_lock_to_committed_build_root() {
    let (home, _lock) = isolated();
    let build = commit_provider_state_layout(home.root());
    std::fs::create_dir_all(&build).unwrap();
    let lock_path = build.join("lock.json");
    write(&lock_path, r#"{"resolvedPlugins":[{"pluginId":"kept"}]}"#);
    let legacy_lock = home.root().join(".ccync/build/lock.json");
    write(&legacy_lock, "legacy lock bytes");

    let mut intent = p7_t05_intent(ProjectionTargetId::Claude, "demo@ccync", "sha256:bb");
    intent.preimage_refs.push(PreimageRef {
        path: ScopedPath {
            root: PathRoot::UserHome,
            path: RelativePath::new(".claude/skills/demo").expect("destination"),
        },
        saved: RelativePath::new("preimages/provider-state").expect("saved path"),
    });
    write(&build.join("preimages/provider-state"), "saved preimage");
    record_native_intent(intent.clone()).expect("record intent");
    commit_native_ownership(p7_t05_ownership()).expect("commit ownership");

    let journal_path = build.join("projection.txn.json");
    let journal = ProviderLifecycleJournal::from_json_str(
        &std::fs::read_to_string(&journal_path).expect("committed journal"),
    )
    .expect("journal parses");
    assert_eq!(journal.schema_version, 3);
    assert_eq!(journal.intents, vec![intent.clone()]);
    assert_eq!(
        std::fs::read(build.join("preimages/provider-state")).unwrap(),
        b"saved preimage"
    );
    let lock: Value = serde_json::from_slice(&std::fs::read(&lock_path).unwrap()).unwrap();
    assert_eq!(lock["resolvedPlugins"][0]["pluginId"], "kept");
    assert_eq!(
        lock["_providerLifecycle"]["committed"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(std::fs::read(&legacy_lock).unwrap(), b"legacy lock bytes");
    assert!(!home
        .root()
        .join(".ccync/build/preimages/provider-state")
        .exists());

    clear_native_intent(&intent).expect("clear intent");
    assert!(ProviderLifecycleJournal::from_json_str(
        &std::fs::read_to_string(&journal_path).unwrap()
    )
    .unwrap()
    .intents
    .is_empty());
    assert!(!home
        .root()
        .join(".ccync/build/projection.txn.json")
        .exists());
}

#[test]
fn provider_state_authority_legacy_fixture_keeps_build_paths_and_journal_bytes() {
    let (home, _lock) = isolated();
    let build = home.root().join(".ccync/build");
    let lock_path = build.join("lock.json");
    write(&lock_path, r#"{"resolvedPlugins":[{"pluginId":"kept"}]}"#);
    let mut intent = p7_t05_intent(ProjectionTargetId::Claude, "demo@ccync", "sha256:bb");
    intent.preimage_refs.push(PreimageRef {
        path: ScopedPath {
            root: PathRoot::UserHome,
            path: RelativePath::new(".claude/skills/demo").expect("destination"),
        },
        saved: RelativePath::new("preimages/provider-state").expect("saved path"),
    });
    write(&build.join("preimages/provider-state"), "saved preimage");

    record_native_intent(intent.clone()).expect("record legacy intent");
    let journal_path = build.join("projection.txn.json");
    let expected = ProviderLifecycleJournal {
        schema_version: 3,
        intents: vec![intent.clone()],
        ..Default::default()
    };
    assert_eq!(
        std::fs::read(&journal_path).unwrap(),
        expected.to_json_string().unwrap().as_bytes()
    );

    commit_native_ownership(p7_t05_ownership()).expect("commit legacy ownership");
    let lock: Value = serde_json::from_slice(&std::fs::read(&lock_path).unwrap()).unwrap();
    assert_eq!(lock["resolvedPlugins"][0]["pluginId"], "kept");
    assert_eq!(
        lock["_providerLifecycle"]["committed"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        std::fs::read(build.join("preimages/provider-state")).unwrap(),
        b"saved preimage"
    );

    clear_native_intent(&intent).expect("clear legacy intent");
    assert_eq!(
        std::fs::read(&journal_path).unwrap(),
        ProviderLifecycleJournal::default()
            .to_json_string()
            .unwrap()
            .as_bytes()
    );
    assert!(!home
        .root()
        .join("AppData/Local/ccync/state/build/projection.txn.json")
        .exists());
}

#[cfg(windows)]
#[test]
fn provider_state_authority_io_failure_does_not_use_legacy_agy_ownership() {
    struct DenyDirectoryReadGuard<'a>(&'a Path);

    impl Drop for DenyDirectoryReadGuard<'_> {
        fn drop(&mut self) {
            let _ = Command::new("icacls.exe")
                .arg(self.0)
                .args(["/remove:d", "*S-1-1-0"])
                .status();
        }
    }

    let (home, _lock) = isolated();
    let legacy_build = home.root().join(".ccync/build");
    let legacy_lock = legacy_build.join("lock.json");
    let target = home.root().join(".gemini/antigravity-cli/plugins/ccync");
    let original_lock = br#"{"resolvedPlugins":[{"pluginId":"kept"}],"_ccyncProjection":{"schemaVersion":1,"agyRootProjectionPaths":["C:\\owned\\plugins\\ccync"]}}"#;
    std::fs::create_dir_all(&legacy_build).unwrap();
    std::fs::write(&legacy_lock, original_lock).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("user-data"), b"keep").unwrap();

    let status = Command::new("icacls.exe")
        .arg(&legacy_build)
        .args(["/deny", "*S-1-1-0:(RD)"])
        .status()
        .expect("icacls is available on Windows");
    assert!(
        status.success(),
        "icacls failed to restrict directory listing"
    );
    let _deny_guard = DenyDirectoryReadGuard(&legacy_build);

    if std::fs::read_dir(&legacy_build).is_ok() {
        println!("NOT EXERCISED: deny ACE did not block directory listing for this token");
        return;
    }
    println!("EXERCISED");

    assert_eq!(std::fs::read(&legacy_lock).unwrap(), original_lock);
    let layout = ccync_foundation::paths::EffectiveLayout::load();
    assert!(
        matches!(
            layout,
            Err(ccync_foundation::paths::LayoutLoadError::Io { .. })
        ),
        "unexpected layout result: {layout:?}"
    );

    let result = apply_agy_evidence(home.root(), true, false, false);
    assert!(result.is_err(), "AGY apply must fail closed on layout I/O");
    assert!(target.is_dir(), "AGY target directory remains intact");
    assert_eq!(
        std::fs::read(target.join("user-data")).unwrap(),
        b"keep",
        "AGY target contents remain intact"
    );
    assert_eq!(std::fs::read(&legacy_lock).unwrap(), original_lock);
}

#[test]
fn agy_evidence_fails_closed_when_layout_cannot_be_inspected() {
    let (home, _lock) = isolated();
    let target = home.root().join(".gemini/antigravity-cli/plugins/ccync");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("user-data"), b"keep").unwrap();
    std::fs::write(home.root().join(".ccync"), b"not a directory").unwrap();

    let result = apply_agy_evidence(home.root(), true, false, false);
    assert!(result.is_err(), "AGY apply must fail closed on layout I/O");
    assert!(target.is_dir(), "AGY target directory remains intact");
    assert_eq!(std::fs::read(target.join("user-data")).unwrap(), b"keep");
}

#[test]
fn p7_t05_intent_round_trip_preserves_other_operations_and_namespaces() {
    let (home, _lock) = isolated();
    let lock_path = home.root().join(".ccync/build/lock.json");
    let journal_path = ccync_foundation::paths::provider_lifecycle_journal_path().expect("journal");
    write(
        &lock_path,
        r#"{"resolvedPlugins":[{"pluginId":"keep"}],"_otherNamespace":{"kept":true}}"#,
    );

    let unrelated = p7_t05_intent(ProjectionTargetId::Codex, "other@market", "sha256:cc");
    record_native_intent(unrelated.clone()).expect("unrelated intent");
    let claude = p7_t05_intent(ProjectionTargetId::Claude, "demo@ccync", "sha256:bb");
    record_native_intent(claude.clone()).expect("claude intent");
    let journal = ProviderLifecycleJournal::from_json_str(
        &std::fs::read_to_string(&journal_path).expect("journal text"),
    )
    .expect("journal parses");
    assert_eq!(journal.intents, vec![unrelated.clone(), claude.clone()]);

    commit_native_ownership(p7_t05_ownership()).expect("commit ownership");
    let lock: Value =
        serde_json::from_str(&std::fs::read_to_string(&lock_path).expect("lock text"))
            .expect("lock parses");
    assert_eq!(lock["resolvedPlugins"][0]["pluginId"], "keep");
    assert_eq!(lock["_otherNamespace"]["kept"], true);
    assert_eq!(
        lock["_providerLifecycle"]["committed"]
            .as_array()
            .expect("committed rows")
            .len(),
        1
    );

    clear_native_intent(&claude).expect("clear claude intent");
    let journal = ProviderLifecycleJournal::from_json_str(
        &std::fs::read_to_string(&journal_path).expect("journal text"),
    )
    .expect("journal parses");
    assert_eq!(journal.intents, vec![unrelated]);
}

#[test]
fn p7_t05_malformed_or_unknown_journal_fails_before_any_write() {
    let (_home, _lock) = isolated();
    let journal_path = ccync_foundation::paths::provider_lifecycle_journal_path().expect("journal");
    let intent = p7_t05_intent(ProjectionTargetId::Claude, "demo@ccync", "sha256:bb");

    for original in ["{not json", r#"{"schemaVersion":99,"intents":[]}"#] {
        write(&journal_path, original);
        assert!(record_native_intent(intent.clone()).is_err());
        assert!(clear_native_intent(&intent).is_err());
        assert_eq!(
            std::fs::read_to_string(&journal_path).expect("journal text"),
            original
        );
    }
}

#[test]
fn p7_t05_commit_without_proof_leaves_lock_unchanged() {
    let (home, _lock) = isolated();
    let lock_path = home.root().join(".ccync/build/lock.json");
    let original = r#"{"resolvedPlugins":[]}"#;
    write(&lock_path, original);

    let mut record = p7_t05_ownership();
    record.client_scope = None;
    assert!(commit_native_ownership(record).is_err());
    assert_eq!(
        std::fs::read_to_string(&lock_path).expect("lock text"),
        original
    );
}

#[test]
#[ignore = "live Copilot CLI loose-route probe requires an installed copilot CLI"]
fn p7_live_copilot() {
    run_loose_live_probe(LooseProbe {
        client: "copilot",
        target: "copilot",
        program: "copilot",
        skill: COPILOT_PROBE_SKILL,
        plugin: COPILOT_PROBE_PLUGIN,
        listing: &["skill", "list", "--json"],
    });
}

#[test]
#[ignore = "live AGY CLI loose-route probe requires an installed agy CLI"]
fn p7_live_agy() {
    run_loose_live_probe(LooseProbe {
        client: "agy-cli",
        target: "agy-cli",
        program: "agy",
        skill: AGY_PROBE_SKILL,
        plugin: AGY_PROBE_PLUGIN,
        listing: &[
            "-p",
            "/skills",
            "--output-format",
            "json",
            "--print-timeout",
            "60s",
        ],
    });
}

/// Skill that the Copilot probe delivers through the loose route. The name is
/// unique so that no skill already on the machine can satisfy the count.
const COPILOT_PROBE_SKILL: &str = "ccync-copilot-probe";
/// Plugin that carries the Copilot probe skill as a bundled component.
const COPILOT_PROBE_PLUGIN: &str = "copilot-probe";
/// Skill that the AGY CLI probe delivers through the loose route.
const AGY_PROBE_SKILL: &str = "ccync-agy-probe";
/// Plugin that carries the AGY CLI probe skill as a bundled component.
const AGY_PROBE_PLUGIN: &str = "agy-probe";

/// One loose-route probe: the target it selects, the CLI that lists skills,
/// and the read-only listing arguments that CLI accepts.
struct LooseProbe {
    client: &'static str,
    target: &'static str,
    program: &'static str,
    skill: &'static str,
    plugin: &'static str,
    listing: &'static [&'static str],
}

/// Environment that points the probed CLI's profile into the isolated home.
fn loose_env(home: &Path) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("HOME", home.to_path_buf()),
        ("USERPROFILE", home.to_path_buf()),
        ("COPILOT_HOME", home.join(".copilot")),
    ]
}

/// Run one read-only listing of the probed CLI in the isolated profile and
/// return the entries it lists. An error names why the listing was not read.
fn loose_listing(program: &str, home: &Path, args: &[&str]) -> Result<Vec<Value>, String> {
    let output = Command::new(program)
        .args(args)
        .envs(
            loose_env(home)
                .iter()
                .map(|(key, value)| (*key, value.as_path())),
        )
        .current_dir(home)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("{program} did not start: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "`{program} {}` exited with {}",
            args.join(" "),
            output.status
        ));
    }
    let value: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("listing is not JSON: {error}"))?;
    match value {
        Value::Array(entries) => Ok(entries),
        Value::Object(map) => map
            .values()
            .find_map(|entry| entry.as_array().cloned())
            .ok_or_else(|| "listing has no entry list".to_string()),
        _ => Err("listing has no entry list".to_string()),
    }
}

/// Directories named `name` that hold a `SKILL.md` under `root`, searched to a
/// bounded depth. These are the loose copies ccync delivered into the profile.
fn loose_fixture_copies(root: &Path, name: &str, depth: usize) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for path in entries.flatten().map(|entry| entry.path()) {
        if !path.is_dir() {
            continue;
        }
        if path.file_name().is_some_and(|file| file == name) && path.join("SKILL.md").is_file() {
            found.push(path.clone());
        }
        if depth > 0 {
            found.extend(loose_fixture_copies(&path, name, depth - 1));
        }
    }
    found
}

/// For the fixture entries in a listing: how many point into the isolated home
/// (identity), and how many are not reported disabled (enabled loads). The
/// first fixture entry's source is returned as the displayed origin.
fn loose_fixture_counts(entries: &[Value], skill: &str, home: &Path) -> (usize, usize, String) {
    let home_prefix = normalized_location(&home.to_string_lossy());
    let fixture: Vec<&Value> = entries
        .iter()
        .filter(|entry| entry.get("name").and_then(Value::as_str) == Some(skill))
        .collect();
    let identity = fixture
        .iter()
        .filter(|entry| {
            entry.as_object().is_some_and(|fields| {
                fields.values().any(|value| {
                    value
                        .as_str()
                        .is_some_and(|text| normalized_location(text).starts_with(&home_prefix))
                })
            })
        })
        .count();
    let enabled = fixture
        .iter()
        .filter(|entry| entry.get("enabled").and_then(Value::as_bool) != Some(false))
        .count();
    let source = fixture
        .first()
        .and_then(|entry| entry.get("source"))
        .and_then(Value::as_str)
        .unwrap_or("not reported")
        .to_string();
    (identity, enabled, source)
}

fn run_loose_live_probe(probe: LooseProbe) {
    let Some(program) = launcher(probe.program) else {
        return probe_line(probe.client, "NotRun reason=CLI not found on PATH");
    };
    let (home, _lock) = isolated();
    let project = home.root().join("project");
    std::fs::create_dir_all(&project).expect("probe project");

    let payload = home.root().join(".ccync/plugins").join(probe.plugin);
    let skill_dir = payload.join("skills").join(probe.skill);
    write(
        &payload.join(".claude-plugin/plugin.json"),
        &format!(
            r#"{{"name":"{}","version":"0.3.0","description":"ccync loose-route loader probe"}}"#,
            probe.plugin
        ),
    );
    write(
        &skill_dir.join("SKILL.md"),
        &format!(
            "---\nname: {}\ndescription: Fixture skill for the {} loose-route probe.\n---\n\nBody.\n",
            probe.skill, probe.client
        ),
    );
    write(
        &home.root().join(".ccync/build/lock.json"),
        &format!(
            r#"{{"resolvedPlugins":[{{"pluginId":"{}","resolvedSource":"{}","installStrategy":"bundled-local","sha":"{UPSTREAM_REVISION}"}}]}}"#,
            probe.plugin, probe.plugin
        ),
    );
    write(
        &home.root().join(".ccync/config.json"),
        &format!(r#"{{"selectedProjectionTargets":["{}"]}}"#, probe.target),
    );

    run_update_with_evidence(&ccync_engine::config::CcyncConfig::default())
        .expect("loose-route sync delivers the component");
    let copies = loose_fixture_copies(home.root(), probe.skill, 6);
    if copies.is_empty() {
        return probe_line(
            probe.client,
            "NotRun reason=ccync delivered no loose copy into the isolated profile",
        );
    }
    let listing = match loose_listing(&program, home.root(), probe.listing) {
        Ok(entries) => entries,
        Err(reason) => return probe_line(probe.client, &format!("NotRun reason={reason}")),
    };
    let (identity, count, source) = loose_fixture_counts(&listing, probe.skill, home.root());
    let version = cli_version(&program);
    probe_line(
        probe.client,
        &format!(
            "identity={identity} count={count} displayed={} source={source} copies={} version={version}",
            probe.skill,
            copies.len()
        ),
    );

    // Withdraw the component and sync again, so ccync retires its delivered copy.
    std::fs::remove_dir_all(&skill_dir).expect("withdraw probe skill");
    run_update_with_evidence(&ccync_engine::config::CcyncConfig::default())
        .expect("loose-route sync retires the withdrawn component");
    let withdrawn = match loose_listing(&program, home.root(), probe.listing) {
        Ok(entries) => entries,
        Err(reason) => return probe_line(probe.client, &format!("NotRun reason={reason}")),
    };
    let (_, disabled, _) = loose_fixture_counts(&withdrawn, probe.skill, home.root());
    probe_line(probe.client, &format!("disabled={disabled}"));

    assert_eq!(
        identity, 1,
        "the delivered copy must be the one the CLI lists"
    );
    assert_eq!(count, 1, "the enabled fixture skill must load once");
    assert_eq!(disabled, 0, "the withdrawn fixture skill must not load");
}
