//! One-shot migration shim: old-to-new `~/.ccync/` layout.

use crate::config::{write_projection_selection_at, CcyncConfig, ProjectionTargetResolution};
use ccync_foundation::json_util;
use std::fs;
use std::path::{Path, PathBuf};

// ── layout migration ─────────────────────────────────────────────────────────

/// A single file/directory move to apply during migration.
#[derive(Debug, Clone)]
pub struct LayoutMove {
    pub from: PathBuf,
    pub to: PathBuf,
}

/// Planned migration: a list of layout moves to apply.
#[derive(Debug, Default)]
pub struct MigrationPlan {
    pub moves: Vec<LayoutMove>,
}

/// Scan a ccync home directory and build a migration plan for any old-layout
/// paths that are present. Absent old paths are silently skipped (idempotent).
///
/// Does **not** apply the plan — call [`apply_migration`] for that.
pub fn build_migration_plan(ccync_home: &Path) -> MigrationPlan {
    let mut plan = MigrationPlan::default();

    // `config/config.json` → `config.json`
    let old_config = ccync_home.join("config").join("config.json");
    let new_config = ccync_home.join("config.json");
    if old_config.exists() && !new_config.exists() {
        plan.moves.push(LayoutMove {
            from: old_config,
            to: new_config,
        });
    }

    // `local/catalog.json` → `plugins.json`
    let old_catalog = ccync_home.join("local").join("catalog.json");
    let new_catalog = ccync_home.join("plugins.json");
    if old_catalog.exists() && !new_catalog.exists() {
        plan.moves.push(LayoutMove {
            from: old_catalog,
            to: new_catalog,
        });
    }

    // `state/plugins.lock.json` → `build/lock.json`
    let old_lock = ccync_home.join("state").join("plugins.lock.json");
    let new_lock = ccync_home.join("build").join("lock.json");
    if old_lock.exists() && !new_lock.exists() {
        plan.moves.push(LayoutMove {
            from: old_lock,
            to: new_lock,
        });
    }

    // `generated/mcp/managed.json` → `build/mcp/managed.json`
    let old_mcp = ccync_home
        .join("generated")
        .join("mcp")
        .join("managed.json");
    let new_mcp = ccync_home.join("build").join("mcp").join("managed.json");
    if old_mcp.exists() && !new_mcp.exists() {
        plan.moves.push(LayoutMove {
            from: old_mcp,
            to: new_mcp,
        });
    }

    plan
}

/// Apply a migration plan: move old files to new locations. Idempotent —
/// absent source paths are skipped.
pub fn apply_migration(plan: &MigrationPlan) -> std::io::Result<()> {
    for m in &plan.moves {
        if m.from.exists() && !m.to.exists() {
            if let Some(parent) = m.to.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(&m.from, &m.to)?;
        }
    }
    Ok(())
}

