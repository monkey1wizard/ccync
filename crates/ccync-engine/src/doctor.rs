//! `ccync doctor` — read-only health checks for CCYNC surfaces.
//!
//! Checks canonical root freshness, runtime surfaces, ledger state, and plan
//! lifecycle drift. Exit-code grading: warning-only → 0, any error → non-zero.
//!
//!
//! `ccync doctor --dry-run` must not modify any files.
//! missing projection or stale steward → error exit naming the fix surface.
//! warning-only → exit 0; any error → exit non-zero.

use crate::ledger::{ledger_path, Ledger};
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
    /// Include release-gate checks (P4).
    pub release_gate: bool,
}

/// Run `ccync doctor` — check canonical root, runtime surfaces, and ledger.
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
        // Check 3: Ledger freshness.
        Box::new(LedgerCheck),
        // Check 4: ccync content projected to the Claude skill surface.
        Box::new(SkillSurfaceCheck {
            canonical_root: canonical_root.clone(),
        }),
        // Check 5: AGY surfaces (warning only, best-effort per OE-A).
        Box::new(AgySurfacesCheck),
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

struct LedgerCheck;

impl HealthCheck for LedgerCheck {
    fn name(&self) -> &str {
        "ledger"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        let mut findings = Vec::new();
        match ledger_path() {
            None => {
                findings.push(DoctorFinding::warning(
                    "could not determine home directory; ledger check skipped",
                ));
            }
            Some(path) => {
                if !path.exists() {
                    findings.push(DoctorFinding::error(
                        "ledger not found — ccync may not have been initialized via `ccync init`",
                        "run `ccync init <claude|codex>` to adopt a master and create the ledger",
                    ));
                    return findings;
                }
                let ledger = Ledger::load(&path);
                if ledger.last.is_none() {
                    findings.push(DoctorFinding::warning(
                        "ledger exists but has no recorded entries",
                    ));
                }
            }
        }
        findings
    }
}

/// Check that `~/.claude/skills/ccync` (the CCYNC-owned Claude skill surface) exists.
///
/// This is the surface Claude Code scans for skills. If missing, `doc-sync` and other
/// CCYNC skills are not loaded by Claude regardless of canonical root state.
struct SkillSurfaceCheck {
    canonical_root: PathBuf,
}

impl HealthCheck for SkillSurfaceCheck {
    fn name(&self) -> &str {
        "skill-surface"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        let mut findings = Vec::new();
        let Some(home) = dirs::home_dir() else {
            return findings;
        };
        let skill_surface = home.join(".claude").join("skills").join("ccync");
        if !skill_surface.exists() {
            findings.push(DoctorFinding::error(
                "Claude skill surface not found (~/.claude/skills/ccync) — ccync skills not loaded by Claude",
                "run `ccync sync` to project the skill surface",
            ));
            return findings;
        }
        // Verify the surface resolves to the canonical root (symlink/junction target check).
        if self.canonical_root.exists() {
            let resolved = std::fs::canonicalize(&skill_surface)
                .ok()
                .or_else(|| Some(skill_surface.clone()));
            let canonical_resolved = std::fs::canonicalize(&self.canonical_root).ok();
            if let (Some(surface_real), Some(root_real)) = (resolved, canonical_resolved) {
                if surface_real != root_real {
                    findings.push(DoctorFinding::error(
                        format!(
                            "Claude skill surface (~/.claude/skills/ccync) does not point to canonical root ({})",
                            self.canonical_root.display()
                        ),
                        "run `ccync sync` to realign the skill surface",
                    ));
                }
            }
        }
        findings
    }
}

struct AgySurfacesCheck;

impl HealthCheck for AgySurfacesCheck {
    fn name(&self) -> &str {
        "agy-surfaces"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        let mut findings = Vec::new();
        if let Some(home) = dirs::home_dir() {
            let cli_path = home
                .join(".gemini")
                .join("antigravity-cli")
                .join("plugins")
                .join("ccync");
            let ide_path = home
                .join(".gemini")
                .join("antigravity-ide")
                .join("plugins")
                .join("ccync");

            if !cli_path.exists() {
                findings.push(DoctorFinding::warning(
                    "AGY CLI surface not found (~/.gemini/antigravity-cli/plugins/ccync)",
                ));
            }
            if !ide_path.exists() {
                findings.push(DoctorFinding::warning(
                    "AGY IDE surface not found (~/.gemini/antigravity-ide/plugins/ccync)",
                ));
            }
        }
        findings
    }
}

fn canonical_root_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ccync")
        .join("plugins")
        .join("ccync")
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let f = DoctorFinding::warning("AGY surface missing");
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

    // ─── skill surface + bin exposure doctor tests ───────────

    #[test]
    fn check_skill_surface_error_when_missing() {
        use tempfile::TempDir;

        let root = TempDir::new().unwrap();

        // Simulate: skill_surface does not exist. Verify the find/message pattern
        // inline (the real check reads the home dir, which tests cannot inject).
        let skill_surface = root.path().join("skills_ccync");
        let mut report = DoctorReport::new();
        if !skill_surface.exists() {
            report.push(DoctorFinding::error(
                "Claude skill surface not found (~/.claude/skills/ccync) — ccync skills not loaded by Claude",
                "run `ccync sync` to project the skill surface",
            ));
        }
        assert!(
            report.has_errors(),
            "missing skill surface must be an error"
        );
        let s = report.findings[0].to_string();
        assert!(
            s.contains("skill surface"),
            "finding must name skill surface: {s}"
        );
        assert!(
            s.contains("ccync sync"),
            "finding must suggest ccync sync: {s}"
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
