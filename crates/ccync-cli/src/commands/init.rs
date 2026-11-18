//! `ccync init [<claude|codex|copilot|opencode|agy>] [--agents <list>] [--yes]`
//! — choose one source agent, scan it without writing, show a preview, then
//! take consent before any write.
//!
//! The write order after consent is fixed: the `_adoption` record with the
//! target snapshot into `plugins.json` first, then the adopted lock rows (items,
//! MCP definitions, loose skills, source), then the target selection into
//! `config.json` last, and finally the unified projection. A failure before the
//! `config.json` write leaves the machine uninitialized or on its previous
//! selection; `sync` then refuses on the snapshot mismatch until init is
//! repeated.

use ccync_engine::adopt::{
    adopted_items_without_source, read_adoption_records, scan_source, scan_source_outcome,
    store_adoption_records, AdoptionRecord, ItemKind, SourceScan, SourceScanOutcome, SourceState,
};
use ccync_engine::delivery::{
    prepare_delivery, ComponentBodies, DeliveryComponent, DeliveryInventory, UnconvertibleCommands,
    SHARED_ROOT_CONSUMERS,
};
use ccync_engine::truth::LooseSkill;
use ccync_engine::ExitCode;
use ccync_foundation::config::LegacyState;
use ccync_foundation::provider::{ComponentIdentity, ComponentKind, ProviderSourceKey};
use ccync_foundation::target::{AgentId, ProjectionTargetId, CANONICAL_PROJECTION_TARGETS};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

const INIT_USAGE: &str =
    "Usage: ccync init [claude|codex|copilot|opencode|agy] [--agents <list>] [--yes]";
const NOTHING_WRITTEN: &str = "nothing written";
const MCP_HOST_LINE: &str = "MCP hosts are written independently of agent selection and are not changed by this screen; each host line below is its own effect.";

/// The interactive questions and the source scan that `init` runs before it
/// writes anything. The terminal implementation is the default. Tests answer
/// the same questions in code, including cancel, so each refusal path runs
/// without a terminal.
pub(crate) trait InitPrompts {
    /// The source agent to import from, or `None` when the user cancels.
    fn source(&mut self) -> Option<AgentId>;
    /// The targets to receive ccync-managed settings, or `None` when the user cancels.
    fn targets(&mut self, saved: &[ProjectionTargetId]) -> Option<Vec<ProjectionTargetId>>;
    /// Scan the source without writing to it, including after consent.
    fn scan(&mut self, source: AgentId) -> SourceScanOutcome;
    /// Consent after the preview. `false` refuses, and nothing is written.
    fn consent(&mut self, assume_yes: bool, env_value: Option<&str>, interactive: bool) -> bool;
}

/// Reads answers from the terminal. The prompt helpers return `None` when stdin
/// is not a terminal, so a non-interactive run takes the refusal path.
struct TerminalPrompts;

impl InitPrompts for TerminalPrompts {
    fn source(&mut self) -> Option<AgentId> {
        prompt_source()
    }

    fn targets(&mut self, saved: &[ProjectionTargetId]) -> Option<Vec<ProjectionTargetId>> {
        prompt_targets(saved)
    }

    fn scan(&mut self, source: AgentId) -> SourceScanOutcome {
        scan_source_outcome(source)
    }

    fn consent(&mut self, assume_yes: bool, env_value: Option<&str>, interactive: bool) -> bool {
        let stdin = std::io::stdin();
        let mut input = stdin.lock();
        super::lifecycle::confirm_overwrite_with(assume_yes, env_value, interactive, &mut input)
    }
}

/// Steps 1 through 6: choose the source and targets, scan, print the preview,
/// take consent, then write. Every refusal returns `Err` before the first write.
fn choose_scan_and_adopt(
    args: &[String],
    interactive: bool,
    prompts: &mut dyn InitPrompts,
) -> Result<Vec<ProjectionTargetId>, ExitCode> {
    let layout = ccync_foundation::paths::EffectiveLayout::load_or_legacy().map_err(|error| {
        eprintln!("ccync init: cannot load effective storage layout: {error}");
        ExitCode::Error
    })?;
    choose_scan_and_adopt_at(args, interactive, prompts, &layout)
}

