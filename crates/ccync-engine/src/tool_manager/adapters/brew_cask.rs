//! Homebrew cask discovery accepts only typed executable artifacts.
use super::{
    brew_formula::{canonical_executable, run},
    AdapterCapabilities, AdapterError, CapabilityState, ToolAdapter,
};
use crate::tool_manager::{
    discovery::{DiscoveryCandidate, DiscoveryQuery},
    file_identity::{hash_same_handle, is_reparse_or_link},
    model::{AdapterKind, EvidenceLevel, InstallationIdentity},
    probe::{ProbeRunner, SystemProbeRunner},
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub struct BrewCaskAdapter {
    executable: PathBuf,
    recorded: Option<InstallationIdentity>,
}
impl Default for BrewCaskAdapter {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("brew"),
            recorded: None,
        }
    }
}
impl BrewCaskAdapter {
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
            Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
            Err(_) => return Ok(vec![]),
        };
        if !version_supported(&version) {
            return Ok(vec![]);
        }
        let prefix = match run(runner, &brew, &["--prefix"]) {
            Ok(o) => fs::canonicalize(PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
                .unwrap_or_default(),
            Err(_) => return Ok(vec![]),
        };
        if prefix.as_os_str().is_empty() {
            return Ok(vec![]);
        }
        let info = match run(runner, &brew, &["info", "--json=v2", "--installed"]) {
            Ok(o) => serde_json::from_slice::<Value>(&o.stdout).ok(),
            Err(_) => None,
        };
        let Some(casks) = info
            .as_ref()
            .and_then(|v| v.get("casks"))
            .and_then(Value::as_array)
        else {
            return Ok(vec![]);
        };
        let mut out = Vec::new();
        for cask in casks {
            let (Some(token), Some(full_token)) = (
                cask.get("token").and_then(Value::as_str),
                cask.get("full_token").and_then(Value::as_str),
            ) else {
                continue;
            };
            let bare_proven = official_bare_token_equivalent(cask, token, full_token);
            let token_used = if token == full_token || bare_proven { full_token } else { continue };
            let artifacts = cask
                .get("artifacts")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let executable_artifacts: Vec<_> = artifacts
                .iter()
                .filter_map(typed_executable_artifact)
                .collect();
            if executable_artifacts.len() != 1 {
                continue;
            }
            let (kind, source, target) = executable_artifacts[0].clone();
            let source_path = absolute_or_prefix(&prefix, &source);
            let target_path = absolute_or_prefix(&prefix, &target);
            let listed = match run(runner, &brew, &["list", "--cask", token_used]) {
                Ok(o) => String::from_utf8_lossy(&o.stdout).lines().any(|l| {
                    fs::canonicalize(l.trim()).ok() == fs::canonicalize(&source_path).ok()
                }),
                Err(_) => false,
            };
            let link = prefix
                .join("bin")
                .join(Path::new(&target).file_name().unwrap_or_default());
            let link_agrees = fs::canonicalize(&link).ok().as_ref()
                == fs::canonicalize(&target_path).ok().as_ref();
            if !listed || !link_agrees {
                continue;
            }
            let Ok(resolved) = fs::canonicalize(&target_path) else {
                continue;
            };
            if !resolved.is_file() || is_reparse_or_link(&resolved).unwrap_or(true) {
                continue;
            }
            let Ok(hash) = hash_same_handle(&resolved) else {
                continue;
            };
            let mut fields = BTreeMap::new();
            fields.insert(
                "managerExecutable".into(),
                brew.to_string_lossy().into_owned(),
            );
            fields.insert(
                "brewIdentity".into(),
                format!("{}:{}", brew.display(), file_identity(&brew)),
            );
            fields.insert("prefix".into(), prefix.to_string_lossy().into_owned());
            fields.insert("token".into(), token.into());
            fields.insert("fullToken".into(), full_token.into());
            fields.insert("name".into(), full_token.into());
            fields.insert(
                "version".into(),
                cask.get("version")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .into(),
            );
            fields.insert("artifactType".into(), kind);
            fields.insert(
                "artifactSource".into(),
                source_path.to_string_lossy().into_owned(),
            );
            fields.insert(
                "artifactTarget".into(),
                target_path.to_string_lossy().into_owned(),
            );
            fields.insert(
                "resolvedTarget".into(),
                resolved.to_string_lossy().into_owned(),
            );
            fields.insert("executable".into(), resolved.to_string_lossy().into_owned());
            fields.insert("hash".into(), hash);
            fields.insert("fileIdentity".into(), file_identity(&resolved));
            let identity = InstallationIdentity {
                adapter_kind: AdapterKind::BrewCask,
                fields,
            };
            if self.recorded.as_ref().is_some_and(|x| x != &identity) {
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
        Ok(out)
    }
}
impl ToolAdapter for BrewCaskAdapter {
    fn kind(&self) -> AdapterKind {
        AdapterKind::BrewCask
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
    fn update_argv(&self) -> Option<Vec<String>> {
        update_argv(self.recorded.as_ref()?)
    }
}
pub fn update_argv(identity: &InstallationIdentity) -> Option<Vec<String>> {
    if identity.adapter_kind != AdapterKind::BrewCask {
        return None;
    }
    Some(vec![
        identity.fields.get("managerExecutable")?.clone(),
        "upgrade".into(),
        "--cask".into(),
        identity.fields.get("fullToken")?.clone(),
    ])
}
fn typed_executable_artifact(value: &Value) -> Option<(String, String, String)> {
    let obj = value.as_object()?;
    if obj.len() != 1 {
        return None;
    }
    let (kind, payload) = obj.iter().next()?;
    if kind != "binary" && kind != "command_wrapper" {
        return None;
    }
    let p = payload.as_object()?;
    let source = p.get("source").and_then(Value::as_str)?;
    let target = p.get("target").and_then(Value::as_str)?;
    if source.is_empty() || target.is_empty() || source.contains("..") || target.contains("..") {
        return None;
    }
    Some((kind.clone(), source.into(), target.into()))
}
fn official_bare_token_equivalent(cask: &Value, token: &str, full: &str) -> bool {
    cask.get("tap")
        .and_then(Value::as_str)
        .is_some_and(|tap| tap == "homebrew/cask")
        && full.strip_prefix("homebrew/cask/") == Some(token)
}
fn absolute_or_prefix(prefix: &Path, p: &str) -> PathBuf {
    let path = PathBuf::from(p);
    if path.is_absolute() {
        path
    } else {
        prefix.join(path)
    }
}
fn version_supported(s: &str) -> bool {
    let part = s
        .split_whitespace()
        .find(|x| x.as_bytes().first().is_some_and(u8::is_ascii_digit))
        .unwrap_or("");
    let mut n = part.split('.');
    let (Some(a), Some(_)) = (
        n.next().and_then(|x| x.parse::<u32>().ok()),
        n.next().and_then(|x| x.parse::<u32>().ok()),
    ) else {
        return false;
    };
    a >= 3
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
    #[test]
    fn typed_binary_cask_and_bare_token_update_are_exact() {
        let d = tempfile::tempdir().unwrap();
        let brew = d.path().join("brew");
        fs::write(&brew, b"brew").unwrap();
        let prefix = d.path().join("prefix");
        let appbin = prefix.join("bin/demo");
        fs::create_dir_all(appbin.parent().unwrap()).unwrap();
        fs::write(&appbin, b"binary").unwrap();
        let info=serde_json::json!({"casks":[{"token":"demo","full_token":"homebrew/cask/demo","tap":"homebrew/cask","version":"1.0","artifacts":[{"binary":{"source":appbin.to_string_lossy(),"target":appbin.to_string_lossy()}}]}]}).to_string();
        let runner = FixtureProbeRunner::new([
            output("Homebrew 4.2.0"),
            output(&prefix.to_string_lossy()),
            output(&info),
            output(&appbin.to_string_lossy()),
        ]);
        let found = BrewCaskAdapter::new(&brew).candidates(&runner).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].identity.fields["fullToken"], "homebrew/cask/demo");
        assert!(runner.requests().iter().all(|r| r
            .env
            .get("HOMEBREW_NO_AUTO_UPDATE")
            .map(String::as_str)
            == Some("1")));
        assert_eq!(
            update_argv(&found[0].identity).unwrap(),
            [
                found[0].identity.fields["managerExecutable"].clone(),
                "upgrade".into(),
                "--cask".into(),
                "homebrew/cask/demo".into()
            ]
        );
        let disagree=serde_json::json!({"casks":[{"token":"demo","full_token":"homebrew/cask/demo","tap":"homebrew/cask","version":"1.0","artifacts":[{"binary":{"source":appbin.to_string_lossy(),"target":prefix.join("bin/other").to_string_lossy()}}]}]}).to_string();
        let bad = FixtureProbeRunner::new([
            output("Homebrew 4.2.0"),
            output(&prefix.to_string_lossy()),
            output(&disagree),
            output(&appbin.to_string_lossy()),
        ]);
        assert!(BrewCaskAdapter::new(&brew)
            .candidates(&bad)
            .unwrap()
            .is_empty());
    }
    #[test]
    fn app_artifact_is_not_a_trackable_cask() {
        assert!(typed_executable_artifact(&serde_json::json!({"app":"Demo.app"})).is_none());
        assert!(typed_executable_artifact(&serde_json::json!({"binary":"demo"})).is_none());
        assert!(typed_executable_artifact(
            &serde_json::json!({"command_wrapper":{"source":"source/demo","target":"bin/demo"}})
        )
        .is_some());
        assert!(typed_executable_artifact(
            &serde_json::json!({"installer":{"source":"setup.pkg","target":"install"}})
        )
        .is_none());
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/tool_manager/brew_cask/app.json"
        ))
        .unwrap();
        assert!(fixture["casks"][0]["artifacts"][0].get("app").is_some());
    }
    #[test]
    fn qualifying_command_wrapper_cask_is_verified_and_online_status_is_exact() {
        let d = tempfile::tempdir().unwrap();
        let brew = d.path().join("brew");
        fs::write(&brew, b"brew").unwrap();
        let prefix = d.path().join("prefix");
        let source_file = prefix.join("source/wrapper");
        let target_file = prefix.join("bin/wrapper");
        fs::create_dir_all(source_file.parent().unwrap()).unwrap();
        fs::write(&source_file, b"source").unwrap();
        fs::create_dir_all(target_file.parent().unwrap()).unwrap();
        fs::write(&target_file, b"wrapper-bin").unwrap();

        let fixture: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/tool_manager/brew_cask/command_wrapper.json"
        ))
        .unwrap();
        let info = fixture.to_string();

        let runner = FixtureProbeRunner::new([
            output("Homebrew 4.2.0"),
            output(&prefix.to_string_lossy()),
            output(&info),
            output(&source_file.to_string_lossy()),
        ]);

        let adapter = BrewCaskAdapter::new(&brew);
        let found = adapter.candidates(&runner).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].evidence_level, EvidenceLevel::Verified);
        assert_eq!(found[0].identity.fields["artifactType"], "command_wrapper");
        assert_eq!(found[0].identity.fields["fullToken"], "homebrew/cask/demo-wrap");
        assert_eq!(found[0].identity.fields["token"], "demo-wrap");
        assert_eq!(found[0].identity.fields["version"], "1.2.0");
        assert!(runner.requests().iter().all(|r| r
            .env
            .get("HOMEBREW_NO_AUTO_UPDATE")
            .map(String::as_str)
            == Some("1")));

        assert_eq!(
            update_argv(&found[0].identity).unwrap(),
            [
                found[0].identity.fields["managerExecutable"].clone(),
                "upgrade".into(),
                "--cask".into(),
                "homebrew/cask/demo-wrap".into()
            ]
        );

        let recorded_adapter = BrewCaskAdapter::for_recorded_update(found[0].identity.clone());
        assert_eq!(
            recorded_adapter
                .remote_update_status(&DiscoveryQuery { online: true })
                .unwrap(),
            crate::tool_manager::adapters::RemoteUpdateStatus::Unsupported
        );
        assert_eq!(
            recorded_adapter
                .remote_update_status(&DiscoveryQuery { online: false })
                .unwrap(),
            crate::tool_manager::adapters::RemoteUpdateStatus::NotChecked
        );
    }
}
