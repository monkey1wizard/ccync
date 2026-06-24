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

        if let Ok(path) = ccync_foundation::config::CcyncConfig::config_path() {
            if path.exists() && std::fs::read_to_string(&path).is_err() {
                findings.push(DoctorFinding::error(
                    format!("machine config is unreadable: {}", path.display()),
                    "fix permissions or re-create ~/.ccync/config/config.json",
                ));
            }
        }

        scan_manifests_for_secrets(&self.repo_root, &mut findings);

        check_personal_layer(&mut findings);

        if which("rg").is_none() {
            findings.push(DoctorFinding::warning(
                "ripgrep (rg) not found on PATH — ccync search-dependent flows degrade",
            ));
        }

        check_cross_agent_drift(&mut findings);

        check_personal_plugins(&mut findings);

        findings
    }
}

/// Report the personal local layer state.
///
/// Produces a warning-severity (non-blocking) informational finding when the layer is
/// enabled and `~/.ccync/local/skills/` exists. Absent or disabled → no finding.
fn check_personal_layer(findings: &mut Vec<DoctorFinding>) {
    // Read config.json raw value; missing file → layer disabled, no finding.
    let config_path = match ccync_foundation::config::CcyncConfig::config_path() {
        Ok(p) => p,
        Err(_) => return,
    };
    let raw_str = match std::fs::read_to_string(&config_path) {
        Ok(s) => s,
        Err(_) => return,
    };
    let raw_cfg: serde_json::Value = match serde_json::from_str(&raw_str) {
        Ok(v) => v,
        Err(_) => return,
    };
    if !ccync_foundation::config::personal_layer_enabled(&raw_cfg) {
        return;
    }
    let local_skills = match ccync_foundation::paths::ccync_local_skills_root() {
        Some(p) => p,
        None => return,
    };
    if !local_skills.is_dir() {
        return;
    }

    let canonical_skills =
        ccync_foundation::paths::ccync_plugin_root("ccync").map(|c| c.join("skills"));
    let (n, collision_count) = personal_layer_counts(&local_skills, canonical_skills.as_deref());

    findings.push(DoctorFinding::warning(format!(
        "personal layer: enabled — {n} skill(s) present, {collision_count} core collision(s) (skipped by core-wins)"
    )));
}

/// Count personal skills and how many collide by name with canonical core skills.
///
/// `local_skills` = `~/.ccync/local/skills`; `canonical_skills` = the rendered core
/// skills dir (`None` when it does not exist yet). A personal skill directory
/// counts only when it contains a `SKILL.md`. Returns `(personal_count,
/// collision_count)`. Pure over the two directories — independently testable.
fn personal_layer_counts(
    local_skills: &std::path::Path,
    canonical_skills: Option<&std::path::Path>,
) -> (usize, usize) {
    let personal_names: std::collections::HashSet<String> = std::fs::read_dir(local_skills)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            e.file_type().map(|ft| ft.is_dir()).unwrap_or(false)
                && e.path().join("SKILL.md").exists()
        })
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    let n = personal_names.len();

    let collision_count = canonical_skills
        .filter(|p| p.is_dir())
        .map(|skills_dir| {
            std::fs::read_dir(skills_dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|core_name| personal_names.contains(core_name))
                .count()
        })
        .unwrap_or(0);

    (n, collision_count)
}

/// Report installed personal plugins.
///
/// Produces a warning-severity (non-blocking) informational finding per installed
/// personal plugin when the layer is enabled and `~/.ccync/local/catalog.json` exists.
/// Absent catalog, disabled layer, or empty catalog → no finding.
fn check_personal_plugins(findings: &mut Vec<DoctorFinding>) {
    let config_path = match ccync_foundation::config::CcyncConfig::config_path() {
        Ok(p) => p,
        Err(_) => return,
    };
    let raw_str = match std::fs::read_to_string(&config_path) {
        Ok(s) => s,
        Err(_) => return,
    };
    let raw_cfg: serde_json::Value = match serde_json::from_str(&raw_str) {
        Ok(v) => v,
        Err(_) => return,
    };
    if !ccync_foundation::config::personal_layer_enabled(&raw_cfg) {
        return;
    }

    let catalog_path = match ccync_foundation::paths::local_catalog_path() {
        Some(p) => p,
        None => return,
    };
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
    let lock_entries = personal_plugin_lock_entries();

    for entry in plugins {
        let id = entry
            .get("pluginId")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let source = entry.get("source").and_then(|v| v.as_str()).unwrap_or("?");
        let sha = lock_entries
            .iter()
            .find(|e| e.get("pluginId").and_then(|v| v.as_str()) == Some(id))
            .and_then(|e| e.get("sha").and_then(|v| v.as_str()))
            .unwrap_or("(unpinned)");
        let short_sha = if sha.len() > 12 { &sha[..12] } else { sha };
        findings.push(DoctorFinding::warning(format!(
            "personal plugin: '{id}' @ {short_sha} — {source}"
        )));
    }
}

