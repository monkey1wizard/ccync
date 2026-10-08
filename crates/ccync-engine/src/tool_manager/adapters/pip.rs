//! Interpreter-bound pip discovery. The interpreter path is always explicit.

use super::{AdapterCapabilities, AdapterError, CapabilityState, RemoteUpdateStatus, ToolAdapter};
use crate::tool_manager::{
    discovery::{DiscoveryCandidate, DiscoveryQuery},
    file_identity::{hash_same_handle, is_reparse_or_link},
    model::{AdapterKind, EvidenceLevel, InstallationIdentity},
    probe::{ProbeRequest, ProbeRunner, SystemProbeRunner},
};
use pep440_rs::Version;
use serde_json::Value;
use std::str::FromStr;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

const PROBE: &str = "import json,sys,sysconfig;print(json.dumps({'executable':sys.executable,'prefix':sys.prefix,'scripts':sysconfig.get_path('scripts')}))";
const OUTPUT_CAP: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct PipAdapter {
    interpreter: Option<PathBuf>,
    distribution: Option<String>,
    script: String,
    recorded: Option<InstallationIdentity>,
}

impl PipAdapter {
    /// Construct an adapter for one explicit interpreter, distribution, and console script.
    pub fn new(
        interpreter: impl Into<PathBuf>,
        distribution: impl Into<String>,
        script: impl Into<String>,
    ) -> Self {
        Self {
            interpreter: Some(interpreter.into()),
            distribution: Some(normalize_name(&distribution.into())),
            script: script.into(),
            recorded: None,
        }
    }

    /// Construct an adapter from a previously observed pip identity.
    pub fn for_recorded_update(identity: InstallationIdentity) -> Self {
        Self {
            interpreter: identity.fields.get("interpreter").map(PathBuf::from),
            distribution: identity.fields.get("distribution").cloned(),
            script: identity
                .fields
                .get("entry_point")
                .cloned()
                .unwrap_or_default(),
            recorded: Some(identity),
        }
    }

    fn candidates(
        &self,
        runner: &dyn ProbeRunner,
    ) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
        let resolved_interpreter;
        let Some(interpreter) = (match self.interpreter.as_deref() {
            Some(path) => Some(path),
            None => {
                resolved_interpreter = match resolve_python() {
                    Some(path) => path,
                    None => return Ok(vec![]),
                };
                Some(resolved_interpreter.as_path())
            }
        }) else {
            return Ok(vec![]);
        };
        let interpreter = match canonical_interpreter(interpreter) {
            Ok(path) => path,
            Err(_) => return Ok(vec![]),
        };
        let probe = run_json(runner, &interpreter, &["-c", PROBE])?;
        let Some(executable) = probe.get("executable").and_then(Value::as_str) else {
            return Ok(vec![]);
        };
        let Some(prefix) = probe.get("prefix").and_then(Value::as_str) else {
            return Ok(vec![]);
        };
        let Some(scripts) = probe.get("scripts").and_then(Value::as_str) else {
            return Ok(vec![]);
        };
        if !same_path(Path::new(executable), &interpreter) {
            return Ok(vec![]);
        }
        let inspect = run_json(runner, &interpreter, &["-m", "pip", "inspect"])?;
        let Some(installed) = inspect.get("installed").and_then(Value::as_array) else {
            return Ok(vec![]);
        };
        let scripts = PathBuf::from(scripts);
        if !scripts.is_absolute() || path_has_link_component(&scripts) {
            return Ok(vec![]);
        }
        let scripts = match fs::canonicalize(&scripts) {
            Ok(path) => path,
            Err(_) => return Ok(vec![]),
        };
        let mut out = Vec::new();
        for item in installed {
            let Some(metadata) = item.get("metadata") else {
                continue;
            };
            let (Some(name), Some(version), Some(location)) = (
                metadata.get("name").and_then(Value::as_str),
                metadata.get("version").and_then(Value::as_str),
                item.get("metadata_location").and_then(Value::as_str),
            ) else {
                continue;
            };
            let location = PathBuf::from(location);
            if !metadata_location_is_owned(&location, name, version) {
                continue;
            }
            let entry_file = location.join("entry_points.txt");
            if is_reparse_or_link(&entry_file).unwrap_or(true) {
                continue;
            }
            let Ok(contents) = fs::read_to_string(&entry_file) else {
                continue;
            };
            let entries = if self.script.is_empty() {
                console_scripts(&contents)
            } else {
                console_script(&contents, &self.script)
                    .map(|target| vec![(self.script.as_str(), target)])
                    .unwrap_or_default()
            };
            // A tool's delivery/display name can differ from its Python
            // distribution name. Resolve the distribution from the installed
            // package that owns the requested console script.
            if self
                .distribution
                .as_deref()
                .is_some_and(|expected| normalize_name(name) != expected && entries.is_empty())
            {
                continue;
            }
            for (script, target) in entries {
                let script = script.to_owned();
                let mut components = Path::new(&script).components();
                if !matches!(components.next(), Some(std::path::Component::Normal(_)))
                    || components.next().is_some()
                {
                    continue;
                }
                let script_path = scripts.join(script_filename(&script));
                if !target.contains(':')
                    || !script_path.is_file()
                    || is_reparse_or_link(&script_path).unwrap_or(true)
                {
                    continue;
                }
                let Ok(hash) = hash_same_handle(&script_path) else {
                    continue;
                };
                let mut fields = BTreeMap::new();
                fields.insert(
                    "interpreter".into(),
                    interpreter.to_string_lossy().into_owned(),
                );
                fields.insert("prefix".into(), prefix.to_owned());
                fields.insert("distribution".into(), normalize_name(name));
                fields.insert("version".into(), version.to_owned());
                fields.insert("location".into(), location.to_string_lossy().into_owned());
                fields.insert("entry_point".into(), script);
                fields.insert("entry_target".into(), target);
                fields.insert("scripts".into(), scripts.to_string_lossy().into_owned());
                fields.insert(
                    "executable".into(),
                    script_path.to_string_lossy().into_owned(),
                );
                fields.insert("hash".into(), hash);
                if self
                    .recorded
                    .as_ref()
                    .is_some_and(|recorded| !recorded_identity_matches(recorded, &fields))
                {
                    continue;
                }
                out.push(DiscoveryCandidate::new(
                    InstallationIdentity {
                        adapter_kind: AdapterKind::InterpreterPip,
                        fields,
                    },
                    EvidenceLevel::Verified,
                    true,
                    true,
                    false,
                ));
            }
        }
        Ok(out)
    }
}

