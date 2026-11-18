//! Setup-domain health check.
//!
//! Read-only. Aggregated into `ccync doctor` and also exercised directly by unit tests.

use ccync_foundation::health::{DoctorFinding, HealthCheck};
use ccync_foundation::secret::secret_re;
use std::path::PathBuf;

/// Health over the machine-setup surface.
pub struct SetupHealthCheck {
    /// CCYNC repo / source root used to evaluate setup-owned surfaces.
    pub repo_root: PathBuf,
}

impl HealthCheck for SetupHealthCheck {
    fn name(&self) -> &str {
        "setup"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        let mut findings = Vec::new();

        let layout = match ccync_foundation::paths::EffectiveLayout::load_or_legacy() {
            Ok(layout) => {
                check_layout_health(&layout, &mut findings);
                Some(layout)
            }
            Err(error) => {
                findings.push(DoctorFinding::error(
                    format!("cannot inspect effective storage layout: {error}"),
                    "check the configured storage roots, then retry `ccync doctor`",
                ));
                None
            }
        };

        scan_manifests_for_secrets(&self.repo_root, &mut findings);

        if which("rg").is_none() {
            findings.push(DoctorFinding::warning(
                "ripgrep (rg) not found on PATH — ccync search-dependent flows degrade",
            ));
        }

        if let Some(layout) = &layout {
            check_mcp_host_drift(layout, &mut findings);
            check_provider_outputs(layout, &mut findings);
            check_personal_plugins(layout, &mut findings);
        }

        findings
    }
}

/// Check provider generations referenced by committed native ownership records.
/// Compatibility umbrella links and shared skill copies are deliberately not
/// part of this contract because targets can load their native provider output.
fn check_layout_health(
    layout: &ccync_foundation::paths::EffectiveLayout,
    findings: &mut Vec<DoctorFinding>,
) {
    use ccync_foundation::paths::EffectiveLayoutAuthority;

    let effective_root = layout.state_base();
    if layout.authority() == EffectiveLayoutAuthority::MutationBlocked {
        findings.push(DoctorFinding::error(
            format!(
                "storage layout authority is MutationBlocked; the layout marker and committed state may not match; effective root: {}",
                effective_root.display()
            ),
            "check the configured storage roots and marker state, then retry `ccync doctor`; do not delete layout state",
        ));
    }
    if let Some(error) = layout.root_error() {
        findings.push(DoctorFinding::error(
            format!(
                "storage root is invalid: {error}; effective root: {}",
                effective_root.display()
            ),
            "correct the configured storage roots, then retry `ccync doctor`",
        ));
    }

    let migration_path = layout.migration_journal_path();
    if migration_path.exists()
        && layout.authority() != EffectiveLayoutAuthority::Committed
    {
        findings.push(DoctorFinding::error(
            format!(
                "uncommitted storage migration journal exists at {}; effective root: {}",
                migration_path.display(),
                effective_root.display()
            ),
            "retry the storage migration after checking the configured roots; do not delete the journal",
        ));
    }

    let config_path = layout.machine_config_path();
    if config_path.exists() {
        if let Err(error) =
            ccync_foundation::config::CcyncConfig::load_strict_from_path(&config_path)
        {
            findings.push(DoctorFinding::error(
                format!(
                    "machine config is invalid or unreadable at {}: {error}",
                    config_path.display()
                ),
                "check permissions and JSON syntax, then retry `ccync doctor`",
            ));
        }
    }
}

fn check_provider_outputs(
    layout: &ccync_foundation::paths::EffectiveLayout,
    findings: &mut Vec<DoctorFinding>,
) {
    let lock_path = layout.build_lock_path();
    let Some(provider_root) = ccync_foundation::paths::provider_render_root() else {
        return;
    };
    check_provider_outputs_with_paths(&lock_path, &provider_root, findings);
}

