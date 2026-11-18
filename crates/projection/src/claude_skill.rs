//! Claude skill surface projection.
//!
//! Creates and maintains `~/.claude/skills/ccync` → canonical root.
//! This is the CCYNC-owned persistent projection surface loaded by Claude Code
//! for skills (e.g. `doc-sync`). Never touches `~/.claude/plugins/ccync` (oracle
//! legacy, removed on every refresh).

use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Error type for Claude skill surface projection operations.
#[derive(Debug, Error)]
pub enum ClaudeSkillError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("home directory not found")]
    NoHome,
    #[error("skills directory creation failed: {0}")]
    SkillsDirCreation(String),
    #[error("skill path link operation failed: {0}")]
    LinkOp(String),
}

/// Manages `~/.claude/skills/ccync` → canonical root.
///
/// On Windows: NTFS directory junction (`mklink /J`).
/// On Unix: symlink (`std::os::unix::fs::symlink`).
///
/// Invariants:
/// - Never reads or writes `~/.claude/plugins/ccync` (legacy, oracle removes it).
/// - `apply()` is idempotent: removes any existing link before recreating.
/// - `remove()` removes only the link, never the canonical root target.
pub struct ClaudeSkillProjection {
    /// Rendered canonical plugin root (`~/.ccync/plugins/ccync`).
    pub canonical_root: PathBuf,
    /// Skill surface link (`~/.claude/skills/ccync`).
    pub skill_path: PathBuf,
}

impl ClaudeSkillProjection {
    /// Construct using the real home directory.
    pub fn new(canonical_root: PathBuf) -> std::result::Result<Self, ClaudeSkillError> {
        let home = ccync_foundation::paths::user_home().ok_or(ClaudeSkillError::NoHome)?;
        let skill_path = home.join(".claude").join("skills").join("ccync");
        Ok(Self {
            canonical_root,
            skill_path,
        })
    }

    /// Accessor for the skill surface link path (`~/.claude/skills/ccync`).
    /// Equivalent to reading the `skill_path` field directly.
    pub fn skill_path(&self) -> &Path {
        &self.skill_path
    }

    /// Create or update `~/.claude/skills/ccync` → `canonical_root`.
    pub fn apply(&self) -> std::result::Result<(), ClaudeSkillError> {
        // Ensure ~/.claude/skills/ exists.
        let skills_dir = self.skill_path.parent().ok_or_else(|| {
            ClaudeSkillError::SkillsDirCreation("invalid skill surface path".to_string())
        })?;
        fs::create_dir_all(skills_dir).map_err(|e| {
            ClaudeSkillError::SkillsDirCreation(format!("{}: {e}", skills_dir.display()))
        })?;

        // Remove any existing link at the surface path.
        self.remove_link()?;

        // Create new link.
        self.create_link()
    }

    /// Return `true` if the skill surface exists (link resolves).
    pub fn verify_aligned(&self) -> bool {
        self.skill_path.exists()
    }

    /// Remove `~/.claude/skills/ccync` (link only, never the canonical root).
    pub fn remove(&self) -> std::result::Result<(), ClaudeSkillError> {
        self.remove_link()
    }

    fn remove_link(&self) -> std::result::Result<(), ClaudeSkillError> {
        // Nothing to remove.
        if !self.skill_path.exists()
            && !ccync_foundation::platform::is_symlink_or_junction(&self.skill_path)
        {
            return Ok(());
        }

        // The legacy registry category has no ownership marks. The target is
        // therefore the ownership proof for this specific migration link.
        if crate::support::is_ccync_repo_link(&self.skill_path, &self.canonical_root) {
            ccync_foundation::platform::remove_dir_link(&self.skill_path)
                .map_err(|e| ClaudeSkillError::LinkOp(format!("link removal failed: {e}")))?;
        }

        Ok(())
    }

    fn create_link(&self) -> std::result::Result<(), ClaudeSkillError> {
        ccync_foundation::platform::create_dir_link(&self.canonical_root, &self.skill_path)
            .map_err(|e| ClaudeSkillError::LinkOp(format!("link creation failed: {e}")))
    }
}

