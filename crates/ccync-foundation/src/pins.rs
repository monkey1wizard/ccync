//! Shared lockfile-entry pin-read helper.
//!
//! Single source of truth for reading a personal-plugin pin out of a
//! `_personalPlugins` lockfile entry. Key precedence follows what the write
//! path actually writes (`pin` / `pinnedSha`), with `sha` kept last as
//! legacy-lockfile tolerance.

use serde_json::Value;

/// Read the pin sha from a lockfile entry, preferring `pin`, then
/// `pinnedSha`, then `sha` (legacy tolerance). An empty string at any key is
/// treated as missing and falls through to the next key.
pub fn entry_pin(entry: &Value) -> Option<String> {
    for key in ["pin", "pinnedSha", "sha"] {
        if let Some(value) = entry.get(key).and_then(Value::as_str) {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_pin_key_present() {
        let entry = json!({"pin": "abc123"});
        assert_eq!(entry_pin(&entry), Some("abc123".to_string()));
    }

    #[test]
    fn only_pinned_sha_key_present() {
        let entry = json!({"pinnedSha": "def456"});
        assert_eq!(entry_pin(&entry), Some("def456".to_string()));
    }

    #[test]
    fn only_sha_key_present() {
        let entry = json!({"sha": "ghi789"});
        assert_eq!(entry_pin(&entry), Some("ghi789".to_string()));
    }

    #[test]
    fn pin_and_pinned_sha_present_prefers_pin() {
        let entry = json!({"pin": "abc123", "pinnedSha": "def456"});
        assert_eq!(entry_pin(&entry), Some("abc123".to_string()));
    }

    #[test]
    fn pin_and_sha_present_prefers_pin() {
        let entry = json!({"pin": "abc123", "sha": "ghi789"});
        assert_eq!(entry_pin(&entry), Some("abc123".to_string()));
    }

    #[test]
    fn all_three_keys_present_prefers_pin() {
        let entry = json!({"pin": "abc123", "pinnedSha": "def456", "sha": "ghi789"});
        assert_eq!(entry_pin(&entry), Some("abc123".to_string()));
    }

    #[test]
    fn no_keys_present_returns_none() {
        let entry = json!({"id": "some-plugin"});
        assert_eq!(entry_pin(&entry), None);
    }

    #[test]
    fn empty_pin_falls_through_to_pinned_sha() {
        let entry = json!({"pin": "", "pinnedSha": "def456"});
        assert_eq!(entry_pin(&entry), Some("def456".to_string()));
    }

    #[test]
    fn empty_pin_and_pinned_sha_falls_through_to_sha() {
        let entry = json!({"pin": "", "pinnedSha": "", "sha": "ghi789"});
        assert_eq!(entry_pin(&entry), Some("ghi789".to_string()));
    }

    #[test]
    fn all_empty_strings_returns_none() {
        let entry = json!({"pin": "", "pinnedSha": "", "sha": ""});
        assert_eq!(entry_pin(&entry), None);
    }
}