/// Fold the legacy selection sources into `config.json`'s canonical
/// `selectedProjectionTargets` key: the removed `install-state.json`'s
/// `selectedRuntimes`, and the old-layout `runtimes` spelling that
/// [`apply_migration`] moves verbatim out of the old `config/config.json`.
///
/// The data-layout flatten moved the runtime selection out of the removed
/// `install-state.json` and into `config.json`. For an existing install the
/// migration must carry that selection across, or it is silently lost
/// (downstream it falls back to "all targets"). The same holds for a
/// pre-flatten `config/config.json` whose selection lived under `runtimes`:
/// after the verbatim layout move nothing else ever reads that spelling, so
/// this fold is what promotes it.
///
/// Precedence (D-01, normative for this writer and the `init` writer): a
/// legacy value is folded ONLY when `config.json` has no canonical
/// `selectedProjectionTargets` of its own. When `config.json` already
/// carries a selection — canonical, or a legacy spelling of its own — that
/// existing selection wins outright and `install-state.json`'s value goes
/// unused. Among the legacy sources the order is `config.json`
/// `selectedRuntimes`, then `config.json` `runtimes`, then
/// `install-state.json` `selectedRuntimes` — config.json's own record
/// always beats the removed file's, matching the resolver's own
/// `selectedRuntimes`-when-canonical-absent precedence.
/// `install-state.json`'s value is used only when `config.json` has no
/// selection of any kind yet.
///
/// Precedence is decided on the raw `config.json` key map — the same
/// representation [`write_projection_selection_at`] mutates — never on the
/// typed [`CcyncConfig`] struct. A typed load collapses ANY deserialization
/// failure (e.g. one unrelated wrong-typed field elsewhere in the file) into
/// the default struct, which would make a physically present canonical
/// selection look absent and let this fold overwrite it. Reading the raw map
/// keeps the read and write halves of the fold agreed on one view of the
/// file. The typed resolver is used only to parse the selected key's entries
/// (normalization, dedup, rejection), never to decide key presence.
///
/// The write itself goes through [`write_projection_selection_at`], the
/// canonical foundation writer: raw-map based, so unrelated `config.json`
/// keys (`secrets`, `devMode`, `ccyncRoot`, …) survive verbatim, and the
/// write is atomic — one publish of the final map; an injected failure
/// mid-write leaves `config.json` byte-identical to before. Legacy
/// `selectedRuntimes` / `primaryRuntime` keys and the old-layout `runtimes`
/// spelling stop propagating: the writer removes them from `config.json`
/// rather than carrying them forward.
///
/// Invalid selection entries in the promoted legacy source are never
/// destroyed silently: each rejected entry is warned about by name on
/// stderr, and the source record is retained on disk — the legacy file is
/// renamed to `install-state.json.bak` (when it was the source) or the
/// pre-fold `config.json` is copied to `config.json.bak` (when `config.json`
/// 's own legacy key was the source) — so the original selection stays
/// recoverable after the writer strips the legacy keys.
///
/// Idempotent: the legacy file is removed (or renamed to `.bak`) after a
/// successful fold, and the write strips the old-layout `runtimes` spelling
/// from `config.json`, so a second run finds nothing legacy and no-ops.
/// Non-destructive on parse failure: an unreadable or malformed
/// `install-state.json` is left in place and `config.json` is never
/// touched; a malformed `config.json` aborts the fold before anything is
/// deleted.
pub fn fold_install_state_into_config(ccync_home: &Path) -> std::io::Result<()> {
    let install_state = ccync_home.join("install-state.json");
    let state: Option<serde_json::Value> = if install_state.exists() {
        let Ok(state_text) = fs::read_to_string(&install_state) else {
            return Ok(());
        };
        match serde_json::from_str(&state_text) {
            Ok(value) => Some(value),
            // Malformed install-state.json: leave both files untouched.
            Err(_) => return Ok(()),
        }
    } else {
        None
    };

    let config_path = ccync_home.join("config.json");
    let config_map = match json_util::read_json_map(&config_path) {
        Ok(map) => map,
        // A malformed config.json is never a fold target. Surface the error
        // only when legacy state is actually waiting to fold; otherwise stay
        // quiet — config diagnostics belong to the paths that read it.
        Err(e) => return if state.is_some() { Err(e) } else { Ok(()) },
    };

    let old_layout_entries = non_empty_string_entries(config_map.get(OLD_LAYOUT_SELECTION_KEY));
    if state.is_none() && old_layout_entries.is_none() {
        // Nothing legacy to fold.
        return Ok(());
    }

    let mut retain_install_state = false;
    if non_empty_string_entries(config_map.get(CANONICAL_SELECTION_KEY)).is_none() {
        // No canonical selection in config.json yet — promote whichever
        // legacy source applies: config.json's own legacy spellings win
        // when present (newer spelling first); otherwise fall back to
        // install-state.json's legacy field. All are parsed and validated
        // the same way.
        let (entries, source_key, source_is_config) =
            if let Some(entries) = non_empty_string_entries(config_map.get(LEGACY_SELECTION_KEY)) {
                (entries, LEGACY_SELECTION_KEY, true)
            } else if let Some(entries) = old_layout_entries {
                (entries, OLD_LAYOUT_SELECTION_KEY, true)
            } else {
                (
                    state
                        .as_ref()
                        .and_then(|s| non_empty_string_entries(s.get(LEGACY_SELECTION_KEY)))
                        .unwrap_or_default(),
                    LEGACY_SELECTION_KEY,
                    false,
                )
            };
        let resolution = parse_selection_entries(entries);

        if !resolution.rejected.is_empty() {
            let source_label = if source_is_config {
                "~/.ccync/config.json"
            } else {
                "~/.ccync/install-state.json"
            };
            warn_rejected_selection_entries(source_label, source_key, &resolution.rejected);
            // Retain the source record so the original selection stays
            // recoverable: back up config.json before the writer strips its
            // legacy keys, or keep the legacy file as a .bak instead of
            // deleting it.
            if source_is_config {
                fs::copy(&config_path, ccync_home.join("config.json.bak"))?;
            } else {
                retain_install_state = true;
            }
        }

        write_projection_selection_at(&config_path, &resolution.targets)?;
    }

    if state.is_some() {
        if retain_install_state {
            fs::rename(&install_state, ccync_home.join("install-state.json.bak"))?;
        } else {
            fs::remove_file(&install_state)?;
        }
    }
    Ok(())
}

