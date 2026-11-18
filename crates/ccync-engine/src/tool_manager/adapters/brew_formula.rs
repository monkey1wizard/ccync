//! Homebrew formula discovery from version-gated JSON probes.
use super::{AdapterCapabilities, AdapterError, CapabilityState, ToolAdapter};
use crate::tool_manager::{
    discovery::{DiscoveryCandidate, DiscoveryQuery},
    file_identity::{hash_same_handle, is_reparse_or_link},
    model::{AdapterKind, EvidenceLevel, InstallationIdentity, ManagerPinState},
    probe::{ProbeRequest, ProbeRunner, SystemProbeRunner},
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};

const ENV_KEY: &str = "HOMEBREW_NO_AUTO_UPDATE";
const CAP: usize = 4 * 1024 * 1024;
#[derive(Debug, Clone)]
pub struct BrewFormulaAdapter {
    executable: PathBuf,
    recorded: Option<InstallationIdentity>,
}
impl Default for BrewFormulaAdapter {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("brew"),
            recorded: None,
        }
    }
}
impl BrewFormulaAdapter {
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
    pub(super) fn candidates(
        &self,
        runner: &dyn ProbeRunner,
    ) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
        let Some(brew) = canonical_executable(&self.executable) else {
            return Ok(vec![]);
        };
        let version = match run(runner, &brew, &["--version"]) {
            Ok(x) => String::from_utf8_lossy(&x.stdout).into_owned(),
            Err(_) => return Ok(vec![]),
        };
        if !version_supported(&version) {
            return Ok(vec![]);
        }
        let prefix = match run(runner, &brew, &["--prefix"]).and_then(|o| output_path(o.stdout)) {
            Ok(p) => fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from("")),
            Err(_) => return Ok(vec![]),
        };
        if prefix.as_os_str().is_empty() {
            return Ok(vec![]);
        }
        let info = match run(runner, &brew, &["info", "--json=v1", "--installed"]) {
            Ok(o) => serde_json::from_slice::<Value>(&o.stdout).ok(),
            Err(_) => None,
        };
        let Some(formulae) = info
            .as_ref()
            .and_then(|v| v.get("formulae"))
            .and_then(Value::as_array)
        else {
            return Ok(vec![]);
        };
        let mut out = Vec::new();
        for formula in formulae {
            let (Some(name), Some(installed)) = (
                formula.get("full_name").and_then(Value::as_str),
                formula.get("installed").and_then(Value::as_array),
            ) else {
                continue;
            };
            if name.is_empty() || name.starts_with('-') {
                continue;
            }
            let listed = match run(runner, &brew, &["list", "--formula", name]) {
                Ok(o) => String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter_map(|line| fs::canonicalize(line.trim()).ok())
                    .collect::<Vec<_>>(),
                Err(_) => Vec::new(),
            };
            if listed.is_empty() {
                continue;
            }
            for keg in installed {
                let (Some(version), Some(keg_only)) = (
                    keg.get("version").and_then(Value::as_str),
                    keg.get("keg_only").and_then(Value::as_bool),
                ) else {
                    continue;
                };
                let short_name = name.rsplit('/').next().unwrap_or(name);
                let cellar = keg
                    .get("prefix")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .unwrap_or_else(|| prefix.join("Cellar").join(short_name).join(version));
                let cellar = fs::canonicalize(&cellar).unwrap_or(cellar);
                let candidates = [
                    prefix.join("bin").join(short_name),
                    cellar.join("bin").join(short_name),
                ];
                for source in candidates {
                    let Ok(target) = fs::canonicalize(&source) else {
                        continue;
                    };
                    if !target.starts_with(&cellar)
                        || !target.is_file()
                        || !listed.contains(&target)
                        || is_reparse_or_link(&target).unwrap_or(true)
                    {
                        continue;
                    }
                    let Ok(hash) = hash_same_handle(&target) else {
                        continue;
                    };
                    let linked = source.starts_with(prefix.join("bin"));
                    let mut fields = BTreeMap::new();
                    fields.insert(
                        "managerExecutable".into(),
                        brew.to_string_lossy().into_owned(),
                    );
                    fields.insert("brewIdentity".into(), brew_identity(&brew));
                    fields.insert("prefix".into(), prefix.to_string_lossy().into_owned());
                    fields.insert("name".into(), name.into());
                    fields.insert("version".into(), version.into());
                    fields.insert("keg".into(), cellar.to_string_lossy().into_owned());
                    fields.insert("kegOnly".into(), keg_only.to_string());
                    fields.insert("providerHint".into(), "unknown".into());
                    fields.insert(
                        "resolvedTarget".into(),
                        target.to_string_lossy().into_owned(),
                    );
                    fields.insert("executable".into(), target.to_string_lossy().into_owned());
                    fields.insert("hash".into(), hash);
                    fields.insert("fileIdentity".into(), file_identity(&target));
                    fields.insert("linked".into(), linked.to_string());
                    let identity = InstallationIdentity {
                        adapter_kind: AdapterKind::BrewFormula,
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
        }
        Ok(out)
    }
    pub(super) fn manager_pin_with_runner(
        &self,
        runner: &dyn ProbeRunner,
    ) -> Result<ManagerPinState, AdapterError> {
        let id = self.recorded().ok_or(AdapterError::Unsupported)?;
        let name = id.fields.get("name").ok_or(AdapterError::Unsupported)?;
        let exe = id
            .fields
            .get("managerExecutable")
            .ok_or(AdapterError::Unsupported)?;
        let o = run(runner, Path::new(exe), &["list", "--pinned"])?;
        let pinned = String::from_utf8_lossy(&o.stdout)
            .lines()
            .any(|x| x.trim() == name);
        Ok(if pinned {
            ManagerPinState::Pinned
        } else {
            ManagerPinState::Unpinned
        })
    }
    fn recorded(&self) -> Option<&InstallationIdentity> {
        self.recorded.as_ref()
    }
}
impl ToolAdapter for BrewFormulaAdapter {
    fn kind(&self) -> AdapterKind {
        AdapterKind::BrewFormula
    }
    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities::for_adapter(
            self.kind(),
            CapabilityState::Supported,
            CapabilityState::Supported,
            CapabilityState::Supported,
            CapabilityState::Supported,
        )
    }
    fn discover(&self, _: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
        self.candidates(&SystemProbeRunner)
    }
    fn reprobe(&self, q: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
        self.discover(q)
    }
    fn remote_update_status(
        &self,
        q: &DiscoveryQuery,
    ) -> Result<super::RemoteUpdateStatus, AdapterError> {
        Ok(if q.online {
            crate::tool_manager::adapters::RemoteUpdateStatus::Unsupported
        } else {
            crate::tool_manager::adapters::RemoteUpdateStatus::NotChecked
        })
    }
    fn manager_pin(&self) -> Result<ManagerPinState, AdapterError> {
        self.manager_pin_with_runner(&SystemProbeRunner)
    }
    fn update_argv(&self) -> Option<Vec<String>> {
        update_argv(self.recorded()?)
    }
}
pub fn update_argv(identity: &InstallationIdentity) -> Option<Vec<String>> {
    if identity.adapter_kind != AdapterKind::BrewFormula {
        return None;
    }
    let exe = identity.fields.get("managerExecutable")?;
    let name = identity.fields.get("name")?;
    Some(vec![
        exe.clone(),
        "upgrade".into(),
        "--formula".into(),
        name.clone(),
    ])
}
pub(super) fn run(
    r: &dyn ProbeRunner,
    exe: &Path,
    argv: &[&str],
) -> Result<crate::tool_manager::probe::ProbeOutput, AdapterError> {
    let out = r.run(&ProbeRequest {
        program: exe.into(),
        argv: argv.iter().map(|s| (*s).into()).collect(),
        env: [(ENV_KEY.into(), "1".into())].into(),
        working_directory: env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        timeout: Duration::from_secs(20),
        output_byte_cap: CAP,
    });
    if out.exit_code != Some(0)
        || out.timed_out
        || out.stdout_truncated
        || out.spawn_error.is_some()
    {
        Err(AdapterError::Failed("Homebrew probe failed".into()))
    } else {
        Ok(out)
    }
}
pub(super) fn canonical_executable(path: &Path) -> Option<PathBuf> {
    let found = executable_candidates(path)
        .into_iter()
        .find(|p| p.is_file())?;
    if is_reparse_or_link(&found).ok()? {
        return None;
    }
    fs::canonicalize(found).ok()
}
fn executable_candidates(path: &Path) -> Vec<PathBuf> {
    if path.is_absolute() {
        return vec![path.into()];
    }
    env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .map(|d| d.join(path))
        .collect()
}
fn output_path(bytes: Vec<u8>) -> Result<PathBuf, AdapterError> {
    let s = String::from_utf8(bytes)
        .map_err(|_| AdapterError::Failed("brew returned invalid path".into()))?;
    let p = PathBuf::from(s.trim());
    if p.is_absolute() {
        Ok(p)
    } else {
        Err(AdapterError::Failed(
            "brew returned non-absolute prefix".into(),
        ))
    }
}
fn version_supported(s: &str) -> bool {
    let mut nums = s
        .split_whitespace()
        .find(|x| x.as_bytes().first().is_some_and(u8::is_ascii_digit))
        .unwrap_or("")
        .split('.');
    let (Some(a), Some(_b)) = (
        nums.next().and_then(|x| x.parse::<u32>().ok()),
        nums.next().and_then(|x| x.parse::<u32>().ok()),
    ) else {
        return false;
    };
    a >= 3
}
fn brew_identity(path: &Path) -> String {
    format!("{}:{}", path.display(), file_identity(path))
}
fn file_identity(path: &Path) -> String {
    let Ok(m) = fs::metadata(path) else {
        return "unknown".into();
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        format!("{}:{}", m.dev(), m.ino())
    }
    #[cfg(not(unix))]
    {
        format!(
            "{}:{}",
            m.len(),
            m.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|t| t.as_nanos())
                .unwrap_or(0)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_manager::probe::{FixtureProbeRunner, ProbeOutput};
    fn output(s: &str) -> ProbeOutput {
        ProbeOutput {
            stdout: s.as_bytes().to_vec(),
            exit_code: Some(0),
            ..Default::default()
        }
    }
    fn formula_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let brew = d.path().join("brew");
        fs::write(&brew, b"brew").unwrap();
        let prefix = d.path().join("prefix");
        let cell = prefix.join("Cellar/demo/1.0/bin");
        fs::create_dir_all(&cell).unwrap();
        let target = cell.join("demo");
        fs::write(&target, b"executable").unwrap();
        fs::create_dir_all(prefix.join("bin")).unwrap();
        (d, brew, prefix)
    }
    #[test]
    fn linked_formula_identity_and_requests_are_exact() {
        let (_d, brew, prefix) = formula_fixture();
        let info=serde_json::json!({"formulae":[{"full_name":"homebrew/core/demo","installed":[{"version":"1.0","keg_only":false,"prefix":prefix.join("Cellar/demo/1.0").to_string_lossy()}]}]}).to_string();
        let target = prefix.join("Cellar/demo/1.0/bin/demo");
        let runner = FixtureProbeRunner::new([
            output("Homebrew 4.2.0"),
            output(&prefix.to_string_lossy()),
            output(&info),
            output(&target.to_string_lossy()),
        ]);
        let found = BrewFormulaAdapter::new(&brew).candidates(&runner).unwrap();
        assert_eq!(
            found.len(),
            1,
            "requests={:?} info={info}",
            runner.requests()
        );
        assert_eq!(found[0].identity.fields["name"], "homebrew/core/demo");
        let requests = runner.requests();
        assert!(requests
            .iter()
            .all(|r| r.env.get(ENV_KEY).map(String::as_str) == Some("1")));
        assert_eq!(
            update_argv(&found[0].identity).unwrap(),
            [
                found[0].identity.fields["managerExecutable"].clone(),
                "upgrade".into(),
                "--formula".into(),
                "homebrew/core/demo".into()
            ]
        );
    }
    #[test]
    fn unsupported_brew_version_fails_closed_after_version_probe() {
        let (_d, brew, _prefix) = formula_fixture();
        let r = FixtureProbeRunner::new([output("Homebrew 2.9.0")]);
        assert!(BrewFormulaAdapter::new(&brew)
            .candidates(&r)
            .unwrap()
            .is_empty());
        assert_eq!(r.requests().len(), 1);
        assert_eq!(r.requests()[0].env[ENV_KEY], "1");
    }
    #[test]
    fn tap_qualified_keg_only_and_prefixes_keep_distinct_identity() {
        let (_d, brew, prefix) = formula_fixture();
        let info = serde_json::json!({"formulae":[{"full_name":"custom/tap/demo","installed":[{"version":"1.0","keg_only":true,"prefix":prefix.join("Cellar/demo/1.0").to_string_lossy()}]}]}).to_string();
        let target = prefix.join("Cellar/demo/1.0/bin/demo");
        let runner = FixtureProbeRunner::new([output("Homebrew 4.2.0"), output(&prefix.to_string_lossy()), output(&info), output(&target.to_string_lossy())]);
        let found = BrewFormulaAdapter::new(&brew).candidates(&runner).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].identity.fields["name"], "custom/tap/demo");
        assert_eq!(found[0].identity.fields["kegOnly"], "true");
        assert_eq!(found[0].identity.fields["providerHint"], "unknown");
        let mut other_prefix = found[0].identity.clone();
        other_prefix.fields.insert("prefix".into(), "another-prefix".into());
        assert_ne!(found[0].identity, other_prefix);
    }
    #[test]
    fn formula_pinned_and_unpinned_state_is_reported() {
        let (_d, brew, _prefix) = formula_fixture();
        let mut fields = BTreeMap::new();
        fields.insert("managerExecutable".into(), brew.to_string_lossy().into_owned());
        fields.insert("name".into(), "homebrew/core/demo".into());
        let id = InstallationIdentity {
            adapter_kind: AdapterKind::BrewFormula,
            fields,
        };
        let adapter = BrewFormulaAdapter::for_recorded_update(id);

        let pinned_runner = FixtureProbeRunner::new([output("homebrew/core/demo\n")]);
        let pin_state = adapter.manager_pin_with_runner(&pinned_runner).unwrap();
        assert_eq!(pin_state, ManagerPinState::Pinned);
        assert_eq!(pinned_runner.requests().len(), 1);
        assert_eq!(pinned_runner.requests()[0].argv, &["list", "--pinned"]);
        assert_eq!(pinned_runner.requests()[0].env[ENV_KEY], "1");

        let unpinned_runner = FixtureProbeRunner::new([output("other/formula\n")]);
        let unpin_state = adapter.manager_pin_with_runner(&unpinned_runner).unwrap();
        assert_eq!(unpin_state, ManagerPinState::Unpinned);
        assert_eq!(unpinned_runner.requests().len(), 1);
        assert_eq!(unpinned_runner.requests()[0].env[ENV_KEY], "1");

        assert_eq!(
            adapter
                .remote_update_status(&DiscoveryQuery { online: true })
                .unwrap(),
            crate::tool_manager::adapters::RemoteUpdateStatus::Unsupported
        );
        assert_eq!(
            adapter
                .remote_update_status(&DiscoveryQuery { online: false })
                .unwrap(),
            crate::tool_manager::adapters::RemoteUpdateStatus::NotChecked
        );
    }
}
