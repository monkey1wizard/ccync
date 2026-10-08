// AGY (Antigravity) plugin-junction projection
//
// All three Antigravity faces consume a whole-plugin junction into the
// canonical root, gated per runtime key:
// 1. CLI junction (gated by `agy-cli`): ~/.gemini/antigravity-cli/plugins/ccync -> canonical root
// 2. IDE junction (gated by `agy-ide`): ~/.gemini/antigravity-ide/plugins/ccync -> canonical root
// 3. GUI junction (gated by `agy-gui`): ~/.gemini/config/plugins/ccync -> canonical root
//
// The GUI face previously read decomposed loose skills + its own mcp_config.json
// instead of a junction; that decomposition is retired in favor of the same
// junction mechanism CLI/IDE already use (`gui_target` below). The former
// `~/.gemini/commands/*.toml` GUI-config face was a misprojection into Gemini
// CLI's command dir and has been removed.
//
// `AgyProjection::apply` itself still only orchestrates the CLI/IDE pair
// (sequential: a CLI failure short-circuits before the IDE junction is ever
// attempted, and an IDE failure rolls back an already-created CLI link).
// `gui_target` records where the GUI junction resolves to (used by `remove`'s
// own teardown of all three, and by tests); the junction is actually created
// and torn down by `update_skills` itself via the shared, dry-run-aware
// `ensure_dir_link` helper, gated on exactly `agy-gui`, independent of the
// CLI/IDE `any_agy` gate — the GUI face intentionally does not route through
// `apply`/a dedicated `create_gui_junction` method. Either outcome is
// reported as evidence, never a panic.
//
// CLI and IDE both publish through `support::publish_agy_root`'s two-rung
// helper (symlink, else a fully-managed directory copy) rather than an
// NTFS-junction-only creator: `apply` takes an ownership handle and a
// `dry_run` flag, and the shape each root actually landed as (`Symlink` or
// `Directory`) is recorded into evidence as `Linked`/`Written` respectively.

use crate::support::AgyRootOwnership;
use crate::{
    ProjectionEvidence, ProjectionExpectedFailure, ProjectionFailure, ProjectionPathChange,
    ProjectionPathResult, ProjectionReport, ProjectionTargetEvidence,
};
use ccync_foundation::platform::LinkShape;
use ccync_foundation::target::ProjectionTargetId;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgyError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Junction/symlink creation failed: {0}")]
    LinkCreation(String),
}

/// AGY plugin-junction projection (CLI + IDE faces; gated per runtime key)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgyProjection {
    /// Path to canonical plugin root to link to
    pub canonical_root: PathBuf,

    /// CLI junction target: ~/.gemini/antigravity-cli/plugins/ccync
    pub cli_target: PathBuf,

    /// IDE junction target: ~/.gemini/antigravity-ide/plugins/ccync
    pub ide_target: PathBuf,

    /// GUI junction target: ~/.gemini/config/plugins/ccync
    pub gui_target: PathBuf,
}

