//! Read-only discovery from Windows uninstall registry entries.
use super::{AdapterCapabilities, AdapterError, CapabilityState, ToolAdapter};
use crate::tool_manager::{discovery::{DiscoveryCandidate, DiscoveryQuery}, model::{AdapterKind, EvidenceLevel, InstallationIdentity},};
use std::{collections::BTreeMap, path::{Path, PathBuf}};

#[derive(Debug, Clone)]
pub struct WingetOfflineAdapter { executable: PathBuf }
impl WingetOfflineAdapter { pub fn new(executable: impl Into<PathBuf>) -> Self { Self { executable: executable.into() } } }
impl Default for WingetOfflineAdapter { fn default() -> Self { Self::new(std::env::current_exe().unwrap_or_default()) } }

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UninstallEntry { pub display_name: Option<String>, pub display_version: Option<String>, pub publisher: Option<String>, pub install_location: Option<String> }

pub fn matches_executable(entry: &UninstallEntry, executable: &Path) -> bool {
    let Some(location) = entry.install_location.as_deref().filter(|s| !s.trim().is_empty()) else { return false; };
    let executable = executable.to_string_lossy();
    let location = location.trim_end_matches(['\\', '/']);
    executable.len() > location.len() && executable[..location.len()].eq_ignore_ascii_case(location)
        && executable.as_bytes().get(location.len()).is_some_and(|c| *c == b'\\' || *c == b'/')
}

#[cfg(windows)]
fn read_registry() -> Vec<UninstallEntry> {
    use windows_sys::Win32::{Foundation::ERROR_SUCCESS, System::Registry::*};
    let mut entries = Vec::new();
    let paths = [
        (HKEY_LOCAL_MACHINE, r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall"),
        (HKEY_LOCAL_MACHINE, r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall"),
        (HKEY_CURRENT_USER, r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall"),
        (HKEY_CURRENT_USER, r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall"),
    ];
    for (root, path) in paths {
        let mut key = std::ptr::null_mut();
        let path: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        unsafe { if RegOpenKeyExW(root, path.as_ptr(), 0, KEY_READ, &mut key) != ERROR_SUCCESS { continue; } }
        let mut index = 0;
        loop {
            let mut name = [0u16; 512]; let mut len = name.len() as u32;
            let rc = unsafe { RegEnumKeyExW(key, index, name.as_mut_ptr(), &mut len, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
            if rc != ERROR_SUCCESS { break; }
            index += 1;
            let sub: Vec<u16> = name[..len as usize].iter().copied().chain(Some(0)).collect();
            let mut child = std::ptr::null_mut();
            if unsafe { RegOpenKeyExW(key, sub.as_ptr(), 0, KEY_READ, &mut child) } != ERROR_SUCCESS { continue; }
            let read = |field: &str| -> Option<String> {
                let field: Vec<u16> = field.encode_utf16().chain(Some(0)).collect(); let mut ty = 0; let mut bytes = 0;
                unsafe { if RegQueryValueExW(child, field.as_ptr(), std::ptr::null_mut(), &mut ty, std::ptr::null_mut(), &mut bytes) != ERROR_SUCCESS || bytes < 2 { return None; } }
                let mut data = vec![0u16; (bytes / 2) as usize];
                unsafe { if RegQueryValueExW(child, field.as_ptr(), std::ptr::null_mut(), &mut ty, data.as_mut_ptr().cast(), &mut bytes) != ERROR_SUCCESS { return None; } }
                Some(String::from_utf16_lossy(&data).trim_end_matches('\0').to_string())
            };
            entries.push(UninstallEntry { display_name: read("DisplayName"), display_version: read("DisplayVersion"), publisher: read("Publisher"), install_location: read("InstallLocation") });
            unsafe { RegCloseKey(child); }
        }
        unsafe { RegCloseKey(key); }
    }
    entries
}

impl WingetOfflineAdapter {
    pub fn candidates_from_entries(&self, entries: &[UninstallEntry]) -> Vec<DiscoveryCandidate> {
        entries.iter().filter(|entry| matches_executable(entry, &self.executable)).map(|entry| {
            let mut fields = BTreeMap::new();
            fields.insert("executable".into(), self.executable.to_string_lossy().into_owned());
            for (key, value) in [("displayName", &entry.display_name), ("version", &entry.display_version), ("publisher", &entry.publisher), ("installLocation", &entry.install_location)] { if let Some(value) = value { fields.insert(key.into(), value.clone()); } }
            DiscoveryCandidate::new(InstallationIdentity { adapter_kind: AdapterKind::WingetOffline, fields }, EvidenceLevel::Heuristic, true, true, false)
        }).collect()
    }
}
impl ToolAdapter for WingetOfflineAdapter {
    fn kind(&self) -> AdapterKind { AdapterKind::WingetOffline }
    fn capabilities(&self) -> AdapterCapabilities { AdapterCapabilities::for_adapter(self.kind(), CapabilityState::Supported, CapabilityState::Supported, CapabilityState::Supported, CapabilityState::Unsupported) }
    fn discover(&self, _: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
        #[cfg(windows)] { Ok(self.candidates_from_entries(&read_registry())) }
        #[cfg(not(windows))] { Ok(vec![]) }
    }
    fn reprobe(&self, q: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> { self.discover(q) }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(location: Option<&str>) -> UninstallEntry { UninstallEntry { display_name: Some("Example".into()), display_version: Some("1.0".into()), publisher: Some("Vendor".into()), install_location: location.map(str::to_owned) } }
    #[test] fn matches_registry_entry_for_executable() { let a = WingetOfflineAdapter::new(r"C:\Apps\Demo\bin\demo.exe"); assert_eq!(a.candidates_from_entries(&[entry(Some(r"C:\Apps\Demo"))]).len(), 1); }
    #[test] fn rejects_unrelated_registry_entry() { let a = WingetOfflineAdapter::new(r"C:\Apps\Other\demo.exe"); assert!(a.candidates_from_entries(&[entry(Some(r"C:\Apps\Demo"))]).is_empty()); }
    #[test] fn rejects_missing_install_location() { let a = WingetOfflineAdapter::new(r"C:\Apps\Demo\demo.exe"); assert!(a.candidates_from_entries(&[entry(None)]).is_empty()); }
    #[cfg(windows)] #[test] fn reads_real_registry_without_error() { let _ = read_registry(); }
}
