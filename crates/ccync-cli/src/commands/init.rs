//! `ccync init [<claude|codex>]` — pick the master agent, adopt its content
//! into the ccync truth store, record `_adoptMaster`, write runtime selection
//! into `config.json`, then project the managed set to all agents via the unified engine.
//!
//! Master selection is owned here (moved off `ccync sync`, which used to carry
//! `--import-from`).

use ccync_engine::adopt::adopted_items_without_source;
use ccync_engine::ExitCode;
use ccync_foundation::target::AgentId;
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
                eprintln!("  Example: ccync init claude");
                return ExitCode::Usage;
            }
        },
    };

    // 1. Adopt the master's content into the ccync truth store + record master.
    let lock_path = ccync_foundation::paths::plugins_lock_path();
    if let Err(code) = run_init_with_reinit_warning(
        &master,
        lock_path.as_deref(),
        || {
            eprintln!(
                "warning: this machine already has an adopted master — re-running init may replace/overwrite the existing adoption state"
            );
        },
        run_import_adopt,
    ) {
        return code;
    }

    // 2. Let the user pick which agents to project to, then persist the selection.
    let selected = prompt_agents(&master);
    let selected_targets: Vec<ccync_foundation::target::ProjectionTargetId> =
        selected.iter().filter_map(|s| s.parse().ok()).collect();
    println!(
        "ccync init: will apply ccync-managed settings to {}",
        super::lifecycle::format_canonical_target_list(&selected_targets)
    );
    if let Err(e) = write_projection_selection_to_config(&selected_targets) {
        eprintln!("ccync init: could not write projection target selection (best-effort): {e}");
    }

    // 3. Project the managed set. init already showed the agent-selection UI,
    //    so skip the first-run gate (skip_first_run_gate=true).
    super::lifecycle::run_unified_projection(false, true, true, None)
}

/// Core of `cmd_init`'s re-init warning + adoption sequence, with `warn` and
/// `adopt` injected so the ordering (warning strictly before adoption
/// mutates the lock) is directly testable without capturing real stderr or
/// invoking the real `run_import_adopt`.
///
/// `lock_path: None` (home directory unresolvable) is treated the same as
/// "no existing state" — quiet, matching `run_import_adopt`'s own graceful
/// handling of that condition (it will independently fail with a clear
/// error before this warning would matter).
fn run_init_with_reinit_warning(
    master: &str,
    lock_path: Option<&Path>,
    warn: impl FnOnce(),
    adopt: impl FnOnce(&str) -> Result<(), ExitCode>,
) -> Result<(), ExitCode> {
    if lock_path.is_some_and(existing_adoption_state) {
        warn();
    }
    adopt(master)
}

