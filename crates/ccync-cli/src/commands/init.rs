//! `ccync init [<claude|codex>]` — pick the master agent, adopt its content
//! into the ccync truth store, record `_adoptMaster`, write runtime selection
//! into `config.json`, then project the managed set to all agents via the unified engine.
//!
//! Master selection is owned here (moved off `ccync sync`, which used to carry
//! `--import-from`).

use ccync_engine::ExitCode;
use serde_json::Value;
use std::io::IsTerminal;
use std::path::Path;

pub(crate) fn cmd_init(args: &[String]) -> ExitCode {
    if let Err(code) = super::args::reject_unknown_flags(
        "init",
        args,
        &[],
        &[],
        1,
        "Usage: ccync init [claude|codex]",
    ) {
        return code;
    }
    // args[0] = "init"; args[1] = optional master (claude|codex).
    let master = match args.get(1).map(String::as_str) {
        Some(explicit) => {
            let m = explicit.to_ascii_lowercase();
            if m != "claude" && m != "codex" {
                eprintln!("ccync init: unknown agent '{explicit}'; expected 'claude' or 'codex'");
                return ExitCode::Usage;
            }
            m
        }
        None => match prompt_master() {
            Some(m) => m,
            None => {
                eprintln!("ccync init: no agent selected; expected 'claude' or 'codex'");
                return ExitCode::Usage;
            }
        },
    };

    // 1. Adopt the master's content into the ccync truth store + record master.
    if let Err(code) = run_import_adopt(&master) {
        return code;
    }

    // 2. Let the user pick which agents to project to, then persist the selection.
    let selected = prompt_runtimes(&master);
    if let Err(e) = write_runtime_selection_to_config(&master, &selected) {
        eprintln!("ccync init: could not write runtime selection (best-effort): {e}");
    }

    // 3. Project the managed set. init already showed the agent-selection UI,
    //    so skip the first-run gate (skip_first_run_gate=true).
    super::lifecycle::run_unified_projection(false, true, true)
}

/// Detect which agent is likely in use from known config file locations.
/// Returns "claude" or "codex", or None if undetermined.
fn detect_agent_from_env() -> Option<&'static str> {
    let home = ccync_foundation::paths::user_home()?;
    // Claude Code: ~/.claude.json or ~/.claude/ directory
    if ccync_foundation::paths::claude_config_path().is_some_and(|p| p.exists())
        || home.join(".claude").is_dir()
    {
        return Some("claude");
    }
    // Codex: ~/.codex/config.toml
    if ccync_foundation::paths::codex_config_path().is_some_and(|p| p.exists()) {
        return Some("codex");
    }
    None
}

/// Interactive agent pick via arrow-key Select. Returns None on non-interactive
/// stdin or if the user cancels (Esc / Ctrl-C).
fn prompt_master() -> Option<String> {
    if !std::io::stdin().is_terminal() {
        return None;
    }
    let detected = detect_agent_from_env();
    let agents = ["claude", "codex"];
    let default_idx = agents
        .iter()
        .position(|&a| Some(a) == detected)
        .unwrap_or(0);

    let selection = dialoguer::Select::new()
        .with_prompt("Import plugins from which agent?")
        .items(&agents)
        .default(default_idx)
        .interact_opt()
        .ok()??;

    Some(agents[selection].to_string())
}

/// Display entries for the runtime MultiSelect.
/// The synthetic "agy" entry expands to all three Antigravity surfaces on selection.
const RUNTIME_ENTRIES: &[(&str, &str, bool)] = &[
    // (display_key, label, default_checked)
    ("claude",     "claude     (~/.claude.json)",           true),
    ("codex",      "codex      (~/.codex/config.toml)",     true),
    ("copilot",    "copilot    (~/.copilot/mcp-config.json)", true),
    ("opencode",   "opencode   (opencode.json)",            false),
    ("agy",        "agy        (Antigravity cli/ide/gui)",  false),
    ("gemini-cli", "gemini-cli (~/.gemini/)",               false),
];

/// Expand display keys back to canonical VALID_RUNTIMES keys.
/// "agy" → ["agy-cli", "agy-ide", "agy-gui"]; all others map to themselves.
fn expand_runtime_key(key: &str) -> &'static [&'static str] {
    match key {
        "agy" => &["agy-cli", "agy-ide", "agy-gui"],
        "claude"     => &["claude"],
        "codex"      => &["codex"],
        "copilot"    => &["copilot"],
        "gemini-cli" => &["gemini-cli"],
        "opencode"   => &["opencode"],
        _            => &[],
    }
}

