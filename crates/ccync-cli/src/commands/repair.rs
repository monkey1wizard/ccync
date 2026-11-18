//! `ccync repair [<id>]` — resolve a knowable clonable source for an adopted
//! item that has none (or explicitly overwrite one), via user-confirmed
//! search. Pre-init and non-projecting: this command never writes
//! anything but the single `_adoptedItems` row's `source`, and never
//! triggers a projection pass.

use ccync_engine::adopt::{adopted_items_without_source, find_adopted_item, write_adopted_source};
use ccync_engine::ExitCode;
use std::io::IsTerminal;
use std::path::Path;

use super::search::{search as run_search, SearchOutcome, SearchStatus};

/// Strip a plugin's `@marketplace` suffix for the search term; MCP/skill
/// names are searched as-is since they carry no marketplace qualifier.
fn repair_search_term(name: &str, source_id: &str) -> String {
    if source_id.contains("plugin") {
        name.split_once('@')
            .map(|(n, _)| n.to_string())
            .unwrap_or_else(|| name.to_string())
    } else {
        name.to_string()
    }
}

/// A candidate repair URL must be non-empty and HTTPS — never `http://`, a
/// bare host/path, or empty.
fn is_valid_repair_url(url: &str) -> bool {
    url.starts_with("https://") && url.len() > "https://".len()
}

/// Outcome of attempting to repair one adopted row's source.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RepairOutcome {
    Written,
    Declined,
    NoResult,
    ProviderFailed,
    InvalidUrl,
    WriteFailed,
}

/// Resolve one adopted row's source: search, then select/confirm, validate,
/// and write. Every side-effecting step (`search_fn`/`select`/`confirm`/
/// `write_fn`) is injected so this is unit-testable without a real terminal,
/// provider subprocess, or lock file write.
pub(crate) fn repair_one(
    name: &str,
    term: &str,
    search_fn: impl FnOnce(&str) -> SearchOutcome,
    select: impl FnOnce(&[String]) -> Option<usize>,
    confirm: impl FnOnce(&str) -> bool,
    write_fn: impl FnOnce(&str, &str) -> Result<bool, String>,
) -> RepairOutcome {
    let outcome = search_fn(term);

    let clone_url = match outcome.status {
        SearchStatus::AllFailed => return RepairOutcome::ProviderFailed,
        SearchStatus::NoResults => return RepairOutcome::NoResult,
        SearchStatus::Found => match outcome.hits.as_slice() {
            [] => return RepairOutcome::NoResult,
            [only] => {
                if confirm(&only.clone_url) {
                    Some(only.clone_url.clone())
                } else {
                    None
                }
            }
            many => {
                let labels: Vec<String> = many
                    .iter()
                    .map(|h| format!("{} ({} stars) - {}", h.full_name, h.stars, h.clone_url))
                    .collect();
                select(&labels).and_then(|i| {
                    let url = &many[i].clone_url;
                    confirm(url).then(|| url.clone())
                })
            }
        },
    };

    let Some(clone_url) = clone_url else {
        return RepairOutcome::Declined;
    };

    if !is_valid_repair_url(&clone_url) {
        return RepairOutcome::InvalidUrl;
    }

    match write_fn(name, &clone_url) {
        Ok(true) => RepairOutcome::Written,
        Ok(false) | Err(_) => RepairOutcome::WriteFailed,
    }
}

/// Repair-specific selection prompt copy — deliberately never reuses `ccync
/// search`'s `Add ...?` wording.
fn select_hit_interactive(labels: &[String]) -> Option<usize> {
    dialoguer::Select::new()
        .with_prompt("Multiple matches found — pick the real source")
        .items(labels)
        .default(0)
        .interact_opt()
        .ok()?
}

