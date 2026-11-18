//! Shared team roster definitions and parsing.

use serde_json::json;
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

/// Report summarizing the outcome of a roster dump operation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DumpReport {
    pub written: Vec<String>,
    pub skipped: Vec<(String, String)>,
}

/// Check if a URL carries userinfo credentials.
///
/// Judges only `http://`, `https://`, and `git://` schemes. Returns true when
/// the authority component between `://` and the next `/` contains `@`.
pub fn has_url_userinfo(url: &str) -> bool {
    let authority = if let Some(rest) = url.strip_prefix("http://") {
        rest
    } else if let Some(rest) = url.strip_prefix("https://") {
        rest
    } else if let Some(rest) = url.strip_prefix("git://") {
        rest
    } else {
        return false;
    };

    let authority = match authority.find('/') {
        Some(pos) => &authority[..pos],
        None => authority,
    };

    authority.contains('@')
}

/// Dump qualifying catalog plugins to a team roster JSON document.
pub fn dump_roster(catalog: &serde_json::Value, out_path: &Path) -> Result<DumpReport, String> {
    let mut report = DumpReport::default();
    let mut written_plugins = Vec::new();

    if let Some(plugins) = catalog.get("plugins").and_then(|v| v.as_array()) {
        for entry in plugins {
            let plugin_id = entry
                .get("pluginId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            let source = entry
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or_default();

            if !crate::install::is_remote_url(source) {
                report
                    .skipped
                    .push((plugin_id, "source is not a remote URL".to_string()));
                continue;
            }

            if has_url_userinfo(source) {
                report
                    .skipped
                    .push((plugin_id, "source carries URL userinfo".to_string()));
                continue;
            }

            let pin = match ccync_foundation::pins::entry_pin(entry) {
                Some(pin) if is_full_sha(&pin) => pin,
                _ => {
                    report
                        .skipped
                        .push((plugin_id, "no full 40-hex pin recorded".to_string()));
                    continue;
                }
            };

            let display_name = entry
                .get("displayName")
                .and_then(|v| v.as_str())
                .unwrap_or(&plugin_id);

            let source_type = entry
                .get("sourceType")
                .and_then(|v| v.as_str())
                .unwrap_or("curated-upstream");

            let out_entry = json!({
                "pluginId": plugin_id,
                "displayName": display_name,
                "sourceType": source_type,
                "installStrategy": "personal-git-clone",
                "source": source,
                "pin": pin,
            });

            written_plugins.push(out_entry);
            report.written.push(plugin_id);
        }
    }

    let doc = json!({
        "schemaVersion": 1,
        "plugins": written_plugins,
    });

    crate::install::write_json_atomic(out_path, &doc)?;

    Ok(report)
}

/// A single plugin entry declared in a team roster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterEntry {
    pub plugin_id: String,
    pub source: String,
    pub pin: String,
    pub display_name: Option<String>,
}

/// Errors when parsing a team roster JSON structure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RosterParseError {
    NotAnObject,
    MissingPlugins,
    EntryMissingField { index: usize, field: &'static str },
    DuplicatePluginId(String),
    InvalidPin { plugin_id: String, pin: String },
}

impl fmt::Display for RosterParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnObject => write!(f, "roster must be a JSON object"),
            Self::MissingPlugins => write!(f, "roster missing 'plugins' array"),
            Self::EntryMissingField { index, field } => {
                write!(f, "entry at index {index} is missing field '{field}'")
            }
            Self::DuplicatePluginId(id) => write!(f, "duplicate pluginId: {id}"),
            Self::InvalidPin { plugin_id, pin } => {
                write!(f, "invalid pin '{pin}' for plugin '{plugin_id}'")
            }
        }
    }
}

impl std::error::Error for RosterParseError {}

