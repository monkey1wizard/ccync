//! Read-only verification of the first-party ccync installation receipt.
use super::{AdapterCapabilities, AdapterError, CapabilityState, ToolAdapter};
use crate::tool_manager::{
    discovery::{DiscoveryCandidate, DiscoveryQuery},
    model::{AdapterKind, EvidenceLevel, InstallationIdentity},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Receipt {
    schema_version: u32,
    version: String,
    target: PathBuf,
    release_source: String,
    sha256: String,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CcyncReceiptAdapter;

impl CcyncReceiptAdapter {
    pub fn new() -> Self {
        Self
    }

    fn candidate(&self) -> Option<DiscoveryCandidate> {
        let path = ccync_foundation::paths::install_receipt_path()?;
        let receipt = read_receipt(&path).ok();
        let verified = receipt.as_ref().is_some_and(verify_receipt);
        let mut fields = BTreeMap::new();
        if let Some(receipt) = receipt {
            fields.insert(
                "target".into(),
                receipt.target.to_string_lossy().into_owned(),
            );
            fields.insert("version".into(), receipt.version);
            fields.insert("releaseSource".into(), receipt.release_source);
            fields.insert("sha256".into(), receipt.sha256);
        }
        Some(DiscoveryCandidate::new(
            InstallationIdentity {
                adapter_kind: AdapterKind::CcyncReceipt,
                fields,
            },
            if verified {
                EvidenceLevel::Verified
            } else {
                EvidenceLevel::Unknown
            },
            verified,
            false,
            true,
        ))
    }
}

impl ToolAdapter for CcyncReceiptAdapter {
    fn kind(&self) -> AdapterKind {
        AdapterKind::CcyncReceipt
    }
    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities::for_adapter(
            self.kind(),
            CapabilityState::Supported,
            CapabilityState::Supported,
            CapabilityState::Supported,
            CapabilityState::Unsupported,
        )
    }
    fn discover(&self, _: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
        Ok(self.candidate().into_iter().collect())
    }
    fn reprobe(&self, q: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
        self.discover(q)
    }
}

fn read_receipt(path: &Path) -> Result<Receipt, std::io::Error> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() || is_reparse(&meta) || !safe_receipt_owner(path, &meta) {
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    let mut file = fs::File::open(path)?;
    let handle_meta = file.metadata()?;
    if !handle_meta.is_file() || !safe_receipt_owner(path, &handle_meta) {
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(|_| std::io::ErrorKind::InvalidData.into())
}

fn verify_receipt(receipt: &Receipt) -> bool {
    let Some(exe) = std::env::current_exe()
        .ok()
        .and_then(|p| fs::canonicalize(p).ok())
    else {
        return false;
    };
    verify_receipt_at(receipt, &exe)
}

fn verify_receipt_at(receipt: &Receipt, exe: &Path) -> bool {
    if receipt.schema_version != 1
        || receipt.version.is_empty()
        || receipt.release_source.is_empty()
        || receipt.sha256.len() != 64
        || !receipt.sha256.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return false;
    }
    let Some(target) = fs::canonicalize(&receipt.target).ok() else {
        return false;
    };
    if fs::canonicalize(exe).ok().as_deref() != Some(target.as_path()) {
        return false;
    }
    let Ok(meta) = fs::symlink_metadata(&receipt.target) else {
        return false;
    };
    if !meta.file_type().is_file() || is_reparse(&meta) {
        return false;
    }
    hash_open_file(&receipt.target).is_ok_and(|hash| hash.eq_ignore_ascii_case(&receipt.sha256))
}

fn hash_open_file(path: &Path) -> Result<String, std::io::Error> {
    let mut file = fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let mut hash = Sha256::new();
    let mut buf = [0; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(hex::encode(hash.finalize()))
}

#[cfg(unix)]
fn safe_receipt_owner(_: &Path, meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(home) = ccync_foundation::paths::ccync_home() else {
        return false;
    };
    fs::metadata(home).is_ok_and(|m| m.uid() == meta.uid()) && meta.mode() & 0o022 == 0
}
#[cfg(windows)]
fn safe_receipt_owner(path: &Path, _: &fs::Metadata) -> bool {
    let Some(owner_class) = receipt_owner_class(path) else {
        return false;
    };
    let is_administrators_member = administrators_membership().unwrap_or(false);
    receipt_owner_is_accepted(owner_class, is_administrators_member)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReceiptOwnerClass {
    User,
    Administrators,
    Foreign,
}

#[cfg(windows)]
fn administrators_membership() -> Result<bool, ()> {
    use windows_sys::Win32::Security::{
        CheckTokenMembership, CreateWellKnownSid, WinBuiltinAdministratorsSid,
    };
    unsafe {
        let mut administrators_sid = [0u8; 68];
        let mut administrators_sid_len = administrators_sid.len() as u32;
        if CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            std::ptr::null_mut(),
            administrators_sid.as_mut_ptr().cast(),
            &mut administrators_sid_len,
        ) == 0
        {
            return Err(());
        }
        let mut is_member = 0;
        if CheckTokenMembership(
            std::ptr::null_mut(),
            administrators_sid.as_mut_ptr().cast(),
            &mut is_member,
        ) == 0
        {
            return Err(());
        }
        Ok(is_member != 0)
    }
}

#[cfg(windows)]
fn receipt_owner_class(path: &Path) -> Option<ReceiptOwnerClass> {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt};
    use windows_sys::Win32::Security::{
        CreateWellKnownSid, EqualSid, WinBuiltinAdministratorsSid, OWNER_SECURITY_INFORMATION,
    };
    use windows_sys::Win32::{
        Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT},
        Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER},
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    unsafe {
        let mut name: Vec<u16> = OsStr::new(path).encode_wide().chain(Some(0)).collect();
        let mut owner = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        if GetNamedSecurityInfoW(
            name.as_mut_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        ) != 0
        {
            return None;
        }
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            windows_sys::Win32::Foundation::LocalFree(descriptor);
            return None;
        }
        let mut needed = 0;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        let mut buffer = vec![0u8; needed as usize];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        ) != 0;
        let user_sid = if ok {
            (*(buffer.as_ptr() as *const TOKEN_USER)).User.Sid
        } else {
            std::ptr::null_mut()
        };
        let mut administrators_sid = [0u8; 68];
        let mut administrators_sid_len = administrators_sid.len() as u32;
        let admin_ok = CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            std::ptr::null_mut(),
            administrators_sid.as_mut_ptr().cast(),
            &mut administrators_sid_len,
        ) != 0;
        let class = if ok && EqualSid(owner, user_sid) != 0 {
            ReceiptOwnerClass::User
        } else if admin_ok && EqualSid(owner, administrators_sid.as_mut_ptr().cast()) != 0 {
            ReceiptOwnerClass::Administrators
        } else {
            ReceiptOwnerClass::Foreign
        };
        windows_sys::Win32::Foundation::CloseHandle(token);
        windows_sys::Win32::Foundation::LocalFree(descriptor);
        Some(class)
    }
}

