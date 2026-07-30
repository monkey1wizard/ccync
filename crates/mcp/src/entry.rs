//! Canonical fingerprinting of a complete host-native MCP server entry.
//!
//! A fingerprint is a SHA-256 over a deterministic, order-normalized
//! serialization of the *exact* host-native server subtree. Map/table key
//! ordering is normalized away, but every value and every unknown field stays
//! significant — so a user edit inside an entry changes the fingerprint while a
//! mere re-ordering does not. Array order is preserved (it is semantically
//! meaningful, e.g. `args`).
//!
//! The projection ownership proof (write-ahead recovery + legacy bootstrap) uses
//! this to decide, *without persisting the entry's resolved content*, whether a
//! live host entry matches what ccync last wrote. Two host formats are covered:
//! JSON (Claude / Copilot / OpenCode) and Codex TOML.

use crate::McpUpdateError;
use sha2::{Digest, Sha256};

/// Sentinel token for a server entry that does not exist on a host. Stored in
/// the transaction journal in place of a fingerprint.
pub(crate) const ABSENT_SENTINEL: &str = "absent";

/// Proof of a single host-native server entry: either a SHA-256 fingerprint of
/// its canonical form, or `Absent` when the entry is not present on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EntryFingerprint {
    Absent,
    Sha256(String),
}

impl EntryFingerprint {
    /// Durable token form: `"absent"` or the lowercase hex digest. This is the
    /// only representation written to the journal — never the entry content.
    pub(crate) fn as_token(&self) -> String {
        match self {
            EntryFingerprint::Absent => ABSENT_SENTINEL.to_string(),
            EntryFingerprint::Sha256(hex) => hex.clone(),
        }
    }
}

/// Fingerprint a single host-native JSON server entry subtree (Claude, Copilot,
/// OpenCode). Object key ordering is normalized; all values + unknown fields
/// count toward the digest.
pub(crate) fn fingerprint_json(entry: &serde_json::Value) -> EntryFingerprint {
    let mut buf = String::new();
    write_canonical(entry, &mut buf);
    EntryFingerprint::Sha256(sha256_hex(buf.as_bytes()))
}

/// Fingerprint a single Codex TOML server entry subtree, given its TOML source
/// text. Parsed to a value then canonicalized identically to JSON, so table-key
/// ordering differences vanish while values + unknown keys remain significant.
pub(crate) fn fingerprint_codex_toml(toml_src: &str) -> Result<EntryFingerprint, McpUpdateError> {
    let value: toml::Value = toml::from_str(toml_src).map_err(|e| {
        // `toml::de::Error`'s Display output embeds the offending source
        // snippet. Here the source is a *rendered* entry, i.e. it carries the
        // fully-resolved credential values, so the diagnostic must stay
        // structural and content-free (same redaction as
        // `read_live_codex_entries` in lib.rs).
        McpUpdateError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "invalid Codex TOML server entry: {} (byte span {:?})",
                e.message(),
                e.span()
            ),
        ))
    })?;
    let json = serde_json::to_value(value)?;
    Ok(fingerprint_json(&json))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Deterministic, order-normalized serialization: object keys are emitted in
/// sorted order recursively; arrays keep their order (semantically significant);
/// scalars are emitted canonically. Independent of serde_json's `preserve_order`
/// feature — keys are sorted explicitly rather than relying on the map's
/// iteration order.
fn write_canonical(v: &serde_json::Value, out: &mut String) {
    use serde_json::Value;
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_json_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_string(k, out);
                out.push(':');
                write_canonical(map.get(*k).expect("key came from this map"), out);
            }
            out.push('}');
        }
    }
}

fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn host_entry_fingerprint_ignores_json_key_order() {
        let a = json!({"command":"bin","args":["x","y"],"env":{"A":"1","B":"2"}});
        let b = json!({"env":{"B":"2","A":"1"},"args":["x","y"],"command":"bin"});
        assert_eq!(fingerprint_json(&a), fingerprint_json(&b));
    }

    #[test]
    fn host_entry_fingerprint_differs_on_value_edit() {
        let a = json!({"command":"bin","env":{"A":"1"}});
        let b = json!({"command":"bin","env":{"A":"2"}});
        assert_ne!(fingerprint_json(&a), fingerprint_json(&b));
    }

    #[test]
    fn host_entry_fingerprint_differs_on_unknown_field() {
        let a = json!({"command":"bin"});
        let b = json!({"command":"bin","x-custom":"keep-me"});
        assert_ne!(
            fingerprint_json(&a),
            fingerprint_json(&b),
            "an unknown field must change the fingerprint"
        );
    }

    #[test]
    fn host_entry_fingerprint_array_order_is_significant() {
        let a = json!({"args":["x","y"]});
        let b = json!({"args":["y","x"]});
        assert_ne!(fingerprint_json(&a), fingerprint_json(&b));
    }

    #[test]
    fn host_entry_fingerprint_codex_toml_ignores_table_key_order() {
        // Same entry, env sub-table keys in different order.
        let a = "command = \"bin\"\n[env]\nA = \"1\"\nB = \"2\"\n";
        let b = "command = \"bin\"\n[env]\nB = \"2\"\nA = \"1\"\n";
        assert_eq!(
            fingerprint_codex_toml(a).unwrap(),
            fingerprint_codex_toml(b).unwrap()
        );
    }

    #[test]
    fn host_entry_fingerprint_codex_toml_differs_on_value() {
        let a = "command = \"bin\"\n[env]\nA = \"1\"\n";
        let b = "command = \"bin\"\n[env]\nA = \"2\"\n";
        assert_ne!(
            fingerprint_codex_toml(a).unwrap(),
            fingerprint_codex_toml(b).unwrap()
        );
    }

    #[test]
    fn host_entry_fingerprint_codex_toml_failure_does_not_disclose_entry_content() {
        // A rendered Codex entry carries *resolved* credential values (the
        // `${VAR}` placeholders are already substituted by the time an entry
        // reaches fingerprinting). `toml_string` in serializers/codex.rs
        // escapes only `\` and `"`, so a resolved value containing a control
        // character renders unparseable TOML — and `toml::de::Error`'s full
        // Display echoes the offending source line (the credential) verbatim.
        let secret = "sk-live-SECRET123";
        let toml_src = format!(
            "[mcp_servers.private]\ncommand = \"runner\"\n[mcp_servers.private.env]\nAPI_KEY = \"{secret}\u{0007}\"\n"
        );
        let legacy_failure = toml::from_str::<toml::Value>(&toml_src)
            .expect_err("fixture must remain malformed")
            .to_string();
        assert!(
            legacy_failure.contains(secret),
            "fixture must be one whose full Display leaks the value: {legacy_failure}"
        );

        let failure = fingerprint_codex_toml(&toml_src)
            .expect_err("malformed rendered entry must fail fingerprinting")
            .to_string();
        assert!(failure.contains("invalid Codex TOML server entry"));
        assert!(failure.contains("byte span"));
        // Assert on the *value*, not the key: under some malformations (e.g.
        // duplicate keys) `message()` legitimately echoes a key name, and key
        // names are not secrets. The resolved value must never appear.
        assert!(
            !failure.contains(secret),
            "resolved secret leaked in: {failure}"
        );
    }

    #[test]
    fn host_entry_fingerprint_absent_token() {
        assert_eq!(EntryFingerprint::Absent.as_token(), "absent");
        let fp = fingerprint_json(&json!({"command":"bin"}));
        assert_ne!(fp.as_token(), "absent");
        assert_ne!(EntryFingerprint::Absent, fp);
    }
}