fn choose_scan_and_adopt_at(
    args: &[String],
    interactive: bool,
    prompts: &mut dyn InitPrompts,
    layout: &ccync_foundation::paths::EffectiveLayout,
) -> Result<Vec<ProjectionTargetId>, ExitCode> {
    super::args::reject_unknown_flags("init", args, &["--yes"], &["--agents"], 1, INIT_USAGE)?;
    let parsed = parse_init_args(args);

    // 1. Choose the one source agent to import from.
    let source = match parsed.source.as_deref() {
        Some(raw) => match raw.to_ascii_lowercase().parse::<AgentId>() {
            Ok(agent) => agent,
            Err(_) => {
                eprintln!(
                    "ccync init: unknown source '{raw}'; expected claude, codex, copilot, opencode, or agy"
                );
                return Err(ExitCode::Usage);
            }
        },
        None if interactive => match prompts.source() {
            Some(agent) => agent,
            None => {
                eprintln!("ccync init: no source selected; {NOTHING_WRITTEN}");
                return Err(ExitCode::Usage);
            }
        },
        None => {
            eprintln!(
                "ccync init: missing source; expected claude, codex, copilot, opencode, or agy"
            );
            eprintln!("  Example: ccync init claude --agents claude,codex --yes");
            return Err(ExitCode::Usage);
        }
    };

    // 2. Choose the targets that receive ccync-managed settings.
    let saved = saved_targets(layout);
    let targets = match parsed.agents.as_deref() {
        Some(list) => match parse_target_list(list) {
            Ok(targets) if targets.is_empty() => {
                eprintln!(
                    "ccync init: missing target list; pass --agents with at least one target, for example --agents claude,codex"
                );
                return Err(ExitCode::Usage);
            }
            Ok(targets) => targets,
            Err(message) => {
                eprintln!("ccync init: {message}");
                return Err(ExitCode::Usage);
            }
        },
        None if interactive => match prompts.targets(&saved) {
            Some(targets) if !targets.is_empty() => targets,
            _ => {
                eprintln!("ccync init: no target selected; {NOTHING_WRITTEN}");
                return Err(ExitCode::Usage);
            }
        },
        None => {
            eprintln!(
                "ccync init: missing target list; pass --agents with at least one target, for example --agents claude,codex"
            );
            return Err(ExitCode::Usage);
        }
    };

    // 3. Scan the source without writing to it, then print the full preview.
    let mut outcome = prompts.scan(source);
    outcome.scan.adoptable.items.retain(|item| {
        item.kind != ItemKind::McpServer
            || (!outcome
                .blocked_mcp
                .iter()
                .any(|server| server.name == item.name)
                && !outcome.disabled_mcp.contains(&item.name))
    });
    let scan = &outcome.scan;
    if matches!(
        scan.state,
        SourceState::Invalid | SourceState::Unreadable | SourceState::Unsupported
    ) {
        eprintln!(
            "ccync init: source {source} is {}: {}",
            state_label(&scan.state),
            scan.reason.as_deref().unwrap_or("no reason reported")
        );
        eprintln!("ccync init: {NOTHING_WRITTEN}");
        return Err(ExitCode::Error);
    }
    let digests = ccync_engine::adopt::preview_mcp_digests(source, scan);
    let existing = existing_records(layout);
    print_preview(source, &outcome, &targets, &existing, layout);

    // 4. Consent gate. Nothing above this line writes, and nothing below runs
    //    without it. An empty source needs the same consent: the preview has
    //    already printed the empty state.
    let env_value = std::env::var("CCYNC_ASSUME_YES").ok();
    if !prompts.consent(parsed.assume_yes, env_value.as_deref(), interactive) {
        eprintln!("ccync init: {NOTHING_WRITTEN}");
        return Err(ExitCode::Usage);
    }

    let mut confirmed = prompts.scan(source);
    confirmed.scan.adoptable.items.retain(|item| {
        item.kind != ItemKind::McpServer
            || (!confirmed
                .blocked_mcp
                .iter()
                .any(|server| server.name == item.name)
                && !confirmed.disabled_mcp.contains(&item.name))
    });
    if confirmed != outcome
        || ccync_engine::adopt::preview_mcp_digests(source, &confirmed.scan) != digests
    {
        eprintln!("ccync init: source changed after preview; run init again. {NOTHING_WRITTEN}");
        return Err(ExitCode::Error);
    }

    // 5. Ordered writes: `_adoption` with the target snapshot first, then the
    //    adopted lock rows, then config.json last. The lifecycle lock is taken
    //    here, not at dispatch, so a refusal above never creates ~/.ccync/build.
    let _lifecycle = match ccync_engine::tool_manager::lock::LifecycleLock::acquire() {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("ccync: {error}");
            return Err(ExitCode::Error);
        }
    };
    if let Err(error) = ccync_engine::tool_manager::lock::check_restore_pending() {
        eprintln!("ccync init: {error}");
        return Err(ExitCode::Error);
    }
    let locked_layout = match ccync_foundation::paths::EffectiveLayout::load_or_legacy() {
        Ok(layout) => layout,
        Err(error) => {
            eprintln!("ccync init: cannot load effective storage layout: {error}");
            return Err(ExitCode::Error);
        }
    };
    if locked_layout.authority()
        == ccync_foundation::paths::EffectiveLayoutAuthority::MutationBlocked
    {
        eprintln!("ccync init: effective storage layout is MutationBlocked; refusing to mutate");
        return Err(ExitCode::Error);
    }
    let mut committed_layout = None;
    if locked_layout.authority() != ccync_foundation::paths::EffectiveLayoutAuthority::Committed {
        if let Some(home) = ccync_foundation::paths::user_home() {
            if let Err(error) = super::lifecycle::run_layout_migration(&home) {
                eprintln!("ccync init: older sublayout fixup failed: {error}");
                return Err(ExitCode::Error);
            }
        }
        match locked_layout.authority() {
            ccync_foundation::paths::EffectiveLayoutAuthority::Fresh => {
                committed_layout = Some(
                    ccync_engine::migrate::commit_fresh_layout(&locked_layout).map_err(
                        |error| {
                            eprintln!("ccync init: layout commit failed: {error}");
                            ExitCode::Error
                        },
                    )?,
                );
            }
            ccync_foundation::paths::EffectiveLayoutAuthority::Legacy
                if locked_layout.root_error().is_none() =>
            {
                committed_layout = Some(
                    super::lifecycle::recover_stage_and_commit_legacy_layout(
                        &locked_layout,
                        &_lifecycle,
                    )
                    .map_err(|error| {
                        eprintln!("ccync init: {error}");
                        ExitCode::Error
                    })?,
                );
            }
            _ => {}
        }
    } else {
        match ccync_engine::migrate::detect_legacy_drift(&locked_layout) {
            Ok(drift) if drift.is_empty() => {}
            Ok(drift) => {
                eprintln!(
                    "ccync init: legacy storage drift blocks mutation: {}",
                    drift.join("; ")
                );
                return Err(ExitCode::Error);
            }
            Err(_)
                if matches!(
                    std::fs::metadata(locked_layout.migration_journal_path()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound
                ) => {}
            Err(error) => {
                eprintln!("ccync init: cannot verify committed storage layout: {error}");
                return Err(ExitCode::Error);
            }
        }
    }
    let effective_layout = committed_layout.as_ref().unwrap_or(&locked_layout);
    if let Err(message) = store_adoption(source, scan, &targets, effective_layout, &digests) {
        eprintln!("ccync init: could not write the adoption record to plugins.json: {message}");
        eprintln!(
            "ccync init: config.json was not written; repeat `ccync init {source}` to finish"
        );
        return Err(ExitCode::Error);
    }

    // 6. Adopt the source content into the lock, after the re-init warning.
    //    Every adoption write lands before config.json, so an interrupted init
    //    never leaves a selection that points at a half-written adoption.
    let lock_path = Some(effective_layout.build_lock_path());
    if let Err(code) = run_init_with_reinit_warning(
        source.as_str(),
        lock_path.as_deref(),
        || {
            eprintln!(
                "warning: this machine already has an adopted source; the adoption record above replaces the previous one"
            );
        },
        |master| run_import_adopt_at(master, &outcome, &effective_layout),
    ) {
        eprintln!(
            "ccync init: config.json was not written; repeat `ccync init {source}` to finish"
        );
        return Err(code);
    }

    // 7. config.json is the last init write.
    if let Err(message) = write_projection_selection_to_config_at(&targets, &effective_layout) {
        eprintln!("ccync init: could not write the target selection to config.json: {message}");
        eprintln!(
            "ccync init: plugins.json already records this selection; repeat `ccync init {source}` to finish"
        );
        return Err(ExitCode::Error);
    }

    Ok(targets)
}

pub(crate) fn cmd_init(args: &[String]) -> ExitCode {
    if let Err(code) =
        super::args::reject_unknown_flags("init", args, &["--yes"], &["--agents"], 1, INIT_USAGE)
    {
        return code;
    }
    let layout = match ccync_foundation::paths::EffectiveLayout::load_or_legacy() {
        Ok(layout) => layout,
        Err(error) => {
            eprintln!("ccync init: cannot load effective storage layout: {error}");
            return ExitCode::Error;
        }
    };
    let interactive = std::io::stdin().is_terminal();
    let targets = match choose_scan_and_adopt_at(args, interactive, &mut TerminalPrompts, &layout) {
        Ok(targets) => targets,
        Err(code) => return code,
    };

    // 8. Disclose the selection and user-scope files before projection. init
    //    passes skip_first_run_gate = true, so this output is its only disclosure.
    println!(
        "ccync init: will apply ccync-managed settings to {}",
        super::lifecycle::format_canonical_target_list(&targets)
    );
    let instruction_files = super::lifecycle::instruction_paths(&targets);
    let instruction_prune_files = super::lifecycle::legacy_instruction_prune_paths(&targets);
    println!("ccync init: user-scope instruction files to update:");
    if instruction_files.is_empty() {
        println!("  (none)");
    } else {
        for path in instruction_files {
            println!("  {}", path.display());
        }
    }
    println!("ccync init: legacy user-scope instruction files to prune:");
    if instruction_prune_files.is_empty() {
        println!("  (none)");
    } else {
        for path in instruction_prune_files {
            println!("  {}", path.display());
        }
    }

    // 9. Project the managed set. Consent was given above, so skip the gate.
    super::lifecycle::run_unified_projection(false, true, true, None)
}

