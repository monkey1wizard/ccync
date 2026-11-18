//! Configuration parsing and management for CCYNC.
//!
//! Rust owns config truth. The config file is `~/.ccync/config.json`.
//! Missing file or keys produce explicit defaults; no panics.
//!
//! Mode authority:
//! - Normal mode: `devMode` absent or `false`. Renders from packaged source.
//! - Dev mode: `devMode: true` AND `ccyncRoot` usable. Renders from working tree.
//! - `devMode: true` but `ccyncRoot` unusable → explicit error, no fallback.
//!
//! The `installMode` field is deprecated. If present, it may be used for
//! migration hints but does not override `devMode`.

use crate::json_util;
use crate::platform;
use crate::target::ProjectionTargetId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use thiserror::Error;

// ── Machine path registry ─────────────────────────────────────────────────────

/// Maps config.json dot-paths to substitution variable names for the MCP
/// `${VAR}` engine. Only generic path keys are included; domain-specific
/// keys have been removed.
///
/// Excludes: `MCP_FILESYSTEM_PATHS` (dead — broken wiring, no consumer),
/// credential keys (`*_API_KEY`, `*_TOKEN`, etc.) which live in
/// `config.json#secrets` and are never path-substituted.
pub const MACHINE_PATH_REGISTRY: &[(&str, &str)] =
    &[("ccyncSkills", "CCYNC_SKILLS"), ("tempDir", "TEMP_DIR")];

/// Look up a machine path value from a `config.json` root `serde_json::Value`.
///
/// `config_path` is a dot-separated key path (e.g. `"custom.path"`).
/// Returns the string value when present, `None` when absent or not a string.
pub fn machine_path_value(config: &Value, config_path: &str) -> Option<String> {
    let mut current = config;
    for segment in config_path.split('.') {
        current = current.get(segment)?;
    }
    current.as_str().map(str::to_owned)
}

/// The complete CCYNC configuration model.
///
/// Corresponds to `~/.ccync/config.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct CcyncConfig {
    /// Developer mode flag. When `true`, CCYNC renders from the working tree
    /// specified by `ccyncRoot`. When `false` or absent, CCYNC renders from
    /// packaged source (normal mode). `None` means the field was not present
    /// in the JSON file, which enables legacy migration logic.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dev_mode: Option<bool>,

    /// Path to the CCYNC repository working tree. Only used when `dev_mode` is
    /// `true`. If `dev_mode` is `true` but `ccync_root` is not usable, CCYNC will
    /// error rather than silently fall back to normal mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ccync_root: Option<String>,

    /// Deprecated field. If present, it may be used for migration hints but
    /// does not control mode authority. `devMode` and `ccyncRoot` are the
    /// authoritative fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_mode: Option<String>,

    /// The set of coding-agent runtimes that are currently selected. Replaces
    /// the former `install-state.json` field of the same name. Absent field
    /// (from old configs) deserializes to an empty vec.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected_runtimes: Vec<String>,

    /// The set of projection targets that are currently selected. Canonical
    /// replacement for `selected_runtimes`. Absent field (from old configs)
    /// deserializes to an empty vec. See [`CcyncConfig::resolve_projection_targets`]
    /// for the read-time resolution rule against the legacy field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected_projection_targets: Vec<String>,

    /// Unrecognized machine configuration stays private and survives edits.
    #[serde(flatten)]
    pub private_fields: BTreeMap<String, Value>,
}