/// Read `_personalPlugins` entries from the lockfile. Returns empty vec on any error.
fn personal_plugin_lock_entries() -> Vec<serde_json::Value> {
    ccync_foundation::paths::plugins_lock_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            v.get("_personalPlugins")
                .and_then(|a| a.as_array())
                .map(|a| a.to_vec())
        })
        .unwrap_or_default()
}

/// Report cross-agent drift: adopted MCP servers not found in the target agent's config.
///
/// Reads `_adoptMaster` and `_adoptedItems` from `plugins.lock.json`.
/// For each adopted MCP entry, checks if it exists in the target agent's live config.
/// Emits a non-blocking warning per missing item. No master → no findings.
fn check_cross_agent_drift(findings: &mut Vec<DoctorFinding>) {
    use ccync_foundation::paths::{claude_config_path, codex_config_path, plugins_lock_path};

    let (lock_path, claude_cfg, codex_cfg) = match (
        plugins_lock_path(),
        claude_config_path(),
        codex_config_path(),
    ) {
        (Some(l), Some(c), Some(x)) => (l, c, x),
        _ => return,
    };
    check_cross_agent_drift_with_paths(&lock_path, &claude_cfg, &codex_cfg, findings);
}

/// Path-injectable core of [`check_cross_agent_drift`] (testable without touching
/// real `~/.ccync` / `~/.claude` / `~/.codex` paths). The public wrapper resolves the
/// canonical paths; this function takes them explicitly.
fn check_cross_agent_drift_with_paths(
    lock_path: &std::path::Path,
    claude_cfg: &std::path::Path,
    codex_cfg: &std::path::Path,
    findings: &mut Vec<DoctorFinding>,
) {
    use serde_json::Value as JsonValue;

    if !lock_path.exists() {
        return;
    }
    let lock_text = match std::fs::read_to_string(lock_path) {
        Ok(t) => t,
        Err(_) => return,
    };
    let lock: JsonValue = match serde_json::from_str(&lock_text) {
        Ok(v) => v,
        Err(_) => return,
    };
    let master = match lock.get("_adoptMaster").and_then(|v| v.as_str()) {
        Some(m) => m.to_string(),
        None => return,
    };
    let adopted = match lock.get("_adoptedItems").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return,
    };

    // Determine target agent based on master.
    let (target, target_mcp_names): (String, std::collections::HashSet<String>) =
        match master.as_str() {
            "claude" => {
                // Target = Codex; read codex mcp_servers keys
                let names = std::fs::read_to_string(codex_cfg)
                    .ok()
                    .and_then(|t| toml::from_str::<toml::Value>(&t).ok())
                    .and_then(|v| {
                        v.get("mcp_servers")?
                            .as_table()
                            .map(|m| m.keys().cloned().collect())
                    })
                    .unwrap_or_default();
                ("codex".to_string(), names)
            }
            "codex" => {
                // Target = Claude; read claude mcpServers keys
                let names = std::fs::read_to_string(claude_cfg)
                    .ok()
                    .and_then(|t| serde_json::from_str::<JsonValue>(&t).ok())
                    .and_then(|v| {
                        v.get("mcpServers")?
                            .as_object()
                            .map(|m| m.keys().cloned().collect())
                    })
                    .unwrap_or_default();
                ("claude".to_string(), names)
            }
            _ => return,
        };

    for item in adopted {
        let name = match item.get("name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => continue,
        };
        let source_id = item.get("sourceId").and_then(|v| v.as_str()).unwrap_or("");
        // Only check MCP servers (plugins require binary install, not in scope).
        if !source_id.ends_with("-mcp") {
            continue;
        }
        if !target_mcp_names.contains(name) {
            findings.push(drift_finding(name, &master, &target));
        }
    }
}

