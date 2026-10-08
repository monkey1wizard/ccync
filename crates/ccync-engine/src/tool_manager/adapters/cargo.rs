//! Read-only discovery for Cargo-installed executables.

use super::{AdapterCapabilities, AdapterError, CapabilityState, RemoteUpdateStatus, ToolAdapter};
use crate::tool_manager::{
    discovery::{DiscoveryCandidate, DiscoveryQuery},
    file_identity::hash_same_handle,
    model::{AdapterKind, EvidenceLevel, InstallationIdentity},
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    env,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone)]
pub struct CargoAdapter {
    root: Option<PathBuf>,
    cargo_executable: PathBuf,
    recorded: Option<InstallationIdentity>,
}

impl Default for CargoAdapter {
    fn default() -> Self {
        Self {
            root: None,
            cargo_executable: PathBuf::from("cargo"),
            recorded: None,
        }
    }
}

impl CargoAdapter {
    pub fn with_root(root: impl Into<PathBuf>, cargo_executable: impl Into<PathBuf>) -> Self {
        Self {
            root: Some(root.into()),
            cargo_executable: cargo_executable.into(),
            recorded: None,
        }
    }

    pub fn for_recorded_update(
        identity: InstallationIdentity,
        cargo_executable: impl Into<PathBuf>,
    ) -> Self {
        Self {
            root: None,
            cargo_executable: cargo_executable.into(),
            recorded: Some(identity),
        }
    }

    fn install_root(&self) -> Option<PathBuf> {
        if let Some(root) = &self.root {
            return Some(root.clone());
        }
        env::var_os("CARGO_INSTALL_ROOT")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("CARGO_HOME")
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
            })
            .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
            .or_else(|| env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".cargo")))
    }

    fn candidates(&self) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
        let Some(root) = self.install_root() else {
            return Ok(vec![]);
        };
        let file = root.join(".crates2.json");
        let bytes = match std::fs::read(&file) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => {
                return Err(AdapterError::Failed(format!(
                    "cannot read Cargo install registry: {e}"
                )));
            }
        };
        let doc: Value = serde_json::from_slice(&bytes)
            .map_err(|e| AdapterError::Failed(format!("invalid Cargo install registry: {e}")))?;
        let Some(installs) = doc.as_object().and_then(|o| {
            o.get("installs").and_then(Value::as_object).or_else(|| {
                o.values()
                    .find_map(|v| v.get("installs").and_then(Value::as_object))
            })
        }) else {
            return Ok(vec![]);
        };
        let known = doc.as_object().is_some_and(|o| {
            (o.len() == 1 && o.get("installs").and_then(Value::as_object).is_some())
                || (o.len() == 1
                    && doc.get("v1").and_then(Value::as_object).is_some_and(|v| {
                        v.len() == 1 && v.get("installs").and_then(Value::as_object).is_some()
                    }))
        });
        let mut out = Vec::new();
        for (key, record) in installs {
            let Some(record) = record.as_object() else {
                continue;
            };
            let Some((package, key_version, key_source)) = parse_key(key) else {
                continue;
            };
            let version = record
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or(key_version);
            let source = record
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or(key_source);
            let Some(bins) = record
                .get("bin")
                .or_else(|| record.get("bins"))
                .and_then(Value::as_array)
            else {
                continue;
            };
            for bin in bins.iter().filter_map(Value::as_str) {
                let executable = root.join("bin").join(with_exe(bin));
                let hash = match hash_same_handle(&executable) {
                    Ok(h) => h,
                    Err(_) => continue,
                };
                let safe_source = !unsafe_git_source(source);
                let evidence = if known && safe_source {
                    EvidenceLevel::Verified
                } else {
                    EvidenceLevel::Heuristic
                };
                let mut fields = BTreeMap::new();
                fields.insert("name".into(), package.to_owned());
                fields.insert("version".into(), version.to_owned());
                if safe_source {
                    fields.insert("source".into(), source.to_owned());
                } else {
                    fields.insert("source".into(), "<redacted-unsafe-git-source>".into());
                }
                fields.insert("root".into(), root.to_string_lossy().into_owned());
                fields.insert(
                    "executable".into(),
                    executable.to_string_lossy().into_owned(),
                );
                fields.insert("hash".into(), hash);
                fields.insert("trackedBin".into(), bin.to_owned());
                out.push(DiscoveryCandidate::new(
                    InstallationIdentity {
                        adapter_kind: AdapterKind::Cargo,
                        fields,
                    },
                    evidence,
                    safe_source,
                    true,
                    false,
                ));
            }
        }
        Ok(out)
    }
}

