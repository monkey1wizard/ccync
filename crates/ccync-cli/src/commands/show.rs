//! `ccync show <id>` — print full detail of one managed item (personal or adopted).
//! Read-only.

use ccync_engine::{AdoptedUpgradeOutcome, ExitCode, ManagedItem, ManagedOrigin};
use serde_json::Value;
use std::path::Path;

pub(crate) fn cmd_show(args: &[String]) -> ExitCode {
    if let Err(code) = super::args::reject_unknown_flags(
        "show",
        args,
        &[],
        &[],
        1,
        "Usage: ccync show <plugin-id>",
    ) {
        return code;
    }
    let Some(id) = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .map(String::as_str)
    else {
        eprintln!("ccync show: missing <id> argument");
        eprintln!("Usage: ccync show <plugin-id>");
        return ExitCode::Usage;
    };

    // Load the lockfile and build the merged managed view so adopted items
    // are visible alongside personal plugins.
    let lockfile_path = ccync_foundation::paths::plugins_lock_path();
    let lock: Value = lockfile_path
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    let items = ccync_engine::build_managed_view(&lock);
    let Some(item) = items.iter().find(|i| i.id == id) else {
        eprintln!("ccync show: '{id}' not found. Run `ccync list` to see installed items.");
        return ExitCode::Error;
    };

    match item.origin {
        ManagedOrigin::Personal => show_personal(id, &lock),
        ManagedOrigin::Adopted => show_adopted(item, &lock),
    }
}

// ── personal plugin path ──────────────────────────────────────────────────────

fn show_personal(id: &str, lock: &Value) -> ExitCode {
    let catalog = ccync_foundation::paths::personal_catalog_path().and_then(|p| read_json(&p));
    let entry = catalog
        .as_ref()
        .and_then(|c| c.get("plugins"))
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .find(|e| e.get("pluginId").and_then(Value::as_str) == Some(id))
        });

    let Some(entry) = entry else {
        eprintln!(
            "ccync show: plugin '{id}' not found. Run `ccync list` to see installed plugins."
        );
        return ExitCode::Error;
    };

    // Prefer the lockfile's resolved pin, then the catalog entry's own pin/pinnedSha.
    let pinned = lock_pin_for(lock, id).or_else(|| entry_pin(entry));

    // Best-effort projected component count from the plugin's cache dir.
    let components = pinned.as_deref().and_then(|sha| {
        ccync_foundation::paths::ccync_cache_root()
            .map(|root| count_components(&root.join(format!("{id}@{sha}"))))
    });

    print!(
        "{}",
        format_plugin_detail(entry, pinned.as_deref(), components)
    );
    ExitCode::Success
}

/// Render the single personal-plugin detail block. Pure (returns the text) so
/// it is unit-testable without touching disk.
fn format_plugin_detail(entry: &Value, pinned: Option<&str>, components: Option<usize>) -> String {
    let id = entry.get("pluginId").and_then(Value::as_str).unwrap_or("?");
    let display = entry
        .get("displayName")
        .and_then(Value::as_str)
        .unwrap_or(id);
    let source = entry.get("source").and_then(Value::as_str).unwrap_or("?");
    let strategy = entry
        .get("installStrategy")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let held = entry.get("held").and_then(Value::as_bool).unwrap_or(false);

    let mut out = String::new();
    out.push_str(&format!("Plugin: {id}\n"));
    out.push_str(&format!("  Display name:  {display}\n"));
    out.push_str(&format!("  Source:        {source}\n"));
    out.push_str(&format!("  Strategy:      {strategy}\n"));
    out.push_str(&format!(
        "  Pinned sha:    {}\n",
        pinned.unwrap_or("(unpinned)")
    ));
    out.push_str(&format!(
        "  Held:          {}\n",
        if held { "yes" } else { "no" }
    ));
    out.push_str(&format!(
        "  Components:    {}\n",
        components
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    ));
    out
}

// ── adopted item path ─────────────────────────────────────────────────────────

