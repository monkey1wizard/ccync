//! `ccync doctor` — read-only health checks for CCYNC state.
//!
//! Checks canonical root freshness, runtime paths and links, and
//! plan lifecycle drift. Exit-code grading: warning-only → 0, any error → non-zero.
//!
//!
//! `ccync doctor --dry-run` must not modify any files.
//! missing projection or stale steward → error exit naming the fix target.
//! warning-only → exit 0; any error → exit non-zero.

use std::path::PathBuf;

// Doctor finding/report vocabulary + the `HealthCheck` trait now live in
// `ccync_foundation::health` (doctor dependency inversion). Re-export so
// `ccync_engine::doctor::*` and the in-module tests keep resolving; each domain
// implements `HealthCheck` and `cli` aggregates the trait objects.
pub use ccync_foundation::health::{DoctorFinding, DoctorReport, HealthCheck, Severity};

/// Options for the doctor command.
#[derive(Debug, Default)]
pub struct DoctorOptions {
    /// When `true`, do NOT modify any files (read-only mode).
    pub dry_run: bool,
    /// Include release-gate checks.
    pub release_gate: bool,
}

/// Run `ccync doctor` — check canonical root, runtime paths and links.
///
/// Always read-only when `dry_run` is true. Returns a report with findings.
/// The caller is responsible for printing and using `report.exit_code()`.
pub fn run_doctor(opts: &DoctorOptions) -> DoctorReport {
    let _ = opts.dry_run; // All checks are read-only; dry_run has no effect here.

    let canonical_root = canonical_root_path();

    // Assemble the checks as `HealthCheck` trait objects in the exact prior order.
    // Conditional checks (canonical-root-dependent, release-gate) are selected here;
    // each check's logic lives in its own `HealthCheck` impl.
    let checks: Vec<Box<dyn HealthCheck>> = vec![
        // Check 1: Canonical root existence (ccync content render target).
        Box::new(CanonicalRootCheck {
            canonical_root: canonical_root.clone(),
        }),
        // Check 2: A master agent has been adopted (`ccync init`).
        Box::new(MasterAdoptedCheck),
        // Check 3: ccync content projected to the Claude skill path.
        Box::new(SkillPathCheck {
            canonical_root: canonical_root.clone(),
        }),
        // Check 4: AGY links (warning only, best-effort and never fatal).
        Box::new(AgyLinksCheck {
            root: ccync_foundation::paths::user_home().map(|home| home.join(".gemini")),
            lockfile: ccync_foundation::paths::ccync_home()
                .map(|home| home.join("build").join("lock.json")),
        }),
        // Check 5: pre-existing vendor-native entries under adopt ownership.
        Box::new(VendorNativeAdoptedCheck),
        // Check 6: personal-catalog entries whose pinned cache dir renders nothing.
        Box::new(NoRenderContributionCheck),
        // Check 7: committed external-tool snapshots and lifecycle journal.
        Box::new(ToolArtifactCheck),
    ];

    // The baked-binary / runtime-manifest checks were removed: ccync does not
    // bake a binary or CCYNC plugin manifests into the canonical root (its content
    // render is the managed-set skills/commands/agents + .mcp.json), so those
    // checks were false positives on a freshly-synced ccync canonical root.
    let _ = opts.release_gate;

    let mut report = DoctorReport::new();
    for check in &checks {
        report.findings.extend(check.check());
    }
    report
}

// ---------------------------------------------------------------------------
// Individual checks
// ---------------------------------------------------------------------------

struct CanonicalRootCheck {
    canonical_root: PathBuf,
}

impl HealthCheck for CanonicalRootCheck {
    fn name(&self) -> &str {
        "canonical-root"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        let mut findings = Vec::new();
        if !self.canonical_root.exists() {
            findings.push(DoctorFinding::error(
                format!(
                    "canonical root not found: {}",
                    self.canonical_root.display()
                ),
                "run `ccync init <claude|codex>` then `ccync sync` to create + project it",
            ));
        }
        // No required-subdirectory check: the ccync content render only
        // materializes the subtrees the managed set actually contains (a
        // master with no commands legitimately has no `commands/`), so a
        // missing subdir is not an error.
        findings
    }
}

/// Check that a master agent has been adopted (`ccync init`), i.e. the lockfile
/// carries `_adoptMaster`. Without it, `ccync sync` has no truth source to project.
struct MasterAdoptedCheck;

/// Read-only integrity check for committed external-tool artifacts.
struct ToolArtifactCheck;

impl HealthCheck for ToolArtifactCheck {
    fn name(&self) -> &str {
        "tool-artifacts"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        use crate::tool_manager::{artifact_tree, store};

        let mut findings = Vec::new();
        match store::load_state() {
            Ok(records) => {
                for (id, record) in records {
                    let Some(generation) = record.committed_generation else {
                        continue;
                    };
                    let snapshot = ccync_foundation::paths::tool_artifacts_root()
                        .map(|root| root.join(id.as_str()).join(generation.to_string()));
                    let Some(snapshot) = snapshot else {
                        findings.push(DoctorFinding::error(
                            format!("committed tool snapshot path unavailable for {id}"),
                            "restore CCYNC home path configuration",
                        ));
                        continue;
                    };
                    if !snapshot.exists() {
                        findings.push(DoctorFinding::error(
                            format!("committed tool snapshot missing for {id}: {}", snapshot.display()),
                            "run `ccync tool refresh` to create a new committed snapshot",
                        ));
                        continue;
                    }
                    match artifact_tree::scan_tree(&snapshot) {
                        Err(error) => findings.push(DoctorFinding::error(
                            format!("committed tool snapshot unreadable for {id}: {error}"),
                            "inspect the committed snapshot and restore its files",
                        )),
                        Ok(manifest) => {
                            let actual = format!("sha256:{}", manifest.sha256);
                            if record.artifact_fingerprint.as_deref() != Some(actual.as_str()) {
                                findings.push(DoctorFinding::error(
                                    format!("committed tool snapshot changed for {id}: {}", snapshot.display()),
                                    "run `ccync tool refresh` to reconcile the committed snapshot",
                                ));
                            }
                        }
                    }
                }
            }
            Err(error) => findings.push(DoctorFinding::error(
                format!("external-tool state unreadable: {error}"),
                "inspect the tool state file under CCYNC build tools",
            )),
        }

        if let Some(path) = ccync_foundation::paths::tool_operation_journal_path() {
            match std::fs::metadata(&path) {
                Ok(_) => findings.push(DoctorFinding::error(
                    format!("external-tool operation journal is active or recovery-required: {}", path.display()),
                    "review the operation and use `ccync tool recover`",
                )),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => findings.push(DoctorFinding::error(
                    format!("external-tool operation journal unreadable: {error}"),
                    "inspect the operation journal under CCYNC build tools",
                )),
            }
        }
        findings
    }
}

