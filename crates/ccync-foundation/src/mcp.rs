//! MCP (Model Context Protocol) shared data-model types.
//!
//! These are the serde-serializable manifest/server types plus the error and
//! `Result` alias shared between the MCP generation logic (`mcp` crate) and the
//! file/skill projection layer (`projection` crate).
//!
//! They live in `base` to break the dependency cycle that would otherwise appear
//! between the projection and `mcp` crates. Pure data + error types; no CCYNC-crate deps.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

/// MCP-related errors.
#[derive(Debug, Error)]
pub enum McpError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON parse error: {0}")]
    JsonParse(#[from] serde_json::Error),

    #[error("Plugin root incomplete, missing required directory: {0}")]
    PluginRootIncomplete(String),

    #[error("Invalid variable name: {0}")]
    InvalidVariableName(String),
}

pub type Result<T> = std::result::Result<T, McpError>;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeclarationIdentity {
    pub source: String,
    pub server: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclarationMetadata {
    pub identity: DeclarationIdentity,
    pub unresolved_digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclarationRoot {
    Personal,
    Legacy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarationRoute {
    pub hosts: Option<Vec<String>>,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarationRoutes {
    pub declarations: HashMap<DeclarationIdentity, DeclarationRoute>,
    pub overrides: HashMap<DeclarationIdentity, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid MCP routes: {0}")]
pub struct DeclarationRoutesError(pub &'static str);

fn route_hosts(
    value: &serde_json::Value,
) -> std::result::Result<Vec<String>, DeclarationRoutesError> {
    const HOSTS: [&str; 5] = ["claude", "codex", "copilot", "opencode", "agy"];
    let entries = value
        .as_array()
        .ok_or(DeclarationRoutesError("hosts must be an array"))?;
    let mut selected = [false; 5];
    for entry in entries {
        let host = entry
            .as_str()
            .ok_or(DeclarationRoutesError("host must be a string"))?;
        let index = HOSTS
            .iter()
            .position(|candidate| *candidate == host)
            .ok_or(DeclarationRoutesError("unknown host"))?;
        selected[index] = true;
    }
    Ok(HOSTS
        .iter()
        .zip(selected)
        .filter_map(|(host, present)| present.then(|| (*host).to_owned()))
        .collect())
}

/// Parse a whole direct-input document without reading or writing files.
pub fn read_declaration_routes(
    document: &serde_json::Value,
    root: DeclarationRoot,
    direct_source: &str,
) -> std::result::Result<DeclarationRoutes, DeclarationRoutesError> {
    let object = document
        .as_object()
        .ok_or(DeclarationRoutesError("root must be an object"))?;
    let server_key = match root {
        DeclarationRoot::Personal => "ccyncMcpServers",
        DeclarationRoot::Legacy => "mcpServers",
    };
    if object
        .keys()
        .any(|key| key != server_key && key != "hostRoutes")
    {
        return Err(DeclarationRoutesError("unknown or foreign root key"));
    }
    let servers = object
        .get(server_key)
        .and_then(serde_json::Value::as_object)
        .ok_or(DeclarationRoutesError("server root must be an object"))?;
    if direct_source.is_empty() {
        return Err(DeclarationRoutesError("source must not be empty"));
    }
    let mut declarations = HashMap::new();
    for (server, value) in servers {
        let entry = value
            .as_object()
            .ok_or(DeclarationRoutesError("server must be an object"))?;
        let hosts = entry.get("hosts").map(route_hosts).transpose()?;
        let enabled = entry
            .get("enabled")
            .map(|value| {
                value
                    .as_bool()
                    .ok_or(DeclarationRoutesError("enabled must be a boolean"))
            })
            .transpose()?
            .unwrap_or(true);
        declarations.insert(
            DeclarationIdentity {
                source: direct_source.to_owned(),
                server: server.clone(),
            },
            DeclarationRoute { hosts, enabled },
        );
    }
    let mut overrides = HashMap::new();
    if let Some(value) = object.get("hostRoutes") {
        for row in value
            .as_array()
            .ok_or(DeclarationRoutesError("hostRoutes must be an array"))?
        {
            let row = row
                .as_object()
                .ok_or(DeclarationRoutesError("route must be an object"))?;
            if row.len() != 3
                || !row.contains_key("source")
                || !row.contains_key("server")
                || !row.contains_key("hosts")
            {
                return Err(DeclarationRoutesError(
                    "route must contain source, server and hosts",
                ));
            }
            let source = row["source"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or(DeclarationRoutesError("invalid route source"))?;
            let server = row["server"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or(DeclarationRoutesError("invalid route server"))?;
            let identity = DeclarationIdentity {
                source: source.to_owned(),
                server: server.to_owned(),
            };
            let hosts = route_hosts(&row["hosts"])?;
            if let Some(previous) = overrides.insert(identity, hosts.clone()) {
                if previous != hosts {
                    return Err(DeclarationRoutesError("conflicting routes"));
                }
            }
        }
    }
    for (identity, hosts) in &overrides {
        if let Some(route) = declarations.get_mut(identity) {
            route.hosts = Some(hosts.clone());
        }
    }
    Ok(DeclarationRoutes {
        declarations,
        overrides,
    })
}

/// A value-free description of a rejected portable declaration.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{path}: {reason}")]
pub struct PortableDeclarationError {
    pub path: String,
    pub reason: &'static str,
}

/// Classify variable names that denote secrets.
pub fn is_secret_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["KEY", "SECRET", "TOKEN", "PASSWORD"]
        .iter()
        .any(|part| upper.contains(part))
        || upper.split(['_', '-']).any(|part| part == "PAT")
}

fn portable_error(path: &str, reason: &'static str) -> PortableDeclarationError {
    PortableDeclarationError {
        path: path.to_owned(),
        reason,
    }
}

fn portable_reference(value: &str) -> bool {
    value
        .strip_prefix("${")
        .and_then(|s| s.strip_suffix('}'))
        .is_some_and(|name| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
        })
}

fn safe_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        && ![
            "__proto__",
            "prototype",
            "constructor",
            "con",
            "prn",
            "aux",
            "nul",
            "com1",
            "com2",
            "com3",
            "com4",
            "com5",
            "com6",
            "com7",
            "com8",
            "com9",
            "lpt1",
            "lpt2",
            "lpt3",
            "lpt4",
            "lpt5",
            "lpt6",
            "lpt7",
            "lpt8",
            "lpt9",
        ]
        .iter()
        .any(|reserved| value.eq_ignore_ascii_case(reserved))
}

fn scan_portable_value(
    value: &serde_json::Value,
    path: &str,
    secrets: &[String],
) -> std::result::Result<(), PortableDeclarationError> {
    match value {
        serde_json::Value::String(text) => {
            if secrets
                .iter()
                .any(|secret| !secret.is_empty() && text.contains(secret))
            {
                return Err(portable_error(path, "known secret value"));
            }
            let lower = text.to_ascii_lowercase();
            if lower.contains("://")
                && lower.split("://").skip(1).any(|part| {
                    part.split(['/', '?', '#'])
                        .next()
                        .unwrap_or("")
                        .contains('@')
                })
            {
                return Err(portable_error(path, "URL userinfo"));
            }
            if crate::secret::secret_re().is_match(text) && !portable_reference(text) {
                return Err(portable_error(path, "credential value"));
            }
        }
        serde_json::Value::Array(values) => {
            for (index, item) in values.iter().enumerate() {
                scan_portable_value(item, &format!("{path}[{index}]"), secrets)?;
            }
        }
        serde_json::Value::Object(values) => {
            for (key, item) in values {
                if secrets
                    .iter()
                    .any(|secret| !secret.is_empty() && key.contains(secret))
                {
                    return Err(portable_error(path, "known secret value"));
                }
                scan_portable_value(item, path, secrets)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Validate one unresolved native server object before it is normalized or persisted.
pub fn validate_portable_declaration(
    value: &serde_json::Value,
    known_secrets: &[String],
) -> std::result::Result<(), PortableDeclarationError> {
    let object = value
        .as_object()
        .ok_or_else(|| portable_error("$", "expected object"))?;
    for (key, item) in object {
        if known_secrets
            .iter()
            .any(|secret| !secret.is_empty() && key.contains(secret))
        {
            return Err(portable_error("$", "known secret value"));
        }
        let path = match key.as_str() {
            "type" | "command" | "args" | "env" | "url" | "headers" | "hosts" | "enabled" => {
                key.as_str()
            }
            _ => "$",
        };
        scan_portable_value(item, path, known_secrets)?;
        if path == "$" {
            return Err(portable_error(path, "unknown field"));
        }
    }
    let command = object.get("command");
    let url = object.get("url");
    if command.is_some() == url.is_some() {
        return Err(portable_error("$", "expected exactly one command or URL"));
    }
    let launch = command
        .or(url)
        .and_then(|v| v.as_str())
        .ok_or_else(|| portable_error("$", "expected string launch value"))?;
    let remote = url.is_some();
    if let Some(kind) = object.get("type") {
        let valid = match kind.as_str() {
            Some("stdio") => !remote,
            Some("http" | "sse") => remote,
            _ => false,
        };
        if !valid {
            return Err(portable_error("type", "invalid transport type"));
        }
    }
    if remote {
        if !portable_reference(launch)
            && ![
                "https://mcp.example.com",
                "https://x.test/mcp",
                "https://x.test/sse",
            ]
            .contains(&launch)
        {
            return Err(portable_error("url", "nonportable value"));
        }
    } else if !portable_reference(launch) && !["node", "npx"].contains(&launch) {
        return Err(portable_error("command", "nonportable value"));
    }
    if let Some(args) = object.get("args") {
        let args = args
            .as_array()
            .ok_or_else(|| portable_error("args", "expected string array"))?;
        let strings: Option<Vec<&str>> = args.iter().map(|v| v.as_str()).collect();
        let strings = strings.ok_or_else(|| portable_error("args", "expected string array"))?;
        let allowed = strings.iter().all(|v| portable_reference(v))
            || (!remote
                && ((launch == "node"
                    && (strings == ["--mode", "safe"] || strings == ["./server.js"]))
                    || (launch == "npx" && strings == ["-y", "srv"])));
        if !allowed {
            return Err(portable_error("args", "nonportable arguments"));
        }
    }
    for field in ["env", "headers"] {
        if let Some(map) = object.get(field) {
            let map = map
                .as_object()
                .ok_or_else(|| portable_error(field, "expected string map"))?;
            for (name, value) in map {
                if !safe_identifier(name) {
                    return Err(portable_error(field, "unsafe identifier"));
                }
                let value = value
                    .as_str()
                    .ok_or_else(|| portable_error(field, "expected string map"))?;
                let allowed = portable_reference(value)
                    || (field == "env" && name == "MODE" && value == "safe")
                    || (field == "headers" && name == "Accept" && value == "application/json")
                    || (field == "headers"
                        && name == "Authorization"
                        && value
                            .strip_prefix("Bearer ")
                            .is_some_and(portable_reference));
                if !allowed {
                    return Err(portable_error(field, "nonportable value"));
                }
            }
        }
    }
    if let Some(hosts) = object.get("hosts") {
        if !hosts.as_array().is_some_and(|hosts| {
            hosts.iter().all(|host| {
                host.as_str().is_some_and(|host| {
                    ["claude", "codex", "copilot", "opencode", "agy"].contains(&host)
                })
            })
        }) {
            return Err(portable_error("hosts", "invalid hosts"));
        }
    }
    if object
        .get("enabled")
        .is_some_and(|enabled| !enabled.is_boolean())
    {
        return Err(portable_error("enabled", "expected boolean"));
    }
    Ok(())
}

/// MCP server configuration.
///
/// Represents a single MCP server entry from a manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServer {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub server_type: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum McpSourceKind {
    Direct,
    Core,
    Package,
    Adopted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerProvenance {
    identity: DeclarationIdentity,
    kind: McpSourceKind,
    package_version: Option<String>,
    unresolved_digest: String,
    hosts: Option<Vec<String>>,
    enabled: bool,
    source_preserved: bool,
    native_package_entry: bool,
}

impl McpServerProvenance {
    #[allow(clippy::too_many_arguments, reason = "The constructor accepts every provenance field.")]
    pub fn new(
        identity: DeclarationIdentity,
        kind: McpSourceKind,
        package_version: Option<String>,
        unresolved_digest: String,
        hosts: Option<Vec<String>>,
        enabled: bool,
        source_preserved: bool,
        native_package_entry: bool,
    ) -> Self {
        Self {
            identity,
            kind,
            package_version,
            unresolved_digest,
            hosts,
            enabled,
            source_preserved,
            native_package_entry,
        }
    }

    pub fn identity(&self) -> &DeclarationIdentity {
        &self.identity
    }
    pub fn kind(&self) -> McpSourceKind {
        self.kind
    }
    pub fn package_version(&self) -> Option<&str> {
        self.package_version.as_deref()
    }
    pub fn unresolved_digest(&self) -> &str {
        &self.unresolved_digest
    }
    pub fn hosts(&self) -> Option<&[String]> {
        self.hosts.as_deref()
    }
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    pub fn source_preserved(&self) -> bool {
        self.source_preserved
    }
    pub fn native_package_entry(&self) -> bool {
        self.native_package_entry
    }
}

/// MCP manifest structure.
///
/// Represents the portable `.mcp.json` manifest from repo source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpManifest {
    pub servers: HashMap<String, McpServer>,

    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub provenance: HashMap<String, McpServerProvenance>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub inputs: Option<serde_json::Value>,
}

impl McpManifest {
    pub fn provenance(&self) -> &HashMap<String, McpServerProvenance> {
        &self.provenance
    }

    pub fn set_provenance(&mut self, server: String, provenance: McpServerProvenance) {
        self.provenance.insert(server, provenance);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_provenance_round_trip_and_legacy_compatibility() {
        let legacy = r#"{"servers":{"example":{"command":"node"}}}"#;
        let mut manifest: McpManifest = serde_json::from_str(legacy).unwrap();
        assert!(manifest.provenance().is_empty());
        assert_eq!(manifest.servers["example"].command.as_deref(), Some("node"));
        assert!(serde_json::to_value(&manifest)
            .unwrap()
            .get("provenance")
            .is_none());
        let make = |hosts| {
            McpServerProvenance::new(
                DeclarationIdentity {
                    source: "package".into(),
                    server: "example".into(),
                },
                McpSourceKind::Package,
                Some("1.2.3".into()),
                "digest".into(),
                hosts,
                true,
                false,
                true,
            )
        };
        manifest.set_provenance("unrouted".into(), make(None));
        manifest.set_provenance("withdrawn".into(), make(Some(vec![])));
        manifest.set_provenance("selected".into(), make(Some(vec!["codex".into()])));
        let json = serde_json::to_string(&manifest).unwrap();
        let restored: McpManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.provenance(), manifest.provenance());
        assert_eq!(restored.provenance()["unrouted"].hosts(), None);
        assert_eq!(
            restored.provenance()["withdrawn"].hosts(),
            Some([].as_slice())
        );
        assert_eq!(restored.servers["example"].command.as_deref(), Some("node"));
    }

    #[test]
    fn routes_preserve_identity_intent_and_validate_the_document() {
        use serde_json::json;
        let document = json!({"ccyncMcpServers": {
            "a": {"hosts":["agy", "claude", "agy"], "enabled":false},
            "b": {}, "c": {"hosts":[]}
        }, "hostRoutes":[
            {"source":"user", "server":"a", "hosts":["codex"]},
            {"source":"package", "server":"a", "hosts":[]}
        ]});
        let parsed = read_declaration_routes(&document, DeclarationRoot::Personal, "user").unwrap();
        let id = |server: &str| DeclarationIdentity {
            source: "user".into(),
            server: server.into(),
        };
        assert_eq!(
            parsed.declarations[&id("a")].hosts,
            Some(vec!["codex".into()])
        );
        assert!(!parsed.declarations[&id("a")].enabled);
        assert_eq!(parsed.declarations[&id("b")].hosts, None);
        assert_eq!(parsed.declarations[&id("c")].hosts, Some(vec![]));
        let revision = DeclarationMetadata {
            identity: id("a"),
            unresolved_digest: "new".into(),
        };
        assert_eq!(revision.identity, id("a"));
        assert_ne!(
            revision.identity,
            DeclarationIdentity {
                source: "package".into(),
                server: "a".into()
            }
        );
        let ordered = read_declaration_routes(
            &json!({"mcpServers":{"a":{"hosts":["agy","claude","agy"]}}}),
            DeclarationRoot::Legacy,
            "user",
        )
        .unwrap();
        assert_eq!(
            ordered.declarations[&id("a")].hosts,
            Some(vec!["claude".into(), "agy".into()])
        );
        for bad in [
            json!({"ccyncMcpServers":{}, "mcpServers":{}}),
            json!({"ccyncMcpServers":{}, "foreign":{}}),
            json!({"ccyncMcpServers":{"a":{"hosts":["codxe"]}}}),
            json!({"ccyncMcpServers":{"a":{"enabled":"false"}}}),
            json!({"ccyncMcpServers":{}, "hostRoutes":[{"source":"user","server":"a","hosts":[]},{"source":"user","server":"a","hosts":["codex"]}]}),
        ] {
            assert!(read_declaration_routes(&bad, DeclarationRoot::Personal, "user").is_err());
        }
    }

    #[test]
    fn portable_declarations_accept_and_refuse_expected_values() {
        use serde_json::json;
        for value in [
            json!({"command":"node","args":["--mode","safe"],"env":{"MODE":"safe","API_KEY":"${API_KEY}"},"hosts":["claude","codex"],"enabled":true}),
            json!({"command":"node","args":["./server.js"],"type":"stdio"}),
            json!({"command":"npx","args":["-y","srv"]}),
            json!({"command":"${COMMAND}","args":["${ARG}"]}),
            json!({"type":"http","url":"https://x.test/mcp","headers":{"Accept":"application/json","Authorization":"Bearer ${TOKEN}"}}),
            json!({"type":"sse","url":"https://x.test/sse"}),
            json!({"url":"https://mcp.example.com"}),
            json!({"url":"${URL}"}),
        ] {
            assert!(
                validate_portable_declaration(&value, &[]).is_ok(),
                "{value}"
            );
        }
        for value in [
            json!({"command":"node","args":["--token","secret"]}),
            json!({"command":"node","args":["--mode","unsafe"]}),
            json!({"command":"${CMD:-node}"}),
            json!({"url":"https://user:pass@x.test/mcp"}),
            json!({"command":"node","cwd":"/private"}),
            json!({"command":"node","env":{"../bad":"${VALUE}"}}),
            json!({"command":"node","extra":{"nested":["safe"]}}),
        ] {
            let err = validate_portable_declaration(&value, &[]).unwrap_err();
            assert!(!err.to_string().contains("pass"));
        }
        let err = validate_portable_declaration(
            &json!({"command":"node","env":{"MODE":"safe"}}),
            &["safe".to_string()],
        )
        .unwrap_err();
        assert_eq!(err.reason, "known secret value");
    }

    #[test]
    fn secret_name_classification_uses_pat_segments() {
        for name in [
            "api_key",
            "Secret",
            "TOKEN",
            "password",
            "PAT",
            "my-pat_value",
        ] {
            assert!(is_secret_name(name));
        }
        for name in ["PATH", "PATTERN", "XPATH", "compat"] {
            assert!(!is_secret_name(name));
        }
    }

    #[test]
    fn mcp_server_cwd_round_trip() {
        let server_with_cwd: McpServer =
            serde_json::from_str(r#"{"command":"node","cwd":"/path/to/workdir"}"#).unwrap();
        assert_eq!(server_with_cwd.cwd.as_deref(), Some("/path/to/workdir"));
        let json_with_cwd = serde_json::to_string(&server_with_cwd).unwrap();
        assert!(json_with_cwd.contains(r#""cwd":"/path/to/workdir""#));

        let back: McpServer = serde_json::from_str(&json_with_cwd).unwrap();
        assert_eq!(back.cwd.as_deref(), Some("/path/to/workdir"));

        let server_without_cwd: McpServer = serde_json::from_str(r#"{"command":"node"}"#).unwrap();
        assert_eq!(server_without_cwd.cwd, None);
        let json_without_cwd = serde_json::to_string(&server_without_cwd).unwrap();
        assert!(!json_without_cwd.contains("cwd"));
    }
}
