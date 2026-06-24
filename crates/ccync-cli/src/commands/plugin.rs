//! Managed-item lifecycle: the top-level `add` / `list` / `remove` commands.
//!
//!   `ccync add <link> [--no-sync]`  — add a plugin/MCP/skill, then auto-sync
//!   `ccync list`                    — list the managed set with pinned sha
//!   `ccync remove <id>`             — remove a managed item, then auto-sync

use ccync_engine::{
    catalog::{resolve_catalog_source, ResolveCatalogOptions},
    install::{fetch_all_personal_plugins, fetch_archive_plugin},
    ExitCode,
};
use serde_json::{json, Value};
use std::fs;

// ── ccync add <repo> [--no-sync] ─────────────────────────────────────────────
//
// args[0] = "add"; args[1] = <repo>. Adds a plugin/MCP/skill to the ccync
// managed set, then auto-syncs (projects to all agents) unless `--no-sync`.

/// Source type classification for `ccync add <source>`.
#[derive(Debug, PartialEq)]
enum SourceKind {
    /// git URL or local git-repo path — existing `fetch_personal_plugin` pipeline.
    Git,
    /// Local archive file (`.zip`, `.tar.gz`, `.tgz`).
    Archive,
    /// Bare catalog ID — look up in embedded catalog to get the real source.
    CatalogId,
}

fn classify_source(source: &str) -> SourceKind {
    // Archive extension check (works for both local paths and URLs).
    let lower = source.to_ascii_lowercase();
    if lower.ends_with(".zip") || lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        return SourceKind::Archive;
    }
    // Explicit git/http URLs.
    if source.starts_with("http://")
        || source.starts_with("https://")
        || source.starts_with("git@")
        || source.starts_with("git://")
    {
        return SourceKind::Git;
    }
    // Paths: contain separators, start with `.`, `~`, or are absolute.
    if source.contains('/') || source.contains('\\') || source.starts_with('.') || source.starts_with('~') {
        return SourceKind::Git;
    }
    // Bare identifier with no slashes/dots → catalog ID.
    SourceKind::CatalogId
}