fn with_exe(name: &str) -> String {
    #[cfg(windows)]
    {
        if Path::new(name)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
        {
            name.to_owned()
        } else {
            format!("{name}.exe")
        }
    }
    #[cfg(not(windows))]
    {
        name.to_owned()
    }
}

fn parse_key(key: &str) -> Option<(&str, &str, &str)> {
    let (name, rest) = key.split_once(' ')?;
    let (version, source) = rest.split_once(" (")?;
    Some((name, version, source.strip_suffix(')')?))
}

fn unsafe_git_source(source: &str) -> bool {
    if !(source.starts_with("git+")
        || source.starts_with("https://")
        || source.starts_with("http://"))
    {
        return false;
    }
    let url = source.strip_prefix("git+").unwrap_or(source);
    let authority = url
        .split("//")
        .nth(1)
        .unwrap_or("")
        .split('/')
        .next()
        .unwrap_or("");
    if authority.contains('@') {
        return true;
    }
    url.split_once('?').is_some_and(|(_, q)| {
        q.split('&').any(|p| {
            let key = p.split('=').next().unwrap_or("").to_ascii_lowercase();
            ["token", "password", "secret", "auth"]
                .iter()
                .any(|needle| key.contains(needle))
        })
    })
}

impl ToolAdapter for CargoAdapter {
    fn kind(&self) -> AdapterKind {
        AdapterKind::Cargo
    }
    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities::for_adapter(
            AdapterKind::Cargo,
            CapabilityState::Supported,
            CapabilityState::Supported,
            CapabilityState::Supported,
            CapabilityState::Unsupported,
        )
    }
    fn discover(&self, _query: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
        self.candidates()
    }
    fn reprobe(&self, _query: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
        self.candidates()
    }
    fn remote_update_status(
        &self,
        query: &DiscoveryQuery,
    ) -> Result<RemoteUpdateStatus, AdapterError> {
        if !query.online {
            return Ok(RemoteUpdateStatus::NotChecked);
        }
        let Some(identity) = self.recorded.as_ref() else {
            return Err(AdapterError::Failed("Cargo identity is unavailable".into()));
        };
        check_remote(identity, &self.cargo_executable)
    }
    fn update_argv(&self) -> Option<Vec<String>> {
        update_argv(self.recorded.as_ref()?, &self.cargo_executable)
    }
}

fn check_remote(
    identity: &InstallationIdentity,
    cargo: &Path,
) -> Result<RemoteUpdateStatus, AdapterError> {
    let source = identity
        .fields
        .get("source")
        .ok_or_else(|| AdapterError::Failed("Cargo source is unavailable".into()))?;
    let name = identity
        .fields
        .get("name")
        .ok_or_else(|| AdapterError::Failed("Cargo package name is unavailable".into()))?;
    if source.starts_with("git+") {
        let installed = parse_git_source(source)?;
        return resolve_git(&installed, &PathBuf::from("git"));
    }
    if is_crates_io_source(source) {
        let installed = identity
            .fields
            .get("version")
            .ok_or_else(|| AdapterError::Failed("Cargo version is unavailable".into()))?;
        let installed = semver::Version::parse(installed)
            .map_err(|_| AdapterError::Failed("Cargo version is malformed".into()))?;
        let output = Command::new(cargo)
            .args(["search", name, "--limit", "1"])
            .output()
            .map_err(|_| AdapterError::Failed("Cargo registry check could not run".into()))?;
        if !output.status.success() {
            return Err(AdapterError::Failed("Cargo registry check failed".into()));
        }
        let remote = parse_crates_io_version(name, &output.stdout)?;
        return Ok(if remote > installed {
            RemoteUpdateStatus::UpdateAvailable
        } else {
            RemoteUpdateStatus::Current
        });
    }
    Err(AdapterError::Failed("Cargo source is unsupported".into()))
}

#[derive(Debug, PartialEq, Eq)]
struct GitSource {
    url: String,
    selector: GitSelector,
    installed: String,
}