/// Arguments after the verb, split into the source, the `--agents` value, and
/// the consent flag. A separated `--agents` value is consumed with its flag.
struct InitArgs {
    source: Option<String>,
    agents: Option<String>,
    assume_yes: bool,
}

fn parse_init_args(args: &[String]) -> InitArgs {
    let mut parsed = InitArgs {
        source: None,
        agents: None,
        assume_yes: false,
    };
    let mut i = 1;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--yes" {
            parsed.assume_yes = true;
        } else if arg == "--agents" {
            parsed.agents = Some(args.get(i + 1).cloned().unwrap_or_default());
            i += 1;
        } else if let Some(value) = arg.strip_prefix("--agents=") {
            parsed.agents = Some(value.to_string());
        } else if !arg.starts_with('-') {
            parsed.source = Some(arg.to_string());
        }
        i += 1;
    }
    parsed
}

/// Parse `--agents` into canonical projection targets. Accepts the five agent
/// names (`agy` expands to its three surfaces), `all`, and the seven canonical
/// target keys. Duplicates are dropped and the given order is kept. An empty
/// list parses to nothing, and the caller reports it as a missing target list.
fn parse_target_list(raw: &str) -> Result<Vec<ProjectionTargetId>, String> {
    let mut targets: Vec<ProjectionTargetId> = Vec::new();
    for token in raw
        .split(',')
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
    {
        let expanded: Vec<ProjectionTargetId> = if token == "all" {
            CANONICAL_PROJECTION_TARGETS.to_vec()
        } else if let Ok(agent) = token.parse::<AgentId>() {
            agent.expand().to_vec()
        } else if let Ok(target) = token.parse::<ProjectionTargetId>() {
            vec![target]
        } else {
            return Err(format!(
                "unknown target '{token}'; expected claude, codex, copilot, opencode, agy, agy-cli, agy-ide, agy-gui, or all"
            ));
        };
        for target in expanded {
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
    }
    Ok(targets)
}

/// The targets already in `config.json`, or none when the selection is absent.
fn saved_targets(layout: &ccync_foundation::paths::EffectiveLayout) -> Vec<ProjectionTargetId> {
    let resolution =
        ccync_foundation::config::CcyncConfig::load_from_path(&layout.machine_config_path())
            .resolve_projection_targets();
    if resolution.state == LegacyState::Absent {
        Vec::new()
    } else {
        resolution.targets.to_vec()
    }
}

/// Re-init warning + adoption sequence, with `warn` and `adopt` injected so the
/// ordering (warning strictly before adoption mutates the lock) is directly
/// testable without capturing real stderr or invoking the real
/// `run_import_adopt`.
///
/// `lock_path: None` (home directory unresolvable) is treated the same as
/// "no existing state" — quiet, matching `run_import_adopt`'s own graceful
/// handling of that condition (it will independently fail with a clear
/// error before this warning would matter).
fn run_init_with_reinit_warning(
    source: &str,
    lock_path: Option<&Path>,
    warn: impl FnOnce(),
    adopt: impl FnOnce(&str) -> Result<(), ExitCode>,
) -> Result<(), ExitCode> {
    if lock_path.is_some_and(existing_adoption_state) {
        warn();
    }
    adopt(source)
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

/// Interactive source pick via arrow-key Select. Returns None on non-interactive
/// stdin or if the user cancels (Esc / Ctrl-C).
fn prompt_source() -> Option<AgentId> {
    if !std::io::stdin().is_terminal() {
        return None;
    }
    let detected = detect_agent_from_env();
    let sources = ["claude", "codex", "copilot", "opencode", "agy"];
    let default_idx = sources
        .iter()
        .position(|&a| Some(a) == detected)
        .unwrap_or(0);

    let selection = dialoguer::Select::new()
        .with_prompt("Import plugins from which agent?")
        .items(&sources)
        .default(default_idx)
        .interact_opt()
        .ok()??;

    sources[selection].parse().ok()
}

/// Display entries for the agent MultiSelect.
/// `AgentId::Agy` expands to all three Antigravity surfaces on selection —
/// see [`ccync_foundation::target::AgentId::expand`], the single authoritative
/// agent→target mapping (no local expansion table is kept here). A fresh init
/// checks all five; a saved selection replaces these defaults.
const AGENT_ENTRIES: &[(AgentId, &str, bool)] = &[
    // (agent, label, default_checked)
    (AgentId::Claude, "claude     (~/.claude.json)", true),
    (AgentId::Codex, "codex      (~/.codex/config.toml)", true),
    (
        AgentId::Copilot,
        "copilot    (~/.copilot/mcp-config.json)",
        true,
    ),
    (AgentId::Opencode, "opencode   (opencode.json)", true),
    (AgentId::Agy, "agy        (Antigravity cli/ide/gui)", true),
];

/// Multi-select prompt: which targets should receive ccync-managed settings?
/// Pre-checks the saved selection when one exists; otherwise the default
/// checked agents. Returns None on non-TTY stdin or on cancel (Esc / Ctrl-C).
fn prompt_targets(saved: &[ProjectionTargetId]) -> Option<Vec<ProjectionTargetId>> {
    if !std::io::stdin().is_terminal() {
        return None;
    }
    let labels: Vec<&str> = AGENT_ENTRIES.iter().map(|(_, l, _)| *l).collect();
    let defaults: Vec<bool> = AGENT_ENTRIES
        .iter()
        .map(|(agent, _, default)| {
            if saved.is_empty() {
                *default
            } else {
                agent.expand().iter().any(|t| saved.contains(t))
            }
        })
        .collect();

    let indices = dialoguer::MultiSelect::new()
        .with_prompt("Which agents should receive ccync-managed settings? (Space to toggle, Enter to confirm)")
        .items(&labels)
        .defaults(&defaults)
        .interact_opt()
        .ok()??;

    Some(expand_selection(&indices, saved))
}

/// Map selected `AGENT_ENTRIES` indices to canonical targets. An AGY row keeps
/// a saved partial AGY subset (one or two surfaces); it expands to all three
/// surfaces only when no partial subset was saved.
fn expand_selection(indices: &[usize], saved: &[ProjectionTargetId]) -> Vec<ProjectionTargetId> {
    let mut targets: Vec<ProjectionTargetId> = Vec::new();
    for &index in indices {
        let (agent, _, _) = AGENT_ENTRIES[index];
        let mut expanded: Vec<ProjectionTargetId> = agent.expand().to_vec();
        if agent == AgentId::Agy {
            let kept: Vec<ProjectionTargetId> = expanded
                .iter()
                .copied()
                .filter(|t| saved.contains(t))
                .collect();
            if !kept.is_empty() && kept.len() < expanded.len() {
                expanded = kept;
            }
        }
        for target in expanded {
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
    }
    targets
}

/// Adoption records already in `plugins.json`. A missing or unreadable catalog
/// has no records to compare, so it yields an empty list.
fn existing_records(layout: &ccync_foundation::paths::EffectiveLayout) -> Vec<AdoptionRecord> {
    read_adoption_records(&layout.personal_catalog_path()).unwrap_or_default()
}

fn state_label(state: &SourceState) -> &'static str {
    match state {
        SourceState::Absent => "absent",
        SourceState::Empty => "empty",
        SourceState::Ready => "ready",
        SourceState::Invalid => "invalid",
        SourceState::Unreadable => "unreadable",
        SourceState::Unsupported => "unsupported",
    }
}

fn kind_label(kind: &ItemKind) -> &'static str {
    match kind {
        ItemKind::Plugin => "plugin",
        ItemKind::McpServer => "mcp-server",
        ItemKind::Skill => "skill",
    }
}

/// True when the agent has any config or skill root on disk. Used only for the
/// preview's runtime label; a root that is merely absent does not count.
fn agent_detected(agent: AgentId) -> bool {
    scan_source(agent)
        .origins
        .iter()
        .any(|origin| origin.state != SourceState::Absent)
}

/// True when the source's files are readable but its command is not on PATH.
/// Import still proceeds, and loader verification for the source stays `NotRun`.
fn source_is_offline(state: &SourceState, executable_found: bool) -> bool {
    matches!(state, SourceState::Ready | SourceState::Empty) && !executable_found
}

/// True when `name` resolves to a file on PATH. On Windows each PATHEXT suffix
/// is tried as well.
fn executable_on_path(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    let suffixes: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
            .split(';')
            .filter(|suffix| !suffix.is_empty())
            .map(str::to_ascii_lowercase)
            .collect()
    } else {
        vec![String::new()]
    };
    std::env::split_paths(&paths).any(|dir| {
        suffixes
            .iter()
            .any(|suffix| dir.join(format!("{name}{suffix}")).is_file())
    })
}