pub(crate) fn cmd_add(args: &[String]) -> ExitCode {
    let no_sync = args.iter().any(|a| a == "--no-sync");
    let Some(source) = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .map(String::as_str)
    else {
        eprintln!("ccync add: missing <source> argument");
        eprintln!("Usage: ccync add <git-url | local-path | archive.zip | catalog-id> [--no-sync]");
        return ExitCode::Usage;
    };

    let kind = classify_source(source);

    // For catalog IDs, resolve to the real fetch source first.
    let resolved_source: String;
    let fetch_source: &str;
    if kind == SourceKind::CatalogId {
        match resolve_catalog_source(source) {
            Some(s) => {
                resolved_source = s;
                fetch_source = &resolved_source;
            }
            None => {
                eprintln!("ccync add: catalog ID '{source}' not found in embedded catalog.");
                eprintln!("  Tip: use a git URL, local path, or archive file instead.");
                return ExitCode::Error;
            }
        }
    } else {
        fetch_source = source;
    }

    let plugin_id = if kind == SourceKind::CatalogId {
        // Keep the catalog ID as the plugin ID — don't re-derive from the resolved URL.
        derive_plugin_id(source)
    } else {
        derive_plugin_id(fetch_source)
    };

    // Read / bootstrap personal catalog.
    let Some(catalog_path) = ccync_foundation::paths::local_catalog_path() else {
        eprintln!("ccync add: cannot determine home directory");
        return ExitCode::Error;
    };
    let mut catalog = read_personal_catalog(&catalog_path)
        .unwrap_or_else(|| json!({ "schemaVersion": 1, "plugins": [] }));

    // Check for duplicates.
    if catalog_contains_id(&catalog, &plugin_id) {
        eprintln!("ccync add: plugin '{plugin_id}' is already in the personal catalog.");
        eprintln!(
            "  Use `ccync remove {plugin_id}` then `ccync add` to re-fetch the latest version."
        );
        return ExitCode::Usage;
    }

    // Archive source: fetch first, then pin catalog entry with the computed hash.
    if kind == SourceKind::Archive {
        let archive_path = std::path::Path::new(fetch_source);
        let Some(cache_root) = ccync_foundation::paths::local_plugin_cache_dir() else {
            eprintln!("ccync add: cannot determine cache directory");
            return ExitCode::Error;
        };
        let result = fetch_archive_plugin(&cache_root, &plugin_id, archive_path);
        use ccync_engine::install::PersonalFetchResult;
        let sha = match result {
            PersonalFetchResult::Cloned { ref sha, .. } => {
                println!("  Extracted '{plugin_id}' @ {}", &sha[..sha.len().min(12)]);
                sha.clone()
            }
            PersonalFetchResult::AlreadyPresent(ref p) => {
                println!("  Already present: {}", p.display());
                // Extract sha from dir name
                p.file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.split('@').nth(1))
                    .unwrap_or("unknown")
                    .to_string()
            }
            PersonalFetchResult::NoCacheRoot => {
                eprintln!("ccync add: could not determine cache root");
                return ExitCode::Error;
            }
            PersonalFetchResult::CloneFailed(msg) => {
                eprintln!("ccync add: archive extraction failed: {msg}");
                return ExitCode::Error;
            }
        };

        let entry = json!({
            "pluginId": plugin_id,
            "displayName": plugin_id,
            "sourceType": "local",
            "installStrategy": "personal-archive",
            "source": fetch_source,
            "pinnedSha": sha,
        });
        if let Some(arr) = catalog.get_mut("plugins").and_then(Value::as_array_mut) {
            arr.push(entry);
        }
        if let Err(e) = write_json_atomic(&catalog_path, &catalog) {
            eprintln!("ccync add: failed to write personal catalog: {e}");
            return ExitCode::Error;
        }
        println!("Added '{plugin_id}' to personal catalog.");
        if let Err(e) = run_personal_resolve() {
            eprintln!("ccync add: resolve step failed: {e}");
            return ExitCode::Error;
        }
    } else {
        // Git / catalog-ID-resolved-to-git: existing clone pipeline.
        let (source_type, install_strategy) = if kind == SourceKind::CatalogId {
            ("curated-upstream", "personal-git-clone")
        } else {
            ("curated-upstream", "personal-git-clone")
        };

        let entry = json!({
            "pluginId": plugin_id,
            "displayName": plugin_id,
            "sourceType": source_type,
            "installStrategy": install_strategy,
            "source": fetch_source,
        });
        if let Some(arr) = catalog.get_mut("plugins").and_then(Value::as_array_mut) {
            arr.push(entry);
        }

        if let Err(e) = write_json_atomic(&catalog_path, &catalog) {
            eprintln!("ccync add: failed to write personal catalog: {e}");
            return ExitCode::Error;
        }
        println!("Added '{plugin_id}' to personal catalog.");

        if let Err(e) = run_personal_resolve() {
            eprintln!("ccync add: resolve step failed: {e}");
            return ExitCode::Error;
        }

        let results = fetch_all_personal_plugins();
        let mut fetch_ok = true;
        for (id, result) in &results {
            use ccync_engine::install::PersonalFetchResult;
            match result {
                PersonalFetchResult::Cloned { sha, .. } => {
                    let short = &sha[..sha.len().min(12)];
                    println!("  Fetched '{id}' @ {short}");
                }
                PersonalFetchResult::AlreadyPresent(p) => {
                    println!("  Already present: {}", p.display());
                }
                PersonalFetchResult::NoCacheRoot => {
                    eprintln!("  Warning: could not determine cache root for '{id}'.");
                }
                PersonalFetchResult::CloneFailed(msg) => {
                    eprintln!("  Error: fetch failed for '{id}': {msg}");
                    fetch_ok = false;
                }
            }
        }
        if !fetch_ok {
            return ExitCode::Error;
        }
    }

    // Render canonical root so the plugin is projected.
    if let Err(e) = run_render() {
        eprintln!("ccync add: render step failed: {e}");
        return ExitCode::Error;
    }

    if no_sync {
        println!(
            "Plugin '{plugin_id}' added (--no-sync). Run `ccync sync` to project to all runtimes."
        );
        return ExitCode::Success;
    }

    println!("Plugin '{plugin_id}' added; syncing to all runtimes…");
    super::lifecycle::run_unified_projection(false, args.iter().any(|a| a == "--yes"))
}

// ── ccync list ───────────────────────────────────────────────────────────────

pub(crate) fn cmd_list() -> ExitCode {
    let catalog_path = ccync_foundation::paths::local_catalog_path();
    let lockfile_path = ccync_foundation::paths::plugins_lock_path();

    // Read personal catalog.
    let catalog_entries = catalog_path
        .as_deref()
        .and_then(read_personal_catalog)
        .and_then(|c| {
            c.get("plugins")
                .and_then(Value::as_array)
                .map(|arr| arr.to_vec())
        })
        .unwrap_or_default();

    // Read personal lock entries for pinned sha.
    let lock_entries: Vec<Value> = lockfile_path
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.get("_personalPlugins")
                .and_then(Value::as_array)
                .map(|a| a.to_vec())
        })
        .unwrap_or_default();

    if catalog_entries.is_empty() {
        println!("No personal plugins installed.");
        println!("Add one with: ccync add <git-url>");
        return ExitCode::Success;
    }

    println!("{:<30} {:<14} SOURCE", "PLUGIN ID", "SHA (PINNED)");
    println!("{}", "-".repeat(80));
    for entry in &catalog_entries {
        let id = entry.get("pluginId").and_then(Value::as_str).unwrap_or("?");
        let source = entry.get("source").and_then(Value::as_str).unwrap_or("?");
        let sha = lock_entries
            .iter()
            .find(|e| e.get("pluginId").and_then(Value::as_str) == Some(id))
            .and_then(|e| e.get("sha").and_then(Value::as_str))
            .unwrap_or("(unpinned)");
        let sha_display = if sha.len() > 12 { &sha[..12] } else { sha };
        println!("{:<30} {:<14} {}", id, sha_display, source);
    }
    ExitCode::Success
}