/// Multi-select prompt: which agents should ccync project to?
/// Default: claude, codex, copilot checked; gemini-cli, opencode, agy unchecked.
/// Falls back to the three default-checked runtimes on non-TTY.
fn prompt_runtimes(master: &str) -> Vec<String> {
    if !std::io::stdin().is_terminal() {
        // Non-interactive: use defaults, expanding agy if it would be checked.
        return RUNTIME_ENTRIES
            .iter()
            .filter(|(_, _, default)| *default)
            .flat_map(|(key, _, _)| expand_runtime_key(key).iter().copied())
            .map(str::to_string)
            .collect();
    }

    let labels: Vec<&str> = RUNTIME_ENTRIES.iter().map(|(_, l, _)| *l).collect();
    let defaults: Vec<bool> = RUNTIME_ENTRIES.iter().map(|(_, _, d)| *d).collect();

    let result = dialoguer::MultiSelect::new()
        .with_prompt("Which agents should ccync project to? (Space to toggle, Enter to confirm)")
        .items(&labels)
        .defaults(&defaults)
        .interact_opt();

    match result {
        Ok(Some(indices)) => runtimes_from_indices(&indices, master),
        // Cancelled (Esc / Ctrl-C) or error → safe fallback: master agent only.
        _ => vec![master.to_string()],
    }
}

/// Map selected `RUNTIME_ENTRIES` indices to canonical runtime keys, expanding
/// the synthetic `agy` entry. An empty selection falls back to the master only,
/// so an explicit "uncheck everything" never writes a surprising runtime set.
fn runtimes_from_indices(indices: &[usize], master: &str) -> Vec<String> {
    if indices.is_empty() {
        return vec![master.to_string()];
    }
    indices
        .iter()
        .flat_map(|&i| expand_runtime_key(RUNTIME_ENTRIES[i].0).iter().copied())
        .map(str::to_string)
        .collect()
}