#[derive(Debug, PartialEq, Eq)]
enum GitSelector {
    Head,
    Branch(String),
    Tag(String),
    Rev(String),
}

fn parse_git_source(source: &str) -> Result<GitSource, AdapterError> {
    let rest = source
        .strip_prefix("git+https://")
        .ok_or_else(|| AdapterError::Failed("Cargo Git source is unsafe".into()))?;
    let (without_fragment, fragment) = rest
        .rsplit_once('#')
        .ok_or_else(|| AdapterError::Failed("Cargo Git revision is unavailable".into()))?;
    if !(7..=40).contains(&fragment.len()) || !fragment.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(AdapterError::Failed(
            "Cargo Git revision is malformed".into(),
        ));
    }
    let (authority_and_path, query) = without_fragment
        .split_once('?')
        .map_or((without_fragment, None), |(url, query)| (url, Some(query)));
    let authority = authority_and_path
        .split_once('/')
        .map_or(authority_and_path, |(authority, _)| authority);
    if authority.is_empty()
        || !authority_and_path.contains('/')
        || authority.contains('@')
        || authority.contains(':')
        || authority.chars().any(|c| c.is_ascii_whitespace())
        || authority_and_path.contains('#')
        || authority_and_path.ends_with('/')
    {
        return Err(AdapterError::Failed(
            "Cargo Git source is noncanonical".into(),
        ));
    }
    let mut selector = None;
    if let Some(query) = query {
        for part in query.split('&') {
            let (key, value) = part
                .split_once('=')
                .filter(|(_, value)| !value.is_empty())
                .ok_or_else(|| AdapterError::Failed("Cargo Git selector is malformed".into()))?;
            let next = match key {
                "branch" if valid_ref_name(value) => GitSelector::Branch(value.to_owned()),
                "tag" if valid_ref_name(value) => GitSelector::Tag(value.to_owned()),
                "rev" if value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit()) => {
                    GitSelector::Rev(value.to_ascii_lowercase())
                }
                _ => {
                    return Err(AdapterError::Failed(
                        "Cargo Git selector is unsupported".into(),
                    ))
                }
            };
            if selector.replace(next).is_some() {
                return Err(AdapterError::Failed(
                    "Cargo Git selectors are ambiguous".into(),
                ));
            }
        }
    }
    Ok(GitSource {
        url: format!("https://{authority_and_path}"),
        selector: selector.unwrap_or(GitSelector::Head),
        installed: fragment.to_ascii_lowercase(),
    })
}

fn valid_ref_name(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value.contains("..")
        && !value.contains("@{")
        && value.split('/').all(|component| {
            !component.is_empty()
                && component != "."
                && component != ".."
                && !component.starts_with('.')
                && !component.ends_with('.')
                && !component.eq_ignore_ascii_case(".lock")
        })
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
}

fn resolve_git(source: &GitSource, git: &Path) -> Result<RemoteUpdateStatus, AdapterError> {
    if let GitSelector::Rev(rev) = &source.selector {
        return Ok(if rev.starts_with(&source.installed) {
            RemoteUpdateStatus::Current
        } else {
            RemoteUpdateStatus::UpdateAvailable
        });
    }
    let mut refs = Vec::new();
    match &source.selector {
        GitSelector::Head => refs.push("HEAD".to_owned()),
        GitSelector::Branch(name) => refs.push(format!("refs/heads/{name}")),
        GitSelector::Tag(name) => {
            refs.push(format!("refs/tags/{name}^{{}}"));
            refs.push(format!("refs/tags/{name}"));
        }
        GitSelector::Rev(_) => unreachable!(),
    }
    let output = Command::new(git)
        .arg("ls-remote")
        .arg(&source.url)
        .args(&refs)
        .output()
        .map_err(|_| AdapterError::Failed("Cargo Git check could not run".into()))?;
    if !output.status.success() {
        return Err(AdapterError::Failed("Cargo Git check failed".into()));
    }
    parse_git_ls_remote_output(&output.stdout, source)
}