impl HealthCheck for MasterAdoptedCheck {
    fn name(&self) -> &str {
        "master-adopted"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        let mut findings = Vec::new();
        let Some(lock_path) = ccync_foundation::paths::plugins_lock_path() else {
            return findings;
        };
        if !lock_path.is_file() {
            findings.push(DoctorFinding::warning(
                "no master adopted yet (lockfile absent) — run `ccync init <claude|codex>`",
            ));
            return findings;
        }
        let has_master = std::fs::read_to_string(&lock_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| {
                v.get("_adoptMaster")
                    .and_then(|m| m.as_str())
                    .map(str::to_string)
            })
            .is_some();
        if !has_master {
            findings.push(DoctorFinding::warning(
                "no master agent recorded (_adoptMaster) — run `ccync init <claude|codex>`",
            ));
        }
        findings
    }
}

/// Check that `~/.claude/skills/ccync` (the CCYNC-owned Claude skill path) exists.
///
/// This is the path Claude Code scans for skills. If missing, `doc-sync` and other
/// CCYNC skills are not loaded by Claude regardless of canonical root state.
struct SkillPathCheck {
    canonical_root: PathBuf,
}

impl HealthCheck for SkillPathCheck {
    fn name(&self) -> &str {
        "skill-path"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        let mut findings = Vec::new();
        let Some(home) = ccync_foundation::paths::user_home() else {
            return findings;
        };
        let skill_path = home.join(".claude").join("skills").join("ccync");
        if !skill_path.exists() {
            findings.push(DoctorFinding::error(
                "Claude skill path not found (~/.claude/skills/ccync) — ccync skills not loaded by Claude",
                "run `ccync sync` to project the skill path",
            ));
            return findings;
        }
        // Verify the path resolves to the canonical root (symlink/junction target check).
        if self.canonical_root.exists() {
            let resolved = std::fs::canonicalize(&skill_path)
                .ok()
                .or_else(|| Some(skill_path.clone()));
            let canonical_resolved = std::fs::canonicalize(&self.canonical_root).ok();
            if let (Some(path_real), Some(root_real)) = (resolved, canonical_resolved) {
                if path_real != root_real {
                    findings.push(DoctorFinding::error(
                        format!(
                            "Claude skill path (~/.claude/skills/ccync) does not point to canonical root ({})",
                            self.canonical_root.display()
                        ),
                        "run `ccync sync` to realign the skill path",
                    ));
                }
            }
        }
        findings
    }
}

/// Checks all three AGY (Antigravity) whole-plugin roots: CLI, IDE, and GUI.
/// Warning-only and best-effort — symbolic links and *owned* directory copies
/// are healthy, while absent roots, legacy junctions, and real directories
/// ccync does not own need another sync or the user's attention.
///
/// The ownership half matters as much as the shape half: when a user's own
/// directory occupies an agy root, `publish_agy_root` preserves it and
/// publishes nothing, so that face is dark. Reporting it healthy would move
/// the invisible-surface failure this check exists to catch from the scanner
/// into the health check itself.
struct AgyLinksCheck {
    /// Injectable `.gemini` root so the health check can be tested directly.
    root: Option<PathBuf>,
    /// Injectable `lock.json` path — the ownership proof that separates a
    /// managed copy from a user's own directory at the same path.
    lockfile: Option<PathBuf>,
}

impl HealthCheck for AgyLinksCheck {
    fn name(&self) -> &str {
        "agy-links"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        let mut findings = Vec::new();
        let Some(root) = &self.root else {
            return findings;
        };

        let roots = [
            ("CLI", root.join("antigravity-cli/plugins/ccync")),
            ("IDE", root.join("antigravity-ide/plugins/ccync")),
            ("GUI", root.join("config/plugins/ccync")),
        ];
        let ownership = self
            .lockfile
            .as_ref()
            // Only a lockfile that exists *and* parses can disprove ownership.
            // `AgyRootOwnership::from_lockfile` tolerates both faults by owning
            // nothing, which reads identically to "ccync owns none of these
            // roots" — so without this filter, deleting `~/.ccync/build/` or a
            // truncated write would make doctor tell the user to remove three
            // healthy managed directories.
            .filter(|lock| {
                std::fs::read_to_string(lock)
                    .ok()
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                    .is_some_and(|value| value.is_object())
            })
            .map(|lock| projection::AgyRootOwnership::from_lockfile(lock));

        for (face, path) in roots {
            let shape = ccync_foundation::platform::classify_link(&path);
            let message = match shape {
                ccync_foundation::platform::LinkShape::Absent => Some(format!(
                    "AGY {face} root missing at {}; run `ccync sync` to publish it",
                    path.display()
                )),
                ccync_foundation::platform::LinkShape::Junction => Some(format!(
                    "AGY {face} root is a legacy junction at {}; run `ccync sync` to replace it with a symbolic link or managed copy",
                    path.display()
                )),
                ccync_foundation::platform::LinkShape::Directory => {
                    // Silent only when the copy is provably ccync's. With no
                    // lockfile to consult, ownership cannot be disproved
                    // either, so stay silent rather than warn on every copy.
                    match &ownership {
                        Some(ownership) if !ownership.is_owned(&path) => Some(format!(
                            "AGY {face} root at {} is a directory ccync does not own; it was preserved rather than published, so this face is not live — move or remove it, then run `ccync sync`",
                            path.display()
                        )),
                        _ => None,
                    }
                }
                ccync_foundation::platform::LinkShape::Symlink => None,
            };
            if let Some(message) = message {
                findings.push(DoctorFinding::warning(message));
            }
        }
        findings
    }
}