// ── ccync remove <id> ────────────────────────────────────────────────────────
//
// args[0] = "remove"; args[1] = <id>. Removes a managed item, then auto-syncs.

pub(crate) fn cmd_remove(args: &[String]) -> ExitCode {
    let Some(plugin_id) = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .map(String::as_str)
    else {
        eprintln!("ccync remove: missing <id> argument");
        eprintln!("Usage: ccync remove <plugin-id>");
        return ExitCode::Usage;
    };

    let Some(catalog_path) = ccync_foundation::paths::local_catalog_path() else {
        eprintln!("ccync remove: cannot determine home directory");
        return ExitCode::Error;
    };

    let Some(mut catalog) = read_personal_catalog(&catalog_path) else {
        eprintln!("ccync remove: personal catalog not found (nothing to remove).");
        return ExitCode::Usage;
    };

    let before_len = catalog
        .get("plugins")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    if let Some(arr) = catalog.get_mut("plugins").and_then(Value::as_array_mut) {
        arr.retain(|e| e.get("pluginId").and_then(Value::as_str) != Some(plugin_id));
    }
    let after_len = catalog
        .get("plugins")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    if before_len == after_len {
        eprintln!("ccync remove: plugin '{plugin_id}' not found in personal catalog.");
        return ExitCode::Usage;
    }

    if let Err(e) = write_json_atomic(&catalog_path, &catalog) {
        eprintln!("ccync remove: failed to write personal catalog: {e}");
        return ExitCode::Error;
    }

    if let Err(e) = run_personal_resolve() {
        eprintln!("ccync remove: resolve step failed: {e}");
        return ExitCode::Error;
    }

    // Re-render prunes projected artifacts via ManagedArtifactRegistry.
    if let Err(e) = run_render() {
        eprintln!("ccync remove: render step failed: {e}");
        return ExitCode::Error;
    }

    println!("Plugin '{plugin_id}' removed; syncing to all runtimes…");
    super::lifecycle::run_unified_projection(false, args.iter().any(|a| a == "--yes"))
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Derive a stable plugin id from a git URL or local path.
/// Uses the last path segment, strips a trailing `.git`, then sanitizes the
/// result so it is always a safe single-component basename — see
/// [`sanitize_plugin_id`].
pub(crate) fn derive_plugin_id(repo: &str) -> String {
    let cleaned = repo.trim_end_matches(['/', '\\']);
    // Handle scp-style git@host:org/repo.git → take the part after ':'
    let after_colon = cleaned.rsplit(':').next().unwrap_or(cleaned);
    // Take the last component splitting on BOTH separators — a Windows local
    // path uses `\`, which `split('/')` would not break on, leaking the whole
    // path (including any `..`) into the id.
    let last = after_colon
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(after_colon);
    sanitize_plugin_id(last.trim_end_matches(".git"))
}

/// Reduce a derived id to a safe cache-directory basename.
///
/// The id is used to build the cache path `~/.ccync/local/cache/<id>@<sha>`, so a
/// crafted git URL or local path must not be able to inject a path separator or a
/// `..` traversal token that escapes the cache root. Keep only `[A-Za-z0-9._-]`
/// (replacing anything else with `-`), strip leading dots so no `.`/`..` token can
/// form, and fall back to `plugin` if nothing safe remains.
fn sanitize_plugin_id(raw: &str) -> String {
    let filtered: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = filtered.trim_start_matches('.').trim_matches('-');
    if trimmed.is_empty() || trimmed == ".." {
        "plugin".to_string()
    } else {
        trimmed.to_string()
    }
}

fn read_personal_catalog(path: &std::path::Path) -> Option<Value> {
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn catalog_contains_id(catalog: &Value, plugin_id: &str) -> bool {
    catalog
        .get("plugins")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .any(|e| e.get("pluginId").and_then(Value::as_str) == Some(plugin_id))
        })
        .unwrap_or(false)
}

/// Write JSON atomically (temp sibling + rename).
fn write_json_atomic(path: &std::path::Path, value: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(
        &tmp,
        serde_json::to_string_pretty(value).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("write tmp: {e}"))?;
    fs::rename(&tmp, path).map_err(|e| format!("rename: {e}"))
}