/// Warn on stderr, naming each legacy selection entry that did not parse as
/// a known projection target — mirroring the named-invalid-key warning the
/// runtime-selection read path already emits — so entries are never dropped
/// without a user-visible trace.
fn warn_rejected_selection_entries(source_label: &str, source_key: &str, rejected: &[String]) {
    let valid: Vec<&str> = ccync_foundation::target::CANONICAL_PROJECTION_TARGETS
        .iter()
        .map(|t| t.as_str())
        .collect();
    eprintln!(
        "ccync: ignoring invalid runtime key(s) in {source_label} `{source_key}` during migration: {} — valid: {}",
        rejected.join(", "),
        valid.join(", ")
    );
}

/// Canonical selection key in `config.json`.
const CANONICAL_SELECTION_KEY: &str = "selectedProjectionTargets";
/// Legacy selection key, in both `config.json` and `install-state.json`.
const LEGACY_SELECTION_KEY: &str = "selectedRuntimes";
/// Old-layout selection spelling from the pre-flatten `config/config.json`,
/// carried verbatim into `config.json` by [`apply_migration`].
const OLD_LAYOUT_SELECTION_KEY: &str = "runtimes";

/// Extract a JSON value's string entries when it is a non-empty array with at
/// least one string element; `None` otherwise. "Present" for the fold's
/// precedence means exactly this — matching the crate convention that an
/// absent key and an empty array are indistinguishable.
fn non_empty_string_entries(value: Option<&serde_json::Value>) -> Option<Vec<String>> {
    let entries: Vec<String> = value?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str())
        .map(str::to_owned)
        .collect();
    if entries.is_empty() {
        None
    } else {
        Some(entries)
    }
}