fn check_provider_outputs_with_paths(
    lock_path: &std::path::Path,
    provider_root: &std::path::Path,
    findings: &mut Vec<DoctorFinding>,
) {
    use ccync_foundation::provider::ProviderLifecycleLock;

    let Ok(contents) = std::fs::read_to_string(lock_path) else {
        return;
    };
    let Ok(lock) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return;
    };
    let Ok(lifecycle) = ProviderLifecycleLock::from_lock(&lock) else {
        findings.push(DoctorFinding::warning(
            "provider lifecycle evidence is invalid; owned provider outputs could not be checked",
        ));
        return;
    };

    for ownership in lifecycle.committed {
        let generation = &ownership.generation;
        let source_root = provider_root.join(generation.source.as_str());
        let expected_pattern = format!(
            "{}/*.views/{}/{}",
            source_root.display(),
            ownership.target,
            generation.generation
        );
        let has_view = std::fs::read_dir(&source_root)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .any(|views_root| {
                views_root
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.ends_with(".views"))
                    .unwrap_or(false)
                    && views_root
                        .join(ownership.target.as_str())
                        .join(generation.generation.as_str())
                        .is_dir()
            });
        if !has_view {
            findings.push(DoctorFinding::warning(format!(
                "owned provider output missing: '{}' for {}; expected view at {}",
                ownership.plugin_name,
                ownership.target,
                expected_pattern
            )));
        }
    }
}

/// Report installed personal plugins.
///
/// Emits a warning-severity finding only for personal plugins that truly have
/// no pin recorded in the lockfile. Pinned plugins are healthy and produce no
/// finding — doctor's vocabulary is problem-only.
/// Absent catalog or empty catalog → no finding.
fn check_personal_plugins(
    layout: &ccync_foundation::paths::EffectiveLayout,
    findings: &mut Vec<DoctorFinding>,
) {
    let catalog_path = layout.personal_catalog_path();
    if !catalog_path.exists() {
        return;
    }
    let catalog_str = match std::fs::read_to_string(&catalog_path) {
        Ok(s) => s,
        Err(_) => return,
    };
    let catalog: serde_json::Value = match serde_json::from_str(&catalog_str) {
        Ok(v) => v,
        Err(_) => return,
    };
    let plugins = match catalog.get("plugins").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return,
    };
    if plugins.is_empty() {
        return;
    }

    // Read lockfile for pinned sha.
    let lock_entries = personal_plugin_lock_entries(&layout.build_lock_path());

    for entry in plugins {
        if let Some(finding) = personal_plugin_finding(entry, &lock_entries) {
            findings.push(finding);
        }
    }
}

/// Pure per-entry seam for [`check_personal_plugins`]: resolves the catalog
/// entry's pin via the shared lockfile-entry pin reader and returns `None`
/// when pinned (healthy, silent) or one warning when truly unpinned.
fn personal_plugin_finding(
    catalog_entry: &serde_json::Value,
    lock_entries: &[serde_json::Value],
) -> Option<DoctorFinding> {
    let id = catalog_entry
        .get("pluginId")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let source = catalog_entry
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let pin = lock_entries
        .iter()
        .find(|e| e.get("pluginId").and_then(|v| v.as_str()) == Some(id))
        .and_then(ccync_foundation::pins::entry_pin);

    if pin.is_some() {
        return None;
    }
    Some(DoctorFinding::warning(format!(
        "personal plugin: '{id}' @ (unpinned) — {source}"
    )))
}

/// Read `_personalPlugins` entries from the lockfile. Returns empty vec on any error.
fn personal_plugin_lock_entries(lock_path: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(lock_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            v.get("_personalPlugins")
                .and_then(|a| a.as_array())
                .map(|a| a.to_vec())
        })
        .unwrap_or_default()
}

/// Hosts whose committed MCP ownership is evaluated. The set is fixed: the
/// source family and component selection never narrow the drift check.
const MCP_HOSTS: [&str; 5] = ["agy", "claude", "codex", "copilot", "opencode"];

/// Report MCP drift across the five hosts from ccync's committed MCP ownership.
///
/// Reads only `build/mcp/projected-state.json`, the per-host ownership record.
/// A name committed for one host but absent from another host's record is drift
/// on that host. No host configuration is parsed. No record → no findings.
fn check_mcp_host_drift(
    layout: &ccync_foundation::paths::EffectiveLayout,
    findings: &mut Vec<DoctorFinding>,
) {
    let state_path = layout.state_base().join("mcp").join("projected-state.json");
    check_mcp_host_drift_with_path(&state_path, findings);
}

