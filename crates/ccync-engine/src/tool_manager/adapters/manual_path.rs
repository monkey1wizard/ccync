//! Read-only observation of a manually selected executable path.
use super::{AdapterCapabilities, AdapterError, CapabilityState, ToolAdapter};
use crate::tool_manager::{
    discovery::{DiscoveryCandidate, DiscoveryQuery},
    model::{AdapterKind, DeliveryId, EvidenceLevel, InstallationIdentity},
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

const MAX_LINK_HOPS: usize = 8;

#[derive(Debug, Clone)]
pub struct ManualPathAdapter {
    invoked: PathBuf,
    delivery: DeliveryId,
}

impl ManualPathAdapter {
    pub fn new(path: impl Into<PathBuf>, delivery: DeliveryId) -> Self {
        Self {
            invoked: path.into(),
            delivery,
        }
    }

    fn candidate(&self) -> Result<Option<DiscoveryCandidate>, AdapterError> {
        let Some(invoked) = resolve_invoked_path(&self.invoked) else {
            return Ok(None);
        };
        let (target, links) = match resolve_links(&invoked) {
            Ok(resolved) => resolved,
            Err(_) => return Ok(None),
        };
        let (identity, hash) = match hash_stable_file(&target) {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };
        let mut fields = BTreeMap::new();
        fields.insert("invokedPath".into(), invoked.to_string_lossy().into_owned());
        fields.insert(
            "linkChain".into(),
            serde_json::to_string(&links).unwrap_or_else(|_| "[]".into()),
        );
        fields.insert("fileIdentity".into(), identity);
        fields.insert("sha256".into(), hash);
        let installation = InstallationIdentity {
            adapter_kind: AdapterKind::ManualPath,
            fields,
        };
        Ok(Some(DiscoveryCandidate::new(
            installation,
            EvidenceLevel::Heuristic,
            true,
            false,
            true,
        )))
    }
}

impl ToolAdapter for ManualPathAdapter {
    fn kind(&self) -> AdapterKind {
        AdapterKind::ManualPath
    }

    fn capabilities(&self) -> AdapterCapabilities {
        let enabled = matches!(
            self.delivery,
            DeliveryId::GraphifyAgents | DeliveryId::RtkPath
        );
        let declared = if enabled {
            CapabilityState::Supported
        } else {
            CapabilityState::Unsupported
        };
        AdapterCapabilities::for_adapter(
            self.kind(),
            CapabilityState::Supported,
            CapabilityState::Supported,
            declared,
            CapabilityState::Unsupported,
        )
    }

    fn discover(&self, _query: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
        Ok(self.candidate()?.into_iter().collect())
    }

    fn reprobe(&self, query: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
        self.discover(query)
    }
}

fn resolve_invoked_path(path: &Path) -> Option<PathBuf> {
    resolve_invoked_path_in(path, &env::var_os("PATH")?)
}

fn resolve_invoked_path_in(path: &Path, path_env: &std::ffi::OsStr) -> Option<PathBuf> {
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    let mut names = vec![path.to_path_buf()];
    #[cfg(windows)]
    if path.extension().is_none() {
        let extensions = env::var_os("PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
        names.extend(
            extensions
                .to_string_lossy()
                .split(';')
                .filter(|e| !e.is_empty())
                .map(|e| path.with_extension(e.trim_start_matches('.'))),
        );
    }
    env::split_paths(path_env)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

fn resolve_links(start: &Path) -> Result<(PathBuf, Vec<(PathBuf, PathBuf)>), std::io::Error> {
    let mut current = absolute_lexical(start)?;
    let mut seen = HashSet::new();
    let mut links = Vec::new();
    loop {
        if !seen.insert(current.clone()) {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        let metadata = fs::symlink_metadata(&current)?;
        #[cfg(windows)]
        let is_link = {
            use std::os::windows::fs::MetadataExt;
            metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
        };
        #[cfg(not(windows))]
        let is_link = metadata.file_type().is_symlink();
        if !is_link {
            let target = fs::canonicalize(&current)?;
            if !fs::metadata(&target)?.is_file() {
                return Err(std::io::ErrorKind::InvalidInput.into());
            }
            return Ok((target, links));
        }
        if links.len() >= MAX_LINK_HOPS {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        let raw = fs::read_link(&current)?;
        let target = if raw.is_absolute() {
            raw
        } else {
            current.parent().unwrap_or(Path::new(".")).join(raw)
        };
        let target = absolute_lexical(&target)?;
        links.push((current, target.clone()));
        current = target;
    }
}

fn absolute_lexical(path: &Path) -> Result<PathBuf, std::io::Error> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    Ok(out)
}

fn handle_identity(file: &fs::File) -> Result<String, std::io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        Ok(format!("{}:{}", metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        use std::mem::MaybeUninit;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
        let ok =
            unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, info.as_mut_ptr()) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let info = unsafe { info.assume_init() };
        Ok(format!(
            "{}:{}",
            info.dwVolumeSerialNumber,
            ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64
        ))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let metadata = file.metadata()?;
        Ok(format!("{}:{:?}", metadata.len(), metadata.modified()?))
    }
}

fn hash_stable_file(path: &Path) -> Result<(String, String), std::io::Error> {
    hash_stable_file_with(path, || {})
}

fn hash_stable_file_with(
    path: &Path,
    between_reads: impl FnOnce(),
) -> Result<(String, String), std::io::Error> {
    let mut file = fs::File::open(path)?;
    let before = handle_identity(&file)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let first = hash_pass(&mut file)?;
    between_reads();
    file.seek(SeekFrom::Start(0))?;
    let second = hash_pass(&mut file)?;
    if first != second || before != handle_identity(&file)? || !file.metadata()?.is_file() {
        return Err(std::io::ErrorKind::Other.into());
    }
    Ok((before, first))
}

fn hash_pass(file: &mut fs::File) -> Result<String, std::io::Error> {
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(hex::encode(digest.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_manager::probe::{FixtureProbeRunner, ProbeOutput, ProbeRequest, ProbeRunner};
    use std::{
        fs,
        sync::atomic::{AtomicUsize, Ordering},
    };

    fn query() -> DiscoveryQuery {
        DiscoveryQuery { online: false }
    }
    fn file(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, b"manual executable").unwrap();
        path
    }

    #[test]
    fn observes_absolute_path_without_runner_requests() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path(), "tool");
        let runner = FixtureProbeRunner::new([]);
        let candidates = ManualPathAdapter::new(&path, DeliveryId::ObserveOnly)
            .discover(&query())
            .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].evidence_level, EvidenceLevel::Heuristic);
        assert_eq!(
            candidates[0].identity.fields["invokedPath"],
            path.to_string_lossy()
        );
        assert_eq!(runner.requests().len(), 0);
    }

    #[test]
    fn resolves_command_from_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path(), "tool");
        let found = resolve_invoked_path_in(Path::new("tool"), dir.path().as_os_str());
        assert_eq!(found, Some(path));
    }

    #[test]
    fn records_link_chain() {
        let dir = tempfile::tempdir().unwrap();
        let target = file(dir.path(), "target");
        let link = dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_file(&target, &link).is_err() {
            return;
        }
        let found = ManualPathAdapter::new(&link, DeliveryId::ObserveOnly)
            .discover(&query())
            .unwrap();
        assert_eq!(found.len(), 1);
        let chain: Vec<(PathBuf, PathBuf)> =
            serde_json::from_str(&found[0].identity.fields["linkChain"]).unwrap();
        assert_eq!(chain.len(), 1);
    }

    #[test]
    fn rejects_broken_links() {
        let dir = tempfile::tempdir().unwrap();
        let directory = dir.path().join("directory");
        fs::create_dir(&directory).unwrap();
        let broken = dir.path().join("broken");
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("missing"), &broken).unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_file(dir.path().join("missing"), &broken).is_err() {
            return;
        }
        assert!(resolve_links(&broken).is_err());
    }

    #[test]
    fn rejects_directories() {
        let dir = tempfile::tempdir().unwrap();
        let directory = dir.path().join("directory");
        fs::create_dir(&directory).unwrap();
        assert!(resolve_links(&directory).is_err());
    }

    #[test]
    fn changed_bytes_between_same_handle_reads_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path(), "mutable");
        let changed =
            hash_stable_file_with(&path, || fs::write(&path, b"different bytes").unwrap());
        assert!(changed.is_err());
    }

    #[test]
    fn observe_only_denies_mutating_capabilities_and_probe_runner_is_unused() {
        struct CountingRunner(AtomicUsize);
        impl ProbeRunner for CountingRunner {
            fn run(&self, _: &ProbeRequest) -> ProbeOutput {
                self.0.fetch_add(1, Ordering::SeqCst);
                ProbeOutput::default()
            }
        }
        let runner = CountingRunner(AtomicUsize::new(0));
        let adapter = ManualPathAdapter::new("missing-tool", DeliveryId::ObserveOnly);
        let caps = adapter.capabilities();
        assert_eq!(caps.local_enumeration, CapabilityState::Supported);
        assert_eq!(caps.executable_mapping, CapabilityState::Supported);
        assert_eq!(caps.installed_version_lookup, CapabilityState::Unsupported);
        assert_eq!(caps.remote_discovery, CapabilityState::Unsupported);
        assert_eq!(caps.package_update, CapabilityState::Gated);
        let _ = &runner;
        assert_eq!(runner.0.load(Ordering::SeqCst), 0);
    }
}