/// One preview line per shared root that a selected target reaches. The line
/// names every consumer of the root, so an unselected consumer shows before consent.
fn shared_root_lines(targets: &[ProjectionTargetId]) -> Vec<String> {
    SHARED_ROOT_CONSUMERS
        .iter()
        .filter(|row| row.consumers.iter().any(|c| targets.contains(c)))
        .map(|row| {
            let consumers: Vec<String> = row
                .consumers
                .iter()
                .map(|c| {
                    let state = if targets.contains(c) {
                        "selected"
                    } else {
                        "not selected"
                    };
                    format!("{} ({state})", c.as_str())
                })
                .collect();
            format!(
                "    shared root {}: reached by {}",
                row.root,
                consumers.join(", ")
            )
        })
        .collect()
}

/// Preview lines from the read-only preparation of the routes the scan would
/// publish. Nothing here reads a component body. Before consent, agent and
/// command content is not available, so each of their routes stays pending, and
/// the alias lines name the foreign path that caused each alias. Only skill
/// names are known before projection renders, so plugins and MCP servers
/// produce no route here.
///
/// The source agent's own target is not a destination here: its skills already
/// sit in that native root, so they are never foreign to it.
fn prepared_delivery_lines(
    scan: &SourceScan,
    source: AgentId,
    targets: &[ProjectionTargetId],
    home: &Path,
    lockfile: &Path,
) -> Vec<String> {
    let Ok(source_key) = ProviderSourceKey::new(source.as_str()) else {
        return Vec::new();
    };
    let targets: Vec<ProjectionTargetId> = targets
        .iter()
        .copied()
        .filter(|target| target.agent_family() != source)
        .collect();
    let identities: Vec<ComponentIdentity> = scan
        .adoptable
        .items
        .iter()
        .filter(|item| item.kind == ItemKind::Skill)
        .filter_map(|item| {
            ComponentIdentity::new(
                source_key.clone(),
                source.as_str(),
                None,
                ComponentKind::Skill,
                item.name.clone(),
            )
            .ok()
        })
        .collect();
    // Skills carry no generated references, so the flag is true for them. The
    // reference check for agent and command text runs in `prepare_delivery`,
    // which gets no bodies here and therefore reports those routes as pending.
    let inventory = DeliveryInventory {
        components: identities
            .iter()
            .map(|identity| DeliveryComponent {
                identity: identity.clone(),
                provider_hooks_declared: false,
                enabled: true,
                references_preserved: true,
            })
            .collect(),
        foreign_components: ccync_engine::delivery_apply::foreign_components(
            &identities,
            &targets,
            home,
            lockfile,
        ),
    };
    let prepared = prepare_delivery(
        &inventory,
        &targets,
        &[],
        &UnconvertibleCommands::new(),
        &ComponentBodies::new(),
    );

    let mut lines: Vec<String> = prepared
        .plan
        .routes
        .iter()
        .filter_map(|route| {
            let foreign = route.foreign_alias_path.as_deref()?;
            let alias = route.alias.as_deref().unwrap_or_default();
            Some(format!(
                "    {}: {} aliased to {alias} because {foreign} is not owned by ccync",
                route.target.as_str(),
                route.identity.component_id,
            ))
        })
        .collect();
    lines.extend(prepared.pending.iter().map(|pending| {
        format!(
            "    {}: {} reference validation pending: source content is not available before consent",
            pending.target.as_str(),
            pending.identity.component_id,
        )
    }));
    lines.extend(prepared.withheld.iter().map(|item| {
        let blocked = item
            .blocked_consumers
            .iter()
            .map(|target| target.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let consumers = item
            .consumers
            .iter()
            .map(|target| target.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "    {} withheld from {}: blocked consumers [{blocked}]; affected consumers [{consumers}]",
            item.pair.identity.component_id, item.pair.root
        )
    }));
    lines
}

/// Preview lines for loose skills that sync would not write as they are. Each
/// scanned skill's origin joins the origins that the lock already records under
/// the same name. Two distinct origins form a conflict group, which sync blocks
/// as a whole. A single legacy origin-less row needs a rescan first.
fn loose_skill_lines(scan: &SourceScan, lockfile: &Path) -> Vec<String> {
    let recorded: Vec<LooseSkill> = std::fs::read_to_string(lockfile)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.get("_looseSkills").cloned())
        .and_then(|rows| rows.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| serde_json::from_value(row).ok())
        .collect();

    let mut groups: BTreeMap<String, BTreeSet<Option<PathBuf>>> = BTreeMap::new();
    for item in scan
        .adoptable
        .items
        .iter()
        .filter(|item| item.kind == ItemKind::Skill)
    {
        if let Some(source) = &item.source {
            groups
                .entry(item.name.clone())
                .or_default()
                .insert(Some(PathBuf::from(source)));
        }
    }
    for skill in recorded {
        if let Some(origins) = groups.get_mut(&skill.name) {
            origins.insert(skill.origin);
        }
    }

    let mut lines = Vec::new();
    for (name, origins) in &groups {
        if origins.len() > 1 {
            let described: Vec<String> = origins
                .iter()
                .map(|origin| {
                    origin.as_ref().map_or_else(
                        || "no recorded origin".to_string(),
                        |path| path.display().to_string(),
                    )
                })
                .collect();
            lines.push(format!(
                "  loose conflict: {name} has {} origins: {}; sync blocks every member and writes none",
                origins.len(),
                described.join(", ")
            ));
        } else if origins.contains(&None) {
            lines.push(format!(
                "  rescan required: loose skill {name} has no recorded origin; sync skips it"
            ));
        }
    }
    lines
}