/// Path-injectable core of [`check_mcp_host_drift`]. Unknown host keys and
/// non-string entries are ignored and never rewritten, so their content survives.
fn check_mcp_host_drift_with_path(state_path: &std::path::Path, findings: &mut Vec<DoctorFinding>) {
    use serde_json::Value as JsonValue;
    use std::collections::BTreeSet;

    let Ok(text) = std::fs::read_to_string(state_path) else {
        return;
    };
    let Ok(state) = serde_json::from_str::<JsonValue>(&text) else {
        return;
    };
    let Some(hosts) = state.get("hosts").and_then(|v| v.as_object()) else {
        return;
    };

    // A missing or malformed host record counts as committing nothing.
    let committed: Vec<(&str, BTreeSet<&str>)> = MCP_HOSTS
        .iter()
        .map(|&host| {
            let names: BTreeSet<&str> = hosts
                .get(host)
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str())
                .collect();
            (host, names)
        })
        .collect();

    let desired: BTreeSet<&str> = committed
        .iter()
        .flat_map(|(_, names)| names.iter().copied())
        .collect();
    for (host, names) in &committed {
        for name in desired.difference(names) {
            findings.push(drift_finding(name, host));
        }
    }
}

fn drift_finding(name: &str, host: &str) -> DoctorFinding {
    DoctorFinding::warning(format!(
        "MCP drift: '{name}' is committed for another host but not for '{host}' — run `ccync sync` to reconcile"
    ))
}

/// Scan `plugin.json` manifests under `repo_root/plugins/` for accidentally leaked
/// secret-pattern values. `${VAR}` placeholder tokens are excluded from the scan
/// (they are intentional substitution targets, not actual credentials).
fn scan_manifests_for_secrets(repo_root: &std::path::Path, findings: &mut Vec<DoctorFinding>) {
    let pattern = secret_re();
    let plugins_dir = repo_root.join("plugins");
    if !plugins_dir.is_dir() {
        return;
    }
    for entry in walkdir_plugin_jsons(&plugins_dir) {
        let Ok(content) = std::fs::read_to_string(&entry) else {
            continue;
        };
        for line in content.lines() {
            // Strip `${...}` placeholders before checking; they're intentional.
            let stripped = strip_placeholders(line);
            if pattern.is_match(&stripped) {
                findings.push(DoctorFinding::error(
                    format!(
                        "manifest may contain a leaked secret: {} — line: {}",
                        entry.display(),
                        line.trim()
                    ),
                    "remove the credential from the manifest and rotate the affected key",
                ));
                break; // one finding per file is sufficient
            }
        }
    }
}

fn walkdir_plugin_jsons(plugins_dir: &std::path::Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    collect_plugin_jsons(plugins_dir, &mut result);
    result
}

fn collect_plugin_jsons(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_plugin_jsons(&path, out);
        } else if path
            .file_name()
            .map(|n| n == "plugin.json")
            .unwrap_or(false)
        {
            out.push(path);
        }
    }
}

fn strip_placeholders(s: &str) -> String {
    // Replace ${UPPER_SNAKE} tokens with empty string so they don't trigger the scan.
    let mut result = s.to_string();
    while let Some(start) = result.find("${") {
        if let Some(end) = result[start..].find('}') {
            result.replace_range(start..start + end + 1, "");
        } else {
            break;
        }
    }
    result
}

/// Check whether the Claude plugin cache for CCYNC is stale relative to the
/// canonical root. CCYNC never writes the cache; Claude Code manages it.
///
/// Produces a non-blocking `Warning` when the cache directory is older
/// (by mtime) than the canonical root, suggesting the user run `ccync sync`
/// to let Claude Code refresh its cached copy.
///
/// No finding when:
/// - the cache directory does not exist (ccync may not be projected to Claude)
/// - the canonical root does not exist (ccync not yet synced)
/// - either mtime is unavailable
pub struct ClaudePluginCacheCheck {
    /// `~/.ccync/build/render/` — the canonical root written by `ccync sync`.
    pub canonical_root: PathBuf,
    /// `~/.claude/plugins/cache/ccync/` — the Claude Code managed plugin cache.
    /// Resolved from user home when `None`.
    pub cache_dir: Option<PathBuf>,
}

