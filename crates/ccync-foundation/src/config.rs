//! Configuration parsing and management for CCYNC.
//!
//! Rust owns config truth. The config file is `~/.ccync/config/config.json`.
//! Missing file or keys produce explicit defaults; no panics.
//!
//! Mode authority:
//! - Normal mode: `devMode` absent or `false`. Renders from packaged source.
//! - Dev mode: `devMode: true` AND `ccyncRoot` usable. Renders from working tree.
//! - `devMode: true` but `ccyncRoot` unusable → explicit error, no fallback.
//!
//! The `installMode` field is deprecated. If present, it may be used for
//! migration hints but does not override `devMode`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

// ── Machine path registry ─────────────────────────────────────────────────────

/// Maps config.json dot-paths to legacy UPPER_SNAKE env var names.
///
/// Used by readers that migrate from `config.local.env` to `config.json`.
/// The dot-path side is the authoritative config.json key; the UPPER_SNAKE
/// side is the legacy fallback key for `config.local.env`.
///
/// Excludes: `MCP_FILESYSTEM_PATHS` (dead — broken wiring, no consumer),
/// credential keys (`*_API_KEY`, `*_TOKEN`, etc.) which live in
/// `config.json#secrets` and are never path-substituted.
pub const MACHINE_PATH_REGISTRY: &[(&str, &str)] = &[
    ("obsidian.vault", "OBSIDIAN_VAULT"),
    ("obsidian.snippetsDir", "OBSIDIAN_SNIPPETS_DIR"),
    ("obsidian.pluginsDir", "OBSIDIAN_PLUGINS_DIR"),
    ("obsidian.templatesDir", "OBSIDIAN_TEMPLATES_DIR"),
    ("obsidian.attachmentsDir", "OBSIDIAN_ATTACHMENTS_DIR"),
    ("obsidian.journalDir", "OBSIDIAN_JOURNAL_DIR"),
    ("obsidian.canvasDir", "OBSIDIAN_CANVAS_DIR"),
    ("research.localSearchProject", "LOCAL_SEARCH_PROJECT"),
    (
        "research.privateResearchDir",
        "OBSIDIAN_PRIVATE_RESEARCH_DIR",
    ),
    ("ccyncSkills", "CCYNC_SKILLS"),
    ("tempDir", "TEMP_DIR"),
];

/// Look up a machine path value from a `config.json` root `serde_json::Value`.
///
/// `config_path` is a dot-separated key path (e.g. `"obsidian.vault"`).
/// Returns the string value when present, `None` when absent or not a string.
pub fn machine_path_value(config: &Value, config_path: &str) -> Option<String> {
    let mut current = config;
    for segment in config_path.split('.') {
        current = current.get(segment)?;
    }
    current.as_str().map(str::to_owned)
}

/// Check whether the personal local layer is enabled in a raw config value.
///
/// Reads `personalLayer.enabled` from a `config.json` root `serde_json::Value`.
/// Returns `true` only when the key is explicitly the JSON boolean `true`.
/// Missing key, missing parent, non-boolean value, or `false` all return `false`
/// (default-off). Never panics.
pub fn personal_layer_enabled(config: &Value) -> bool {
    let mut current = config;
    for segment in "personalLayer.enabled".split('.') {
        match current.get(segment) {
            Some(v) => current = v,
            None => return false,
        }
    }
    current.as_bool().unwrap_or(false)
}

/// The complete CCYNC configuration model.
///
/// Corresponds to `~/.ccync/config/config.json`.
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
}

impl Default for CcyncConfig {
    /// Default configuration: normal mode (dev_mode = None), no ccyncRoot.
    fn default() -> Self {
        CcyncConfig {
            dev_mode: None,
            ccync_root: None,
            install_mode: None,
        }
    }
}

impl CcyncConfig {
    /// Load configuration from `~/.ccync/config/config.json`.
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

