//! Persisted per-host committed ownership.
//!
//! `projected-state.json` records, per host, the set of MCP server names ccync
//! has *proven* it owns — i.e. names it has successfully written and may later
//! remove. It is the durable answer to "did ccync put this entry here?", so a
//! managed server that disappears from the desired manifest is only deleted from
//! a host where ccync committed ownership of that name. One host's ownership
//! never implies another's (the sets are independent).
//!
//! The file lives beside `managed.json` and is loaded/saved via the crate's
//! atomic writer so a crash cannot truncate it.

use crate::host_render::Host;
use crate::io::write_atomic;
use crate::McpUpdateError;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Current on-disk schema version.
const PROJECTED_STATE_VERSION: u32 = 2;
/// File name, resolved beside `managed.json`.
const PROJECTED_STATE_FILE: &str = "projected-state.json";

/// Committed per-host ownership. `hosts` maps a host key (`claude` / `copilot` /
/// `opencode` / `codex`) to the set of owned server names. A host absent from
/// the map owns nothing. `BTreeMap`/`BTreeSet` keep serialization deterministic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProjectedState {
    pub version: u32,
    #[serde(default)]
    pub hosts: BTreeMap<String, BTreeSet<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration: Option<MigrationSection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub publications: Vec<crate::ReselectHold>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub holds: Vec<crate::ReselectHold>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MigrationSection {
    pub complete: bool,
    pub routes: Vec<crate::MigratedRoute>,
    pub unproven: Vec<UnprovenRoute>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UnprovenRoute {
    pub host: String,
    pub server: String,
    pub missing: Vec<String>,
}

impl ProjectedState {
    /// Fresh, empty state at the current schema version.
    pub(crate) fn new() -> Self {
        Self {
            version: PROJECTED_STATE_VERSION,
            hosts: BTreeMap::new(),
            migration: None,
            publications: Vec::new(),
            holds: Vec::new(),
        }
    }

    /// The owned-name set for a host (empty if the host owns nothing). The
    /// read accessor paired with `set_owned`; drives uninstall's per-host
    /// removal.
    pub(crate) fn owned(&self, host: Host) -> BTreeSet<String> {
        self.hosts.get(host.as_str()).cloned().unwrap_or_default()
    }

    /// Replace a host's owned-name set. An empty set removes the host key so the
    /// serialized form never carries empty maps.
    pub(crate) fn set_owned(&mut self, host: Host, names: BTreeSet<String>) {
        if names.is_empty() {
            self.hosts.remove(host.as_str());
        } else {
            self.hosts.insert(host.as_str().to_string(), names);
        }
    }

    /// State-file path beside a given `managed.json`.
    pub(crate) fn path_beside(managed_json: &Path) -> PathBuf {
        managed_json.with_file_name(PROJECTED_STATE_FILE)
    }

    /// Load committed ownership. A missing or empty file is the safe default
    /// (no ownership yet), never an error.
    pub(crate) fn load(path: &Path) -> Result<Self, McpUpdateError> {
        if !path.exists() {
            return Ok(Self::new());
        }
        let content = std::fs::read_to_string(path)?;
        if content.trim().is_empty() {
            return Ok(Self::new());
        }
        let state: Self = serde_json::from_str(&content)?;
        if state.version == 0 || state.version > PROJECTED_STATE_VERSION {
            return Err(McpUpdateError::AmbiguousProjection(format!(
                "unsupported projected state version: {}",
                state.version
            )));
        }
        Ok(state)
    }

    /// Persist atomically (pretty JSON, deterministic key order).
    pub(crate) fn to_bytes(&self) -> Result<Vec<u8>, McpUpdateError> {
        if self.version == 0 || self.version > PROJECTED_STATE_VERSION {
            return Err(McpUpdateError::AmbiguousProjection(format!(
                "unsupported projected state version: {}",
                self.version
            )));
        }
        Ok(serde_json::to_vec_pretty(self)?)
    }

    pub(crate) fn save(&self, path: &Path) -> Result<(), McpUpdateError> {
        let bytes = self.to_bytes()?;
        crate::ensure_parent(path)?;
        write_atomic(path, std::str::from_utf8(&bytes).expect("JSON is UTF-8"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn persisted_projection_state_round_trips_deterministically() {
        let tmp = TempDir::new().unwrap();
        let path = ProjectedState::path_beside(&tmp.path().join("managed.json"));

        let mut state = ProjectedState::new();
        state.set_owned(Host::Claude, set(&["a", "b"]));
        state.set_owned(Host::Codex, set(&["c"]));
        state.save(&path).unwrap();

        let first_bytes = std::fs::read_to_string(&path).unwrap();
        let loaded = ProjectedState::load(&path).unwrap();
        assert_eq!(loaded, state);

        // Re-saving the loaded state yields byte-identical output (deterministic).
        loaded.save(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), first_bytes);
    }

    #[test]
    fn persisted_projection_state_missing_file_is_empty_default() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("absent-state.json");
        let state = ProjectedState::load(&path).unwrap();
        assert_eq!(state, ProjectedState::new());
        assert!(state.owned(Host::Claude).is_empty());
    }

    #[test]
    fn persisted_projection_state_hosts_are_independent() {
        let mut state = ProjectedState::new();
        state.set_owned(Host::Claude, set(&["shared", "claude-only"]));
        state.set_owned(Host::Codex, set(&["shared"]));

        assert_eq!(state.owned(Host::Claude), set(&["claude-only", "shared"]));
        assert_eq!(state.owned(Host::Codex), set(&["shared"]));
        assert!(state.owned(Host::Copilot).is_empty());
        assert!(state.owned(Host::OpenCode).is_empty());
    }

    #[test]
    fn persisted_projection_state_path_is_sibling_of_managed() {
        let p = ProjectedState::path_beside(Path::new("/x/build/mcp/managed.json"));
        assert_eq!(p, PathBuf::from("/x/build/mcp/projected-state.json"));
    }

    #[test]
    fn persisted_projection_state_empty_set_drops_host_key() {
        let mut state = ProjectedState::new();
        state.set_owned(Host::Claude, set(&["a"]));
        state.set_owned(Host::Claude, set(&[]));
        assert!(
            state.hosts.is_empty(),
            "empty owned-set must remove the host key"
        );
    }
}
