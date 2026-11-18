use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{collections::BTreeMap, fmt};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ToolId(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid tool id: {0}")]
pub struct InvalidToolId(String);

impl ToolId {
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidToolId> {
        let value = value.into();
        if crate::install::is_valid_plugin_id(&value) {
            Ok(Self(value))
        } else {
            Err(InvalidToolId(value))
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl Serialize for ToolId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for ToolId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

macro_rules! status_enum {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "kebab-case")]
        pub enum $name { $($variant),+ }
    };
}

status_enum!(EvidenceLevel { Verified => "verified", Manageable => "manageable", Heuristic => "heuristic", Unknown => "unknown" });
status_enum!(SelectionState { Unique => "unique", Ambiguous => "ambiguous", None => "none" });
status_enum!(ManagerAvailability { Available => "available", Unavailable => "unavailable", NotApplicable => "not-applicable", NotChecked => "not-checked" });
status_enum!(InstallState { Installed => "installed", Missing => "missing", IdentityMismatch => "identity-mismatch", VersionUnresolved => "version-unresolved", NotChecked => "not-checked" });
status_enum!(PackageState { MatchesArtifactSource => "matches-artifact-source", ExternallyChanged => "externally-changed", Missing => "missing", VersionUnresolved => "version-unresolved", NotChecked => "not-checked" });
status_enum!(UpdateState { UpdateAvailable => "update-available", Current => "current", Unsupported => "unsupported", NotChecked => "not-checked", CheckFailed => "check-failed" });
status_enum!(ArtifactState { Current => "current", ArtifactDrifted => "artifact-drifted", Missing => "missing", Unreadable => "unreadable", NotApplicable => "not-applicable", NotChecked => "not-checked" });
status_enum!(ProjectionState { Current => "current", Drifted => "drifted", Missing => "missing", NotApplicable => "not-applicable", NotChecked => "not-checked" });
status_enum!(HoldPolicy { Held => "held", Unheld => "unheld" });
status_enum!(ManagerPinState { Pinned => "pinned", Unpinned => "unpinned", Unsupported => "unsupported", Unknown => "unknown" });
status_enum!(OperationState { None => "none", Prepared => "prepared", PackageVerified => "package-verified", ArtifactValidated => "artifact-validated", ProjectionSucceeded => "projection-succeeded", Committed => "committed", RecoveryRequired => "recovery-required" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdapterKind {
    Cargo,
    UvTool,
    Pipx,
    InterpreterPip,
    NpmGlobal,
    PnpmGlobal,
    BrewFormula,
    BrewCask,
    WingetOffline,
    WingetOnline,
    CcyncReceipt,
    ManualPath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeliveryId {
    GraphifyAgents,
    RtkPath,
    ObserveOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallationIdentity {
    pub adapter_kind: AdapterKind,
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub candidate_id: String,
    pub identity: InstallationIdentity,
    pub evidence_level: EvidenceLevel,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDeclaration {
    pub id: ToolId,
    pub identity: InstallationIdentity,
    pub delivery_id: DeliveryId,
    pub hold_policy: HoldPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limited_evidence_acceptance: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_baseline: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager_takeover: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolFacets {
    pub evidence_level: EvidenceLevel,
    pub selection_state: SelectionState,
    pub manager_availability: ManagerAvailability,
    pub install_state: InstallState,
    pub package_state: PackageState,
    pub update_state: UpdateState,
    pub artifact_state: ArtifactState,
    pub projection_state: ProjectionState,
    pub hold_policy: HoldPolicy,
    pub manager_pin_state: ManagerPinState,
    pub operation_state: OperationState,
    pub last_check: Option<String>,
}

pub type IdentityFields = BTreeMap<String, String>;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn status_enum_variants_use_canonical_kebab_case() {
        macro_rules! check { ($ty:ty, [$($value:expr => $literal:literal),+ $(,)?]) => {$({
            assert_eq!(serde_json::to_string(&$value).unwrap(), concat!("\"", $literal, "\""));
        })+}; }
        check!(EvidenceLevel, [EvidenceLevel::Verified => "verified", EvidenceLevel::Manageable => "manageable", EvidenceLevel::Heuristic => "heuristic", EvidenceLevel::Unknown => "unknown"]);
        check!(SelectionState, [SelectionState::Unique => "unique", SelectionState::Ambiguous => "ambiguous", SelectionState::None => "none"]);
        check!(ManagerAvailability, [ManagerAvailability::Available => "available", ManagerAvailability::Unavailable => "unavailable", ManagerAvailability::NotApplicable => "not-applicable", ManagerAvailability::NotChecked => "not-checked"]);
        check!(InstallState, [InstallState::Installed => "installed", InstallState::Missing => "missing", InstallState::IdentityMismatch => "identity-mismatch", InstallState::VersionUnresolved => "version-unresolved", InstallState::NotChecked => "not-checked"]);
        check!(PackageState, [PackageState::MatchesArtifactSource => "matches-artifact-source", PackageState::ExternallyChanged => "externally-changed", PackageState::Missing => "missing", PackageState::VersionUnresolved => "version-unresolved", PackageState::NotChecked => "not-checked"]);
        check!(UpdateState, [UpdateState::UpdateAvailable => "update-available", UpdateState::Current => "current", UpdateState::Unsupported => "unsupported", UpdateState::NotChecked => "not-checked", UpdateState::CheckFailed => "check-failed"]);
        check!(ArtifactState, [ArtifactState::Current => "current", ArtifactState::ArtifactDrifted => "artifact-drifted", ArtifactState::Missing => "missing", ArtifactState::Unreadable => "unreadable", ArtifactState::NotApplicable => "not-applicable", ArtifactState::NotChecked => "not-checked"]);
        check!(ProjectionState, [ProjectionState::Current => "current", ProjectionState::Drifted => "drifted", ProjectionState::Missing => "missing", ProjectionState::NotApplicable => "not-applicable", ProjectionState::NotChecked => "not-checked"]);
        check!(HoldPolicy, [HoldPolicy::Held => "held", HoldPolicy::Unheld => "unheld"]);
        check!(ManagerPinState, [ManagerPinState::Pinned => "pinned", ManagerPinState::Unpinned => "unpinned", ManagerPinState::Unsupported => "unsupported", ManagerPinState::Unknown => "unknown"]);
        check!(OperationState, [OperationState::None => "none", OperationState::Prepared => "prepared", OperationState::PackageVerified => "package-verified", OperationState::ArtifactValidated => "artifact-validated", OperationState::ProjectionSucceeded => "projection-succeeded", OperationState::Committed => "committed", OperationState::RecoveryRequired => "recovery-required"]);
        check!(AdapterKind, [
            AdapterKind::Cargo => "cargo",
            AdapterKind::UvTool => "uv-tool",
            AdapterKind::Pipx => "pipx",
            AdapterKind::InterpreterPip => "interpreter-pip",
            AdapterKind::NpmGlobal => "npm-global",
            AdapterKind::PnpmGlobal => "pnpm-global",
            AdapterKind::BrewFormula => "brew-formula",
            AdapterKind::BrewCask => "brew-cask",
            AdapterKind::WingetOffline => "winget-offline",
            AdapterKind::WingetOnline => "winget-online",
            AdapterKind::CcyncReceipt => "ccync-receipt",
            AdapterKind::ManualPath => "manual-path",
        ]);
        check!(DeliveryId, [
            DeliveryId::GraphifyAgents => "graphify-agents",
            DeliveryId::RtkPath => "rtk-path",
            DeliveryId::ObserveOnly => "observe-only",
        ]);
    }
    #[test]
    fn identity_fields_normalize_order() {
        let a = InstallationIdentity {
            adapter_kind: AdapterKind::Cargo,
            fields: [("z".into(), "1".into()), ("a".into(), "2".into())].into(),
        };
        let b = InstallationIdentity {
            adapter_kind: AdapterKind::Cargo,
            fields: [("a".into(), "2".into()), ("z".into(), "1".into())].into(),
        };
        assert_eq!(a, b);
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
    }
    #[test]
    fn tool_ids_reject_unsafe_components() {
        for id in [
            "",
            ".",
            "..",
            "/",
            "\\",
            "a/b",
            "a\\b",
            "../x",
            "x/..",
            "C:",
            "C:\\tool",
            "D:tool",
            "a/b/c",
            "foo/../bar",
        ] {
            assert!(ToolId::new(id).is_err(), "accepted {id:?}");
            let json = serde_json::to_string(id).unwrap();
            assert!(
                serde_json::from_str::<ToolId>(&json).is_err(),
                "deserialized unsafe id {id:?}"
            );
        }
        assert!(ToolId::new("safe-id").is_ok());
        assert_eq!(
            serde_json::from_str::<ToolId>("\"safe-id\"").unwrap(),
            ToolId::new("safe-id").unwrap()
        );
    }
}