/// Check if a string is a 40-character full hex sha.
pub fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Parse a roster JSON value into a list of [`RosterEntry`].
pub fn parse_roster(value: &serde_json::Value) -> Result<Vec<RosterEntry>, RosterParseError> {
    let Some(map) = value.as_object() else {
        return Err(RosterParseError::NotAnObject);
    };

    let Some(plugins_val) = map.get("plugins") else {
        return Err(RosterParseError::MissingPlugins);
    };

    let Some(plugins_arr) = plugins_val.as_array() else {
        return Err(RosterParseError::MissingPlugins);
    };

    let mut seen_ids = HashSet::new();
    let mut entries = Vec::with_capacity(plugins_arr.len());

    for (index, entry_val) in plugins_arr.iter().enumerate() {
        let Some(entry_obj) = entry_val.as_object() else {
            return Err(RosterParseError::EntryMissingField {
                index,
                field: "pluginId",
            });
        };

        let Some(plugin_id) = entry_obj.get("pluginId").and_then(|v| v.as_str()) else {
            return Err(RosterParseError::EntryMissingField {
                index,
                field: "pluginId",
            });
        };

        let Some(source) = entry_obj.get("source").and_then(|v| v.as_str()) else {
            return Err(RosterParseError::EntryMissingField {
                index,
                field: "source",
            });
        };

        let Some(pin) = entry_obj.get("pin").and_then(|v| v.as_str()) else {
            return Err(RosterParseError::EntryMissingField {
                index,
                field: "pin",
            });
        };

        if !is_full_sha(pin) {
            return Err(RosterParseError::InvalidPin {
                plugin_id: plugin_id.to_string(),
                pin: pin.to_string(),
            });
        }

        if !seen_ids.insert(plugin_id.to_string()) {
            return Err(RosterParseError::DuplicatePluginId(plugin_id.to_string()));
        }

        let display_name = entry_obj
            .get("displayName")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        entries.push(RosterEntry {
            plugin_id: plugin_id.to_string(),
            source: source.to_string(),
            pin: pin.to_string(),
            display_name,
        });
    }

    Ok(entries)
}

/// Outcome of classifying a roster entry against local machine state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    Rejected(String),
    Missing,
    Held {
        current_sha: Option<String>,
        current_source: Option<String>,
        diverged: bool,
    },
    SourceMismatched {
        current_source: String,
        current_sha: Option<String>,
    },
    PinMismatched {
        current_sha: Option<String>,
    },
    Converged,
}

/// Classify a roster entry against personal-catalog local plugin state.
pub fn classify_entry(entry: &RosterEntry, local: Option<&serde_json::Value>) -> Classification {
    if !crate::install::is_remote_url(&entry.source) {
        return Classification::Rejected("source is not a remote URL".to_string());
    }
    if has_url_userinfo(&entry.source) {
        return Classification::Rejected("source carries URL userinfo".to_string());
    }

    let Some(local) = local else {
        return Classification::Missing;
    };

    let is_held = local.get("held").and_then(|v| v.as_bool()).unwrap_or(false);
    if is_held {
        let current_sha = ccync_foundation::pins::entry_pin(local);
        let current_source = local
            .get("source")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let diverged = current_source.as_deref() != Some(entry.source.as_str())
            || current_sha.as_deref() != Some(entry.pin.as_str());
        return Classification::Held {
            current_sha,
            current_source,
            diverged,
        };
    }

    let current_source = local
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if current_source != entry.source {
        return Classification::SourceMismatched {
            current_source: current_source.to_string(),
            current_sha: ccync_foundation::pins::entry_pin(local),
        };
    }

    let current_sha = ccync_foundation::pins::entry_pin(local);
    if current_sha.as_deref() != Some(entry.pin.as_str()) {
        return Classification::PinMismatched { current_sha };
    }

    Classification::Converged
}

/// A roster entry paired with its classification against local state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedEntry {
    pub entry: RosterEntry,
    pub class: Classification,
}

/// Options controlling installation execution of classified entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOptions {
    pub cache_root: PathBuf,
    pub catalog_path: PathBuf,
    pub pin_files: Vec<PathBuf>,
}

/// Report summarizing the outcome of executing roster installation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InstallReport {
    pub added: Vec<String>,
    pub repinned: Vec<(String, Option<String>, String)>,
    pub source_rewrites: Vec<(String, String, String)>,
    pub fetched: Vec<(String, String)>,
    pub failed: Vec<(String, String)>,
    pub untouched: Vec<String>,
}

