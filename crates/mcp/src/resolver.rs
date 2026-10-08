//! MCP variable resolver (${VAR} substitution) + the variable-map builders.

use ccync_foundation::mcp::{McpManifest, McpServer};
use std::collections::HashMap;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Resolves `${VAR_NAME}` placeholders in MCP server configs.
///
/// Ports `Resolve-McpConfig` / variable substitution from `Update-Mcp.ps1`.
#[derive(Debug)]
pub struct McpVariableResolver {
    variables: HashMap<String, String>,
}

impl McpVariableResolver {
    /// Create a resolver with an explicit variable map.
    pub fn new(variables: HashMap<String, String>) -> Self {
        Self { variables }
    }

    /// Create an empty resolver (no substitution performed).
    pub fn empty() -> Self {
        Self {
            variables: HashMap::new(),
        }
    }

    /// Resolve `${VAR}` and `${VAR:-default}` placeholders in `value`.
    ///
    /// Resolve a string and collect unresolved secret-name placeholders.
    pub fn resolve_string(&self, value: &str) -> (String, BTreeSet<String>) {
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = RE.get_or_init(|| regex::Regex::new(r"\$\{([A-Z0-9_]+)(?::-([^}]*))?\}").unwrap());

        let mut result = value.to_string();
        let mut unresolved = BTreeSet::new();
        for cap in re.captures_iter(value) {
            let var_name = &cap[1];
            let placeholder = &cap[0];
            if let Some(default) = cap.get(2).map(|value| value.as_str()) {
                let resolved = self
                    .variables
                    .get(var_name)
                    .filter(|value| !value.is_empty())
                    .map(String::as_str)
                    .unwrap_or(default);
                result = result.replace(placeholder, resolved);
            } else if let Some(resolved) = self.variables.get(var_name) {
                result = result.replace(placeholder, resolved);
            } else if is_secret_name(var_name) {
                unresolved.insert(var_name.to_string());
            }
        }
        (result, unresolved)
    }

    /// Resolve all string fields of a single server entry.
    pub fn resolve_server(&self, server: &McpServer) -> (McpServer, BTreeSet<String>) {
        let mut s = server.clone();
        let mut unresolved = BTreeSet::new();
        // `cwd` gets no `${VAR}` resolution because plugin placeholders were expanded at merge time.
        if let Some(ref cmd) = server.command {
            let (value, names) = self.resolve_string(cmd);
            s.command = Some(value);
            unresolved.extend(names);
        }
        if let Some(ref args) = server.args {
            s.args = Some(
                args.iter()
                    .map(|a| {
                        let (value, names) = self.resolve_string(a);
                        unresolved.extend(names);
                        value
                    })
                    .collect(),
            );
        }
        if let Some(ref env) = server.env {
            let mut resolved = HashMap::new();
            for (k, v) in env {
                let (value, names) = self.resolve_string(v);
                unresolved.extend(names);
                resolved.insert(k.clone(), value);
            }
            s.env = Some(resolved);
        }
        if let Some(ref url) = server.url {
            let (value, names) = self.resolve_string(url);
            s.url = Some(value);
            unresolved.extend(names);
        }
        if let Some(ref headers) = server.headers {
            let mut resolved = HashMap::new();
            for (k, v) in headers {
                let (value, names) = self.resolve_string(v);
                unresolved.extend(names);
                resolved.insert(k.clone(), value);
            }
            s.headers = Some(resolved);
        }
        (s, unresolved)
    }

    /// Resolve all servers in a manifest.
    pub fn resolve_manifest(
        &self,
        manifest: &McpManifest,
    ) -> (McpManifest, BTreeMap<String, BTreeSet<String>>) {
        let mut servers = HashMap::new();
        let mut skipped = BTreeMap::new();
        for (name, server) in &manifest.servers {
            let (resolved, unresolved) = self.resolve_server(server);
            if unresolved.is_empty() {
                servers.insert(name.clone(), resolved);
            } else {
                skipped.insert(name.clone(), unresolved);
            }
        }
        (
            McpManifest {
                servers,
                inputs: manifest.inputs.clone(),
            },
            skipped,
        )
    }
}

pub fn is_secret_name(name: &str) -> bool {
    let u = name.to_uppercase();
    u.contains("KEY") || u.contains("SECRET") || u.contains("TOKEN") || u.contains("PASSWORD")
}

/// Find unresolved secret-name placeholders in the current machine environment.
pub fn unresolved_secret_vars(manifest: &McpManifest) -> BTreeMap<String, BTreeSet<String>> {
    let resolver = McpVariableResolver::new(build_variable_map());
    resolver.resolve_manifest(manifest).1
}
/// Build the MCP variable map used to resolve `${VAR}` placeholders.
///
/// Resolution layers, lowest priority first (later layers overlay earlier):
/// 1. process environment;
/// 2. `~/.ccync/config.json` — path values via `MACHINE_PATH_REGISTRY` +
///    `config.json#secrets` verbatim (highest priority).
pub(crate) fn build_variable_map() -> HashMap<String, String> {
    let machine_cfg = ccync_foundation::paths::machine_config_path();
    build_variable_map_from(machine_cfg.as_deref())
}

/// Testable core of [`build_variable_map`] with an explicit config path.
///
/// `machine_cfg_path` — JSON `config.json` (Layer 2 primary).
pub(crate) fn build_variable_map_from(machine_cfg_path: Option<&Path>) -> HashMap<String, String> {
    // Layer 1: process environment.
    let mut map: HashMap<String, String> = std::env::vars().collect();

    // Layer 2: config.json — wins over Layer 1 for overlapping keys.
    // Empty string values are treated as "not set" and do not shadow Layer 1.
    // If the file is unreadable or malformed, a warning is emitted.
    if let Some(cfg_path) = machine_cfg_path {
        match std::fs::read_to_string(cfg_path) {
            Err(e) => eprintln!("ccync-mcp: WARN — cannot read {}: {e}", cfg_path.display()),
            Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                Err(e) => eprintln!(
                    "ccync-mcp: WARN — malformed config.json at {}: {e}",
                    cfg_path.display()
                ),
                Ok(root) => {
                    // Path values via MACHINE_PATH_REGISTRY (nested dot-path).
                    for (config_path, snake_key) in ccync_foundation::config::MACHINE_PATH_REGISTRY
                    {
                        if let Some(value) =
                            ccync_foundation::config::machine_path_value(&root, config_path)
                        {
                            if !value.trim().is_empty() {
                                map.insert((*snake_key).to_string(), value);
                            }
                        }
                    }
                    // Credential placeholders from config.json#secrets verbatim.
                    if let Some(secrets) = root.get("secrets").and_then(|v| v.as_object()) {
                        for (k, v) in secrets {
                            if let Some(s) = v.as_str() {
                                if !s.trim().is_empty() {
                                    map.insert(k.clone(), s.to_string());
                                }
                            }
                        }
                    }
                }
            },
        }
    }

    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_server_keeps_cwd() {
        let mut vars = HashMap::new();
        vars.insert("DIR".to_string(), "expanded_dir".to_string());
        let resolver = McpVariableResolver::new(vars);

        let server = McpServer {
            server_type: Some("stdio".to_string()),
            command: Some("cmd".to_string()),
            args: None,
            env: None,
            cwd: Some("/custom/working/${DIR}".to_string()),
            url: None,
            headers: None,
        };

        let (resolved, unresolved) = resolver.resolve_server(&server);

        assert_eq!(resolved.cwd, server.cwd);
        assert_eq!(resolved.cwd, Some("/custom/working/${DIR}".to_string()));
        assert!(unresolved.is_empty());
    }
}
