//! Installation evidence captured at one probe boundary.
use super::{
    adapters::{AdapterError, ToolAdapter},
    discovery::DiscoveryQuery,
    file_identity::{hash_same_handle, FileIdentity},
    model::InstallationIdentity,
    probe::ProbeRunner,
};
use std::{fs::File, path::PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceTuple {
    pub adapter_fields: InstallationIdentity,
    pub resolved_target: PathBuf,
    pub file_identity: FileIdentity,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceDifference {
    pub fields: Vec<&'static str>,
}

impl EvidenceTuple {
    /// Reports every tuple component that differs.
    pub fn compare(&self, other: &Self) -> Result<(), EvidenceDifference> {
        let mut fields = Vec::new();
        if self.adapter_fields != other.adapter_fields { fields.push("adapter_fields"); }
        if self.resolved_target != other.resolved_target { fields.push("resolved_target"); }
        if self.file_identity != other.file_identity { fields.push("file_identity"); }
        if self.sha256 != other.sha256 { fields.push("sha256"); }
        if fields.is_empty() { Ok(()) } else { Err(EvidenceDifference { fields }) }
    }
}

/// Serializes installation evidence with the stable public payload shape.
pub fn evidence_to_json(value: &EvidenceTuple) -> serde_json::Value {
    #[cfg(unix)]
    let file_identity = serde_json::json!({
        "dev": value.file_identity.dev,
        "ino": value.file_identity.ino,
    });
    #[cfg(windows)]
    let file_identity = serde_json::json!({
        "volumeSerialNumber": value.file_identity.volume_serial_number,
        "fileIndex": value.file_identity.file_index,
    });

    serde_json::json!({
        "adapterFields": value.adapter_fields,
        "resolvedTarget": value.resolved_target,
        "fileIdentity": file_identity,
        "sha256": value.sha256,
    })
}

impl std::fmt::Display for EvidenceDifference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "installation evidence differs: {}", self.fields.join(", "))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("adapter re-probe failed: {0:?}")]
    Adapter(AdapterError),
    #[error("tracked identity was not present in adapter re-probe")]
    IdentityNotFound,
    #[error("adapter identity has no resolved executable target")]
    TargetMissing,
    #[error("executable target could not be resolved: {0}")]
    Target(std::io::Error),
    #[error("executable identity/hash failed: {0}")]
    Hash(String),
}

pub fn capture_evidence(
    adapter: &dyn ToolAdapter,
    runner: &dyn ProbeRunner,
    identity: &InstallationIdentity,
) -> Result<EvidenceTuple, CaptureError> {
    // Re-probe adapters own their typed, adapter-specific probe implementation.
    // The runner is retained in this seam for adapters that accept injected runners.
    let _runner = runner;
    let observations = adapter.reprobe(&DiscoveryQuery { online: false }).map_err(CaptureError::Adapter)?;
    let requested_path = target_path(identity).ok_or(CaptureError::TargetMissing)?;
    let requested_target = std::fs::canonicalize(&requested_path).map_err(CaptureError::Target)?;
    let candidate = observations.into_iter().find(|c| {
        c.identity.adapter_kind == identity.adapter_kind
            && target_path(&c.identity).and_then(|p| std::fs::canonicalize(p).ok()).as_ref() == Some(&requested_target)
    }).ok_or(CaptureError::IdentityNotFound)?;
    let path = target_path(&candidate.identity).ok_or(CaptureError::TargetMissing)?;
    let resolved_target = std::fs::canonicalize(&path).map_err(CaptureError::Target)?;
    let file = File::open(&resolved_target).map_err(CaptureError::Target)?;
    let file_identity = identity_of(&file).map_err(CaptureError::Target)?;
    let sha256 = hash_same_handle(&resolved_target).map_err(|e| CaptureError::Hash(e.to_string()))?;
    Ok(EvidenceTuple { adapter_fields: candidate.identity, resolved_target, file_identity, sha256 })
}

fn target_path(identity: &InstallationIdentity) -> Option<PathBuf> {
    ["resolvedTarget", "executable", "target", "path", "invokedPath"]
        .iter().find_map(|key| identity.fields.get(*key).map(PathBuf::from))
}

#[cfg(unix)]
fn identity_of(file: &File) -> std::io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let m = file.metadata()?;
    Ok(FileIdentity { dev: m.dev(), ino: m.ino() })
}