impl AgyProjection {
    /// Create a new AGY projection configuration
    pub fn new(canonical_root: PathBuf) -> Result<Self, AgyError> {
        let home = ccync_foundation::paths::user_home().ok_or_else(|| {
            AgyError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Could not determine home directory",
            ))
        })?;

        let cli_target = home
            .join(".gemini")
            .join("antigravity-cli")
            .join("plugins")
            .join("ccync");
        let ide_target = home
            .join(".gemini")
            .join("antigravity-ide")
            .join("plugins")
            .join("ccync");
        let gui_target = home
            .join(".gemini")
            .join("config")
            .join("plugins")
            .join("ccync");

        Ok(Self {
            canonical_root,
            cli_target,
            ide_target,
            gui_target,
        })
    }

    /// Publish the CLI root (~/.gemini/antigravity-cli/plugins/ccync) through
    /// the two-rung helper: symlink where the platform permits, else a
    /// fully-managed directory copy. `ownership` records the copy rung's
    /// ownership row on disk before the tree becomes visible; `dry_run`
    /// performs zero disk/lockfile writes. Returns the shape that landed (or
    /// that a dry run predicts). Parent-directory creation is the helper's
    /// own concern — it creates `dest`'s parent itself before either rung.
    fn publish_cli_root(
        &self,
        ownership: &AgyRootOwnership,
        dry_run: bool,
        report: &mut ProjectionReport,
    ) -> Result<(LinkShape, bool), AgyError> {
        crate::support::publish_agy_root(
            &self.canonical_root,
            &self.cli_target,
            ownership,
            dry_run,
            report,
        )
        .map_err(|e| AgyError::LinkCreation(format!("CLI link creation failed: {e}")))
    }

    /// Publish the IDE root (~/.gemini/antigravity-ide/plugins/ccync) through
    /// the two-rung helper. See [`Self::publish_cli_root`] for the shared
    /// mechanism.
    fn publish_ide_root(
        &self,
        ownership: &AgyRootOwnership,
        dry_run: bool,
        report: &mut ProjectionReport,
    ) -> Result<(LinkShape, bool), AgyError> {
        crate::support::publish_agy_root(
            &self.canonical_root,
            &self.ide_target,
            ownership,
            dry_run,
            report,
        )
        .map_err(|e| AgyError::LinkCreation(format!("IDE link creation failed: {e}")))
    }

    /// Create or update the AGY plugin junctions, reporting evidence instead
    /// of a bare `Result<(), AgyError>`.
    ///
    /// This method runs on an already-constructed `AgyProjection` — any fault
    /// in building the projection itself (missing home directory, etc.)
    /// happens in [`AgyProjection::new`], before this method is ever called,
    /// so there is no "constructor fault" branch here. The faults this method
    /// actually produces are publish faults from `publish_cli_root` /
    /// `publish_ide_root`, e.g. a fault forcing rollback of the CLI root when
    /// the IDE root fails. Both are expected, owned-path failures —
    /// represented as evidence values on the relevant target, never as the
    /// outer `ProjectionFailure`.
    ///
    /// `ownership` is the ownership handle each root's publish records
    /// through when it falls back to a directory copy; `dry_run` is threaded
    /// straight into both publish calls, which themselves are the sole
    /// authority on what zero-write means (see `support::publish_agy_root`).
    ///
    /// CLI is attempted first. If it fails, evidence records the failure on the
    /// CLI target and the IDE target is never attempted (short-circuit) — the
    /// IDE target stays absent from `completed_targets` (`NotRun`).
    ///
    /// A root whose publish call preserved an existing unowned real directory
    /// (nothing was written) is recorded as `skipped`, not `Written`/`Linked`,
    /// and is never added to the rollback list — there is nothing of ours to
    /// undo. A root that was actually published (symlink or copy) records
    /// `Linked` or `Written` respectively and joins the rollback list.
    ///
    /// If CLI succeeds (or was not selected) and IDE then fails, a CLI root
    /// this call actually published is rolled back; a CLI root this call only
    /// preserved is left untouched (there is nothing to roll back). The CLI
    /// target still gets recorded as completed either way: the CLI phase
    /// itself genuinely succeeded at the time it ran, and a later target's
    /// failure triggering rollback does not retroactively make the CLI step's
    /// own outcome a failure. Only the target that actually failed (IDE) gets
    /// `failure` evidence; this matches the rule that failure/skip only wins
    /// for the target whose own step failed.
    ///
    /// When neither `cli` nor `ide` is selected, nothing is attempted and this
    /// returns `Ok(ProjectionEvidence::default())`.
    pub fn apply(
        &self,
        cli: bool,
        ide: bool,
        ownership: &AgyRootOwnership,
        dry_run: bool,
    ) -> Result<ProjectionEvidence, ProjectionFailure> {
        let mut evidence = ProjectionEvidence::default();
        let mut created_links = Vec::new();

        if cli {
            let mut report = ProjectionReport::default();
            match self.publish_cli_root(ownership, dry_run, &mut report) {
                Ok((shape, wrote)) => {
                    if report.warnings.is_empty() {
                        // Record a path event, and enrol for rollback, only
                        // when this call actually wrote. An already-correct
                        // root is a no-op: reporting it would claim a change
                        // that never happened, and rolling it back would
                        // delete a root this call did not create.
                        if wrote {
                            evidence
                                .by_target
                                .entry(ProjectionTargetId::AgyCli)
                                .or_default()
                                .paths
                                .push(ProjectionPathResult {
                                    path: self.cli_target.clone(),
                                    change: shape_to_path_change(shape),
                                });
                            created_links.push(AgyLink::CliLink);
                        }
                    } else {
                        let messages: Vec<String> =
                            report.warnings.iter().map(|w| w.message.clone()).collect();
                        evidence.by_target.insert(
                            ProjectionTargetId::AgyCli,
                            ProjectionTargetEvidence::new(
                                Vec::new(),
                                messages,
                                Some("preserved user-owned path".to_string()),
                                None,
                            )
                            .expect("skipped/failure exclusivity: only skipped is set"),
                        );
                    }
                    evidence
                        .completed_targets
                        .insert(ProjectionTargetId::AgyCli);
                }
                Err(err) => {
                    let failure = ProjectionExpectedFailure {
                        path: Some(self.cli_target.clone()),
                        reason: err.to_string(),
                    };
                    evidence.by_target.insert(
                        ProjectionTargetId::AgyCli,
                        ProjectionTargetEvidence::new(Vec::new(), Vec::new(), None, Some(failure))
                            .expect("skipped/failure exclusivity: only failure is set"),
                    );
                    // Short-circuit: CLI failing means IDE is never attempted.
                    return Ok(evidence);
                }
            }
        }

        if ide {
            let mut report = ProjectionReport::default();
            match self.publish_ide_root(ownership, dry_run, &mut report) {
                Ok((shape, wrote)) => {
                    if report.warnings.is_empty() {
                        // Same no-op rule as the CLI arm above: an
                        // already-correct root records nothing.
                        if wrote {
                            evidence
                                .by_target
                                .entry(ProjectionTargetId::AgyIde)
                                .or_default()
                                .paths
                                .push(ProjectionPathResult {
                                    path: self.ide_target.clone(),
                                    change: shape_to_path_change(shape),
                                });
                        }
                    } else {
                        let messages: Vec<String> =
                            report.warnings.iter().map(|w| w.message.clone()).collect();
                        evidence.by_target.insert(
                            ProjectionTargetId::AgyIde,
                            ProjectionTargetEvidence::new(
                                Vec::new(),
                                messages,
                                Some("preserved user-owned path".to_string()),
                                None,
                            )
                            .expect("skipped/failure exclusivity: only skipped is set"),
                        );
                    }
                    evidence
                        .completed_targets
                        .insert(ProjectionTargetId::AgyIde);
                }
                Err(err) => {
                    // The compensating `Removed` event is only appended when
                    // the rollback actually removed the CLI root from disk —
                    // never inferred from evidence shape alone. A failed
                    // rollback (e.g. a locked file inside a directory copy)
                    // leaves the original `Linked`/`Written` event standing,
                    // so evidence never claims more than what happened.
                    let rollback_succeeded = self.rollback_links(&created_links).is_ok();
                    // A rolled-back root leaves its pre-publish ownership row
                    // behind, and that row is a standing `remove_dir_all`
                    // authority over whatever later occupies the path. Release
                    // it only when this call both published the CLI root and
                    // proved the removal succeeded — forgetting a row this call
                    // did not write would strip a genuinely owned copy of the
                    // proof teardown needs.
                    if rollback_succeeded
                        && created_links
                            .iter()
                            .any(|link| matches!(link, AgyLink::CliLink))
                    {
                        let _ = ownership.forget_now(&self.cli_target, dry_run);
                    }
                    compensate_rolled_back_cli(&mut evidence, rollback_succeeded);
                    let failure = ProjectionExpectedFailure {
                        path: Some(self.ide_target.clone()),
                        reason: err.to_string(),
                    };
                    evidence.by_target.insert(
                        ProjectionTargetId::AgyIde,
                        ProjectionTargetEvidence::new(Vec::new(), Vec::new(), None, Some(failure))
                            .expect("skipped/failure exclusivity: only failure is set"),
                    );
                    return Ok(evidence);
                }
            }
        }

        Ok(evidence)
    }

    /// Check that all three plugin junctions (CLI, IDE, GUI) exist.
    pub fn verify_links_exist(&self) -> bool {
        self.cli_target.exists() && self.ide_target.exists() && self.gui_target.exists()
    }

    /// Remove all three AGY plugin roots (reverse of apply plus the GUI
    /// root `update_skills` creates), regardless of which shape each was
    /// published as — symbolic link, owned directory copy, or legacy
    /// junction.
    ///
    /// Routes through [`crate::support::remove_agy_root`]: a symlink or
    /// junction is unlinked without following through to `canonical_root`;
    /// a real directory is deleted — and its ownership row forgotten, only
    /// after the delete succeeds — solely when `ownership` recorded it
    /// (`AgyRootOwnership::is_owned`); an unrecorded real directory is
    /// preserved untouched with a warning, including on a machine with no
    /// lockfile at all (an empty/missing prior authorizes nothing). This is
    /// a different safety property from `rollback_links`/
    /// `remove_link_if_present`: those undo what one `apply` call just
    /// created in the same run, so no ownership check is needed; this acts
    /// on persisted, cross-run state, where an unrecorded real directory
    /// could just as easily be the user's own.
    ///
    /// `dry_run` performs zero disk writes and zero lockfile writes.
    ///
    /// Best-effort: continues on partial failures, returns the first error
    /// encountered after attempting all three.
    pub fn remove(&self, ownership: &AgyRootOwnership, dry_run: bool) -> Result<(), AgyError> {
        self.remove_reporting(ownership, dry_run, &mut Vec::new())
    }

    /// Same teardown as [`Self::remove`], but hands the caller the warnings it
    /// produced instead of dropping them.
    ///
    /// The warning that matters is "preserved user-owned path": a root ccync
    /// published but can no longer prove it owns (a hand-edited or corrupted
    /// lockfile) is left standing, and because it is a self-contained
    /// directory copy, Antigravity keeps loading the whole ccync bundle out of
    /// it. Swallowing that warning during `ccync uninstall` is how an
    /// abandoned live plugin root becomes invisible — the user is told the
    /// uninstall succeeded and has nothing pointing at the leftover.
    pub fn remove_reporting(
        &self,
        ownership: &AgyRootOwnership,
        dry_run: bool,
        warnings: &mut Vec<String>,
    ) -> Result<(), AgyError> {
        let mut first_error = None;
        let mut report = ProjectionReport::default();

        for target in [&self.cli_target, &self.ide_target, &self.gui_target] {
            if let Err(err) =
                crate::support::remove_agy_root(target, ownership, dry_run, &mut report)
            {
                if first_error.is_none() {
                    first_error = Some(AgyError::LinkCreation(format!(
                        "AGY root removal failed: {err}"
                    )));
                }
            }
        }

        warnings.extend(report.warnings.iter().map(|w| w.message.clone()));

        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Undo whatever `apply` just published, in reverse order. Returns the
    /// first removal error encountered (after attempting all of `links`) so
    /// the caller can tell a genuine rollback from a failed one — never
    /// discarded, unlike the prior `let _ =` here, which let a rolled-back
    /// copy silently survive on disk while evidence still claimed it was
    /// gone.
    fn rollback_links(&self, links: &[AgyLink]) -> std::io::Result<()> {
        let mut first_error = None;
        for link in links.iter().rev() {
            match link {
                AgyLink::CliLink => {
                    if let Err(err) = remove_link_if_present(&self.cli_target) {
                        if first_error.is_none() {
                            first_error = Some(err);
                        }
                    }
                }
            }
        }
        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

#[derive(Debug, Clone)]
enum AgyLink {
    CliLink,
}

/// Append a compensating `Removed` path event onto `AgyCli`'s existing
/// `Linked`/`Written` event — but only when `rollback_succeeded` is true.
/// Extracted out of `apply`'s IDE-failure branch so this decision is
/// independently testable without racing real filesystem timing: the
/// production call site passes `self.rollback_links(&created_links).is_ok()`
/// directly, so a test can drive both outcomes deterministically. A no-op
/// (never published this call, or nothing found to compensate) leaves
/// `evidence` untouched either way.
fn compensate_rolled_back_cli(evidence: &mut ProjectionEvidence, rollback_succeeded: bool) {
    if !rollback_succeeded {
        return;
    }
    if let Some(cli_evidence) = evidence.by_target.get_mut(&ProjectionTargetId::AgyCli) {
        if let Some(last_path) = cli_evidence
            .paths
            .last()
            .filter(|p| {
                matches!(
                    p.change,
                    ProjectionPathChange::Written | ProjectionPathChange::Linked
                )
            })
            .map(|p| p.path.clone())
        {
            cli_evidence.paths.push(ProjectionPathResult {
                path: last_path,
                change: ProjectionPathChange::Removed,
            });
        }
    }
}

/// Map a published root's landed shape to the evidence path-change it
/// represents: a symbolic link is `Linked`; a directory copy (the only other
/// shape [`crate::support::publish_agy_root`] can actually return from a
/// write branch) is `Written`. `Junction`/`Absent` are unreachable from a
/// successful publish call but are mapped to `Written` rather than making
/// this conversion partial.
fn shape_to_path_change(shape: LinkShape) -> ProjectionPathChange {
    match shape {
        LinkShape::Symlink => ProjectionPathChange::Linked,
        LinkShape::Directory | LinkShape::Junction | LinkShape::Absent => {
            ProjectionPathChange::Written
        }
    }
}

/// Unconditionally undo whatever `rollback_links`' own `apply` call just
/// published at `path`, moments ago, in this same call — never a persisted,
/// ownership-ambiguous artifact (that is `support`'s teardown helper's job,
/// not this one's). Must handle every shape a rung-1/rung-2 publish can
/// leave: a symbolic link or legacy junction (unlink only, never follow
/// through to `canonical_root`), or a real directory copy — the shape
/// `ccync_foundation::platform::remove_dir_link`'s Windows `rmdir` cannot
/// remove once it holds more than one entry, which a same-call rollback of a
/// populated copy always does.
fn remove_link_if_present(path: &Path) -> std::io::Result<()> {
    if ccync_foundation::platform::is_symlink_or_junction(path) {
        ccync_foundation::platform::remove_dir_link(path)
    } else if path.is_dir() {
        fs::remove_dir_all(path)
    } else if path.exists() {
        fs::remove_file(path)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_agy_projection_new() {
        let temp_dir = TempDir::new().unwrap();
        let canonical_root = temp_dir.path().to_path_buf();

        let projection = AgyProjection::new(canonical_root.clone()).unwrap();

        assert_eq!(projection.canonical_root, canonical_root);
        assert!(projection.cli_target.to_string_lossy().contains(".gemini"));
        assert!(projection
            .cli_target
            .to_string_lossy()
            .contains("antigravity-cli"));
        assert!(projection.ide_target.to_string_lossy().contains(".gemini"));
        assert!(projection
            .ide_target
            .to_string_lossy()
            .contains("antigravity-ide"));
        assert!(projection.gui_target.to_string_lossy().contains(".gemini"));
        assert!(projection.gui_target.to_string_lossy().contains("config"));
        assert!(projection.gui_target.to_string_lossy().contains("plugins"));
    }

    #[test]
    fn test_verify_links_exist_when_missing() {
        let temp_dir = TempDir::new().unwrap();
        let canonical_root = temp_dir.path().to_path_buf();

        // Use explicitly non-existent paths inside temp_dir to avoid
        // depending on real HOME state (AGY surfaces may exist from a
        // prior installation on this machine).
        let projection = AgyProjection {
            canonical_root: canonical_root.clone(),
            cli_target: temp_dir.path().join("nonexistent_cli"),
            ide_target: temp_dir.path().join("nonexistent_ide"),
            gui_target: temp_dir.path().join("nonexistent_gui"),
        };

        // Should return false when links don't exist
        assert!(!projection.verify_links_exist());
    }

    /// Helper: build an AgyProjection rooted at isolated temp dirs (no real HOME).
    fn isolated_projection(canonical_root: &Path, home: &Path) -> AgyProjection {
        AgyProjection {
            canonical_root: canonical_root.to_path_buf(),
            cli_target: home
                .join(".gemini")
                .join("antigravity-cli")
                .join("plugins")
                .join("ccync"),
            ide_target: home
                .join(".gemini")
                .join("antigravity-ide")
                .join("plugins")
                .join("ccync"),
            gui_target: home
                .join(".gemini")
                .join("config")
                .join("plugins")
                .join("ccync"),
        }
    }

    /// Fresh `AgyRootOwnership` over a throwaway lockfile path under `dir` —
    /// no prior lockfile, so every test starts from an empty ownership state.
    fn fresh_ownership(dir: &Path) -> AgyRootOwnership {
        AgyRootOwnership::from_lockfile(&dir.join("lock.json"))
    }

    #[test]
    fn apply_cli_only_does_not_create_ide_junction() {
        let temp_canonical = TempDir::new().unwrap();
        let canonical_root = temp_canonical.path().to_path_buf();
        fs::create_dir_all(canonical_root.join("skills")).unwrap();
        let temp_home = TempDir::new().unwrap();

        let projection = isolated_projection(&canonical_root, temp_home.path());
        let ownership = fresh_ownership(temp_home.path());
        projection.apply(true, false, &ownership, false).unwrap();

        assert!(
            projection.cli_target.exists(),
            "agy-cli junction must be created"
        );
        assert!(
            !projection.ide_target.exists(),
            "agy-ide junction must NOT be created when ide is not selected"
        );
        // The deleted GUI face must never write into Gemini CLI's command dir.
        assert!(
            !temp_home.path().join(".gemini").join("commands").exists(),
            "Antigravity projection must not write ~/.gemini/commands"
        );
    }

    #[test]
    fn apply_both_creates_both_junctions() {
        let temp_canonical = TempDir::new().unwrap();
        let canonical_root = temp_canonical.path().to_path_buf();
        fs::create_dir_all(canonical_root.join("skills")).unwrap();
        let temp_home = TempDir::new().unwrap();

        let projection = isolated_projection(&canonical_root, temp_home.path());
        let ownership = fresh_ownership(temp_home.path());
        projection.apply(true, true, &ownership, false).unwrap();

        assert!(
            projection.cli_target.exists(),
            "agy-cli junction must be created"
        );
        assert!(
            projection.ide_target.exists(),
            "agy-ide junction must be created"
        );
        assert!(
            !projection.verify_links_exist(),
            "the GUI junction is created by update_skills, not apply — all three \
             must be present before verify_links_exist reports true"
        );
        fs::create_dir_all(projection.gui_target.parent().unwrap()).unwrap();
        ccync_foundation::platform::create_dir_link(&canonical_root, &projection.gui_target)
            .unwrap();
        assert!(projection.verify_links_exist());
        assert!(
            !temp_home.path().join(".gemini").join("commands").exists(),
            "Antigravity projection must not write ~/.gemini/commands"
        );
    }

    #[test]
    fn apply_neither_selected_is_default_evidence() {
        let temp_canonical = TempDir::new().unwrap();
        let canonical_root = temp_canonical.path().to_path_buf();
        let temp_home = TempDir::new().unwrap();

        let projection = isolated_projection(&canonical_root, temp_home.path());
        let ownership = fresh_ownership(temp_home.path());
        let evidence = projection.apply(false, false, &ownership, false).unwrap();

        assert_eq!(evidence, ProjectionEvidence::default());
    }

    #[test]
    fn apply_both_succeed_records_both_targets_written_and_completed() {
        let temp_canonical = TempDir::new().unwrap();
        let canonical_root = temp_canonical.path().to_path_buf();
        fs::create_dir_all(canonical_root.join("skills")).unwrap();
        let temp_home = TempDir::new().unwrap();

        let projection = isolated_projection(&canonical_root, temp_home.path());
        let ownership = fresh_ownership(temp_home.path());
        let evidence = projection.apply(true, true, &ownership, false).unwrap();

        assert!(evidence
            .completed_targets
            .contains(&ProjectionTargetId::AgyCli));
        assert!(evidence
            .completed_targets
            .contains(&ProjectionTargetId::AgyIde));

        // Shape depends on platform symlink privilege: `Linked` where
        // permitted, `Written` where a copy landed instead. Either is a
        // genuine publish, never a preserve/skip.
        let cli_evidence = evidence.by_target.get(&ProjectionTargetId::AgyCli).unwrap();
        assert_eq!(cli_evidence.paths.len(), 1);
        assert_eq!(cli_evidence.paths[0].path, projection.cli_target);
        assert!(matches!(
            cli_evidence.paths[0].change,
            ProjectionPathChange::Linked | ProjectionPathChange::Written
        ));
        assert!(cli_evidence.failure.is_none());
        assert!(cli_evidence.skipped.is_none());

        let ide_evidence = evidence.by_target.get(&ProjectionTargetId::AgyIde).unwrap();
        assert_eq!(ide_evidence.paths.len(), 1);
        assert_eq!(ide_evidence.paths[0].path, projection.ide_target);
        assert!(matches!(
            ide_evidence.paths[0].change,
            ProjectionPathChange::Linked | ProjectionPathChange::Written
        ));
        assert!(ide_evidence.failure.is_none());
        assert!(ide_evidence.skipped.is_none());
    }

    #[test]
    fn apply_cli_failure_short_circuits_before_ide_is_attempted() {
        let temp_canonical = TempDir::new().unwrap();
        let canonical_root = temp_canonical.path().to_path_buf();
        fs::create_dir_all(canonical_root.join("skills")).unwrap();
        let temp_home = TempDir::new().unwrap();

        // Force CLI junction creation to fail: place a regular file where the
        // CLI plugins directory's parent must be a directory.
        fs::create_dir_all(temp_home.path().join(".gemini")).unwrap();
        fs::write(
            temp_home.path().join(".gemini").join("antigravity-cli"),
            b"not a directory",
        )
        .unwrap();

        let projection = isolated_projection(&canonical_root, temp_home.path());
        let ownership = fresh_ownership(temp_home.path());
        let evidence = projection.apply(true, true, &ownership, false).unwrap();

        assert!(
            !evidence
                .completed_targets
                .contains(&ProjectionTargetId::AgyCli),
            "CLI target must stay NotRun (absent from completed_targets) when its own step fails"
        );
        assert!(
            !evidence
                .completed_targets
                .contains(&ProjectionTargetId::AgyIde),
            "IDE target must stay NotRun — CLI failure short-circuits before IDE is attempted"
        );
        assert!(
            !evidence.by_target.contains_key(&ProjectionTargetId::AgyIde),
            "IDE must never be attempted when CLI fails first"
        );

        let cli_evidence = evidence.by_target.get(&ProjectionTargetId::AgyCli).unwrap();
        assert!(cli_evidence.paths.is_empty());
        assert!(cli_evidence.skipped.is_none());
        let failure = cli_evidence.failure.as_ref().unwrap();
        assert_eq!(
            failure.path.as_deref(),
            Some(projection.cli_target.as_path())
        );
        assert!(
            !projection.ide_target.exists(),
            "IDE junction must never be created"
        );
    }

    #[test]
    fn apply_ide_failure_rolls_back_cli_with_pinned_evidence() {
        let temp_canonical = TempDir::new().unwrap();
        let canonical_root = temp_canonical.path().to_path_buf();
        fs::create_dir_all(canonical_root.join("skills")).unwrap();
        let temp_home = TempDir::new().unwrap();

        // Force IDE junction creation to fail (same technique as the legacy
        // `apply_rolls_back_cli_link_when_ide_junction_fails` test).
        fs::create_dir_all(temp_home.path().join(".gemini")).unwrap();
        fs::write(
            temp_home.path().join(".gemini").join("antigravity-ide"),
            b"not a directory",
        )
        .unwrap();

        let projection = isolated_projection(&canonical_root, temp_home.path());
        let ownership = fresh_ownership(temp_home.path());
        let evidence = projection.apply(true, true, &ownership, false).unwrap();

        // Behavior: the CLI link is physically rolled back on IDE failure.
        assert!(
            !projection.cli_target.exists(),
            "CLI junction must be rolled back after IDE junction failure"
        );

        // Evidence: CLI's own step genuinely completed before the later IDE
        // failure triggered rollback, so it is still recorded as published
        // (Linked or Written, per platform symlink privilege) + completed.
        // Only IDE — the target whose own step failed — carries `failure`
        // evidence. Because this rollback genuinely succeeded on disk (real
        // filesystem, nothing locked), `apply` appends a compensating
        // `Removed` event onto the original publish event — the path history
        // agrees with disk, it is not retroactively edited away.
        assert!(evidence
            .completed_targets
            .contains(&ProjectionTargetId::AgyCli));
        assert!(!evidence
            .completed_targets
            .contains(&ProjectionTargetId::AgyIde));

        let cli_evidence = evidence.by_target.get(&ProjectionTargetId::AgyCli).unwrap();
        assert_eq!(cli_evidence.paths.len(), 2);
        assert!(matches!(
            cli_evidence.paths[0].change,
            ProjectionPathChange::Linked | ProjectionPathChange::Written
        ));
        assert_eq!(cli_evidence.paths[1].change, ProjectionPathChange::Removed);
        assert_eq!(cli_evidence.paths[1].path, cli_evidence.paths[0].path);
        assert!(cli_evidence.failure.is_none());

        let ide_evidence = evidence.by_target.get(&ProjectionTargetId::AgyIde).unwrap();
        assert!(ide_evidence.paths.is_empty());
        let failure = ide_evidence.failure.as_ref().unwrap();
        assert_eq!(
            failure.path.as_deref(),
            Some(projection.ide_target.as_path())
        );
    }

    // Reported-status-level proof of the plan's own "neither run reports a
    // spurious change" Test Case, for `apply` itself: a second consecutive
    // call with no upstream change must record no path event for either
    // root (DEV-04's `wrote` flag), so a stable rerun reads back as
    // unchanged instead of applied.
    #[test]
    fn apply_second_consecutive_call_records_no_path_event_for_either_root() {
        let temp_canonical = TempDir::new().unwrap();
        let canonical_root = temp_canonical.path().to_path_buf();
        fs::create_dir_all(canonical_root.join("skills")).unwrap();
        let temp_home = TempDir::new().unwrap();

        let projection = isolated_projection(&canonical_root, temp_home.path());
        let ownership = fresh_ownership(temp_home.path());

        let first = projection.apply(true, true, &ownership, false).unwrap();
        assert!(!first
            .by_target
            .get(&ProjectionTargetId::AgyCli)
            .unwrap()
            .paths
            .is_empty());
        assert!(!first
            .by_target
            .get(&ProjectionTargetId::AgyIde)
            .unwrap()
            .paths
            .is_empty());

        // Re-load ownership so the second call sees the row the first
        // wrote, exactly as a fresh `ccync sync` process would.
        let ownership = fresh_ownership(temp_home.path());
        let second = projection.apply(true, true, &ownership, false).unwrap();

        let cli_paths_empty = second
            .by_target
            .get(&ProjectionTargetId::AgyCli)
            .map(|t| t.paths.is_empty())
            .unwrap_or(true);
        let ide_paths_empty = second
            .by_target
            .get(&ProjectionTargetId::AgyIde)
            .map(|t| t.paths.is_empty())
            .unwrap_or(true);
        assert!(
            cli_paths_empty,
            "an unchanged republish must record no path event for CLI: {:?}",
            second.by_target.get(&ProjectionTargetId::AgyCli)
        );
        assert!(
            ide_paths_empty,
            "an unchanged republish must record no path event for IDE: {:?}",
            second.by_target.get(&ProjectionTargetId::AgyIde)
        );
        assert!(
            second
                .completed_targets
                .contains(&ProjectionTargetId::AgyCli),
            "a no-op root still completed its own step"
        );
        assert!(second
            .completed_targets
            .contains(&ProjectionTargetId::AgyIde));
    }

    // A CLI root that was a no-op (already correct) must not enter `apply`'s
    // rollback list: rollback undoes what this call created, and a no-op
    // created nothing. Proves the second half of DEV-04 — the first half
    // (`apply_second_consecutive_call_records_no_path_event_for_either_root`)
    // covers evidence; this covers disk survival when IDE then fails.
    #[test]
    fn apply_noop_cli_root_survives_ide_failure_rollback() {
        let temp_canonical = TempDir::new().unwrap();
        let canonical_root = temp_canonical.path().to_path_buf();
        fs::create_dir_all(canonical_root.join("skills")).unwrap();
        let temp_home = TempDir::new().unwrap();

        let projection = isolated_projection(&canonical_root, temp_home.path());
        let ownership = fresh_ownership(temp_home.path());

        // First call publishes CLI for real.
        projection.apply(true, false, &ownership, false).unwrap();
        assert!(projection.cli_target.exists());

        // Second call: CLI is now a no-op (already correct); force IDE to
        // fail so `apply` attempts a rollback.
        fs::create_dir_all(temp_home.path().join(".gemini")).unwrap();
        fs::write(
            temp_home.path().join(".gemini").join("antigravity-ide"),
            b"not a directory",
        )
        .unwrap();
        let ownership = fresh_ownership(temp_home.path());
        let evidence = projection.apply(true, true, &ownership, false).unwrap();

        assert!(
            evidence
                .by_target
                .get(&ProjectionTargetId::AgyIde)
                .unwrap()
                .failure
                .is_some(),
            "IDE must fail as forced"
        );
        assert!(
            projection.cli_target.exists(),
            "a no-op CLI root must survive IDE-failure rollback: rollback \
             undoes what this call created, and a no-op created nothing"
        );
    }

    #[test]
    fn gui_target_resolves_under_gemini_config_plugins() {
        let temp_canonical = TempDir::new().unwrap();
        let canonical_root = temp_canonical.path().to_path_buf();
        let temp_home = TempDir::new().unwrap();

        let projection = isolated_projection(&canonical_root, temp_home.path());

        assert_eq!(
            projection.gui_target,
            temp_home
                .path()
                .join(".gemini")
                .join("config")
                .join("plugins")
                .join("ccync")
        );
    }

    #[test]
    fn remove_tears_down_gui_and_any_symlinked_agy_root() {
        // The GUI junction itself is created by `update_skills` (via the
        // shared, dry-run-aware `ensure_dir_link` helper — see the module
        // doc comment), not by a method on `AgyProjection`; this test creates
        // it directly with the same primitive `update_skills` uses, to prove
        // `remove()` tears it down regardless of which face created it.
        let temp_canonical = TempDir::new().unwrap();
        let canonical_root = temp_canonical.path().to_path_buf();
        fs::create_dir_all(canonical_root.join("skills")).unwrap();
        let temp_home = TempDir::new().unwrap();
        let lockfile = temp_home.path().join("lock.json");

        let projection = isolated_projection(&canonical_root, temp_home.path());
        let ownership = fresh_ownership(temp_home.path());
        projection.apply(true, true, &ownership, false).unwrap();
        fs::create_dir_all(projection.gui_target.parent().unwrap()).unwrap();
        ccync_foundation::platform::create_dir_link(&canonical_root, &projection.gui_target)
            .unwrap();

        // `record_now` (used internally by `apply`) writes straight to the
        // on-disk lockfile without updating this handle's own in-memory
        // snapshot, so reload before `remove()` checks ownership — the same
        // reload every other caller of `record_now` followed by `is_owned`
        // must do.
        let ownership = AgyRootOwnership::from_lockfile(&lockfile);

        projection.remove(&ownership, false).unwrap();

        // `remove()` now walks each target through the shape-aware,
        // ownership-checked teardown helper: a symbolic link or junction is
        // unlinked without following through to `canonical_root`, and an
        // owned directory-copy root is fully removed (its lockfile row
        // forgotten too) rather than silently no-op'd by Windows `rmdir`.
        assert!(
            !projection.cli_target.exists(),
            "CLI root must be gone regardless of which shape it was published as"
        );
        assert!(
            !projection.ide_target.exists(),
            "IDE root must be gone regardless of which shape it was published as"
        );
        assert!(!projection.gui_target.exists());
    }

    // ── rollback error propagation (audit FINDING-001 regression) ──────────

    #[cfg(windows)]
    fn lock_without_delete_sharing(path: &Path) -> fs::File {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_NONE: u32 = 0;
        fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_NONE)
            .open(path)
            .unwrap()
    }

    /// Direct unit test of `remove_link_if_present` (the primitive
    /// `rollback_links` calls): a directory containing a file held open
    /// without delete-sharing must make `fs::remove_dir_all` fail, and the
    /// function must surface that failure rather than swallow it. Proves the
    /// underlying primitive genuinely reports faults — this behavior itself
    /// was never in question; what changed is that its caller now checks.
    #[test]
    #[cfg(windows)]
    fn remove_link_if_present_surfaces_a_blocked_directory_delete() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("cli-root");
        fs::create_dir_all(&dir).unwrap();
        let blocked = dir.join("locked.txt");
        fs::write(&blocked, "held open").unwrap();
        let _locked = lock_without_delete_sharing(&blocked);

        remove_link_if_present(&dir).expect_err("a locked file inside must fail the removal");

        assert!(
            dir.exists(),
            "a failed removal must not silently vanish the directory"
        );
        assert!(
            blocked.exists(),
            "the locked file itself must survive its own failed delete"
        );
    }

    /// `rollback_links` (private, called from `apply`'s IDE-failure branch)
    /// must propagate a real removal failure instead of discarding it via
    /// `let _ =`, as it did before this fix — the exact defect audit
    /// `FINDING-001` named. `cli_target` here is constructed directly rather
    /// than through a real `apply` call, since publish's own stage-then-swap
    /// rename requires the destination to be absent beforehand, making it
    /// impossible to pre-lock a file at the final path before publish lands
    /// it — `rollback_links` doesn't care how `cli_target` came to exist, so
    /// this exercises the exact production method deterministically.
    #[test]
    #[cfg(windows)]
    fn rollback_links_propagates_removal_failure_instead_of_discarding_it() {
        let temp_canonical = TempDir::new().unwrap();
        let temp_home = TempDir::new().unwrap();
        let projection = isolated_projection(temp_canonical.path(), temp_home.path());

        fs::create_dir_all(&projection.cli_target).unwrap();
        let blocked = projection.cli_target.join("locked.txt");
        fs::write(&blocked, "held open").unwrap();
        let _locked = lock_without_delete_sharing(&blocked);

        let result = projection.rollback_links(&[AgyLink::CliLink]);

        assert!(
            result.is_err(),
            "a blocked delete must surface as an error, not be silently discarded"
        );
        assert!(
            projection.cli_target.exists(),
            "the CLI root must still be present on disk after a failed rollback"
        );
    }

    /// `compensate_rolled_back_cli` is the exact function `apply`'s
    /// IDE-failure branch calls with `self.rollback_links(...).is_ok()`.
    /// Testing it directly (rather than racing real filesystem timing to
    /// force a mid-`apply` rollback failure) proves the conditional gate
    /// itself: a successful rollback gets the compensating `Removed` event,
    /// a failed one does not — mutation-tested by asserting both outcomes
    /// against the same starting evidence.
    #[test]
    fn compensate_rolled_back_cli_only_appends_removed_when_rollback_succeeded() {
        let cli_path = PathBuf::from("/fake/cli-root");
        let starting_evidence = || {
            let mut evidence = ProjectionEvidence::default();
            evidence.by_target.insert(
                ProjectionTargetId::AgyCli,
                ProjectionTargetEvidence::new(
                    vec![ProjectionPathResult {
                        path: cli_path.clone(),
                        change: ProjectionPathChange::Written,
                    }],
                    Vec::new(),
                    None,
                    None,
                )
                .unwrap(),
            );
            evidence
        };

        let mut succeeded = starting_evidence();
        compensate_rolled_back_cli(&mut succeeded, true);
        let cli_evidence = succeeded
            .by_target
            .get(&ProjectionTargetId::AgyCli)
            .unwrap();
        assert_eq!(
            cli_evidence.paths,
            vec![
                ProjectionPathResult {
                    path: cli_path.clone(),
                    change: ProjectionPathChange::Written,
                },
                ProjectionPathResult {
                    path: cli_path.clone(),
                    change: ProjectionPathChange::Removed,
                },
            ],
            "a successful rollback must append the compensating Removed event"
        );

        let mut failed = starting_evidence();
        compensate_rolled_back_cli(&mut failed, false);
        let cli_evidence = failed.by_target.get(&ProjectionTargetId::AgyCli).unwrap();
        assert_eq!(
            cli_evidence.paths,
            vec![ProjectionPathResult {
                path: cli_path,
                change: ProjectionPathChange::Written,
            }],
            "a failed rollback must leave the original event standing, not claim removal \
             that never happened"
        );
    }
}
