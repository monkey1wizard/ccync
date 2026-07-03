//! Claude skill surface projection.
//!
//! Creates and maintains `~/.claude/skills/ccync` → canonical root.
//! This is the CCYNC-owned persistent projection surface loaded by Claude Code
//! for skills (e.g. `doc-sync`). Never touches `~/.claude/plugins/ccync` (oracle
//! legacy, removed on every refresh).

use std::fs;
use std::path::PathBuf;
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
    #[error("skill surface link operation failed: {0}")]
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
    pub skill_surface: PathBuf,
}

impl ClaudeSkillProjection {
    /// Construct using the real home directory.
    pub fn new(canonical_root: PathBuf) -> std::result::Result<Self, ClaudeSkillError> {
        let home = ccync_foundation::paths::user_home().ok_or(ClaudeSkillError::NoHome)?;
        let skill_surface = home.join(".claude").join("skills").join("ccync");
        Ok(Self {
            canonical_root,
            skill_surface,
        })
    }

    /// Create or update `~/.claude/skills/ccync` → `canonical_root`.
    pub fn apply(&self) -> std::result::Result<(), ClaudeSkillError> {
        // Ensure ~/.claude/skills/ exists.
        let skills_dir = self.skill_surface.parent().ok_or_else(|| {
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
        self.skill_surface.exists()
    }

    /// Remove `~/.claude/skills/ccync` (link only, never the canonical root).
    pub fn remove(&self) -> std::result::Result<(), ClaudeSkillError> {
        self.remove_link()
    }

    fn remove_link(&self) -> std::result::Result<(), ClaudeSkillError> {
        // Nothing to remove.
        if !self.skill_surface.exists()
            && !ccync_foundation::platform::is_symlink_or_junction(&self.skill_surface)
        {
            return Ok(());
        }

        // Only remove when the surface is the link/junction we own — never delete
        // a real directory. Windows guards on is_dir (junction), Unix on is_symlink.
        #[cfg(windows)]
        let should_remove = self.skill_surface.is_dir();
        #[cfg(not(windows))]
        let should_remove = self.skill_surface.is_symlink();

        if should_remove {
            ccync_foundation::platform::remove_dir_link(&self.skill_surface)
                .map_err(|e| ClaudeSkillError::LinkOp(format!("link removal failed: {e}")))?;
        }

        Ok(())
    }

    fn create_link(&self) -> std::result::Result<(), ClaudeSkillError> {
        ccync_foundation::platform::create_dir_link(&self.canonical_root, &self.skill_surface)
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
            skill_surface: temp.path().join(".claude").join("skills").join("ccync"),
        };

        assert!(proj.canonical_root.ends_with("canonical"));
        assert!(proj.skill_surface.to_string_lossy().contains("skills"));
        assert!(proj.skill_surface.to_string_lossy().ends_with("ccync"));
    }

    #[test]
    fn test_apply_creates_skill_surface() {
        let temp = TempDir::new().unwrap();
        let canonical = temp.path().join("canonical");
        fs::create_dir_all(&canonical).unwrap();

        let skill_surface = temp.path().join(".claude").join("skills").join("ccync");

        let proj = ClaudeSkillProjection {
            canonical_root: canonical.clone(),
            skill_surface: skill_surface.clone(),
        };

        proj.apply().unwrap();

        // Surface must exist (link resolves to canonical which exists).
        assert!(
            skill_surface.exists(),
            "skill surface must exist after apply"
        );
    }

    #[test]
    fn test_apply_is_idempotent() {
        let temp = TempDir::new().unwrap();
        let canonical = temp.path().join("canonical");
        fs::create_dir_all(&canonical).unwrap();

        let skill_surface = temp.path().join(".claude").join("skills").join("ccync");

        let proj = ClaudeSkillProjection {
            canonical_root: canonical.clone(),
            skill_surface: skill_surface.clone(),
        };

        proj.apply().unwrap();
        // Second apply must not error.
        proj.apply().unwrap();

        assert!(skill_surface.exists());
    }

    #[test]
    fn test_verify_aligned_false_when_absent() {
        let temp = TempDir::new().unwrap();
        let proj = ClaudeSkillProjection {
            canonical_root: temp.path().join("canonical"),
            skill_surface: temp.path().join(".claude").join("skills").join("ccync"),
        };
        assert!(!proj.verify_aligned());
    }

    #[test]
    fn test_remove_after_apply() {
        let temp = TempDir::new().unwrap();
        let canonical = temp.path().join("canonical");
        fs::create_dir_all(&canonical).unwrap();

        let skill_surface = temp.path().join(".claude").join("skills").join("ccync");

        let proj = ClaudeSkillProjection {
            canonical_root: canonical.clone(),
            skill_surface: skill_surface.clone(),
        };

        proj.apply().unwrap();
        assert!(skill_surface.exists());

        proj.remove().unwrap();
        // After remove, the surface link is gone but canonical root is untouched.
        assert!(
            !skill_surface.exists(),
            "skill surface must be gone after remove"
        );
        assert!(canonical.exists(), "canonical root must survive remove");
    }

    #[test]
    fn test_remove_when_absent_is_noop() {
        let temp = TempDir::new().unwrap();
        let proj = ClaudeSkillProjection {
            canonical_root: temp.path().join("canonical"),
            skill_surface: temp.path().join(".claude").join("skills").join("ccync"),
        };
        // Must not error when nothing exists.
        proj.remove().unwrap();
    }

    #[test]
    fn test_never_touches_legacy_plugins_path() {
        let temp = TempDir::new().unwrap();
        let canonical = temp.path().join("canonical");
        fs::create_dir_all(&canonical).unwrap();

        let skill_surface = temp.path().join(".claude").join("skills").join("ccync");
        let legacy_plugins = temp.path().join(".claude").join("plugins").join("ccync");

        let proj = ClaudeSkillProjection {
            canonical_root: canonical,
            skill_surface,
        };

        proj.apply().unwrap();
        // Legacy path must never be created.
        assert!(
            !legacy_plugins.exists(),
            "legacy ~/.claude/plugins/ccync must never be touched"
        );
    }
}
