//! Durable projection-domain vocabulary guard.
//!
//! Rejects new projection-domain uses of the word "runtime" while
//! permitting a fixed, embedded allowlist of already-reviewed exceptions:
//! legacy config-key spellings (the durable legacy-read boundary --
//! `selectedRuntimes`/`primaryRuntime`/old-layout `runtimes` are read
//! forever, never renamed), pre-existing engine/CLI identifiers that
//! predate this guard and are out of this plan's code-change scope
//! (chains 1-6 renamed the projection-domain *type* model; per-function
//! and per-test renames were not in their blast radius), a literal quote
//! of a live CLI string, and genuine execution-environment/third-party
//! meaning (C runtime linkage, `VCRUNTIME140.dll`).
//!
//! Granularity is (file path, matched token) -- not (file, line). A token
//! is an identifier-like run of `[A-Za-z0-9_]` characters that
//! case-insensitively contains "runtime". Once a (path, token) pair is
//! allowlisted, every occurrence of that exact token anywhere in that file
//! is permitted. This keeps the allowlist under 110 entries instead of
//! hundreds of line coordinates. The trade-off: a brand new *use* of an
//! already-allowed token in an already-allowed file is not caught -- what
//! this guard does catch is the actual growth vector it exists to block,
//! any NEW file or any NEW token introducing "runtime" outside this list.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// (path relative to repo root, exact case-sensitive token) pairs.
/// Every entry is a reviewed exception -- see the module doc above for the
/// four categories.
///
/// The bare-word entries below (`"runtime"`, `"runtimes"`, `"Runtime"`) are
/// grandfathered: each covers a file that already carried those occurrences
/// when the guard was introduced, and admitting them per-file was the only
/// way to seal the guard without a repo-wide rename. They are not the
/// pattern to copy. For a file not already listed with that bare word, or
/// for any token this list does not already carry, add the exact (path,
/// token) identifier pair -- confirm it fits a category first, and justify
/// it in one line in the commit message. Widening an existing bare-word
/// entry to a new file needs the same scrutiny as a rename.
const ALLOWED: &[(&str, &str)] = &[
    ("crates/ccync-cli/src/commands/doctor.rs", "selectedRuntimes"),
    ("crates/ccync-cli/src/commands/doctor_block_tests.rs", "selectedRuntimes"),
    ("crates/ccync-cli/src/commands/init.rs", "runtime"),
    ("crates/ccync-cli/src/commands/init.rs", "selectedRuntimes"),
    ("crates/ccync-cli/src/commands/lifecycle.rs", "Runtime"),
    (
        "crates/ccync-cli/src/commands/lifecycle.rs",
        "grouped_touch_scope_excludes_unselected_runtime_paths",
    ),
    (
        "crates/ccync-cli/src/commands/lifecycle.rs",
        "grouped_touch_scope_runtime_section_uses_preview_language_not_guarantee",
    ),
    ("crates/ccync-cli/src/commands/lifecycle.rs", "runtime"),
    ("crates/ccync-cli/src/commands/lifecycle.rs", "runtime_surfaces"),
    ("crates/ccync-cli/src/commands/lifecycle.rs", "runtimes"),
    ("crates/ccync-cli/src/commands/lifecycle.rs", "selectedRuntimes"),
    ("crates/ccync-cli/src/commands/lifecycle.rs", "selected_runtimes"),
    ("crates/ccync-cli/src/commands/plugin.rs", "runtime"),
    ("crates/ccync-cli/src/commands/plugin.rs", "runtimes"),
    ("crates/ccync-engine/src/adopt.rs", "runtime"),
    ("crates/ccync-engine/src/catalog.rs", "runtime"),
    ("crates/ccync-engine/src/doctor.rs", "runtime"),
    ("crates/ccync-engine/src/install.rs", "DEFAULT_RUNTIME_ORDER"),
    ("crates/ccync-engine/src/install.rs", "EXPECTED_DEFAULT_RUNTIME_ORDER"),
    ("crates/ccync-engine/src/install.rs", "Runtime"),
    ("crates/ccync-engine/src/install.rs", "RuntimeSelection"),
    ("crates/ccync-engine/src/install.rs", "VALID_RUNTIMES"),
    ("crates/ccync-engine/src/install.rs", "apply_agy_runtime_state"),
    ("crates/ccync-engine/src/install.rs", "default_selected_runtimes"),
    (
        "crates/ccync-engine/src/install.rs",
        "install_default_selected_runtimes_match_projection_fallback_source",
    ),
    ("crates/ccync-engine/src/install.rs", "load_runtime_selection"),
    ("crates/ccync-engine/src/install.rs", "opts_for_runtimes"),
    ("crates/ccync-engine/src/install.rs", "primaryRuntime"),
    ("crates/ccync-engine/src/install.rs", "primary_runtime"),
    ("crates/ccync-engine/src/install.rs", "resolve_runtime_roots"),
    ("crates/ccync-engine/src/install.rs", "resolve_runtime_selection"),
    (
        "crates/ccync-engine/src/install.rs",
        "resolve_runtime_selection_all_blank_keys_does_not_panic_and_falls_back_to_full_default_set",
    ),
    (
        "crates/ccync-engine/src/install.rs",
        "resolve_runtime_selection_all_valid_config_is_unchanged",
    ),
    (
        "crates/ccync-engine/src/install.rs",
        "resolve_runtime_selection_both_keys_present_canonical_values_win",
    ),
    (
        "crates/ccync-engine/src/install.rs",
        "resolve_runtime_selection_empty_config_uses_full_default_set_and_no_warnings",
    ),
    (
        "crates/ccync-engine/src/install.rs",
        "resolve_runtime_selection_filters_invalid_keys_and_keeps_valid_ones",
    ),
    (
        "crates/ccync-engine/src/install.rs",
        "run_update_agy_gui_only_selection_writes_runtime_state_without_junction_apply",
    ),
    (
        "crates/ccync-engine/src/install.rs",
        "run_update_codex_failure_short_circuits_agy_runtime_state_write",
    ),
    (
        "crates/ccync-engine/src/install.rs",
        "run_update_copilot_failure_short_circuits_codex_and_agy_runtime_state",
    ),
    ("crates/ccync-engine/src/install.rs", "runtime"),
    (
        "crates/ccync-engine/src/install.rs",
        "runtime_report_outcomes_excludes_selected_runtime_whose_apply_failed",
    ),
    (
        "crates/ccync-engine/src/install.rs",
        "runtime_report_outcomes_excludes_unselected_copilot",
    ),
    ("crates/ccync-engine/src/install.rs", "runtimes"),
    ("crates/ccync-engine/src/install.rs", "select_runtimes_config"),
    ("crates/ccync-engine/src/install.rs", "selectedRuntimes"),
    ("crates/ccync-engine/src/install.rs", "selected_runtimes"),
    ("crates/ccync-engine/src/install.rs", "stale_dist_runtimes_root"),
    ("crates/ccync-engine/src/install.rs", "write_selected_runtimes_config"),
    ("crates/ccync-engine/src/lib.rs", "runtime"),
    (
        "crates/ccync-engine/src/migrate.rs",
        "fold_config_selected_runtimes_wins_over_old_layout_runtimes",
    ),
    (
        "crates/ccync-engine/src/migrate.rs",
        "fold_old_layout_runtimes_wins_over_install_state_file",
    ),
    (
        "crates/ccync-engine/src/migrate.rs",
        "fold_promotes_old_layout_runtimes_after_apply_migration",
    ),
    ("crates/ccync-engine/src/migrate.rs", "primaryRuntime"),
    ("crates/ccync-engine/src/migrate.rs", "runtime"),
    ("crates/ccync-engine/src/migrate.rs", "runtimes"),
    ("crates/ccync-engine/src/migrate.rs", "selectedRuntimes"),
    ("crates/ccync-engine/src/plugin_upgrade.rs", "runtime"),
    ("crates/ccync-engine/tests/removal.rs", "runtime"),
    ("crates/ccync-engine/tests/removal.rs", "runtimes"),
    ("crates/ccync-engine/tests/removal.rs", "selected_runtimes"),
    ("crates/ccync-engine/tests/removal.rs", "stale_runtimes_dist"),
    ("crates/ccync-foundation/src/config.rs", "LEGACY_OLD_LAYOUT_RUNTIMES_KEY"),
    ("crates/ccync-foundation/src/config.rs", "LEGACY_PRIMARY_RUNTIME_KEY"),
    ("crates/ccync-foundation/src/config.rs", "LEGACY_SELECTED_RUNTIMES_KEY"),
    (
        "crates/ccync-foundation/src/config.rs",
        "empty_selected_runtimes_is_omitted_on_serialize",
    ),
    ("crates/ccync-foundation/src/config.rs", "primaryRuntime"),
    ("crates/ccync-foundation/src/config.rs", "runtime"),
    (
        "crates/ccync-foundation/src/config.rs",
        "runtime_fields_missing_deserialize_to_defaults",
    ),
    ("crates/ccync-foundation/src/config.rs", "runtime_fields_roundtrip"),
    ("crates/ccync-foundation/src/config.rs", "runtimes"),
    ("crates/ccync-foundation/src/config.rs", "selectedRuntimes"),
    ("crates/ccync-foundation/src/config.rs", "selected_runtimes"),
    ("crates/ccync-foundation/src/lib.rs", "runtime"),
    ("crates/projection/src/agy.rs", "runtime"),
    ("crates/projection/src/lib.rs", "runtime"),
    ("crates/projection/src/lib.rs", "runtimes"),
    ("crates/projection/src/lib.rs", "selectedRuntimes"),
    ("crates/projection/src/lib.rs", "selected_runtimes"),
    ("crates/projection/src/support.rs", "runtime"),
    ("crates/projection/src/support.rs", "runtimes"),
    (
        "crates/projection/src/tests.rs",
        "machine_update_selected_runtime_still_gets_command_projection",
    ),
    (
        "crates/projection/src/tests.rs",
        "personalization_runtime_dirs_created_for_selected_runtime",
    ),
    (
        "crates/projection/src/tests.rs",
        "personalization_runtime_dirs_gated_by_selection",
    ),
    (
        "crates/projection/src/tests.rs",
        "planned_machine_touches_excludes_unselected_runtime_paths",
    ),
    ("crates/projection/src/tests.rs", "runtime"),
    ("crates/projection/src/tests.rs", "runtimes"),
    ("crates/projection/src/tests.rs", "selectedRuntimes"),
    ("crates/projection/src/tests.rs", "selected_runtimes"),
    (
        "crates/projection/src/tests.rs",
        "update_commands_cleans_command_skill_when_runtime_deselected",
    ),
    (
        "crates/projection/src/tests.rs",
        "update_commands_removes_managed_outputs_for_unselected_runtimes",
    ),
    ("crates/projection/src/tool_map.rs", "runtime"),
    ("crates/setup/src/lib.rs", "runtime"),
    ("docs/architecture.md", "primaryRuntime"),
    // Official arc42 section title and its exact test fixture, not a product term.
    ("docs/architecture.md", "Runtime"),
    ("crates/ccync-engine/tests/docs_i18n.rs", "Runtime"),
    ("docs/architecture.md", "runtimes"),
    ("docs/architecture.md", "selectedRuntimes"),
    // Legacy config key quoted verbatim where the scoped instruction sync documents conflicting selection keys.
    ("docs/commands.md", "selectedRuntimes"),
    ("docs/i18n/zh-Hant/architecture.zh-Hant.md", "Runtime"),
    ("docs/i18n/zh-Hant/commands.zh-Hant.md", "selectedRuntimes"),
    ("docs/i18n/zh-Hant/setup.zh-Hant.md", "selectedRuntimes"),
    // Legacy config keys remain exact spellings in the setup contract.
    ("docs/setup.md", "runtimes"),
    ("docs/setup.md", "selectedRuntimes"),
    ("docs/release.md", "Runtime"),
    ("docs/release.md", "VCRUNTIME140"),
    ("docs/release.md", "runtime"),
];

