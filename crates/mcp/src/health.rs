//! MCP projection HealthCheck.

use ccync_foundation::health::{DoctorFinding, HealthCheck};

// ─────────────────────────────────────────────────────────────────────────────
// HealthCheck implementation
// ─────────────────────────────────────────────────────────────────────────────

/// Checks that the CCYNC-managed MCP projection file exists and is valid JSON.
pub struct McpProjectionHealthCheck {
    projection_path: std::path::PathBuf,
}

impl McpProjectionHealthCheck {
    /// Construct using the standard `~/.ccync/build/mcp/managed.json` path.
    pub fn from_standard_path() -> Option<Self> {
        ccync_foundation::paths::generated_mcp_path().map(|p| Self { projection_path: p })
    }

    pub fn with_path(path: std::path::PathBuf) -> Self {
        Self {
            projection_path: path,
        }
    }
}

impl HealthCheck for McpProjectionHealthCheck {
    fn name(&self) -> &str {
        "mcp-projection"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        if !self.projection_path.exists() {
            return vec![DoctorFinding::warning(format!(
                "MCP projection not found: {} — run `ccync sync` to generate it",
                self.projection_path.display()
            ))];
        }
        let content = match std::fs::read_to_string(&self.projection_path) {
            Ok(c) => c,
            Err(e) => {
                return vec![DoctorFinding::error(
                    format!("MCP projection unreadable: {e}"),
                    "run `ccync sync` to regenerate the MCP projection",
                )]
            }
        };
        match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(_) => {
                let Ok(manifest) = serde_json::from_str::<crate::McpManifest>(&content) else {
                    return vec![];
                };
                crate::unresolved_secret_vars(&manifest)
                    .into_iter()
                    .map(|(server, vars)| {
                        DoctorFinding::warning(format!(
                            "MCP server `{server}` has unresolved secret variable(s) {} that cannot be resolved in the current environment; add them to config.json#secrets or the environment",
                            vars.into_iter().collect::<Vec<_>>().join(", ")
                        ))
                    })
                    .collect()
            }
            Err(e) => vec![DoctorFinding::error(
                format!("MCP projection invalid JSON: {e}"),
                "run `ccync sync` to regenerate the MCP projection",
            )],
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
