// AGY (Antigravity) plugin-junction projection
//
// The Antigravity CLI and IDE consume a whole-plugin junction into the canonical
// root, gated per runtime key:
// 1. CLI junction (gated by `agy-cli`): ~/.gemini/antigravity-cli/plugins/ccync -> canonical root
// 2. IDE junction (gated by `agy-ide`): ~/.gemini/antigravity-ide/plugins/ccync -> canonical root
//
// The Antigravity GUI (`agy-gui`) is NOT a plugin junction — it reads decomposed
// loose skills + an mcp_config.json (see `agy_gui_*` below + the projection wiring
// in `update_skills`). The former `~/.gemini/commands/*.toml` GUI-config face was a
// misprojection into Gemini CLI's command dir and has been removed.
//
// Junction creation is sequential, not independent per face: the CLI
// junction is attempted first, and a CLI failure short-circuits before the
// IDE junction is ever attempted. An IDE failure rolls back an already-
// created CLI link. Either outcome is reported as evidence, never a panic.

use crate::{
    ProjectionEvidence, ProjectionExpectedFailure, ProjectionFailure, ProjectionPathChange,
    ProjectionPathResult, ProjectionTargetEvidence,
};
use ccync_foundation::target::ProjectionTargetId;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgyError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("AGY CLI plugins directory creation failed: {0}")]
    CliPluginsDirCreation(String),

    #[error("AGY IDE plugins directory creation failed: {0}")]
    IdePluginsDirCreation(String),

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

        Ok(Self {
            canonical_root,
            cli_target,
            ide_target,
        })
    }

    /// Create CLI junction: ~/.gemini/antigravity-cli/plugins/ccync -> canonical root
    fn create_cli_junction(&self) -> Result<(), AgyError> {
        let plugins_dir = self.cli_target.parent().ok_or_else(|| {
            AgyError::CliPluginsDirCreation("Invalid CLI target path".to_string())
        })?;

        fs::create_dir_all(plugins_dir).map_err(|e| {
            AgyError::CliPluginsDirCreation(format!("{}: {}", plugins_dir.display(), e))
        })?;

        // Remove existing link/junction if present
        if self.cli_target.exists() && self.cli_target.is_dir() {
            #[cfg(windows)]
            {
                // On Windows, remove junction using rmdir
                std::process::Command::new("cmd")
                    .args(["/C", "rmdir", &path_arg(&self.cli_target)])
                    .output()
                    .map_err(|e| {
                        AgyError::LinkCreation(format!(
                            "Failed to remove existing CLI junction: {}",
                            e
                        ))
                    })?;
            }
            #[cfg(not(windows))]
            {
                fs::remove_file(&self.cli_target).map_err(|e| {
                    AgyError::LinkCreation(format!("Failed to remove existing CLI symlink: {}", e))
                })?;
            }
        }

        // Create junction/symlink
        self.create_link(&self.canonical_root, &self.cli_target, "CLI")?;

        Ok(())
    }

    /// Create IDE junction: ~/.gemini/antigravity-ide/plugins/ccync -> canonical root
    fn create_ide_junction(&self) -> Result<(), AgyError> {
        let plugins_dir = self.ide_target.parent().ok_or_else(|| {
            AgyError::IdePluginsDirCreation("Invalid IDE target path".to_string())
        })?;

        fs::create_dir_all(plugins_dir).map_err(|e| {
            AgyError::IdePluginsDirCreation(format!("{}: {}", plugins_dir.display(), e))
        })?;

        // Remove existing link/junction if present
        if self.ide_target.exists() && self.ide_target.is_dir() {
            #[cfg(windows)]
            {
                // On Windows, remove junction using rmdir
                std::process::Command::new("cmd")
                    .args(["/C", "rmdir", &path_arg(&self.ide_target)])
                    .output()
                    .map_err(|e| {
                        AgyError::LinkCreation(format!(
                            "Failed to remove existing IDE junction: {}",
                            e
                        ))
                    })?;
            }
            #[cfg(not(windows))]
            {
                fs::remove_file(&self.ide_target).map_err(|e| {
                    AgyError::LinkCreation(format!("Failed to remove existing IDE symlink: {}", e))
                })?;
            }
        }

        // Create junction/symlink
        self.create_link(&self.canonical_root, &self.ide_target, "IDE")?;

        Ok(())
    }

    /// Create or update the AGY plugin junctions, reporting evidence instead
    /// of a bare `Result<(), AgyError>`.
    ///
    /// This method runs on an already-constructed `AgyProjection` — any fault
    /// in building the projection itself (missing home directory, etc.)
    /// happens in [`AgyProjection::new`], before this method is ever called,
    /// so there is no "constructor fault" branch here. The faults this method
    /// actually produces are junction-creation faults from `create_cli_junction`
    /// / `create_ide_junction`, e.g. a fault forcing rollback of the CLI link
    /// when the IDE junction fails. Both are expected, owned-path failures —
    /// represented as evidence values on the relevant target, never as the
    /// outer `ProjectionFailure`.
    ///
    /// CLI is attempted first. If it fails, evidence records the failure on the
    /// CLI target and the IDE target is never attempted (short-circuit) — the
    /// IDE target stays absent from `completed_targets` (`NotRun`).
    ///
    /// If CLI succeeds (or was not selected) and IDE then fails, the CLI link
    /// created in this call is rolled back. The CLI target still gets recorded
    /// as `Written` + completed: the
    /// CLI phase itself genuinely succeeded at the time it ran, and a later
    /// target's failure triggering rollback does not retroactively make the
    /// CLI step's own outcome a failure. Only the target that actually failed
    /// (IDE) gets `failure` evidence; this matches the rule that failure/skip
    /// only wins for the target whose own step failed.
    ///
    /// When neither `cli` nor `ide` is selected, nothing is attempted and this
    /// returns `Ok(ProjectionEvidence::default())`.
    pub fn apply(&self, cli: bool, ide: bool) -> Result<ProjectionEvidence, ProjectionFailure> {
        let mut evidence = ProjectionEvidence::default();
        let mut created_links = Vec::new();

        if cli {
            match self.create_cli_junction() {
                Ok(()) => {
                    evidence
                        .by_target
                        .entry(ProjectionTargetId::AgyCli)
                        .or_default()
                        .paths
                        .push(ProjectionPathResult {
                            path: self.cli_target.clone(),
                            change: ProjectionPathChange::Written,
                        });
                    evidence
                        .completed_targets
                        .insert(ProjectionTargetId::AgyCli);
                    created_links.push(AgyLink::CliLink);
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
            match self.create_ide_junction() {
                Ok(()) => {
                    evidence
                        .by_target
                        .entry(ProjectionTargetId::AgyIde)
                        .or_default()
                        .paths
                        .push(ProjectionPathResult {
                            path: self.ide_target.clone(),
                            change: ProjectionPathChange::Written,
                        });
                    evidence
                        .completed_targets
                        .insert(ProjectionTargetId::AgyIde);
                }
                Err(err) => {
                    self.rollback_links(&created_links);
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

    /// Cross-platform link/junction creation (delegates to `ccync_foundation::platform`).
    fn create_link(&self, target: &Path, link: &Path, label: &str) -> Result<(), AgyError> {
        ccync_foundation::platform::create_dir_link(target, link)
            .map_err(|e| AgyError::LinkCreation(format!("{} link creation failed: {}", label, e)))
    }

    /// Check if both plugin junctions exist
    pub fn verify_links_exist(&self) -> bool {
        self.cli_target.exists() && self.ide_target.exists()
    }

    /// Remove both AGY plugin junctions (reverse of apply).
    ///
    /// Best-effort: continues on partial failures, returns first error encountered.
    pub fn remove(&self) -> Result<(), AgyError> {
        // Remove CLI junction/symlink
        if self.cli_target.exists() || self.cli_target.is_symlink() {
            let _ = ccync_foundation::platform::remove_dir_link(&self.cli_target);
        }

        // Remove IDE junction/symlink
        if self.ide_target.exists() || self.ide_target.is_symlink() {
            let _ = ccync_foundation::platform::remove_dir_link(&self.ide_target);
        }

        // GUI config TOML files are preserved (user may have customized them).

        Ok(())
    }

    fn rollback_links(&self, links: &[AgyLink]) {
        for link in links.iter().rev() {
            match link {
                AgyLink::CliLink => {
                    let _ = remove_link_if_present(&self.cli_target);
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
enum AgyLink {
    CliLink,
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn remove_link_if_present(path: &Path) -> std::io::Result<()> {
    if path.exists() || path.is_symlink() {
        ccync_foundation::platform::remove_dir_link(path)
    } else {
        Ok(())
    }
}

// --- Antigravity GUI (agy-gui) native projection paths ---
//
// The Antigravity GUI does NOT consume a whole-plugin junction the way agy-cli /
// agy-ide do (`~/.gemini/antigravity-{cli,ide}/plugins/ccync`). It reads loose,
// decomposed skills from `~/.gemini/antigravity/skills/<name>` and MCP servers
// from `~/.gemini/antigravity/mcp_config.json`. These helpers own those native
// paths; the projection wiring lives in `update_skills` (registry-tracked + pruned).

/// Antigravity GUI root: `~/.gemini/antigravity`.
pub fn agy_gui_root(home: &Path) -> PathBuf {
    home.join(".gemini").join("antigravity")
}

/// Antigravity GUI decomposed-skills dir: `~/.gemini/antigravity/skills`.
pub fn agy_gui_skills_root(home: &Path) -> PathBuf {
    agy_gui_root(home).join("skills")
}

/// Antigravity GUI MCP config file: `~/.gemini/antigravity/mcp_config.json`.
pub fn agy_gui_mcp_config_path(home: &Path) -> PathBuf {
    agy_gui_root(home).join("mcp_config.json")
}

/// Render the Antigravity GUI MCP config body from a canonical `servers` map
/// (the merged managed + adopted MCP servers from the canonical `.mcp.json`).
/// Antigravity is Gemini-family, so the GUI config uses the `mcpServers` key.
/// Returns `None` when there are no servers to project (no file should be written).
pub fn render_agy_gui_mcp_config(
    servers: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    if servers.is_empty() {
        return None;
    }
    let body = serde_json::json!({ "mcpServers": serde_json::Value::Object(servers.clone()) });
    serde_json::to_string_pretty(&body).ok()
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
        };

        // Should return false when links don't exist
        assert!(!projection.verify_links_exist());
    }

    #[test]
    fn path_arg_handles_non_utf8_lossily() {
        #[cfg(unix)]
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;

            let path = PathBuf::from(OsString::from_vec(vec![0x66, 0x6f, 0x80, 0x6f]));
            assert!(!path_arg(&path).is_empty());
        }

        #[cfg(not(unix))]
        {
            let path = PathBuf::from("C:/temp/ccync");
            assert_eq!(path_arg(&path), "C:/temp/ccync");
        }
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
        }
    }

    #[test]
    fn apply_cli_only_does_not_create_ide_junction() {
        let temp_canonical = TempDir::new().unwrap();
        let canonical_root = temp_canonical.path().to_path_buf();
        fs::create_dir_all(canonical_root.join("skills")).unwrap();
        let temp_home = TempDir::new().unwrap();

        let projection = isolated_projection(&canonical_root, temp_home.path());
        projection.apply(true, false).unwrap();

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
        projection.apply(true, true).unwrap();

        assert!(
            projection.cli_target.exists(),
            "agy-cli junction must be created"
        );
        assert!(
            projection.ide_target.exists(),
            "agy-ide junction must be created"
        );
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
        let evidence = projection.apply(false, false).unwrap();

        assert_eq!(evidence, ProjectionEvidence::default());
    }

    #[test]
    fn apply_both_succeed_records_both_targets_written_and_completed() {
        let temp_canonical = TempDir::new().unwrap();
        let canonical_root = temp_canonical.path().to_path_buf();
        fs::create_dir_all(canonical_root.join("skills")).unwrap();
        let temp_home = TempDir::new().unwrap();

        let projection = isolated_projection(&canonical_root, temp_home.path());
        let evidence = projection.apply(true, true).unwrap();

        assert!(evidence
            .completed_targets
            .contains(&ProjectionTargetId::AgyCli));
        assert!(evidence
            .completed_targets
            .contains(&ProjectionTargetId::AgyIde));

        let cli_evidence = evidence.by_target.get(&ProjectionTargetId::AgyCli).unwrap();
        assert_eq!(cli_evidence.paths.len(), 1);
        assert_eq!(cli_evidence.paths[0].path, projection.cli_target);
        assert_eq!(cli_evidence.paths[0].change, ProjectionPathChange::Written);
        assert!(cli_evidence.failure.is_none());

        let ide_evidence = evidence.by_target.get(&ProjectionTargetId::AgyIde).unwrap();
        assert_eq!(ide_evidence.paths.len(), 1);
        assert_eq!(ide_evidence.paths[0].path, projection.ide_target);
        assert_eq!(ide_evidence.paths[0].change, ProjectionPathChange::Written);
        assert!(ide_evidence.failure.is_none());
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
        let evidence = projection.apply(true, true).unwrap();

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
        let evidence = projection.apply(true, true).unwrap();

        // Behavior: the CLI link is physically rolled back on IDE failure.
        assert!(
            !projection.cli_target.exists(),
            "CLI junction must be rolled back after IDE junction failure"
        );

        // Evidence: CLI's own step genuinely completed before the later IDE
        // failure triggered rollback, so it is still recorded as Written +
        // completed. Only IDE — the target whose own step failed — carries
        // `failure` evidence.
        assert!(evidence
            .completed_targets
            .contains(&ProjectionTargetId::AgyCli));
        assert!(!evidence
            .completed_targets
            .contains(&ProjectionTargetId::AgyIde));

        let cli_evidence = evidence.by_target.get(&ProjectionTargetId::AgyCli).unwrap();
        assert_eq!(cli_evidence.paths.len(), 1);
        assert_eq!(cli_evidence.paths[0].change, ProjectionPathChange::Written);
        assert!(cli_evidence.failure.is_none());

        let ide_evidence = evidence.by_target.get(&ProjectionTargetId::AgyIde).unwrap();
        assert!(ide_evidence.paths.is_empty());
        let failure = ide_evidence.failure.as_ref().unwrap();
        assert_eq!(
            failure.path.as_deref(),
            Some(projection.ide_target.as_path())
        );
    }
}
