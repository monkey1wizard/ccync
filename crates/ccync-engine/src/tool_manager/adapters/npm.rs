//! Global npm package discovery from npm's structured inventory and executable links.
use super::{AdapterCapabilities, AdapterError, CapabilityState, ToolAdapter};
#[cfg(unix)]
use crate::tool_manager::link_chain::{resolve_link_chain, LinkChainPolicy};
use crate::tool_manager::{
    discovery::{DiscoveryCandidate, DiscoveryQuery},
    file_identity::hash_same_handle,
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
pub struct NpmAdapter {
    executable: PathBuf,
    recorded: Option<InstallationIdentity>,
}
impl Default for NpmAdapter {
    fn default() -> Self {
        Self {
            executable: "npm".into(),
            recorded: None,
        }
    }
}
impl NpmAdapter {
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
        let prefix = output_path(run(runner, &exe, &["prefix", "-g"])?.stdout)?;
        let root = output_path(run(runner, &exe, &["root", "-g"])?.stdout)?;
        let listing = run(runner, &exe, &["ls", "-g", "--depth=0", "--json"])?;
        let doc: Value = serde_json::from_slice(&listing.stdout)
            .map_err(|_| AdapterError::Failed("npm returned invalid JSON".into()))?;
        let Some(packages) = doc.get("dependencies").and_then(Value::as_object) else {
            return Ok(vec![]);
        };
        let mut out = Vec::new();
        for (package, record) in packages {
            let Some(version) = record.get("version").and_then(Value::as_str) else {
                continue;
            };
            let Some(bins) = record.get("bin") else {
                continue;
            };
            for (bin_name, target_rel) in bin_entries(package, bins) {
                let link = prefix.join("bin").join(with_exe(&bin_name));
                #[cfg(unix)]
                let (target, evidence) = {
                    let resolved = match resolve_link_chain(
                        &link,
                        &LinkChainPolicy {
                            containment_roots: vec![root.clone()],
                            max_hops: 16,
                        },
                    ) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if !resolved
                        .target
                        .starts_with(fs::canonicalize(&root).unwrap_or(root.clone()))
                    {
                        continue;
                    }
                    let expected = root.join(package).join(&target_rel);
                    let evidence =
                        if fs::canonicalize(expected).ok().as_ref() == Some(&resolved.target) {
                            EvidenceLevel::Verified
                        } else {
                            EvidenceLevel::Heuristic
                        };
                    (resolved.target, evidence)
                };
                #[cfg(windows)]
                let (target, evidence) = {
                    let shim = match fs::read_to_string(link.with_extension("cmd")) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    let rel_str = target_rel.to_string_lossy();
                    match parse_windows_shim(&shim, package, &rel_str) {
                        Some(()) => (root.join(package).join(&target_rel), EvidenceLevel::Verified),
                        None => (root.join(package).join(&target_rel), EvidenceLevel::Heuristic),
                    }
                };
                #[cfg(not(any(unix, windows)))]
                let (target, evidence) = (root.join(package).join(&target_rel), EvidenceLevel::Heuristic);
                let Ok(hash) = hash_same_handle(&target) else {
                    continue;
                };
                let mut fields = BTreeMap::new();
                fields.insert(
                    "managerExecutable".into(),
                    exe.to_string_lossy().into_owned(),
                );
                fields.insert("name".into(), package.clone());
                fields.insert("version".into(), version.into());
                fields.insert("prefix".into(), prefix.to_string_lossy().into_owned());
                fields.insert("root".into(), root.to_string_lossy().into_owned());
                fields.insert("bin".into(), bin_name);
                fields.insert("executable".into(), target.to_string_lossy().into_owned());
                fields.insert("hash".into(), hash);
                let identity = InstallationIdentity {
                    adapter_kind: AdapterKind::NpmGlobal,
                    fields,
                };
                if self.recorded.as_ref().is_some_and(|v| v != &identity) {
                    continue;
                }
                out.push(DiscoveryCandidate::new(
                    identity, evidence, true, true, false,
                ));
            }
        }
        Ok(out)
    }
}
impl ToolAdapter for NpmAdapter {
    fn kind(&self) -> AdapterKind {
        AdapterKind::NpmGlobal
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
    fn discover(&self, q: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
        let _ = q;
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
            super::RemoteUpdateStatus::Unsupported
        } else {
            super::RemoteUpdateStatus::NotChecked
        })
    }
    fn update_argv(&self) -> Option<Vec<String>> {
        update_argv(self.recorded.as_ref()?)
    }
}
pub fn update_argv(identity: &InstallationIdentity) -> Option<Vec<String>> {
    if identity.adapter_kind != AdapterKind::NpmGlobal {
        return None;
    }
    let exe = identity.fields.get("managerExecutable")?;
    let package = identity.fields.get("name")?;
    if exe.is_empty() || package.is_empty() || package.starts_with('-') {
        return None;
    }
    Some(vec![
        exe.clone(),
        "update".into(),
        "-g".into(),
        package.clone(),
    ])
}
fn bin_entries(package: &str, value: &Value) -> Vec<(String, PathBuf)> {
    if let Some(s) = value.as_str() {
        return vec![(
            package.rsplit('/').next().unwrap_or(package).into(),
            PathBuf::from(s),
        )];
    }
    value
        .as_object()
        .into_iter()
        .flat_map(|o| o.iter())
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), PathBuf::from(s))))
        .collect()
}
fn output_path(bytes: Vec<u8>) -> Result<PathBuf, AdapterError> {
    let s = String::from_utf8(bytes)
        .map_err(|_| AdapterError::Failed("npm returned invalid path".into()))?;
    let p = PathBuf::from(s.trim());
    if p.is_absolute() {
        Ok(p)
    } else {
        Err(AdapterError::Failed(
            "npm returned non-absolute path".into(),
        ))
    }
}
fn with_exe(s: &str) -> String {
    if cfg!(windows) {
        format!("{s}.cmd")
    } else {
        s.into()
    }
}
pub(super) fn parse_windows_shim(text: &str, package: &str, bin: &str) -> Option<()> {
    let normalized = text.replace("\r\n", "\n");
    if normalized.lines().any(|line| {
        line.to_ascii_lowercase().contains("node ")
            && !line.to_ascii_lowercase().contains("%dp0%\\node_modules\\")
    }) {
        return None;
    }
    let targets: Vec<_> = normalized
        .lines()
        .filter_map(|line| {
            let lower = line.to_ascii_lowercase();
            let pos = lower.find("%dp0%\\node_modules\\")?;
            let tail = &line[pos + "%dp0%\\node_modules\\".len()..];
            let target = format!("{}\\{}", package.replace('/', "\\"), bin.replace('/', "\\"));
            tail.to_ascii_lowercase()
                .starts_with(&target.to_ascii_lowercase())
                .then_some(tail[..target.len().min(tail.len())].to_owned())
        })
        .collect();
    (targets.len() == 1
        && targets[0].eq_ignore_ascii_case(&format!(
            "{}\\{}",
            package.replace('/', "\\"),
            bin.replace('/', "\\")
        )))
    .then_some(())
}
fn canonical_executable(path: &Path) -> Option<PathBuf> {
    let found = executable_candidates(path)
        .into_iter()
        .find(|p| p.is_file())?;
    if crate::tool_manager::file_identity::is_reparse_or_link(&found).ok()? {
        return None;
    }
    fs::canonicalize(found).ok()
}
fn executable_candidates(path: &Path) -> Vec<PathBuf> {
    if path.is_absolute() {
        return vec![path.into()];
    }
    let names = executable_names(path);
    env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .collect()
}
fn executable_names(path: &Path) -> Vec<PathBuf> {
    let mut names = Vec::new();
    #[cfg(windows)]
    if path.extension().is_none() {
        let exts = env::var_os("PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
        names.extend(
            exts.to_string_lossy()
                .split(';')
                .filter(|x| !x.is_empty())
                .map(|x| path.with_extension(x.trim_start_matches('.'))),
        );
    }
    names.push(path.to_path_buf());
    names
}
fn run(
    r: &dyn ProbeRunner,
    exe: &Path,
    argv: &[&str],
) -> Result<crate::tool_manager::probe::ProbeOutput, AdapterError> {
    let out = r.run(&ProbeRequest {
        program: exe.into(),
        argv: argv.iter().map(|s| (*s).into()).collect(),
        env: BTreeMap::new(),
        working_directory: env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        timeout: Duration::from_secs(20),
        output_byte_cap: CAP,
    });
    if out.exit_code != Some(0)
        || out.timed_out
        || out.stdout_truncated
        || out.spawn_error.is_some()
    {
        Err(AdapterError::Failed("npm probe failed".into()))
    } else {
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_manager::probe::{FixtureProbeRunner, ProbeOutput};

    #[test]
    #[cfg(windows)]
    fn npm_windows_fixture_prefers_cmd_over_extensionless_bash_shim() {
        let fixture = tempfile::tempdir().unwrap();
        fs::write(fixture.path().join("npm"), "#!/usr/bin/env bash\n").unwrap();
        fs::write(fixture.path().join("npm.cmd"), "@echo off\r\n").unwrap();

        let names = executable_names(Path::new("npm"));
        let selected = names
            .into_iter()
            .find(|name| fixture.path().join(name).is_file());
        assert_eq!(
            selected.unwrap().to_string_lossy().to_ascii_lowercase(),
            "npm.cmd"
        );
    }

    fn out(s: &str) -> ProbeOutput {
        ProbeOutput {
            stdout: s.as_bytes().to_vec(),
            exit_code: Some(0),
            ..Default::default()
        }
    }

    #[test]
    fn parses_agreeing_windows_shim_and_unrecognized_shim() {
        // Agreeing Windows shim: single target matches %dp0%\node_modules\<package>\<bin>
        assert!(parse_windows_shim(
            "@SETLOCAL\r\nnode \"%dp0%\\node_modules\\demo\\bin\\cli.js\" %*\r\n",
            "demo",
            "bin/cli.js"
        )
        .is_some());

        // Unrecognized shim: foreign command / shape -> heuristic (None)
        assert!(parse_windows_shim(
            "node %dp0%\\node_modules\\demo\\bin\\cli.js\nnode evil\n",
            "demo",
            "bin/cli.js"
        )
        .is_none());

        assert!(parse_windows_shim(
            "@SETLOCAL\r\nnode \"C:\\foreign\\path.js\" %*\r\n",
            "demo",
            "bin/cli.js"
        )
        .is_none());
    }

    #[test]
    #[cfg(unix)]
    fn agreeing_unix_link_resolves_inside_global_root() {
        use crate::tool_manager::link_chain::{resolve_link_chain, LinkChainPolicy};
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("lib/node_modules");
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(root.join("demo/bin")).unwrap();
        fs::create_dir_all(&bin_dir).unwrap();
        let target = root.join("demo/bin/cli.js");
        fs::write(&target, b"#!/usr/bin/env node\n").unwrap();
        let link = bin_dir.join("demo");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let resolved = resolve_link_chain(
            &link,
            &LinkChainPolicy {
                containment_roots: vec![tmp.path().to_path_buf()],
                max_hops: 16,
            },
        )
        .unwrap();
        assert_eq!(resolved.target, fs::canonicalize(&target).unwrap());
    }

    #[test]
    fn update_argv_is_exact() {
        let id = InstallationIdentity {
            adapter_kind: AdapterKind::NpmGlobal,
            fields: [
                ("managerExecutable".into(), "/usr/bin/npm".into()),
                ("name".into(), "demo".into()),
            ]
            .into(),
        };
        assert_eq!(
            update_argv(&id).unwrap(),
            ["/usr/bin/npm", "update", "-g", "demo"]
        );
    }

    #[test]
    fn npm_fixture_schema_has_bin_and_excludes_local_dependency() {
        let doc: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/tool_manager/npm/list.json"
        ))
        .unwrap();
        let p = doc["dependencies"].as_object().unwrap();
        // demo package has bin field
        assert!(p.contains_key("demo"));
        assert!(p["demo"].get("bin").is_some());
        // package without bin field
        assert!(p.contains_key("no-bin"));
        assert!(p["no-bin"].get("bin").is_none());
        // project-local devDependencies are excluded
        assert!(doc.get("devDependencies").is_none());
    }

    #[test]
    fn npm_candidates_probe_with_fixtures() {
        let tmp = tempfile::tempdir().unwrap();
        let fake_exe = tmp.path().join(if cfg!(windows) { "npm.cmd" } else { "npm" });
        fs::write(&fake_exe, b"echo npm").unwrap();

        let prefix = tmp.path().join("prefix");
        let root = prefix.join("lib").join("node_modules");
        let bin_dir = prefix.join("bin");
        let target = root.join("demo").join("bin").join("cli.js");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::create_dir_all(&bin_dir).unwrap();
        fs::write(&target, b"console.log('demo');").unwrap();

        #[cfg(windows)]
        {
            let shim_content = "@SETLOCAL\r\nnode \"%dp0%\\node_modules\\demo\\bin\\cli.js\" %*\r\n";
            fs::write(bin_dir.join("demo.cmd"), shim_content).unwrap();
        }
        #[cfg(unix)]
        {
            let link = bin_dir.join("demo");
            let _ = std::os::unix::fs::symlink(&target, &link);
        }

        let list_json = include_str!("../../../tests/fixtures/tool_manager/npm/list.json");
        let runner = FixtureProbeRunner::new([
            out(&prefix.to_string_lossy()),
            out(&root.to_string_lossy()),
            out(list_json),
        ]);

        let adapter = NpmAdapter::new(&fake_exe);
        let candidates = adapter.candidates(&runner).unwrap();
        assert_eq!(candidates.len(), 1);
        let c = &candidates[0];
        assert_eq!(c.identity.fields.get("name").unwrap(), "demo");
        assert_eq!(c.identity.fields.get("version").unwrap(), "1.2.3");
        assert_eq!(c.identity.fields.get("bin").unwrap(), "demo");
        #[cfg(windows)]
        assert_eq!(c.evidence_level, EvidenceLevel::Verified);
    }

}