#[cfg(test)]
mod skill_tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_claude_skill_projection_new_paths() {
        let temp = TempDir::new().unwrap();
        let canonical = temp.path().join("canonical");
        fs::create_dir_all(&canonical).unwrap();

        let proj = ClaudeSkillProjection {
            canonical_root: canonical.clone(),
            skill_path: temp.path().join(".claude").join("skills").join("ccync"),
        };

        assert!(proj.canonical_root.ends_with("canonical"));
        assert!(proj.skill_path.to_string_lossy().contains("skills"));
        assert!(proj.skill_path.to_string_lossy().ends_with("ccync"));
    }

    #[test]
    fn test_apply_creates_skill_path() {
        let temp = TempDir::new().unwrap();
        let canonical = temp.path().join("canonical");
        fs::create_dir_all(&canonical).unwrap();

        let skill_path = temp.path().join(".claude").join("skills").join("ccync");

        let proj = ClaudeSkillProjection {
            canonical_root: canonical.clone(),
            skill_path: skill_path.clone(),
        };

        proj.apply().unwrap();

        // Surface must exist (link resolves to canonical which exists).
        assert!(skill_path.exists(), "skill surface must exist after apply");
    }

    #[test]
    fn test_apply_is_idempotent() {
        let temp = TempDir::new().unwrap();
        let canonical = temp.path().join("canonical");
        fs::create_dir_all(&canonical).unwrap();

        let skill_path = temp.path().join(".claude").join("skills").join("ccync");

        let proj = ClaudeSkillProjection {
            canonical_root: canonical.clone(),
            skill_path: skill_path.clone(),
        };

        proj.apply().unwrap();
        // Second apply must not error.
        proj.apply().unwrap();

        assert!(skill_path.exists());
    }

    #[test]
    fn test_verify_aligned_false_when_absent() {
        let temp = TempDir::new().unwrap();
        let proj = ClaudeSkillProjection {
            canonical_root: temp.path().join("canonical"),
            skill_path: temp.path().join(".claude").join("skills").join("ccync"),
        };
        assert!(!proj.verify_aligned());
    }

    #[test]
    fn test_remove_after_apply() {
        let temp = TempDir::new().unwrap();
        let canonical = temp.path().join("canonical");
        fs::create_dir_all(&canonical).unwrap();

        let skill_path = temp.path().join(".claude").join("skills").join("ccync");

        let proj = ClaudeSkillProjection {
            canonical_root: canonical.clone(),
            skill_path: skill_path.clone(),
        };

        proj.apply().unwrap();
        assert!(skill_path.exists());

        proj.remove().unwrap();
        // After remove, the surface link is gone but canonical root is untouched.
        assert!(
            !skill_path.exists(),
            "skill surface must be gone after remove"
        );
        assert!(canonical.exists(), "canonical root must survive remove");
    }

    #[test]
    fn test_remove_when_absent_is_noop() {
        let temp = TempDir::new().unwrap();
        let proj = ClaudeSkillProjection {
            canonical_root: temp.path().join("canonical"),
            skill_path: temp.path().join(".claude").join("skills").join("ccync"),
        };
        // Must not error when nothing exists.
        proj.remove().unwrap();
    }

    #[test]
    fn direct_skills_remove_only_link_into_canonical_root() {
        let temp = TempDir::new().unwrap();
        let canonical = temp.path().join("canonical");
        let elsewhere = temp.path().join("elsewhere");
        fs::create_dir_all(&canonical).unwrap();
        fs::create_dir_all(&elsewhere).unwrap();
        // Keep the fixture path simple because the platform helper passes
        // junction paths through cmd.exe on Windows.
        let link = temp.path().join("skill-link");
        let projection = ClaudeSkillProjection {
            canonical_root: canonical,
            skill_path: link.clone(),
        };
        ccync_foundation::platform::create_dir_link(&projection.canonical_root, &link).unwrap();
        projection.remove().unwrap();
        assert!(!link.exists());
        ccync_foundation::platform::create_dir_link(&elsewhere, &link).unwrap();
        projection.remove().unwrap();
        assert!(link.exists() || ccync_foundation::platform::is_symlink_or_junction(&link));
    }

    #[test]
    fn test_never_touches_legacy_plugins_path() {
        let temp = TempDir::new().unwrap();
        let canonical = temp.path().join("canonical");
        fs::create_dir_all(&canonical).unwrap();

        let skill_path = temp.path().join(".claude").join("skills").join("ccync");
        let legacy_plugins = temp.path().join(".claude").join("plugins").join("ccync");

        let proj = ClaudeSkillProjection {
            canonical_root: canonical,
            skill_path,
        };

        proj.apply().unwrap();
        // Legacy path must never be created.
        assert!(
            !legacy_plugins.exists(),
            "legacy ~/.claude/plugins/ccync must never be touched"
        );
    }

    #[test]
    fn test_skill_path_matches_field_success_fixture() {
        // "Success" fixture: mirrors test_apply_creates_skill_path — real
        // temp-dir paths where the canonical root exists and apply() succeeds.
        let temp = TempDir::new().unwrap();
        let canonical = temp.path().join("canonical");
        fs::create_dir_all(&canonical).unwrap();
        let skill_path = temp.path().join(".claude").join("skills").join("ccync");

        let proj = ClaudeSkillProjection {
            canonical_root: canonical,
            skill_path: skill_path.clone(),
        };
        proj.apply().unwrap();

        assert_eq!(proj.skill_path(), proj.skill_path.as_path());
        assert_eq!(proj.skill_path(), skill_path.as_path());
    }

    #[test]
    fn test_skill_path_matches_field_fault_fixture() {
        // "Fault" fixture: this file has no forced-failure injection seam
        // (no way to make create_dir_link/remove_dir_link fail), so the
        // closest analog is the absent/edge-case state used by
        // test_verify_aligned_false_when_absent and
        // test_remove_when_absent_is_noop — paths that never touched disk.
        // The accessor must still equal the field even when nothing exists.
        let temp = TempDir::new().unwrap();
        let skill_path = temp.path().join(".claude").join("skills").join("ccync");
        let proj = ClaudeSkillProjection {
            canonical_root: temp.path().join("canonical"),
            skill_path: skill_path.clone(),
        };

        assert_eq!(proj.skill_path(), proj.skill_path.as_path());
        assert_eq!(proj.skill_path(), skill_path.as_path());
        assert!(!proj.skill_path().exists());
    }

    #[test]
    fn test_link_creation_fault_reports_new_literal() {
        // Genuine (non-mocked) fault injection: pre-create a regular file at
        // `skill_path`. `remove_link()`'s ownership guard (is_dir on Windows,
        // is_symlink on Unix) is false for a plain file, so it is left in
        // place; `create_link()` then genuinely fails, because both
        // `mklink /J` (Windows) and `symlink()` (Unix) refuse to create a
        // link where a filesystem entry already exists. This forces the real
        // `ClaudeSkillError::LinkOp` variant through its real code path (not
        // a hand-constructed enum value), proving the renamed literal is
        // actually what a real I/O fault surfaces.
        let temp = TempDir::new().unwrap();
        let canonical = temp.path().join("canonical");
        fs::create_dir_all(&canonical).unwrap();

        let skill_path = temp.path().join(".claude").join("skills").join("ccync");
        fs::create_dir_all(skill_path.parent().unwrap()).unwrap();
        // Blocker: a regular file sitting at the link target path.
        fs::write(&skill_path, b"blocker").unwrap();

        let proj = ClaudeSkillProjection {
            canonical_root: canonical,
            skill_path: skill_path.clone(),
        };

        let err = proj
            .apply()
            .expect_err("link creation must fail: a regular file occupies skill_path");
        let msg = err.to_string();
        assert!(
            msg.starts_with("skill path link operation failed: "),
            "expected new literal prefix from a genuinely injected fault, got: {msg}"
        );
        assert!(
            !msg.contains("skill surface link operation failed"),
            "old literal must not resurface"
        );

        // The blocker file must survive untouched (apply() must not clobber
        // an unrelated file it doesn't own).
        assert_eq!(fs::read(&skill_path).unwrap(), b"blocker");
    }
}