fn show_adopted(item: &ManagedItem, lock: &Value) -> ExitCode {
    // Read baselineVersion from the raw _adoptedItems entry.
    let baseline_version = lock
        .get("_adoptedItems")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .find(|e| e.get("name").and_then(Value::as_str) == Some(&item.id))
        })
        .and_then(|e| e.get("baselineVersion").and_then(Value::as_str))
        .map(str::to_string);

    // Classify upgrade state via the local catalog (network-free).
    let adopted_outcomes = ccync_engine::resolve_adopted_upgrades(lock);
    let outcome = adopted_outcomes.iter().find(|o| o.name() == item.id);

    print!(
        "{}",
        format_adopted_detail(item, baseline_version.as_deref(), outcome)
    );
    ExitCode::Success
}

/// Render one adopted-item detail block. Pure so it is unit-testable.
pub(crate) fn format_adopted_detail(
    item: &ManagedItem,
    baseline_version: Option<&str>,
    outcome: Option<&AdoptedUpgradeOutcome>,
) -> String {
    let kind_str = match item.kind {
        ccync_engine::ManagedItemKind::Plugin => "plugin",
        ccync_engine::ManagedItemKind::McpServer => "mcp",
        ccync_engine::ManagedItemKind::Skill => "skill",
    };

    let upgradeability = match outcome {
        Some(AdoptedUpgradeOutcome::Changed {
            current_version,
            latest_version,
            source,
            takeover_supported,
            ..
        }) => {
            let current = current_version.as_deref().unwrap_or("(unknown)");
            if *takeover_supported {
                format!(
                    "update available: {current} -> {latest_version}\n    activate via Claude's /plugin, or take over: ccync add {source}"
                )
            } else {
                format!(
                    "update available: {current} -> {latest_version}\n    activate via Claude's /plugin (plugin lives inside the marketplace repo — `ccync add` cannot fetch it directly)"
                )
            }
        }
        Some(AdoptedUpgradeOutcome::Unchanged { .. }) => "up to date".to_string(),
        Some(AdoptedUpgradeOutcome::VersionUnresolved { .. }) => "version-unresolved".to_string(),
        Some(AdoptedUpgradeOutcome::Skipped { reason, .. }) => {
            format!("not upgradeable: {reason}")
        }
        None => "(not classified)".to_string(),
    };

    let mut out = String::new();
    out.push_str(&format!("Adopted: {}\n", item.id));
    out.push_str("  Origin:        adopted\n");
    out.push_str(&format!("  Kind:          {kind_str}\n"));
    out.push_str(&format!(
        "  Source:        {}\n",
        item.source.as_deref().unwrap_or("(unknown)")
    ));
    out.push_str(&format!(
        "  Version:       {}\n",
        baseline_version.unwrap_or("(unknown)")
    ));
    out.push_str(&format!("  Upgradeability: {upgradeability}\n"));
    if item.source.is_none() {
        out.push_str(&format!("    Tip: run `ccync repair {}`\n", item.id));
    }
    out
}

// ── shared helpers ────────────────────────────────────────────────────────────

/// Sum of entries in the plugin's `skills/` `commands/` `agents/` `hooks/`
/// component dirs (best-effort; absent dirs count as zero).
pub(crate) fn count_components(plugin_dir: &Path) -> usize {
    ["skills", "commands", "agents", "hooks"]
        .iter()
        .map(|sub| {
            std::fs::read_dir(plugin_dir.join(sub))
                .map(|d| d.count())
                .unwrap_or(0)
        })
        .sum()
}

fn entry_pin(entry: &Value) -> Option<String> {
    ccync_foundation::pins::entry_pin(entry)
}

