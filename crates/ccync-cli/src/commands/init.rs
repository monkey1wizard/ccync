//! `ccync init [<claude|codex>]` — pick the master agent, adopt its content
//! into the ccync truth store, record `_adoptMaster`, write `install-state.json`,
//! then project the managed set to all agents via the unified engine.
//!
//! Master selection is owned here (moved off `ccync sync`, which used to carry
//! `--import-from`). Interactive runtime selection reuses `setup::session::Prompter`.

use ccync_engine::ExitCode;
use serde_json::Value;
use std::io::IsTerminal;
use std::path::Path;

pub(crate) fn cmd_init(args: &[String]) -> ExitCode {
    // args[0] = "init"; args[1] = optional master (claude|codex).
    let master = match args.get(1).map(String::as_str) {
        Some(explicit) => {
            let m = explicit.to_ascii_lowercase();
            if m != "claude" && m != "codex" {
                eprintln!("ccync init: unknown master '{explicit}'; expected 'claude' or 'codex'");
                return ExitCode::Usage;
            }
            m
        }
        None => match prompt_master() {
            Some(m) => m,
            None => {
                eprintln!("ccync init: no master selected; expected 'claude' or 'codex'");
                return ExitCode::Usage;
            }
        },
    };

    // 1. Adopt the master's content into the ccync truth store + record master.
    if let Err(code) = run_import_adopt(&master) {
        return code;
    }

    // 2. Resolve the runtime selection and persist install-state.json
    //    (default = all valid runtimes; interactive override via Prompter).
    if let Err(e) = write_install_state() {
        eprintln!("ccync init: could not write install state (best-effort): {e}");
    }

    // 3. Project the managed set to all agents via the unified engine.
    let assume_yes = args.iter().any(|a| a == "--yes");
    super::lifecycle::run_unified_projection(false, assume_yes)
}

/// Interactive master pick via stdin (claude|codex). Returns None on EOF /
/// non-interactive / invalid.
fn prompt_master() -> Option<String> {
    if !std::io::stdin().is_terminal() {
        return None;
    }
    eprintln!("  [PROMPT] Master agent to adopt from (claude|codex):");
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return None;
    }
    let answer = line.trim().to_ascii_lowercase();
    match answer.as_str() {
        "claude" | "codex" => Some(answer),
        _ => None,
    }
}

/// Read the master agent's state, adopt non-ccync-managed items into the lock
/// file, and record the master under `_adoptMaster`. Prints a summary.
fn run_import_adopt(master: &str) -> Result<(), ExitCode> {
    use ccync_engine::adopt::{adopt_items, read_claude_state, read_codex_state};
    use ccync_foundation::paths::plugins_lock_path;

    let lock_path = plugins_lock_path().ok_or_else(|| {
        eprintln!("ccync init: cannot resolve home directory");
        ExitCode::Error
    })?;

    let items = match master {
        "claude" => read_claude_state(),
        "codex" => read_codex_state(),
        other => {
            eprintln!("ccync init: unknown agent '{other}'; expected 'claude' or 'codex'");
            return Err(ExitCode::Usage);
        }
    };

    let newly_adopted = adopt_items(items, &lock_path).map_err(|e| {
        eprintln!("ccync init: adopt failed: {e}");
        ExitCode::Error
    })?;

    record_master_in_lock(&lock_path, master).map_err(|e| {
        eprintln!("ccync init: could not record master: {e}");
        ExitCode::Error
    })?;

    println!(
        "ccync init {master}: adopted {} item(s); master recorded",
        newly_adopted.len()
    );
    for item in &newly_adopted {
        println!(
            "  + {} ({})",
            item.name,
            item.version.as_deref().unwrap_or("no version")
        );
    }
    Ok(())
}

/// Splice `_adoptMaster` into the lock file, preserving all other namespaces.
fn record_master_in_lock(lock_path: &Path, master: &str) -> Result<(), String> {
    let existing: Value = if lock_path.exists() {
        let text = std::fs::read_to_string(lock_path).map_err(|e| e.to_string())?;
        serde_json::from_str(&text).unwrap_or(Value::Object(serde_json::Map::new()))
    } else {
        Value::Object(serde_json::Map::new())
    };

    let updated = match existing {
        Value::Object(mut map) => {
            map.insert(
                "_adoptMaster".to_string(),
                Value::String(master.to_string()),
            );
            Value::Object(map)
        }
        _ => serde_json::json!({ "_adoptMaster": master }),
    };

    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(
        lock_path,
        serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

/// Persist `install-state.json` with the selected runtime set. Default = all
/// valid runtimes; an interactive TTY may override the selection / primary via
/// `setup::session::Prompter`.
fn write_install_state() -> Result<(), String> {
    use ccync_foundation::runtime::{default_primary_runtime, VALID_RUNTIMES};
    use setup::session::{Prompter, StdinPrompter};

    let default_selection: Vec<String> = VALID_RUNTIMES.iter().map(|s| (*s).to_string()).collect();

    let (selected, primary) = if std::io::stdin().is_terminal() {
        let mut prompter = StdinPrompter;
        let selected = prompter.select_runtimes(&default_selection);
        let selected = if selected.is_empty() {
            default_selection.clone()
        } else {
            selected
        };
        let default_primary =
            default_primary_runtime(&selected.iter().map(String::as_str).collect::<Vec<_>>())
                .unwrap_or("claude")
                .to_string();
        let primary = prompter.select_primary(&selected, &default_primary);
        (selected, primary)
    } else {
        let primary = default_primary_runtime(
            &default_selection
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        )
        .unwrap_or("claude")
        .to_string();
        (default_selection, primary)
    };

    let install_state =
        ccync_foundation::paths::install_state_path().ok_or("cannot resolve install-state path")?;
    if let Some(parent) = install_state.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut state = serde_json::Map::new();
    state.insert("schemaVersion".into(), serde_json::json!(1));
    state.insert("selectedRuntimes".into(), serde_json::json!(selected));
    state.insert("primaryRuntime".into(), serde_json::json!(primary));
    ccync_foundation::json_util::write_json_map(&install_state, &state).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_init_unknown_master_is_usage() {
        assert_eq!(
            cmd_init(&["init".to_string(), "frobnicate".to_string()]),
            ExitCode::Usage
        );
    }

    #[test]
    fn record_master_in_lock_writes_and_preserves() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        std::fs::write(&lock, r#"{"_ccyncProjection":{"x":1}}"#).unwrap();
        record_master_in_lock(&lock, "claude").unwrap();
        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
        assert_eq!(content["_adoptMaster"], "claude");
        assert!(
            content.get("_ccyncProjection").is_some(),
            "_ccyncProjection must be preserved"
        );
    }

    #[test]
    fn record_master_in_lock_absent_file_created() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("state").join("plugins.lock.json");
        record_master_in_lock(&lock, "codex").unwrap();
        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
        assert_eq!(content["_adoptMaster"], "codex");
    }
}