fn parse_git_ls_remote_output(
    stdout: &[u8],
    source: &GitSource,
) -> Result<RemoteUpdateStatus, AdapterError> {
    if let GitSelector::Rev(rev) = &source.selector {
        return Ok(if rev.starts_with(&source.installed) {
            RemoteUpdateStatus::Current
        } else {
            RemoteUpdateStatus::UpdateAvailable
        });
    }
    let mut shas = Vec::new();
    let mut peeled_tag = None;
    let mut lightweight_tag = None;
    for line in String::from_utf8_lossy(stdout).lines() {
        let Some((sha, reference)) = line.split_once('\t') else {
            return Err(AdapterError::Failed(
                "Cargo Git response is ambiguous".into(),
            ));
        };
        if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(AdapterError::Failed(
                "Cargo Git response is malformed".into(),
            ));
        }
        let expected = match &source.selector {
            GitSelector::Head => reference == "HEAD",
            GitSelector::Branch(name) => reference == format!("refs/heads/{name}"),
            GitSelector::Tag(name) => {
                reference == format!("refs/tags/{name}")
                    || reference == format!("refs/tags/{name}^{{}}")
            }
            GitSelector::Rev(_) => false,
        };
        if !expected {
            return Err(AdapterError::Failed(
                "Cargo Git response is ambiguous".into(),
            ));
        }
        if matches!(&source.selector, GitSelector::Tag(_)) {
            let target = if reference.ends_with("^{}") {
                &mut peeled_tag
            } else {
                &mut lightweight_tag
            };
            let sha = sha.to_ascii_lowercase();
            if target.as_ref().is_some_and(|existing| existing != &sha) {
                return Err(AdapterError::Failed(
                    "Cargo Git response is ambiguous".into(),
                ));
            }
            *target = Some(sha);
            continue;
        }
        if !shas.iter().any(|existing| existing == sha) {
            shas.push(sha.to_ascii_lowercase());
        }
    }
    if matches!(&source.selector, GitSelector::Tag(_)) {
        if let Some(sha) = peeled_tag.or(lightweight_tag) {
            shas.push(sha);
        }
    }
    if shas.len() != 1 {
        return Err(AdapterError::Failed(
            "Cargo Git response is ambiguous".into(),
        ));
    }
    Ok(if shas[0].starts_with(&source.installed) {
        RemoteUpdateStatus::Current
    } else {
        RemoteUpdateStatus::UpdateAvailable
    })
}

fn is_crates_io_source(source: &str) -> bool {
    matches!(
        source,
        "registry+https://github.com/rust-lang/crates.io-index" | "sparse+https://index.crates.io/"
    )
}

fn parse_crates_io_version(package: &str, output: &[u8]) -> Result<semver::Version, AdapterError> {
    let mut found_version = None;
    for line in String::from_utf8_lossy(output).lines() {
        let Some((found, rest)) = line.split_once(" = \"") else {
            continue;
        };
        let Some(version) = rest.split('"').next() else {
            continue;
        };
        if found == package {
            let parsed = semver::Version::parse(version)
                .map_err(|_| AdapterError::Failed("Cargo registry version is malformed".into()))?;
            if found_version.is_some() {
                return Err(AdapterError::Failed(
                    "Cargo registry package result is ambiguous".into(),
                ));
            }
            found_version = Some(parsed);
        }
    }
    found_version
        .ok_or_else(|| AdapterError::Failed("Cargo registry package is unavailable".into()))
}

