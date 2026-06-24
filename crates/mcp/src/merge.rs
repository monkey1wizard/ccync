//! Safe CCYNC-managed merge + per-host non-destructive config writers.

use crate::{ensure_parent, McpUpdateError};
use ccync_foundation::mcp::McpManifest;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ─────────────────────────────────────────────────────────────────────────────
// Safe-merge
// ─────────────────────────────────────────────────────────────────────────────

/// Safe merge of CCYNC-managed entries over user-owned entries.
///
/// CCYNC-managed servers (those listed in the manifest) replace any existing
/// entry; user-owned entries are preserved unchanged.
///
/// Ports `Merge-OrderedMap` + the CCYNC-managed guard from `Update-Mcp.ps1`.
#[derive(Debug)]
pub struct McpMerger {
    ccync_managed_servers: Vec<String>,
}

impl McpMerger {
    pub fn new(ccync_managed_servers: Vec<String>) -> Self {
        Self {
            ccync_managed_servers,
        }
    }

    /// Merge `ccync_manifest` over `existing` (if any).
    ///
    /// - CCYNC-managed names are always replaced.
    /// - User-owned names in `existing` are preserved.
    pub fn merge(
        &self,
        ccync_manifest: &McpManifest,
        existing: Option<&McpManifest>,
    ) -> McpManifest {
        let mut servers = HashMap::new();
        for (name, server) in &ccync_manifest.servers {
            servers.insert(name.clone(), server.clone());
        }
        if let Some(ex) = existing {
            for (name, server) in &ex.servers {
                if !self.ccync_managed_servers.contains(name) {
                    servers.insert(name.clone(), server.clone());
                }
            }
        }
        McpManifest {
            servers,
            inputs: ccync_manifest.inputs.clone(),
        }
    }
}
/// Non-destructively overlay CCYNC-managed entries into a JSON provider config.
///
/// Preserves user-owned server entries and any unrelated top-level keys.
/// `servers_key` is the provider's server-map field (`mcpServers` / `mcp`).
/// `generated` is the freshly serialized provider config (`{servers_key: {…}}`).
/// Returns the number of managed entries written. An existing file that is not
/// a JSON object is treated as an error rather than being overwritten.
pub(crate) fn write_json_provider_merged(
    dest: &Path,
    servers_key: &str,
    generated: &serde_json::Value,
) -> std::result::Result<usize, McpUpdateError> {
    let managed = generated
        .get(servers_key)
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    let mut root = if dest.exists() {
        let content = std::fs::read_to_string(dest)?;
        if content.trim().is_empty() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            match serde_json::from_str::<serde_json::Value>(&content)? {
                v @ serde_json::Value::Object(_) => v,
                _ => {
                    return Err(McpUpdateError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "{} is not a JSON object — refusing to overwrite",
                            dest.display()
                        ),
                    )))
                }
            }
        }
    } else {
        serde_json::Value::Object(serde_json::Map::new())
    };

    let obj = root.as_object_mut().expect("root is an object");
    let servers = obj
        .entry(servers_key.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !servers.is_object() {
        *servers = serde_json::Value::Object(serde_json::Map::new());
    }
    let servers_obj = servers.as_object_mut().expect("servers slot is an object");

    let count = managed.len();
    for (name, entry) in managed {
        servers_obj.insert(name, entry);
    }

    ensure_parent(dest)?;
    write_atomic(dest, &serde_json::to_string_pretty(&root)?)?;
    Ok(count)
}

/// Extract the server name from a Codex TOML table header line.
///
/// Returns `Some(name)` for `[mcp_servers.NAME]` / `[mcp_servers.NAME.env]`
/// (bare or quoted), else `None`. Ports `Get-CodexTableServerName`.
pub(crate) fn codex_table_server_name(line: &str) -> Option<String> {
    let t = line.trim();
    let inner = t.strip_prefix('[')?.strip_suffix(']')?;
    let remainder = inner.strip_prefix("mcp_servers.")?;
    if remainder.is_empty() {
        return None;
    }
    if let Some(rest) = remainder.strip_prefix('"') {
        let mut out = String::new();
        let mut prev = '"';
        for c in rest.chars() {
            if c == '"' && prev != '\\' {
                return Some(out.replace("\\\"", "\"").replace("\\\\", "\\"));
            }
            out.push(c);
            prev = c;
        }
        return None;
    }
    Some(remainder.split('.').next().unwrap_or(remainder).to_string())
}

/// True for a single-bracket TOML table header `[name]` (not `[[array]]`).
fn is_toml_table_header(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('[') && !t.starts_with("[[") && t.ends_with(']') && t.len() > 2
}

