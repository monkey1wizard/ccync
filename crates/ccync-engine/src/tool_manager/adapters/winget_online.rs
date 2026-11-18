//! Explicit, online-only WinGet export matching and takeover command construction.
use super::{AdapterCapabilities, AdapterError, CapabilityState, ToolAdapter};
use crate::tool_manager::{discovery::{DiscoveryCandidate, DiscoveryQuery}, model::{AdapterKind, EvidenceLevel, InstallationIdentity}};
use serde_json::Value;
use std::{collections::BTreeMap, fs, path::PathBuf, time::Duration};

#[derive(Debug, Clone)]
pub struct WingetOnlineAdapter { executable: PathBuf, recorded: Option<InstallationIdentity>, takeover: bool }
impl Default for WingetOnlineAdapter { fn default() -> Self { Self::new("winget") } }
impl WingetOnlineAdapter {
    pub fn new(executable: impl Into<PathBuf>) -> Self { Self { executable: executable.into(), recorded: None, takeover: false } }
    pub fn for_recorded_update(identity: InstallationIdentity, manager_takeover: Option<&Value>) -> Self {
        let executable = identity.fields.get("managerExecutable").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("winget"));
        let takeover = manager_takeover.is_some_and(|v| v.get("manager").and_then(Value::as_str) == Some("winget"));
        let mut identity = identity;
        if takeover { identity.fields.insert("managerTakeover".into(), "winget".into()); }
        Self { executable, recorded: Some(identity), takeover }
    }
    pub fn parse_export(&self, data: &[u8]) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
        let root: Value = serde_json::from_slice(data).map_err(|e| AdapterError::Failed(format!("invalid WinGet export JSON: {e}")))?;
        let mut out = Vec::new();
        for source in root.get("Sources").and_then(Value::as_array).into_iter().flatten() {
            let details = source.get("SourceDetails").unwrap_or(&Value::Null);
            let source_id = details.get("Name").or_else(|| details.get("Argument")).and_then(Value::as_str).unwrap_or("");
            for package in source.get("Packages").and_then(Value::as_array).into_iter().flatten() {
                let (Some(id), Some(version)) = (package.get("PackageIdentifier").and_then(Value::as_str), package.get("Version").and_then(Value::as_str)) else { continue; };
                let package_source = package.get("Source").and_then(Value::as_str).unwrap_or(source_id);
                let display = package.get("PackageName").or_else(|| package.get("Name")).and_then(Value::as_str);
                let Some(offline) = self.recorded.as_ref() else { continue; };
                let display_offline = offline.fields.get("displayName").map(String::as_str).unwrap_or("");
                let name_join = display.is_some_and(|n| n.eq_ignore_ascii_case(display_offline));
                let structured_join = package.get("InstallLocation").and_then(Value::as_str).is_some_and(|p| offline.fields.get("installLocation").is_some_and(|v| p.eq_ignore_ascii_case(v)))
                    || package.get("PackageFamilyName").and_then(Value::as_str).is_some_and(|f| offline.fields.get("packageFamilyName").is_some_and(|v| f == v));
                let Some(expected_source) = offline.fields.get("source") else { continue; };
                if id != offline.fields.get("packageId").map(String::as_str).unwrap_or("") || package_source != expected_source || (!name_join && !structured_join) { continue; }
                let evidence = if structured_join { EvidenceLevel::Manageable } else { EvidenceLevel::Heuristic };
                let mut fields = offline.fields.clone();
                fields.extend([("managerExecutable".into(), self.executable.to_string_lossy().into_owned()), ("packageId".into(), id.into()), ("source".into(), package_source.into()), ("version".into(), version.into()), ("exportJoinKey".into(), if structured_join { "installLocation/packageFamilyName" } else { "displayName" }.into()), ("exactOnlineRevalidation".into(), "true".into())]);
                if !structured_join { fields.insert("evidenceReason".into(), "export entry linked to offline uninstall entry by display name only".into()); }
                out.push(DiscoveryCandidate::new(InstallationIdentity { adapter_kind: AdapterKind::WingetOnline, fields }, evidence, true, true, false));
            }
        }
        Ok(out)
    }
    pub fn export_with_runner(&self, runner: &dyn crate::tool_manager::probe::ProbeRunner) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
        let temp = std::env::temp_dir().join(format!("ccync-winget-export-{}.json", uuid::Uuid::new_v4()));
        let _private_file = fs::OpenOptions::new().write(true).create_new(true).open(&temp).map_err(|e| AdapterError::Failed(e.to_string()))?;
        let args = vec!["export".into(), "--include-versions".into(), "--disable-interactivity".into(), "--output".into(), temp.to_string_lossy().into_owned()];
        let request = crate::tool_manager::probe::ProbeRequest { program: self.executable.clone(), argv: args.clone(), env: BTreeMap::new(), working_directory: std::env::current_dir().unwrap_or_default(), timeout: Duration::from_secs(60), output_byte_cap: 1024 * 1024 };
        let output = runner.run(&request);
        if output.exit_code != Some(0) || output.spawn_error.is_some() {
            let text = String::from_utf8_lossy(&output.stderr).to_string();
            let _ = fs::remove_file(&temp);
            if text.to_ascii_lowercase().contains("agreement") { return Err(AdapterError::Failed(format!("WinGet requires an agreement. Run: {} {}", self.executable.display(), args.join(" ")))); }
            return Err(AdapterError::Failed(format!("WinGet export failed: {text}")));
        }
        let result = fs::read(&temp).map_err(|e| AdapterError::Failed(e.to_string())).and_then(|bytes| self.parse_export(&bytes));
        let _ = fs::remove_file(temp);
        result
    }
    pub fn update_argv(&self) -> Option<Vec<String>> { if !self.takeover { return None; } update_argv(self.recorded.as_ref()?) }
}
pub fn update_argv(identity: &InstallationIdentity) -> Option<Vec<String>> {
    if identity.adapter_kind != AdapterKind::WingetOnline || identity.fields.get("exactOnlineRevalidation")? != "true" || identity.fields.get("managerTakeover")? != "winget" { return None; }
    Some(vec![identity.fields.get("managerExecutable")?.clone(), "upgrade".into(), "--id".into(), identity.fields.get("packageId")?.clone(), "--exact".into(), "--source".into(), identity.fields.get("source")?.clone(), "--disable-interactivity".into()])
}
impl ToolAdapter for WingetOnlineAdapter {
    fn kind(&self) -> AdapterKind { AdapterKind::WingetOnline }
    fn capabilities(&self) -> AdapterCapabilities { AdapterCapabilities::for_adapter(self.kind(), CapabilityState::Unsupported, CapabilityState::Supported, CapabilityState::Supported, CapabilityState::Supported) }
    fn discover(&self, q: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> { if !q.online { return Err(AdapterError::NotChecked); } self.export_with_runner(&crate::tool_manager::probe::SystemProbeRunner) }
    fn reprobe(&self, q: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> { self.discover(q) }
    fn remote_update_status(&self, q: &DiscoveryQuery) -> Result<super::RemoteUpdateStatus, AdapterError> { Ok(if q.online { super::RemoteUpdateStatus::Unsupported } else { super::RemoteUpdateStatus::NotChecked }) }
    fn update_argv(&self) -> Option<Vec<String>> { WingetOnlineAdapter::update_argv(self) }
}

#[cfg(test)] mod tests {
    use super::*;
    fn identity() -> InstallationIdentity { InstallationIdentity { adapter_kind: AdapterKind::WingetOffline, fields: [("displayName".into(), "Example Tool".into()), ("installLocation".into(), r"C:\Example".into()), ("packageId".into(), "Contoso.Example".into()), ("source".into(), "winget".into())].into() } }
    fn adapter() -> WingetOnlineAdapter { WingetOnlineAdapter { executable: "winget.exe".into(), recorded: Some(identity()), takeover: false } }
    fn export(source: &str, name: Option<&str>) -> Vec<u8> { format!(r#"{{"Sources":[{{"SourceDetails":{{"Name":"winget"}},"Packages":[{{"PackageIdentifier":"Contoso.Example","Version":"2.0","Source":"{source}"{}}}]}}]}}"#, name.map(|n| format!(",\"PackageName\":\"{n}\"")).unwrap_or_default()).into_bytes() }
    #[test] fn exact_export_match_joins_by_structured_install_location() { let mut a = adapter(); a.recorded.as_mut().unwrap().fields.insert("packageFamilyName".into(), "Contoso.Example_123".into()); let found = a.parse_export(include_bytes!("../../../tests/fixtures/tool_manager/winget/exact-match.json")).unwrap(); assert_eq!(found.len(), 1); assert_eq!(found[0].identity.fields["exportJoinKey"], "installLocation/packageFamilyName"); assert_eq!(found[0].evidence_level, EvidenceLevel::Manageable); }
    #[test] fn source_mismatch_is_rejected() { assert!(adapter().parse_export(include_bytes!("../../../tests/fixtures/tool_manager/winget/source-mismatch.json")).unwrap().is_empty()); }
    #[test] fn name_only_join_is_heuristic() { let found = adapter().parse_export(&export("winget", Some("Example Tool"))).unwrap(); assert_eq!(found[0].evidence_level, EvidenceLevel::Heuristic); }
    #[test] fn takeover_emits_exact_argv_and_without_takeover_none() { let mut i = identity(); i.adapter_kind = AdapterKind::WingetOnline; i.fields.extend([("managerExecutable".into(), "winget.exe".into()), ("exactOnlineRevalidation".into(), "true".into())]); let declaration = serde_json::json!({"manager":"winget"}); assert_eq!(WingetOnlineAdapter::for_recorded_update(i.clone(), Some(&declaration)).update_argv().unwrap(), ["winget.exe", "upgrade", "--id", "Contoso.Example", "--exact", "--source", "winget", "--disable-interactivity"]); assert!(WingetOnlineAdapter::for_recorded_update(i, None).update_argv().is_none()); }
    #[test] fn offline_query_makes_no_request() { assert!(matches!(adapter().discover(&DiscoveryQuery { online: false }), Err(AdapterError::NotChecked))); }
    #[test] fn agreement_failure_names_command_to_run() { let runner = crate::tool_manager::probe::FixtureProbeRunner::new([crate::tool_manager::probe::ProbeOutput { exit_code: Some(1), stderr: b"agreement required".to_vec(), ..Default::default() }]); let err = adapter().export_with_runner(&runner).unwrap_err(); assert!(format!("{err:?}").contains("WinGet requires an agreement")); assert_eq!(runner.requests().len(), 1); }
}