/// Errors returned by the strict configuration loader.
#[derive(Debug, Error)]
pub enum CcyncConfigLoadError {
    /// The platform could not determine the configuration file path.
    #[error("could not determine config path: {0}")]
    Path(#[source] io::Error),
    /// The configuration file could not be read.
    #[error("could not read config at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The configuration file did not contain valid configuration JSON.
    #[error("invalid config JSON at {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

impl Default for CcyncConfig {
    /// Default configuration: normal mode, no runtimes selected.
    fn default() -> Self {
        CcyncConfig {
            dev_mode: None,
            ccync_root: None,
            install_mode: None,
            selected_runtimes: Vec::new(),
            selected_projection_targets: Vec::new(),
            private_fields: BTreeMap::new(),
        }
    }
}

/// Which of the canonical (`selectedProjectionTargets`) and legacy
/// (`selectedRuntimes`) config keys were present (non-empty) when
/// [`CcyncConfig::resolve_projection_targets`] was called.
///
/// "Present" means non-empty — this crate's existing convention already
/// treats an absent JSON key and an empty array as indistinguishable after
/// deserialization, so the same rule applies here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyState {
    /// Only the canonical key was present. Its values were used.
    Canonical,
    /// Only the legacy key was present. Its values were used.
    Legacy,
    /// Both keys were present. The canonical key's values were used; this
    /// variant is itself the warning signal — callers that want to surface a
    /// warning to the user should do so when they observe this state.
    Both,
    /// Neither key was present. No values are available.
    Absent,
}

/// Outcome of [`CcyncConfig::resolve_projection_targets`].
///
/// The resolver parses the selected key's entries but does not itself log or
/// fail on bad input; it reports what it kept, which keys were present, and
/// which entries it could not parse, and leaves any user-facing warning to the
/// caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionTargetResolution {
    /// Parsed targets, de-duplicated, in the on-disk order of each target's
    /// first appearance.
    pub targets: Vec<ProjectionTargetId>,
    /// Which config keys were present (the warning signal for [`LegacyState::Both`]).
    pub state: LegacyState,
    /// Selection entries that were non-empty after normalization but did not
    /// parse as a known [`ProjectionTargetId`], in on-disk order and normalized
    /// form. A caller that wants to warn the user (as the legacy `install.rs`
    /// path does) surfaces these; the resolver neither logs nor fails on them.
    pub rejected: Vec<String>,
}

impl CcyncConfig {
    /// Load configuration from the effective machine config path, preserving
    /// path, filesystem, and JSON failures for callers that must fail closed.
    pub fn load_strict() -> Result<Self, CcyncConfigLoadError> {
        let path = Self::config_path().map_err(CcyncConfigLoadError::Path)?;
        Self::load_strict_from_path(&path)
    }

    /// Load configuration from a specific path, preserving read and JSON
    /// failures for callers that must fail closed.
    pub fn load_strict_from_path(path: &Path) -> Result<Self, CcyncConfigLoadError> {
        let content = fs::read_to_string(path).map_err(|source| CcyncConfigLoadError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        serde_json::from_str::<CcyncConfig>(&content).map_err(|source| CcyncConfigLoadError::Json {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Load configuration from the effective machine config path.
    ///
    /// Returns the default configuration if:
    /// - The file does not exist
    /// - The file cannot be read
    /// - The JSON is malformed
    ///
    /// No panics.
    pub fn load() -> Self {
        let config_path = match Self::config_path() {
            Ok(path) => path,
            Err(_) => return CcyncConfig::default(),
        };

        Self::load_from_path(&config_path)
    }

    /// Load configuration from a specific path.
    ///
    /// Returns the default configuration if the file does not exist, cannot
    /// be read, or contains invalid JSON.
    pub fn load_from_path(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(content) => serde_json::from_str::<CcyncConfig>(&content).unwrap_or_default(),
            Err(_) => CcyncConfig::default(),
        }
    }

    /// Get the effective machine config path, falling back to the legacy
    /// `~/.ccync/config.json` path until a layout commit selects new roots.
    pub fn config_path() -> io::Result<PathBuf> {
        match crate::paths::EffectiveLayout::load() {
            Ok(layout) => Ok(layout.machine_config_path()),
            Err(crate::paths::LayoutLoadError::RootValidation(_)) => {
                crate::paths::machine_config_path().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "could not determine legacy config path",
                    )
                })
            }
            Err(crate::paths::LayoutLoadError::Io { source, .. }) => Err(source),
        }
    }

    /// Check if the configuration is in dev mode.
    pub fn is_dev_mode(&self) -> bool {
        self.dev_mode.unwrap_or(false)
    }

    /// Get the ccyncRoot path if set.
    pub fn ccync_root(&self) -> Option<&str> {
        self.ccync_root.as_deref()
    }

    /// Check if the deprecated installMode field is present.
    ///
    /// Used for migration warnings in `ccync doctor`.
    pub fn has_deprecated_install_mode(&self) -> bool {
        self.install_mode.is_some()
    }