impl ClaudePluginCacheCheck {
    /// Resolve both paths from the real user home.
    pub fn from_user_home(canonical_root: PathBuf) -> Self {
        let cache_dir = ccync_foundation::paths::user_home().map(|h| {
            h.join(".claude")
                .join("plugins")
                .join("cache")
                .join("ccync")
        });
        Self {
            canonical_root,
            cache_dir,
        }
    }
}

impl HealthCheck for ClaudePluginCacheCheck {
    fn name(&self) -> &str {
        "claude-plugin-cache"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        let mut findings = Vec::new();

        let cache_dir = match &self.cache_dir {
            Some(p) => p.clone(),
            None => match ccync_foundation::paths::user_home() {
                Some(h) => h
                    .join(".claude")
                    .join("plugins")
                    .join("cache")
                    .join("ccync"),
                None => return findings,
            },
        };

        // Only check when both dirs exist.
        if !cache_dir.is_dir() || !self.canonical_root.is_dir() {
            return findings;
        }

        let cache_mtime = std::fs::metadata(&cache_dir)
            .ok()
            .and_then(|m| m.modified().ok());
        let canon_mtime = std::fs::metadata(&self.canonical_root)
            .ok()
            .and_then(|m| m.modified().ok());

        if let (Some(cache_t), Some(canon_t)) = (cache_mtime, canon_mtime) {
            if cache_is_stale(cache_t, canon_t) {
                findings.push(DoctorFinding::warning(
                    "Claude plugin cache for ccync is stale (canonical root is newer) — \
                     run `ccync sync` so Claude Code can refresh its cached copy",
                ));
            }
        }

        findings
    }
}

/// Pure staleness predicate: the cache is stale when it is strictly older than
/// the canonical root. Equal mtimes (low-resolution filesystems) are treated as
/// not-stale so the advisory never false-positives on a tie.
fn cache_is_stale(cache_mtime: std::time::SystemTime, canon_mtime: std::time::SystemTime) -> bool {
    cache_mtime < canon_mtime
}

