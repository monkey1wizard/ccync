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
    if let Err(code) = super::args::reject_unknown_flags(
        "add",
        args,
        &["--no-sync", "--yes"],
        &[],
        1,
        "Usage: ccync add <git-url | local-path | archive.zip | catalog-id> [--no-sync] [--yes]",
    ) {
        return code;
    }
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
    let Some(catalog_path) = ccync_foundation::paths::personal_catalog_path() else {
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
        let Some(cache_root) = ccync_foundation::paths::ccync_cache_root() else {
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
        // Git / catalog-ID-resolved-to-git: existing clone pipeline. Both kinds
        // share the same source-type/strategy.
        let (source_type, install_strategy) = ("curated-upstream", "personal-git-clone");

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
        let mut pinned_sha: Option<String> = None;
        for (id, result) in &results {
            use ccync_engine::install::PersonalFetchResult;
            match result {
                PersonalFetchResult::Cloned { sha, .. } => {
                    let short = &sha[..sha.len().min(12)];
                    println!("  Fetched '{id}' @ {short}");
                    if id == &plugin_id {
                        pinned_sha = Some(sha.clone());
                    }
                }
                PersonalFetchResult::AlreadyPresent(p) => {
                    println!("  Already present: {}", p.display());
                    if id == &plugin_id {
                        // Recover the sha from the existing `<id>@<sha>` dir name.
                        pinned_sha = p
                            .file_name()
                            .and_then(|n| n.to_str())
                            .and_then(|n| n.split('@').nth(1))
                            .map(str::to_string);
                    }
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

        // Persist the initial pin so the lockfile carries a trustworthy live sha
        // (upgrade current→latest, cleanup live set, render exact dir all depend
        // on it). Raw-JSON merge into the catalog entry, then re-resolve so the
        // extended splice writes the pin into `_personalPlugins`.
        if let Some(sha) = pinned_sha {
            if let Some(arr) = catalog.get_mut("plugins").and_then(Value::as_array_mut) {
                for e in arr.iter_mut() {
                    if e.get("pluginId").and_then(Value::as_str) == Some(plugin_id.as_str()) {
                        e["pin"] = Value::String(sha.clone());
                    }
                }
            }
            if let Err(e) = write_json_atomic(&catalog_path, &catalog) {
                eprintln!("ccync add: failed to persist pin: {e}");
                return ExitCode::Error;
            }
            if let Err(e) = run_personal_resolve() {
                eprintln!("ccync add: pin re-resolve failed: {e}");
                return ExitCode::Error;
            }
        }
    }

    if no_sync {
        println!(
            "Plugin '{plugin_id}' added (--no-sync). Run `ccync sync` to project to all runtimes."
        );
        return ExitCode::Success;
    }

    println!("Plugin '{plugin_id}' added; syncing to all runtimes…");
    super::lifecycle::run_unified_projection(false, args.iter().any(|a| a == "--yes"), false)
}

// ── ccync list ───────────────────────────────────────────────────────────────

pub(crate) fn cmd_list(args: &[String]) -> ExitCode {
    if let Err(code) = super::args::reject_unknown_flags(
        "list",
        args,
        &["--upgrade-available"],
        &[],
        0,
        "Usage: ccync list [--upgrade-available]",
    ) {
        return code;
    }
    // `ccync list --upgrade-available`: read-only outdated view reusing the
    // upgrade engine (resolve each git plugin's remote HEAD vs pin). Bare
    // `list` keeps its original behavior below.
    if wants_upgrade_view(args) {
        let report = super::upgrade::upgrade_available_report();
        print!("{}", super::upgrade::format_upgrade_view(&report));
        if let Some(err) = &report.error {
            eprintln!("ccync list: {err}");
        }
        return super::upgrade::report_to_exit(&report);
    }

    let catalog_path = ccync_foundation::paths::personal_catalog_path();
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
    if let Err(code) = super::args::reject_unknown_flags(
        "remove",
        args,
        &["--yes"],
        &[],
        1,
        "Usage: ccync remove <plugin-id> [--yes]",
    ) {
        return code;
    }
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

    let Some(catalog_path) = ccync_foundation::paths::personal_catalog_path() else {
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
    super::lifecycle::run_unified_projection(false, args.iter().any(|a| a == "--yes"), false)
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
/// The id is used to build the cache path `~/.ccync/cache/<id>@<sha>`, so a
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

/// True when `ccync list` was asked for the read-only upgrade-available view.
fn wants_upgrade_view(args: &[String]) -> bool {
    args.iter().any(|a| a == "--upgrade-available")
}

pub(crate) fn read_personal_catalog(path: &std::path::Path) -> Option<Value> {
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
pub(crate) fn write_json_atomic(path: &std::path::Path, value: &Value) -> Result<(), String> {
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

/// Run personal catalog resolution to update `~/.ccync/build/lock.json`.
/// The public catalog is embedded in the binary.
pub(crate) fn run_personal_resolve() -> Result<(), String> {
    use ccync_engine::catalog::run_resolve_catalog;
    use ccync_foundation::paths::{machine_config_path, plugins_lock_path};

    let config_path = machine_config_path().ok_or("cannot determine config path")?;
    let lockfile_path = plugins_lock_path().ok_or("cannot determine lockfile path")?;

    let opts = ResolveCatalogOptions {
        catalog_path: None,
        config_path,
        lockfile_path,
        dry_run: false,
        personal_catalog_path_override: ccync_foundation::paths::personal_catalog_path(),
    };
    let resolution = run_resolve_catalog(&opts)?;
    for e in &resolution.errors {
        eprintln!("ccync: catalog resolve warning: {e}");
    }
    Ok(())
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

    // ── run_personal_resolve: resolve errors are warnings, not fatal ──────────

    #[test]
    fn run_personal_resolve_returns_ok_when_resolution_has_errors() {
        // Same non-fatal-errors contract as run_catalog_resolve in lifecycle.rs:
        // an unknown defaultProfile in config.json makes the resolver push an
        // entry into CatalogResolution.errors, but run_personal_resolve must
        // still return Ok(()) since resolve errors are warned about, not
        // propagated as a hard failure.
        let _guard = crate::test_io_lock();
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let orig_home;
        #[cfg(windows)]
        unsafe {
            orig_home = std::env::var("USERPROFILE").ok();
            std::env::set_var("USERPROFILE", tmp.path());
            std::env::set_var("HOME", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            orig_home = std::env::var("HOME").ok();
            std::env::set_var("HOME", tmp.path());
        }

        let ccync_home = tmp.path().join(".ccync");
        fs::create_dir_all(&ccync_home).unwrap();
        fs::write(
            ccync_home.join("config.json"),
            r#"{"defaultProfile":"does-not-exist"}"#,
        )
        .unwrap();

        let result = run_personal_resolve();

        #[cfg(windows)]
        unsafe {
            match &orig_home {
                Some(v) => { std::env::set_var("USERPROFILE", v); std::env::set_var("HOME", v); }
                None => { std::env::remove_var("USERPROFILE"); std::env::remove_var("HOME"); }
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
            "resolve errors (e.g. unknown defaultProfile) must be collected as warnings, not returned as Err: {result:?}"
        );
    }

    // ── cmd_add first-run visibility gate (fake-home sandboxed) ───────────────
    //
    // These tests never touch the real developer machine: `set_home` points
    // `ccync_foundation::paths::user_home()` (and everything derived from it —
    // canonical root, ~/.claude/skills, personal catalog/lockfile) at a fresh
    // `TempDir` for the duration of the test. `crate::test_io_lock()` is the
    // crate-wide shared mutex (defined in main.rs, also used by refresh.rs and
    // restore_source.rs) that serializes every test in this binary that
    // mutates the process-global HOME/USERPROFILE env vars — a module-local
    // lock would NOT coordinate with those other modules' tests and races
    // under `cargo test`'s default multithreaded runner.
    mod gate {
        use super::*;
        use tempfile::TempDir;

        fn set_home(home: &std::path::Path) {
            #[cfg(windows)]
            unsafe {
                std::env::set_var("USERPROFILE", home);
                std::env::set_var("HOME", home);
            }
            #[cfg(not(windows))]
            unsafe {
                std::env::set_var("HOME", home);
            }
        }

        /// Non-interactive default-decline requires `CCYNC_ASSUME_YES` unset —
        /// clear it defensively since env state is process-global and another
        /// test (or the outer shell) may have left it set.
        fn clear_assume_yes() {
            unsafe {
                std::env::remove_var("CCYNC_ASSUME_YES");
            }
        }

        /// A minimal local git repo usable as a `ccync add <path>` source —
        /// classified as `SourceKind::Git` by `classify_source` (contains a
        /// path separator) and clonable by `git clone --depth 1 <local-path>`
        /// with no network access.
        fn make_source_repo(dir: &std::path::Path) {
            fs::create_dir_all(dir).unwrap();
            fs::write(dir.join("README.md"), "# fixture plugin\n").unwrap();
            let run = |args: &[&str]| {
                let out = std::process::Command::new("git")
                    .args(args)
                    .current_dir(dir)
                    .output()
                    .expect("git must be on PATH for this test");
                assert!(
                    out.status.success(),
                    "git {args:?} failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            };
            run(&["init", "-q"]);
            run(&["config", "user.email", "test@example.com"]);
            run(&["config", "user.name", "test"]);
            run(&["add", "."]);
            run(&["commit", "-q", "-m", "fixture"]);
        }

        /// The foundation-crate-resolved live surfaces that a completed
        /// projection would populate: canonical render root and the
        /// Claude skills dir, both scoped under the fake home.
        fn canonical_root_exists(fake_home: &std::path::Path) -> bool {
            fake_home.join(".ccync").join("build").join("render").exists()
        }

        fn claude_skills_ccync_exists(fake_home: &std::path::Path) -> bool {
            fake_home
                .join(".claude")
                .join("skills")
                .join("ccync")
                .exists()
        }

        fn catalog_has_plugin(fake_home: &std::path::Path, plugin_id: &str) -> bool {
            let catalog_path = fake_home.path_join_ccync("plugins.json");
            let Some(catalog) = read_personal_catalog(&catalog_path) else {
                return false;
            };
            catalog_contains_id(&catalog, plugin_id)
        }

        // Small local extension so the helpers above read cleanly.
        trait JoinCcync {
            fn path_join_ccync(&self, leaf: &str) -> std::path::PathBuf;
        }
        impl JoinCcync for std::path::Path {
            fn path_join_ccync(&self, leaf: &str) -> std::path::PathBuf {
                self.join(".ccync").join(leaf)
            }
        }

        #[test]
        fn cmd_add_gate_declined_writes_catalog_but_no_live_surfaces() {
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());

            let source = TempDir::new().unwrap();
            make_source_repo(source.path());

            let args = vec!["add".to_string(), source.path().display().to_string()];
            let exit = cmd_add(&args);

            assert_eq!(
                exit,
                ExitCode::Success,
                "non-interactive decline still returns Success (no write is not an error)"
            );
            assert!(
                !canonical_root_exists(fake_home.path()),
                "declined gate must not render the canonical root"
            );
            assert!(
                !claude_skills_ccync_exists(fake_home.path()),
                "declined gate must not project the Claude skills surface"
            );

            let plugin_id = derive_plugin_id(&source.path().display().to_string());
            assert!(
                catalog_has_plugin(fake_home.path(), &plugin_id),
                "the plugin must still be added to the personal catalog/lockfile even though \
                 the gate declined the render — only the downstream projection is skipped"
            );

            // Limitation: cmd_add prints its hint text ("aborted — no files
            // written") via println!/eprintln! rather than a return value, and
            // this crate has no stdout-capture seam for in-process unit tests.
            // Not asserted here.
        }

        #[test]
        fn cmd_add_gate_accepted_writes_live_surfaces() {
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());

            let source = TempDir::new().unwrap();
            make_source_repo(source.path());

            let args = vec![
                "add".to_string(),
                source.path().display().to_string(),
                "--yes".to_string(),
            ];
            let exit = cmd_add(&args);

            assert_eq!(exit, ExitCode::Success, "accepted gate must succeed");
            assert!(
                canonical_root_exists(fake_home.path()),
                "accepted gate must render the canonical root"
            );
            assert!(
                claude_skills_ccync_exists(fake_home.path()),
                "accepted gate must project the Claude skills surface"
            );
            // Live MCP config surfaces (~/.claude.json etc.) are intentionally
            // NOT asserted: the `mcp` crate resolves home via `dirs::`, which
            // can pierce this fake-HOME env-var override on some platforms —
            // out of scope here, covered elsewhere by a real-machine gate.
        }

        #[test]
        fn cmd_add_no_sync_flag_skips_gate_and_leaves_no_live_surfaces() {
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());

            let source = TempDir::new().unwrap();
            make_source_repo(source.path());

            let args = vec![
                "add".to_string(),
                source.path().display().to_string(),
                "--no-sync".to_string(),
            ];
            let exit = cmd_add(&args);

            assert_eq!(exit, ExitCode::Success);
            assert!(
                !canonical_root_exists(fake_home.path()),
                "--no-sync must not render the canonical root"
            );
            assert!(
                !claude_skills_ccync_exists(fake_home.path()),
                "--no-sync must not project the Claude skills surface"
            );

            let plugin_id = derive_plugin_id(&source.path().display().to_string());
            assert!(
                catalog_has_plugin(fake_home.path(), &plugin_id),
                "--no-sync still adds the plugin to the personal catalog"
            );

            // Limitation: verifying that a later plain `sync` would still hit
            // the gate normally would require driving cmd_sync end-to-end in
            // the same process/fake-home, which is exercised by the two tests
            // above via cmd_add's own tail call to run_unified_projection.
            // No separate seam exists to isolate "next sync" from this test
            // without duplicating that coverage, so it is noted as a
            // limitation rather than forced.
        }

        #[test]
        fn cmd_remove_dry_run_flag_is_rejected_without_deleting() {
            // F-01 regression: `remove <id> --dry-run` must return Usage (64)
            // and leave the personal catalog byte-for-byte unchanged, instead
            // of taking the first non---- arg and performing a real delete.
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());

            let ccync = fake_home.path().join(".ccync");
            std::fs::create_dir_all(&ccync).unwrap();
            let catalog = ccync.join("plugins.json");
            let before = r#"{"plugins":[{"pluginId":"demo","source":"https://example.com/demo.git"}]}"#;
            std::fs::write(&catalog, before).unwrap();

            let args = vec![
                "remove".to_string(),
                "demo".to_string(),
                "--dry-run".to_string(),
            ];
            let exit = cmd_remove(&args);

            assert_eq!(
                exit,
                ExitCode::Usage,
                "remove <id> --dry-run must be rejected as a usage error, not a real delete"
            );
            assert_eq!(
                std::fs::read_to_string(&catalog).unwrap(),
                before,
                "the personal catalog must be byte-for-byte unchanged after a rejected remove"
            );
        }
    }

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
        // the cache path `~/.ccync/cache/<id>@<sha>`.
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

    #[test]
    fn list_upgrade_available_flag_detected() {
        assert!(wants_upgrade_view(&[
            "list".to_string(),
            "--upgrade-available".to_string()
        ]));
        assert!(!wants_upgrade_view(&["list".to_string()]));
    }
}