/// This guard's own source path, exempted from self-scanning (see the
/// exemption comment in `scan_dir`).
const GUARD_FILE_PATH: &str = "crates/ccync-engine/tests/projection_vocabulary.rs";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Hit {
    path: String,
    token: String,
}

/// Extracts identifier-like tokens from `text` that case-insensitively
/// contain "runtime". A token is a maximal run of ASCII alphanumerics and
/// underscore; any other character (including `-`) ends it.
fn runtime_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars().chain(std::iter::once(' ')) {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            cur.push(ch);
        } else if !cur.is_empty() {
            if cur.to_ascii_lowercase().contains("runtime") {
                out.push(std::mem::take(&mut cur));
            } else {
                cur.clear();
            }
        }
    }
    out
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/ccync-engine has a parent")
        .parent()
        .expect("crates/ has a parent")
        .to_path_buf()
}

fn scan_dir(dir: &Path, root: &Path, exts: &[&str], hits: &mut Vec<Hit>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Never follow a symlink. `path.is_dir()` follows links, so a link to
        // an ancestor would recurse until the stack is exhausted, and a link
        // pointing outside the repo would break `strip_prefix(root)` below and
        // report an absolute host path as a violation. Same non-following
        // posture the prune path uses before it removes anything.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some("target") {
                continue;
            }
            scan_dir(&path, root, exts, hits);
            continue;
        }
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if !exts.contains(&ext) {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if rel == GUARD_FILE_PATH {
            // This guard's own source is exempt: its ALLOWED array
            // necessarily contains every allowed token as string data, and
            // its identifiers (runtime_tokens, ALLOWED, the test names)
            // legitimately describe the guard itself, not projection-domain
            // misuse. Self-scanning it would be circular, not protective.
            continue;
        }
        for token in runtime_tokens(&content) {
            hits.push(Hit {
                path: rel.clone(),
                token,
            });
        }
    }
}