    /// Resolve the selected projection targets, reconciling the canonical
    /// `selectedProjectionTargets` key against the legacy `selectedRuntimes`
    /// key.
    ///
    /// Resolution rule (read-only — this does not write or migrate anything):
    /// - Canonical present, legacy present → [`LegacyState::Both`]; canonical
    ///   values win.
    /// - Canonical present, legacy absent → [`LegacyState::Canonical`];
    ///   canonical values.
    /// - Canonical absent, legacy present → [`LegacyState::Legacy`]; legacy
    ///   values.
    /// - Canonical absent, legacy absent → [`LegacyState::Absent`]; empty
    ///   result. No fallback is applied here.
    ///
    /// "Present" means non-empty, matching this struct's existing
    /// deserialization convention.
    ///
    /// The returned targets preserve the order the strings appear in on disk
    /// (whichever key was selected) — they are not re-sorted into
    /// [`crate::target::CANONICAL_PROJECTION_TARGETS`] order.
    ///
    /// Each entry is normalized (trimmed, lowercased) before parsing, matching
    /// the tolerance the legacy `install.rs` / projection selection paths this
    /// resolver supersedes already gave hand-edited config (`"Claude"`,
    /// `" codex"` resolve). Whitespace-only entries are silently skipped;
    /// duplicates collapse to their first occurrence. Entries that are still
    /// unknown after normalization are collected into
    /// [`ProjectionTargetResolution::rejected`] rather than silently dropped,
    /// so a caller can reproduce install.rs's named-invalid-key warning; the
    /// resolver itself neither logs nor fails (read-only, no panics).
    pub fn resolve_projection_targets(&self) -> ProjectionTargetResolution {
        let canonical_present = !self.selected_projection_targets.is_empty();
        let legacy_present = !self.selected_runtimes.is_empty();

        let (source, state): (&[String], LegacyState) = match (canonical_present, legacy_present) {
            (true, true) => (&self.selected_projection_targets, LegacyState::Both),
            (true, false) => (&self.selected_projection_targets, LegacyState::Canonical),
            (false, true) => (&self.selected_runtimes, LegacyState::Legacy),
            (false, false) => (&[], LegacyState::Absent),
        };

        let mut targets: Vec<ProjectionTargetId> = Vec::new();
        let mut rejected: Vec<String> = Vec::new();
        for raw in source {
            let normalized = raw.trim().to_ascii_lowercase();
            if normalized.is_empty() {
                continue;
            }
            match ProjectionTargetId::from_str(&normalized) {
                Ok(id) => {
                    if !targets.contains(&id) {
                        targets.push(id);
                    }
                }
                Err(_) => rejected.push(normalized),
            }
        }

        ProjectionTargetResolution {
            targets,
            state,
            rejected,
        }
    }
}

/// Legacy config keys removed by [`write_projection_selection_at`] when it
/// writes the canonical `selectedProjectionTargets` key.
const LEGACY_SELECTED_RUNTIMES_KEY: &str = "selectedRuntimes";
const LEGACY_PRIMARY_RUNTIME_KEY: &str = "primaryRuntime";
/// Old-layout spelling of the selection key: the pre-flatten
/// `config/config.json` wrote the selection as `runtimes`, and the layout
/// migration moves that file verbatim, so the spelling can surface in a
/// migrated `config.json`. It stops propagating the same way the other
/// legacy keys do.
const LEGACY_OLD_LAYOUT_RUNTIMES_KEY: &str = "runtimes";
const CANONICAL_SELECTED_PROJECTION_TARGETS_KEY: &str = "selectedProjectionTargets";

