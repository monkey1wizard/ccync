//! uv tool discovery. Upstream currently documents no structured local inventory command or minimum version for mapping a tool environment, entry point, and shim target. Probe `uv --version`, `uv tool dir`, and `uv tool dir --bin`; do not parse human `uv tool list` output or report verified evidence until upstream documents such a contract.

use super::{AdapterCapabilities, AdapterError, CapabilityState, ToolAdapter};
use crate::tool_manager::{
    discovery::{DiscoveryCandidate, DiscoveryQuery},
    file_identity::is_reparse_or_link,
    model::{AdapterKind, InstallationIdentity},
    probe::{ProbeRequest, ProbeRunner, SystemProbeRunner},
};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};

const CAP: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct UvAdapter {
    executable: PathBuf,
    recorded: Option<InstallationIdentity>,
}

impl Default for UvAdapter {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("uv"),
            recorded: None,
        }
    }
}

impl UvAdapter {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            recorded: None,
        }
    }
    pub fn for_recorded_update(identity: InstallationIdentity) -> Self {
        let executable = identity
            .fields
            .get("managerExecutable")
            .map(PathBuf::from)
            .unwrap_or_default();
        Self {
            executable,
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
        let version_out = run(runner, &exe, &["--version"])?;
        let version_text = String::from_utf8_lossy(&version_out.stdout);
        let Some(version) = version_text
            .split_whitespace()
            .last()
            .and_then(parse_version)
        else {
            return Ok(vec![]);
        };
        let tool_root = output_path(run(runner, &exe, &["tool", "dir"])?.stdout)?;
        let bin_root = output_path(run(runner, &exe, &["tool", "dir", "--bin"])?.stdout)?;
        let _ = (version, tool_root, bin_root);
        // No documented command currently maps all required identity fields. Keep discovery empty.
        Ok(Vec::new())
    }
}
impl ToolAdapter for UvAdapter {
    fn kind(&self) -> AdapterKind {
        AdapterKind::UvTool
    }
    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities::for_adapter(
            AdapterKind::UvTool,
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
    if identity.adapter_kind != AdapterKind::UvTool {
        return None;
    }
    let exe = identity.fields.get("managerExecutable")?;
    let name = identity.fields.get("name")?;
    if !Path::new(exe).is_absolute() || name.is_empty() || name.contains(['/', '\\']) {
        return None;
    }
    Some(vec![
        exe.clone(),
        "tool".into(),
        "upgrade".into(),
        name.clone(),
    ])
}
fn canonical_executable(path: &Path) -> Option<PathBuf> {
    let resolved = executable_candidates(path)
        .into_iter()
        .find(|p| p.is_file())?;
    if is_reparse_or_link(&resolved).ok()? {
        return None;
    }
    fs::canonicalize(&resolved).ok()
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
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim_start_matches('v');
    let mut p = s.split('.');
    Some((
        p.next()?.parse().ok()?,
        p.next()?.parse().ok()?,
        p.next()?
            .split(|c: char| !c.is_ascii_digit())
            .next()?
            .parse()
            .ok()?,
    ))
}
fn output_path(bytes: Vec<u8>) -> Result<PathBuf, AdapterError> {
    let s = String::from_utf8(bytes)
        .map_err(|_| AdapterError::Failed("uv returned invalid path".into()))?;
    let p = PathBuf::from(s.trim());
    if p.is_absolute() {
        Ok(p)
    } else {
        Err(AdapterError::Failed(
            "uv returned a non-absolute tool root".into(),
        ))
    }
}
fn run(
    r: &dyn ProbeRunner,
    exe: &Path,
    argv: &[&str],
) -> Result<crate::tool_manager::probe::ProbeOutput, AdapterError> {
    let request = ProbeRequest {
        program: exe.to_path_buf(),
        argv: argv.iter().map(|x| (*x).into()).collect(),
        env: BTreeMap::new(),
        working_directory: env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        timeout: Duration::from_secs(20),
        output_byte_cap: CAP,
    };
    let out = r.run(&request);
    if out.exit_code != Some(0)
        || out.timed_out
        || out.stdout_truncated
        || out.spawn_error.is_some()
    {
        Err(AdapterError::Failed("uv probe failed".into()))
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
    #[test]
    fn tool_roots_are_probed_but_undocumented_contract_is_not_parsed() {
        let human_list = include_str!("../../../tests/fixtures/tool_manager/uv/human-list.txt");
        assert!(!human_list.is_empty());
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("uv");
        fs::write(&exe, b"manager").unwrap();
        let root = d.path().join("root");
        fs::create_dir(&root).unwrap();
        let bin = d.path().join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("demo"), b"shim").unwrap();
        for version in ["uv 0.7.9", "uv 0.8.0", "uv 0.9.0"] {
            let runner = FixtureProbeRunner::new([
                out(version),
                out(root.to_string_lossy()),
                out(bin.to_string_lossy()),
            ]);
            assert!(UvAdapter::new(&exe).candidates(&runner).unwrap().is_empty());
            assert_eq!(runner.requests().len(), 3);
            assert_eq!(runner.requests()[1].argv, ["tool", "dir"]);
            assert_eq!(runner.requests()[2].argv, ["tool", "dir", "--bin"]);
            assert!(!runner
                .requests()
                .iter()
                .any(|request| request.argv.starts_with(&["tool".into(), "list".into()])));
        }
    }
    #[test]
    fn update_argv_is_exact() {
        let exe = std::env::current_exe().unwrap();
        let path = exe.to_string_lossy().into_owned();
        let identity = InstallationIdentity {
            adapter_kind: AdapterKind::UvTool,
            fields: [
                ("managerExecutable".into(), path.clone()),
                ("name".into(), "demo".into()),
            ]
            .into(),
        };
        assert_eq!(
            update_argv(&identity).unwrap(),
            [path, "tool".into(), "upgrade".into(), "demo".into()]
        );
    }
}