/// Scans `crates/**/*.rs`, `docs/**/*.md`, and root README/CONTRIBUTING under
/// `root` for case-insensitive "runtime" tokens.
fn scan_repo(root: &Path) -> Vec<Hit> {
    let mut hits = Vec::new();
    scan_dir(&root.join("crates"), root, &["rs"], &mut hits);
    scan_dir(&root.join("docs"), root, &["md"], &mut hits);
    for rel in ["README.md", "CONTRIBUTING.md"] {
        if let Ok(content) = fs::read_to_string(root.join(rel)) {
            for token in runtime_tokens(&content) {
                hits.push(Hit {
                    path: rel.to_string(),
                    token,
                });
            }
        }
    }
    hits
}

/// Returns every hit whose (path, token) pair is not in `allowed`.
fn unlisted<'a>(hits: &'a [Hit], allowed: &HashSet<(&str, &str)>) -> Vec<&'a Hit> {
    hits.iter()
        .filter(|h| !allowed.contains(&(h.path.as_str(), h.token.as_str())))
        .collect()
}

/// Conservative floor on the number of tokens a healthy scan must find.
/// The point is not to track the real count -- it is to make a scan that
/// silently found nothing (unreadable root, `repo_root()` landing outside
/// the real checkout) fail loudly instead of reading as a pass. Set well
/// below the current tree's count so ordinary edits never trip it (the tree
/// measured 496 tokens when this floor was set).
const MIN_SCAN_HITS: usize = 200;

