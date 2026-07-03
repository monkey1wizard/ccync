//! `ccync cleanup [--dry-run]` — prune orphaned `~/.ccync/cache/<id>@<sha>`
//! directories no longer referenced by the lockfile (brew-cleanup semantics).
//!
//! Safety model (cache is regenerable, but we still never guess):
//! - Lockfile unreadable/unparseable → live set undetermined → delete nothing.
//! - A dir whose id is absent from the lockfile → removed plugin → prune.
//! - A dir whose id has a live pin but a *different* sha → stale sha → prune.
//! - A dir whose id is in the lockfile but has NO live pin → ambiguous →
//!   retain + warn (fail-closed; never delete an undecidable id's dir).

use ccync_engine::cache::{live_pins_from_lock, orphan_cache_dirs};
use ccync_engine::ExitCode;
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;

pub(crate) fn cmd_cleanup(args: &[String]) -> ExitCode {
    if let Err(code) = super::args::reject_unknown_flags(
        "cleanup",
        args,
        &["--dry-run"],
        &[],
        0,
        "Usage: ccync cleanup [--dry-run]",
    ) {
        return code;
    }
    let dry_run = args.iter().any(|a| a == "--dry-run");

    let Some(cache_root) = ccync_foundation::paths::ccync_cache_root() else {
        eprintln!("ccync cleanup: cannot resolve ~/.ccync/cache");
        return ExitCode::Error;
    };

    // Live set + managed ids from the lockfile. read failure → None (fail-closed).
    let lock = ccync_foundation::paths::plugins_lock_path().and_then(read_json);
    let live = lock.as_ref().map(live_pins_from_lock);
    let lock_ids = lock.as_ref().map(lock_ids_from).unwrap_or_default();

    if live.is_none() {
        eprintln!(
            "ccync cleanup: lockfile unreadable — live set undetermined; nothing pruned (fail-closed)."
        );
        return ExitCode::Success;
    }

    let entries = cache_dirs(&cache_root);
    let raw_orphans = orphan_cache_dirs(&entries, live.as_ref());
    let (deletable, ambiguous) =
        partition_deletable(&raw_orphans, live.as_ref().unwrap(), &lock_ids);

    for p in &ambiguous {
        eprintln!(
            "ccync cleanup: retained {} — id has no resolved live pin (ambiguous, not pruned).",
            p.display()
        );
    }

    if deletable.is_empty() {
        println!("ccync cleanup: nothing to prune.");
        return ExitCode::Success;
    }

    if dry_run {
        println!("ccync cleanup --dry-run: would remove:");
        for p in &deletable {
            println!("  {}", p.display());
        }
        return ExitCode::Success;
    }

    let mut removed = 0usize;
    for p in &deletable {
        match std::fs::remove_dir_all(p) {
            Ok(()) => {
                println!("removed {}", p.display());
                removed += 1;
            }
            Err(e) => eprintln!("ccync cleanup: failed to remove {}: {e}", p.display()),
        }
    }
    println!("ccync cleanup: {removed} dir(s) pruned.");
    ExitCode::Success
}

/// Split raw orphan candidates into (deletable, ambiguous-retained).
///
/// An orphan `id@sha` is **deletable** when its id is absent from the lockfile
/// (removed plugin) OR the id has some live pin (so this dir is a stale sha).
/// It is **ambiguous** (retained) when the id is managed but has no live pin —
/// we cannot tell which dir is current, so we keep it.
fn partition_deletable(
    orphans: &[PathBuf],
    live: &HashSet<String>,
    lock_ids: &HashSet<String>,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let ids_with_live_pin: HashSet<&str> =
        live.iter().filter_map(|k| k.split('@').next()).collect();

    let mut deletable = Vec::new();
    let mut ambiguous = Vec::new();
    for p in orphans {
        let id = p
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|name| name.split('@').next())
            .unwrap_or("");
        let managed = lock_ids.contains(id);
        if !managed || ids_with_live_pin.contains(id) {
            deletable.push(p.clone());
        } else {
            ambiguous.push(p.clone());
        }
    }
    (deletable, ambiguous)
}

/// Plugin ids present in the lockfile's `_personalPlugins` (regardless of sha).
fn lock_ids_from(lock: &Value) -> HashSet<String> {
    lock.get("_personalPlugins")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.get("pluginId").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Immediate `<id>@<sha>` subdirectories of the cache root.
fn cache_dirs(cache_root: &std::path::Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(cache_root) else {
        return Vec::new();
    };
    rd.filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}

fn read_json(path: PathBuf) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(names: &[&str]) -> Vec<PathBuf> {
        names
            .iter()
            .map(|n| PathBuf::from(format!("/cache/{n}")))
            .collect()
    }

    #[test]
    fn stale_sha_is_deletable_ambiguous_is_retained() {
        // a: managed with live pin a@new → a@old is stale (deletable).
        // b: not in lockfile → removed plugin (deletable).
        // c: managed but no live pin → ambiguous (retained).
        let orphans = dirs(&["a@old", "b@1", "c@x"]);
        let live: HashSet<String> = ["a@new".to_string()].into_iter().collect();
        let lock_ids: HashSet<String> =
            ["a".to_string(), "c".to_string()].into_iter().collect();
        let (deletable, ambiguous) = partition_deletable(&orphans, &live, &lock_ids);
        assert_eq!(deletable, dirs(&["a@old", "b@1"]));
        assert_eq!(ambiguous, dirs(&["c@x"]));
    }
}