    /// Get the expected path to the config file: `~/.ccync/config/config.json`.
    pub fn config_path() -> io::Result<PathBuf> {
        let home = dirs::home_dir().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "cannot determine home directory")
        })?;

        Ok(home.join(".ccync").join("config").join("config.json"))
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_default_config() {
        let config = CcyncConfig::default();
        assert!(!config.is_dev_mode());
        assert_eq!(config.ccync_root(), None);
        assert!(!config.has_deprecated_install_mode());
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
    fn registry_contains_all_obsidian_seven_paths() {
        let obsidian_keys: Vec<&str> = MACHINE_PATH_REGISTRY
            .iter()
            .filter(|(cp, _)| cp.starts_with("obsidian."))
            .map(|(cp, _)| *cp)
            .collect();
        assert_eq!(
            obsidian_keys.len(),
            7,
            "expected 7 obsidian.* entries, got {obsidian_keys:?}"
        );
    }

    #[test]
    fn registry_contains_research_and_misc_keys() {
        let keys: Vec<&str> = MACHINE_PATH_REGISTRY.iter().map(|(cp, _)| *cp).collect();
        assert!(keys.contains(&"research.localSearchProject"));
        assert!(keys.contains(&"research.privateResearchDir"));
        assert!(keys.contains(&"ccyncSkills"));
        assert!(keys.contains(&"tempDir"));
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
    fn registry_snake_for_local_search_project() {
        let found = MACHINE_PATH_REGISTRY
            .iter()
            .find(|(cp, _)| *cp == "research.localSearchProject");
        assert_eq!(found.map(|(_, s)| *s), Some("LOCAL_SEARCH_PROJECT"));
    }

    #[test]
    fn registry_snake_for_private_research_dir() {
        let found = MACHINE_PATH_REGISTRY
            .iter()
            .find(|(cp, _)| *cp == "research.privateResearchDir");
        assert_eq!(
            found.map(|(_, s)| *s),
            Some("OBSIDIAN_PRIVATE_RESEARCH_DIR")
        );
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
    fn machine_path_value_nested_obsidian_vault() {
        let config = serde_json::json!({
            "obsidian": { "vault": "/Users/user/Documents/Vault" }
        });
        assert_eq!(
            machine_path_value(&config, "obsidian.vault"),
            Some("/Users/user/Documents/Vault".to_owned())
        );
    }

    #[test]
    fn machine_path_value_missing_key_returns_none() {
        let config = serde_json::json!({ "obsidian": {} });
        assert_eq!(machine_path_value(&config, "obsidian.vault"), None);
    }

    #[test]
    fn machine_path_value_missing_parent_returns_none() {
        let config = serde_json::json!({});
        assert_eq!(machine_path_value(&config, "obsidian.vault"), None);
    }

    #[test]
    fn machine_path_value_non_string_returns_none() {
        let config = serde_json::json!({ "obsidian": { "vault": 42 } });
        assert_eq!(machine_path_value(&config, "obsidian.vault"), None);
    }

    // ── personal_layer_enabled ───────────────────────────────────────────────

    #[test]
    fn personal_layer_enabled_missing_key_returns_false() {
        let config = serde_json::json!({});
        assert!(!personal_layer_enabled(&config));
    }

    #[test]
    fn personal_layer_enabled_true_returns_true() {
        let config = serde_json::json!({"personalLayer": {"enabled": true}});
        assert!(personal_layer_enabled(&config));
    }

    #[test]
    fn personal_layer_enabled_false_returns_false() {
        let config = serde_json::json!({"personalLayer": {"enabled": false}});
        assert!(!personal_layer_enabled(&config));
    }

    #[test]
    fn personal_layer_enabled_non_bool_string_returns_false() {
        let config = serde_json::json!({"personalLayer": {"enabled": "yes"}});
        assert!(!personal_layer_enabled(&config));
    }

    #[test]
    fn personal_layer_enabled_non_bool_number_returns_false() {
        let config = serde_json::json!({"personalLayer": {"enabled": 1}});
        assert!(!personal_layer_enabled(&config));
    }

    #[test]
    fn personal_layer_enabled_missing_enabled_subkey_returns_false() {
        let config = serde_json::json!({"personalLayer": {}});
        assert!(!personal_layer_enabled(&config));
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
}