/// Run personal catalog resolution to update `~/.ccync/state/plugins.lock.json`.
fn run_personal_resolve() -> Result<(), String> {
    use ccync_engine::catalog::run_resolve_catalog;
    use ccync_foundation::paths::{ccync_plugin_root, machine_config_path, plugins_lock_path};

    // Public catalog lives at ~/.ccync/plugins/ccync/catalog.json after install.
    let catalog_path = ccync_plugin_root("ccync")
        .ok_or("cannot determine ccync plugin root")?
        .join("catalog.json");
    let config_path = machine_config_path().ok_or("cannot determine config path")?;
    let lockfile_path = plugins_lock_path().ok_or("cannot determine lockfile path")?;

    let opts = ResolveCatalogOptions {
        catalog_path,
        config_path,
        lockfile_path,
        dry_run: false,
        personal_catalog_path_override: ccync_foundation::paths::local_catalog_path(),
    };
    run_resolve_catalog(&opts).map(|_| ())
}

/// Re-render canonical root so personal plugin components are (re)projected.
fn run_render() -> Result<(), String> {
    use ccync_engine::{config::CcyncConfig, install::run_update};
    let config = CcyncConfig::load();
    run_update(&config).map(|_| ()).map_err(|e| format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_plugin_id_strips_git_suffix() {
        assert_eq!(
            derive_plugin_id("https://github.com/user/my-plugin.git"),
            "my-plugin"
        );
    }

    #[test]
    fn derive_plugin_id_no_git_suffix() {
        assert_eq!(
            derive_plugin_id("https://github.com/user/my-plugin"),
            "my-plugin"
        );
    }

    #[test]
    fn derive_plugin_id_local_path() {
        assert_eq!(
            derive_plugin_id("/home/user/my-local-plugin"),
            "my-local-plugin"
        );
    }

    #[test]
    fn derive_plugin_id_trailing_slash() {
        assert_eq!(
            derive_plugin_id("https://github.com/user/plugin/"),
            "plugin"
        );
    }

    #[test]
    fn derive_plugin_id_rejects_windows_path_traversal() {
        // A Windows local path with backslashes + `..` must collapse to the bare
        // final component — no separator and no traversal token may survive into
        // the cache path `~/.ccync/local/cache/<id>@<sha>`.
        for crafted in [
            r"C:\Users\x\..\..\..\Windows\System32",
            r"..\..\evil",
            "../../etc/passwd",
            r"\\server\share\..\secret",
        ] {
            let id = derive_plugin_id(crafted);
            assert!(
                !id.contains('/') && !id.contains('\\'),
                "derived id {id:?} from {crafted:?} must not contain a path separator"
            );
            assert!(
                id != ".." && !id.starts_with(".."),
                "derived id {id:?} from {crafted:?} must not be a traversal token"
            );
            assert!(!id.is_empty(), "derived id from {crafted:?} must not be empty");
        }
    }

    #[test]
    fn derive_plugin_id_bare_dotdot_falls_back() {
        assert_eq!(derive_plugin_id(".."), "plugin");
        assert_eq!(derive_plugin_id("foo/.."), "plugin");
    }

    #[test]
    fn classify_source_archive_zip() {
        assert_eq!(classify_source("plugin.zip"), SourceKind::Archive);
        assert_eq!(classify_source("/path/to/plugin.zip"), SourceKind::Archive);
        assert_eq!(
            classify_source("https://example.com/plugin.tar.gz"),
            SourceKind::Archive
        );
    }

    #[test]
    fn classify_source_git_url() {
        assert_eq!(
            classify_source("https://github.com/user/repo.git"),
            SourceKind::Git
        );
        assert_eq!(
            classify_source("git@github.com:user/repo.git"),
            SourceKind::Git
        );
        assert_eq!(
            classify_source("http://example.com/repo"),
            SourceKind::Git
        );
    }

    #[test]
    fn classify_source_local_path() {
        assert_eq!(classify_source("./local-plugin"), SourceKind::Git);
        assert_eq!(classify_source("~/plugins/my-thing"), SourceKind::Git);
        assert_eq!(classify_source("/absolute/path"), SourceKind::Git);
    }

    #[test]
    fn classify_source_catalog_id() {
        assert_eq!(classify_source("my-plugin"), SourceKind::CatalogId);
        assert_eq!(classify_source("ccync-game-assets"), SourceKind::CatalogId);
    }

    #[test]
    fn cmd_add_missing_repo_is_usage() {
        assert_eq!(cmd_add(&["add".to_string()]), ExitCode::Usage);
    }

    #[test]
    fn cmd_remove_missing_id_is_usage() {
        assert_eq!(cmd_remove(&["remove".to_string()]), ExitCode::Usage);
    }
}