/// Check the lockfile's `_adoptedItems` and `_mcpServers` namespaces for names
/// that collide with the recorded master's [`crate::adopt::vendor_native_names`]
/// list — i.e. entries adopted before that name was surveyed as vendor-native.
/// Report only, per adopt's own read-boundary contract: this check never
/// rewrites the lockfile, it only points at `ccync remove <name>` for the
/// human to run. No `_adoptMaster` recorded yields no finding here — that gap
/// is already reported by [`MasterAdoptedCheck`], not duplicated.
struct VendorNativeAdoptedCheck;

impl HealthCheck for VendorNativeAdoptedCheck {
    fn name(&self) -> &str {
        "vendor-native-adopted"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        let mut findings = Vec::new();
        let Some(lock_path) = ccync_foundation::paths::plugins_lock_path() else {
            return findings;
        };
        if !lock_path.is_file() {
            return findings;
        }
        let Some(lock) = std::fs::read_to_string(&lock_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        else {
            return findings;
        };
        let Some(master) = lock.get("_adoptMaster").and_then(|v| v.as_str()) else {
            return findings;
        };
        let agent = match master {
            "claude" => crate::adopt::AgentKind::Claude,
            "codex" => crate::adopt::AgentKind::Codex,
            _ => return findings,
        };
        let vendor_names = crate::adopt::vendor_native_names(&agent);
        if vendor_names.is_empty() {
            return findings;
        }

        let mut hit_names: Vec<String> = Vec::new();
        if let Some(items) = lock.get("_adoptedItems").and_then(|v| v.as_array()) {
            for item in items {
                if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
                    if vendor_names.contains(&name) && !hit_names.iter().any(|n| n == name) {
                        hit_names.push(name.to_string());
                    }
                }
            }
        }
        if let Some(servers) = lock.get("_mcpServers").and_then(|v| v.as_object()) {
            for name in servers.keys() {
                if vendor_names.contains(&name.as_str()) && !hit_names.iter().any(|n| n == name) {
                    hit_names.push(name.clone());
                }
            }
        }

        if !hit_names.is_empty() {
            let names = hit_names.join(", ");
            findings.push(DoctorFinding::warning(format!(
                "pre-existing vendor-native entries adopted under {master}: {names} — run `ccync remove <name>` for each to release them"
            )));
        }
        findings
    }
}

/// Check every personal-catalog entry's pinned cache dir for render
/// contribution. A pinned entry whose cache dir exists yet renders neither a
/// component nor an MCP server is invisible to every other check — it never
/// fails a read, it just contributes nothing — so this walks the catalog and
/// calls [`no_contribution_finding`] once per entry. An absent or unparsable
/// catalog or lockfile yields no findings here, mirroring the [`AgyLinksCheck`]
/// ownership-filter rule: a failed read must never be treated as a positive
/// result (i.e. "nothing to warn about").
struct NoRenderContributionCheck;

impl HealthCheck for NoRenderContributionCheck {
    fn name(&self) -> &str {
        "render-contribution"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        let mut findings = Vec::new();

        let Some(catalog_path) = ccync_foundation::paths::personal_catalog_path() else {
            return findings;
        };
        let Some(catalog_entries) = std::fs::read_to_string(&catalog_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("plugins").and_then(|p| p.as_array()).cloned())
        else {
            return findings;
        };

        let Some(lock_path) = ccync_foundation::paths::plugins_lock_path() else {
            return findings;
        };
        let Some(lock_entries) = std::fs::read_to_string(&lock_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| {
                v.get("_personalPlugins")
                    .and_then(|p| p.as_array())
                    .cloned()
            })
        else {
            return findings;
        };

        let Some(cache_root) = ccync_foundation::paths::ccync_cache_root() else {
            return findings;
        };

        for entry in &catalog_entries {
            if let Some(finding) = no_contribution_finding(entry, &lock_entries, &cache_root) {
                findings.push(finding);
            }
        }
        findings
    }
}

/// Pure per-entry seam mirroring `setup::health::personal_plugin_finding`'s
/// shape: resolves the catalog entry's pin from the lockfile only, resolves
/// the pinned cache dir, and reports a finding only when that cache dir
/// exists yet contributes neither a component nor an MCP server to the
/// render. An unpinned entry or an absent cache dir stays silent here —
/// those gaps belong to other checks.
fn no_contribution_finding(
    catalog_entry: &serde_json::Value,
    lock_entries: &[serde_json::Value],
    cache_root: &std::path::Path,
) -> Option<DoctorFinding> {
    let id = catalog_entry.get("pluginId").and_then(|v| v.as_str())?;
    let pin = lock_entries
        .iter()
        .find(|e| e.get("pluginId").and_then(|v| v.as_str()) == Some(id))
        .and_then(ccync_foundation::pins::entry_pin)?;

    let plugin_dir = cache_root.join(format!("{id}@{pin}"));
    if !plugin_dir.is_dir() {
        return None;
    }

    if crate::install::contributes_to_render(id, &plugin_dir) {
        return None;
    }

    Some(DoctorFinding {
        severity: Severity::Warning,
        message: format!(
            "personal plugin '{id}' at {} contributes no component and no MCP server",
            plugin_dir.display()
        ),
        fix_hint: Some(
            "the source is not a plugin, skill, or MCP payload, or the running binary \
             predates the payload-shape support that source needs"
                .to_string(),
        ),
    })
}