/// Fails if any of the four scan roots is missing, which would make the
/// scan a silent no-op.
fn assert_scan_roots_exist(root: &Path) {
    for rel in ["crates", "docs", "README.md", "CONTRIBUTING.md"] {
        let p = root.join(rel);
        assert!(
            p.exists(),
            "vocabulary guard scan root {} is missing at {} -- the scan would be a no-op \
             and every guard below would pass vacuously.",
            rel,
            p.display(),
        );
    }
}

#[test]
fn vocabulary_guard_scan_actually_covers_the_repo() {
    let root = repo_root();
    assert_scan_roots_exist(&root);
    let hits = scan_repo(&root);
    assert!(
        hits.len() > MIN_SCAN_HITS,
        "vocabulary guard scanned {} token(s) under {}, at or below the floor of {} -- \
         the scan did not really cover the repo, so an empty result must not be read as clean.",
        hits.len(),
        root.display(),
        MIN_SCAN_HITS,
    );
}

#[test]
fn vocabulary_guard_matches_embedded_allowlist() {
    let root = repo_root();
    assert_scan_roots_exist(&root);
    let hits = scan_repo(&root);
    assert!(
        hits.len() > MIN_SCAN_HITS,
        "vocabulary guard scanned only {} token(s) -- refusing to report clean on a scan \
         that did not cover the repo.",
        hits.len(),
    );
    let allowed: HashSet<(&str, &str)> = ALLOWED.iter().copied().collect();
    let mut violations: Vec<&Hit> = unlisted(&hits, &allowed);
    violations.sort();
    violations.dedup();

    assert!(
        violations.is_empty(),
        "projection-domain vocabulary guard found {} unlisted 'runtime' use(s); first offender {}: `{}`. \
         Rename it per docs/naming.md's glossary map, or if it is a reviewed exception \
         (legacy config-key boundary, pre-existing out-of-scope identifier, literal CLI quote, \
         or execution/third-party meaning), add its (path, token) pair to ALLOWED in \
         crates/ccync-engine/tests/projection_vocabulary.rs.",
        violations.len(),
        violations[0].path,
        violations[0].token,
    );
}

#[test]
fn vocabulary_guard_detects_seeded_violation() {
    // Proves the detector itself flags an unlisted (path, token) pair,
    // naming both -- independent of the real repo's allowlist size.
    let seeded = [Hit {
        path: "crates/ccync-engine/src/does_not_exist_seed.rs".to_string(),
        token: "SeededRuntimeViolationToken".to_string(),
    }];
    let allowed: HashSet<(&str, &str)> = ALLOWED.iter().copied().collect();
    let violations = unlisted(&seeded, &allowed);

    assert_eq!(violations.len(), 1, "seeded violation was not detected");
    assert_eq!(
        violations[0].path,
        "crates/ccync-engine/src/does_not_exist_seed.rs"
    );
    assert_eq!(violations[0].token, "SeededRuntimeViolationToken");
}

#[test]
fn runtime_tokens_extracts_identifier_like_words_only() {
    let text = "let selectedRuntimes = vec![]; // subRuntime2 stays whole; Runtime-ish splits on the hyphen";
    let tokens = runtime_tokens(text);
    assert!(tokens.contains(&"selectedRuntimes".to_string()));
    assert!(tokens.contains(&"subRuntime2".to_string()));
    assert!(tokens.contains(&"Runtime".to_string()));
    assert!(!tokens.iter().any(|t| t == "Runtime-ish"));
}

#[test]
fn allowed_list_has_no_duplicate_entries() {
    let mut seen = HashSet::new();
    for pair in ALLOWED {
        assert!(seen.insert(pair), "duplicate ALLOWED entry: {pair:?}");
    }
}
