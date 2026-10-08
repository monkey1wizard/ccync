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

pub(crate) fn cmd_pin(
    args: &[String],
    guard: Option<&ccync_engine::tool_manager::lock::LifecycleGuard>,
) -> ExitCode {
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

    if let Some(raw_id) = id.strip_prefix("tool:") {
        let id = match ccync_engine::tool_manager::model::ToolId::new(raw_id.to_string()) {
            Ok(id) => id,
            Err(_) => {
                eprintln!("ccync pin: invalid tool id");
                return ExitCode::Usage;
            }
        };
        let Some(_guard) = guard else {
            eprintln!("ccync pin: lifecycle guard unavailable");
            return ExitCode::Error;
        };
        let mut declarations = match ccync_engine::tool_manager::store::load_declarations() {
            Ok(value) => value,
            Err(error) => {
                eprintln!("ccync pin: {error}");
                return ExitCode::Error;
            }
        };
        let Some(declaration) = declarations.get_mut(&id) else {
            eprintln!("ccync pin: tool '{id}' not found");
            return ExitCode::Error;
        };
        declaration.hold_policy = if remove {
            ccync_engine::tool_manager::model::HoldPolicy::Unheld
        } else {
            ccync_engine::tool_manager::model::HoldPolicy::Held
        };
        return match ccync_engine::tool_manager::store::replace_declarations(&declarations) {
            Ok(()) => {
                println!("{} '{id}'", if remove { "Released" } else { "Held" });
                ExitCode::Success
            }
            Err(error) => {
                eprintln!("ccync pin: {error}");
                ExitCode::Error
            }
        };
    }
    set_held(id, !remove)
}

