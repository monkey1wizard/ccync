pub mod brew_cask;
pub mod brew_formula;
pub mod cargo;
pub mod ccync_receipt;
pub mod gates;
pub mod manual_path;
pub mod npm;
pub mod pip;
pub mod pipx;
pub mod pnpm;
pub mod uv;
pub mod winget_offline;
pub mod winget_online;

use crate::tool_manager::{
    discovery::{DiscoveryCandidate, DiscoveryQuery},
    model::{AdapterKind, DeliveryId, ManagerPinState, ToolDeclaration},
};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityState {
    Supported,
    Unsupported,
    Gated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterCapabilities {
    pub local_enumeration: CapabilityState,
    pub executable_mapping: CapabilityState,
    pub installed_version_lookup: CapabilityState,
    pub remote_discovery: CapabilityState,
    pub package_update: CapabilityState,
}

impl AdapterCapabilities {
    pub fn for_adapter(
        kind: AdapterKind,
        local_enumeration: CapabilityState,
        executable_mapping: CapabilityState,
        installed_version_lookup: CapabilityState,
        remote_discovery: CapabilityState,
    ) -> Self {
        Self {
            local_enumeration,
            executable_mapping,
            installed_version_lookup,
            remote_discovery,
            package_update: gates::package_update_state(kind),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterError {
    Failed(String),
    NotChecked,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteUpdateStatus {
    Current,
    UpdateAvailable,
    Unsupported,
    NotChecked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterOutcome {
    Discovered(Vec<DiscoveryCandidate>),
    NotChecked,
    Failed(String),
}

pub trait ToolAdapter: Sync {
    fn kind(&self) -> AdapterKind;
    fn capabilities(&self) -> AdapterCapabilities;
    fn discover(&self, query: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError>;
    fn reprobe(&self, query: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError>;
    fn remote_update_status(
        &self,
        _query: &DiscoveryQuery,
    ) -> Result<RemoteUpdateStatus, AdapterError> {
        Ok(RemoteUpdateStatus::Unsupported)
    }
    fn manager_pin(&self) -> Result<ManagerPinState, AdapterError> {
        Ok(ManagerPinState::Unsupported)
    }
    fn update_argv(&self) -> Option<Vec<String>> {
        None
    }
}

pub const ADAPTER_REGISTRY: [AdapterKind; 12] = [
    AdapterKind::Cargo,
    AdapterKind::UvTool,
    AdapterKind::Pipx,
    AdapterKind::InterpreterPip,
    AdapterKind::NpmGlobal,
    AdapterKind::PnpmGlobal,
    AdapterKind::BrewFormula,
    AdapterKind::BrewCask,
    AdapterKind::WingetOffline,
    AdapterKind::WingetOnline,
    AdapterKind::CcyncReceipt,
    AdapterKind::ManualPath,
];

/// Reconstruct an adapter and the declaration metadata needed by lifecycle callers.
pub fn for_tracked_declaration(
    declaration: &ToolDeclaration,
) -> Result<(Box<dyn ToolAdapter>, DeliveryId, Option<serde_json::Value>), AdapterError> {
    let fields = &declaration.identity.fields;
    let field = |name: &str| {
        fields
            .get(name)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| AdapterError::Failed(format!("tracked identity is missing {name}")))
    };

    let adapter: Box<dyn ToolAdapter> = match declaration.identity.adapter_kind {
        AdapterKind::Cargo => {
            if declaration.delivery_id != DeliveryId::RtkPath {
                return Err(AdapterError::Unsupported);
            }
            let source = field("source")?;
            let name = field("name")?;
            let root = field("root")?;
            let executable = field("executable")?;
            let cargo_executable = fields
                .get("managerExecutable")
                .filter(|value| !value.is_empty())
                .map(Path::new)
                .unwrap_or_else(|| Path::new("cargo"));
            if !Path::new(root).is_absolute()
                || !Path::new(executable).is_absolute()
                || name.contains(['/', '\\'])
            {
                return Err(AdapterError::Failed(
                    "tracked Cargo identity has an invalid executable or package name".into(),
                ));
            }
            if source.contains(['\r', '\n']) {
                return Err(AdapterError::Failed(
                    "tracked Cargo identity has an invalid source".into(),
                ));
            }
            Box::new(cargo::CargoAdapter::for_recorded_update(
                declaration.identity.clone(),
                cargo_executable,
            ))
        }
        AdapterKind::InterpreterPip => {
            if declaration.delivery_id != DeliveryId::GraphifyAgents {
                return Err(AdapterError::Unsupported);
            }
            let interpreter = field("interpreter")?;
            let distribution = field("distribution")?;
            let entry_point = field("entry_point")?;
            let executable = field("executable")?;
            if !Path::new(interpreter).is_absolute()
                || !Path::new(executable).is_absolute()
                || distribution != &normalize_distribution(distribution)
                || entry_point.contains(['/', '\\'])
            {
                return Err(AdapterError::Failed(
                    "tracked interpreter-pip identity is invalid".into(),
                ));
            }
            Box::new(pip::PipAdapter::for_recorded_update(
                declaration.identity.clone(),
            ))
        }
        _ => return Err(AdapterError::Unsupported),
    };

    Ok((adapter, declaration.delivery_id, declaration.manager_takeover.clone()))
}

fn normalize_distribution(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    let mut separator = false;
    for character in value.chars() {
        if matches!(character, '-' | '_' | '.') {
            separator = true;
        } else {
            if separator && !normalized.is_empty() {
                normalized.push('-');
            }
            separator = false;
            normalized.push(character.to_ascii_lowercase());
        }
    }
    normalized
}

pub fn discover_all(
    adapters: &[&dyn ToolAdapter],
    query: &DiscoveryQuery,
) -> Vec<(AdapterKind, AdapterOutcome)> {
    adapters
        .iter()
        .map(|adapter| {
            let result = if adapter.capabilities().remote_discovery != CapabilityState::Unsupported
                && !query.online
            {
                Ok(None)
            } else {
                adapter.discover(query).map(Some)
            };
            (
                adapter.kind(),
                match result {
                    Ok(Some(candidates)) => AdapterOutcome::Discovered(candidates),
                    Ok(None) => AdapterOutcome::NotChecked,
                    Err(AdapterError::NotChecked) => AdapterOutcome::NotChecked,
                    Err(AdapterError::Failed(e)) => AdapterOutcome::Failed(e),
                    Err(AdapterError::Unsupported) => AdapterOutcome::NotChecked,
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_manager::model::{HoldPolicy, InstallationIdentity, ToolId};
    use std::collections::BTreeMap;

    fn declaration<const N: usize>(
        kind: AdapterKind,
        delivery_id: DeliveryId,
        fields: [(&str, &str); N],
        manager_takeover: Option<serde_json::Value>,
    ) -> ToolDeclaration {
        ToolDeclaration {
            id: ToolId::new("tracked-tool").unwrap(),
            identity: InstallationIdentity {
                adapter_kind: kind,
                fields: fields
                    .into_iter()
                    .filter(|(_, value)| !value.is_empty())
                    .map(|(key, value)| (key.into(), value.into()))
                    .collect::<BTreeMap<_, _>>(),
            },
            delivery_id,
            hold_policy: HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover,
        }
    }

    #[test]
    fn for_tracked_declaration_preserves_identity_metadata_and_rejects_invalid_shapes() {
        let cargo_path = if cfg!(windows) {
            r"C:\opt\.cargo\bin\cargo"
        } else {
            "/opt/.cargo/bin/cargo"
        };
        let cargo_executable = if cfg!(windows) {
            r"C:\opt\.cargo\bin\rtk.exe"
        } else {
            "/opt/.cargo/bin/rtk"
        };
        let cargo = declaration(
            AdapterKind::Cargo,
            DeliveryId::RtkPath,
            [
                ("name", "rtk"),
                ("source", "registry+https://github.com/rust-lang/crates.io-index"),
                ("root", cargo_path),
                ("executable", cargo_executable),
                ("managerExecutable", cargo_path),
                ("unused", ""),
            ],
            Some(serde_json::json!({"manager": "cargo"})),
        );
        let (adapter, delivery, takeover) = for_tracked_declaration(&cargo).unwrap();
        assert_eq!(adapter.kind(), AdapterKind::Cargo);
        assert_eq!(delivery, DeliveryId::RtkPath);
        assert_eq!(takeover, cargo.manager_takeover);
        assert_eq!(adapter.update_argv().unwrap()[0], cargo_path);

        let interpreter_path = if cfg!(windows) {
            r"C:\opt\python\bin\python.exe"
        } else {
            "/opt/python/bin/python"
        };
        let pip_executable = if cfg!(windows) {
            r"C:\opt\python\bin\graphify.exe"
        } else {
            "/opt/python/bin/graphify"
        };
        let pip = declaration(
            AdapterKind::InterpreterPip,
            DeliveryId::GraphifyAgents,
            [
                ("interpreter", interpreter_path),
                ("distribution", "graphify-y"),
                ("entry_point", "graphify"),
                ("executable", pip_executable),
                ("unused", ""),
            ],
            None,
        );
        let (adapter, delivery, takeover) = for_tracked_declaration(&pip).unwrap();
        assert_eq!(adapter.kind(), AdapterKind::InterpreterPip);
        assert_eq!(delivery, DeliveryId::GraphifyAgents);
        assert_eq!(takeover, None);
        assert_eq!(adapter.update_argv().unwrap()[0], interpreter_path);

        let unsupported = declaration(
            AdapterKind::ManualPath,
            DeliveryId::ObserveOnly,
            [("", ""), ("", ""), ("", ""), ("", ""), ("", "")],
            None,
        );
        assert!(matches!(
            for_tracked_declaration(&unsupported),
            Err(AdapterError::Unsupported)
        ));

        let mut wrong_delivery = cargo.clone();
        wrong_delivery.delivery_id = DeliveryId::GraphifyAgents;
        assert!(matches!(
            for_tracked_declaration(&wrong_delivery),
            Err(AdapterError::Unsupported)
        ));

        let mut incomplete = pip.clone();
        incomplete.identity.fields.remove("interpreter");
        assert!(matches!(
            for_tracked_declaration(&incomplete),
            Err(AdapterError::Failed(_))
        ));
    }

    struct Fake {
        kind: AdapterKind,
        remote: CapabilityState,
        fail: bool,
    }
    impl ToolAdapter for Fake {
        fn kind(&self) -> AdapterKind {
            self.kind
        }
        fn capabilities(&self) -> AdapterCapabilities {
            AdapterCapabilities::for_adapter(
                self.kind,
                CapabilityState::Supported,
                CapabilityState::Supported,
                CapabilityState::Supported,
                self.remote,
            )
        }
        fn discover(&self, _: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
            if self.fail {
                Err(AdapterError::Failed("fixture".into()))
            } else {
                Ok(vec![])
            }
        }
        fn reprobe(&self, q: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
            self.discover(q)
        }
    }
    #[test]
    fn isolates_failures_and_skips_network_offline() {
        let a = Fake {
            kind: AdapterKind::Cargo,
            remote: CapabilityState::Unsupported,
            fail: true,
        };
        let b = Fake {
            kind: AdapterKind::WingetOnline,
            remote: CapabilityState::Supported,
            fail: false,
        };
        let out = discover_all(&[&a, &b], &DiscoveryQuery { online: false });
        assert!(matches!(out[0].1, AdapterOutcome::Failed(_)));
        assert_eq!(out[1].1, AdapterOutcome::NotChecked);
    }
    #[test]
    fn package_update_capability_comes_from_compiled_gate_policy() {
        for kind in [AdapterKind::Cargo, AdapterKind::InterpreterPip] {
            let f = Fake {
                kind,
                remote: CapabilityState::Unsupported,
                fail: false,
            };
            assert_eq!(
                f.capabilities().package_update,
                gates::package_update_state(kind)
            );
        }
        for kind in [AdapterKind::ManualPath, AdapterKind::WingetOnline] {
            let f = Fake {
                kind,
                remote: CapabilityState::Unsupported,
                fail: false,
            };
            assert_eq!(f.capabilities().package_update, CapabilityState::Gated);
        }
    }
}
