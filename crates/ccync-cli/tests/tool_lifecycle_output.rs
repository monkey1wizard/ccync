#![cfg(windows)]

use ccync_engine::tool_manager::{
    adapters::{gates::package_update_state, CapabilityState},
    model::AdapterKind,
};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    bin: PathBuf,
    interpreter: PathBuf,
    script: PathBuf,
    argv_log: PathBuf,
    package: PathBuf,
    metadata_pointer: PathBuf,
    failure_marker: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        // tempfile may return an 8.3 path on Windows. The CLI canonicalizes
        // discovered paths, so build selectors from the same canonical root.
        let root_path = root.path().canonicalize().unwrap();
        let home = root_path.join("home");
        let bin = root_path.join("bin");
        let package = root_path
            .join("site-packages")
            .join("graphifyy-0.9.71.dist-info");
        let scripts = root_path.join("scripts");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(home.join(".ccync")).unwrap();
        fs::write(home.join(".ccync/config.json"), "{}").unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&package).unwrap();
        fs::create_dir_all(&scripts).unwrap();

        let interpreter = bin.join("python.exe");
        let script = scripts.join("graphify.exe");
        let argv_log = root_path.join("argv.jsonl");
        let metadata_pointer = root_path.join("metadata-path.txt");
        let failure_marker = root_path.join("fail-index-check");
        fs::write(&metadata_pointer, package.to_string_lossy().as_bytes()).unwrap();
        fs::write(&script, b"fixture console script").unwrap();
        fs::write(
            package.join("entry_points.txt"),
            "[console_scripts]\ngraphify = graphify.cli:main\n",
        )
        .unwrap();

        let source = r#"