/// Parse raw selection entries through the shared resolver so the fold gets
/// the same normalization, dedup, and rejection reporting as every other
/// read path — used for entry parsing only, never for key-presence decisions.
fn parse_selection_entries(entries: Vec<String>) -> ProjectionTargetResolution {
    CcyncConfig {
        selected_projection_targets: entries,
        ..CcyncConfig::default()
    }
    .resolve_projection_targets()
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn migration_plan_maps_old_paths_to_new() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");

        // Create old layout files.
        fs::create_dir_all(ccync.join("config")).unwrap();
        fs::write(
            ccync.join("config").join("config.json"),
            r#"{"runtimes":["claude"]}"#,
        )
        .unwrap();

        fs::create_dir_all(ccync.join("local")).unwrap();
        fs::write(
            ccync.join("local").join("catalog.json"),
            r#"{"plugins":[]}"#,
        )
        .unwrap();

        fs::create_dir_all(ccync.join("state")).unwrap();
        fs::write(
            ccync.join("state").join("plugins.lock.json"),
            r#"{"resolvedPlugins":[]}"#,
        )
        .unwrap();

        fs::create_dir_all(ccync.join("generated").join("mcp")).unwrap();
        fs::write(
            ccync.join("generated").join("mcp").join("managed.json"),
            r#"{"servers":{}}"#,
        )
        .unwrap();

        let plan = build_migration_plan(&ccync);

        assert_eq!(
            plan.moves.len(),
            4,
            "all four old-layout paths must appear in plan"
        );
        assert!(plan
            .moves
            .iter()
            .any(|m| m.from.ends_with("config/config.json") && m.to.ends_with("config.json")));
        assert!(plan
            .moves
            .iter()
            .any(|m| m.from.ends_with("catalog.json") && m.to.ends_with("plugins.json")));
        assert!(plan
            .moves
            .iter()
            .any(|m| m.from.ends_with("plugins.lock.json") && m.to.ends_with("lock.json")));
        assert!(plan.moves.iter().any(|m| m.from.ends_with("managed.json")
            && m.to.ends_with("managed.json")
            && m.to.to_string_lossy().contains("build")));
    }

    #[test]
    fn apply_migration_moves_old_config_verbatim() {
        // apply_migration alone is a pure move: the old-layout selection
        // spellings arrive in config.json byte-for-byte. Promoting them to
        // the canonical key is the fold's job, exercised separately below.
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(ccync.join("config")).unwrap();
        let payload = r#"{"runtimes":["claude"],"primaryRuntime":"claude"}"#;
        fs::write(ccync.join("config").join("config.json"), payload).unwrap();

        let plan = build_migration_plan(&ccync);
        apply_migration(&plan).unwrap();

        let migrated = fs::read_to_string(ccync.join("config.json")).unwrap();
        assert_eq!(
            migrated, payload,
            "the layout move itself must preserve bytes verbatim"
        );
    }

    #[test]
    fn fold_promotes_old_layout_runtimes_after_apply_migration() {
        // End-to-end old layout: apply_migration moves config/config.json
        // verbatim, then the fold — with no install-state.json anywhere —
        // promotes the old `runtimes` spelling to the canonical key and
        // strips every legacy spelling from the write.
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(ccync.join("config")).unwrap();
        fs::write(
            ccync.join("config").join("config.json"),
            r#"{"runtimes":["codex","claude"],"primaryRuntime":"claude","secrets":{"TOK":"abc"}}"#,
        )
        .unwrap();

        let plan = build_migration_plan(&ccync);
        apply_migration(&plan).unwrap();
        fold_install_state_into_config(&ccync).unwrap();

        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(ccync.join("config.json")).unwrap()).unwrap();
        assert_eq!(
            merged["selectedProjectionTargets"],
            serde_json::json!(["codex", "claude"]),
            "the old-layout selection must be promoted in its on-disk order"
        );
        assert!(
            merged.get("runtimes").is_none(),
            "the old-layout spelling must be stripped"
        );
        assert!(merged.get("primaryRuntime").is_none());
        assert_eq!(merged["secrets"]["TOK"], serde_json::json!("abc"));

        // Idempotent: the spelling is gone, so a second fold no-ops.
        let after_first = fs::read_to_string(ccync.join("config.json")).unwrap();
        fold_install_state_into_config(&ccync).unwrap();
        assert_eq!(
            fs::read_to_string(ccync.join("config.json")).unwrap(),
            after_first
        );
    }

    #[test]
    fn fold_config_selected_runtimes_wins_over_old_layout_runtimes() {
        // Among config.json's own legacy spellings the newer one wins, and
        // both are stripped by the single canonical write.
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync).unwrap();
        fs::write(
            ccync.join("config.json"),
            r#"{"selectedRuntimes":["claude"],"runtimes":["codex"]}"#,
        )
        .unwrap();

        fold_install_state_into_config(&ccync).unwrap();

        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(ccync.join("config.json")).unwrap()).unwrap();
        assert_eq!(
            merged["selectedProjectionTargets"],
            serde_json::json!(["claude"])
        );
        assert!(merged.get("selectedRuntimes").is_none());
        assert!(merged.get("runtimes").is_none());
    }

    #[test]
    fn fold_old_layout_runtimes_wins_over_install_state_file() {
        // config.json's own record — even in the old-layout spelling — beats
        // the removed install-state.json's value.
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync).unwrap();
        fs::write(ccync.join("config.json"), r#"{"runtimes":["claude"]}"#).unwrap();
        fs::write(
            ccync.join("install-state.json"),
            r#"{"selectedRuntimes":["codex"]}"#,
        )
        .unwrap();

        fold_install_state_into_config(&ccync).unwrap();

        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(ccync.join("config.json")).unwrap()).unwrap();
        assert_eq!(
            merged["selectedProjectionTargets"],
            serde_json::json!(["claude"])
        );
        assert!(merged.get("runtimes").is_none());
        assert!(!ccync.join("install-state.json").exists());
    }

    #[test]
    fn fold_invalid_old_layout_entries_promote_valid_subset_and_keep_config_backup() {
        // The old-layout spelling gets the same rejected-entry contract:
        // named warning, valid subset promoted, pre-fold config.json backed
        // up so the original selection stays recoverable.
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync).unwrap();
        let original_config = r#"{"runtimes":["claude","windsurf"]}"#;
        fs::write(ccync.join("config.json"), original_config).unwrap();

        fold_install_state_into_config(&ccync).unwrap();

        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(ccync.join("config.json")).unwrap()).unwrap();
        assert_eq!(
            merged["selectedProjectionTargets"],
            serde_json::json!(["claude"])
        );
        assert!(merged.get("runtimes").is_none());
        assert_eq!(
            fs::read_to_string(ccync.join("config.json.bak")).unwrap(),
            original_config,
            "the pre-fold config.json must be backed up when old-layout entries were rejected"
        );
    }

    #[test]
    fn migration_is_idempotent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(ccync.join("config")).unwrap();
        fs::write(ccync.join("config").join("config.json"), r#"{"v":1}"#).unwrap();

        let plan1 = build_migration_plan(&ccync);
        apply_migration(&plan1).unwrap();

        // Second call: old path gone, new path exists → plan is empty.
        let plan2 = build_migration_plan(&ccync);
        assert!(
            plan2.moves.is_empty(),
            "second migration plan must be empty (idempotent)"
        );
        apply_migration(&plan2).unwrap(); // must not error
    }

    #[test]
    fn fold_install_state_legacy_only_promotes_to_canonical_and_preserves_secrets() {
        // legacy-only: config.json has no selection of any kind, so
        // install-state.json's legacy selectedRuntimes is what gets promoted.
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync).unwrap();
        // config.json carries secrets + devMode but no runtime selection yet.
        fs::write(
            ccync.join("config.json"),
            r#"{"secrets":{"TOK":"abc"},"devMode":false}"#,
        )
        .unwrap();
        fs::write(
            ccync.join("install-state.json"),
            r#"{"selectedRuntimes":["claude","codex"],"primaryRuntime":"codex"}"#,
        )
        .unwrap();

        fold_install_state_into_config(&ccync).unwrap();

        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(ccync.join("config.json")).unwrap()).unwrap();
        assert_eq!(
            merged["selectedProjectionTargets"],
            serde_json::json!(["claude", "codex"])
        );
        assert!(
            merged.get("selectedRuntimes").is_none(),
            "legacy selectedRuntimes must stop propagating into config.json"
        );
        assert!(
            merged.get("primaryRuntime").is_none(),
            "legacy primaryRuntime must stop propagating into config.json"
        );
        assert_eq!(
            merged["secrets"]["TOK"],
            serde_json::json!("abc"),
            "unrelated config keys must survive the fold"
        );
        assert!(
            !ccync.join("install-state.json").exists(),
            "legacy install-state.json must be removed after a successful fold"
        );

        // Idempotent: second run is a no-op (install-state already gone).
        fold_install_state_into_config(&ccync).unwrap();
    }

    #[test]
    fn fold_install_state_precedence_canonical_config_selection_wins_unread() {
        // precedence: config.json already has a canonical selection —
        // install-state.json's (different) legacy value must never be read.
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync).unwrap();
        fs::write(
            ccync.join("config.json"),
            r#"{"selectedProjectionTargets":["claude"]}"#,
        )
        .unwrap();
        fs::write(
            ccync.join("install-state.json"),
            r#"{"selectedRuntimes":["codex"]}"#,
        )
        .unwrap();

        fold_install_state_into_config(&ccync).unwrap();

        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(ccync.join("config.json")).unwrap()).unwrap();
        assert_eq!(
            merged["selectedProjectionTargets"],
            serde_json::json!(["claude"]),
            "an existing canonical config.json selection must win over the legacy install-state file"
        );
        assert!(
            !ccync.join("install-state.json").exists(),
            "the now-superseded install-state.json is still cleaned up"
        );
    }

    #[test]
    fn fold_keeps_canonical_selection_when_unrelated_field_is_wrong_typed() {
        // Regression: precedence must be decided on the raw key map. A typed
        // load would fail wholesale on the wrong-typed `devMode` (string, not
        // bool), report the canonical key as absent, and let install-state's
        // stale selection overwrite the user's real one. The canonical
        // selection must survive byte-identically instead.
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync).unwrap();
        let original =
            r#"{"selectedProjectionTargets":["claude"],"devMode":"true","secrets":{"TOK":"abc"}}"#;
        fs::write(ccync.join("config.json"), original).unwrap();
        fs::write(
            ccync.join("install-state.json"),
            r#"{"selectedRuntimes":["codex"]}"#,
        )
        .unwrap();

        fold_install_state_into_config(&ccync).unwrap();

        assert_eq!(
            fs::read_to_string(ccync.join("config.json")).unwrap(),
            original,
            "a physically present canonical selection must survive byte-identically even when an unrelated field is wrong-typed"
        );
        assert!(
            !ccync.join("install-state.json").exists(),
            "the unused install-state.json is still cleaned up"
        );
    }

    #[test]
    fn fold_install_state_own_legacy_selection_wins_over_install_state_file() {
        // config.json's own legacy selectedRuntimes (no canonical yet) wins
        // over install-state.json's differing value, and is what gets
        // promoted to canonical.
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync).unwrap();
        fs::write(
            ccync.join("config.json"),
            r#"{"selectedRuntimes":["claude"]}"#,
        )
        .unwrap();
        fs::write(
            ccync.join("install-state.json"),
            r#"{"selectedRuntimes":["codex"]}"#,
        )
        .unwrap();

        fold_install_state_into_config(&ccync).unwrap();

        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(ccync.join("config.json")).unwrap()).unwrap();
        assert_eq!(
            merged["selectedProjectionTargets"],
            serde_json::json!(["claude"]),
            "config.json's own legacy selection must win over the legacy install-state file"
        );
        assert!(merged.get("selectedRuntimes").is_none());
    }

    #[test]
    fn fold_invalid_install_state_entries_promote_valid_subset_and_keep_backup() {
        // Unparseable entries in the promoted install-state selection must
        // not vanish without a trace: the valid subset is promoted and the
        // source file is retained as a .bak so the original selection stays
        // recoverable. (The named-entry stderr warning is emitted on the
        // same path.)
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync).unwrap();
        fs::write(ccync.join("config.json"), r#"{"devMode":false}"#).unwrap();
        let original_state = r#"{"selectedRuntimes":["claude","windsurf"]}"#;
        fs::write(ccync.join("install-state.json"), original_state).unwrap();

        fold_install_state_into_config(&ccync).unwrap();

        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(ccync.join("config.json")).unwrap()).unwrap();
        assert_eq!(
            merged["selectedProjectionTargets"],
            serde_json::json!(["claude"]),
            "the valid subset must still be promoted"
        );
        assert!(
            !ccync.join("install-state.json").exists(),
            "the folded install-state.json must not stay under its live name"
        );
        assert_eq!(
            fs::read_to_string(ccync.join("install-state.json.bak")).unwrap(),
            original_state,
            "the source record must be retained as a .bak when entries were rejected"
        );

        // Idempotent: the live legacy file is gone, so a second run no-ops
        // and the .bak survives.
        fold_install_state_into_config(&ccync).unwrap();
        assert!(ccync.join("install-state.json.bak").exists());
    }

    #[test]
    fn fold_invalid_config_legacy_entries_promote_valid_subset_and_keep_config_backup() {
        // Same rejected-entry contract for config.json's own legacy key: the
        // writer strips that key, so the pre-fold config.json is backed up
        // before the write.
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync).unwrap();
        let original_config =
            r#"{"selectedRuntimes":["claude","windsurf"],"secrets":{"TOK":"abc"}}"#;
        fs::write(ccync.join("config.json"), original_config).unwrap();
        fs::write(
            ccync.join("install-state.json"),
            r#"{"selectedRuntimes":["codex"]}"#,
        )
        .unwrap();

        fold_install_state_into_config(&ccync).unwrap();

        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(ccync.join("config.json")).unwrap()).unwrap();
        assert_eq!(
            merged["selectedProjectionTargets"],
            serde_json::json!(["claude"])
        );
        assert_eq!(
            merged["secrets"]["TOK"],
            serde_json::json!("abc"),
            "secrets still survive the fold"
        );
        assert_eq!(
            fs::read_to_string(ccync.join("config.json.bak")).unwrap(),
            original_config,
            "the pre-fold config.json must be backed up when its own legacy entries were rejected"
        );
        assert!(
            !ccync.join("install-state.json").exists(),
            "the unused install-state.json is still removed"
        );
    }

    #[test]
    fn fold_install_state_malformed_file_leaves_config_untouched() {
        // injected-failure byte equality: a malformed install-state.json is
        // left in place, non-destructively, and config.json is never touched.
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync).unwrap();
        let original = r#"{"selectedProjectionTargets":["claude"],"secrets":{"TOK":"abc"}}"#;
        fs::write(ccync.join("config.json"), original).unwrap();
        fs::write(ccync.join("install-state.json"), "{ not valid json").unwrap();

        fold_install_state_into_config(&ccync).unwrap();

        assert_eq!(
            fs::read_to_string(ccync.join("config.json")).unwrap(),
            original,
            "config.json must be byte-identical when the fold source is unparseable"
        );
        assert!(
            ccync.join("install-state.json").exists(),
            "a malformed install-state.json must be left in place, not silently deleted"
        );
    }
}