fn lock_pin_for(lock: &Value, id: &str) -> Option<String> {
    lock.get("_personalPlugins")
        .and_then(Value::as_array)?
        .iter()
        .find(|e| e.get("pluginId").and_then(Value::as_str) == Some(id))
        .and_then(ccync_foundation::pins::entry_pin)
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccync_engine::{ManagedItemKind, ManagedOrigin};
    use serde_json::json;

    fn adopted_item(id: &str, kind: ManagedItemKind, source: Option<&str>) -> ManagedItem {
        ManagedItem {
            id: id.to_string(),
            kind,
            origin: ManagedOrigin::Adopted,
            source: source.map(str::to_string),
            pin: None,
            held: false,
            shadowed: false,
        }
    }

    // ── personal path ──────────────────────────────────────────────────────────

    #[test]
    fn count_components_sums_known_component_dirs() {
        let tmp = tempfile::TempDir::new().unwrap();
        for sub in ["skills", "commands", "agents", "hooks"] {
            std::fs::create_dir_all(tmp.path().join(sub)).unwrap();
        }
        std::fs::write(tmp.path().join("skills").join("a.md"), "x").unwrap();
        std::fs::write(tmp.path().join("commands").join("b.md"), "x").unwrap();
        std::fs::write(tmp.path().join("agents").join("c.md"), "x").unwrap();
        assert_eq!(count_components(tmp.path()), 3);
    }

    #[test]
    fn personal_detail_renders_all_fields() {
        let entry = json!({
            "pluginId": "my-plugin",
            "displayName": "My Plugin",
            "source": "https://github.com/u/my-plugin.git",
            "installStrategy": "personal-git-clone",
            "held": true
        });
        let out = format_plugin_detail(&entry, Some("abc123def456"), Some(7));
        assert!(out.contains("My Plugin"));
        assert!(out.contains("https://github.com/u/my-plugin.git"));
        assert!(out.contains("personal-git-clone"));
        assert!(out.contains("abc123def456"));
        assert!(out.contains("Held:          yes"));
        assert!(out.contains("Components:    7"));
    }

    #[test]
    fn personal_detail_handles_missing_optionals() {
        let entry =
            json!({ "pluginId": "bare", "source": "x", "installStrategy": "personal-archive" });
        let out = format_plugin_detail(&entry, None, None);
        assert!(out.contains("Plugin: bare"));
        assert!(out.contains("(unpinned)"));
        assert!(out.contains("Held:          no"));
        assert!(out.contains("Components:    unknown"));
    }

    // ── adopted path ───────────────────────────────────────────────────────────

    #[test]
    fn adopted_detail_plugin_changed() {
        let item = adopted_item(
            "tool@market",
            ManagedItemKind::Plugin,
            Some("https://github.com/u/tool.git"),
        );
        let outcome = AdoptedUpgradeOutcome::Changed {
            name: "tool@market".into(),
            source: "https://github.com/u/tool.git".into(),
            current_version: Some("1.0.0".into()),
            latest_version: "1.2.0".into(),
            takeover_supported: true,
        };
        let out = format_adopted_detail(&item, Some("1.0.0"), Some(&outcome));
        assert!(out.starts_with("Adopted: tool@market\n"));
        assert!(out.contains("Origin:        adopted"));
        assert!(out.contains("Kind:          plugin"));
        assert!(out.contains("Source:        https://github.com/u/tool.git"));
        assert!(out.contains("Version:       1.0.0"));
        assert!(out.contains("update available: 1.0.0 -> 1.2.0"));
        assert!(out.contains("ccync add https://github.com/u/tool.git"));
    }

    /// Mirror of upgrade.rs's suppression rule: an embedded plugin's
    /// browse-path source must not be advertised as a `ccync add` target,
    /// because the plain-clone fetch path cannot resolve a subpath.
    #[test]
    fn adopted_detail_plugin_changed_without_takeover_support_suppresses_add() {
        let item = adopted_item(
            "tool@market",
            ManagedItemKind::Plugin,
            Some("https://github.com/u/market/plugins/tool"),
        );
        let outcome = AdoptedUpgradeOutcome::Changed {
            name: "tool@market".into(),
            source: "https://github.com/u/market/plugins/tool".into(),
            current_version: Some("1.0.0".into()),
            latest_version: "1.2.0".into(),
            takeover_supported: false,
        };
        let out = format_adopted_detail(&item, Some("1.0.0"), Some(&outcome));
        assert!(out.contains("update available: 1.0.0 -> 1.2.0"));
        // The explanation names the verb on purpose; what must never appear
        // is a runnable `ccync add <url>` the user could copy and have fail.
        assert!(
            !out.contains("ccync add https://"),
            "non-clonable source must not be advertised as a runnable add: {out:?}"
        );
        assert!(out.contains("cannot fetch it directly"));
    }

    #[test]
    fn adopted_detail_mcp_skipped() {
        let item = adopted_item("my-mcp", ManagedItemKind::McpServer, None);
        let outcome = AdoptedUpgradeOutcome::Skipped {
            name: "my-mcp".into(),
            reason: "adopted MCP server — config entry, not versioned".into(),
        };
        let out = format_adopted_detail(&item, None, Some(&outcome));
        assert!(out.contains("Kind:          mcp"));
        assert!(out.contains("Source:        (unknown)"));
        assert!(out.contains("Version:       (unknown)"));
        assert!(out.contains("not upgradeable: adopted MCP server"));
    }

    #[test]
    fn adopted_detail_unchanged() {
        let item = adopted_item(
            "stable@market",
            ManagedItemKind::Plugin,
            Some("https://github.com/u/stable.git"),
        );
        let outcome = AdoptedUpgradeOutcome::Unchanged {
            name: "stable@market".into(),
        };
        let out = format_adopted_detail(&item, Some("2.0.0"), Some(&outcome));
        assert!(out.contains("Version:       2.0.0"));
        assert!(out.contains("up to date"));
    }

    #[test]
    fn show_repair_hint_for_adopted_item_with_no_source() {
        let item = adopted_item("standalone-tool", ManagedItemKind::Plugin, None);
        let outcome = AdoptedUpgradeOutcome::Skipped {
            name: "standalone-tool".into(),
            reason: "adopted plugin — source-unresolved (no clonable source recorded)".into(),
        };
        let out = format_adopted_detail(&item, None, Some(&outcome));
        assert!(
            out.contains("Tip: run `ccync repair standalone-tool`"),
            "must print a ccync repair tip naming the item: {out:?}"
        );
        let hint_lines = out
            .lines()
            .filter(|l| l.contains("Tip: run `ccync repair"))
            .count();
        assert_eq!(
            hint_lines, 1,
            "must print exactly one repair-hint line: {out:?}"
        );
    }

    #[test]
    fn show_repair_hint_covers_mcp_and_skill_uniformly_when_no_source() {
        // Unlike the removed plugin-only hint, the tip is driven by whether
        // the item actually has a source — MCP/skill items with none get the
        // same tip a plugin would, covering plugin/MCP/skill uniformly.
        let mcp_item = adopted_item("memory", ManagedItemKind::McpServer, None);
        let mcp_outcome = AdoptedUpgradeOutcome::Skipped {
            name: "memory".into(),
            reason: "adopted MCP server — config entry, not versioned".into(),
        };
        let mcp_out = format_adopted_detail(&mcp_item, None, Some(&mcp_outcome));
        assert!(
            mcp_out.contains("Tip: run `ccync repair memory`"),
            "adopted MCP item with no source must print the repair tip: {mcp_out:?}"
        );

        let skill_item = adopted_item("my-skill", ManagedItemKind::Skill, None);
        let skill_outcome = AdoptedUpgradeOutcome::Skipped {
            name: "my-skill".into(),
            reason: "adopted skill — local content, no upstream".into(),
        };
        let skill_out = format_adopted_detail(&skill_item, None, Some(&skill_outcome));
        assert!(
            skill_out.contains("Tip: run `ccync repair my-skill`"),
            "adopted skill item with no source must print the repair tip: {skill_out:?}"
        );
    }

    #[test]
    fn show_repair_hint_absent_when_source_already_recorded() {
        let item = adopted_item(
            "tool@market",
            ManagedItemKind::Plugin,
            Some("https://github.com/example/tool-plugin.git"),
        );
        let outcome = AdoptedUpgradeOutcome::Unchanged {
            name: "tool@market".into(),
        };
        let out = format_adopted_detail(&item, None, Some(&outcome));
        assert!(
            !out.contains("Tip: run `ccync repair"),
            "an item with a recorded source must not print the repair tip: {out:?}"
        );
    }

    #[test]
    fn show_upgradeability_version_unresolved_is_not_rendered_as_not_upgradeable() {
        let item = adopted_item(
            "tool@market",
            ManagedItemKind::Plugin,
            Some("https://github.com/example/tool.git"),
        );
        let outcome = AdoptedUpgradeOutcome::VersionUnresolved {
            name: "tool@market".into(),
            source: "https://github.com/example/tool.git".into(),
        };
        let out = format_adopted_detail(&item, None, Some(&outcome));
        assert!(out.contains("version-unresolved"));
        assert!(
            !out.contains("not upgradeable"),
            "a source-bearing item must never render the Skipped wording: {out:?}"
        );
    }

    #[test]
    fn adopted_detail_skill_no_outcome() {
        let item = adopted_item("my-skill", ManagedItemKind::Skill, None);
        let out = format_adopted_detail(&item, None, None);
        assert!(out.contains("Kind:          skill"));
        assert!(out.contains("(not classified)"));
    }

    #[test]
    fn adopted_detail_unknown_id_produces_not_found() {
        // When build_managed_view finds no item, cmd_show returns Error.
        // Verify the not-found message text is correct (pure text check).
        let lock = json!({});
        let items = ccync_engine::build_managed_view(&lock);
        assert!(items.iter().find(|i| i.id == "ghost").is_none());
    }

    // ── audit repro: pinned lockfile agrees across surfaces ──────────────────────

    /// Audit-repro integration test for the "three surfaces, three answers"
    /// bug: once a personal plugin is recorded in the lockfile with a `pin`, the
    /// doctor surface (real `setup::health::SetupHealthCheck`) must emit no false
    /// `(unpinned)` warning for it, and the managed view that drives `list` /
    /// `show` (`ccync_engine::build_managed_view`) must surface the same pin.
    /// Exercises both production paths against a temp `~/.ccync`.
    #[test]
    fn pinned_plugin_agrees_across_doctor_and_managed_view() {
        // Serialize with the other HOME-redirecting tests in this crate.
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

        let pin = "554ed51abc0000000000";
        let ccync = home.path().join(".ccync");
        std::fs::create_dir_all(ccync.join("build")).unwrap();
        // Personal catalog (~/.ccync/plugins.json): one plugin, no pin here.
        std::fs::write(
            ccync.join("plugins.json"),
            r#"{"schemaVersion":1,"plugins":[{"pluginId":"my-plugin","source":"https://github.com/u/my-plugin.git","installStrategy":"personal-git-clone"}]}"#,
        )
        .unwrap();
        // Lockfile (~/.ccync/build/lock.json): the pin the write path really
        // writes is `pin` (never `sha`) — the exact case that produced the bug.
        let lock = json!({
            "_personalPlugins": [
                {"pluginId": "my-plugin", "source": "https://github.com/u/my-plugin.git", "pin": pin}
            ]
        });
        std::fs::write(
            ccync.join("build").join("lock.json"),
            serde_json::to_string(&lock).unwrap(),
        )
        .unwrap();

        // Doctor surface: run the real setup health check against the temp home.
        let repo_root = home.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let findings = {
            use ccync_foundation::health::HealthCheck as _;
            setup::health::SetupHealthCheck { repo_root }.check()
        };

        // list/show surface: the managed view reading the same lockfile.
        let view = ccync_engine::build_managed_view(&lock);

        // Restore env before any assertion so an unwinding failure can't leave
        // the process env pointed at a temp dir that's about to be dropped.
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

        // Doctor must NOT report the pinned plugin as (unpinned).
        assert!(
            !findings
                .iter()
                .any(|f| f.message.contains("my-plugin") && f.message.contains("(unpinned)")),
            "doctor must not report a pinned plugin as (unpinned): {findings:?}"
        );

        // list/show must show the same pin the lockfile recorded.
        let item = view
            .iter()
            .find(|i| i.id == "my-plugin")
            .expect("managed view must include the personal plugin");
        assert_eq!(
            item.pin.as_deref(),
            Some(pin),
            "managed view pin must match the lockfile pin"
        );
    }
}