/// Repair-specific confirmation prompt copy — see [`select_hit_interactive`].
/// Defaults to declining; any interaction error is treated as "no" (a
/// confirmation prompt must never fail open into a write).
fn confirm_hit_interactive(clone_url: &str) -> bool {
    dialoguer::Confirm::new()
        .with_prompt(format!("Record {clone_url} as the source?"))
        .default(false)
        .interact()
        .unwrap_or(false)
}

/// A single adopted row targeted for repair: its name plus its `sourceId`
/// (used to decide whether `@marketplace` must be stripped from the search
/// term).
struct RepairTarget {
    name: String,
    source_id: String,
}

fn source_id_of(row: &serde_json::Value) -> String {
    row.get("sourceId")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Resolve the set of rows this invocation should repair: the explicit `id`
/// (which may already have a source because an explicit repair may overwrite
/// any adopted source), or every no-source row when no `id` was given.
fn resolve_targets(
    explicit_id: Option<&str>,
    lock_path: &Path,
) -> Result<Vec<RepairTarget>, ExitCode> {
    match explicit_id {
        Some(id) => match find_adopted_item(id, lock_path) {
            Ok(Some(row)) => Ok(vec![RepairTarget {
                name: id.to_string(),
                source_id: source_id_of(&row),
            }]),
            Ok(None) => {
                eprintln!("ccync repair: unknown adopted item {id:?}");
                Err(ExitCode::Error)
            }
            Err(e) => {
                eprintln!("ccync repair: failed to read lock file: {e}");
                Err(ExitCode::Error)
            }
        },
        None => match adopted_items_without_source(lock_path) {
            Ok(names) => Ok(names
                .into_iter()
                .map(|name| {
                    let source_id = find_adopted_item(&name, lock_path)
                        .ok()
                        .flatten()
                        .map(|row| source_id_of(&row))
                        .unwrap_or_default();
                    RepairTarget { name, source_id }
                })
                .collect()),
            Err(e) => {
                eprintln!("ccync repair: failed to read lock file: {e}");
                Err(ExitCode::Error)
            }
        },
    }
}

/// `ccync repair [<id>]` CLI entry point.
///
/// No argument processes every no-source `_adoptedItems` row; an explicit
/// `<id>` may overwrite any adopted row's existing source. If stdin or
/// stdout is not a terminal, this only lists the targeted ids, states that
/// no search or write occurred, and makes zero provider calls — repair is an
/// inherently interactive, per-item confirmation flow.
pub(crate) fn cmd_repair(args: &[String]) -> ExitCode {
    if let Err(code) =
        super::args::reject_unknown_flags("repair", args, &[], &[], 1, "Usage: ccync repair [<id>]")
    {
        return code;
    }

    let layout = match ccync_foundation::paths::EffectiveLayout::load_or_legacy() {
        Ok(layout) => layout,
        Err(error) => {
            eprintln!("ccync repair: cannot load storage layout: {error}");
            return ExitCode::Error;
        }
    };

    let explicit_id = args.iter().skip(1).find(|a| !a.starts_with("--"));

    let lock_path = layout.build_lock_path();

    let targets = match resolve_targets(explicit_id.map(String::as_str), &lock_path) {
        Ok(targets) => targets,
        Err(code) => return code,
    };

    if targets.is_empty() {
        println!("ccync repair: no unresolved adopted items.");
        return ExitCode::Success;
    }

    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if !interactive {
        println!(
            "ccync repair: {} item(s) need a source (non-interactive: no search, no write, zero provider calls):",
            targets.len()
        );
        for target in &targets {
            println!("  {}", target.name);
        }
        return ExitCode::Success;
    }

    let mut had_error = false;
    for target in &targets {
        let term = repair_search_term(&target.name, &target.source_id);
        let name = target.name.clone();
        let outcome = repair_one(
            &name,
            &term,
            |t| run_search(t, 10),
            select_hit_interactive,
            confirm_hit_interactive,
            |n, url| write_adopted_source(n, url, &lock_path).map_err(|e| e.to_string()),
        );
        match outcome {
            RepairOutcome::Written => println!("ccync repair: recorded source for {name}"),
            RepairOutcome::Declined => println!("ccync repair: skipped {name} (declined)"),
            RepairOutcome::NoResult => println!("ccync repair: no candidates found for {name}"),
            RepairOutcome::ProviderFailed => {
                eprintln!("ccync repair: all search providers failed for {name}");
                had_error = true;
            }
            RepairOutcome::InvalidUrl => {
                eprintln!("ccync repair: selected URL is not a valid HTTPS URL for {name}");
                had_error = true;
            }
            RepairOutcome::WriteFailed => {
                eprintln!("ccync repair: failed to write source for {name}");
                had_error = true;
            }
        }
    }

    if had_error {
        ExitCode::Error
    } else {
        ExitCode::Success
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::search::{Platform, Provider, SearchHit};

    fn hit(full_name: &str, stars: u64, clone_url: &str) -> SearchHit {
        SearchHit {
            provider: Provider::Gh,
            platform: Platform::GitHub,
            full_name: full_name.to_string(),
            stars,
            description: None,
            clone_url: clone_url.to_string(),
        }
    }

    #[test]
    fn repair_search_term_strips_marketplace_suffix_for_plugins() {
        assert_eq!(
            repair_search_term("i-have-adhd-zh-tw@panda850819", "claude-plugin"),
            "i-have-adhd-zh-tw"
        );
    }

    #[test]
    fn repair_search_term_keeps_full_name_for_mcp_and_skill() {
        assert_eq!(
            repair_search_term("codebase-memory-mcp", "claude-mcp"),
            "codebase-memory-mcp"
        );
        assert_eq!(
            repair_search_term("some-skill", "claude-skill"),
            "some-skill"
        );
    }

    #[test]
    fn repair_search_term_plugin_without_at_sign_is_unchanged() {
        assert_eq!(
            repair_search_term("bare-name", "claude-plugin"),
            "bare-name"
        );
    }

    #[test]
    fn is_valid_repair_url_accepts_only_non_empty_https() {
        assert!(is_valid_repair_url("https://github.com/o/r"));
        assert!(!is_valid_repair_url("http://github.com/o/r"));
        assert!(!is_valid_repair_url(""));
        assert!(!is_valid_repair_url("https://"));
        assert!(!is_valid_repair_url("github.com/o/r"));
    }

    fn outcome(status: SearchStatus, hits: Vec<SearchHit>) -> SearchOutcome {
        SearchOutcome {
            status,
            hits,
            warnings: vec![],
        }
    }

    #[test]
    fn repair_one_all_failed_returns_provider_failed() {
        let result = repair_one(
            "x",
            "term",
            |_| outcome(SearchStatus::AllFailed, vec![]),
            |_| None,
            |_| true,
            |_, _| Ok(true),
        );
        assert_eq!(result, RepairOutcome::ProviderFailed);
    }

    #[test]
    fn repair_one_no_results_returns_no_result() {
        let result = repair_one(
            "x",
            "term",
            |_| outcome(SearchStatus::NoResults, vec![]),
            |_| None,
            |_| true,
            |_, _| Ok(true),
        );
        assert_eq!(result, RepairOutcome::NoResult);
    }

    #[test]
    fn repair_one_single_hit_declined_returns_declined() {
        let result = repair_one(
            "x",
            "term",
            |_| {
                outcome(
                    SearchStatus::Found,
                    vec![hit("o/r", 5, "https://github.com/o/r")],
                )
            },
            |_| None,
            |_| false, // decline
            |_, _| Ok(true),
        );
        assert_eq!(result, RepairOutcome::Declined);
    }

    #[test]
    fn repair_one_single_hit_confirmed_writes() {
        let mut written = None;
        let result = repair_one(
            "x",
            "term",
            |_| {
                outcome(
                    SearchStatus::Found,
                    vec![hit("o/r", 5, "https://github.com/o/r")],
                )
            },
            |_| None,
            |_| true,
            |n, url| {
                written = Some((n.to_string(), url.to_string()));
                Ok(true)
            },
        );
        assert_eq!(result, RepairOutcome::Written);
        assert_eq!(
            written,
            Some(("x".to_string(), "https://github.com/o/r".to_string()))
        );
    }

    #[test]
    fn repair_one_multiple_hits_cancelled_selection_returns_declined() {
        let result = repair_one(
            "x",
            "term",
            |_| {
                outcome(
                    SearchStatus::Found,
                    vec![
                        hit("a/r", 5, "https://github.com/a/r"),
                        hit("b/r", 3, "https://github.com/b/r"),
                    ],
                )
            },
            |_| None, // cancel selection
            |_| true,
            |_, _| Ok(true),
        );
        assert_eq!(result, RepairOutcome::Declined);
    }

    #[test]
    fn repair_one_multiple_hits_selected_and_confirmed_writes_chosen_url() {
        let mut written = None;
        let result = repair_one(
            "x",
            "term",
            |_| {
                outcome(
                    SearchStatus::Found,
                    vec![
                        hit("a/r", 5, "https://github.com/a/r"),
                        hit("b/r", 3, "https://github.com/b/r"),
                    ],
                )
            },
            |_| Some(1),
            |_| true,
            |n, url| {
                written = Some((n.to_string(), url.to_string()));
                Ok(true)
            },
        );
        assert_eq!(result, RepairOutcome::Written);
        assert_eq!(
            written,
            Some(("x".to_string(), "https://github.com/b/r".to_string()))
        );
    }

    #[test]
    fn repair_one_invalid_url_never_calls_write() {
        let mut write_called = false;
        let result = repair_one(
            "x",
            "term",
            |_| {
                outcome(
                    SearchStatus::Found,
                    vec![hit("o/r", 5, "http://github.com/o/r")],
                )
            },
            |_| None,
            |_| true,
            |_, _| {
                write_called = true;
                Ok(true)
            },
        );
        assert_eq!(result, RepairOutcome::InvalidUrl);
        assert!(!write_called, "an invalid URL must never reach the writer");
    }

    #[test]
    fn repair_one_write_failure_reported_as_write_failed() {
        let result = repair_one(
            "x",
            "term",
            |_| {
                outcome(
                    SearchStatus::Found,
                    vec![hit("o/r", 5, "https://github.com/o/r")],
                )
            },
            |_| None,
            |_| true,
            |_, _| Err("boom".to_string()),
        );
        assert_eq!(result, RepairOutcome::WriteFailed);
    }

    #[test]
    fn repair_one_write_unknown_name_reported_as_write_failed() {
        let result = repair_one(
            "x",
            "term",
            |_| {
                outcome(
                    SearchStatus::Found,
                    vec![hit("o/r", 5, "https://github.com/o/r")],
                )
            },
            |_| None,
            |_| true,
            |_, _| Ok(false),
        );
        assert_eq!(result, RepairOutcome::WriteFailed);
    }

    // ── cmd_repair argument discipline ──────────────────────────────────────

    #[test]
    fn cmd_repair_help_prints_usage_and_succeeds() {
        assert_eq!(
            cmd_repair(&["repair".to_string(), "--help".to_string()]),
            ExitCode::Success
        );
    }

    #[test]
    fn cmd_repair_rejects_unknown_flag() {
        assert_eq!(
            cmd_repair(&["repair".to_string(), "--frobnicate".to_string()]),
            ExitCode::Usage
        );
    }

    #[test]
    fn cmd_repair_rejects_extra_positional() {
        assert_eq!(
            cmd_repair(&[
                "repair".to_string(),
                "one-id".to_string(),
                "two-id".to_string()
            ]),
            ExitCode::Usage
        );
    }
}
