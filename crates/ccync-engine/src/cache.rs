//! Cache lifecycle: compute which `~/.ccync/cache/<id>@<sha>` directories are
//! orphaned (no longer referenced by the lockfile's live pins) so `ccync
//! cleanup` can prune them safely.
//!
//! The core is pure and fail-closed: orphan detection is a set difference
//! against the *known* live set, and when the live set cannot be determined
//! (unreadable / unparseable lockfile) nothing is reported as orphan.

use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;

/// Build the live cache-key set (`<id>@<sha>`) from a parsed lockfile value.
///
/// Reads `_personalPlugins[]`, taking the resolved `pin` (or `pinnedSha` /
/// `sha`) per entry. A parsed-but-empty personal set yields an empty
/// live set — that is a *known* "nothing is live" answer, distinct from the
/// caller passing `None` for "could not determine" (see [`orphan_cache_dirs`]).
pub fn live_pins_from_lock(lock: &Value) -> HashSet<String> {
    let mut set = HashSet::new();
    if let Some(arr) = lock.get("_personalPlugins").and_then(Value::as_array) {
        for e in arr {
            let id = e.get("pluginId").and_then(Value::as_str);
            let sha = ccync_foundation::pins::entry_pin(e);
            if let (Some(id), Some(sha)) = (id, sha) {
                set.insert(format!("{id}@{sha}"));
            }
        }
    }
    set
}

/// Orphan cache dirs = `entries` whose `<id>@<sha>` basename is not in `live`.
///
/// Fail-closed: when `live` is `None` (the live set could not be determined —
/// e.g. the lockfile is missing or unparseable), this returns empty, so the
/// caller deletes nothing. A directory whose basename is not valid UTF-8 is
/// kept (never reported as orphan) rather than guessed at.
pub fn orphan_cache_dirs(entries: &[PathBuf], live: Option<&HashSet<String>>) -> Vec<PathBuf> {
    let Some(live) = live else {
        return Vec::new();
    };
    entries
        .iter()
        .filter(|p| match p.file_name().and_then(|n| n.to_str()) {
            Some(name) => !live.contains(name),
            None => false,
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dirs(names: &[&str]) -> Vec<PathBuf> {
        names
            .iter()
            .map(|n| PathBuf::from(format!("/cache/{n}")))
            .collect()
    }

    #[test]
    fn orphans_are_entries_not_in_live() {
        let entries = dirs(&["a@1", "b@2", "c@3"]);
        let live: HashSet<String> = ["b@2".to_string()].into_iter().collect();
        let orphans = orphan_cache_dirs(&entries, Some(&live));
        assert_eq!(orphans, dirs(&["a@1", "c@3"]));
    }

    #[test]
    fn none_live_is_fail_closed() {
        let entries = dirs(&["a@1", "b@2", "c@3"]);
        assert!(
            orphan_cache_dirs(&entries, None).is_empty(),
            "undetermined live set must delete nothing"
        );
    }

    #[test]
    fn known_empty_live_orphans_everything() {
        let entries = dirs(&["a@1", "b@2"]);
        let empty = HashSet::new();
        assert_eq!(orphan_cache_dirs(&entries, Some(&empty)), entries);
    }

    #[test]
    fn live_pins_reads_sha_pin_pinnedsha() {
        let lock = json!({
            "_personalPlugins": [
                { "pluginId": "a", "sha": "111" },
                { "pluginId": "b", "pin": "222" },
                { "pluginId": "c", "pinnedSha": "333" },
                { "pluginId": "d" }
            ]
        });
        let live = live_pins_from_lock(&lock);
        assert!(live.contains("a@111"));
        assert!(live.contains("b@222"));
        assert!(live.contains("c@333"));
        assert!(
            !live.iter().any(|k| k.starts_with("d@")),
            "no sha → not live"
        );
    }

    #[test]
    fn live_pins_missing_section_is_empty() {
        assert!(live_pins_from_lock(&json!({})).is_empty());
    }
}