/// The MCP servers the consented scan blocked or disabled, as counts and
/// reasons. Preview and completion print these same lines. Only server names,
/// reasons, and config paths appear here. Definition values such as `env` and
/// `headers` never reach a line.
fn mcp_diagnostic_lines(outcome: &SourceScanOutcome) -> Vec<String> {
    let mut lines = vec![format!(
        "  mcp blocked: {} server(s), not imported; mcp disabled: {} server(s)",
        outcome.blocked_mcp.len(),
        outcome.disabled_mcp.len()
    )];
    for server in &outcome.blocked_mcp {
        let origin = server.origin.as_ref().map_or_else(
            || "no recorded origin".to_string(),
            |path| path.display().to_string(),
        );
        let disabled = if server.disabled {
            ", also disabled by the source"
        } else {
            ""
        };
        lines.push(format!(
            "    {}: {} ({origin}{disabled})",
            server.name, server.reason
        ));
    }
    if !outcome.disabled_mcp.is_empty() {
        lines.push(format!(
            "    disabled by the source: {}",
            outcome.disabled_mcp.join(", ")
        ));
    }
    lines
}

/// Print everything the user will consent to. Nothing here writes.
fn print_preview(
    source: AgentId,
    outcome: &SourceScanOutcome,
    targets: &[ProjectionTargetId],
    existing: &[AdoptionRecord],
    layout: &ccync_foundation::paths::EffectiveLayout,
) {
    let scan = &outcome.scan;
    let items = &scan.adoptable.items;
    println!("ccync init: preview for source {source} (nothing is written until you confirm)");
    if source_is_offline(&scan.state, executable_on_path(source.as_str())) {
        println!("  source state: offline");
        println!("    {source} is not on PATH; its files are readable, so import proceeds");
        println!("    loader verification: NotRun");
    } else {
        println!("  source state: {}", state_label(&scan.state));
    }
    for origin in &scan.origins {
        println!(
            "    {} ({})",
            origin.origin.display(),
            state_label(&origin.state)
        );
    }

    if items.is_empty() {
        println!("  nothing to import from {source}; confirming writes an empty adoption record");
    } else {
        let count = |kind: ItemKind| items.iter().filter(|i| i.kind == kind).count();
        println!(
            "  imports: {} plugin(s), {} MCP server(s), {} skill(s)",
            count(ItemKind::Plugin),
            count(ItemKind::McpServer),
            count(ItemKind::Skill)
        );
        for item in items {
            println!("    {}: {}", kind_label(&item.kind), item.name);
        }
    }

    for skipped in &scan.adoptable.skipped_owned {
        println!("  excluded (ccync-owned copy, not imported): {skipped}");
    }
    for item in &scan.unsupported {
        println!("  unsupported from {source} (not imported): {item}");
    }
    if !scan.unsupported.is_empty() {
        for target in targets {
            println!(
                "    {}: unsupported items from {source} are not written to it",
                target.as_str()
            );
        }
    }

    println!("  targets:");
    for target in targets {
        let label = if agent_detected(target.agent_family()) {
            "runtime: detected"
        } else {
            "runtime: not detected"
        };
        println!("    {} ({label})", target.as_str());
    }
    for line in shared_root_lines(targets) {
        println!("{line}");
    }
    if let Some(home) = ccync_foundation::paths::user_home() {
        let lockfile = layout.build_lock_path();
        for line in prepared_delivery_lines(scan, source, targets, &home, &lockfile) {
            println!("{line}");
        }
        for line in loose_skill_lines(scan, &lockfile) {
            println!("{line}");
        }
    }

    if !existing.is_empty() {
        println!("  existing adoption records:");
        for record in existing {
            let matched = items
                .iter()
                .any(|i| i.name == record.name && kind_label(&i.kind) == record.kind);
            // Confirming refuses a matched legacy row with no origin, so the
            // preview must name that before consent.
            if matched && record.origin.is_none() {
                println!(
                    "    rescan required: {} ({}) has no recorded origin; confirming stops before any write",
                    record.name, record.kind
                );
                continue;
            }
            let action = if matched { "update" } else { "keep" };
            println!("    {action}: {} ({})", record.name, record.kind);
        }
        for item in items {
            let recorded = existing
                .iter()
                .any(|r| r.name == item.name && r.kind == kind_label(&item.kind));
            if !recorded {
                println!("    new: {} ({})", item.name, kind_label(&item.kind));
            }
        }
    }

    let names: Vec<&str> = targets.iter().map(|t| t.as_str()).collect();
    println!(
        "  after sync, each selected agent will need a restart to load the changes: {}",
        names.join(", ")
    );

    let mcp_count = items
        .iter()
        .filter(|i| i.kind == ItemKind::McpServer)
        .count();
    println!("  {MCP_HOST_LINE}");
    for line in mcp_host_lines(source, mcp_count, targets) {
        println!("{line}");
    }
    for line in mcp_diagnostic_lines(outcome) {
        println!("{line}");
    }
}

/// One preview line per MCP host. MCP projection writes every host whether or
/// not its agent is a selected target, so each line states that write; the
/// agent's selection is shown only as context.
fn mcp_host_lines(
    source: AgentId,
    mcp_count: usize,
    targets: &[ProjectionTargetId],
) -> Vec<String> {
    AGENT_ENTRIES
        .iter()
        .map(|(host, _, _)| {
            let selection = if targets.iter().any(|t| t.agent_family() == *host) {
                "agent selected"
            } else {
                "agent not selected"
            };
            format!(
                "    {host}: MCP config receives the managed MCP set, including {mcp_count} server(s) imported from {source}; written unless already identical ({selection})"
            )
        })
        .collect()
}

/// Write the `_adoption` record and the target snapshot into `plugins.json`.
/// Absent records are never removed: the store merges by source and identity.
fn store_adoption(
    source: AgentId,
    scan: &SourceScan,
    targets: &[ProjectionTargetId],
    layout: &ccync_foundation::paths::EffectiveLayout,
    digests: &BTreeMap<String, String>,
) -> Result<(), String> {
    let catalog = layout.personal_catalog_path();
    let records: Vec<AdoptionRecord> = scan
        .adoptable
        .items
        .iter()
        .map(|item| {
            let origin = item
                .source
                .clone()
                .or_else(|| declaring_file(source, &item.kind))
                .ok_or_else(|| {
                    format!(
                        "cannot resolve the origin of {} '{}'",
                        kind_label(&item.kind),
                        item.name
                    )
                })?;
            Ok(AdoptionRecord {
                source: source.as_str().to_string(),
                kind: kind_label(&item.kind).to_string(),
                component_id: item.name.clone(),
                name: item.name.clone(),
                origin: Some(origin),
                revision: None,
                digest: digests.get(&item.name).cloned(),
                enabled: None,
                policy: scan.adoptable.policy.get(&item.name).cloned(),
            })
        })
        .collect::<Result<_, String>>()?;
    let keys: Vec<String> = targets.iter().map(|t| t.as_str().to_string()).collect();
    store_adoption_records(&catalog, &records, &keys).map_err(|e| e.to_string())
}

