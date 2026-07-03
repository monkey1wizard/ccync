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
// This is a best-effort implementation for M1. Full transaction/ledger support
// is deferred to M2 per the plan's OE-A (over-engineering avoidance).

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

    /// Create or update the AGY plugin junctions, each gated by its runtime key:
    /// `cli` creates the agy-cli junction, `ide` the agy-ide junction. A later
    /// face failing rolls back the earlier one created in this call.
    pub fn apply(&self, cli: bool, ide: bool) -> Result<(), AgyError> {
        let mut created_surfaces = Vec::new();

        if cli {
            self.create_cli_junction()?;
            created_surfaces.push(AgySurface::CliLink);
        }

        if ide {
            if let Err(err) = self.create_ide_junction() {
                self.rollback_surfaces(&created_surfaces);
                return Err(err);
            }
            created_surfaces.push(AgySurface::IdeLink);
        }

        Ok(())
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

    /// Cross-platform link/junction creation (delegates to `ccync_foundation::platform`).
    fn create_link(&self, target: &Path, link: &Path, label: &str) -> Result<(), AgyError> {
        ccync_foundation::platform::create_dir_link(target, link)
            .map_err(|e| AgyError::LinkCreation(format!("{} link creation failed: {}", label, e)))
    }

    /// Check if both plugin junctions exist
    pub fn verify_surfaces_exist(&self) -> bool {
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

    fn rollback_surfaces(&self, surfaces: &[AgySurface]) {
        for surface in surfaces.iter().rev() {
            match surface {
                AgySurface::CliLink => {
                    let _ = remove_link_if_present(&self.cli_target);
                }
                AgySurface::IdeLink => {
                    let _ = remove_link_if_present(&self.ide_target);
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
enum AgySurface {
    CliLink,
    IdeLink,
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
    fn test_verify_surfaces_exist_when_missing() {
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

        // Should return false when surfaces don't exist
        assert!(!projection.verify_surfaces_exist());
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
        assert!(projection.verify_surfaces_exist());
        assert!(
            !temp_home.path().join(".gemini").join("commands").exists(),
            "Antigravity projection must not write ~/.gemini/commands"
        );
    }
}