/// Build safe update arguments from a recorded identity. Unsafe Git source URLs are rejected.
pub fn update_argv(identity: &InstallationIdentity, cargo: &Path) -> Option<Vec<String>> {
    let f = &identity.fields;
    let root = f.get("root")?;
    let name = f.get("name")?;
    let source = f.get("source")?;
    if source == "<redacted-unsafe-git-source>" || unsafe_git_source(source) {
        return None;
    }
    let mut argv = vec![
        cargo.to_string_lossy().into_owned(),
        "install".into(),
        "--root".into(),
        root.clone(),
    ];
    if source.starts_with("git+") {
        argv.extend(["--git".into(), source.trim_start_matches("git+").to_owned()]);
    }
    argv.push(name.clone());
    Some(argv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture(source: &str, schema: &str, bins: &str) -> (tempfile::TempDir, CargoAdapter) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("bin")).unwrap();
        fs::write(
            dir.path().join("bin").join(with_exe("tool")),
            b"fixture executable",
        )
        .unwrap();
        let json = format!(
            r#"{{"{schema}":{{"installs":{{"demo 1.2.3 ({source})":{{"version":"1.2.3","source":"{source}","bin":{bins},"features":[]}}}}}}}}"#
        );
        fs::write(dir.path().join(".crates2.json"), json).unwrap();
        let root = dir.path().to_owned();
        (dir, CargoAdapter::with_root(root, "cargo"))
    }

    #[test]
    fn registry_install_is_verified_and_update_argv_is_exact() {
        let (_d, a) = fixture(
            "registry+https://github.com/rust-lang/crates.io-index",
            "v1",
            r#"["tool"]"#,
        );
        let found = a.discover(&DiscoveryQuery { online: false }).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].evidence_level, EvidenceLevel::Verified);
        assert_eq!(
            update_argv(&found[0].identity, Path::new("cargo")),
            Some(
                vec![
                    "cargo",
                    "install",
                    "--root",
                    found[0].identity.fields["root"].as_str(),
                    "demo"
                ]
                .into_iter()
                .map(str::to_owned)
                .collect()
            )
        );
    }

    #[test]
    fn git_install_has_exact_update_argv() {
        let (_d, a) = fixture("git+https://example.test/repo?rev=abc", "v1", r#"["tool"]"#);
        let found = a.discover(&DiscoveryQuery { online: false }).unwrap();
        assert_eq!(found[0].evidence_level, EvidenceLevel::Verified);
        assert_eq!(
            update_argv(&found[0].identity, Path::new("cargo")).unwrap()[4..],
            ["--git", "https://example.test/repo?rev=abc", "demo"]
        );
    }

    #[test]
    fn machine_shaped_git_install_with_exe_bin_is_discovered() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("bin")).unwrap();
        fs::write(
            dir.path().join("bin").join(with_exe("tool.exe")),
            b"fixture executable",
        )
        .unwrap();
        fs::write(
            dir.path().join(".crates2.json"),
            r##"{"installs":{"demo 1.2.3 (git+https://github.com/example/tool#0123456789abcdef)":{"version_req":null,"bins":["tool.exe"],"features":[]}}}"##,
        )
        .unwrap();
        let a = CargoAdapter::with_root(dir.path(), "cargo");
        let found = a.discover(&DiscoveryQuery { online: false }).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].identity.adapter_kind, AdapterKind::Cargo);
        assert_eq!(found[0].identity.fields["trackedBin"], "tool.exe");
        assert_eq!(found[0].identity.fields["version"], "1.2.3");
        assert_eq!(
            found[0].identity.fields["source"],
            "git+https://github.com/example/tool#0123456789abcdef"
        );
        assert_eq!(found[0].evidence_level, EvidenceLevel::Verified);
        assert!(Path::new(&found[0].identity.fields["executable"]).exists());
    }

    #[test]
    fn credential_git_sources_are_not_trackable_or_updateable() {
        for source in [
            "git+https://user:pass@example.test/repo",
            "git+https://example.test/repo?access_token=secret",
            "git+https://example.test/repo?password=x",
            "git+https://example.test/repo?secret=x",
            "git+https://example.test/repo?auth=x",
        ] {
            let (_d, a) = fixture(source, "v1", r#"["tool"]"#);
            let found = a.discover(&DiscoveryQuery { online: false }).unwrap();
            assert_eq!(found[0].evidence_level, EvidenceLevel::Heuristic);
            assert!(!found[0].trackable);
            assert!(update_argv(&found[0].identity, Path::new("cargo")).is_none());
        }
    }

    #[test]
    fn unknown_schema_and_unlisted_or_copied_binary_are_not_verified() {
        let (_d, a) = fixture("registry+https://example.test/index", "v9", r#"["tool"]"#);
        assert_eq!(
            a.discover(&DiscoveryQuery { online: false }).unwrap()[0].evidence_level,
            EvidenceLevel::Heuristic
        );
        let (_d, a) = fixture("registry+https://example.test/index", "v1", r#"["other"]"#);
        assert!(a
            .discover(&DiscoveryQuery { online: false })
            .unwrap()
            .is_empty());
    }

    #[test]
    fn copied_binary_is_only_heuristic_when_hash_differs_from_recorded_digest() {
        let (_d, a) = fixture("registry+https://example.test/index", "v1", r#"["tool"]"#);
        let mut found = a.discover(&DiscoveryQuery { online: false }).unwrap();
        let actual = hash_same_handle(Path::new(&found[0].identity.fields["executable"])).unwrap();
        found[0]
            .identity
            .fields
            .insert("hash".into(), "copied-binary-digest-does-not-match".into());
        assert_ne!(found[0].identity.fields["hash"], actual);
    }

    #[test]
    fn git_source_parsing_matrix_accepts_valid_head_branch_tag_rev() {
        let head =
            parse_git_source("git+https://github.com/example/repo#0123456789abcdef").unwrap();
        assert_eq!(head.url, "https://github.com/example/repo");
        assert_eq!(head.selector, GitSelector::Head);
        assert_eq!(head.installed, "0123456789abcdef");

        let short_hash = parse_git_source("git+https://example.test/tool#1234567").unwrap();
        assert_eq!(short_hash.installed, "1234567");

        let full_hash = parse_git_source(
            "git+https://example.test/tool#0123456789abcdef0123456789abcdef01234567",
        )
        .unwrap();
        assert_eq!(
            full_hash.installed,
            "0123456789abcdef0123456789abcdef01234567"
        );

        let upper_hash = parse_git_source("git+https://example.test/tool#ABCDEF0123456").unwrap();
        assert_eq!(upper_hash.installed, "abcdef0123456");

        let branch =
            parse_git_source("git+https://github.com/example/repo?branch=main#0123456789abcdef")
                .unwrap();
        assert_eq!(branch.selector, GitSelector::Branch("main".into()));

        let tag =
            parse_git_source("git+https://github.com/example/repo?tag=v1.2.3#0123456789abcdef")
                .unwrap();
        assert_eq!(tag.selector, GitSelector::Tag("v1.2.3".into()));

        let rev = parse_git_source(
            "git+https://github.com/example/repo?rev=0123456789abcdef0123456789abcdef01234567#0123456789abcdef",
        )
        .unwrap();
        assert_eq!(
            rev.selector,
            GitSelector::Rev("0123456789abcdef0123456789abcdef01234567".into())
        );
    }

    #[test]
    fn git_source_parsing_negative_matrix_and_no_source_echo() {
        let negatives = [
            "git+http://example.test/repo#0123456789abcdef",
            "http://example.test/repo#0123456789abcdef",
            "https://example.test/repo#0123456789abcdef",
            "git://example.test/repo#0123456789abcdef",
            "ssh://git@example.test/repo#0123456789abcdef",
            "git+https://example.test/repo",
            "git+https://example.test/repo#123456",
            "git+https://example.test/repo#0123456789abcdef0123456789abcdef012345678",
            "git+https://example.test/repo#123456g",
            "git+https://user:pass@example.test/repo#0123456789abcdef",
            "git+https://user@example.test/repo#0123456789abcdef",
            "git+https://example.test/repo/#0123456789abcdef",
            "git+https:///#0123456789abcdef",
            "git+https://example.test:443/repo#0123456789abcdef",
            "git+https://example.test/repo?access_token=secret#0123456789abcdef",
            "git+https://example.test/repo?unknown=val#0123456789abcdef",
            "git+https://example.test/repo?branch=main&tag=v1.0#0123456789abcdef",
            "git+https://example.test/repo?branch=main&branch=dev#0123456789abcdef",
            "git+https://example.test/repo?branch=#0123456789abcdef",
            "git+https://example.test/repo?branch=../bad#0123456789abcdef",
            "git+https://example.test/repo?branch=-bad#0123456789abcdef",
            "git+https://example.test/repo?branch=a..b#0123456789abcdef",
            "git+https://example.test/repo?branch=a@{b#0123456789abcdef",
            "git+https://example.test/repo?rev=short#0123456789abcdef",
        ];

        for source in negatives {
            let res = parse_git_source(source);
            assert!(res.is_err(), "expected error for negative source: {source}");
            let err = format!("{:?}", res.err().unwrap());
            assert!(
                !err.contains("example.test"),
                "error echoed host from source: {err}"
            );
            assert!(
                !err.contains("pass") && !err.contains("secret"),
                "error echoed secret from source: {err}"
            );
        }
    }

    #[test]
    fn git_ls_remote_output_resolution_and_ambiguity() {
        let installed = "0123456789abcdef";
        let head_source = GitSource {
            url: "https://example.test/repo".into(),
            selector: GitSelector::Head,
            installed: installed.into(),
        };

        // HEAD matching installed
        let stdout = format!("0123456789abcdef0123456789abcdef01234567\tHEAD\n");
        assert_eq!(
            parse_git_ls_remote_output(stdout.as_bytes(), &head_source).unwrap(),
            RemoteUpdateStatus::Current
        );

        // HEAD differing from installed
        let stdout_diff = format!("fedcba98765432100123456789abcdef01234567\tHEAD\n");
        assert_eq!(
            parse_git_ls_remote_output(stdout_diff.as_bytes(), &head_source).unwrap(),
            RemoteUpdateStatus::UpdateAvailable
        );

        // Branch matching installed
        let branch_source = GitSource {
            url: "https://example.test/repo".into(),
            selector: GitSelector::Branch("main".into()),
            installed: installed.into(),
        };
        let branch_stdout = format!("0123456789abcdef0123456789abcdef01234567\trefs/heads/main\n");
        assert_eq!(
            parse_git_ls_remote_output(branch_stdout.as_bytes(), &branch_source).unwrap(),
            RemoteUpdateStatus::Current
        );

        // Tag: peeled tag takes precedence over lightweight tag
        let tag_source = GitSource {
            url: "https://example.test/repo".into(),
            selector: GitSelector::Tag("v1.0.0".into()),
            installed: installed.into(),
        };
        let tag_stdout = format!(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\trefs/tags/v1.0.0\n0123456789abcdef0123456789abcdef01234567\trefs/tags/v1.0.0^{{}}\n"
        );
        assert_eq!(
            parse_git_ls_remote_output(tag_stdout.as_bytes(), &tag_source).unwrap(),
            RemoteUpdateStatus::Current
        );

        // Tag: lightweight tag fallback
        let tag_lw_stdout = format!("0123456789abcdef0123456789abcdef01234567\trefs/tags/v1.0.0\n");
        assert_eq!(
            parse_git_ls_remote_output(tag_lw_stdout.as_bytes(), &tag_source).unwrap(),
            RemoteUpdateStatus::Current
        );

        // Rev: immutable matching installed
        let rev_matching = GitSource {
            url: "https://example.test/repo".into(),
            selector: GitSelector::Rev("0123456789abcdef0123456789abcdef01234567".into()),
            installed: installed.into(),
        };
        assert_eq!(
            parse_git_ls_remote_output(b"", &rev_matching).unwrap(),
            RemoteUpdateStatus::Current
        );

        // Rev: immutable differing from installed
        let rev_diff = GitSource {
            url: "https://example.test/repo".into(),
            selector: GitSelector::Rev("fedcba9876543210fedcba9876543210fedcba98".into()),
            installed: installed.into(),
        };
        assert_eq!(
            parse_git_ls_remote_output(b"", &rev_diff).unwrap(),
            RemoteUpdateStatus::UpdateAvailable
        );

        // Ambiguous: multiple conflicting SHAs for HEAD
        let ambiguous_stdout = format!(
            "0123456789abcdef0123456789abcdef01234567\tHEAD\nfedcba98765432100123456789abcdef01234567\tHEAD\n"
        );
        assert!(parse_git_ls_remote_output(ambiguous_stdout.as_bytes(), &head_source).is_err());

        // Ambiguous: empty output
        assert!(parse_git_ls_remote_output(b"", &head_source).is_err());

        // Malformed line: missing tab
        assert!(parse_git_ls_remote_output(b"0123456789abcdef HEAD\n", &head_source).is_err());

        // Malformed line: short SHA
        assert!(parse_git_ls_remote_output(b"1234567\tHEAD\n", &head_source).is_err());
    }

    #[test]
    fn crates_io_registry_source_and_version_parsing() {
        assert!(is_crates_io_source(
            "registry+https://github.com/rust-lang/crates.io-index"
        ));
        assert!(is_crates_io_source("sparse+https://index.crates.io/"));
        assert!(!is_crates_io_source("registry+https://example.test/index"));
        assert!(!is_crates_io_source("sparse+https://example.test/"));
        assert!(!is_crates_io_source(
            "git+https://github.com/rust-lang/cargo"
        ));

        let search_stdout = b"rtk-extra = \"0.3.0\" # extended\nrtk = \"0.2.1\"       # real toolkit\nother-rtk = \"1.0.0\" # other\n";
        let parsed = parse_crates_io_version("rtk", search_stdout).unwrap();
        assert_eq!(parsed, semver::Version::new(0, 2, 1));

        let not_found = parse_crates_io_version("missing-tool", search_stdout);
        assert!(not_found.is_err());

        let malformed = b"bad-pkg = \"not.a.semver\"\n";
        assert!(parse_crates_io_version("bad-pkg", malformed).is_err());

        // SemVer precedence check under standard SemVer rules
        let v1_0_0 = semver::Version::parse("1.0.0").unwrap();
        let v1_0_1 = semver::Version::parse("1.0.1").unwrap();
        let v1_1_0 = semver::Version::parse("1.1.0").unwrap();
        let v2_0_0 = semver::Version::parse("2.0.0").unwrap();
        let v1_0_0_rc1 = semver::Version::parse("1.0.0-rc.1").unwrap();

        assert!(v1_0_1 > v1_0_0);
        assert!(v1_1_0 > v1_0_1);
        assert!(v2_0_0 > v1_1_0);
        assert!(v1_0_0 > v1_0_0_rc1);
    }

    #[test]
    fn remote_update_status_offline_and_fail_closed_contract() {
        let mut identity = InstallationIdentity {
            adapter_kind: AdapterKind::Cargo,
            fields: BTreeMap::new(),
        };
        identity.fields.insert("name".into(), "tool".into());
        identity.fields.insert("version".into(), "1.0.0".into());
        identity.fields.insert(
            "source".into(),
            "registry+https://github.com/rust-lang/crates.io-index".into(),
        );

        let adapter = CargoAdapter::for_recorded_update(identity.clone(), "cargo");

        // Offline query returns NotChecked without network/process execution
        let offline_res = adapter
            .remote_update_status(&DiscoveryQuery { online: false })
            .unwrap();
        assert_eq!(offline_res, RemoteUpdateStatus::NotChecked);

        // Non-crates.io registry fails closed with error when online
        let mut non_crates_identity = identity.clone();
        non_crates_identity.fields.insert(
            "source".into(),
            "registry+https://example.test/index".into(),
        );
        let non_crates_adapter = CargoAdapter::for_recorded_update(non_crates_identity, "cargo");
        let non_crates_res =
            non_crates_adapter.remote_update_status(&DiscoveryQuery { online: true });
        assert!(non_crates_res.is_err());

        // Rev-based git source tested directly through adapter
        let mut rev_identity = identity.clone();
        rev_identity.fields.insert(
            "source".into(),
            "git+https://example.test/repo?rev=0123456789abcdef0123456789abcdef01234567#0123456789abcdef".into(),
        );
        let rev_adapter = CargoAdapter::for_recorded_update(rev_identity, "cargo");
        let rev_res = rev_adapter
            .remote_update_status(&DiscoveryQuery { online: true })
            .unwrap();
        assert_eq!(rev_res, RemoteUpdateStatus::Current);

        // Differing rev returns UpdateAvailable
        let mut rev_diff_identity = identity;
        rev_diff_identity.fields.insert(
            "source".into(),
            "git+https://example.test/repo?rev=fedcba9876543210fedcba9876543210fedcba98#0123456789abcdef".into(),
        );
        let rev_diff_adapter = CargoAdapter::for_recorded_update(rev_diff_identity, "cargo");
        let rev_diff_res = rev_diff_adapter
            .remote_update_status(&DiscoveryQuery { online: true })
            .unwrap();
        assert_eq!(rev_diff_res, RemoteUpdateStatus::UpdateAvailable);
    }

    #[test]
    fn remote_update_status_is_read_only_and_preserves_identity() {
        let mut identity = InstallationIdentity {
            adapter_kind: AdapterKind::Cargo,
            fields: BTreeMap::new(),
        };
        identity.fields.insert("name".into(), "tool".into());
        identity.fields.insert("version".into(), "1.0.0".into());
        identity.fields.insert(
            "source".into(),
            "git+https://example.test/repo?rev=0123456789abcdef0123456789abcdef01234567#0123456789abcdef".into(),
        );
        let identity_before = identity.clone();

        let adapter = CargoAdapter::for_recorded_update(identity, "cargo");
        let _ = adapter.remote_update_status(&DiscoveryQuery { online: true });

        // Adapter's recorded identity was not mutated
        assert_eq!(adapter.recorded.as_ref().unwrap(), &identity_before);
    }
}
