//! Global pnpm package discovery from pnpm's JSON inventory and bin links.
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
    collections::{BTreeMap, HashMap},
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};
const CAP: usize = 1024 * 1024;
#[derive(Debug, Clone)]
pub struct PnpmAdapter {
    executable: PathBuf,
    recorded: Option<InstallationIdentity>,
}
impl Default for PnpmAdapter {
    fn default() -> Self {
        Self {
            executable: "pnpm".into(),
            recorded: None,
        }
    }
}
impl PnpmAdapter {
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
    fn candidates(&self, r: &dyn ProbeRunner) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
        let Some(exe) = canonical_executable(&self.executable) else {
            return Ok(vec![]);
        };
        let Some(root) = run_for_candidates(r, &exe, &["root", "-g"])? else {
            return Ok(vec![]);
        };
        let global = output_path(root.stdout)?;
        let Some(bin_output) = run_for_candidates(r, &exe, &["bin", "-g"])? else {
            return Ok(vec![]);
        };
        let bin = output_path(bin_output.stdout)?;
        let Some(listing) = run_for_candidates(r, &exe, &["list", "-g", "--depth=0", "--json"])? else {
            return Ok(vec![]);
        };
        let doc: Value = serde_json::from_slice(&listing.stdout)
            .map_err(|_| AdapterError::Failed("pnpm returned invalid JSON".into()))?;
        let Some(groups) = doc.as_array() else {
            return Ok(vec![]);
        };
        let mut occurrences: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for group in groups {
            if let Some(deps) = group.get("dependencies").and_then(Value::as_object) {
                for (name, record) in deps {
                    if let Some(version) = record.get("version").and_then(Value::as_str) {
                        occurrences.entry(name.clone()).or_default().push((
                            version.into(),
                            group
                                .get("path")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .into(),
                        ));
                    }
                }
            }
        }
        let mut out = Vec::new();
        for (package, entries) in occurrences {
            if entries.len() != 1 {
                let mut fields = BTreeMap::new();
                fields.insert(
                    "managerExecutable".into(),
                    exe.to_string_lossy().into_owned(),
                );
                fields.insert("name".into(), package.clone());
                fields.insert(
                    "version".into(),
                    entries.first().map(|v| v.0.clone()).unwrap_or_default(),
                );
                fields.insert("globalDir".into(), global.to_string_lossy().into_owned());
                fields.insert("binDir".into(), bin.to_string_lossy().into_owned());
                fields.insert("isolatedGroupAmbiguity".into(), "true".into());
                let identity = InstallationIdentity {
                    adapter_kind: AdapterKind::PnpmGlobal,
                    fields,
                };
                if self.recorded.as_ref().is_none_or(|v| v == &identity) {
                    out.push(DiscoveryCandidate::new(
                        identity,
                        EvidenceLevel::Unknown,
                        false,
                        false,
                        false,
                    ));
                }
                continue;
            }
            let (version, group_path) = &entries[0];
            let Some(record) = groups
                .iter()
                .find(|g| g.get("path").and_then(Value::as_str).unwrap_or("") == group_path)
            else {
                continue;
            };
            let Some(dep) = record.pointer(&format!("/dependencies/{}", pointer_escape(&package)))
            else {
                continue;
            };
            let Some(bins) = dep.get("bin") else { continue };
            let bin_entries: Vec<(String, String)> = if let Some(s) = bins.as_str() {
                vec![(
                    package.rsplit('/').next().unwrap_or(&package).into(),
                    s.into(),
                )]
            } else {
                bins.as_object()
                    .into_iter()
                    .flat_map(|m| m.iter())
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.into())))
                    .collect()
            };
            for (name, relative) in bin_entries {
                let link = bin.join(with_exe(&name));
                #[cfg(unix)]
                let (target, evidence) = {
                    let resolved = match resolve_link_chain(
                        &link,
                        &LinkChainPolicy {
                            containment_roots: vec![global.clone(), bin.clone()],
                            max_hops: 16,
                        },
                    ) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    let target_root = fs::canonicalize(global.join(group_path))
                        .unwrap_or_else(|_| global.join(group_path));
                    if !resolved.target.starts_with(&target_root) {
                        continue;
                    }
                    let expected = target_root.join(relative);
                    let level =
                        if fs::canonicalize(expected).ok().as_ref() == Some(&resolved.target) {
                            EvidenceLevel::Verified
                        } else {
                            EvidenceLevel::Heuristic
                        };
                    (resolved.target, level)
                };
                #[cfg(windows)]
                let (target, evidence) = {
                    let shim = match fs::read_to_string(link.with_extension("cmd")) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    match super::npm::parse_windows_shim(&shim, &package, &relative) {
                        Some(()) => (
                            global.join(group_path).join(relative),
                            EvidenceLevel::Verified,
                        ),
                        None => (
                            global.join(group_path).join(relative),
                            EvidenceLevel::Heuristic,
                        ),
                    }
                };
                #[cfg(not(any(unix, windows)))]
                let (target, evidence) = (
                    global.join(group_path).join(relative),
                    EvidenceLevel::Heuristic,
                );
                let Ok(hash) = hash_same_handle(&target) else {
                    continue;
                };
                let mut fields = BTreeMap::new();
                fields.insert(
                    "managerExecutable".into(),
                    exe.to_string_lossy().into_owned(),
                );
                fields.insert("name".into(), package.clone());
                fields.insert("version".into(), version.clone());
                fields.insert("globalDir".into(), global.to_string_lossy().into_owned());
                fields.insert("binDir".into(), bin.to_string_lossy().into_owned());
                fields.insert("group".into(), group_path.clone());
                fields.insert("bin".into(), name);
                fields.insert("executable".into(), target.to_string_lossy().into_owned());
                fields.insert("hash".into(), hash);
                let identity = InstallationIdentity {
                    adapter_kind: AdapterKind::PnpmGlobal,
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
impl ToolAdapter for PnpmAdapter {
    fn kind(&self) -> AdapterKind {
        AdapterKind::PnpmGlobal
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
pub fn update_argv(id: &InstallationIdentity) -> Option<Vec<String>> {
    if id.adapter_kind != AdapterKind::PnpmGlobal {
        return None;
    }
    let exe = id.fields.get("managerExecutable")?;
    let package = id.fields.get("name")?;
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
fn pointer_escape(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}
fn output_path(bytes: Vec<u8>) -> Result<PathBuf, AdapterError> {
    let s = String::from_utf8(bytes)
        .map_err(|_| AdapterError::Failed("pnpm returned invalid path".into()))?;
    let p = PathBuf::from(s.trim());
    if p.is_absolute() {
        Ok(p)
    } else {
        Err(AdapterError::Failed(
            "pnpm returned non-absolute path".into(),
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
        argv: argv.iter().map(|x| (*x).into()).collect(),
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
        Err(AdapterError::Failed("pnpm probe failed".into()))
    } else {
        Ok(out)
    }
}

// pnpm uses this error when its global bin directory is not on PATH. In that
// case there cannot be usable global command shims to discover.
fn run_for_candidates(
    r: &dyn ProbeRunner,
    exe: &Path,
    argv: &[&str],
) -> Result<Option<crate::tool_manager::probe::ProbeOutput>, AdapterError> {
    let out = r.run(&ProbeRequest {
        program: exe.into(),
        argv: argv.iter().map(|x| (*x).into()).collect(),
        env: BTreeMap::new(),
        working_directory: env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        timeout: Duration::from_secs(20),
        output_byte_cap: CAP,
    });
    if out.exit_code != Some(0)
        && String::from_utf8_lossy(&out.stderr).contains("ERR_PNPM_GLOBAL_BIN_DIR_NOT_IN_PATH")
    {
        return Ok(None);
    }
    if out.exit_code != Some(0)
        || out.timed_out
        || out.stdout_truncated
        || out.spawn_error.is_some()
    {
        return Err(AdapterError::Failed("pnpm probe failed".into()));
    }
    Ok(Some(out))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_manager::probe::{FixtureProbeRunner, ProbeOutput};

    fn out(s: &str) -> ProbeOutput {
        ProbeOutput {
            stdout: s.as_bytes().to_vec(),
            exit_code: Some(0),
            ..Default::default()
        }
    }

    #[test]
    fn pnpm_global_bin_dir_missing_returns_no_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        let fake_exe = tmp.path().join(if cfg!(windows) { "pnpm.cmd" } else { "pnpm" });
        fs::write(&fake_exe, b"echo pnpm").unwrap();
        let failure = ProbeOutput {
            stderr: b"ERR_PNPM_GLOBAL_BIN_DIR_NOT_IN_PATH".to_vec(),
            exit_code: Some(1),
            ..Default::default()
        };
        let runner = FixtureProbeRunner::new([failure]);
        assert!(PnpmAdapter::new(fake_exe).candidates(&runner).unwrap().is_empty());
    }

    #[test]
    #[cfg(windows)]
    fn pnpm_windows_fixture_prefers_cmd_over_extensionless_bash_shim() {
        let fixture = tempfile::tempdir().unwrap();
        fs::write(fixture.path().join("pnpm"), "#!/usr/bin/env bash\n").unwrap();
        fs::write(fixture.path().join("pnpm.cmd"), "@echo off\r\n").unwrap();

        let selected = executable_names(Path::new("pnpm"))
            .into_iter()
            .find(|name| fixture.path().join(name).is_file());
        assert_eq!(
            selected.unwrap().to_string_lossy().to_ascii_lowercase(),
            "pnpm.cmd"
        );
    }

    #[test]
    fn update_argv_is_exact() {
        let id = InstallationIdentity {
            adapter_kind: AdapterKind::PnpmGlobal,
            fields: [
                ("managerExecutable".into(), "/usr/bin/pnpm".into()),
                ("name".into(), "demo".into()),
            ]
            .into(),
        };
        assert_eq!(
            update_argv(&id).unwrap(),
            ["/usr/bin/pnpm", "update", "-g", "demo"]
        );
    }

    #[test]
    fn fixture_covers_multiple_isolated_groups_schema() {
        let doc: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/tool_manager/pnpm/list.json"
        ))
        .unwrap();
        let groups = doc.as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert!(groups.iter().all(|g| g.get("dependencies").is_some()));
        assert_eq!(doc[0]["dependencies"]["demo"]["bin"]["demo"], "dist/cli.js");
        assert_eq!(doc[0]["dependencies"]["demo"]["version"], "1.2.3");
        assert_eq!(doc[1]["dependencies"]["demo"]["bin"]["demo"], "other.js");
        assert_eq!(doc[1]["dependencies"]["demo"]["version"], "2.0.0");
    }

    #[test]
    fn pnpm_isolated_group_ambiguity_fails_closed_as_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let fake_exe = tmp.path().join(if cfg!(windows) { "pnpm.cmd" } else { "pnpm" });
        fs::write(&fake_exe, b"echo pnpm").unwrap();

        let global = tmp.path().join("global");
        let bin = tmp.path().join("bin");
        fs::create_dir_all(&global).unwrap();
        fs::create_dir_all(&bin).unwrap();

        let list_json = include_str!("../../../tests/fixtures/tool_manager/pnpm/list.json");
        let runner = FixtureProbeRunner::new([
            out(&global.to_string_lossy()),
            out(&bin.to_string_lossy()),
            out(list_json),
        ]);

        let adapter = PnpmAdapter::new(&fake_exe);
        let candidates = adapter.candidates(&runner).unwrap();
        assert_eq!(candidates.len(), 1);
        let c = &candidates[0];
        assert_eq!(c.identity.fields.get("name").unwrap(), "demo");
        assert_eq!(c.evidence_level, EvidenceLevel::Unknown);
        assert_eq!(
            c.identity.fields.get("isolatedGroupAmbiguity").unwrap(),
            "true"
        );
        assert!(!c.trackable);
    }

    #[test]
    fn pnpm_target_mismatch_and_agreement() {
        // Agreement: matching package and bin relative path
        let agreeing_shim = "@SETLOCAL\r\nnode \"%dp0%\\node_modules\\demo\\dist\\cli.js\" %*\r\n";
        assert_eq!(
            super::super::npm::parse_windows_shim(agreeing_shim, "demo", "dist/cli.js"),
            Some(())
        );

        // Mismatch: foreign target in shim -> None (degrades to Heuristic)
        let mismatch_shim = "@SETLOCAL\r\nnode \"%dp0%\\node_modules\\other\\dist\\cli.js\" %*\r\n";
        assert_eq!(
            super::super::npm::parse_windows_shim(mismatch_shim, "demo", "dist/cli.js"),
            None
        );
    }
}