fn set_held(id: &str, held: bool) -> ExitCode {
    let Some(catalog_path) = ccync_foundation::paths::personal_catalog_path() else {
        eprintln!("ccync pin: cannot determine home directory");
        return ExitCode::Error;
    };
    let Some(mut catalog) = read_personal_catalog(&catalog_path) else {
        eprintln!("ccync pin: personal catalog not found. Add a plugin first with `ccync add`.");
        return ExitCode::Error;
    };

    if !apply_held(&mut catalog, id, held) {
        eprintln!("ccync pin: plugin '{id}' not found. Run `ccync list` to see installed plugins.");
        return ExitCode::Error;
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
        println!(
            "Held '{id}' — `ccync upgrade` will hold it at its current sha. Run `ccync pin --remove {id}` to release."
        );
    } else {
        println!("Released '{id}' — `ccync upgrade` will consider it again.");
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
    use tempfile::TempDir;

    fn with_env_home<F: FnOnce() -> ExitCode>(home: &std::path::Path, f: F) -> ExitCode {
        let orig_home;
        #[cfg(windows)]
        unsafe {
            orig_home = std::env::var("USERPROFILE").ok();
            std::env::set_var("USERPROFILE", home);
            std::env::set_var("HOME", home);
        }
        #[cfg(not(windows))]
        unsafe {
            orig_home = std::env::var("HOME").ok();
            std::env::set_var("HOME", home);
        }

        let result = f();

        #[cfg(windows)]
        unsafe {
            match &orig_home {
                Some(v) => {
                    std::env::set_var("USERPROFILE", v);
                    std::env::set_var("HOME", v);
                }
                None => {
                    std::env::remove_var("USERPROFILE");
                    std::env::remove_var("HOME");
                }
            }
        }
        #[cfg(not(windows))]
        unsafe {
            match &orig_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        result
    }

    #[test]
    fn cmd_pin_missing_catalog_returns_error_not_usage() {
        let _guard = crate::test_io_lock();
        let home = TempDir::new().unwrap();
        // No ~/.ccync/plugins.json at all.
        let exit = with_env_home(home.path(), || {
            cmd_pin(&["pin".to_string(), "some-id".to_string()], None)
        });
        assert_eq!(
            exit,
            ExitCode::Error,
            "missing personal catalog is an execution-time lookup failure, not Usage"
        );
    }

    #[test]
    fn cmd_pin_unknown_id_returns_error_not_usage() {
        let _guard = crate::test_io_lock();
        let home = TempDir::new().unwrap();
        let ccync = home.path().join(".ccync");
        std::fs::create_dir_all(&ccync).unwrap();
        std::fs::write(
            ccync.join("plugins.json"),
            r#"{"plugins":[{"pluginId":"real-plugin"}]}"#,
        )
        .unwrap();

        let exit = with_env_home(home.path(), || {
            cmd_pin(&["pin".to_string(), "ghost-plugin".to_string()], None)
        });
        assert_eq!(
            exit,
            ExitCode::Error,
            "unknown plugin id is an execution-time lookup failure, not Usage"
        );
    }

    #[test]
    fn source_hold_vocabulary_does_not_reuse_sha_pin_words() {
        // HOLD family (held/hold/release) must be distinct from
        // SHA-PIN family (pinned sha/unpinned) so `pin --remove` and
        // `list`'s `(unpinned)` sha display never collide on the same word.
        let this_file = include_str!("pin.rs");
        assert!(
            this_file.contains("Held '{id}'"),
            "hold-set message must use Held"
        );
        assert!(
            this_file.contains("Released '{id}'"),
            "hold-clear message must use Released"
        );
        assert!(
            !this_file.contains("println!(\"Pinned"),
            "cmd_pin success output must not reuse the sha-pin word 'Pinned'"
        );
        assert!(
            !this_file.contains("println!(\"Unpinned"),
            "cmd_pin success output must not reuse the sha-pin word 'Unpinned'"
        );
    }

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

    #[test]
    fn pin_tool_hold_and_release() {
        let _guard = crate::test_io_lock();
        let home = TempDir::new().unwrap();
        let ccync_dir = home.path().join(".ccync");
        std::fs::create_dir_all(&ccync_dir).unwrap();

        let tool_id = ccync_engine::tool_manager::model::ToolId::new("my-tool").unwrap();
        let decl = ccync_engine::tool_manager::model::ToolDeclaration {
            id: tool_id.clone(),
            identity: ccync_engine::tool_manager::model::InstallationIdentity {
                adapter_kind: ccync_engine::tool_manager::model::AdapterKind::Cargo,
                fields: std::collections::BTreeMap::new(),
            },
            delivery_id: ccync_engine::tool_manager::model::DeliveryId::RtkPath,
            hold_policy: ccync_engine::tool_manager::model::HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        };
        let mut decls = std::collections::BTreeMap::new();
        decls.insert(tool_id.clone(), decl);

        with_env_home(home.path(), || {
            ccync_engine::tool_manager::store::replace_declarations(&decls).unwrap();
            let lock_guard = ccync_engine::tool_manager::lock::LifecycleLock::acquire().unwrap();

            // Hold tool
            let exit_hold = cmd_pin(
                &["pin".to_string(), "tool:my-tool".to_string()],
                Some(&lock_guard),
            );
            assert_eq!(exit_hold, ExitCode::Success);

            let loaded = ccync_engine::tool_manager::store::load_declarations().unwrap();
            assert_eq!(
                loaded.get(&tool_id).unwrap().hold_policy,
                ccync_engine::tool_manager::model::HoldPolicy::Held
            );

            // Release tool
            let exit_release = cmd_pin(
                &[
                    "pin".to_string(),
                    "--remove".to_string(),
                    "tool:my-tool".to_string(),
                ],
                Some(&lock_guard),
            );
            assert_eq!(exit_release, ExitCode::Success);

            let loaded2 = ccync_engine::tool_manager::store::load_declarations().unwrap();
            assert_eq!(
                loaded2.get(&tool_id).unwrap().hold_policy,
                ccync_engine::tool_manager::model::HoldPolicy::Unheld
            );

            ExitCode::Success
        });
    }

    #[test]
    fn pin_tool_unknown_id() {
        let _guard = crate::test_io_lock();
        let home = TempDir::new().unwrap();
        let ccync_dir = home.path().join(".ccync");
        std::fs::create_dir_all(&ccync_dir).unwrap();

        with_env_home(home.path(), || {
            let decls = std::collections::BTreeMap::new();
            ccync_engine::tool_manager::store::replace_declarations(&decls).unwrap();
            let lock_guard = ccync_engine::tool_manager::lock::LifecycleLock::acquire().unwrap();

            let exit = cmd_pin(
                &["pin".to_string(), "tool:ghost-tool".to_string()],
                Some(&lock_guard),
            );
            assert_eq!(exit, ExitCode::Error, "unknown tool id returns Error, not Usage");
            ExitCode::Success
        });
    }

    #[test]
    fn pin_tool_unsafe_id() {
        let _guard = crate::test_io_lock();
        let home = TempDir::new().unwrap();

        with_env_home(home.path(), || {
            let lock_guard = ccync_engine::tool_manager::lock::LifecycleLock::acquire().unwrap();
            let exit = cmd_pin(
                &["pin".to_string(), "tool:../unsafe".to_string()],
                Some(&lock_guard),
            );
            assert_eq!(exit, ExitCode::Usage, "unsafe tool id must return Usage");
            ExitCode::Success
        });
    }

    #[test]
    fn pin_plugin_behavior_unchanged() {
        let _guard = crate::test_io_lock();
        let home = TempDir::new().unwrap();
        let ccync = home.path().join(".ccync");
        std::fs::create_dir_all(&ccync).unwrap();
        std::fs::write(
            ccync.join("plugins.json"),
            r#"{"plugins":[{"pluginId":"my-plugin","source":"https://example.com/repo.git"}]}"#,
        )
        .unwrap();
        std::fs::write(ccync.join("config.json"), r#"{"schemaVersion":1}"#).unwrap();

        with_env_home(home.path(), || {
            // Pin plugin (bare id)
            let exit_hold = cmd_pin(
                &["pin".to_string(), "my-plugin".to_string()],
                None,
            );
            assert_eq!(exit_hold, ExitCode::Success);
            let text = std::fs::read_to_string(ccync.join("plugins.json")).unwrap();
            let val: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(val["plugins"][0]["held"], true);

            // Unpin plugin
            let exit_remove = cmd_pin(
                &[
                    "pin".to_string(),
                    "--remove".to_string(),
                    "my-plugin".to_string(),
                ],
                None,
            );
            assert_eq!(exit_remove, ExitCode::Success);
            let text2 = std::fs::read_to_string(ccync.join("plugins.json")).unwrap();
            let val2: Value = serde_json::from_str(&text2).unwrap();
            assert!(val2["plugins"][0].get("held").is_none());

            ExitCode::Success
        });
    }
}