/// Write the canonical `selectedProjectionTargets` key to the config file at
/// `path`, removing the legacy `selectedRuntimes` / `primaryRuntime` keys and
/// the old-layout `runtimes` spelling.
///
/// Operates on the config file as a raw JSON object map rather than
/// round-tripping through [`CcyncConfig`]: a typed round-trip would silently
/// drop the `secrets` subtree and any unknown/forward-compatible keys. Only
/// the four keys named above are touched; every other top-level key
/// (including `secrets`) is preserved unchanged. The write itself is atomic
/// and durable via [`platform::atomic_write_bytes`].
///
/// `targets` is written in the exact order given — this function does not
/// sort or dedupe.
pub fn write_projection_selection_at(
    path: &Path,
    targets: &[ProjectionTargetId],
) -> io::Result<()> {
    let mut map = json_util::read_json_map(path)?;

    let array = Value::Array(
        targets
            .iter()
            .map(|t| Value::String(t.as_str().to_owned()))
            .collect(),
    );
    map.insert(CANONICAL_SELECTED_PROJECTION_TARGETS_KEY.to_owned(), array);
    map.remove(LEGACY_SELECTED_RUNTIMES_KEY);
    map.remove(LEGACY_PRIMARY_RUNTIME_KEY);
    map.remove(LEGACY_OLD_LAYOUT_RUNTIMES_KEY);

    let json = serde_json::to_string_pretty(&Value::Object(map))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    platform::atomic_write_bytes(path, json.as_bytes()).map_err(|e| match e {
        platform::AtomicWriteError::TempWrite(e) => e,
        platform::AtomicWriteError::PreRename(e) => e,
        platform::AtomicWriteError::Rename(e) => e,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn config_fallible_load_reports_missing_file_and_tolerant_loader_defaults() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("missing.json");

        let error = CcyncConfig::load_strict_from_path(&path).unwrap_err();
        assert!(matches!(error, CcyncConfigLoadError::Read { .. }));
        assert_eq!(CcyncConfig::load_from_path(&path), CcyncConfig::default());
    }

    #[test]
    fn config_fallible_load_reports_directory_read_error() {
        let dir = tempfile::TempDir::new().unwrap();

        let error = CcyncConfig::load_strict_from_path(dir.path()).unwrap_err();
        assert!(matches!(error, CcyncConfigLoadError::Read { .. }));
    }

    #[test]
    fn config_fallible_load_reports_malformed_json_and_tolerant_loader_defaults() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "{{ malformed json").unwrap();
        file.flush().unwrap();

        let error = CcyncConfig::load_strict_from_path(file.path()).unwrap_err();
        assert!(matches!(error, CcyncConfigLoadError::Json { .. }));
        assert_eq!(
            CcyncConfig::load_from_path(file.path()),
            CcyncConfig::default()
        );
    }

    #[test]
    fn config_fallible_load_parses_valid_json() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"devMode":true,"ccyncRoot":"/repo"}}"#).unwrap();
        file.flush().unwrap();

        let config = CcyncConfig::load_strict_from_path(file.path()).unwrap();
        assert_eq!(config.dev_mode, Some(true));
        assert_eq!(config.ccync_root.as_deref(), Some("/repo"));
    }

    #[test]
    fn test_default_config() {
        let config = CcyncConfig::default();
        assert!(!config.is_dev_mode());
        assert_eq!(config.ccync_root(), None);
        assert!(!config.has_deprecated_install_mode());
    }

    #[test]
    fn config_layout_path_uses_legacy_path_for_fresh_and_legacy_fixtures() {
        let _guard = crate::test_support::env_guard();
        let fake_home = tempfile::TempDir::new().unwrap();
        let vars = if cfg!(windows) {
            vec!["USERPROFILE", "APPDATA", "LOCALAPPDATA"]
        } else {
            vec![
                "HOME",
                "XDG_CONFIG_HOME",
                "XDG_STATE_HOME",
                "XDG_CACHE_HOME",
            ]
        };
        let prior: Vec<_> = vars
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        let set_fixture_env = || {
            let home_key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
            std::env::set_var(home_key, fake_home.path());
            if cfg!(windows) {
                std::env::set_var("APPDATA", fake_home.path().join("AppData").join("Roaming"));
                std::env::set_var(
                    "LOCALAPPDATA",
                    fake_home.path().join("AppData").join("Local"),
                );
            } else {
                std::env::set_var("XDG_CONFIG_HOME", fake_home.path().join("config"));
                std::env::set_var("XDG_STATE_HOME", fake_home.path().join("state"));
                std::env::set_var("XDG_CACHE_HOME", fake_home.path().join("cache"));
            }
        };
        set_fixture_env();

        let expected = fake_home.path().join(".ccync").join("config.json");
        let fresh_path = CcyncConfig::config_path().unwrap();
        assert_eq!(fresh_path, expected, "fresh fixture must use {expected:?}");

        fs::create_dir_all(expected.parent().unwrap()).unwrap();
        fs::write(expected.parent().unwrap().join("legacy-marker"), b"").unwrap();
        let legacy_path = CcyncConfig::config_path().unwrap();
        assert_eq!(
            legacy_path, expected,
            "legacy fixture must use {expected:?}"
        );

        for (key, value) in prior {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn config_layout_path_falls_back_to_legacy_path_for_invalid_roots() {
        let _guard = crate::test_support::env_guard();
        let fake_home = tempfile::TempDir::new().unwrap();
        let vars = if cfg!(windows) {
            vec!["USERPROFILE", "APPDATA", "LOCALAPPDATA"]
        } else {
            vec![
                "HOME",
                "XDG_CONFIG_HOME",
                "XDG_STATE_HOME",
                "XDG_CACHE_HOME",
            ]
        };
        let prior: Vec<_> = vars
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        let home_key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        std::env::set_var(home_key, fake_home.path());
        if cfg!(windows) {
            std::env::set_var("APPDATA", fake_home.path().join(".agents").join("ccync"));
            std::env::set_var(
                "LOCALAPPDATA",
                fake_home.path().join("AppData").join("Local"),
            );
        } else {
            std::env::set_var(
                "XDG_CONFIG_HOME",
                fake_home.path().join(".agents").join("ccync"),
            );
            std::env::set_var("XDG_STATE_HOME", fake_home.path().join("state"));
            std::env::set_var("XDG_CACHE_HOME", fake_home.path().join("cache"));
        }

        let expected = fake_home.path().join(".ccync").join("config.json");
        assert_eq!(CcyncConfig::config_path().unwrap(), expected);

        for (key, value) in prior {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn config_layout_path_uses_committed_machine_config_and_loads_its_bytes() {
        let _guard = crate::test_support::env_guard();
        let fake_home = tempfile::TempDir::new().unwrap();
        let vars = if cfg!(windows) {
            vec!["USERPROFILE", "APPDATA", "LOCALAPPDATA"]
        } else {
            vec![
                "HOME",
                "XDG_CONFIG_HOME",
                "XDG_STATE_HOME",
                "XDG_CACHE_HOME",
            ]
        };
        let prior: Vec<_> = vars
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        let home_key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        std::env::set_var(home_key, fake_home.path());
        if cfg!(windows) {
            std::env::set_var("APPDATA", fake_home.path().join("AppData").join("Roaming"));
            std::env::set_var(
                "LOCALAPPDATA",
                fake_home.path().join("AppData").join("Local"),
            );
        } else {
            std::env::set_var("XDG_CONFIG_HOME", fake_home.path().join("config"));
            std::env::set_var("XDG_STATE_HOME", fake_home.path().join("state"));
            std::env::set_var("XDG_CACHE_HOME", fake_home.path().join("cache"));
        }

        let roots = crate::paths::StorageRoots::resolve().unwrap();
        let roots_json = serde_json::json!({
            "layoutVersion": 1,
            "roots": [
                { "name": "portable", "path": roots.portable },
                { "name": "config", "path": roots.config },
                { "name": "state", "path": roots.state },
                { "name": "cache", "path": roots.cache }
            ]
        });
        let sentinel = fake_home.path().join(".ccync").join("migration.json");
        let state_layout = roots.state.join("layout.json");
        fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
        fs::create_dir_all(state_layout.parent().unwrap()).unwrap();
        let serialized = serde_json::to_vec(&roots_json).unwrap();
        fs::write(&sentinel, &serialized).unwrap();
        fs::write(&state_layout, &serialized).unwrap();

        let config_path = CcyncConfig::config_path().unwrap();
        assert_eq!(config_path, roots.config.join("config.json"));
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let committed_bytes = br#"{"devMode":true,"ccyncRoot":"committed-root"}"#;
        fs::write(&config_path, committed_bytes).unwrap();
        let config = CcyncConfig::load();
        assert_eq!(config.dev_mode, Some(true));
        assert_eq!(config.ccync_root.as_deref(), Some("committed-root"));

        for (key, value) in prior {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }

        assert_eq!(fs::read(config_path).unwrap(), committed_bytes);
    }

    #[test]
    fn test_load_missing_file() {
        let config = CcyncConfig::load_from_path(Path::new("/nonexistent/config.json"));
        assert!(!config.is_dev_mode());
        assert_eq!(config.ccync_root(), None);
    }

    #[test]
    fn test_load_invalid_json() {
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(temp, "{{ invalid json").unwrap();
        temp.flush().unwrap();

        let config = CcyncConfig::load_from_path(temp.path());
        assert!(!config.is_dev_mode());
        assert_eq!(config.ccync_root(), None);
    }

    #[test]
    fn test_load_normal_mode() {
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(temp, r#"{{"devMode": false}}"#).unwrap();
        temp.flush().unwrap();

        let config = CcyncConfig::load_from_path(temp.path());
        assert!(!config.is_dev_mode());
        assert_eq!(config.ccync_root(), None);
    }

    #[test]
    fn test_load_dev_mode_with_root() {
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(temp, r#"{{"devMode": true, "ccyncRoot": "/path/to/repo"}}"#).unwrap();
        temp.flush().unwrap();

        let config = CcyncConfig::load_from_path(temp.path());
        assert!(config.is_dev_mode());
        assert_eq!(config.ccync_root(), Some("/path/to/repo"));
    }

    #[test]
    fn test_load_empty_object() {
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(temp, "{{}}").unwrap();
        temp.flush().unwrap();

        let config = CcyncConfig::load_from_path(temp.path());
        assert!(!config.is_dev_mode());
        assert_eq!(config.ccync_root(), None);
    }

    #[test]
    fn test_deprecated_install_mode() {
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(
            temp,
            r#"{{"devMode": true, "ccyncRoot": "/path", "installMode": "dev"}}"#
        )
        .unwrap();
        temp.flush().unwrap();

        let config = CcyncConfig::load_from_path(temp.path());
        assert!(config.is_dev_mode());
        assert!(config.has_deprecated_install_mode());
    }

    // ── MACHINE_PATH_REGISTRY + machine_path_value ───────────────────────────

    #[test]
    fn registry_contains_only_generic_path_keys() {
        let keys: Vec<&str> = MACHINE_PATH_REGISTRY.iter().map(|(cp, _)| *cp).collect();
        assert!(keys.contains(&"ccyncSkills"));
        assert!(keys.contains(&"tempDir"));
    }

    #[test]
    fn registry_has_no_non_generic_keys() {
        // Regression guard: the registry stays minimal — only generic path
        // keys, no domain-specific keys leaking back in.
        let allowed: std::collections::HashSet<&str> =
            ["ccyncSkills", "tempDir"].into_iter().collect();
        for (cp, _snake) in MACHINE_PATH_REGISTRY {
            assert!(
                allowed.contains(cp),
                "unexpected non-generic key in registry: {cp}"
            );
        }
    }

    #[test]
    fn registry_excludes_mcp_filesystem_paths() {
        for (_, snake) in MACHINE_PATH_REGISTRY {
            assert_ne!(*snake, "MCP_FILESYSTEM_PATHS");
        }
    }

    #[test]
    fn registry_excludes_credential_keys() {
        for (_, snake) in MACHINE_PATH_REGISTRY {
            assert!(
                !snake.ends_with("_API_KEY") && !snake.ends_with("_TOKEN"),
                "credential key found in registry: {snake}"
            );
        }
    }

    #[test]
    fn machine_path_value_flat_key() {
        let config = serde_json::json!({ "ccyncSkills": "/home/user/.ccync/skills" });
        assert_eq!(
            machine_path_value(&config, "ccyncSkills"),
            Some("/home/user/.ccync/skills".to_owned())
        );
    }

    #[test]
    fn machine_path_value_nested_key() {
        let config = serde_json::json!({
            "custom": { "path": "/Users/user/Documents/Data" }
        });
        assert_eq!(
            machine_path_value(&config, "custom.path"),
            Some("/Users/user/Documents/Data".to_owned())
        );
    }

    #[test]
    fn machine_path_value_missing_key_returns_none() {
        let config = serde_json::json!({ "custom": {} });
        assert_eq!(machine_path_value(&config, "custom.path"), None);
    }

    #[test]
    fn machine_path_value_missing_parent_returns_none() {
        let config = serde_json::json!({});
        assert_eq!(machine_path_value(&config, "custom.path"), None);
    }

    #[test]
    fn machine_path_value_non_string_returns_none() {
        let config = serde_json::json!({ "custom": { "path": 42 } });
        assert_eq!(machine_path_value(&config, "custom.path"), None);
    }

    // ── selectedRuntimes ─────────────────────────────────────────────────────

    #[test]
    fn runtime_fields_missing_deserialize_to_defaults() {
        // Old config without runtime fields — must deserialize cleanly.
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(temp, r#"{{"devMode": false}}"#).unwrap();
        temp.flush().unwrap();

        let config = CcyncConfig::load_from_path(temp.path());
        assert!(config.selected_runtimes.is_empty());
    }

    #[test]
    fn runtime_fields_roundtrip() {
        // A legacy `primaryRuntime` key (no longer modeled by CcyncConfig)
        // must be silently ignored rather than breaking deserialization.
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(
            temp,
            r#"{{"selectedRuntimes":["claude","codex"],"primaryRuntime":"claude"}}"#
        )
        .unwrap();
        temp.flush().unwrap();

        let config = CcyncConfig::load_from_path(temp.path());
        assert_eq!(config.selected_runtimes, vec!["claude", "codex"]);
    }

    #[test]
    fn empty_selected_runtimes_is_omitted_on_serialize() {
        let config = CcyncConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            !json.contains("selectedRuntimes"),
            "empty vec should be omitted: {json}"
        );
        assert!(
            !json.contains("primaryRuntime"),
            "primaryRuntime must never appear in current serialization: {json}"
        );
        assert!(
            !json.contains("selectedProjectionTargets"),
            "empty vec should be omitted: {json}"
        );
    }

    // ── selectedProjectionTargets / resolve_projection_targets ──────────────

    #[test]
    fn resolve_projection_targets_canonical_only_returns_canonical_order() {
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(
            temp,
            r#"{{"selectedProjectionTargets":["codex","claude"]}}"#
        )
        .unwrap();
        temp.flush().unwrap();

        let config = CcyncConfig::load_from_path(temp.path());
        let res = config.resolve_projection_targets();

        assert_eq!(res.state, LegacyState::Canonical);
        assert_eq!(
            res.targets,
            vec![ProjectionTargetId::Codex, ProjectionTargetId::Claude]
        );
        assert!(res.rejected.is_empty());
    }

    #[test]
    fn resolve_projection_targets_legacy_only_returns_legacy_order() {
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(
            temp,
            r#"{{"selectedRuntimes":["opencode","copilot","claude"]}}"#
        )
        .unwrap();
        temp.flush().unwrap();

        let config = CcyncConfig::load_from_path(temp.path());
        let res = config.resolve_projection_targets();

        assert_eq!(res.state, LegacyState::Legacy);
        assert_eq!(
            res.targets,
            vec![
                ProjectionTargetId::Opencode,
                ProjectionTargetId::Copilot,
                ProjectionTargetId::Claude
            ]
        );
        assert!(res.rejected.is_empty());
    }

    #[test]
    fn resolve_projection_targets_both_present_canonical_wins() {
        // Canonical and legacy hold genuinely different values/orderings, so
        // a passing test proves canonical truly wins rather than the two
        // fixtures happening to agree.
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(
            temp,
            r#"{{"selectedProjectionTargets":["claude","codex"],"selectedRuntimes":["agy-gui"]}}"#
        )
        .unwrap();
        temp.flush().unwrap();

        let config = CcyncConfig::load_from_path(temp.path());
        let res = config.resolve_projection_targets();

        assert_eq!(res.state, LegacyState::Both);
        assert_eq!(
            res.targets,
            vec![ProjectionTargetId::Claude, ProjectionTargetId::Codex]
        );
        assert!(res.rejected.is_empty());
    }

    #[test]
    fn resolve_projection_targets_absent_returns_empty_default() {
        let config = CcyncConfig::default();
        let res = config.resolve_projection_targets();

        assert_eq!(res.state, LegacyState::Absent);
        assert!(res.targets.is_empty());
        assert!(res.rejected.is_empty());
    }

    #[test]
    fn resolve_projection_targets_reports_unparseable_entries_as_rejected() {
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(
            temp,
            r#"{{"selectedProjectionTargets":["claude","not-a-real-target","codex"]}}"#
        )
        .unwrap();
        temp.flush().unwrap();

        let config = CcyncConfig::load_from_path(temp.path());
        let res = config.resolve_projection_targets();

        assert_eq!(res.state, LegacyState::Canonical);
        assert_eq!(
            res.targets,
            vec![ProjectionTargetId::Claude, ProjectionTargetId::Codex]
        );
        // The unknown entry is surfaced, not silently dropped, so a caller can
        // reproduce install.rs's named-invalid-key warning.
        assert_eq!(res.rejected, vec!["not-a-real-target".to_string()]);
    }

    #[test]
    fn resolve_projection_targets_normalizes_case_and_whitespace() {
        // Hand-edited config with mixed case and padding — the tolerance the
        // legacy install.rs path gave via `.trim().to_ascii_lowercase()`.
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(
            temp,
            r#"{{"selectedProjectionTargets":["Claude"," codex ","AGY-GUI"]}}"#
        )
        .unwrap();
        temp.flush().unwrap();

        let config = CcyncConfig::load_from_path(temp.path());
        let res = config.resolve_projection_targets();

        assert_eq!(res.state, LegacyState::Canonical);
        assert_eq!(
            res.targets,
            vec![
                ProjectionTargetId::Claude,
                ProjectionTargetId::Codex,
                ProjectionTargetId::AgyGui
            ]
        );
        assert!(res.rejected.is_empty());
    }

    #[test]
    fn resolve_projection_targets_dedups_repeated_entries() {
        // Duplicates (including via normalization) collapse to first occurrence,
        // so projection does not run the same target twice.
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(
            temp,
            r#"{{"selectedProjectionTargets":["claude","Claude","codex","claude"]}}"#
        )
        .unwrap();
        temp.flush().unwrap();

        let config = CcyncConfig::load_from_path(temp.path());
        let res = config.resolve_projection_targets();

        assert_eq!(res.state, LegacyState::Canonical);
        assert_eq!(
            res.targets,
            vec![ProjectionTargetId::Claude, ProjectionTargetId::Codex]
        );
        assert!(res.rejected.is_empty());
    }

    #[test]
    fn resolve_projection_targets_skips_whitespace_only_entries() {
        // A whitespace-only entry is silently dropped (empty after trim),
        // matching install.rs's `.filter(|r| !r.is_empty())` — not a rejection.
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(
            temp,
            r#"{{"selectedProjectionTargets":["claude","   ","codex"]}}"#
        )
        .unwrap();
        temp.flush().unwrap();

        let config = CcyncConfig::load_from_path(temp.path());
        let res = config.resolve_projection_targets();

        assert_eq!(res.state, LegacyState::Canonical);
        assert_eq!(
            res.targets,
            vec![ProjectionTargetId::Claude, ProjectionTargetId::Codex]
        );
        assert!(res.rejected.is_empty());
    }

    // ── CcyncConfig tests ──────────────────────────────────────────────────────

    #[test]
    fn test_missing_keys_use_defaults() {
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(temp, r#"{{"ccyncRoot": "/some/path"}}"#).unwrap();
        temp.flush().unwrap();

        let config = CcyncConfig::load_from_path(temp.path());
        // devMode defaults to false when not present
        assert!(!config.is_dev_mode());
        assert_eq!(config.ccync_root(), Some("/some/path"));
    }

    #[test]
    fn test_case_sensitivity() {
        // serde should handle camelCase properly
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(temp, r#"{{"devMode": true, "ccyncRoot": "/test"}}"#).unwrap();
        temp.flush().unwrap();

        let config = CcyncConfig::load_from_path(temp.path());
        assert!(config.is_dev_mode());
        assert_eq!(config.ccync_root(), Some("/test"));
    }

    // ── write_projection_selection_at ────────────────────────────────────────

    #[test]
    fn write_projection_selection_at_writes_canonical_array_in_order() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.json");

        write_projection_selection_at(
            &path,
            &[
                ProjectionTargetId::Codex,
                ProjectionTargetId::Claude,
                ProjectionTargetId::Opencode,
            ],
        )
        .unwrap();

        let map = json_util::read_json_map(&path).unwrap();
        assert_eq!(
            map["selectedProjectionTargets"],
            serde_json::json!(["codex", "claude", "opencode"])
        );
    }

    #[test]
    fn write_projection_selection_at_removes_legacy_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        fs::write(
            &path,
            r#"{"selectedRuntimes":["claude","codex"],"primaryRuntime":"claude","runtimes":["claude"]}"#,
        )
        .unwrap();

        write_projection_selection_at(&path, &[ProjectionTargetId::Claude]).unwrap();

        let map = json_util::read_json_map(&path).unwrap();
        assert!(!map.contains_key("selectedRuntimes"));
        assert!(!map.contains_key("primaryRuntime"));
        assert!(
            !map.contains_key("runtimes"),
            "the old-layout selection spelling must stop propagating too"
        );
    }

    #[test]
    fn write_projection_selection_at_preserves_unknown_and_secrets_subtrees() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        fs::write(
            &path,
            r#"{"devMode":true,"secrets":{"CLAUDE_API_KEY":"sk-abc123","nested":{"x":1}}}"#,
        )
        .unwrap();

        write_projection_selection_at(&path, &[ProjectionTargetId::Claude]).unwrap();

        let map = json_util::read_json_map(&path).unwrap();
        assert_eq!(map["devMode"], serde_json::json!(true));
        assert_eq!(
            map["secrets"],
            serde_json::json!({"CLAUDE_API_KEY":"sk-abc123","nested":{"x":1}})
        );
    }

    #[test]
    fn portable_schema_private_config_fields_roundtrip_without_mutating_input() {
        let input = r#"{"devMode":true,"secrets":{"API_TOKEN":"private"},"origin":{"path":"C:/work"},"nativeSelector":"agent@host","installationId":"machine-1","future":{"value":7}}"#;
        let original: Value = serde_json::from_str(input).unwrap();
        let config: CcyncConfig = serde_json::from_value(original.clone()).unwrap();
        let encoded = serde_json::to_value(config).unwrap();
        for field in [
            "secrets",
            "origin",
            "nativeSelector",
            "installationId",
            "future",
        ] {
            assert_eq!(encoded[field], original[field], "{field}");
        }
        assert_eq!(original["secrets"]["API_TOKEN"], "private");
    }

    #[test]
    fn write_projection_selection_at_creates_file_when_absent() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        assert!(!path.exists());

        write_projection_selection_at(&path, &[ProjectionTargetId::Claude]).unwrap();

        let map = json_util::read_json_map(&path).unwrap();
        assert_eq!(
            map["selectedProjectionTargets"],
            serde_json::json!(["claude"])
        );
        assert_eq!(map.len(), 1);
    }
}
