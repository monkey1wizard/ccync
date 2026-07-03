//! One-shot migration shim: old-to-new `~/.ccync/` layout.

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
        plan.moves.push(LayoutMove { from: old_config, to: new_config });
    }

    // `local/catalog.json` → `plugins.json`
    let old_catalog = ccync_home.join("local").join("catalog.json");
    let new_catalog = ccync_home.join("plugins.json");
    if old_catalog.exists() && !new_catalog.exists() {
        plan.moves.push(LayoutMove { from: old_catalog, to: new_catalog });
    }

    // `state/plugins.lock.json` → `build/lock.json`
    let old_lock = ccync_home.join("state").join("plugins.lock.json");
    let new_lock = ccync_home.join("build").join("lock.json");
    if old_lock.exists() && !new_lock.exists() {
        plan.moves.push(LayoutMove { from: old_lock, to: new_lock });
    }

    // `generated/mcp/managed.json` → `build/mcp/managed.json`
    let old_mcp = ccync_home.join("generated").join("mcp").join("managed.json");
    let new_mcp = ccync_home.join("build").join("mcp").join("managed.json");
    if old_mcp.exists() && !new_mcp.exists() {
        plan.moves.push(LayoutMove { from: old_mcp, to: new_mcp });
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

/// Fold the legacy `install-state.json` runtime selection into `config.json`.
///
/// The data-layout flatten moved `selectedRuntimes` / `primaryRuntime` out of
/// the removed `install-state.json` and into `config.json`. For an existing install
/// the migration must carry those values across, or the user's runtime
/// selection is silently lost (downstream it falls back to "all runtimes").
///
/// The merge runs on the raw JSON map so unrelated `config.json` keys (e.g.
/// `secrets`, `devMode`, `ccyncRoot`) survive verbatim — round-tripping through
/// a typed struct would drop fields it does not model. Existing `config.json`
/// runtime fields win (never clobbered). Idempotent: the legacy file is removed
/// after a successful fold, so a second run is a no-op. Non-destructive on parse
/// failure: an unreadable/malformed `install-state.json` is left in place.
pub fn fold_install_state_into_config(ccync_home: &Path) -> std::io::Result<()> {
    let install_state = ccync_home.join("install-state.json");
    if !install_state.exists() {
        return Ok(());
    }
    let Ok(state_text) = fs::read_to_string(&install_state) else {
        return Ok(());
    };
    let Ok(state) = serde_json::from_str::<serde_json::Value>(&state_text) else {
        return Ok(());
    };

    let config_path = ccync_home.join("config.json");
    let mut config: serde_json::Map<String, serde_json::Value> =
        match fs::read_to_string(&config_path) {
            Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default(),
            Err(_) => serde_json::Map::new(),
        };

    if !config.contains_key("selectedRuntimes") {
        if let Some(value) = state.get("selectedRuntimes") {
            config.insert("selectedRuntimes".to_string(), value.clone());
        }
    }
    if !config.contains_key("primaryRuntime") {
        if let Some(value) = state.get("primaryRuntime") {
            config.insert("primaryRuntime".to_string(), value.clone());
        }
    }

    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let serialized = serde_json::to_string_pretty(&serde_json::Value::Object(config))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    fs::write(&config_path, serialized)?;
    fs::remove_file(&install_state)?;
    Ok(())
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
        fs::write(ccync.join("config").join("config.json"), r#"{"runtimes":["claude"]}"#).unwrap();

        fs::create_dir_all(ccync.join("local")).unwrap();
        fs::write(ccync.join("local").join("catalog.json"), r#"{"plugins":[]}"#).unwrap();

        fs::create_dir_all(ccync.join("state")).unwrap();
        fs::write(ccync.join("state").join("plugins.lock.json"), r#"{"resolvedPlugins":[]}"#).unwrap();

        fs::create_dir_all(ccync.join("generated").join("mcp")).unwrap();
        fs::write(ccync.join("generated").join("mcp").join("managed.json"), r#"{"servers":{}}"#).unwrap();

        let plan = build_migration_plan(&ccync);

        assert_eq!(plan.moves.len(), 4, "all four old-layout paths must appear in plan");
        assert!(plan.moves.iter().any(|m| m.from.ends_with("config/config.json") && m.to.ends_with("config.json")));
        assert!(plan.moves.iter().any(|m| m.from.ends_with("catalog.json") && m.to.ends_with("plugins.json")));
        assert!(plan.moves.iter().any(|m| m.from.ends_with("plugins.lock.json") && m.to.ends_with("lock.json")));
        assert!(plan.moves.iter().any(|m| m.from.ends_with("managed.json") && m.to.ends_with("managed.json") && m.to.to_string_lossy().contains("build")));
    }

    #[test]
    fn migration_preserves_file_contents() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(ccync.join("config")).unwrap();
        let payload = r#"{"runtimes":["claude"],"primaryRuntime":"claude"}"#;
        fs::write(ccync.join("config").join("config.json"), payload).unwrap();

        let plan = build_migration_plan(&ccync);
        apply_migration(&plan).unwrap();

        let migrated = fs::read_to_string(ccync.join("config.json")).unwrap();
        assert_eq!(migrated, payload, "INPUT/runtime fields must be preserved verbatim");
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
        assert!(plan2.moves.is_empty(), "second migration plan must be empty (idempotent)");
        apply_migration(&plan2).unwrap(); // must not error
    }

    #[test]
    fn fold_install_state_merges_runtime_fields_and_preserves_secrets() {
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
        assert_eq!(merged["selectedRuntimes"], serde_json::json!(["claude", "codex"]));
        assert_eq!(merged["primaryRuntime"], serde_json::json!("codex"));
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
    fn fold_install_state_does_not_clobber_existing_config_selection() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync).unwrap();
        fs::write(ccync.join("config.json"), r#"{"selectedRuntimes":["claude"]}"#).unwrap();
        fs::write(
            ccync.join("install-state.json"),
            r#"{"selectedRuntimes":["codex"]}"#,
        )
        .unwrap();

        fold_install_state_into_config(&ccync).unwrap();

        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(ccync.join("config.json")).unwrap()).unwrap();
        assert_eq!(
            merged["selectedRuntimes"],
            serde_json::json!(["claude"]),
            "an existing config.json selection must win over the legacy file"
        );
    }

}