#[cfg(windows)]
fn observed_receipt_owner_class(path: &Path) -> &'static str {
    match receipt_owner_class(path) {
        Some(ReceiptOwnerClass::User) => "user",
        Some(ReceiptOwnerClass::Administrators) => "Administrators",
        Some(ReceiptOwnerClass::Foreign) => "foreign",
        None => "unavailable",
    }
}

fn receipt_owner_is_accepted(owner: ReceiptOwnerClass, is_administrators_member: bool) -> bool {
    match owner {
        ReceiptOwnerClass::User => true,
        ReceiptOwnerClass::Administrators => is_administrators_member,
        ReceiptOwnerClass::Foreign => false,
    }
}
#[cfg(not(any(unix, windows)))]
fn safe_receipt_owner(_: &Path, _: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn is_reparse(meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}
#[cfg(windows)]
fn is_reparse(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_type().is_symlink() || meta.file_attributes() & 0x400 != 0
}
#[cfg(not(any(unix, windows)))]
fn is_reparse(_: &fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    fn receipt(target: &Path, version: u32, hash: &str) -> Receipt {
        Receipt {
            schema_version: version,
            version: "1.2.3".into(),
            target: target.into(),
            release_source: "github-release".into(),
            sha256: hash.into(),
        }
    }
    fn hash(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    #[test]
    fn verifies_matching_target_hash_and_rejects_mismatch_or_relocation() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("ccync");
        fs::write(&target, b"binary").unwrap();
        let good = receipt(&target, 1, &hash(b"binary"));
        assert!(verify_receipt_at(&good, &target));
        assert!(!verify_receipt_at(
            &receipt(&target, 1, &hash(b"other")),
            &target
        ));
        assert!(!verify_receipt_at(&good, &dir.path().join("relocated")));
    }

    #[test]
    fn rejects_malformed_and_unsupported_receipts() {
        assert!(serde_json::from_str::<Receipt>("{").is_err());
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("ccync");
        fs::write(&target, b"binary").unwrap();
        assert!(!verify_receipt_at(
            &receipt(&target, 2, &hash(b"binary")),
            &target
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_group_or_other_writable_receipt() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".ccync");
        fs::create_dir_all(dir.join("install-receipts")).unwrap();
        let receipt_path = dir.join("install-receipts/ccync.json");
        fs::write(&receipt_path, b"{}").unwrap();
        fs::set_permissions(&receipt_path, fs::Permissions::from_mode(0o666)).unwrap();
        // Owner check follows the effective ~/.ccync path; point it at this fixture.
        let prior = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", home.path());
        }
        assert!(read_receipt(&receipt_path).is_err());
        if let Some(value) = prior {
            unsafe {
                std::env::set_var("HOME", value);
            }
        }
    }
    #[cfg(windows)]
    #[test]
    fn verifies_owner_sid_on_windows_and_rejects_unsafe_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("ccync.exe");
        fs::write(&target, b"binary").unwrap();
        let receipt_path = dir.path().join("ccync.json");
        fs::write(&receipt_path, b"{}").unwrap();
        let meta = fs::metadata(&receipt_path).unwrap();
        let owner_class = observed_receipt_owner_class(&receipt_path);
        assert!(
            safe_receipt_owner(&receipt_path, &meta),
            "receipt owner class was {owner_class}"
        );
        assert!(!safe_receipt_owner(&dir.path().join("nonexistent"), &meta));
        assert!(read_receipt(dir.path()).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn administrators_membership_check_succeeds_for_current_process() {
        assert!(administrators_membership().is_ok());
    }

    #[test]
    fn receipt_owner_decision_accepts_only_user_or_member_administrators() {
        assert!(receipt_owner_is_accepted(ReceiptOwnerClass::User, false));
        assert!(receipt_owner_is_accepted(
            ReceiptOwnerClass::Administrators,
            true
        ));
        assert!(!receipt_owner_is_accepted(
            ReceiptOwnerClass::Administrators,
            false
        ));
        assert!(!receipt_owner_is_accepted(ReceiptOwnerClass::Foreign, true));
    }
}