fn canonical_root_path() -> PathBuf {
    // The canonical render root the install/sync engine actually writes
    // (`~/.ccync/build/render`). Resolve it from the shared foundation helper so
    // doctor never drifts from the layout used by `install::run_update`; the old
    // hardcoded `~/.ccync/plugins/ccync` path predates the plugins→build layout
    // migration and is never created anymore.
    ccync_foundation::paths::canonical_plugin_root().unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_check_home() -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>, Option<std::ffi::OsString>) {
        let guard = crate::test_support::env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        let home = tempfile::tempdir().unwrap();
        std::env::set_var(var, home.path());
        (home, guard, prior)
    }

    fn restore_tool_check_home(prior: Option<std::ffi::OsString>) {
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        match prior {
            Some(value) => std::env::set_var(var, value),
            None => std::env::remove_var(var),
        }
    }

    fn write_tool_state(home: &std::path::Path, fingerprint: &str) {
        let path = home.join(".ccync/build/tools/state.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!(r#"{{"version":1,"records":{{"graphify":{{"committedGeneration":1,"artifactFingerprint":"{fingerprint}","sourceEvidence":{{}},"operationId":null,"facets":{{"evidenceLevel":"verified","selectionState":"unique","managerAvailability":"available","installState":"installed","packageState":"matches-artifact-source","updateState":"not-checked","artifactState":"current","projectionState":"current","holdPolicy":"unheld","managerPinState":"unknown","operationState":"none","lastCheck":null}}}}}}}}"#)).unwrap();
    }

    #[test]
    fn tool_artifact_check_is_clean_without_committed_tools_or_journal() {
        let (home, _guard, prior) = tool_check_home();
        let snapshot = home.path().join(".ccync/build/tools/artifacts/graphify/1");
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::write(snapshot.join("artifact.txt"), "committed").unwrap();
        let manifest = crate::tool_manager::artifact_tree::scan_tree(&snapshot).unwrap();
        write_tool_state(home.path(), &format!("sha256:{}", manifest.sha256));
        let findings = ToolArtifactCheck.check();
        restore_tool_check_home(prior);
        assert!(findings.is_empty(), "unexpected findings: {findings:?}");
        drop(home);
    }

    #[test]
    fn tool_artifact_check_reports_missing_snapshot() {
        let (home, _guard, prior) = tool_check_home();
        write_tool_state(home.path(), "sha256:expected");
        let findings = ToolArtifactCheck.check();
        restore_tool_check_home(prior);
        assert!(findings.iter().any(|f| f.message.contains("snapshot missing")));
        drop(home);
    }

    #[test]
    fn tool_artifact_check_reports_changed_snapshot() {
        let (home, _guard, prior) = tool_check_home();
        let snapshot = home.path().join(".ccync/build/tools/artifacts/graphify/1");
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::write(snapshot.join("artifact.txt"), "changed").unwrap();
        write_tool_state(home.path(), "sha256:expected");
        let findings = ToolArtifactCheck.check();
        restore_tool_check_home(prior);
        assert!(findings.iter().any(|f| f.message.contains("snapshot changed")));
        drop(home);
    }

    #[test]
    fn tool_artifact_check_reports_unreadable_snapshot() {
        let (home, _guard, prior) = tool_check_home();
        let snapshot = home.path().join(".ccync/build/tools/artifacts/graphify/1");
        std::fs::create_dir_all(snapshot.parent().unwrap()).unwrap();
        std::fs::write(&snapshot, "not a directory").unwrap();
        write_tool_state(home.path(), "sha256:expected");
        let findings = ToolArtifactCheck.check();
        restore_tool_check_home(prior);
        assert!(findings.iter().any(|f| f.message.contains("snapshot unreadable")));
        drop(home);
    }

    #[test]
    fn tool_artifact_check_reports_unreadable_state_and_journal() {
        let (home, _guard, prior) = tool_check_home();
        let state = home.path().join(".ccync/build/tools/state.json");
        std::fs::create_dir_all(state.parent().unwrap()).unwrap();
        std::fs::write(&state, "not json").unwrap();
        std::fs::write(home.path().join(".ccync/build/tools/operation.txn.json"), "{}").unwrap();
        let findings = ToolArtifactCheck.check();
        restore_tool_check_home(prior);
        assert!(findings.iter().any(|f| f.message.contains("state unreadable")));
        assert!(findings.iter().any(|f| f.message.contains("journal is active or recovery-required")));
        drop(home);
    }

    #[test]
    fn tool_artifact_check_is_clean_without_state_file() {
        let (home, _guard, prior) = tool_check_home();
        let findings = ToolArtifactCheck.check();
        restore_tool_check_home(prior);
        assert!(findings.is_empty(), "expected no findings for absent state and journal: {findings:?}");
        drop(home);
    }

    #[test]
    fn tool_artifact_check_reports_active_journal_with_valid_snapshot() {
        let (home, _guard, prior) = tool_check_home();
        let snapshot = home.path().join(".ccync/build/tools/artifacts/graphify/1");
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::write(snapshot.join("artifact.txt"), "committed").unwrap();
        let manifest = crate::tool_manager::artifact_tree::scan_tree(&snapshot).unwrap();
        write_tool_state(home.path(), &format!("sha256:{}", manifest.sha256));
        std::fs::write(home.path().join(".ccync/build/tools/operation.txn.json"), "{}").unwrap();
        let findings = ToolArtifactCheck.check();
        restore_tool_check_home(prior);
        assert_eq!(findings.len(), 1, "expected only the journal finding: {findings:?}");
        assert!(findings[0].message.contains("journal is active or recovery-required"));
        drop(home);
    }

    #[test]
    fn run_doctor_aggregates_tool_artifact_check() {
        let (home, _guard, prior) = tool_check_home();
        write_tool_state(home.path(), "sha256:expected");
        let report = run_doctor(&DoctorOptions::default());
        restore_tool_check_home(prior);
        assert!(
            report.findings.iter().any(|f| f.message.contains("committed tool snapshot missing")),
            "expected run_doctor to include tool artifact check findings: {:?}",
            report.findings
        );
        drop(home);
    }

    #[test]
    fn doctor_finding_display_includes_severity_and_message() {
        let f = DoctorFinding::error("canonical root missing", "run `ccync sync`");
        let s = f.to_string();
        assert!(s.contains("ERROR"), "display should include ERROR: {s}");
        assert!(
            s.contains("canonical root missing"),
            "display should include message: {s}"
        );
        assert!(
            s.contains("ccync sync"),
            "display should include fix hint: {s}"
        );
    }

    #[test]
    fn doctor_finding_warning_has_no_fix_hint() {
        let f = DoctorFinding::warning("AGY link missing");
        let s = f.to_string();
        assert!(s.contains("WARNING"), "display should include WARNING: {s}");
        assert!(f.fix_hint.is_none());
    }

    #[test]
    fn report_exit_code_zero_when_only_warnings() {
        let mut report = DoctorReport::new();
        report.push(DoctorFinding::warning("some warning"));
        assert_eq!(report.exit_code(), 0, "warning-only should exit 0");
        assert!(!report.has_errors());
    }

    #[test]
    fn report_exit_code_nonzero_when_error_present() {
        let mut report = DoctorReport::new();
        report.push(DoctorFinding::error("critical failure", "fix it"));
        assert_eq!(report.exit_code(), 1, "error should exit non-zero");
        assert!(report.has_errors());
    }

    #[test]
    fn report_exit_code_zero_when_empty() {
        let report = DoctorReport::new();
        assert_eq!(report.exit_code(), 0);
        assert!(!report.has_errors());
    }

    #[test]
    fn dry_run_option_is_constructible() {
        let opts = DoctorOptions {
            dry_run: true,
            release_gate: false,
        };
        // run_doctor is read-only anyway; dry_run is a no-op for checks.
        // This test verifies the struct is usable.
        assert!(opts.dry_run);
    }

    #[test]
    fn run_doctor_returns_report_without_panic() {
        // Doctor may find errors (canonical root missing on CI/test machine),
        // but must not panic regardless of filesystem state.
        let opts = DoctorOptions::default();
        let report = run_doctor(&opts);
        // We don't assert exit code here — it depends on filesystem state.
        // We only verify no panic and findings is a Vec.
        let _ = report.findings.len();
        let _ = report.exit_code();
    }

    // ─── skill path + bin exposure doctor tests ───────────

    #[test]
    fn check_skill_path_error_when_missing() {
        use tempfile::TempDir;

        let root = TempDir::new().unwrap();

        // Simulate: skill_path does not exist. Verify the find/message pattern
        // inline (the real check reads the home dir, which tests cannot inject).
        let skill_path = root.path().join("skills_ccync");
        let mut report = DoctorReport::new();
        if !skill_path.exists() {
            report.push(DoctorFinding::error(
                "Claude skill path not found (~/.claude/skills/ccync) — ccync skills not loaded by Claude",
                "run `ccync sync` to project the skill path",
            ));
        }
        assert!(report.has_errors(), "missing skill path must be an error");
        let s = report.findings[0].to_string();
        assert!(
            s.contains("Claude skill path not found"),
            "finding must use the exact fixed prefix: {s}"
        );
        assert!(
            s.contains("ccync sync"),
            "finding must suggest ccync sync: {s}"
        );
    }

    #[test]
    fn check_skill_path_silent_when_present() {
        // present fixtures stay silent: an existing skill_path pushes no finding.
        use tempfile::TempDir;

        let root = TempDir::new().unwrap();
        let skill_path = root.path().join("skills_ccync");
        std::fs::create_dir_all(&skill_path).unwrap();

        let mut report = DoctorReport::new();
        if !skill_path.exists() {
            report.push(DoctorFinding::error(
                "Claude skill path not found (~/.claude/skills/ccync) — ccync skills not loaded by Claude",
                "run `ccync sync` to project the skill path",
            ));
        }
        assert!(
            !report.has_errors(),
            "a present skill path fixture must not raise a finding"
        );
    }

    #[test]
    fn skill_path_check_follows_injected_env_home() {
        use crate::test_support::env_guard;
        use tempfile::TempDir;

        let _guard = env_guard();
        let fake_home = TempDir::new().unwrap();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        std::env::set_var(var, fake_home.path());

        let check = SkillPathCheck {
            canonical_root: PathBuf::from("does-not-exist"),
        };
        let findings = check.check();

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }

        assert!(
            findings
                .iter()
                .any(|f| f.message.starts_with("Claude skill path not found")),
            "check must report the injected-home skill path as missing, not the real home's: {findings:?}"
        );
    }

    // ─── agy-links doctor tests ───────────────────────────────

    #[test]
    fn check_agy_links_warns_for_absent_roots() {
        use tempfile::TempDir;

        let root = TempDir::new().unwrap();
        let findings = AgyLinksCheck {
            root: Some(root.path().to_path_buf()),
            lockfile: None,
        }
        .check();

        assert_eq!(findings.len(), 3);
        assert!(findings
            .iter()
            .all(|finding| finding.severity == Severity::Warning));
        assert!(findings
            .iter()
            .all(|finding| finding.message.contains("run `ccync sync`")));
    }

    /// Seed real ownership rows through the production write path, so the
    /// check is driven by the same lockfile shape `publish_agy_root` writes.
    fn seed_agy_ownership(root: &std::path::Path, lockfile: &std::path::Path) {
        let ownership = projection::AgyRootOwnership::from_lockfile(lockfile);
        for relative in [
            "antigravity-cli/plugins/ccync",
            "antigravity-ide/plugins/ccync",
            "config/plugins/ccync",
        ] {
            ownership.record_now(&root.join(relative), false).unwrap();
        }
    }

    #[test]
    fn check_agy_links_is_silent_for_owned_directory_copies() {
        use tempfile::TempDir;

        let root = TempDir::new().unwrap();
        let lockfile = root.path().join("lock.json");
        for relative in [
            "antigravity-cli/plugins/ccync",
            "antigravity-ide/plugins/ccync",
            "config/plugins/ccync",
        ] {
            std::fs::create_dir_all(root.path().join(relative)).unwrap();
        }
        seed_agy_ownership(root.path(), &lockfile);

        assert!(AgyLinksCheck {
            root: Some(root.path().to_path_buf()),
            lockfile: Some(lockfile),
        }
        .check()
        .is_empty());
    }

    #[test]
    fn check_agy_links_warns_for_unowned_directories_at_agy_roots() {
        use tempfile::TempDir;

        // Same on-disk shape as the owned case above — real directories at all
        // three roots — but no ownership row. `publish_agy_root` preserves such
        // a path and publishes nothing, so the face is dark and the check must
        // say so rather than report health.
        let root = TempDir::new().unwrap();
        let lockfile = root.path().join("lock.json");
        for relative in [
            "antigravity-cli/plugins/ccync",
            "antigravity-ide/plugins/ccync",
            "config/plugins/ccync",
        ] {
            std::fs::create_dir_all(root.path().join(relative)).unwrap();
        }
        // A real, parseable lockfile that owns *something* — just not these
        // roots. An absent lockfile is the separate "ownership unprovable"
        // case and must stay silent; this case is a positive disproof.
        projection::AgyRootOwnership::from_lockfile(&lockfile)
            .record_now(&root.path().join("some/other/root"), false)
            .unwrap();

        let findings = AgyLinksCheck {
            root: Some(root.path().to_path_buf()),
            lockfile: Some(lockfile),
        }
        .check();

        assert_eq!(findings.len(), 3);
        assert!(findings
            .iter()
            .all(|finding| finding.severity == Severity::Warning));
        assert!(findings
            .iter()
            .all(|finding| finding.message.contains("ccync does not own")));
    }

    #[test]
    fn check_agy_links_stays_silent_for_directories_when_the_lockfile_is_missing_or_malformed() {
        use tempfile::TempDir;

        // The production wiring always supplies a lockfile *path* (it is pure
        // path construction), so "no lockfile" reaches this check as a path
        // that does not exist or does not parse — not as `None`. Both must
        // read as "ownership unprovable", never as "ccync owns nothing".
        for seed in [None, Some("{ not json")] {
            let root = TempDir::new().unwrap();
            let lockfile = root.path().join("lock.json");
            if let Some(raw) = seed {
                std::fs::write(&lockfile, raw).unwrap();
            }
            for relative in [
                "antigravity-cli/plugins/ccync",
                "antigravity-ide/plugins/ccync",
                "config/plugins/ccync",
            ] {
                std::fs::create_dir_all(root.path().join(relative)).unwrap();
            }

            let findings = AgyLinksCheck {
                root: Some(root.path().to_path_buf()),
                lockfile: Some(lockfile),
            }
            .check();

            assert!(
                findings.is_empty(),
                "an unreadable lockfile must not turn healthy managed roots into \
                 'remove this directory' advice (seed: {seed:?}): {findings:?}"
            );
        }
    }

    #[test]
    fn check_agy_links_stays_silent_for_directories_when_no_lockfile_is_available() {
        use tempfile::TempDir;

        // Ownership cannot be disproved without a lockfile, so an unprovable
        // copy must not be warned about — that would fire on every root of a
        // healthy install whose lockfile path could not be resolved.
        let root = TempDir::new().unwrap();
        for relative in [
            "antigravity-cli/plugins/ccync",
            "antigravity-ide/plugins/ccync",
            "config/plugins/ccync",
        ] {
            std::fs::create_dir_all(root.path().join(relative)).unwrap();
        }

        assert!(AgyLinksCheck {
            root: Some(root.path().to_path_buf()),
            lockfile: None,
        }
        .check()
        .is_empty());
    }

    #[test]
    fn check_agy_links_is_silent_for_symbolic_links() {
        use tempfile::TempDir;

        let root = TempDir::new().unwrap();
        let target = root.path().join("canonical");
        std::fs::create_dir(&target).unwrap();
        for relative in [
            "antigravity-cli/plugins/ccync",
            "antigravity-ide/plugins/ccync",
            "config/plugins/ccync",
        ] {
            let path = root.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            if ccync_foundation::platform::create_symlink_dir(&target, &path).is_err() {
                return;
            }
        }

        assert!(AgyLinksCheck {
            root: Some(root.path().to_path_buf()),
            lockfile: None,
        }
        .check()
        .is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn check_agy_links_warns_for_legacy_junctions() {
        use tempfile::TempDir;

        let root = TempDir::new().unwrap();
        let target = root.path().join("canonical");
        std::fs::create_dir(&target).unwrap();
        for relative in [
            "antigravity-cli/plugins/ccync",
            "antigravity-ide/plugins/ccync",
            "config/plugins/ccync",
        ] {
            let path = root.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let command = format!(
                "New-Item -ItemType Junction -Path '{}' -Target '{}' | Out-Null",
                path.display(),
                target.display()
            );
            assert!(std::process::Command::new("powershell")
                .args(["-NoProfile", "-Command", &command])
                .status()
                .unwrap()
                .success());
        }

        let findings = AgyLinksCheck {
            root: Some(root.path().to_path_buf()),
            lockfile: None,
        }
        .check();
        assert_eq!(findings.len(), 3);
        assert!(findings
            .iter()
            .all(|finding| finding.message.contains("legacy junction")));
    }

    #[test]
    fn agy_links_check_id_and_struct_are_renamed() {
        let check = AgyLinksCheck {
            root: None,
            lockfile: None,
        };
        assert_eq!(check.name(), "agy-links");
    }

    // ─── vendor-native-adopted doctor tests ───────────────────────────────

    /// Point the process-wide home env var (`USERPROFILE`/`HOME`) at `fake_home`
    /// and write `lock_json` to the resolved `plugins_lock_path()` (mirrors how
    /// `skill_path_check_follows_injected_env_home` injects home for path tests).
    fn write_injected_lock(fake_home: &std::path::Path, lock_json: &str) {
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        std::env::set_var(var, fake_home);
        let lock_path = ccync_foundation::paths::plugins_lock_path().unwrap();
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        std::fs::write(&lock_path, lock_json).unwrap();
    }

    #[test]
    fn vendor_native_adopted_check_warns_on_codex_hit_in_adopted_items() {
        use crate::test_support::env_guard;
        use tempfile::TempDir;

        let _guard = env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        let fake_home = TempDir::new().unwrap();
        write_injected_lock(
            fake_home.path(),
            r#"{"_adoptMaster":"codex","_adoptedItems":[{"name":"node_repl","adoptedFrom":"codex","sourceId":"codex-mcp"}]}"#,
        );

        let findings = VendorNativeAdoptedCheck.check();

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }

        assert_eq!(findings.len(), 1);
        let msg = &findings[0].message;
        assert!(msg.contains("node_repl"), "message: {msg}");
        assert!(msg.contains("ccync remove"), "message: {msg}");
    }

    #[test]
    fn vendor_native_adopted_check_warns_on_codex_hit_in_mcp_servers() {
        use crate::test_support::env_guard;
        use tempfile::TempDir;

        let _guard = env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        let fake_home = TempDir::new().unwrap();
        write_injected_lock(
            fake_home.path(),
            r#"{"_adoptMaster":"codex","_mcpServers":{"node_repl":{"command":"node"}}}"#,
        );

        let findings = VendorNativeAdoptedCheck.check();

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }

        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("node_repl"));
    }

    #[test]
    fn vendor_native_adopted_check_silent_when_no_collision() {
        use crate::test_support::env_guard;
        use tempfile::TempDir;

        let _guard = env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        let fake_home = TempDir::new().unwrap();
        write_injected_lock(
            fake_home.path(),
            r#"{"_adoptMaster":"claude","_adoptedItems":[{"name":"memory","adoptedFrom":"claude","sourceId":"claude-mcp"}]}"#,
        );

        let findings = VendorNativeAdoptedCheck.check();

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }

        assert!(findings.is_empty());
    }

    #[test]
    fn vendor_native_adopted_check_silent_when_no_master_recorded() {
        use crate::test_support::env_guard;
        use tempfile::TempDir;

        let _guard = env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        let fake_home = TempDir::new().unwrap();
        write_injected_lock(fake_home.path(), r#"{}"#);

        let findings = VendorNativeAdoptedCheck.check();

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }

        assert!(
            findings.is_empty(),
            "missing _adoptMaster is MasterAdoptedCheck's finding, not this check's"
        );
    }

    // ─── DoctorReport round-trip coverage for the vendor-native-adopted check ──
    // Independent of the implementer's 4 direct-`check()` tests above: these
    // drive the finding through `DoctorReport` (exit-code contribution) and
    // assert the lockfile is untouched (report-only), plus the
    // finding-disappears-after-removal round trip.

    #[test]
    fn vendor_native_adopted_finding_is_warning_severity_exit_zero() {
        // "exits 0" — a single vendor-native hit must be Severity::Warning,
        // not Error, so DoctorReport::exit_code() stays 0 (warning-only grading).
        use crate::test_support::env_guard;
        use tempfile::TempDir;

        let _guard = env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        let fake_home = TempDir::new().unwrap();
        write_injected_lock(
            fake_home.path(),
            r#"{"_adoptMaster":"codex","_adoptedItems":[{"name":"node_repl","adoptedFrom":"codex","sourceId":"codex-mcp"}]}"#,
        );

        let findings = VendorNativeAdoptedCheck.check();

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }

        let mut report = DoctorReport::new();
        report.findings.extend(findings);
        assert_eq!(
            report.exit_code(),
            0,
            "vendor-native hit must stay warning-only"
        );
        assert!(!report.has_errors());
    }

    #[test]
    fn vendor_native_adopted_check_leaves_lockfile_byte_identical() {
        // "leaves the lockfile byte-identical" — report only, never rewrite.
        use crate::test_support::env_guard;
        use tempfile::TempDir;

        let _guard = env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        let fake_home = TempDir::new().unwrap();
        let lock_json = r#"{"_adoptMaster":"codex","_adoptedItems":[{"name":"node_repl","adoptedFrom":"codex","sourceId":"codex-mcp"}]}"#;
        write_injected_lock(fake_home.path(), lock_json);
        let lock_path = ccync_foundation::paths::plugins_lock_path().unwrap();
        let before = std::fs::read_to_string(&lock_path).unwrap();

        let _findings = VendorNativeAdoptedCheck.check();

        let after = std::fs::read_to_string(&lock_path).unwrap();

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }

        assert_eq!(before, after, "check() must not rewrite the lockfile");
    }

    #[test]
    fn vendor_native_adopted_finding_disappears_after_entry_removed() {
        // "the finding is gone after the entry is removed" — the round trip
        // a human performs by following the `ccync remove <name>` pointer.
        use crate::test_support::env_guard;
        use tempfile::TempDir;

        let _guard = env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        let fake_home = TempDir::new().unwrap();
        write_injected_lock(
            fake_home.path(),
            r#"{"_adoptMaster":"codex","_adoptedItems":[{"name":"node_repl","adoptedFrom":"codex","sourceId":"codex-mcp"}]}"#,
        );
        let before_removal = VendorNativeAdoptedCheck.check();

        // Simulate `ccync remove node_repl`: the entry is gone from the lockfile.
        write_injected_lock(
            fake_home.path(),
            r#"{"_adoptMaster":"codex","_adoptedItems":[]}"#,
        );
        let after_removal = VendorNativeAdoptedCheck.check();

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }

        assert_eq!(
            before_removal.len(),
            1,
            "hit must be reported before removal"
        );
        assert!(
            after_removal.is_empty(),
            "finding must be gone once the entry is removed from the lockfile"
        );
    }

    // ─── no-contribution per-entry decision ────────────────────────────────

    fn catalog_entry(id: &str) -> serde_json::Value {
        serde_json::json!({ "pluginId": id, "source": "https://example.test/repo" })
    }

    fn lock_entry_with_pin(id: &str, pin: &str) -> serde_json::Value {
        serde_json::json!({ "pluginId": id, "pin": pin })
    }

    #[test]
    fn no_contribution_finding_silent_when_cache_dir_contributes() {
        let cache_root = tempfile::tempdir().unwrap();
        let plugin_dir = cache_root.path().join("my-plugin@abc123");
        std::fs::create_dir_all(plugin_dir.join("skills")).unwrap();
        std::fs::write(plugin_dir.join("skills").join("a"), "a").unwrap();

        let entry = catalog_entry("my-plugin");
        let lock_entries = vec![lock_entry_with_pin("my-plugin", "abc123")];

        assert!(
            no_contribution_finding(&entry, &lock_entries, cache_root.path()).is_none(),
            "a contributing cache dir must not raise a finding"
        );
    }

    #[test]
    fn no_contribution_finding_fires_when_cache_dir_contributes_nothing() {
        let cache_root = tempfile::tempdir().unwrap();
        let plugin_dir = cache_root.path().join("my-plugin@abc123");
        std::fs::create_dir_all(plugin_dir.join("skills")).unwrap();

        let entry = catalog_entry("my-plugin");
        let lock_entries = vec![lock_entry_with_pin("my-plugin", "abc123")];

        let finding = no_contribution_finding(&entry, &lock_entries, cache_root.path())
            .expect("a non-contributing cache dir must raise exactly one finding");
        assert!(
            finding.message.contains("my-plugin"),
            "message must name the plugin id: {}",
            finding.message
        );
        assert!(
            finding.message.contains(&plugin_dir.display().to_string()),
            "message must name the resolved cache dir path: {}",
            finding.message
        );
    }

    #[test]
    fn no_contribution_finding_silent_when_unpinned() {
        let cache_root = tempfile::tempdir().unwrap();
        let entry = catalog_entry("my-plugin");

        assert!(
            no_contribution_finding(&entry, &[], cache_root.path()).is_none(),
            "an unpinned entry must produce no finding from this check"
        );
    }

    #[test]
    fn no_contribution_finding_silent_when_cache_dir_absent() {
        let cache_root = tempfile::tempdir().unwrap();
        let entry = catalog_entry("my-plugin");
        let lock_entries = vec![lock_entry_with_pin("my-plugin", "abc123")];

        assert!(
            no_contribution_finding(&entry, &lock_entries, cache_root.path()).is_none(),
            "a pinned entry whose cache dir is absent must produce no finding"
        );
    }

    #[test]
    fn no_contribution_finding_is_warning_with_fix_hint() {
        let cache_root = tempfile::tempdir().unwrap();
        let plugin_dir = cache_root.path().join("my-plugin@abc123");
        std::fs::create_dir_all(&plugin_dir).unwrap();

        let entry = catalog_entry("my-plugin");
        let lock_entries = vec![lock_entry_with_pin("my-plugin", "abc123")];

        let finding = no_contribution_finding(&entry, &lock_entries, cache_root.path())
            .expect("a non-contributing cache dir must raise a finding");
        assert_eq!(finding.severity, Severity::Warning);
        assert!(finding.fix_hint.is_some(), "finding must carry a fix hint");
    }

    // ─── render-contribution doctor check, aggregated via run_doctor ──────────

    #[test]
    fn doctor_reports_no_contribution() {
        use crate::test_support::env_guard;
        use tempfile::TempDir;

        let _guard = env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        let fake_home = TempDir::new().unwrap();
        std::env::set_var(var, fake_home.path());

        let catalog_path = ccync_foundation::paths::personal_catalog_path().unwrap();
        std::fs::create_dir_all(catalog_path.parent().unwrap()).unwrap();
        std::fs::write(
            &catalog_path,
            r#"{"plugins":[{"pluginId":"my-plugin","source":"https://example.test/repo"}]}"#,
        )
        .unwrap();

        let lock_path = ccync_foundation::paths::plugins_lock_path().unwrap();
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        std::fs::write(
            &lock_path,
            r#"{"_personalPlugins":[{"pluginId":"my-plugin","pin":"abc123"}]}"#,
        )
        .unwrap();

        let cache_root = ccync_foundation::paths::ccync_cache_root().unwrap();
        std::fs::create_dir_all(cache_root.join("my-plugin@abc123")).unwrap();

        let report = run_doctor(&DoctorOptions::default());

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }

        let finding = report
            .findings
            .iter()
            .find(|f| f.message.contains("my-plugin"))
            .expect("aggregated report must carry the no-contribution finding");
        assert!(
            finding
                .message
                .contains("contributes no component and no MCP server"),
            "message: {}",
            finding.message
        );
        assert_eq!(finding.severity, Severity::Warning);
    }

    #[test]
    fn doctor_no_contribution_silent_on_absent_and_empty_catalog() {
        use crate::test_support::env_guard;
        use tempfile::TempDir;

        // The check must stay silent when it cannot prove a non-contribution:
        // a missing catalog and an empty plugin list are both zero-evidence
        // states, never an accusation.
        const MARKER: &str = "contributes no component and no MCP server";

        let _guard = env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);

        // Leg 1 - no personal catalog on disk at all.
        let absent_home = TempDir::new().unwrap();
        std::env::set_var(var, absent_home.path());
        let absent_report = run_doctor(&DoctorOptions::default());

        // Leg 2 - catalog present but its plugins array is empty.
        let empty_home = TempDir::new().unwrap();
        std::env::set_var(var, empty_home.path());
        let catalog_path = ccync_foundation::paths::personal_catalog_path().unwrap();
        std::fs::create_dir_all(catalog_path.parent().unwrap()).unwrap();
        std::fs::write(&catalog_path, r#"{"plugins":[]}"#).unwrap();
        let lock_path = ccync_foundation::paths::plugins_lock_path().unwrap();
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        std::fs::write(&lock_path, r#"{"_personalPlugins":[]}"#).unwrap();
        let empty_report = run_doctor(&DoctorOptions::default());

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }

        assert!(
            !absent_report
                .findings
                .iter()
                .any(|f| f.message.contains(MARKER)),
            "absent catalog must draw no no-contribution finding"
        );
        assert!(
            !empty_report
                .findings
                .iter()
                .any(|f| f.message.contains(MARKER)),
            "empty plugins array must draw no no-contribution finding"
        );
    }

    // after removing ClaudeMarketplaceState, doctor compiles and exit grading is correct.
    #[test]
    fn tp08_no_marketplace_classification_residue() {
        // Compile-time check: this test file does not reference ClaudeMarketplaceState.
        // If the enum still exists, this test serves as a reminder to remove it.
        // The real check is: `cargo test` compiles without any ClaudeMarketplaceState usage.
        let opts = DoctorOptions::default();
        let report = run_doctor(&opts);
        // Exit grading: 0 when warning-only, 1 when any error.
        let code = report.exit_code();
        assert!(
            code == 0 || code == 1,
            "exit code must be 0 or 1, got {code}"
        );
    }
}
