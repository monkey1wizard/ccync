//! `ccync show <id>` — print full detail of one installed personal plugin
//! (brew-info / winget-show semantics). Read-only.

use ccync_engine::ExitCode;
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

    // Pinned sha: prefer the lockfile's resolved `_personalPlugins` sha, then the
    // catalog entry's own pin/pinnedSha.
    let pinned = ccync_foundation::paths::plugins_lock_path()
        .and_then(|p| read_json(&p))
        .and_then(|v| lock_pin_for(&v, id))
        .or_else(|| entry_pin(entry));

    // Best-effort projected component count from the plugin's cache dir.
    let components = pinned.as_deref().and_then(|sha| {
        ccync_foundation::paths::ccync_cache_root()
            .map(|root| count_components(&root.join(format!("{id}@{sha}"))))
    });

    print!("{}", format_plugin_detail(entry, pinned.as_deref(), components));
    ExitCode::Success
}

/// Render the single-plugin detail block. Pure (returns the text) so it is
/// unit-testable without touching disk.
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
    out.push_str(&format!("  Held:          {}\n", if held { "yes" } else { "no" }));
    out.push_str(&format!(
        "  Components:    {}\n",
        components
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    ));
    out
}

/// Sum of entries in the plugin's `skills/` `commands/` `agents/` `hooks/`
/// component dirs (best-effort; absent dirs count as zero).
fn count_components(plugin_dir: &Path) -> usize {
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
    for key in ["pin", "pinnedSha"] {
        if let Some(sha) = entry.get(key).and_then(Value::as_str) {
            if !sha.is_empty() {
                return Some(sha.to_string());
            }
        }
    }
    None
}

fn lock_pin_for(lock: &Value, id: &str) -> Option<String> {
    lock.get("_personalPlugins")
        .and_then(Value::as_array)?
        .iter()
        .find(|e| e.get("pluginId").and_then(Value::as_str) == Some(id))
        .and_then(|e| {
            e.get("sha")
                .or_else(|| e.get("pin"))
                .or_else(|| e.get("pinnedSha"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detail_renders_all_fields() {
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
    fn detail_handles_missing_optionals() {
        let entry = json!({ "pluginId": "bare", "source": "x", "installStrategy": "personal-archive" });
        let out = format_plugin_detail(&entry, None, None);
        assert!(out.contains("Plugin: bare"));
        assert!(out.contains("(unpinned)"));
        assert!(out.contains("Held:          no"));
        assert!(out.contains("Components:    unknown"));
    }
}
