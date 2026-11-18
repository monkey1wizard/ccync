//! MCP projection HealthCheck.

use ccync_foundation::health::{DoctorFinding, HealthCheck};

// ─────────────────────────────────────────────────────────────────────────────
// HealthCheck implementation
// ─────────────────────────────────────────────────────────────────────────────

/// Checks that the CCYNC-managed MCP projection file exists and is valid JSON.
pub struct McpProjectionHealthCheck {
    projection_path: std::path::PathBuf,
    machine_config_path: std::path::PathBuf,
}

impl McpProjectionHealthCheck {
    pub fn with_path(path: std::path::PathBuf) -> Self {
        let machine_config_path =
            crate::resolver::effective_machine_config_path().unwrap_or_default();
        Self::with_paths(path, machine_config_path)
    }

    /// Construct with explicit generated MCP and machine config paths.
    pub fn with_paths(
        projection_path: std::path::PathBuf,
        machine_config_path: std::path::PathBuf,
    ) -> Self {
        Self {
            projection_path,
            machine_config_path,
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
                )];
            }
        };
        match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(_) => {
                let Ok(manifest) = serde_json::from_str::<crate::McpManifest>(&content) else {
                    return vec![];
                };
                crate::server_variable_reports_at(&manifest, &self.machine_config_path)
                    .into_iter()
                    .map(|report| {
                        let secret = report.secret_names().iter().cloned().collect::<Vec<_>>().join(", ");
                        let ordinary = report.ordinary_names().iter().cloned().collect::<Vec<_>>().join(", ");
                        let fields = report.field_locations().iter().cloned().collect::<Vec<_>>().join(", ");
                        DoctorFinding::warning(format!(
                            "MCP server `{}` has unresolved secret variable(s) [{}] and ordinary variable(s) [{}] in [{}] that cannot be resolved in the current environment; add them to config.json#secrets or the environment",
                            report.server(), secret, ordinary, fields
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
