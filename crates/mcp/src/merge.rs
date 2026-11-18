//! Safe CCYNC-managed merge + per-host non-destructive config writers.

use crate::io::write_atomic;
use crate::{ensure_parent, McpUpdateError};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Whether a host write actually changed the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteOutcome {
    Written,
    Unchanged,
}

/// Read `dest` as a JSON object (empty object if missing/blank). Errors if the
/// file exists but is not a JSON object, rather than clobbering it.
fn read_json_object_or_empty(
    dest: &Path,
) -> std::result::Result<serde_json::Value, McpUpdateError> {
    if !dest.exists() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    let content = std::fs::read_to_string(dest)?;
    if content.trim().is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    match serde_json::from_str::<serde_json::Value>(&content)? {
        v @ serde_json::Value::Object(_) => Ok(v),
        _ => Err(McpUpdateError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{} is not a JSON object — refusing to overwrite",
                dest.display()
            ),
        ))),
    }
}

/// Apply one removal-plus-overlay delta to a JSON provider config.
///
/// Deletes exactly the `removals` names and writes the `overlays` entries under
/// `servers_key`, preserving unowned/user server entries and every unrelated
/// top-level key. Returns [`WriteOutcome::Unchanged`] (and skips the write,
/// preserving bytes+mtime) when the merged value equals what is already on disk.
/// The projection apply path passes the planner's per-host removals + desired
/// overlays here.
pub(crate) fn write_json_provider_delta(
    dest: &Path,
    servers_key: &str,
    removals: &BTreeSet<String>,
    overlays: &BTreeMap<String, serde_json::Value>,
) -> std::result::Result<WriteOutcome, McpUpdateError> {
    let mut root = read_json_object_or_empty(dest)?;
    let original = root.clone();

    let obj = root.as_object_mut().expect("root is an object");
    let servers = obj
        .entry(servers_key.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !servers.is_object() {
        *servers = serde_json::Value::Object(serde_json::Map::new());
    }
    let servers_obj = servers.as_object_mut().expect("servers slot is an object");

    for name in removals {
        servers_obj.remove(name);
    }
    for (name, entry) in overlays {
        servers_obj.insert(name.clone(), entry.clone());
    }

    if root == original {
        return Ok(WriteOutcome::Unchanged);
    }
    ensure_parent(dest)?;
    write_atomic(dest, &serde_json::to_string_pretty(&root)?)?;
    Ok(WriteOutcome::Written)
}

/// Apply one removal-plus-overlay delta to a Codex `config.toml`.
///
/// Removes the `[mcp_servers.NAME]` sections for the `removals` names *and* for
/// any overlaid names (so an overlay replaces its prior section), then appends
/// the `overlay_sections` text, preserving user MCP sections and all non-MCP
/// content. `overlay_sections` maps a server name to its host-native TOML
/// section (from the Codex serializer) — no second TOML parser is introduced.
/// Returns [`WriteOutcome::Unchanged`] when the result equals the existing file.
pub(crate) fn write_codex_delta(
    dest: &Path,
    removals: &BTreeSet<String>,
    overlay_sections: &BTreeMap<String, String>,
) -> std::result::Result<WriteOutcome, McpUpdateError> {
    let existing = if dest.exists() {
        std::fs::read_to_string(dest)?
    } else {
        String::new()
    };

    // Strip the removed names and the overlaid names (overlay replaces).
    let mut strip: std::collections::HashSet<String> = removals.iter().cloned().collect();
    for name in overlay_sections.keys() {
        strip.insert(name.clone());
    }
    let remaining = remove_codex_managed_sections(&existing, &strip);

    // Append overlay sections, each normalized to no trailing newline.
    let sections: Vec<String> = overlay_sections
        .values()
        .map(|s| s.trim_end_matches(['\r', '\n']).to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let mut new_raw = remaining.trim_end_matches(['\r', '\n']).to_string();
    for section in &sections {
        if !new_raw.is_empty() {
            new_raw.push_str("\r\n\r\n");
        }
        new_raw.push_str(section);
    }
    if !new_raw.is_empty() {
        new_raw.push_str("\r\n");
    }

    if new_raw == existing {
        return Ok(WriteOutcome::Unchanged);
    }
    ensure_parent(dest)?;
    write_atomic(dest, &new_raw)?;
    Ok(WriteOutcome::Written)
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

/// The server names declared by a `[mcp_servers.NAME…]` table header in `raw`.
///
/// These are exactly the names the line-based writer above can address: it
/// removes a server by dropping its header line and the lines beneath it, so a
/// server that has no header of its own is invisible to it. The semantic
/// (`toml::from_str`) live reader sees such a server anyway — e.g. one declared
/// as an inline key under a plain `[mcp_servers]` table — which is why the
/// reader consults this to decide what it may *not* claim ownership of.
pub(crate) fn codex_header_declared_names(raw: &str) -> BTreeSet<String> {
    raw.split('\n')
        .filter_map(|line| codex_table_server_name(line.strip_suffix('\r').unwrap_or(line)))
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::tmp_sibling;
    use tempfile::TempDir;

    #[test]
    fn remove_codex_managed_sections_drops_managed_and_env_table_keeps_model_and_user() {
        let raw = "[model]\nname = \"o3\"\n\n[mcp_servers.ccync-managed]\ncmd = \"a\"\n\n[mcp_servers.ccync-managed.env]\nKEY = \"val\"\n\n[mcp_servers.user-server]\ncmd = \"b\"\n";
        let managed = std::collections::HashSet::from(["ccync-managed".to_string()]);
        let result = remove_codex_managed_sections(raw, &managed);
        assert!(result.contains("[model]"), "model section must survive");
        assert!(
            result.contains("[mcp_servers.user-server]"),
            "user server must survive"
        );
        assert!(
            !result.contains("[mcp_servers.ccync-managed]"),
            "managed section must be gone"
        );
        assert!(
            !result.contains("[mcp_servers.ccync-managed.env]"),
            "env sub-table must be gone"
        );
    }

    /// `{name: entry}` overlay map helper for the delta writer.
    fn overlay(name: &str, entry: serde_json::Value) -> BTreeMap<String, serde_json::Value> {
        [(name.to_string(), entry)].into()
    }

    #[test]
    fn write_json_provider_delta_round_trips_via_atomic_write() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("mcp-config.json");

        let outcome = write_json_provider_delta(
            &dest,
            "mcpServers",
            &BTreeSet::new(),
            &overlay("foo", serde_json::json!({ "command": "foo-bin" })),
        )
        .unwrap();
        assert_eq!(outcome, WriteOutcome::Written);

        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        assert_eq!(written["mcpServers"]["foo"]["command"], "foo-bin");
        assert!(!tmp_sibling(&dest).exists());
    }

    #[test]
    fn write_json_provider_delta_refuses_to_overwrite_non_object() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("bad.json");
        std::fs::write(&dest, "[1,2,3]").unwrap();
        let result = write_json_provider_delta(
            &dest,
            "mcpServers",
            &BTreeSet::new(),
            &overlay("x", serde_json::json!({})),
        );
        assert!(result.is_err(), "must not clobber a non-object config");
        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "[1,2,3]",
            "original content untouched"
        );
    }

    #[test]
    fn claude_semantic_noop_preserves_bytes_and_mtime() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("claude.json");
        let entry = serde_json::json!({ "command": "npx", "args": ["-y", "x"] });

        // First write establishes the file.
        write_json_provider_delta(
            &dest,
            "mcpServers",
            &BTreeSet::new(),
            &overlay("ccync", entry.clone()),
        )
        .unwrap();
        let bytes1 = std::fs::read(&dest).unwrap();
        let mtime1 = std::fs::metadata(&dest).unwrap().modified().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(25));

        // An identical overlay is a semantic no-op: bytes and mtime unchanged.
        let outcome = write_json_provider_delta(
            &dest,
            "mcpServers",
            &BTreeSet::new(),
            &overlay("ccync", entry),
        )
        .unwrap();
        assert_eq!(outcome, WriteOutcome::Unchanged);
        let bytes2 = std::fs::read(&dest).unwrap();
        let mtime2 = std::fs::metadata(&dest).unwrap().modified().unwrap();
        assert_eq!(bytes1, bytes2, "no-op must not change bytes");
        assert_eq!(
            mtime1, mtime2,
            "no-op must not touch the file (mtime preserved)"
        );

        std::thread::sleep(std::time::Duration::from_millis(25));

        // Changing an entry value writes.
        let changed = serde_json::json!({ "command": "npx", "args": ["-y", "y"] });
        let outcome = write_json_provider_delta(
            &dest,
            "mcpServers",
            &BTreeSet::new(),
            &overlay("ccync", changed),
        )
        .unwrap();
        assert_eq!(outcome, WriteOutcome::Written);
        let bytes3 = std::fs::read(&dest).unwrap();
        assert_ne!(
            bytes2, bytes3,
            "an entry value change must rewrite the file"
        );
    }

    #[test]
    fn claude_semantic_noop_ignores_unrelated_key_order() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("claude.json");
        // Pre-existing file already contains the managed entry plus a user key.
        let initial = serde_json::json!({
            "zzz": 1,
            "mcpServers": { "ccync": { "command": "npx" }, "user": { "command": "u" } }
        });
        std::fs::write(&dest, serde_json::to_string_pretty(&initial).unwrap()).unwrap();
        let mtime1 = std::fs::metadata(&dest).unwrap().modified().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(25));

        // Overlay the same managed entry — semantic no-op despite the user key.
        let outcome = write_json_provider_delta(
            &dest,
            "mcpServers",
            &BTreeSet::new(),
            &overlay("ccync", serde_json::json!({ "command": "npx" })),
        )
        .unwrap();
        assert_eq!(outcome, WriteOutcome::Unchanged);
        let mtime2 = std::fs::metadata(&dest).unwrap().modified().unwrap();
        assert_eq!(
            mtime1, mtime2,
            "no-op preserved even with unrelated keys present"
        );
    }

    #[test]
    fn managed_delta_writers_json_removes_owned_keeps_unowned_and_nonmcp() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("host.json");
        let initial = serde_json::json!({
            "other": { "x": 1 },
            "mcpServers": {
                "owned-old": { "command": "old" },
                "user":      { "command": "u" }
            }
        });
        std::fs::write(&dest, serde_json::to_string_pretty(&initial).unwrap()).unwrap();

        let removals: BTreeSet<String> = ["owned-old".to_string()].into();
        let mut overlays: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        overlays.insert(
            "owned-new".to_string(),
            serde_json::json!({ "command": "new" }),
        );

        let outcome = write_json_provider_delta(&dest, "mcpServers", &removals, &overlays).unwrap();
        assert_eq!(outcome, WriteOutcome::Written);

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        assert!(
            v["mcpServers"]["owned-old"].is_null(),
            "removed owned name is gone"
        );
        assert_eq!(
            v["mcpServers"]["owned-new"]["command"], "new",
            "overlay written"
        );
        assert_eq!(
            v["mcpServers"]["user"]["command"], "u",
            "unowned user entry kept"
        );
        assert_eq!(v["other"]["x"], 1, "unrelated top-level key preserved");
    }

    #[test]
    fn managed_delta_writers_opencode_key_variant_and_noop() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("opencode.json");
        let initial = serde_json::json!({ "mcp": { "a": { "type": "local" } } });
        std::fs::write(&dest, serde_json::to_string_pretty(&initial).unwrap()).unwrap();
        let mtime1 = std::fs::metadata(&dest).unwrap().modified().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(25));

        // Overlay the same entry under the "mcp" key → unchanged.
        let mut overlays: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        overlays.insert("a".to_string(), serde_json::json!({ "type": "local" }));
        let outcome = write_json_provider_delta(&dest, "mcp", &BTreeSet::new(), &overlays).unwrap();
        assert_eq!(outcome, WriteOutcome::Unchanged);
        assert_eq!(
            mtime1,
            std::fs::metadata(&dest).unwrap().modified().unwrap(),
            "no-op preserves mtime"
        );
    }

    /// Documents *why* the live reader refuses to claim an inline-defined Codex
    /// server (see `read_live_codex_entries`): if such a name ever reached this
    /// writer as an overlay, the result would not merely be wrong — it would be
    /// invalid TOML, and Codex would fail to load its entire config. The
    /// reader-side refusal is what keeps this unreachable.
    #[test]
    fn codex_header_declared_names_bounds_what_the_writer_can_strip() {
        let raw =
            "[mcp_servers]\nfoo = { command = \"x\" }\n\n[mcp_servers.bar]\ncommand = \"y\"\n";

        // Only `bar` has a table header, so only `bar` is addressable.
        let declared = codex_header_declared_names(raw);
        assert_eq!(declared.iter().collect::<Vec<_>>(), vec!["bar"]);

        // Proof of the hazard: ask the writer to overlay both names anyway.
        let mut overlays: BTreeMap<String, String> = BTreeMap::new();
        overlays.insert(
            "foo".to_string(),
            "[mcp_servers.foo]\r\ncommand = \"ccync\"\r\n".to_string(),
        );
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("config.toml");
        std::fs::write(&dest, raw).unwrap();
        write_codex_delta(&dest, &BTreeSet::new(), &overlays).unwrap();

        let written = std::fs::read_to_string(&dest).unwrap();
        assert!(
            written.contains("foo = { command = \"x\" }") && written.contains("[mcp_servers.foo]"),
            "the inline key survives and a second definition is appended: {written}"
        );
        assert!(
            toml::from_str::<toml::Value>(&written).is_err(),
            "which is a duplicate key — Codex could not load this at all: {written}"
        );
    }

    #[test]
    fn managed_delta_writers_codex_removes_section_keeps_user_and_model() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("config.toml");
        let existing = "[model]\r\nname = \"o3\"\r\n\r\n[mcp_servers.owned-old]\r\ncommand = \"old\"\r\n\r\n[mcp_servers.user]\r\ncommand = \"u\"\r\n";
        std::fs::write(&dest, existing).unwrap();

        let removals: BTreeSet<String> = ["owned-old".to_string()].into();
        let mut overlays: BTreeMap<String, String> = BTreeMap::new();
        overlays.insert(
            "owned-new".to_string(),
            "[mcp_servers.owned-new]\r\ncommand = \"new\"\r\n".to_string(),
        );

        let outcome = write_codex_delta(&dest, &removals, &overlays).unwrap();
        assert_eq!(outcome, WriteOutcome::Written);

        let written = std::fs::read_to_string(&dest).unwrap();
        assert!(written.contains("[model]"), "non-mcp content preserved");
        assert!(
            written.contains("[mcp_servers.user]"),
            "unowned user section kept"
        );
        assert!(
            !written.contains("[mcp_servers.owned-old]"),
            "removed owned section is gone"
        );
        assert!(
            written.contains("[mcp_servers.owned-new]"),
            "overlay section written"
        );
    }
}