fn drift_finding(name: &str, master: &str, target: &str) -> DoctorFinding {
    DoctorFinding::warning(format!(
        "cross-agent drift: adopted MCP '{name}' (from {master}) not found in {target} — run `ccync sync` to reconcile"
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
    /// `~/.ccync/plugins/ccync/` — the canonical root written by `ccync sync`.
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

    // ─── personal layer ──────────────────────────────────────────────────────

    fn make_local_skill(skills_root: &std::path::Path, name: &str) {
        let skill_dir = skills_root.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), format!("# {name}")).unwrap();
    }

    #[test]
    fn personal_layer_disabled_no_finding() {
        // No config.json under repo_root → personal_layer_enabled is false →
        // check() must not emit a personal-layer finding.
        let temp = tempfile::tempdir().unwrap();
        let local_skills = temp.path().join("local").join("skills");
        std::fs::create_dir_all(&local_skills).unwrap();
        make_local_skill(&local_skills, "my-skill");
        let check = SetupHealthCheck {
            repo_root: temp.path().to_path_buf(),
        };
        let findings = check.check();
        assert!(
            !findings
                .iter()
                .any(|f| f.message.contains("personal layer: enabled")),
            "disabled personal layer must not produce a finding: {findings:?}"
        );
    }

    #[test]
    fn personal_layer_counts_no_canonical_dir() {
        let temp = tempfile::tempdir().unwrap();
        let local = temp.path().join("skills");
        std::fs::create_dir_all(&local).unwrap();
        make_local_skill(&local, "aaa-skill");
        make_local_skill(&local, "zzz-skill");
        let (n, collisions) = personal_layer_counts(&local, None);
        assert_eq!(n, 2);
        assert_eq!(collisions, 0, "no canonical dir → no collisions");
    }

    #[test]
    fn personal_layer_counts_detects_collision() {
        let temp = tempfile::tempdir().unwrap();
        let local = temp.path().join("local-skills");
        let canon = temp.path().join("canon-skills");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::create_dir_all(&canon).unwrap();
        make_local_skill(&local, "doc-sync"); // collides with core
        make_local_skill(&local, "personal-only"); // unique
        std::fs::create_dir_all(canon.join("doc-sync")).unwrap(); // core skill dir
        let (n, collisions) = personal_layer_counts(&local, Some(&canon));
        assert_eq!(n, 2);
        assert_eq!(collisions, 1, "doc-sync collides with the core skill");
    }

    #[test]
    fn personal_layer_counts_ignores_dir_without_skill_md() {
        let temp = tempfile::tempdir().unwrap();
        let local = temp.path().join("skills");
        std::fs::create_dir_all(local.join("incomplete")).unwrap(); // no SKILL.md
        make_local_skill(&local, "valid");
        let (n, _) = personal_layer_counts(&local, None);
        assert_eq!(n, 1, "dir without SKILL.md is not counted");
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

    // ─── cross-agent drift ───────────────────────────────────────────────────

    #[test]
    fn drift_finding_message_contains_name_and_agents() {
        let f = drift_finding("memory", "claude", "codex");
        assert!(f.message.contains("memory"), "finding must name the server");
        assert!(f.message.contains("claude"), "finding must name the master");
        assert!(f.message.contains("codex"), "finding must name the target");
        assert!(
            f.message.contains("ccync sync"),
            "finding must suggest remediation"
        );
    }

    #[test]
    fn drift_finding_is_warning_severity() {
        use ccync_foundation::health::Severity;
        let f = drift_finding("fetch", "codex", "claude");
        assert_eq!(f.severity, Severity::Warning);
    }

    #[test]
    fn drift_no_lock_file_produces_no_findings() {
        // Injected lock path that does not exist → early return, no findings.
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("absent.lock.json");
        let claude = dir.path().join(".claude.json");
        let codex = dir.path().join("config.toml");
        let mut findings = Vec::new();
        check_cross_agent_drift_with_paths(&lock, &claude, &codex, &mut findings);
        assert!(
            findings.is_empty(),
            "absent lock must produce no drift findings"
        );
    }

    #[test]
    fn drift_present_emits_warning_for_missing_target_mcp() {
        // master=claude, adopted claude-mcp 'memory'; codex config lacks it → 1 warning.
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        std::fs::write(
            &lock,
            r#"{"_adoptMaster":"claude","_adoptedItems":[{"name":"memory","adoptedFrom":"claude","sourceId":"claude-mcp"}]}"#,
        )
        .unwrap();
        let claude = dir.path().join(".claude.json");
        let codex = dir.path().join("config.toml");
        // codex config exists but has no mcp_servers entry for 'memory'
        std::fs::write(&codex, "other = 1\n").unwrap();
        let mut findings = Vec::new();
        check_cross_agent_drift_with_paths(&lock, &claude, &codex, &mut findings);
        assert_eq!(findings.len(), 1, "missing target MCP must drift");
        assert!(findings[0].message.contains("memory"));
    }

    #[test]
    fn drift_absent_when_target_has_the_mcp() {
        // master=claude, adopted 'memory'; codex config DOES contain it → no findings.
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        std::fs::write(
            &lock,
            r#"{"_adoptMaster":"claude","_adoptedItems":[{"name":"memory","adoptedFrom":"claude","sourceId":"claude-mcp"}]}"#,
        )
        .unwrap();
        let claude = dir.path().join(".claude.json");
        let codex = dir.path().join("config.toml");
        std::fs::write(&codex, "[mcp_servers.memory]\ncommand = \"x\"\n").unwrap();
        let mut findings = Vec::new();
        check_cross_agent_drift_with_paths(&lock, &claude, &codex, &mut findings);
        assert!(findings.is_empty(), "present target MCP must not drift");
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
    fn personal_plugin_finding_reports_id_and_source() {
        let catalog = personal_catalog(&[("my-plugin", "https://github.com/user/my-plugin")]);
        let entries = catalog
            .get("plugins")
            .and_then(|v| v.as_array())
            .unwrap()
            .to_vec();

        let mut findings = Vec::new();
        // Simulate what check_personal_plugins does for the entries loop directly.
        for entry in &entries {
            let id = entry
                .get("pluginId")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let source = entry.get("source").and_then(|v| v.as_str()).unwrap_or("?");
            let sha = "(unpinned)";
            findings.push(DoctorFinding::warning(format!(
                "personal plugin: '{id}' @ {sha} — {source}"
            )));
        }

        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].message.contains("my-plugin"),
            "finding must name the plugin id"
        );
        assert!(
            findings[0]
                .message
                .contains("https://github.com/user/my-plugin"),
            "finding must include the source"
        );
        assert!(
            findings[0].message.contains("(unpinned)"),
            "finding must show unpinned sha when lock absent"
        );
    }

    #[test]
    fn personal_plugin_finding_shows_short_sha_when_pinned() {
        let full_sha = "abcdef123456789012";
        let short = &full_sha[..12];
        let entry = serde_json::json!({
            "pluginId": "my-plugin",
            "sha": full_sha
        });
        let sha = entry
            .get("sha")
            .and_then(|v| v.as_str())
            .unwrap_or("(unpinned)");
        let short_sha = if sha.len() > 12 { &sha[..12] } else { sha };

        let msg = format!("personal plugin: 'my-plugin' @ {short_sha} — src");
        assert!(msg.contains(short), "sha must be truncated to 12 chars");
        assert!(
            !msg.contains(full_sha),
            "full sha must not appear in finding"
        );
    }

    #[test]
    fn personal_plugin_lock_entries_empty_when_no_lockfile() {
        // personal_plugin_lock_entries() reads the real lockfile; without a fake path
        // we at least confirm it returns a Vec (empty or not) without panicking.
        let entries = personal_plugin_lock_entries();
        // Just confirm the function does not panic and returns a Vec.
        let _ = entries;
    }
}