use std::{env, fs, io::Write, path::PathBuf};
fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let log = PathBuf::from(__ARGV_LOG__);
    let mut file = fs::OpenOptions::new().create(true).append(true).open(log).unwrap();
    writeln!(file, "{:?}", args).unwrap();
    if args.iter().any(|a| a == "-c") {
        println!("{{\"executable\":{:?},\"prefix\":{:?},\"scripts\":{:?}}}", __INTERPRETER__, __PREFIX__, __SCRIPTS__);
    } else if args.get(0).map(String::as_str) == Some("-m") && args.get(1).map(String::as_str) == Some("pip") {
        if args.get(2).map(String::as_str) == Some("inspect") {
            let package = fs::read_to_string(__METADATA_POINTER__).unwrap();
            let version = if package.contains("0.9.73") { "0.9.73" } else { "0.9.71" };
            println!("{{\"installed\":[{{\"metadata\":{{\"name\":\"graphifyy\",\"version\":{:?}}},\"metadata_location\":{:?}}}]}}", version, package);
        } else if args.get(2).map(String::as_str) == Some("index") {
            if fs::metadata(__FAILURE_MARKER__).is_ok() {
                eprintln!("credential=hidden https://user:secret@example.test");
                std::process::exit(7);
            }
            println!("{{\"name\":\"graphifyy\",\"latest\":\"1.0.0\",\"installed_version\":\"0.9.73\",\"versions\":[\"0.9.73\",\"1.0.0\"]}}");
        }
    } else if args == ["-m", "graphify", "install", "--project", "agents"] {
        fs::create_dir_all(env::current_dir().unwrap().join(".agents/skills/graphify")).unwrap();
    }
}
"#;
        let source = source
            .replace("__ARGV_LOG__", &rust_string(&argv_log))
            .replace("__INTERPRETER__", &rust_string(&interpreter))
            .replace("__PREFIX__", &rust_string(&root_path))
            .replace("__SCRIPTS__", &rust_string(&scripts))
            .replace("__METADATA_POINTER__", &rust_string(&metadata_pointer));
        let source = source.replace("__FAILURE_MARKER__", &rust_string(&failure_marker));
        let source_path = root_path.join("fake_manager.rs");
        fs::write(&source_path, source).unwrap();
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let status = Command::new(rustc)
            .args(["--edition=2021"])
            .arg(&source_path)
            .arg("-o")
            .arg(&interpreter)
            .status()
            .expect("compile fake interpreter manager");
        assert!(status.success(), "fake manager compilation failed");

        Self {
            _root: root,
            home,
            bin,
            interpreter,
            script,
            argv_log,
            package,
            metadata_pointer,
            failure_marker,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ccync"))
            .args(args)
            .env("USERPROFILE", &self.home)
            .env("HOME", &self.home)
            .env("APPDATA", self.home.join("AppData/Roaming"))
            .env("LOCALAPPDATA", self.home.join("AppData/Local"))
            .env("CARGO_HOME", self.home.join(".cargo"))
            .env("PATH", &self.bin)
            .env("CCYNC_ASSUME_YES", "1")
            .output()
            .unwrap()
    }

    fn argv(&self) -> Vec<Vec<String>> {
        fs::read_to_string(&self.argv_log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

fn rust_string(path: &Path) -> String {
    format!("r#\"{}\"#", path.to_string_lossy())
}

fn assert_json_document(output: &Output, expected_exit: i32, expected_outcome: &str) -> Value {
    assert_eq!(
        output.status.code(),
        Some(expected_exit),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout.clone()).unwrap();
    let value: Value = serde_json::from_str(&text).unwrap_or_else(|error| {
        panic!("stdout contains exactly one JSON document: {error}; stdout={text}")
    });
    assert_eq!(value["outcome"], expected_outcome);
    value
}

#[test]
fn process_tool_lifecycle_human_and_json_contract() {
    let fixture = Fixture::new();
    let script = fixture.script.to_string_lossy().into_owned();

    let discover_json = fixture.run(&["tool", "discover", &script, "--json"]);
    let discovered = assert_json_document(&discover_json, 3, "ambiguous");
    let candidate_id = discovered["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| candidate["adapter"] == "interpreter-pip")
        .unwrap()["candidateId"]
        .as_str()
        .unwrap()
        .to_owned();
    let discover_human = fixture.run(&["tool", "discover", &script]);
    assert_eq!(discover_human.status.code(), Some(3));
    assert!(discover_human.stderr.is_empty());
    assert!(String::from_utf8_lossy(&discover_human.stdout).contains("ambiguous"));

    let track = fixture.run(&[
        "tool",
        "track",
        "graphify",
        "--candidate",
        &candidate_id,
        "--recipe",
        "graphify-agents",
        "--yes",
    ]);
    assert_eq!(
        track.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&track.stdout),
        String::from_utf8_lossy(&track.stderr)
    );
    assert!(
        track.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&track.stderr)
    );
    let track_text = String::from_utf8_lossy(&track.stdout);
    assert!(track_text.contains("tracked"), "{track_text}");
    assert!(!track_text.to_ascii_lowercase().contains("reload"));

    // External version drift preserves anchors and removes the old metadata leaf.
    let next_package = fixture.package.with_file_name("graphifyy-0.9.73.dist-info");
    fs::rename(&fixture.package, &next_package).unwrap();
    fs::write(
        &fixture.metadata_pointer,
        next_package.to_string_lossy().as_bytes(),
    )
    .unwrap();
    fs::write(&fixture.script, b"externally updated console script").unwrap();
    let outdated_json = fixture.run(&["outdated", "--json"]);
    let outdated = assert_json_document(&outdated_json, 3, "update-available");
    assert_eq!(outdated["tools"][0]["id"], "graphify");
    assert_eq!(outdated["tools"][0]["updateFreshness"], "fresh");
    let outdated_human = fixture.run(&["outdated"]);
    assert_eq!(outdated_human.status.code(), Some(3));
    assert!(outdated_human.stderr.is_empty());
    let outdated_text = String::from_utf8_lossy(&outdated_human.stdout);
    assert!(outdated_text.contains("graphify"));
    assert!(outdated_text.contains("freshness=Fresh"));

    // A failed remote check retains a safe reason on stderr and one JSON document on stdout.
    fs::write(&fixture.failure_marker, b"fail").unwrap();
    let state_before_failed_check = fs::read(&fixture.home.join(".ccync/build/tools/state.json")).ok();
    let calls_before_failed_check = fixture.argv().len();
    let offline_json = fixture.run(&["outdated", "--offline", "--json"]);
    let _: Value = serde_json::from_slice(&offline_json.stdout).expect("offline JSON document");
    assert_eq!(offline_json.status.code(), Some(0));
    assert!(offline_json.stderr.is_empty());
    assert_eq!(fixture.argv().len(), calls_before_failed_check, "offline outdated must make no remote calls");
    let failed_json = fixture.run(&["outdated", "--json"]);
    assert_eq!(failed_json.status.code(), Some(1));
    let failed_stdout = String::from_utf8(failed_json.stdout.clone()).unwrap();
    let failed_value: Value = serde_json::from_str(&failed_stdout).expect("one JSON document");
    assert_eq!(failed_value["outcome"], "check-failed");
    assert_eq!(failed_value["tools"][0]["outcome"], "check-failed");
    let failed_stderr = String::from_utf8_lossy(&failed_json.stderr);
    assert!(failed_stderr.contains("graphify"));
    assert!(failed_stderr.contains("remote check exited unsuccessfully"));
    assert!(!failed_stderr.contains("credential"));
    assert!(!failed_stderr.contains("https://"));
    assert!(!failed_stderr.contains("secret"));
    let new_calls = fixture.argv().into_iter().skip(calls_before_failed_check).collect::<Vec<_>>();
    assert_eq!(new_calls.iter().filter(|args| args.iter().any(|arg| arg == "index")).count(), 1);
    assert_eq!(fs::read(&fixture.home.join(".ccync/build/tools/state.json")).ok(), state_before_failed_check);
    fs::remove_file(&fixture.failure_marker).unwrap();

    let state_path = fixture.home.join(".ccync/build/tools/state.json");
    let state_before_dry_run = fs::read(&state_path).ok();
    let declaration_before_dry_run = fs::read(fixture.home.join(".ccync/tools.json")).unwrap();
    let build_before_dry_run = managed_files(&fixture.home.join(".ccync/build"));
    let refresh_json = fixture.run(&["tool", "refresh", "graphify", "--dry-run", "--json"]);
    let refresh = assert_json_document(&refresh_json, 0, "dry-run");
    assert!(refresh["reloadAdvice"].is_null());
    let refresh_human = fixture.run(&["tool", "refresh", "graphify", "--dry-run"]);
    assert_eq!(refresh_human.status.code(), Some(0));
    assert!(refresh_human.stderr.is_empty());
    assert!(String::from_utf8_lossy(&refresh_human.stdout).contains("dry-run"));
    assert_eq!(fs::read(&state_path).ok(), state_before_dry_run);
    assert_eq!(
        fs::read(fixture.home.join(".ccync/tools.json")).unwrap(),
        declaration_before_dry_run
    );
    assert_eq!(
        managed_files(&fixture.home.join(".ccync/build")),
        build_before_dry_run
    );

    let committed_refresh = fixture.run(&["tool", "refresh", "graphify"]);
    assert_eq!(committed_refresh.status.code(), Some(0));
    assert!(committed_refresh.stderr.is_empty());
    let committed_text = String::from_utf8_lossy(&committed_refresh.stdout);
    assert!(committed_text.contains("committed"), "{committed_text}");
    assert!(
        committed_text.contains("reload advised"),
        "{committed_text}"
    );
    assert!(
        state_path.exists(),
        "committed refresh must create fresh state"
    );

    let declaration_before = fs::read(fixture.home.join(".ccync/tools.json")).unwrap();
    let build_before = managed_files(&fixture.home.join(".ccync/build"));
    let argv_before_upgrade = fixture.argv();
    let upgrade_json = fixture.run(&["upgrade", "tool:graphify", "--dry-run", "--json"]);
    let upgrade_human = fixture.run(&["upgrade", "tool:graphify", "--dry-run"]);
    match package_update_state(AdapterKind::InterpreterPip) {
        CapabilityState::Gated => {
            assert_json_document(&upgrade_json, 3, "blocked");
            assert_eq!(upgrade_human.status.code(), Some(3));
            assert!(upgrade_human.stdout.is_empty());
            assert!(String::from_utf8_lossy(&upgrade_human.stderr)
                .contains("package update is unsupported"));
        }
        CapabilityState::Supported => {
            let value = assert_json_document(&upgrade_json, 0, "dry-run");
            assert!(value["reloadAdvice"].is_null());
            assert_eq!(value["projectionChanged"], false);
            assert_eq!(upgrade_human.status.code(), Some(0));
            assert!(upgrade_human.stderr.is_empty());
            assert!(String::from_utf8_lossy(&upgrade_human.stdout).contains("dry-run"));
        }
        state => panic!("unexpected pip gate state: {state:?}"),
    }
    assert_eq!(
        fs::read(fixture.home.join(".ccync/tools.json")).unwrap(),
        declaration_before
    );
    assert_eq!(
        managed_files(&fixture.home.join(".ccync/build")),
        build_before
    );
    let mutations = |calls: Vec<Vec<String>>| -> Vec<Vec<String>> {
        calls
            .into_iter()
            .filter(|args| args.iter().any(|arg| arg == "install"))
            .collect()
    };
    assert_eq!(
        mutations(fixture.argv()),
        mutations(argv_before_upgrade),
        "upgrade dry-run must not execute package installation or recipe generation"
    );
    assert!(!fixture
        .argv()
        .iter()
        .any(|args| args.iter().map(String::as_str).eq([
            "-m",
            "pip",
            "install",
            "--upgrade",
            "graphifyy"
        ])));

    let untrack = fixture.run(&["tool", "untrack", "graphify", "--yes"]);
    assert_eq!(untrack.status.code(), Some(0));
    assert!(
        untrack.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&untrack.stderr)
    );
    let untrack_text = String::from_utf8_lossy(&untrack.stdout);
    assert!(untrack_text.contains("untracked"));
    assert!(
        untrack_text.contains("projection changed: true"),
        "{untrack_text}"
    );
    assert!(untrack_text.contains("reload advised"), "{untrack_text}");

    let argv = fixture.argv();
    assert!(argv.iter().any(|args| args.iter().map(String::as_str).eq(["-c", "import json,sys,sysconfig;print(json.dumps({'executable':sys.executable,'prefix':sys.prefix,'scripts':sysconfig.get_path('scripts')}))"])));
    assert!(argv
        .iter()
        .any(|args| args.iter().map(String::as_str).eq(["-m", "pip", "inspect"])));
    assert!(argv.iter().any(|args| args.iter().map(String::as_str).eq([
        "-m",
        "pip",
        "index",
        "versions",
        "graphifyy",
        "--json",
        "--disable-pip-version-check"
    ])));
    assert!(fixture.interpreter.exists());

    for args in [
        vec!["tool", "track", "graphify", "--json"],
        vec!["tool", "untrack", "graphify", "--json"],
    ] {
        let output = fixture.run(&args);
        assert_eq!(output.status.code(), Some(64));
        assert!(
            output.stdout.is_empty(),
            "usage diagnostics belong on stderr"
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("does not support --json"));
    }
}

fn managed_files(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, files: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut files = std::collections::BTreeMap::new();
    visit(root, root, &mut files);
    files
}