/// The source file that declares an item whose reader recorded no origin of
/// its own: the agent's plugin registry for a plugin whose marketplace source
/// could not be resolved. Skills and MCP servers always carry their origin.
fn declaring_file(source: AgentId, kind: &ItemKind) -> Option<String> {
    let path = match (source, kind) {
        (AgentId::Claude, ItemKind::Plugin) => {
            ccync_foundation::paths::claude_installed_plugins_path()
        }
        (AgentId::Copilot, ItemKind::Plugin) => ccync_foundation::paths::user_home()
            .map(|home| home.join(".copilot").join("installed-plugins.json")),
        _ => None,
    }?;
    Some(path.to_string_lossy().into_owned())
}

/// Adopt the consented scan's non-ccync-managed items into the lock file, and
/// record the source under `_adoptMaster`. Prints a summary. Runs only after
/// consent and after the `_adoption` and config writes.
fn run_import_adopt(master: &str, consented: &SourceScanOutcome) -> Result<(), ExitCode> {
    let layout = ccync_foundation::paths::EffectiveLayout::load_or_legacy().map_err(|error| {
        eprintln!("ccync init: cannot load effective storage layout: {error}");
        ExitCode::Error
    })?;
    run_import_adopt_at(master, consented, &layout)
}

fn run_import_adopt_at(
    master: &str,
    consented: &SourceScanOutcome,
    layout: &ccync_foundation::paths::EffectiveLayout,
) -> Result<(), ExitCode> {
    use ccync_engine::adopt::{
        adopt_consented_mcp_definitions, adopt_items, adopt_loose_skills, read_mcp_definitions,
        AgentKind,
    };
    use std::collections::HashSet;

    let lock_path = layout.build_lock_path();

    let source: AgentId = master.parse().map_err(|_| {
        eprintln!(
            "ccync init: unknown source '{master}'; expected claude, codex, copilot, opencode, or agy"
        );
        ExitCode::Usage
    })?;
    let scanned = consented.scan.adoptable.clone();
    let items = scanned.items;
    let mut skipped_vendor_native = scanned.skipped_vendor_native;
    let skipped_owned = scanned.skipped_owned;

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
    let source_kind = match source {
        AgentId::Claude => AgentKind::Claude,
        AgentId::Codex => AgentKind::Codex,
        AgentId::Copilot => AgentKind::Copilot,
        AgentId::Opencode => AgentKind::OpenCode,
        AgentId::Agy => AgentKind::Agy,
    };
    let mcp_defs = read_mcp_definitions(&source_kind);
    skipped_vendor_native.extend(mcp_defs.skipped_vendor_native.iter().cloned());
    let refused =
        adopt_consented_mcp_definitions(&layout.personal_catalog_path(), &lock_path, source)
            .map_err(|e| {
                eprintln!("ccync init: could not snapshot MCP definitions: {e}");
                ExitCode::Error
            })?;
    let mut completion = consented.clone();
    for (name, reason) in refused {
        let origin = items_for_loose_skills
            .iter()
            .find(|item| item.name == name && item.kind == ItemKind::McpServer)
            .and_then(|item| item.source.as_ref())
            .map(PathBuf::from);
        completion
            .blocked_mcp
            .push(ccync_engine::adopt::BlockedMcpServer {
                name,
                origin,
                reason: format!("{reason:?}"),
                disabled: false,
            });
    }

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
    let mut skipped_names = skipped_vendor_native;
    skipped_names.sort();
    skipped_names.dedup();
    if !skipped_names.is_empty() {
        println!(
            "Skipped vendor-native item(s): {}",
            skipped_names.join(", ")
        );
    }
    if !skipped_owned.is_empty() {
        println!(
            "Excluded ccync-owned skill copies (not imported): {}",
            skipped_owned.join(", ")
        );
    }
    for line in mcp_diagnostic_lines(&completion) {
        println!("{line}");
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
    let text = serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?;
    ccync_foundation::platform::atomic_write_bytes(lock_path, text.as_bytes())
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
    let layout = ccync_foundation::paths::EffectiveLayout::load_or_legacy()
        .map_err(|error| error.to_string())?;
    write_projection_selection_to_config_at(targets, &layout)
}

fn write_projection_selection_to_config_at(
    targets: &[ccync_foundation::target::ProjectionTargetId],
    layout: &ccync_foundation::paths::EffectiveLayout,
) -> Result<(), String> {
    let cfg_path = layout.machine_config_path();
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

    struct TestHomeEnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl Drop for TestHomeEnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    fn isolate_test_home(home: &std::path::Path) -> TestHomeEnvGuard {
        let keys = [
            "USERPROFILE",
            "HOME",
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
        ];
        let saved = keys
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        for key in keys {
            unsafe {
                std::env::set_var(key, home);
            }
        }
        TestHomeEnvGuard(saved)
    }

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
                "ccync init: could not write the target selection to config.json: {message}"
            ),
            "the config write-error literal must be present byte-exact"
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
    fn agent_entries_has_five_choices_all_checked_for_a_fresh_init() {
        assert_eq!(AGENT_ENTRIES.len(), 5, "five agent choices");

        let checked: Vec<AgentId> = AGENT_ENTRIES
            .iter()
            .filter(|(_, _, default)| *default)
            .map(|(agent, _, _)| *agent)
            .collect();
        assert_eq!(
            checked,
            vec![
                AgentId::Claude,
                AgentId::Codex,
                AgentId::Copilot,
                AgentId::Opencode,
                AgentId::Agy
            ],
            "a fresh init selects all five targets"
        );
        // Every checked row expands to the seven canonical targets.
        let expanded = expand_selection(&[0, 1, 2, 3, 4], &[]);
        assert_eq!(expanded.len(), 7);
    }

    #[test]
    fn mcp_host_lines_state_a_write_for_every_host_selected_or_not() {
        let lines = mcp_host_lines(AgentId::Claude, 2, &[ProjectionTargetId::Claude]);
        assert_eq!(lines.len(), 5, "one line per MCP host");
        for line in &lines {
            assert!(line.contains("receives the managed MCP set"), "{line}");
            assert!(!line.contains("no MCP write"), "{line}");
        }
        assert!(lines[0].contains("claude:") && lines[0].contains("(agent selected)"));
        assert!(lines[1].contains("codex:") && lines[1].contains("(agent not selected)"));
        assert!(MCP_HOST_LINE.contains("independently of agent selection"));
        assert!(MCP_HOST_LINE.contains("not changed by this screen"));
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
    fn agent_entries_four_non_agy_choices_are_identity_families() {
        for agent in [
            AgentId::Claude,
            AgentId::Codex,
            AgentId::Copilot,
            AgentId::Opencode,
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
    fn parse_target_list_accepts_agents_targets_and_all() {
        let keys = |raw: &str| -> Vec<&'static str> {
            parse_target_list(raw)
                .unwrap()
                .iter()
                .map(|t| t.as_str())
                .collect()
        };
        // An agent name expands to its own target.
        assert_eq!(keys("claude"), vec!["claude"]);
        // `agy` expands to all three surfaces.
        assert_eq!(keys("agy"), vec!["agy-cli", "agy-ide", "agy-gui"]);
        // A canonical target key keeps a partial AGY subset.
        assert_eq!(keys("claude,agy-cli"), vec!["claude", "agy-cli"]);
        // Duplicates collapse, order is kept, and `all` lists the seven targets.
        assert_eq!(keys("codex,claude,codex"), vec!["codex", "claude"]);
        assert_eq!(keys("all").len(), 7);
    }

    #[test]
    fn parse_target_list_rejects_unknown_tokens_and_reports_empty() {
        assert!(parse_target_list("frobnicate").is_err());
        // An empty list is not an error here; the caller reports it as missing.
        assert!(parse_target_list(" , ").unwrap().is_empty());
    }

    #[test]
    fn expand_selection_keeps_saved_partial_agy_and_expands_otherwise() {
        let agy_idx = AGENT_ENTRIES
            .iter()
            .position(|(a, _, _)| *a == AgentId::Agy)
            .unwrap();
        // No saved selection: the AGY row expands to all three surfaces.
        assert_eq!(
            expand_selection(&[agy_idx], &[]),
            vec![
                ProjectionTargetId::AgyCli,
                ProjectionTargetId::AgyIde,
                ProjectionTargetId::AgyGui
            ]
        );
        // A saved partial subset is kept as saved.
        assert_eq!(
            expand_selection(&[agy_idx], &[ProjectionTargetId::AgyCli]),
            vec![ProjectionTargetId::AgyCli]
        );
    }

    #[test]
    fn prompt_targets_non_tty_returns_none_so_the_caller_refuses() {
        // In cargo test stdin is not a TTY, so no selection is made and the
        // caller reports the missing target list instead of falling back.
        assert!(prompt_targets(&[]).is_none());
    }

    // ── interactive seam: cancel at each prompt refuses before any write ────

    /// Answers the questions from a script and records which were asked, in order.
    struct ScriptedPrompts {
        source: Option<AgentId>,
        targets: Option<Vec<ProjectionTargetId>>,
        consent: bool,
        asked: Vec<&'static str>,
    }

    impl ScriptedPrompts {
        fn new(
            source: Option<AgentId>,
            targets: Option<Vec<ProjectionTargetId>>,
            consent: bool,
        ) -> Self {
            Self {
                source,
                targets,
                consent,
                asked: Vec::new(),
            }
        }
    }

    impl InitPrompts for ScriptedPrompts {
        fn source(&mut self) -> Option<AgentId> {
            self.asked.push("source");
            self.source
        }

        fn targets(&mut self, _saved: &[ProjectionTargetId]) -> Option<Vec<ProjectionTargetId>> {
            self.asked.push("targets");
            self.targets.clone()
        }

        fn scan(&mut self, source: AgentId) -> SourceScanOutcome {
            self.asked.push("scan");
            SourceScanOutcome {
                scan: SourceScan {
                    agent: source,
                    state: SourceState::Ready,
                    origins: Vec::new(),
                    adoptable: ccync_engine::adopt::AdoptableState::default(),
                    reason: None,
                    unsupported: Vec::new(),
                },
                blocked_mcp: Vec::new(),
                disabled_mcp: Vec::new(),
            }
        }

        fn consent(&mut self, _assume_yes: bool, _env: Option<&str>, _interactive: bool) -> bool {
            self.asked.push("consent");
            self.consent
        }
    }

    #[test]
    fn init_selection_cancel_at_source_prompt_refuses_before_any_other_question() {
        let mut prompts = ScriptedPrompts::new(None, None, true);
        let result = choose_scan_and_adopt(&["init".to_string()], true, &mut prompts);
        assert_eq!(result, Err(ExitCode::Usage));
        assert_eq!(prompts.asked, vec!["source"]);
    }

    #[test]
    fn init_selection_cancel_at_target_prompt_refuses_before_scanning() {
        let args = ["init", "claude"].map(String::from);
        let mut prompts = ScriptedPrompts::new(None, None, true);
        let result = choose_scan_and_adopt(&args, true, &mut prompts);
        assert_eq!(result, Err(ExitCode::Usage));
        assert_eq!(prompts.asked, vec!["targets"]);
    }

    #[test]
    fn init_selection_cancel_at_consent_prompt_refuses_after_the_preview_only() {
        let args = ["init", "claude", "--agents", "claude"].map(String::from);
        let mut prompts = ScriptedPrompts::new(None, None, false);
        let result = choose_scan_and_adopt(&args, true, &mut prompts);
        assert_eq!(result, Err(ExitCode::Usage));
        assert_eq!(prompts.asked, vec!["scan", "consent"]);
    }

    #[test]
    fn init_selection_offline_label_needs_readable_files_and_no_executable() {
        assert!(source_is_offline(&SourceState::Ready, false));
        assert!(source_is_offline(&SourceState::Empty, false));
        assert!(!source_is_offline(&SourceState::Ready, true));
        assert!(!source_is_offline(&SourceState::Absent, false));
        assert!(!source_is_offline(&SourceState::Unreadable, false));
    }

    #[test]
    fn init_selection_shared_root_lines_name_unselected_consumers() {
        let text = shared_root_lines(&[ProjectionTargetId::Opencode]).join("\n");
        assert!(
            text.contains(
                "shared root ~/.agents/skills: reached by codex (not selected), copilot (not selected), opencode (selected)"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "shared root ~/.claude/skills: reached by claude (not selected), opencode (selected)"
            ),
            "{text}"
        );
        assert!(!text.contains("~/.gemini"), "{text}");
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
        let _home_env = isolate_test_home(home.path());

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

        let result = run_import_adopt("claude", &scan_source_outcome(AgentId::Claude));

        let lock_path = home.path().join(".ccync").join("build").join("lock.json");
        let lock_text = std::fs::read_to_string(&lock_path);

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

    // ── vendor-native names reach no lockfile namespace ─────────────────────

    /// The reader-level tests in `ccync-engine` prove the item vector and the
    /// MCP-definition store come back filtered, and the summary test below
    /// proves the printed line is right — but neither opens the lock file the
    /// init actually writes. Two independent writers land in it
    /// (`adopt_items` for `_adoptedItems`, `adopt_mcp_definitions` for
    /// `_mcpServers`), so "the readers returned clean data" is not the same
    /// claim as "nothing vendor-native was persisted". This asserts the
    /// namespaces directly rather than the exit code: a codex master carrying
    /// the vendor-native `node_repl` beside a real user server must persist the
    /// user server with its definition intact and leave no trace of the vendor
    /// entry in any namespace.
    #[test]
    fn run_import_adopt_codex_persists_no_vendor_native_row_in_any_namespace() {
        let _env = crate::test_io_lock();
        let home = tempfile::tempdir().unwrap();
        let _home_env = isolate_test_home(home.path());
        let codex_dir = home.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("config.toml"),
            r#"
[mcp_servers.node_repl]
command = "node"

[mcp_servers.fetch]
command = "node"
args = ["./server.js"]
"#,
        )
        .unwrap();

        let outcome = scan_source_outcome(AgentId::Codex);
        let layout = ccync_foundation::paths::EffectiveLayout::load_or_legacy().unwrap();
        let digests = ccync_engine::adopt::preview_mcp_digests(AgentId::Codex, &outcome.scan);
        store_adoption(AgentId::Codex, &outcome.scan, &[], &layout, &digests).unwrap();
        let result = run_import_adopt("codex", &outcome);

        let lock_path = home.path().join(".ccync").join("build").join("lock.json");
        let lock_text = std::fs::read_to_string(&lock_path);

        assert!(
            result.is_ok(),
            "run_import_adopt should succeed: {result:?}"
        );
        let content: Value = serde_json::from_str(&lock_text.unwrap()).unwrap();

        // `_adoptedItems`: the user server is recorded, the vendor entry is not.
        let adopted_names: Vec<&str> = content["_adoptedItems"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|e| e["name"].as_str())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert!(
            adopted_names.contains(&"fetch"),
            "the user's own MCP server must still adopt: {adopted_names:?}"
        );
        assert!(
            !adopted_names.contains(&"node_repl"),
            "a vendor-native name must not reach _adoptedItems: {adopted_names:?}"
        );

        // `_mcpServers`: the user server's definition survives byte-for-byte,
        // the vendor entry has no key at all.
        let servers = content["_mcpServers"]
            .as_object()
            .expect("_mcpServers namespace should exist once a user server adopts");
        assert!(
            !servers.contains_key("node_repl"),
            "a vendor-native name must not reach _mcpServers: {:?}",
            servers.keys().collect::<Vec<_>>()
        );
        assert_eq!(servers["fetch"]["command"], "node");
        assert_eq!(servers["fetch"]["args"][0], "./server.js");

        // `_looseSkills`: absent or present, it must not carry the vendor name.
        let loose_names: Vec<&str> = content["_looseSkills"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|e| e["name"].as_str())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert!(
            !loose_names.contains(&"node_repl"),
            "a vendor-native name must not reach _looseSkills: {loose_names:?}"
        );

        assert_eq!(content["_adoptMaster"], "codex");
    }

    // ── skipped vendor-native name dedup + summary print ────────────────────

    /// A codex `node_repl` MCP server is vendor-native and gets independently
    /// skipped by BOTH readers `run_import_adopt` merges:
    /// `read_codex_adoptable_state` (the item vector) and `read_mcp_definitions`
    /// (the full-definition store). Without dedup, the printed summary line
    /// would repeat `node_repl` once per reader. Runs `run_import_adopt` in a
    /// relaunched child process (with `--nocapture`) so this test observes the
    /// real `println!` output rather than the pre-print data model.
    #[test]
    fn run_import_adopt_dedups_skipped_vendor_native_across_both_readers() {
        let _env_lock = crate::test_io_lock();
        if std::env::var("CCYNC_TEST_INIT_CHILD").as_deref() == Ok("skipped_dedup") {
            let home = std::env::var("CCYNC_TEST_HOME").expect("child must receive fixture home");
            let _home_env = isolate_test_home(std::path::Path::new(&home));
            let _ = run_import_adopt("codex", &scan_source_outcome(AgentId::Codex));
            return;
        }

        let home = tempfile::tempdir().unwrap();
        let _home_env = isolate_test_home(home.path());
        let codex_dir = home.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("config.toml"),
            r#"
[mcp_servers.node_repl]
command = "node"

[mcp_servers.fetch]
command = "uvx"
args = ["mcp-server-fetch"]
"#,
        )
        .unwrap();

        let exe = std::env::current_exe().expect("test binary path");
        let output = std::process::Command::new(exe)
            .arg("--exact")
            .arg(
                "commands::init::tests::\
                 run_import_adopt_dedups_skipped_vendor_native_across_both_readers",
            )
            .arg("--nocapture")
            .env("CCYNC_TEST_INIT_CHILD", "skipped_dedup")
            .env("CCYNC_TEST_HOME", home.path())
            .output()
            .expect("relaunch test binary for the child run");

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("Skipped vendor-native item(s): node_repl"),
            "expected the deduplicated skipped-name summary line, got: {stdout}"
        );
        assert_eq!(
            stdout.matches("node_repl").count(),
            1,
            "node_repl is reported skipped by both the item reader and the \
             MCP-definitions reader; the printed line must dedup it to a single \
             occurrence, got: {stdout}"
        );
    }

    /// Mirrors the dedup test above but with nothing vendor-native present:
    /// the summary line must not print at all, not even an empty
    /// "Skipped vendor-native item(s): " line.
    #[test]
    fn run_import_adopt_prints_no_skipped_line_when_nothing_skipped() {
        let _env_lock = crate::test_io_lock();
        if std::env::var("CCYNC_TEST_INIT_CHILD").as_deref() == Ok("skipped_none") {
            let home = std::env::var("CCYNC_TEST_HOME").expect("child must receive fixture home");
            let _home_env = isolate_test_home(std::path::Path::new(&home));
            let _ = run_import_adopt("codex", &scan_source_outcome(AgentId::Codex));
            return;
        }

        let home = tempfile::tempdir().unwrap();
        let _home_env = isolate_test_home(home.path());
        let codex_dir = home.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("config.toml"),
            r#"
[mcp_servers.fetch]
command = "uvx"
args = ["mcp-server-fetch"]
"#,
        )
        .unwrap();

        let exe = std::env::current_exe().expect("test binary path");
        let output = std::process::Command::new(exe)
            .arg("--exact")
            .arg(
                "commands::init::tests::\
                 run_import_adopt_prints_no_skipped_line_when_nothing_skipped",
            )
            .arg("--nocapture")
            .env("CCYNC_TEST_INIT_CHILD", "skipped_none")
            .env("CCYNC_TEST_HOME", home.path())
            .output()
            .expect("relaunch test binary for the child run");

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !stdout.contains("Skipped vendor-native"),
            "no vendor-native item was skipped, so no summary line should print, got: {stdout}"
        );
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
        let _home_env = isolate_test_home(home.path());

        let targets = [
            ccync_foundation::target::ProjectionTargetId::Codex,
            ccync_foundation::target::ProjectionTargetId::Claude,
        ];
        let result = write_projection_selection_to_config(&targets);

        let cfg_path = ccync_foundation::paths::EffectiveLayout::load_or_legacy()
            .unwrap()
            .machine_config_path();
        let content_text = std::fs::read_to_string(&cfg_path);

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