fn which(binary: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let extensions: Vec<String> = if cfg!(windows) {
        vec![".exe".into(), ".cmd".into(), ".bat".into(), String::new()]
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path_var) {
        for ext in &extensions {
            let candidate = dir.join(format!("{binary}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvRestore {
        values: [(&'static str, Option<std::ffi::OsString>); 7],
        _lock: MutexGuard<'static, ()>,
    }

    impl EnvRestore {
        fn with_storage_roots(home: &std::path::Path) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
            let keys = [
                "USERPROFILE",
                "HOME",
                "APPDATA",
                "LOCALAPPDATA",
                "XDG_CONFIG_HOME",
                "XDG_STATE_HOME",
                "XDG_CACHE_HOME",
            ];
            let values = keys.map(|key| (key, std::env::var_os(key)));
            unsafe {
                std::env::set_var("USERPROFILE", home);
                std::env::set_var("HOME", home);
                std::env::set_var("APPDATA", home.join("appdata"));
                std::env::set_var("LOCALAPPDATA", home.join("localappdata"));
                std::env::set_var("XDG_CONFIG_HOME", home.join("xdg-config"));
                std::env::set_var("XDG_STATE_HOME", home.join("xdg-state"));
                std::env::set_var("XDG_CACHE_HOME", home.join("xdg-cache"));
            }
            Self {
                values,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (key, value) in &self.values {
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    #[test]
    fn missing_legacy_script_no_longer_flags_after_r05_cutover() {
        let temp = tempfile::tempdir().unwrap();
        let check = SetupHealthCheck {
            repo_root: temp.path().to_path_buf(),
        };
        let findings = check.check();
        assert!(
            !findings
                .iter()
                .any(|f| f.message.contains("legacy install script missing")),
            "legacy install script should no longer be a health requirement: {findings:?}"
        );
    }

    #[test]
    fn layout_health_reports_blocked_authority_and_migration_without_writes() {
        let temp = tempfile::tempdir().unwrap();
        let _env = EnvRestore::with_storage_roots(temp.path());
        let legacy_marker = temp.path().join(".ccync").join("migration.json");
        std::fs::create_dir_all(legacy_marker.parent().unwrap()).unwrap();
        std::fs::write(&legacy_marker, r#"{"layoutVersion":1,"roots":[]}"#).unwrap();

        let layout = ccync_foundation::paths::EffectiveLayout::load_or_legacy().unwrap();
        let layout_state = layout.layout_path();
        std::fs::create_dir_all(layout_state.parent().unwrap()).unwrap();
        std::fs::write(&layout_state, r#"{"layoutVersion":2,"roots":[]}"#).unwrap();
        let migration = layout.migration_journal_path();
        std::fs::write(&migration, r#"{"status":"pending"}"#).unwrap();
        let before = [
            (
                legacy_marker.clone(),
                std::fs::read(&legacy_marker).unwrap(),
            ),
            (layout_state.clone(), std::fs::read(&layout_state).unwrap()),
            (migration.clone(), std::fs::read(&migration).unwrap()),
        ];

        let loaded = ccync_foundation::paths::EffectiveLayout::load_or_legacy().unwrap();
        let mut findings = Vec::new();
        check_layout_health(&loaded, &mut findings);

        let messages = findings
            .iter()
            .map(|finding| {
                format!(
                    "{} {}",
                    finding.message,
                    finding.fix_hint.as_deref().unwrap_or_default()
                )
            })
            .collect::<Vec<_>>();
        assert!(
            messages.iter().any(|message| {
                message.contains("MutationBlocked")
                    && message.contains(&loaded.state_base().display().to_string())
                    && message.contains("retry `ccync doctor`")
            }),
            "blocked authority should name the effective root and safe retry: {messages:?}"
        );
        assert!(
            messages.iter().any(|message| {
                message.contains("migration journal")
                    && message.contains(&migration.display().to_string())
                    && message.contains(&loaded.state_base().display().to_string())
                    && message.contains("do not delete")
            }),
            "migration presence should name its path, effective root, and safe retry: {messages:?}"
        );
        for (path, contents) in before {
            assert_eq!(std::fs::read(path).unwrap(), contents);
        }
    }

    #[test]
    fn layout_health_accepts_committed_layout_with_prepared_journal() {
        let temp = tempfile::tempdir().unwrap();
        let _env = EnvRestore::with_storage_roots(temp.path());
        let initial = ccync_foundation::paths::EffectiveLayout::load_or_legacy().unwrap();
        let roots = initial.storage_roots();
        let commit = serde_json::json!({
            "layoutVersion": 1,
            "roots": [
                {"name": "portable", "path": roots.portable},
                {"name": "config", "path": roots.config},
                {"name": "state", "path": roots.state},
                {"name": "cache", "path": roots.cache}
            ]
        });
        let commit_text = serde_json::to_string(&commit).unwrap();
        for path in [initial.layout_path(), initial.migration_sentinel_path()] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, &commit_text).unwrap();
        }
        let migration = initial.migration_journal_path();
        std::fs::create_dir_all(migration.parent().unwrap()).unwrap();
        std::fs::write(&migration, r#"{"status":"prepared"}"#).unwrap();

        let layout = ccync_foundation::paths::EffectiveLayout::load_or_legacy().unwrap();
        assert_eq!(
            layout.authority(),
            ccync_foundation::paths::EffectiveLayoutAuthority::Committed
        );
        let mut findings = Vec::new();
        check_layout_health(&layout, &mut findings);

        assert!(
            findings
                .iter()
                .all(|finding| !finding.message.contains("migration journal")),
            "committed prepared journal should not be reported as uncommitted: {findings:?}"
        );
    }

    #[test]
    fn provider_health_accepts_committed_native_view_without_compatibility_links() {
        let temp = tempfile::tempdir().unwrap();
        let lock_path = temp.path().join("lock.json");
        let provider_root = temp.path().join("providers");
        std::fs::create_dir_all(
            provider_root
                .join("probe")
                .join("plugin.views")
                .join("codex")
                .join("v-abc"),
        )
        .unwrap();
        std::fs::write(&lock_path, provider_lock_fixture()).unwrap();

        let mut findings = Vec::new();
        check_provider_outputs_with_paths(&lock_path, &provider_root, &mut findings);

        assert!(
            findings.is_empty(),
            "valid native generation should pass: {findings:?}"
        );
    }

    #[test]
    fn provider_health_reports_missing_committed_output() {
        let temp = tempfile::tempdir().unwrap();
        let lock_path = temp.path().join("lock.json");
        let provider_root = temp.path().join("providers");
        std::fs::create_dir_all(&provider_root).unwrap();
        std::fs::write(&lock_path, provider_lock_fixture()).unwrap();

        let mut findings = Vec::new();
        check_provider_outputs_with_paths(&lock_path, &provider_root, &mut findings);

        assert_eq!(findings.len(), 1);
        assert!(findings[0]
            .message
            .contains("owned provider output missing"));
        assert!(findings[0].message.contains("probe-provider"));
        assert!(findings[0].message.contains("codex"));
        assert!(findings[0].message.contains("probe/*.views/codex/v-abc"));
    }

    #[test]
    fn provider_health_reports_invalid_lifecycle_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let lock_path = temp.path().join("lock.json");
        let provider_root = temp.path().join("providers");
        std::fs::write(&lock_path, r#"{"_providerLifecycle":{"schemaVersion":1,"committed":"invalid"}}"#).unwrap();

        let mut findings = Vec::new();
        check_provider_outputs_with_paths(&lock_path, &provider_root, &mut findings);

        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("lifecycle evidence is invalid"));
    }

    fn provider_lock_fixture() -> &'static str {
        r#"{"_providerLifecycle":{"schemaVersion":1,"committed":[{
            "target":"codex","selector":"probe@marketplace","pluginName":"probe-provider",
            "generation":{"source":"probe","generation":"v-abc","packageDigest":"sha256:abc"},
            "capabilities":[]
        }]}}"#
    }

    #[test]
    fn name_is_setup() {
        let check = SetupHealthCheck {
            repo_root: PathBuf::from("."),
        };
        assert_eq!(check.name(), "setup");
    }

    #[test]
    fn manifest_secret_scan_flags_leaked_credential() {
        let temp = tempfile::tempdir().unwrap();
        let plugin_dir = temp
            .path()
            .join("plugins")
            .join("test-plugin")
            .join(".claude-plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("plugin.json"),
            r#"{"name":"test","description":"GITHUB_TOKEN=ghp_XXXXXXXXXXXXXXXX leaked here"}"#,
        )
        .unwrap();
        let mut findings = Vec::new();
        scan_manifests_for_secrets(temp.path(), &mut findings);
        assert!(
            findings.iter().any(|f| f.message.contains("leaked secret")),
            "should flag manifest with leaked credential: {findings:?}"
        );
    }

    #[test]
    fn manifest_secret_scan_allows_placeholder_tokens() {
        let temp = tempfile::tempdir().unwrap();
        let plugin_dir = temp
            .path()
            .join("plugins")
            .join("test-plugin")
            .join(".claude-plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("plugin.json"),
            r#"{"name":"test","description":"Use ${GITHUB_TOKEN} for auth"}"#,
        )
        .unwrap();
        let mut findings = Vec::new();
        scan_manifests_for_secrets(temp.path(), &mut findings);
        assert!(
            !findings.iter().any(|f| f.message.contains("leaked secret")),
            "should NOT flag placeholder as a leaked credential: {findings:?}"
        );
    }

    #[test]
    fn manifest_secret_scan_clean_manifest_no_findings() {
        let temp = tempfile::tempdir().unwrap();
        let plugin_dir = temp
            .path()
            .join("plugins")
            .join("ccync-core")
            .join(".claude-plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("plugin.json"),
            r#"{"name":"ccync","displayName":"CCYNC","version":"0.1.0","sourceType":"official-ccync-bundle"}"#,
        )
        .unwrap();
        let mut findings = Vec::new();
        scan_manifests_for_secrets(temp.path(), &mut findings);
        assert!(
            findings.is_empty(),
            "clean manifest should produce no findings: {findings:?}"
        );
    }

    // ─── Claude plugin cache ─────────────────────────────────────────────────

    #[test]
    fn claude_plugin_cache_check_no_finding_when_cache_absent() {
        let temp = tempfile::tempdir().unwrap();
        let canonical_root = temp.path().join("canonical");
        std::fs::create_dir_all(&canonical_root).unwrap();
        // Provide a non-existent cache dir so the check can't see the real machine cache.
        let check = ClaudePluginCacheCheck {
            canonical_root,
            cache_dir: Some(temp.path().join("no-such-cache")),
        };
        let findings = check.check();
        assert!(
            !findings.iter().any(|f| f.message.contains("stale")),
            "absent cache dir must not produce a stale finding: {findings:?}"
        );
    }

    #[test]
    fn claude_plugin_cache_check_no_finding_when_canonical_absent() {
        let temp = tempfile::tempdir().unwrap();
        let cache_dir = temp.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let check = ClaudePluginCacheCheck {
            canonical_root: temp.path().join("not-installed"),
            cache_dir: Some(cache_dir),
        };
        let findings = check.check();
        assert!(
            findings.is_empty(),
            "absent canonical root must produce no finding: {findings:?}"
        );
    }

    #[test]
    fn claude_plugin_cache_check_warns_when_cache_stale() {
        // Deterministic positive-path oracle: an older cache mtime than the
        // canonical root mtime is stale; a newer cache is not; a tie is not.
        // Asserting the pure predicate avoids depending on cross-filesystem
        // directory-mtime resolution (which made the prior integration form vacuous).
        use std::time::{Duration, SystemTime};
        let older = SystemTime::UNIX_EPOCH;
        let newer = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        assert!(cache_is_stale(older, newer), "older cache must be stale");
        assert!(
            !cache_is_stale(newer, older),
            "newer cache must not be stale"
        );
        assert!(
            !cache_is_stale(older, older),
            "equal mtimes must not be stale (no false-positive)"
        );
    }

    // ─── MCP host drift (independent of component selection) ─────────────────

    /// Write a projected-state fixture with the given per-host records.
    fn write_mcp_state(dir: &std::path::Path, hosts: serde_json::Value) -> PathBuf {
        let path = dir.join("projected-state.json");
        let state = serde_json::json!({ "version": 1, "hosts": hosts });
        std::fs::write(&path, state.to_string()).unwrap();
        path
    }

    fn mcp_drift_messages(state_path: &std::path::Path) -> Vec<String> {
        let mut findings = Vec::new();
        check_mcp_host_drift_with_path(state_path, &mut findings);
        findings.into_iter().map(|f| f.message).collect()
    }

    fn mcp_partial_hosts() -> serde_json::Value {
        serde_json::json!({
            "claude": ["memory", "fetch"],
            "codex": ["memory"],
            "copilot": [],
            "agy": ["fetch"],
            "opencode": ["memory", "fetch"]
        })
    }

    #[test]
    fn mcp_health_selection_absent_record_produces_no_findings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("projected-state.json");
        assert!(mcp_drift_messages(&path).is_empty());
    }

    #[test]
    fn mcp_health_selection_reports_missing_name_per_host() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_mcp_state(dir.path(), mcp_partial_hosts());

        let messages = mcp_drift_messages(&path);

        assert_eq!(messages.len(), 4, "{messages:?}");
        for (host, name) in [
            ("agy", "memory"),
            ("codex", "fetch"),
            ("copilot", "fetch"),
            ("copilot", "memory"),
        ] {
            assert!(
                messages
                    .iter()
                    .any(|m| m.contains(&format!("'{name}'")) && m.contains(&format!("'{host}'"))),
                "missing {host}/{name} drift: {messages:?}"
            );
        }
    }

    #[test]
    fn mcp_health_selection_five_sources_give_same_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_mcp_state(dir.path(), mcp_partial_hosts());
        let sources = ["claude", "codex", "copilot", "opencode", "agy"];

        // Each fixture varies the source family and the selected components in
        // the lock file. The check takes no input from that file, so every
        // fixture must yield the same five-host result.
        let mut results = Vec::new();
        for i in 0..sources.len() {
            let adopted: Vec<serde_json::Value> = sources[..=i]
                .iter()
                .map(|name| serde_json::json!({ "name": name, "sourceId": format!("{name}-mcp") }))
                .collect();
            let lock = serde_json::json!({ "_adoptMaster": sources[i], "_adoptedItems": adopted });
            std::fs::write(dir.path().join("plugins.lock.json"), lock.to_string()).unwrap();
            results.push(mcp_drift_messages(&path));
        }

        assert_eq!(results[0].len(), 4, "{:?}", results[0]);
        for result in &results {
            assert_eq!(
                result, &results[0],
                "drift must not depend on source or selection"
            );
        }
    }

    #[test]
    fn mcp_health_selection_ignores_unknown_host_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_mcp_state(
            dir.path(),
            serde_json::json!({
                "claude": ["memory"],
                "codex": ["memory"],
                "copilot": [42, "memory"],
                "agy": ["memory"],
                "opencode": ["memory"],
                "zed": ["ghost"]
            }),
        );
        let before = std::fs::read_to_string(&path).unwrap();

        assert!(mcp_drift_messages(&path).is_empty());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "record must not be rewritten"
        );
    }

    #[test]
    fn mcp_health_selection_finding_is_warning() {
        use ccync_foundation::health::Severity;
        let f = drift_finding("fetch", "codex");
        assert_eq!(f.severity, Severity::Warning);
        assert!(f.message.contains("fetch") && f.message.contains("codex"));
        assert!(f.message.contains("ccync sync"));
    }

    // ─── personal plugin doctor ──────────────────────────────────────────────

    /// Build a minimal personal catalog JSON containing one or more plugins.
    fn personal_catalog(entries: &[(&str, &str)]) -> serde_json::Value {
        let plugins: Vec<serde_json::Value> = entries
            .iter()
            .map(|(id, src)| {
                serde_json::json!({
                    "pluginId": id,
                    "source": src,
                    "sourceType": "curated-upstream",
                    "installStrategy": "personal-git-clone"
                })
            })
            .collect();
        serde_json::json!({ "schemaVersion": 1, "plugins": plugins })
    }

    #[test]
    fn personal_plugin_finding_reports_id_and_source_when_unpinned() {
        let catalog = personal_catalog(&[("my-plugin", "https://github.com/user/my-plugin")]);
        let entry = catalog
            .get("plugins")
            .and_then(|v| v.as_array())
            .unwrap()
            .first()
            .unwrap()
            .clone();

        let finding = personal_plugin_finding(&entry, &[]);

        let finding = finding.expect("truly unpinned entry must produce a finding");
        assert!(
            finding.message.contains("my-plugin"),
            "finding must name the plugin id"
        );
        assert!(
            finding
                .message
                .contains("https://github.com/user/my-plugin"),
            "finding must include the source"
        );
        assert!(
            finding.message.contains("(unpinned)"),
            "finding must show unpinned when lock has no matching entry"
        );
    }

    #[test]
    fn personal_plugin_finding_silent_when_pinned() {
        let catalog = personal_catalog(&[("my-plugin", "src")]);
        let entry = catalog
            .get("plugins")
            .and_then(|v| v.as_array())
            .unwrap()
            .first()
            .unwrap()
            .clone();
        let lock_entries = vec![serde_json::json!({
            "pluginId": "my-plugin",
            "pin": "abcdef123456789012"
        })];

        let finding = personal_plugin_finding(&entry, &lock_entries);

        assert!(
            finding.is_none(),
            "pinned entry must produce no finding (healthy = silent)"
        );
    }

    #[test]
    fn personal_plugin_lock_entries_empty_when_no_lockfile() {
        let temp = tempfile::tempdir().unwrap();
        let entries = personal_plugin_lock_entries(&temp.path().join("missing-lock.json"));
        assert!(entries.is_empty());
    }
}