/// Remove managed `[mcp_servers.NAME]` sections (and their sub-tables) for the
/// given names, preserving all other content (user MCP entries + non-MCP config).
///
/// Ports `Remove-CodexManagedServersFromToml`.
pub(crate) fn remove_codex_managed_sections(
    raw: &str,
    managed: &std::collections::HashSet<String>,
) -> String {
    if raw.trim().is_empty() {
        return String::new();
    }
    let mut out: Vec<&str> = Vec::new();
    let mut skip = false;
    for line in raw.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(name) = codex_table_server_name(line) {
            skip = managed.contains(&name);
        } else if is_toml_table_header(line) {
            skip = false;
        }
        if !skip {
            out.push(line);
        }
    }
    out.join("\r\n").trim_end_matches(['\r', '\n']).to_string()
}

/// Write Codex `config.toml` by removing managed sections then appending fresh
/// ones, preserving non-MCP content. `sections` is the codex serializer output
/// (already `\r\n`-joined with a trailing newline).
pub(crate) fn write_codex_merged(
    dest: &Path,
    managed_names: &std::collections::HashSet<String>,
    sections: &str,
) -> std::result::Result<(), McpUpdateError> {
    let existing = if dest.exists() {
        std::fs::read_to_string(dest)?
    } else {
        String::new()
    };
    let remaining = remove_codex_managed_sections(&existing, managed_names);

    let mut new_raw = remaining;
    if !new_raw.trim().is_empty() {
        new_raw = new_raw.trim_end_matches(['\r', '\n']).to_string();
        new_raw.push_str("\r\n\r\n");
    }
    new_raw.push_str(sections);
    if !new_raw.ends_with("\r\n") {
        new_raw.push_str("\r\n");
    }

    ensure_parent(dest)?;
    write_atomic(dest, &new_raw)?;
    Ok(())
}

/// Sibling temp path `<dest>.tmp` in the same directory (so the rename stays on
/// one filesystem). Appends `.tmp` to the full file name rather than replacing
/// the extension, so `config.toml` → `config.toml.tmp`.
fn tmp_sibling(dest: &Path) -> PathBuf {
    let mut name = dest
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    dest.with_file_name(name)
}

/// Write `contents` to `dest` atomically: write a sibling `<dest>.tmp`, then
/// `rename` it over `dest`. The rename is atomic on a single filesystem (on
/// Windows it overwrites the target). A failure during the temp write leaves
/// `dest` untouched; a failure during the rename removes the temp file.
///
/// Hand-rolled to keep `tempfile` a dev-only dependency. Mirrors the
/// `write_atomic` helper that lived in the (since-deleted) cross-agent
/// `reconcile.rs`, so live agent configs (`~/.claude.json`, `~/.codex/config.toml`)
/// can never be truncated by a crash mid-write.
fn write_atomic(dest: &Path, contents: &str) -> std::result::Result<(), McpUpdateError> {
    let tmp = tmp_sibling(dest);
    if let Err(e) = std::fs::write(&tmp, contents) {
        let _ = std::fs::remove_file(&tmp);
        return Err(McpUpdateError::Io(e));
    }
    if let Err(e) = std::fs::rename(&tmp, dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(McpUpdateError::Io(e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn tmp_sibling_appends_suffix_keeps_extension() {
        let p = Path::new("/some/dir/config.toml");
        assert_eq!(tmp_sibling(p), PathBuf::from("/some/dir/config.toml.tmp"));
    }

    #[test]
    fn write_atomic_round_trips_and_overwrites() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("config.toml");

        write_atomic(&dest, "hello = 1\n").unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "hello = 1\n");

        // Overwrite an existing target.
        write_atomic(&dest, "hello = 2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "hello = 2\n");

        // No leftover temp sibling.
        assert!(!tmp_sibling(&dest).exists(), "temp sibling must not linger");
    }

    #[test]
    fn write_atomic_leaves_target_untouched_when_temp_write_fails() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("config.toml");
        std::fs::write(&dest, "original\n").unwrap();

        // Force the temp write to fail: pre-create <dest>.tmp as a directory, so
        // `std::fs::write` to that path errors before any rename can run.
        let tmp_path = tmp_sibling(&dest);
        std::fs::create_dir(&tmp_path).unwrap();

        let result = write_atomic(&dest, "new content\n");
        assert!(result.is_err(), "temp write onto a directory path must fail");

        // The live target must be exactly as it was — never truncated.
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "original\n");
    }

    #[test]
    fn write_json_provider_merged_round_trips_via_atomic_write() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("mcp-config.json");
        let generated = serde_json::json!({
            "mcpServers": { "foo": { "command": "foo-bin" } }
        });

        let count = write_json_provider_merged(&dest, "mcpServers", &generated).unwrap();
        assert_eq!(count, 1);

        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        assert_eq!(written["mcpServers"]["foo"]["command"], "foo-bin");
        assert!(!tmp_sibling(&dest).exists());
    }
}