#[cfg(windows)]
fn identity_of(file: &File) -> std::io::Result<FileIdentity> {
    use std::{mem::MaybeUninit, os::windows::io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, info.as_mut_ptr()) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let i = unsafe { info.assume_init() };
    Ok(FileIdentity { volume_serial_number: i.dwVolumeSerialNumber, file_index: ((i.nFileIndexHigh as u64) << 32) | i.nFileIndexLow as u64 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    #[test]
    fn tuple_compare_names_each_changed_field_and_accepts_equality() {
        let id = InstallationIdentity { adapter_kind: super::super::model::AdapterKind::ManualPath, fields: Default::default() };
        let a = EvidenceTuple { adapter_fields: id.clone(), resolved_target: Path::new("a").into(), file_identity: test_identity(), sha256: "a".into() };
        assert_eq!(a.compare(&a), Ok(()));
        let mut b = a.clone(); b.sha256 = "b".into(); b.resolved_target = Path::new("b").into();
        assert_eq!(a.compare(&b).unwrap_err().fields, ["resolved_target", "sha256"]);
    }

    #[test]
    fn evidence_json_has_stable_order_and_preserves_public_payload() {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert("zeta".into(), "last".into());
        fields.insert("alpha".into(), "first".into());
        let value = EvidenceTuple {
            adapter_fields: InstallationIdentity {
                adapter_kind: super::super::model::AdapterKind::ManualPath,
                fields,
            },
            resolved_target: Path::new("tool").into(),
            file_identity: test_identity(),
            sha256: "digest".into(),
        };

        let encoded = serde_json::to_string(&evidence_to_json(&value)).unwrap();
        assert_eq!(encoded, serde_json::to_string(&evidence_to_json(&value)).unwrap());
        let payload: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        #[cfg(unix)]
        assert_eq!(
            payload,
            serde_json::from_str::<serde_json::Value>(r#"{"adapterFields":{"adapterKind":"manual-path","fields":{"alpha":"first","zeta":"last"}},"fileIdentity":{"dev":1,"ino":1},"resolvedTarget":"tool","sha256":"digest"}"#).unwrap()
        );
        #[cfg(windows)]
        assert_eq!(
            payload,
            serde_json::from_str::<serde_json::Value>(r#"{"adapterFields":{"adapterKind":"manual-path","fields":{"alpha":"first","zeta":"last"}},"fileIdentity":{"fileIndex":1,"volumeSerialNumber":1},"resolvedTarget":"tool","sha256":"digest"}"#).unwrap()
        );
    }
    #[test]
    fn same_version_byte_replacement_changes_hash_and_same_path_replacement_changes_identity() {
        use crate::tool_manager::{adapters::{manual_path::ManualPathAdapter, ToolAdapter}, model::{AdapterKind, DeliveryId}, probe::FixtureProbeRunner};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        std::fs::write(&path, b"first bytes").unwrap();
        let adapter = ManualPathAdapter::new(&path, DeliveryId::RtkPath);
        let runner = FixtureProbeRunner::new([]);
        let first_identity = adapter.discover(&DiscoveryQuery { online: false }).unwrap()[0].identity.clone();
        let first = capture_evidence(&adapter, &runner, &first_identity).unwrap();
        std::fs::write(&path, b"other bytes").unwrap();
        let second_identity = adapter.discover(&DiscoveryQuery { online: false }).unwrap()[0].identity.clone();
        let second = capture_evidence(&adapter, &runner, &first_identity).unwrap();
        assert_eq!(first.resolved_target, second.resolved_target);
        assert_ne!(first.sha256, second.sha256);
        assert_eq!(first.file_identity, second.file_identity);
        assert!(first.compare(&second).unwrap_err().fields.contains(&"sha256"));

        let replacement = dir.path().join("replacement");
        std::fs::write(&replacement, b"other bytes").unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let third = capture_evidence(&adapter, &runner, &second_identity).unwrap();
        assert_ne!(second.file_identity, third.file_identity);
        assert!(second.compare(&third).unwrap_err().fields.contains(&"file_identity"));
        assert_eq!(first_identity.adapter_kind, AdapterKind::ManualPath);
    }
    #[cfg(unix)] fn test_identity() -> FileIdentity { FileIdentity { dev: 1, ino: 1 } }
    #[cfg(windows)] fn test_identity() -> FileIdentity { FileIdentity { volume_serial_number: 1, file_index: 1 } }
}