/// True when `lock_path` already carries a recorded `_adoptMaster` — i.e.
/// this machine was already `ccync init`-ed at least once. A missing or
/// unparseable lockfile is treated as "no existing state" (first init),
/// never as an error.
fn existing_adoption_state(lock_path: &Path) -> bool {
    if !lock_path.is_file() {
        return false;
    }
    std::fs::read_to_string(lock_path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .is_some_and(|v| v.get("_adoptMaster").and_then(Value::as_str).is_some())
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

/// Display entries for the agent MultiSelect.
/// `AgentId::Agy` expands to all three Antigravity surfaces on selection —
/// see [`ccync_foundation::target::AgentId::expand`], the single authoritative
/// agent→target mapping (no local expansion table is kept here).
const AGENT_ENTRIES: &[(AgentId, &str, bool)] = &[
    // (agent, label, default_checked)
    (AgentId::Claude, "claude     (~/.claude.json)", true),
    (AgentId::Codex, "codex      (~/.codex/config.toml)", true),
    (
        AgentId::Copilot,
        "copilot    (~/.copilot/mcp-config.json)",
        true,
    ),
    (AgentId::Opencode, "opencode   (opencode.json)", false),
    (AgentId::Agy, "agy        (Antigravity cli/ide/gui)", false),
    (AgentId::GeminiCli, "gemini-cli (~/.gemini/)", false),
];

/// Multi-select prompt: which agents should ccync project to?
/// Default: claude, codex, copilot checked; gemini-cli, opencode, agy unchecked.
/// Falls back to the three default-checked agents (expanded to target strings)
/// on non-TTY.
fn prompt_agents(master: &str) -> Vec<String> {
    if !std::io::stdin().is_terminal() {
        // Non-interactive: use defaults, expanding agy via AgentId::expand().
        return AGENT_ENTRIES
            .iter()
            .filter(|(_, _, default)| *default)
            .flat_map(|(agent, _, _)| agent.expand().iter())
            .map(|target| target.as_str().to_string())
            .collect();
    }

    let labels: Vec<&str> = AGENT_ENTRIES.iter().map(|(_, l, _)| *l).collect();
    let defaults: Vec<bool> = AGENT_ENTRIES.iter().map(|(_, _, d)| *d).collect();

    let result = dialoguer::MultiSelect::new()
        .with_prompt("Which agents should receive ccync-managed settings? (Space to toggle, Enter to confirm)")
        .items(&labels)
        .defaults(&defaults)
        .interact_opt();

    match result {
        Ok(Some(indices)) => targets_from_indices(&indices, master),
        // Cancelled (Esc / Ctrl-C) or error → safe fallback: master agent only.
        // `master` is already a validated "claude"/"codex" string that matches
        // its identity-agent target string 1:1, so no AgentId round-trip is
        // needed here.
        _ => vec![master.to_string()],
    }
}

/// Map selected `AGENT_ENTRIES` indices to canonical projection-target key
/// strings, expanding each agent via `AgentId::expand()`. An empty selection
/// falls back to the master only, so an explicit "uncheck everything" never
/// writes a surprising target set.
fn targets_from_indices(indices: &[usize], master: &str) -> Vec<String> {
    if indices.is_empty() {
        return vec![master.to_string()];
    }
    indices
        .iter()
        .flat_map(|&i| AGENT_ENTRIES[i].0.expand().iter())
        .map(|target| target.as_str().to_string())
        .collect()
}

/// Read the master agent's state, adopt non-ccync-managed items into the lock
/// file, and record the master under `_adoptMaster`. Prints a summary.
fn run_import_adopt(master: &str) -> Result<(), ExitCode> {
    use ccync_engine::adopt::{
        adopt_items, adopt_loose_skills, adopt_mcp_definitions, read_claude_state,
        read_codex_state, read_mcp_definitions, AgentKind,
    };
    use ccync_foundation::paths::plugins_lock_path;
    use std::collections::HashSet;

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

    // Retain a copy for loose-skill marking below — `adopt_items` takes
    // ownership of `items`.
    let items_for_loose_skills = items.clone();

    // Pre-move discovery snapshot: does this master have anything besides
    // loose skills? A skills-only (or fully empty) master still "succeeds"
    // below (0 plugins/MCP is not an error), but the user should be told
    // there was nothing substantive to adopt rather than silently reporting
    // "Imported 0 plugin(s)." with no further signal.
    let has_plugin_or_mcp_content = has_plugin_or_mcp_content(&items_for_loose_skills);

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

    // Mark master skills that no plugin claims as "loose" so they still get
    // materialized into the canonical root. There is no mechanism yet to
    // enumerate which skills a given installed plugin bundles, so the
    // exclusion set is empty for now — every enumerable skill is loose until
    // that data exists.
    let loose_skills = adopt_loose_skills(&items_for_loose_skills, &HashSet::new(), &lock_path)
        .map_err(|e| {
            eprintln!("ccync init: could not mark loose skills: {e}");
            ExitCode::Error
        })?;

    let agent_label = match master {
        "claude" => "Claude Code",
        "codex" => "Codex",
        other => other,
    };
    println!(
        "Imported {} plugin(s) from {}.",
        newly_adopted.len(),
        agent_label
    );
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
    if !loose_skills.is_empty() {
        println!(
            "Marked {} loose skill(s) for sync: {}",
            loose_skills.len(),
            loose_skills
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !has_plugin_or_mcp_content {
        eprintln!(
            "warning: no plugin or MCP definition found for '{master}' — loose skills alone (if any) were adopted"
        );
    }
    print_repair_tip_if_unresolved(&lock_path);
    Ok(())
}

/// The static repair-tip line, or `None` when every adopted row already has
/// a source (or the lock-derived seam,
/// [`ccync_engine::adopt::adopted_items_without_source`], can't be
/// read — graceful-missing, never an error). Pure and testable; separated
/// from [`print_repair_tip_if_unresolved`] so the condition can be checked
/// without capturing stdout.
fn repair_tip_line(lock_path: &Path) -> Option<&'static str> {
    match adopted_items_without_source(lock_path) {
        Ok(unresolved) if !unresolved.is_empty() => {
            Some("Tip: run `ccync repair` to resolve missing adopted-item sources.")
        }
        _ => None,
    }
}

/// After the adoption summary, print one repair-tip line
/// ([`repair_tip_line`]) when any adopted row has no source — covers
/// plugin/MCP/skill uniformly, same as `upgrade`/`list --upgrade-available`'s
/// shared tip. Prints nothing when every adopted row is already
/// resolved. `init` itself never searches — this is static text only.
fn print_repair_tip_if_unresolved(lock_path: &Path) {
    if let Some(line) = repair_tip_line(lock_path) {
        println!("{line}");
    }
}

/// True when a pre-move discovery snapshot contains at least one plugin or
/// MCP server item — i.e. there is something for `run_import_adopt` to adopt
/// besides loose skills. Loose skills alone (or a fully empty snapshot) do
/// not count: skills have no plugin/MCP identity to import.
fn has_plugin_or_mcp_content(items: &[ccync_engine::adopt::InstalledItem]) -> bool {
    items.iter().any(|item| {
        matches!(
            item.kind,
            ccync_engine::adopt::ItemKind::Plugin | ccync_engine::adopt::ItemKind::McpServer
        )
    })
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

/// Write the user's exact chosen projection-target selection into
/// `config.json`, via the canonical raw-map writer
/// [`ccync_foundation::config::write_projection_selection_at`]. Only
/// `cmd_init`'s live write flow calls this — there is no dry-run or
/// read-only path through `init.rs` that reaches it.
fn write_projection_selection_to_config(
    targets: &[ccync_foundation::target::ProjectionTargetId],
) -> Result<(), String> {
    let cfg_path =
        ccync_foundation::paths::machine_config_path().ok_or("cannot resolve config.json path")?;
    // The foundation writer does not create its parent directory (it only
    // owns the file's own contents) — first-run `init` on a machine with no
    // `~/.ccync/` yet needs it created here, mirroring the prior writer's
    // own `create_dir_all` at this call site.
    if let Some(parent) = cfg_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    ccync_foundation::config::write_projection_selection_at(&cfg_path, targets)
        .map_err(|e| e.to_string())
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
    fn cmd_init_non_interactive_missing_master_is_usage() {
        // Test runners have no TTY stdin, so prompt_master() returns None,
        // exercising the same non-interactive missing-master path as a real
        // headless/CI invocation.
        assert_eq!(cmd_init(&["init".to_string()]), ExitCode::Usage);
    }

    #[test]
    fn source_non_interactive_missing_master_error_includes_example() {
        let this_file = include_str!("init.rs");
        assert!(
            this_file.contains("Example: ccync init claude"),
            "non-interactive missing-master error must include a literal `ccync init claude` example"
        );
    }

    // ── reworded write-error and prompt literals ──────────────────────────────

    #[test]
    fn write_error_and_prompt_literals_are_byte_exact_and_old_literals_are_gone() {
        let this_file = include_str!("init.rs");
        assert!(
            this_file.contains(
                "ccync init: could not write projection target selection (best-effort): {e}"
            ),
            "the reworded write-error literal must be present byte-exact"
        );
        assert!(
            this_file.contains(
                "Which agents should receive ccync-managed settings? (Space to toggle, Enter to confirm)"
            ),
            "the reworded prompt literal must be present byte-exact"
        );
        // Needles are assembled at runtime (split across literals) so this
        // test's own source text does not trip its own assertion via
        // include_str!, mirroring lifecycle.rs's removed_suppression_helper_never_reappears.
        let old_write_error = ["could not write ", "runtime selection (best-effort)"].concat();
        let old_prompt = ["Which agents should ", "ccync project to?"].concat();
        assert_eq!(
            this_file.matches(&old_write_error).count(),
            0,
            "the old write-error literal must be fully retired"
        );
        assert_eq!(
            this_file.matches(&old_prompt).count(),
            0,
            "the old prompt literal must be fully retired"
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

    // ── agent selection model ─────────────────────────────────────────────────

    #[test]
    fn agent_entries_has_six_choices_default_to_claude_codex_copilot() {
        assert_eq!(AGENT_ENTRIES.len(), 6, "six agent choices");

        let checked: Vec<AgentId> = AGENT_ENTRIES
            .iter()
            .filter(|(_, _, default)| *default)
            .map(|(agent, _, _)| *agent)
            .collect();
        assert_eq!(
            checked,
            vec![AgentId::Claude, AgentId::Codex, AgentId::Copilot]
        );

        // opencode, agy, gemini-cli must be unchecked by default.
        let unchecked: Vec<AgentId> = AGENT_ENTRIES
            .iter()
            .filter(|(_, _, default)| !*default)
            .map(|(agent, _, _)| *agent)
            .collect();
        assert_eq!(
            unchecked,
            vec![AgentId::Opencode, AgentId::Agy, AgentId::GeminiCli]
        );
    }

    #[test]
    fn agent_entries_agy_expands_to_three_surfaces_via_agent_id() {
        // Delegates straight to AgentId::Agy.expand() — no local expansion
        // table exists in this file.
        assert_eq!(
            AgentId::Agy
                .expand()
                .iter()
                .map(|t| t.as_str())
                .collect::<Vec<_>>(),
            vec!["agy-cli", "agy-ide", "agy-gui"]
        );
    }

    #[test]
    fn agent_entries_five_non_agy_choices_are_identity_families() {
        for agent in [
            AgentId::Claude,
            AgentId::Codex,
            AgentId::Copilot,
            AgentId::Opencode,
            AgentId::GeminiCli,
        ] {
            assert_eq!(
                agent
                    .expand()
                    .iter()
                    .map(|t| t.as_str())
                    .collect::<Vec<_>>(),
                vec![agent.as_str()],
                "{agent} must expand to exactly its own matching target"
            );
        }
    }

    #[test]
    fn targets_from_indices_expands_agy_and_falls_back_when_empty() {
        // Empty selection → master-only fallback.
        assert_eq!(targets_from_indices(&[], "claude"), vec!["claude"]);

        // agy index expands to all three Antigravity surfaces.
        let agy_idx = AGENT_ENTRIES
            .iter()
            .position(|(a, _, _)| *a == AgentId::Agy)
            .unwrap();
        assert_eq!(
            targets_from_indices(&[agy_idx], "claude"),
            vec!["agy-cli", "agy-ide", "agy-gui"]
        );

        // Mixed selection preserves order and expands only agy.
        let claude_idx = AGENT_ENTRIES
            .iter()
            .position(|(a, _, _)| *a == AgentId::Claude)
            .unwrap();
        assert_eq!(
            targets_from_indices(&[claude_idx, agy_idx], "codex"),
            vec!["claude", "agy-cli", "agy-ide", "agy-gui"]
        );
    }

    #[test]
    fn prompt_agents_non_tty_returns_default_checked_set() {
        // In cargo test stdin is not a TTY, so prompt_agents takes the
        // non-interactive branch and returns the default-checked agents.
        assert_eq!(prompt_agents("claude"), vec!["claude", "codex", "copilot"]);
    }

    // ── loose-skill marking (mirrors run_import_adopt's adopt sequence) ──────

    #[test]
    fn run_import_adopt_sequence_marks_loose_skills_in_lock() {
        use ccync_engine::adopt::{
            adopt_items, adopt_loose_skills, AgentKind, InstalledItem, ItemKind,
        };
        use std::collections::HashSet;

        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");

        let items = vec![InstalledItem {
            name: "doc-sync".to_string(),
            version: None,
            agent: AgentKind::Claude,
            kind: ItemKind::Skill,
            source: None,
            baseline_version: None,
        }];

        // Same sequence run_import_adopt follows: clone before adopt_items
        // consumes the Vec, then mark loose skills against an empty
        // plugin-owned set (no bundled-skill enumeration exists yet).
        let items_for_loose_skills = items.clone();
        adopt_items(items, &lock).unwrap();
        let loose = adopt_loose_skills(&items_for_loose_skills, &HashSet::new(), &lock).unwrap();

        assert_eq!(loose.len(), 1);
        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
        let arr = content["_looseSkills"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "doc-sync");
    }

    #[test]
    fn run_import_adopt_sequence_empty_plugin_owned_marks_all_skills_loose() {
        use ccync_engine::adopt::{adopt_loose_skills, AgentKind, InstalledItem, ItemKind};
        use std::collections::HashSet;

        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");

        // No installed_plugins / no plugin-owned skill data available at all —
        // every skill item enumerable from master state must be treated as
        // loose, per the empty-owned-set contract.
        let items = vec![
            InstalledItem {
                name: "skill-one".to_string(),
                version: None,
                agent: AgentKind::Claude,
                kind: ItemKind::Skill,
                source: None,
                baseline_version: None,
            },
            InstalledItem {
                name: "skill-two".to_string(),
                version: None,
                agent: AgentKind::Codex,
                kind: ItemKind::Skill,
                source: None,
                baseline_version: None,
            },
        ];

        let loose = adopt_loose_skills(&items, &HashSet::new(), &lock).unwrap();
        assert_eq!(
            loose.len(),
            2,
            "empty plugin_owned treats all skills as loose"
        );
        let names: Vec<&str> = loose.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"skill-one"));
        assert!(names.contains(&"skill-two"));
    }

    // ── warn when a master has no plugin/MCP content ──────────────────────────

    #[test]
    fn init_empty_content_warning_true_for_fully_empty_master() {
        use ccync_engine::adopt::InstalledItem;
        assert!(
            !has_plugin_or_mcp_content(&Vec::<InstalledItem>::new()),
            "an empty discovery must trigger the empty-content warning"
        );
    }

    #[test]
    fn init_empty_content_warning_true_for_skills_only_master() {
        use ccync_engine::adopt::{AgentKind, InstalledItem, ItemKind};
        let items = vec![
            InstalledItem {
                name: "skill-one".to_string(),
                version: None,
                agent: AgentKind::Claude,
                kind: ItemKind::Skill,
                source: None,
                baseline_version: None,
            },
            InstalledItem {
                name: "skill-two".to_string(),
                version: None,
                agent: AgentKind::Claude,
                kind: ItemKind::Skill,
                source: None,
                baseline_version: None,
            },
        ];
        assert!(
            !has_plugin_or_mcp_content(&items),
            "skills-only discovery must NOT suppress the empty-content warning"
        );
    }

    #[test]
    fn init_empty_content_warning_false_when_plugin_present() {
        use ccync_engine::adopt::{AgentKind, InstalledItem, ItemKind};
        let items = vec![
            InstalledItem {
                name: "some-plugin".to_string(),
                version: Some("1.0.0".to_string()),
                agent: AgentKind::Claude,
                kind: ItemKind::Plugin,
                source: None,
                baseline_version: None,
            },
            InstalledItem {
                name: "some-skill".to_string(),
                version: None,
                agent: AgentKind::Claude,
                kind: ItemKind::Skill,
                source: None,
                baseline_version: None,
            },
        ];
        assert!(
            has_plugin_or_mcp_content(&items),
            "a discovered plugin must suppress the empty-content warning"
        );
    }

    #[test]
    fn init_empty_content_warning_false_when_mcp_server_present() {
        use ccync_engine::adopt::{AgentKind, InstalledItem, ItemKind};
        let items = vec![InstalledItem {
            name: "memory".to_string(),
            version: None,
            agent: AgentKind::Codex,
            kind: ItemKind::McpServer,
            source: None,
            baseline_version: None,
        }];
        assert!(
            has_plugin_or_mcp_content(&items),
            "a discovered MCP server must suppress the empty-content warning"
        );
    }

    // ── warn before re-init adoption ──────────────────────────────────────────

    #[test]
    fn existing_adoption_state_false_when_lockfile_absent() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        assert!(
            !existing_adoption_state(&lock),
            "a missing lockfile is a first init, not existing state"
        );
    }

    #[test]
    fn existing_adoption_state_false_when_no_adopt_master_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        std::fs::write(&lock, r#"{"_personalPlugins":[]}"#).unwrap();
        assert!(!existing_adoption_state(&lock));
    }

    #[test]
    fn existing_adoption_state_true_when_adopt_master_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        std::fs::write(&lock, r#"{"_adoptMaster":"claude"}"#).unwrap();
        assert!(existing_adoption_state(&lock));
    }

    #[test]
    fn reinit_warning_order_warns_before_adoption_when_state_exists() {
        // Probe requirement: warning fires strictly before the (injected)
        // adoption call — proven by trace order, not by capturing real
        // stderr/invoking the real run_import_adopt.
        use std::cell::RefCell;
        use std::rc::Rc;

        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        std::fs::write(&lock, r#"{"_adoptMaster":"claude"}"#).unwrap();

        let trace: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));
        let warn_trace = Rc::clone(&trace);
        let adopt_trace = Rc::clone(&trace);

        let result = run_init_with_reinit_warning(
            "claude",
            Some(lock.as_path()),
            || warn_trace.borrow_mut().push("warn"),
            |_| {
                adopt_trace.borrow_mut().push("adopt");
                Ok(())
            },
        );

        assert!(result.is_ok());
        assert_eq!(
            *trace.borrow(),
            vec!["warn", "adopt"],
            "the warning must fire before the adoption call, not after or instead of it"
        );
    }

    #[test]
    fn reinit_warning_order_no_warning_on_empty_home() {
        // First init: no existing lockfile at all → no warning, adoption
        // still runs (the command proceeds normally).
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json"); // never created

        use std::cell::RefCell;
        use std::rc::Rc;
        let trace: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));
        let warn_trace = Rc::clone(&trace);
        let adopt_trace = Rc::clone(&trace);

        let result = run_init_with_reinit_warning(
            "claude",
            Some(lock.as_path()),
            || warn_trace.borrow_mut().push("warn"),
            |_| {
                adopt_trace.borrow_mut().push("adopt");
                Ok(())
            },
        );

        assert!(result.is_ok());
        assert_eq!(
            *trace.borrow(),
            vec!["adopt"],
            "an empty home (first init) must not print the re-init warning"
        );
    }

    #[test]
    fn reinit_warning_order_command_proceeds_even_when_warned() {
        // The warning is advisory, not blocking — adopt still runs and its
        // Err propagates normally when it fails.
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("plugins.lock.json");
        std::fs::write(&lock, r#"{"_adoptMaster":"codex"}"#).unwrap();

        let result = run_init_with_reinit_warning(
            "codex",
            Some(lock.as_path()),
            || {},
            |_| Err(ExitCode::Error),
        );
        assert_eq!(result, Err(ExitCode::Error));
    }

    /// End-to-end coverage of `run_import_adopt` itself (not just the
    /// logic-mirroring sequence above): fakes a `~/.claude` master state with
    /// a plugin, an MCP server, and a loose skill, then asserts the real
    /// function wires `adopt_items` + `adopt_loose_skills` together correctly
    /// against a single lock file — closing the "real function vs. mirrored
    /// sequence" gap.
    #[test]
    fn run_import_adopt_end_to_end_writes_adopted_items_and_loose_skills() {
        // Shared crate-wide guard (see main.rs::test_io_lock) — required
        // because several other test modules in this crate also redirect
        // HOME/USERPROFILE and cargo test runs them concurrently.
        let _env = crate::test_io_lock();
        let home = tempfile::tempdir().unwrap();

        let orig_home;
        #[cfg(windows)]
        unsafe {
            orig_home = std::env::var("USERPROFILE").ok();
            std::env::set_var("USERPROFILE", home.path());
            std::env::set_var("HOME", home.path());
        }
        #[cfg(not(windows))]
        unsafe {
            orig_home = std::env::var("HOME").ok();
            std::env::set_var("HOME", home.path());
        }

        // Installed plugin state (v2 schema).
        let plugins_dir = home.path().join(".claude").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("installed_plugins.json"),
            r#"{"version":2,"plugins":{"widget@mkt":[{"version":"1.0.0"}]}}"#,
        )
        .unwrap();

        // Top-level ~/.claude.json with one MCP server.
        std::fs::write(
            home.path().join(".claude.json"),
            r#"{"mcpServers":{"memory":{"type":"stdio","command":"npx"}}}"#,
        )
        .unwrap();

        // One loose skill directory (no owning plugin data exists anywhere).
        let skill_dir = home.path().join(".claude").join("skills").join("doc-sync");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "# doc-sync\n").unwrap();

        let result = run_import_adopt("claude");

        let lock_path = home.path().join(".ccync").join("build").join("lock.json");
        let lock_text = std::fs::read_to_string(&lock_path);

        // Restore HOME/USERPROFILE before any assertion so a failed assertion
        // (which unwinds past this point) can't leave the process env pointed
        // at a temp dir that's about to be dropped.
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

        assert!(
            result.is_ok(),
            "run_import_adopt should succeed: {result:?}"
        );

        let content: Value = serde_json::from_str(&lock_text.unwrap()).unwrap();

        // adopt_items recorded the plugin, MCP server, AND the skill (adopt_items
        // has no kind filter — it adopts every InstalledItem it is given).
        let adopted = content["_adoptedItems"].as_array().unwrap();
        let adopted_names: Vec<&str> = adopted
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert!(adopted_names.contains(&"widget@mkt"));
        assert!(adopted_names.contains(&"memory"));
        assert!(adopted_names.contains(&"doc-sync"));

        // adopt_loose_skills separately marked the skill loose for canonical-root
        // materialization (empty plugin-owned set — nothing enumerable yet).
        let loose = content["_looseSkills"].as_array().unwrap();
        assert_eq!(loose.len(), 1);
        assert_eq!(loose[0]["name"], "doc-sync");

        // _adoptMaster recorded.
        assert_eq!(content["_adoptMaster"], "claude");
    }

    // ── canonical projection-target write wiring ──────────────────────────────
    //
    // Raw-map preservation, legacy-key removal, and byte-level write behavior
    // are owned and tested by `write_projection_selection_at` itself in
    // `ccync-foundation::config`. These tests cover only this crate's wiring:
    // `write_projection_selection_to_config` resolves the machine config path
    // and delegates the exact target list through unchanged.

    #[test]
    fn write_projection_selection_to_config_writes_exact_chosen_targets() {
        let _env = crate::test_io_lock();
        let home = tempfile::tempdir().unwrap();

        let orig_home;
        #[cfg(windows)]
        unsafe {
            orig_home = std::env::var("USERPROFILE").ok();
            std::env::set_var("USERPROFILE", home.path());
            std::env::set_var("HOME", home.path());
        }
        #[cfg(not(windows))]
        unsafe {
            orig_home = std::env::var("HOME").ok();
            std::env::set_var("HOME", home.path());
        }

        let targets = [
            ccync_foundation::target::ProjectionTargetId::Codex,
            ccync_foundation::target::ProjectionTargetId::Claude,
        ];
        let result = write_projection_selection_to_config(&targets);

        let cfg_path = ccync_foundation::paths::machine_config_path().unwrap();
        let content_text = std::fs::read_to_string(&cfg_path);

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

        assert!(result.is_ok(), "write must succeed: {result:?}");
        let content: Value = serde_json::from_str(&content_text.unwrap()).unwrap();
        // Exact chosen order preserved, matching the user's selection — this
        // is the wiring this test exists to prove (the raw-map preservation
        // and legacy-removal behavior itself is `write_projection_selection_at`'s
        // own contract, tested in `ccync-foundation::config`).
        assert_eq!(
            content["selectedProjectionTargets"],
            serde_json::json!(["codex", "claude"])
        );
        assert!(!content
            .as_object()
            .unwrap()
            .contains_key("selectedRuntimes"));
    }

    // ── repair tip ───────────────────────────────────────────────────────────

    #[test]
    fn repair_tip_line_present_when_unresolved_adopted_row_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join("lock.json");
        std::fs::write(
            &lock_path,
            serde_json::json!({
                "_adoptedItems": [
                    { "name": "memory", "sourceId": "claude-mcp" }
                ]
            })
            .to_string(),
        )
        .unwrap();
        assert!(repair_tip_line(&lock_path).is_some());
    }

    #[test]
    fn repair_tip_line_absent_when_all_adopted_rows_resolved() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join("lock.json");
        std::fs::write(
            &lock_path,
            serde_json::json!({
                "_adoptedItems": [
                    { "name": "memory", "sourceId": "claude-mcp", "source": "https://github.com/example/memory.git" }
                ]
            })
            .to_string(),
        )
        .unwrap();
        assert!(repair_tip_line(&lock_path).is_none());
    }

    #[test]
    fn repair_tip_line_absent_when_no_adopted_items_at_all() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join("lock.json");
        std::fs::write(&lock_path, serde_json::json!({}).to_string()).unwrap();
        assert!(repair_tip_line(&lock_path).is_none());
    }

    #[test]
    fn repair_tip_line_absent_when_lock_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join("does-not-exist.json");
        assert!(repair_tip_line(&lock_path).is_none());
    }
}