/// Read the master agent's state, adopt non-ccync-managed items into the lock
/// file, and record the master under `_adoptMaster`. Prints a summary.
fn run_import_adopt(master: &str) -> Result<(), ExitCode> {
    use ccync_engine::adopt::{
        adopt_items, adopt_mcp_definitions, read_claude_state, read_codex_state,
        read_mcp_definitions, AgentKind,
    };
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

    // Snapshot the FULL MCP server definitions (command/args/env/url/headers)
    // into `_mcpServers` — `adopt_items` only records item *names*, which leaves
    // MCP projection with nothing to write (empty `managed.json`, no reprojection
    // to secondary agents). Reaching here means `master` is claude or codex.
    let master_kind = if master == "claude" {
        AgentKind::Claude
    } else {
        AgentKind::Codex
    };
    adopt_mcp_definitions(read_mcp_definitions(&master_kind), &lock_path).map_err(|e| {
        eprintln!("ccync init: could not snapshot MCP definitions: {e}");
        ExitCode::Error
    })?;

    let agent_label = match master {
        "claude" => "Claude Code",
        "codex" => "Codex",
        other => other,
    };
    println!("Imported {} plugin(s) from {}.", newly_adopted.len(), agent_label);
    // Print names in a compact wrapped list (≤80 chars per line).
    if !newly_adopted.is_empty() {
        let names: Vec<&str> = newly_adopted.iter().map(|i| i.name.as_str()).collect();
        let mut line = String::from("  ");
        for (i, name) in names.iter().enumerate() {
            let sep = if i == 0 { "" } else { ", " };
            if line.len() + sep.len() + name.len() > 80 {
                println!("{}", line.trim_end_matches(", "));
                line = format!("  {}", name);
            } else {
                line.push_str(sep);
                line.push_str(name);
            }
        }
        println!("{}", line);
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

/// Write the user-chosen runtime selection into `config.json`. The master
/// agent is used as the primary runtime.
fn write_runtime_selection_to_config(master: &str, selected: &[String]) -> Result<(), String> {
    let cfg_path = ccync_foundation::paths::machine_config_path()
        .ok_or("cannot resolve config.json path")?;
    write_runtime_selection_at(&cfg_path, master, selected)
}

/// Path-explicit core of [`write_runtime_selection_to_config`]: load the config
/// at `cfg_path` and update only `selectedRuntimes` + `primaryRuntime`.
///
/// The merge runs on the raw JSON map so unrelated `config.json` keys (e.g.
/// `secrets`, `devMode`, `ccyncRoot`) survive verbatim — round-tripping through
/// the typed `CcyncConfig` struct would silently drop fields it does not model
/// (notably `secrets`, which `mcp::resolver` reads straight from the raw JSON).
/// This mirrors `ccync_engine::migrate::fold_install_state_into_config`. Split
/// out so the field-preservation contract is unit-testable without redirecting
/// the process home.
fn write_runtime_selection_at(cfg_path: &Path, master: &str, selected: &[String]) -> Result<(), String> {
    if let Some(parent) = cfg_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    // Load the existing config as a raw JSON object so every key we do not
    // explicitly own is preserved on write-back.
    let mut config: serde_json::Map<String, Value> = match std::fs::read_to_string(cfg_path) {
        Ok(text) => serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default(),
        Err(_) => serde_json::Map::new(),
    };

    config.insert(
        "selectedRuntimes".to_string(),
        Value::Array(selected.iter().map(|r| Value::String(r.clone())).collect()),
    );
    config.insert(
        "primaryRuntime".to_string(),
        Value::String(master.to_string()),
    );

    let text = serde_json::to_string_pretty(&Value::Object(config)).map_err(|e| e.to_string())?;
    std::fs::write(cfg_path, text).map_err(|e| e.to_string())
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
        let lock = dir.path().join("build").join("lock.json");
        record_master_in_lock(&lock, "codex").unwrap();
        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
        assert_eq!(content["_adoptMaster"], "codex");
    }

    // ── runtime selection model ───────────────────────────────────────────────

    #[test]
    fn runtime_entries_default_to_claude_codex_copilot() {
        let checked: Vec<&str> = RUNTIME_ENTRIES
            .iter()
            .filter(|(_, _, default)| *default)
            .map(|(key, _, _)| *key)
            .collect();
        assert_eq!(checked, vec!["claude", "codex", "copilot"]);

        // opencode, agy, gemini-cli must be unchecked by default.
        let unchecked: Vec<&str> = RUNTIME_ENTRIES
            .iter()
            .filter(|(_, _, default)| !*default)
            .map(|(key, _, _)| *key)
            .collect();
        assert_eq!(unchecked, vec!["opencode", "agy", "gemini-cli"]);
    }

    #[test]
    fn expand_runtime_key_agy_expands_to_three_surfaces() {
        assert_eq!(
            expand_runtime_key("agy"),
            &["agy-cli", "agy-ide", "agy-gui"]
        );
    }

    #[test]
    fn expand_runtime_key_canonical_keys_map_to_themselves() {
        for key in ["claude", "codex", "copilot", "opencode", "gemini-cli"] {
            assert_eq!(expand_runtime_key(key), &[key]);
        }
        // Unknown keys expand to nothing (never panic, never write junk).
        assert!(expand_runtime_key("frobnicate").is_empty());
    }

    #[test]
    fn runtimes_from_indices_expands_agy_and_falls_back_when_empty() {
        // Empty selection → master-only fallback.
        assert_eq!(runtimes_from_indices(&[], "claude"), vec!["claude"]);

        // agy index (4) expands to all three Antigravity surfaces.
        let agy_idx = RUNTIME_ENTRIES.iter().position(|(k, _, _)| *k == "agy").unwrap();
        assert_eq!(
            runtimes_from_indices(&[agy_idx], "claude"),
            vec!["agy-cli", "agy-ide", "agy-gui"]
        );

        // Mixed selection preserves order and expands only agy.
        let claude_idx = RUNTIME_ENTRIES.iter().position(|(k, _, _)| *k == "claude").unwrap();
        assert_eq!(
            runtimes_from_indices(&[claude_idx, agy_idx], "codex"),
            vec!["claude", "agy-cli", "agy-ide", "agy-gui"]
        );
    }

    #[test]
    fn prompt_runtimes_non_tty_returns_default_checked_set() {
        // In cargo test stdin is not a TTY, so prompt_runtimes takes the
        // non-interactive branch and returns the default-checked runtimes.
        assert_eq!(
            prompt_runtimes("claude"),
            vec!["claude", "codex", "copilot"]
        );
    }

    // ── config-field preservation ─────────────────────────────────────────────

    #[test]
    fn write_runtime_selection_preserves_unmodeled_config_fields() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.json");
        // Existing config carries devMode, ccyncRoot, and a secrets block —
        // none of which CcyncConfig models on serialize. They must survive.
        std::fs::write(
            &cfg,
            r#"{"devMode":true,"ccyncRoot":"/repo","secrets":{"GITHUB_TOKEN":"ghp_xxx"}}"#,
        )
        .unwrap();

        write_runtime_selection_at(&cfg, "claude", &["claude".into(), "codex".into()]).unwrap();

        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        // Runtime fields updated.
        assert_eq!(content["selectedRuntimes"], serde_json::json!(["claude", "codex"]));
        assert_eq!(content["primaryRuntime"], "claude");
        // Unmodeled fields preserved verbatim — regression guard against the
        // lossy typed round-trip that would wipe a user's secrets on init.
        assert_eq!(content["devMode"], true);
        assert_eq!(content["ccyncRoot"], "/repo");
        assert_eq!(content["secrets"]["GITHUB_TOKEN"], "ghp_xxx");
    }

    #[test]
    fn write_runtime_selection_creates_config_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("nested").join("config.json");
        write_runtime_selection_at(&cfg, "codex", &["codex".into()]).unwrap();
        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(content["primaryRuntime"], "codex");
        assert_eq!(content["selectedRuntimes"], serde_json::json!(["codex"]));
    }
}