/// Execute install actions for classified roster entries.
pub fn execute_install(entries: &[ClassifiedEntry], opts: &InstallOptions) -> InstallReport {
    let mut report = InstallReport::default();
    let pin_files: Vec<&Path> = opts.pin_files.iter().map(PathBuf::as_path).collect();

    for classified in entries {
        let entry = &classified.entry;
        match &classified.class {
            Classification::Missing => {
                match crate::install::add_plugin_entry(
                    &opts.cache_root,
                    &opts.catalog_path,
                    &entry.plugin_id,
                    &entry.source,
                    Some(&entry.pin),
                ) {
                    crate::install::AddResult::Added { .. } => {
                        report.added.push(entry.plugin_id.clone());
                        report
                            .fetched
                            .push((entry.plugin_id.clone(), entry.source.clone()));
                    }
                    crate::install::AddResult::Failed(m)
                    | crate::install::AddResult::WriteFailed(m) => {
                        report.failed.push((entry.plugin_id.clone(), m));
                    }
                    crate::install::AddResult::AlreadyPresent => {
                        report.failed.push((
                            entry.plugin_id.clone(),
                            "already present in catalog".to_string(),
                        ));
                    }
                }
            }
            Classification::PinMismatched { current_sha } => {
                let change = crate::plugin_upgrade::UpgradeChange {
                    plugin_id: entry.plugin_id.clone(),
                    source: entry.source.clone(),
                    old_sha: current_sha.clone(),
                    new_sha: entry.pin.clone(),
                };
                let clone = |url: &str, dest: &Path| match crate::install::fetch_personal_plugin_at(
                    &opts.cache_root,
                    &entry.plugin_id,
                    url,
                    Some(&entry.pin),
                ) {
                    crate::install::PersonalFetchResult::Cloned { path, .. }
                    | crate::install::PersonalFetchResult::AlreadyPresent(path)
                        if path == dest =>
                    {
                        Ok(())
                    }
                    crate::install::PersonalFetchResult::Cloned { path, .. }
                    | crate::install::PersonalFetchResult::AlreadyPresent(path) => Err(format!(
                        "landed at {} not {}",
                        path.display(),
                        dest.display()
                    )),
                    crate::install::PersonalFetchResult::CloneFailed(m) => Err(m),
                    crate::install::PersonalFetchResult::NoCacheRoot => Err("no cache root".into()),
                };
                match crate::plugin_upgrade::apply_upgrade(
                    &change,
                    &opts.cache_root,
                    &pin_files,
                    clone,
                ) {
                    Ok(()) => {
                        report.repinned.push((
                            entry.plugin_id.clone(),
                            current_sha.clone(),
                            entry.pin.clone(),
                        ));
                        report
                            .fetched
                            .push((entry.plugin_id.clone(), entry.source.clone()));
                    }
                    Err(e) => {
                        report.failed.push((entry.plugin_id.clone(), e.to_string()));
                    }
                }
            }
            Classification::SourceMismatched {
                current_source,
                current_sha,
            } => {
                let change = crate::plugin_upgrade::UpgradeChange {
                    plugin_id: entry.plugin_id.clone(),
                    source: entry.source.clone(),
                    old_sha: current_sha.clone(),
                    new_sha: entry.pin.clone(),
                };
                let clone = |url: &str, dest: &Path| match crate::install::fetch_personal_plugin_at(
                    &opts.cache_root,
                    &entry.plugin_id,
                    url,
                    Some(&entry.pin),
                ) {
                    crate::install::PersonalFetchResult::Cloned { path, .. }
                    | crate::install::PersonalFetchResult::AlreadyPresent(path)
                        if path == dest =>
                    {
                        Ok(())
                    }
                    crate::install::PersonalFetchResult::Cloned { path, .. }
                    | crate::install::PersonalFetchResult::AlreadyPresent(path) => Err(format!(
                        "landed at {} not {}",
                        path.display(),
                        dest.display()
                    )),
                    crate::install::PersonalFetchResult::CloneFailed(m) => Err(m),
                    crate::install::PersonalFetchResult::NoCacheRoot => Err("no cache root".into()),
                };
                match crate::plugin_upgrade::apply_upgrade_rewriting_source(
                    &change,
                    &opts.cache_root,
                    &pin_files,
                    clone,
                ) {
                    Ok(()) => {
                        report.source_rewrites.push((
                            entry.plugin_id.clone(),
                            current_source.clone(),
                            entry.source.clone(),
                        ));
                        report.repinned.push((
                            entry.plugin_id.clone(),
                            current_sha.clone(),
                            entry.pin.clone(),
                        ));
                        report
                            .fetched
                            .push((entry.plugin_id.clone(), entry.source.clone()));
                    }
                    Err(e) => {
                        report.failed.push((entry.plugin_id.clone(), e.to_string()));
                    }
                }
            }
            Classification::Held { .. } | Classification::Converged => {
                report.untouched.push(entry.plugin_id.clone());
            }
            Classification::Rejected(_) => {
                report.failed.push((
                    entry.plugin_id.clone(),
                    "rejected entry reached executor".to_string(),
                ));
            }
        }
    }

    report
}