impl ToolAdapter for PipAdapter {
    fn kind(&self) -> AdapterKind {
        AdapterKind::InterpreterPip
    }
    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities::for_adapter(
            AdapterKind::InterpreterPip,
            CapabilityState::Supported,
            CapabilityState::Supported,
            CapabilityState::Supported,
            CapabilityState::Supported,
        )
    }
    fn discover(&self, _query: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
        self.candidates(&SystemProbeRunner)
    }
    fn reprobe(&self, _query: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
        self.candidates(&SystemProbeRunner)
    }
    fn remote_update_status(
        &self,
        query: &DiscoveryQuery,
    ) -> Result<RemoteUpdateStatus, AdapterError> {
        if !query.online {
            return Ok(RemoteUpdateStatus::NotChecked);
        }
        remote_update_status_with_runner(self.recorded.as_ref(), &SystemProbeRunner)
    }
    fn update_argv(&self) -> Option<Vec<String>> {
        update_argv(self.recorded.as_ref()?)
    }
}

fn remote_update_status_with_runner(
    identity: Option<&InstallationIdentity>,
    runner: &dyn ProbeRunner,
) -> Result<RemoteUpdateStatus, AdapterError> {
    let identity = identity
        .ok_or_else(|| AdapterError::Failed("interpreter pip identity is unavailable".into()))?;
    if identity.adapter_kind != AdapterKind::InterpreterPip {
        return Err(AdapterError::Failed(
            "interpreter pip identity has the wrong adapter kind".into(),
        ));
    }
    let interpreter = identity.fields.get("interpreter").ok_or_else(|| {
        AdapterError::Failed("interpreter pip identity has no interpreter".into())
    })?;
    let distribution = identity.fields.get("distribution").ok_or_else(|| {
        AdapterError::Failed("interpreter pip identity has no distribution".into())
    })?;
    if !Path::new(interpreter).is_absolute() {
        return Err(AdapterError::Failed(
            "interpreter pip identity has a non-absolute interpreter".into(),
        ));
    }

    let tracked = normalize_name(distribution);
    let output = run_json(
        runner,
        Path::new(interpreter),
        &[
            "-m",
            "pip",
            "index",
            "versions",
            &tracked,
            "--json",
            "--disable-pip-version-check",
        ],
    )?;
    let object = output.as_object().ok_or_else(|| {
        AdapterError::Failed("interpreter pip returned an invalid index response".into())
    })?;
    let name = object.get("name").and_then(Value::as_str).ok_or_else(|| {
        AdapterError::Failed("interpreter pip index response has no distribution name".into())
    })?;
    if normalize_name(name) != tracked {
        return Err(AdapterError::Failed(
            "interpreter pip index response names a different distribution".into(),
        ));
    }
    let latest = object
        .get("latest")
        .and_then(Value::as_str)
        .and_then(|value| Version::from_str(value).ok())
        .ok_or_else(|| {
            AdapterError::Failed("interpreter pip index latest version is invalid".into())
        })?;
    let installed = object
        .get("installed_version")
        .and_then(Value::as_str)
        .and_then(|value| Version::from_str(value).ok())
        .ok_or_else(|| {
            AdapterError::Failed("interpreter pip index installed version is invalid".into())
        })?;
    let versions = object
        .get("versions")
        .and_then(Value::as_array)
        .filter(|versions| !versions.is_empty())
        .ok_or_else(|| AdapterError::Failed("interpreter pip index versions are missing".into()))?;
    let versions = versions
        .iter()
        .map(|value| {
            value
                .as_str()
                .and_then(|version| Version::from_str(version).ok())
                .ok_or_else(|| {
                    AdapterError::Failed("interpreter pip index contains an invalid version".into())
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if versions.iter().max() != Some(&latest) {
        return Err(AdapterError::Failed(
            "interpreter pip index latest version is inconsistent".into(),
        ));
    }
    Ok(if latest > installed {
        RemoteUpdateStatus::UpdateAvailable
    } else {
        RemoteUpdateStatus::Current
    })
}

pub fn update_argv(identity: &InstallationIdentity) -> Option<Vec<String>> {
    if identity.adapter_kind != AdapterKind::InterpreterPip {
        return None;
    }
    let interpreter = identity.fields.get("interpreter")?;
    let distribution = identity.fields.get("distribution")?;
    if !Path::new(interpreter).is_absolute() || normalize_name(distribution) != *distribution {
        return None;
    }
    Some(vec![
        interpreter.clone(),
        "-m".into(),
        "pip".into(),
        "install".into(),
        "--upgrade".into(),
        distribution.clone(),
    ])
}

fn canonical_interpreter(path: &Path) -> Result<PathBuf, ()> {
    if !path.is_absolute() || path_has_link_component(path) {
        return Err(());
    }
    let canonical = fs::canonicalize(path).map_err(|_| ())?;
    if !same_path(&canonical, path) || !canonical.is_file() {
        return Err(());
    }
    Ok(canonical)
}

fn metadata_location_is_owned(location: &Path, name: &str, version: &str) -> bool {
    if !location.is_absolute() || path_has_link_component(location) {
        return false;
    }
    let Some(parent) = location.parent() else {
        return false;
    };
    let Ok(canonical_parent) = fs::canonicalize(parent) else {
        return false;
    };
    if !same_path(parent, &canonical_parent) || !location.is_dir() {
        return false;
    }
    if !fs::canonicalize(location)
        .ok()
        .is_some_and(|canonical| same_path(&canonical, location))
    {
        return false;
    }
    let leaf = location
        .file_name()
        .and_then(|part| part.to_str())
        .unwrap_or_default();
    let Some(stem) = leaf.strip_suffix(".dist-info") else {
        return false;
    };
    let Some((distribution, installed_version)) = stem.rsplit_once('-') else {
        return false;
    };
    !distribution.is_empty()
        && !version.is_empty()
        && normalize_name(distribution) == normalize_name(name)
        && installed_version == version
}

fn path_has_link_component(path: &Path) -> bool {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match component {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => continue,
            std::path::Component::ParentDir => return true,
            _ => {}
        }
        if is_reparse_or_link(&current).unwrap_or(true) {
            return true;
        }
    }
    false
}

fn recorded_identity_matches(
    recorded: &InstallationIdentity,
    observed: &BTreeMap<String, String>,
) -> bool {
    let anchors = [
        "interpreter",
        "prefix",
        "scripts",
        "distribution",
        "entry_point",
        "entry_target",
        "executable",
    ];
    if anchors.iter().any(
        |key| match (recorded.fields.get(*key), observed.get(*key)) {
            (Some(old), Some(new))
                if matches!(*key, "interpreter" | "prefix" | "scripts" | "executable") =>
            {
                !same_path(Path::new(old), Path::new(new))
            }
            (Some(old), Some(new)) if *key == "distribution" => {
                normalize_name(old) != normalize_name(new)
            }
            (Some(old), Some(new)) => old != new,
            _ => true,
        },
    ) {
        return false;
    }
    let Some(old_location) = recorded.fields.get("location").map(PathBuf::from) else {
        return false;
    };
    let Some(new_location) = observed.get("location").map(PathBuf::from) else {
        return false;
    };
    if !old_location.is_absolute() || !new_location.is_absolute() {
        return false;
    }
    let (Some(old_parent), Some(new_parent)) = (old_location.parent(), new_location.parent())
    else {
        return false;
    };
    let (Ok(old_parent), Ok(new_parent)) =
        (fs::canonicalize(old_parent), fs::canonicalize(new_parent))
    else {
        return false;
    };
    same_path(&old_parent, &new_parent)
        && !path_has_link_component(old_location.parent().unwrap_or(Path::new("")))
}

fn same_path(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        fn without_verbatim_prefix(path: &Path) -> String {
            let path = path.to_string_lossy();
            path.strip_prefix(r"\\?\").unwrap_or(&path).to_owned()
        }
        without_verbatim_prefix(left).eq_ignore_ascii_case(&without_verbatim_prefix(right))
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

fn run_json(
    runner: &dyn ProbeRunner,
    interpreter: &Path,
    argv: &[&str],
) -> Result<Value, AdapterError> {
    let request = ProbeRequest {
        program: interpreter.to_owned(),
        argv: argv.iter().map(|s| (*s).into()).collect(),
        env: BTreeMap::new(),
        working_directory: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        timeout: Duration::from_secs(20),
        output_byte_cap: OUTPUT_CAP,
    };
    let output = runner.run(&request);
    if output.timed_out {
        return Err(AdapterError::Failed(
            "interpreter pip probe timed out".into(),
        ));
    }
    if output.stdout_truncated || output.stderr_truncated {
        return Err(AdapterError::Failed(
            "interpreter pip probe output exceeded limit".into(),
        ));
    }
    if output.spawn_error.is_some() {
        return Err(AdapterError::Failed(
            "interpreter pip probe could not start".into(),
        ));
    }
    if output.exit_code != Some(0) {
        return Err(AdapterError::Failed(format!(
            "interpreter pip probe exited unsuccessfully (exit={:?})",
            output.exit_code
        )));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|_| AdapterError::Failed("interpreter pip returned invalid JSON".into()))
}

fn normalize_name(name: &str) -> String {
    let mut normalized = String::new();
    let mut separator = false;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            separator = true;
        } else {
            if separator && !normalized.is_empty() {
                normalized.push('-');
            }
            separator = false;
            normalized.extend(c.to_lowercase());
        }
    }
    normalized
}
fn resolve_python() -> Option<PathBuf> {
    let filename = if cfg!(windows) {
        "python.exe"
    } else {
        "python"
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(filename))
        .find(|path| path.is_file() && !is_reparse_or_link(path).unwrap_or(true))
}

fn console_scripts(contents: &str) -> Vec<(&str, String)> {
    let mut section = false;
    let mut entries = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            section = line.eq_ignore_ascii_case("[console_scripts]");
        } else if section {
            if let Some((name, target)) = line.split_once('=') {
                let name = name.trim();
                let target = target.trim();
                if !name.is_empty() && target.contains(':') {
                    entries.push((name, target.to_owned()));
                }
            }
        }
    }
    entries
}
fn console_script(contents: &str, script: &str) -> Option<String> {
    let mut section = false;
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            section = line.eq_ignore_ascii_case("[console_scripts]");
            continue;
        }
        if section {
            let (name, target) = line.split_once('=')?;
            if name.trim() == script {
                return Some(target.trim().to_owned());
            }
        }
    }
    None
}
fn script_filename(script: &str) -> String {
    if cfg!(windows) {
        format!("{script}.exe")
    } else {
        script.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_manager::probe::{FixtureProbeRunner, ProbeOutput};

    #[cfg(windows)]
    #[test]
    fn verbatim_and_plain_interpreter_paths_compare_equal() {
        let plain = Path::new(r"C:\Python314\python.exe");
        let verbatim = Path::new(r"\\?\C:\Python314\python.exe");
        let different = Path::new(r"C:\Python313\python.exe");

        assert!(same_path(verbatim, plain));
        assert!(!same_path(verbatim, different));
    }

    fn setup(
        distribution: &str,
        entry: &str,
        prefix_change: bool,
    ) -> (tempfile::TempDir, PathBuf, FixtureProbeRunner) {
        let dir = tempfile::tempdir().unwrap();
        let interpreter = dir.path().join(if cfg!(windows) {
            "python.exe"
        } else {
            "python"
        });
        fs::write(&interpreter, b"interpreter fixture").unwrap();
        let canonical = fs::canonicalize(&interpreter).unwrap();
        let scripts = dir.path().join("scripts");
        fs::create_dir(&scripts).unwrap();
        fs::write(scripts.join(script_filename("graphify")), b"script fixture").unwrap();
        let location = dir
            .path()
            .join("site-packages")
            .join("graphifyy-1.0.dist-info");
        fs::create_dir_all(&location).unwrap();
        fs::write(
            location.join("entry_points.txt"),
            format!("[console_scripts]\ngraphify = {entry}\n"),
        )
        .unwrap();
        let probe = serde_json::json!({"executable":canonical,"prefix":if prefix_change {"/changed"} else {"/python"},"scripts":scripts});
        let inspect = serde_json::json!({"installed":[{"metadata":{"name":distribution,"version":"1.0"},"metadata_location":location}]});
        let outputs = [probe, inspect].into_iter().map(|v| ProbeOutput {
            stdout: serde_json::to_vec(&v).unwrap(),
            exit_code: Some(0),
            ..Default::default()
        });
        (dir, canonical.clone(), FixtureProbeRunner::new(outputs))
    }

    #[test]
    fn graphify_identity_is_verified_and_update_argv_is_exact() {
        let (_dir, interpreter, runner) = setup("Graphifyy", "graphify.cli:main", false);
        let adapter = PipAdapter::new(interpreter.clone(), "graphifyy", "graphify");
        let found = adapter.candidates(&runner).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].evidence_level, EvidenceLevel::Verified);
        assert_eq!(found[0].identity.fields["distribution"], "graphifyy");
        assert_eq!(found[0].identity.fields["entry_point"], "graphify");
        assert_eq!(
            update_argv(&found[0].identity).unwrap(),
            vec![
                interpreter.to_string_lossy().into_owned(),
                "-m".into(),
                "pip".into(),
                "install".into(),
                "--upgrade".into(),
                "graphifyy".into()
            ]
        );
    }

    #[test]
    fn display_name_can_differ_from_installed_distribution_name() {
        let (_dir, interpreter, runner) = setup("Graphifyy", "graphify.cli:main", false);
        let adapter = PipAdapter::new(interpreter, "Graphify", "graphify");
        let found = adapter.candidates(&runner).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].identity.fields["distribution"], "graphifyy");
    }

    #[test]
    fn empty_script_enumerates_installed_console_scripts() {
        let (_dir, interpreter, runner) = setup("Graphifyy", "graphify.cli:main", false);
        let adapter = PipAdapter {
            interpreter: Some(interpreter),
            ..Default::default()
        };
        let found = adapter.candidates(&runner).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].identity.fields["distribution"], "graphifyy");
        assert_eq!(found[0].identity.fields["entry_point"], "graphify");
    }

    #[test]
    fn changed_prefix_renamed_distribution_and_moved_entry_point_are_rejected() {
        let (_d, interpreter, runner) = setup("graphifyy", "graphify.cli:main", true);
        let identity = InstallationIdentity {
            adapter_kind: AdapterKind::InterpreterPip,
            fields: [
                (
                    "interpreter".into(),
                    interpreter.to_string_lossy().into_owned(),
                ),
                ("distribution".into(), "graphifyy".into()),
                ("entry_point".into(), "graphify".into()),
                ("prefix".into(), "/python".into()),
            ]
            .into(),
        };
        assert!(PipAdapter::for_recorded_update(identity)
            .candidates(&runner)
            .unwrap()
            .is_empty());
        let (_d, interpreter, runner) = setup("renamed", "other.cli:main", false);
        let found = PipAdapter::new(interpreter, "graphifyy", "graphify")
            .candidates(&runner)
            .unwrap();
        assert!(
            found.is_empty(),
            "metadata ownership mismatch must fail closed"
        );
        let (_d, interpreter, runner) = setup("graphifyy", "other.cli:main", false);
        let fields: BTreeMap<String, String> = [
            (
                "interpreter".into(),
                interpreter.to_string_lossy().into_owned(),
            ),
            ("distribution".into(), "graphifyy".into()),
            ("entry_point".into(), "graphify".into()),
            ("entry_target".into(), "graphify.cli:main".into()),
            ("prefix".into(), "/python".into()),
        ]
        .into();
        let identity = InstallationIdentity {
            adapter_kind: AdapterKind::InterpreterPip,
            fields,
        };
        assert!(PipAdapter::for_recorded_update(identity)
            .candidates(&runner)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn recorded_update_accepts_versioned_metadata_drift_with_absent_old_leaf() {
        let (dir, interpreter, initial_runner) = setup("graphifyy", "graphify.cli:main", false);
        let initial = PipAdapter::new(interpreter.clone(), "graphifyy", "graphify")
            .candidates(&initial_runner)
            .unwrap()
            .remove(0)
            .identity;
        let initial_hash = initial.fields["hash"].clone();
        let old_location = PathBuf::from(&initial.fields["location"]);
        fs::remove_file(old_location.join("entry_points.txt")).unwrap();
        fs::remove_dir(&old_location).unwrap();
        let next_location = old_location.with_file_name("graphifyy-1.0.2.dist-info");
        fs::create_dir(&next_location).unwrap();
        fs::write(
            next_location.join("entry_points.txt"),
            "[console_scripts]\ngraphify = graphify.cli:main\n",
        )
        .unwrap();
        let script = PathBuf::from(&initial.fields["executable"]);
        fs::write(&script, b"refreshed console script bytes").unwrap();
        let probe = serde_json::json!({"executable":interpreter,"prefix":"/python","scripts":dir.path().join("scripts")});
        let inspect = serde_json::json!({"installed":[{"metadata":{"name":"graphifyy","version":"1.0.2"},"metadata_location":next_location}]});
        let runner =
            FixtureProbeRunner::new([probe, inspect].into_iter().map(|value| ProbeOutput {
                stdout: serde_json::to_vec(&value).unwrap(),
                exit_code: Some(0),
                ..Default::default()
            }));
        let found = PipAdapter::for_recorded_update(initial)
            .candidates(&runner)
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].identity.fields["version"], "1.0.2");
        assert_ne!(found[0].identity.fields["hash"], initial_hash);
        assert!(!old_location.exists());
    }

    #[test]
    fn recorded_anchors_and_metadata_parent_remain_fixed() {
        let (dir, interpreter, runner) = setup("graphifyy", "graphify.cli:main", false);
        let identity = PipAdapter::new(interpreter, "graphifyy", "graphify")
            .candidates(&runner)
            .unwrap()
            .remove(0)
            .identity;
        let mut observed = identity.fields.clone();
        observed.insert("version".into(), "0.9.73".into());
        observed.insert("hash".into(), "changed".into());
        observed.insert(
            "location".into(),
            PathBuf::from(&identity.fields["location"])
                .with_file_name("graphifyy-0.9.73.dist-info")
                .to_string_lossy()
                .into_owned(),
        );
        assert!(recorded_identity_matches(&identity, &observed));
        for key in [
            "interpreter",
            "prefix",
            "scripts",
            "distribution",
            "entry_point",
            "entry_target",
            "executable",
        ] {
            let mut wrong = observed.clone();
            wrong.insert(key.into(), "different".into());
            assert!(
                !recorded_identity_matches(&identity, &wrong),
                "changed {key}"
            );
            wrong.remove(key);
            assert!(
                !recorded_identity_matches(&identity, &wrong),
                "missing {key}"
            );
        }
        let moved = dir.path().join("foreign-site");
        fs::create_dir(&moved).unwrap();
        observed.insert(
            "location".into(),
            moved
                .join("graphifyy-0.9.73.dist-info")
                .to_string_lossy()
                .into_owned(),
        );
        assert!(!recorded_identity_matches(&identity, &observed));
    }

    #[test]
    fn metadata_leaf_requires_exact_normalized_distribution_and_version() {
        let dir = tempfile::tempdir().unwrap();
        for (leaf, name, version, expected) in [
            ("my_package-1.0.dist-info", "My-Package", "1.0", true),
            ("graphifyy-0.9.73.dist-info", "graphifyy", "0.9.73", true),
            ("other-0.9.73.dist-info", "graphifyy", "0.9.73", false),
            ("graphifyy-0.9.71.dist-info", "graphifyy", "0.9.73", false),
            ("graphifyy-0.9.73", "graphifyy", "0.9.73", false),
            ("graphifyy-.dist-info", "graphifyy", "", false),
        ] {
            let path = dir.path().join(leaf);
            fs::create_dir(&path).unwrap();
            assert_eq!(
                metadata_location_is_owned(&path, name, version),
                expected,
                "{leaf}"
            );
        }
        assert!(!metadata_location_is_owned(
            Path::new("relative.dist-info"),
            "relative",
            "1"
        ));
    }

    #[test]
    fn metadata_links_and_linked_ancestors_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let actual = dir.path().join("actual");
        let metadata = actual.join("graphifyy-0.9.73.dist-info");
        fs::create_dir_all(&metadata).unwrap();
        let link = dir.path().join("linked");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&actual, &link).unwrap();
        #[cfg(windows)]
        {
            let output = std::process::Command::new("cmd.exe")
                .args(["/C", "mklink", "/J"])
                .arg(&link)
                .arg(&actual)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "create isolated junction: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert!(path_has_link_component(&link));
        assert!(!metadata_location_is_owned(
            &link.join("graphifyy-0.9.73.dist-info"),
            "graphifyy",
            "0.9.73"
        ));
        assert!(!metadata_location_is_owned(&link, "graphifyy", "0.9.73"));
        // Remove only the isolated link, leaving its target intact.
        #[cfg(windows)]
        fs::remove_dir(&link).unwrap();
        #[cfg(unix)]
        fs::remove_file(&link).unwrap();
        assert!(metadata.exists());
    }

    #[test]
    fn recorded_reprobe_preserves_duplicate_candidates_for_ambiguity() {
        let (_dir, interpreter, runner) = setup("graphifyy", "graphify.cli:main", false);
        let initial = PipAdapter::new(interpreter, "graphifyy", "graphify")
            .candidates(&runner)
            .unwrap()
            .remove(0)
            .identity;
        let probe = serde_json::json!({"executable":initial.fields["interpreter"], "prefix":initial.fields["prefix"], "scripts":initial.fields["scripts"]});
        let item = serde_json::json!({"metadata":{"name":"graphifyy","version":"1.0"},"metadata_location":initial.fields["location"]});
        let inspect = serde_json::json!({"installed":[item.clone(), item]});
        let runner =
            FixtureProbeRunner::new([probe, inspect].into_iter().map(|value| ProbeOutput {
                stdout: serde_json::to_vec(&value).unwrap(),
                exit_code: Some(0),
                ..Default::default()
            }));
        assert_eq!(
            PipAdapter::for_recorded_update(initial)
                .candidates(&runner)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn probe_failure_classification_never_echoes_process_secrets() {
        for (output, expected) in [
            (
                ProbeOutput {
                    timed_out: true,
                    ..Default::default()
                },
                "timed out",
            ),
            (
                ProbeOutput {
                    stdout_truncated: true,
                    ..Default::default()
                },
                "exceeded limit",
            ),
            (
                ProbeOutput {
                    stderr_truncated: true,
                    ..Default::default()
                },
                "exceeded limit",
            ),
            (
                ProbeOutput {
                    spawn_error: Some("credential-secret".into()),
                    ..Default::default()
                },
                "could not start",
            ),
            (
                ProbeOutput {
                    exit_code: Some(7),
                    ..Default::default()
                },
                "exit=Some(7)",
            ),
        ] {
            let runner = FixtureProbeRunner::new([ProbeOutput {
                stderr: b"credential-secret".to_vec(),
                ..output
            }]);
            let error =
                remote_update_status_with_runner(Some(&recorded_identity("graphifyy")), &runner)
                    .unwrap_err();
            let message = format!("{error:?}");
            assert!(message.contains(expected), "{message}");
            assert!(!message.contains("credential-secret"));
            assert_eq!(runner.requests().len(), 1);
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn reparse_interpreter_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("python");
        fs::write(&target, b"python").unwrap();
        let link = dir.path().join("python-link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_file(&target, &link).is_err() {
            return;
        }
        assert!(canonical_interpreter(&link).is_err());
    }

    #[test]
    fn bare_pip_request_is_refused() {
        let adapter = PipAdapter::default();
        assert!(adapter.update_argv().is_none());
    }

    fn recorded_identity(distribution: &str) -> InstallationIdentity {
        let interpreter = if cfg!(windows) {
            r"C:\recorded\python.exe"
        } else {
            "/recorded/python"
        };
        InstallationIdentity {
            adapter_kind: AdapterKind::InterpreterPip,
            fields: [
                ("interpreter".into(), interpreter.into()),
                ("distribution".into(), distribution.into()),
            ]
            .into(),
        }
    }

    fn index_runner(
        name: &str,
        latest: &str,
        installed: &str,
        versions: &[&str],
    ) -> FixtureProbeRunner {
        FixtureProbeRunner::new([ProbeOutput {
            stdout: serde_json::to_vec(&serde_json::json!({
                "name": name,
                "latest": latest,
                "installed_version": installed,
                "versions": versions,
            }))
            .unwrap(),
            exit_code: Some(0),
            ..Default::default()
        }])
    }

    #[test]
    fn remote_update_status_uses_recorded_interpreter_and_exact_normalized_distribution() {
        let runner = index_runner("My.pkg_name", "2.0", "1.0", &["1.0", "2.0"]);
        let identity = recorded_identity("my-pkg.name");
        assert_eq!(
            remote_update_status_with_runner(Some(&identity), &runner).unwrap(),
            RemoteUpdateStatus::UpdateAvailable
        );
        let request = &runner.requests()[0];
        assert_eq!(
            request.program,
            PathBuf::from(identity.fields["interpreter"].clone())
        );
        assert_eq!(
            request.argv,
            vec![
                "-m",
                "pip",
                "index",
                "versions",
                "my-pkg-name",
                "--json",
                "--disable-pip-version-check"
            ]
        );
    }

    #[test]
    fn remote_update_status_compares_latest_with_fresh_installed_version() {
        let identity = recorded_identity("tracked-package");
        let runner = index_runner("tracked.package", "1.5", "2.0", &["1.0", "1.5"]);
        assert_eq!(
            remote_update_status_with_runner(Some(&identity), &runner).unwrap(),
            RemoteUpdateStatus::Current
        );
        let runner = index_runner("tracked-package", "2.0", "1.9", &["1.9", "2.0"]);
        assert_eq!(
            remote_update_status_with_runner(Some(&identity), &runner).unwrap(),
            RemoteUpdateStatus::UpdateAvailable
        );
        assert_eq!(runner.requests().len(), 1);
    }

    #[test]
    fn remote_update_status_uses_pep440_ordering() {
        let identity = recorded_identity("tracked-package");
        for (latest, installed, expected) in [
            ("1.0.post1", "1.0", RemoteUpdateStatus::UpdateAvailable),
            (
                "1.0+local.2",
                "1.0+local.1",
                RemoteUpdateStatus::UpdateAvailable,
            ),
            ("1.0rc2", "1.0rc1", RemoteUpdateStatus::UpdateAvailable),
            ("1.0", "1.0.0", RemoteUpdateStatus::Current),
            ("1.0rc1", "1.0", RemoteUpdateStatus::Current),
            ("1.0.dev1", "1.0a1", RemoteUpdateStatus::Current),
        ] {
            let runner = index_runner("tracked-package", latest, installed, &[latest]);
            assert_eq!(
                remote_update_status_with_runner(Some(&identity), &runner).unwrap(),
                expected,
                "latest={latest}, installed={installed}"
            );
        }
    }

    #[test]
    fn remote_update_status_rejects_malformed_output_and_command_failure() {
        let identity = recorded_identity("tracked-package");
        let malformed = FixtureProbeRunner::new([ProbeOutput {
            stdout: b"not json".to_vec(),
            exit_code: Some(0),
            ..Default::default()
        }]);
        assert!(matches!(
            remote_update_status_with_runner(Some(&identity), &malformed),
            Err(AdapterError::Failed(_))
        ));
        let failed = FixtureProbeRunner::new([ProbeOutput {
            exit_code: Some(1),
            ..Default::default()
        }]);
        assert!(matches!(
            remote_update_status_with_runner(Some(&identity), &failed),
            Err(AdapterError::Failed(_))
        ));
        let timed_out = FixtureProbeRunner::new([ProbeOutput {
            timed_out: true,
            ..Default::default()
        }]);
        assert!(
            matches!(remote_update_status_with_runner(Some(&identity), &timed_out), Err(AdapterError::Failed(message)) if message.contains("timed out"))
        );
        let capped = FixtureProbeRunner::new([ProbeOutput {
            stdout_truncated: true,
            ..Default::default()
        }]);
        assert!(
            matches!(remote_update_status_with_runner(Some(&identity), &capped), Err(AdapterError::Failed(message)) if message.contains("exceeded limit"))
        );
        assert!(
            matches!(remote_update_status_with_runner(Some(&identity), &failed), Err(AdapterError::Failed(message)) if message.contains("unsuccessfully"))
        );
    }

    #[test]
    fn remote_update_status_rejects_incomplete_or_mismatched_index_proof() {
        let identity = recorded_identity("tracked-package");
        for response in [
            serde_json::json!({"name":"tracked-package", "latest":"2.0", "installed_version":"1.0", "versions":[]}),
            serde_json::json!({"name":"other", "latest":"2.0", "installed_version":"1.0", "versions":["2.0"]}),
            serde_json::json!({"name":"tracked-package", "latest":"2.0", "installed_version":"1.0", "versions":["1.0"]}),
            serde_json::json!({"name":"tracked-package", "latest":"2.0", "versions":["2.0"]}),
            serde_json::json!({"name":"tracked-package", "latest":"bad", "installed_version":"1.0", "versions":["2.0"]}),
        ] {
            let runner = FixtureProbeRunner::new([ProbeOutput {
                stdout: serde_json::to_vec(&response).unwrap(),
                exit_code: Some(0),
                ..Default::default()
            }]);
            assert!(matches!(
                remote_update_status_with_runner(Some(&identity), &runner),
                Err(AdapterError::Failed(_))
            ));
        }
    }

    #[test]
    fn real_pip_index_503_is_failed_in_isolated_environment() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            process::Command,
            sync::{
                atomic::{AtomicBool, Ordering},
                Arc,
            },
            thread,
            time::Instant,
        };

        let python = std::env::var_os("CCYNC_RECORDED_PYTHON")
            .map(PathBuf::from)
            .filter(|path| path.is_file())
            .or_else(|| {
                std::env::split_paths(&std::env::var_os("PATH")?).find_map(|dir| {
                    ["python.exe", "python3.exe", "python", "python3"]
                        .iter()
                        .map(|name| dir.join(name))
                        .find(|path| path.is_file())
                })
            });
        let Some(python) = python else {
            eprintln!("NotRun: no recorded Python interpreter is available");
            return;
        };
        if !Command::new(&python)
            .args(["-m", "pip", "--version"])
            .output()
            .is_ok_and(|output| output.status.success())
        {
            eprintln!("NotRun: recorded Python interpreter has no runnable pip");
            return;
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/simple/", listener.local_addr().unwrap());
        let requested = Arc::new(AtomicBool::new(false));
        let request_flag = Arc::clone(&requested);
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        request_flag.store(true, Ordering::SeqCst);
                        let mut request = [0u8; 4096];
                        let _ = stream.read(&mut request);
                        let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => return,
                }
            }
        });
        let temp = tempfile::tempdir().unwrap();
        let mut runner = SystemProbeRunner;
        let identity = InstallationIdentity {
            adapter_kind: AdapterKind::InterpreterPip,
            fields: [
                ("interpreter".into(), python.to_string_lossy().into_owned()),
                (
                    "distribution".into(),
                    "ccync-isolated-regression-probe".into(),
                ),
            ]
            .into(),
        };
        let wrapper = Pip503Runner {
            inner: &mut runner,
            index_url: url,
            config_file: if cfg!(windows) {
                "NUL".into()
            } else {
                "/dev/null".into()
            },
            cache: temp.path().join("pip-cache"),
        };
        let result = remote_update_status_with_runner(Some(&identity), &wrapper);
        server.join().unwrap();
        assert!(
            requested.load(Ordering::SeqCst),
            "local HTTP endpoint was not requested"
        );
        assert!(
            matches!(result, Err(AdapterError::Failed(_))),
            "unexpected result: {result:?}"
        );
    }

    struct Pip503Runner<'a> {
        inner: &'a mut SystemProbeRunner,
        index_url: String,
        config_file: String,
        cache: PathBuf,
    }

    impl ProbeRunner for Pip503Runner<'_> {
        fn run(&self, request: &ProbeRequest) -> ProbeOutput {
            let mut request = request.clone();
            request
                .env
                .insert("PIP_INDEX_URL".into(), self.index_url.clone());
            request
                .env
                .insert("PIP_CONFIG_FILE".into(), self.config_file.clone());
            request.env.insert(
                "PIP_CACHE_DIR".into(),
                self.cache.to_string_lossy().into_owned(),
            );
            request.env.insert("PIP_RETRIES".into(), "0".into());
            request
                .env
                .insert("PIP_EXTRA_INDEX_URL".into(), String::new());
            request.env.insert("PIP_FIND_LINKS".into(), String::new());
            self.inner.run(&request)
        }
    }
}
