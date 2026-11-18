//! pipx discovery from its JSON inventory and app links.
use super::{AdapterCapabilities, AdapterError, CapabilityState, ToolAdapter};
use crate::tool_manager::{
    discovery::{DiscoveryCandidate, DiscoveryQuery},
    file_identity::hash_same_handle,
    link_chain::{resolve_link_chain, LinkChainPolicy},
    model::{AdapterKind, EvidenceLevel, InstallationIdentity},
    probe::{ProbeRequest, ProbeRunner, SystemProbeRunner},
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};

const CAP: usize = 1024 * 1024;
#[derive(Debug, Clone)]
pub struct PipxAdapter {
    executable: PathBuf,
    recorded: Option<InstallationIdentity>,
}
impl Default for PipxAdapter {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("pipx"),
            recorded: None,
        }
    }
}
impl PipxAdapter {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            recorded: None,
        }
    }
    pub fn for_recorded_update(identity: InstallationIdentity) -> Self {
        Self {
            executable: identity
                .fields
                .get("managerExecutable")
                .map(PathBuf::from)
                .unwrap_or_default(),
            recorded: Some(identity),
        }
    }
    fn candidates(
        &self,
        runner: &dyn ProbeRunner,
    ) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
        let Some(exe) = canonical_executable(&self.executable) else {
            return Ok(vec![]);
        };
        let home = PathBuf::from(query_value(runner, &exe, "PIPX_HOME")?);
        let bin = PathBuf::from(query_value(runner, &exe, "PIPX_BIN_DIR")?);
        if !home.is_absolute() || !bin.is_absolute() {
            return Ok(vec![]);
        }
        let listing = run(runner, &exe, &["list", "--json"])?;
        let doc: Value = serde_json::from_slice(&listing.stdout)
            .map_err(|_| AdapterError::Failed("pipx returned invalid JSON".into()))?;
        let Some(venvs) = doc.get("venv").and_then(Value::as_object) else {
            return Ok(vec![]);
        };
        let mut out = Vec::new();
        for (package, venv) in venvs {
            let Some(apps) = venv.get("apps").and_then(Value::as_array) else {
                continue;
            };
            let version = venv
                .pointer("/metadata/main_package/package_version")
                .and_then(Value::as_str)
                .unwrap_or("");
            let distribution = venv
                .pointer("/metadata/main_package/package")
                .and_then(Value::as_str)
                .unwrap_or(package);
            for app in apps {
                let Some(app_name) = app.get("app").and_then(Value::as_str) else {
                    continue;
                };
                let link = bin.join(with_exe(app_name));
                // The link itself lives in PIPX_BIN_DIR. Resolve it with both declared roots,
                // then require its final executable to remain inside PIPX_HOME.
                let resolved = match resolve_link_chain(
                    &link,
                    &LinkChainPolicy {
                        containment_roots: vec![home.clone(), bin.clone()],
                        max_hops: 16,
                    },
                ) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let home_canonical = fs::canonicalize(&home).unwrap_or(home.clone());
                if !resolved.target.starts_with(&home_canonical) {
                    continue;
                }
                let Ok(hash) = hash_same_handle(&resolved.target) else {
                    continue;
                };
                let mut fields = BTreeMap::new();
                fields.insert(
                    "managerExecutable".into(),
                    exe.to_string_lossy().into_owned(),
                );
                fields.insert("package".into(), package.clone());
                fields.insert("distribution".into(), distribution.to_owned());
                fields.insert("version".into(), version.to_owned());
                fields.insert("app".into(), app_name.to_owned());
                fields.insert("appLink".into(), link.to_string_lossy().into_owned());
                fields.insert(
                    "appTarget".into(),
                    resolved.target.to_string_lossy().into_owned(),
                );
                fields.insert("pipxHome".into(), home.to_string_lossy().into_owned());
                fields.insert("pipxBinDir".into(), bin.to_string_lossy().into_owned());
                fields.insert("hash".into(), hash);
                let identity = InstallationIdentity {
                    adapter_kind: AdapterKind::Pipx,
                    fields,
                };
                if self.recorded.as_ref().is_some_and(|r| r != &identity) {
                    continue;
                }
                out.push(DiscoveryCandidate::new(
                    identity,
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
impl ToolAdapter for PipxAdapter {
    fn kind(&self) -> AdapterKind {
        AdapterKind::Pipx
    }
    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities::for_adapter(
            AdapterKind::Pipx,
            CapabilityState::Supported,
            CapabilityState::Supported,
            CapabilityState::Supported,
            CapabilityState::Unsupported,
        )
    }
    fn discover(&self, _: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
        self.candidates(&SystemProbeRunner)
    }
    fn reprobe(&self, _: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
        self.discover(&DiscoveryQuery { online: false })
    }
    fn update_argv(&self) -> Option<Vec<String>> {
        update_argv(self.recorded.as_ref()?)
    }
}
pub fn update_argv(identity: &InstallationIdentity) -> Option<Vec<String>> {
    if identity.adapter_kind != AdapterKind::Pipx {
        return None;
    }
    let exe = identity.fields.get("managerExecutable")?;
    let package = identity.fields.get("package")?;
    if !Path::new(exe).is_absolute() || package.is_empty() || package.contains(['/', '\\']) {
        return None;
    }
    Some(vec![exe.clone(), "upgrade".into(), package.clone()])
}
fn query_value(r: &dyn ProbeRunner, exe: &Path, key: &str) -> Result<String, AdapterError> {
    let out = run(r, exe, &["environment", "--value", key])?;
    String::from_utf8(out.stdout)
        .map(|v| v.trim().to_owned())
        .map_err(|_| AdapterError::Failed("pipx returned invalid environment path".into()))
}
fn canonical_executable(path: &Path) -> Option<PathBuf> {
    let found = executable_candidates(path)
        .into_iter()
        .find(|p| p.is_file())?;
    if !found.is_file() || crate::tool_manager::file_identity::is_reparse_or_link(&found).ok()? {
        return None;
    }
    fs::canonicalize(&found).ok()
}
fn executable_candidates(path: &Path) -> Vec<PathBuf> {
    if path.is_absolute() {
        return vec![path.to_path_buf()];
    }
    let mut names = vec![path.to_path_buf()];
    #[cfg(windows)]
    if path.extension().is_none() {
        let extensions = env::var_os("PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
        names.extend(
            extensions
                .to_string_lossy()
                .split(';')
                .filter(|s| !s.is_empty())
                .map(|s| path.with_extension(s.trim_start_matches('.'))),
        );
    }
    env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .collect()
}
fn with_exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}
fn run(
    r: &dyn ProbeRunner,
    exe: &Path,
    argv: &[&str],
) -> Result<crate::tool_manager::probe::ProbeOutput, AdapterError> {
    let req = ProbeRequest {
        program: exe.to_path_buf(),
        argv: argv.iter().map(|s| (*s).into()).collect(),
        env: BTreeMap::new(),
        working_directory: env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        timeout: Duration::from_secs(20),
        output_byte_cap: CAP,
    };
    let out = r.run(&req);
    if out.exit_code != Some(0)
        || out.timed_out
        || out.stdout_truncated
        || out.spawn_error.is_some()
    {
        Err(AdapterError::Failed("pipx probe failed".into()))
    } else {
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_manager::probe::{FixtureProbeRunner, ProbeOutput};
    fn out(s: impl AsRef<str>) -> ProbeOutput {
        ProbeOutput {
            stdout: s.as_ref().as_bytes().to_vec(),
            exit_code: Some(0),
            ..Default::default()
        }
    }
    fn runner(home: &Path, bin: &Path, list: &str) -> FixtureProbeRunner {
        FixtureProbeRunner::new([
            out(home.to_string_lossy()),
            out(bin.to_string_lossy()),
            out(list),
        ])
    }
    #[test]
    fn agreeing_pipx_sources_are_verified_and_missing_app_is_empty() {
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("pipx");
        fs::write(&exe, b"manager").unwrap();
        let home = d.path().join("home");
        let bin = home.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let target = home.join("venvs/demo/bin/demo");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"app").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, bin.join("demo")).unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_file(&target, bin.join("demo.exe")).is_err() {
            eprintln!("skip: creating a symlink requires Windows privilege");
            return;
        }
        let fixture = include_str!("../../../tests/fixtures/tool_manager/pipx/agreeing.json")
            .replace("__APP__", "demo")
            .replace("__PACKAGE__", "demo");
        let found = PipxAdapter::new(&exe)
            .candidates(&runner(&home, &bin, &fixture))
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].evidence_level, EvidenceLevel::Verified);
        let missing = r#"{"venv":{"demo":{"metadata":{"main_package":{"package":"demo","package_version":"1.0"}},"apps":[]}}}"#;
        assert!(PipxAdapter::new(&exe)
            .candidates(&runner(&home, &bin, missing))
            .unwrap()
            .is_empty());
    }
    #[test]
    fn schema_mismatch_and_app_link_escape_are_rejected() {
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("pipx");
        fs::write(&exe, b"manager").unwrap();
        let home = d.path().join("home");
        let bin = home.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let bad = r#"{"version":1,"packages":{}}"#;
        assert!(PipxAdapter::new(&exe)
            .candidates(&runner(&home, &bin, bad))
            .unwrap()
            .is_empty());
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("evil");
        fs::write(&target, b"evil").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, bin.join("demo")).unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_file(&target, bin.join("demo.exe")).is_err() {
            eprintln!("skip: creating a symlink requires Windows privilege");
            return;
        }
        let list = r#"{"venv":{"demo":{"metadata":{"main_package":{"package":"demo","package_version":"1.0"}},"apps":[{"app":"demo"}]}}}"#;
        assert!(PipxAdapter::new(&exe)
            .candidates(&runner(&home, &bin, list))
            .unwrap()
            .is_empty());
    }
    #[test]
    fn update_argv_is_exact() {
        let executable = std::env::current_exe().unwrap();
        let path = executable.to_string_lossy().into_owned();
        let identity = InstallationIdentity {
            adapter_kind: AdapterKind::Pipx,
            fields: [
                ("managerExecutable".into(), path.clone()),
                ("package".into(), "demo".into()),
            ]
            .into(),
        };
        assert_eq!(
            update_argv(&identity).unwrap(),
            [path, "upgrade".to_owned(), "demo".to_owned()]
        );
    }
}
