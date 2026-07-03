//! `ccync pin <id>` / `pin --remove <id>` / `pin --list` — hold a personal
//! plugin at its current sha so `ccync upgrade` skips it (winget-style pin).
//!
//! NB: "pin" the verb means *hold against upgrade* (state lives in the `held`
//! field); it is distinct from the `pin`/`pinnedSha` fields which record the
//! *locked commit sha*. The hold state is written with a raw-JSON merge (never
//! a typed round-trip) and then re-resolved so the lockfile carries `held`.

use super::plugin::{read_personal_catalog, run_personal_resolve, write_json_atomic};
use ccync_engine::ExitCode;
use serde_json::Value;

pub(crate) fn cmd_pin(args: &[String]) -> ExitCode {
    if let Err(code) = super::args::reject_unknown_flags(
        "pin",
        args,
        &["--remove", "--list"],
        &[],
        1,
        "Usage: ccync pin <id> | ccync pin --remove <id> | ccync pin --list",
    ) {
        return code;
    }
    let rest = &args[1..]; // skip the "pin" verb

    if rest.iter().any(|a| a == "--list") {
        return list_held();
    }

    let remove = rest.iter().any(|a| a == "--remove");
    let Some(id) = rest
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(String::as_str)
    else {
        eprintln!("ccync pin: missing <id> argument");
        eprintln!("Usage: ccync pin <id> | ccync pin --remove <id> | ccync pin --list");
        return ExitCode::Usage;
    };

    set_held(id, !remove)
}

fn set_held(id: &str, held: bool) -> ExitCode {
    let Some(catalog_path) = ccync_foundation::paths::personal_catalog_path() else {
        eprintln!("ccync pin: cannot determine home directory");
        return ExitCode::Error;
    };
    let Some(mut catalog) = read_personal_catalog(&catalog_path) else {
        eprintln!("ccync pin: personal catalog not found. Add a plugin first with `ccync add`.");
        return ExitCode::Usage;
    };

    if !apply_held(&mut catalog, id, held) {
        eprintln!("ccync pin: plugin '{id}' not found. Run `ccync list` to see installed plugins.");
        return ExitCode::Usage;
    }

    if let Err(e) = write_json_atomic(&catalog_path, &catalog) {
        eprintln!("ccync pin: failed to write personal catalog: {e}");
        return ExitCode::Error;
    }
    // Re-resolve so the lockfile's _personalPlugins carries the new held state
    // (the splice now preserves `held`), which is what `upgrade` reads.
    if let Err(e) = run_personal_resolve() {
        eprintln!("ccync pin: resolve step failed: {e}");
        return ExitCode::Error;
    }

    if held {
        println!("Pinned '{id}' — `ccync upgrade` will hold it at its current sha.");
    } else {
        println!("Unpinned '{id}' — `ccync upgrade` will consider it again.");
    }
    ExitCode::Success
}

fn list_held() -> ExitCode {
    let entries = ccync_foundation::paths::personal_catalog_path()
        .as_deref()
        .and_then(read_personal_catalog)
        .and_then(|c| c.get("plugins").and_then(Value::as_array).cloned())
        .unwrap_or_default();

    let held: Vec<&str> = entries
        .iter()
        .filter(|e| e.get("held").and_then(Value::as_bool).unwrap_or(false))
        .filter_map(|e| e.get("pluginId").and_then(Value::as_str))
        .collect();

    if held.is_empty() {
        println!("No held plugins.");
    } else {
        println!("Held plugins:");
        for id in held {
            println!("  {id}");
        }
    }
    ExitCode::Success
}

/// Set or clear the `held` flag on the catalog entry for `id` (raw-JSON merge).
/// Returns whether an entry was found. Pure aside from the `&mut` it edits.
fn apply_held(catalog: &mut Value, id: &str, held: bool) -> bool {
    let Some(arr) = catalog.get_mut("plugins").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut found = false;
    for e in arr.iter_mut() {
        if e.get("pluginId").and_then(Value::as_str) == Some(id) {
            found = true;
            if held {
                e["held"] = Value::Bool(true);
            } else if let Some(obj) = e.as_object_mut() {
                obj.remove("held");
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn apply_held_sets_and_clears() {
        let mut catalog = json!({ "plugins": [
            { "pluginId": "a", "source": "x" },
            { "pluginId": "b", "source": "y" }
        ]});
        assert!(apply_held(&mut catalog, "a", true));
        assert_eq!(catalog["plugins"][0]["held"], true);
        // b untouched
        assert!(catalog["plugins"][1].get("held").is_none());
        // clearing removes the key entirely
        assert!(apply_held(&mut catalog, "a", false));
        assert!(catalog["plugins"][0].get("held").is_none());
    }

    #[test]
    fn apply_held_unknown_id_is_false() {
        let mut catalog = json!({ "plugins": [ { "pluginId": "a" } ] });
        assert!(!apply_held(&mut catalog, "nope", true));
    }
}
