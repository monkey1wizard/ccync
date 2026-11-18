//! Managed-item lifecycle: the top-level `add` / `list` / `remove` commands.
//!
//!   `ccync add <link> [--no-sync]`  — add a plugin/MCP/skill, then auto-sync
//!   `ccync list`                    — list the managed set with pinned sha
//!   `ccync remove <id>`             — remove a managed item, then auto-sync

use ccync_engine::{
    catalog::{resolve_catalog_source, ResolveCatalogOptions},
    install::{
        fetch_archive_plugin, fetch_dir_plugin, fetch_personal_plugin_at,
        inspect_render_contribution, payload_shape, PayloadShape, PersonalFetchResult,
    },
    plugin_render::authoritative_plugin_name,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddIntakeOutcome {
    Added,
    FetchFailed,
    NoContribution,
}

impl AddIntakeOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::FetchFailed => "fetch-failed",
            Self::NoContribution => "no-contribution",
        }
    }
}

fn format_add_component_messages(component_count: usize, shape: PayloadShape) -> Vec<String> {
    let mut lines = vec![format!("Components: {component_count}")];
    if matches!(shape, PayloadShape::Unrecognized) {
        lines.push(
            "warning: detected 0 components (skills/commands/agents/hooks) — this may not be a plugin repo"
                .to_string(),
        );
    }
    lines
}

fn classify_source(source: &str) -> SourceKind {
    if ccync_engine::install::split_git_subpath(source).1.is_some() {
        return SourceKind::Git;
    }
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
    if source.contains('/')
        || source.contains('\\')
        || source.starts_with('.')
        || source.starts_with('~')
    {
        return SourceKind::Git;
    }
    // Bare identifier with no slashes/dots → catalog ID.
    SourceKind::CatalogId
}

/// True when `dir` is a git working copy or linked worktree — has a `.git`
/// file (linked worktree) or directory (normal clone) at its root.
fn is_git_working_copy(dir: &std::path::Path) -> bool {
    dir.join(".git").exists()
}

/// Route a [`SourceKind::Git`]-classified source by filesystem identity: a
/// remote URL or an existing git working copy/worktree keeps the existing
/// `git clone` pipeline; a plain local directory without `.git` is an
/// immutable content snapshot fetched via `fetch_dir_plugin`.
fn is_plain_directory_snapshot(source: &str) -> bool {
    !ccync_engine::install::is_remote_url(source) && {
        let path = std::path::Path::new(source);
        path.is_dir() && !is_git_working_copy(path)
    }
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

    // Machine-initialization preflight: a projecting add (no `--no-sync`)
    // will fall through to `run_unified_projection`'s live-agent-surface
    // write, so it must not run before `ccync init`. Checked here — before
    // catalog-ID resolution, source inspection/fetch, or any catalog write —
    // so a refusal is a true zero-mutation abort. `add <source> --no-sync`
    // is the exception: it only ever touches the plugin cache and
    // personal catalog, never a live agent surface, so it stays available
    // pre-init.
    if !no_sync {
        if let Err(code) = super::lifecycle::require_initialized_machine() {
            eprintln!("  Tip: use `ccync add {source} --no-sync` to add without projecting on this machine.");
            return code;
        }
    }

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
    let layout = match ccync_foundation::paths::EffectiveLayout::load_or_legacy() {
        Ok(layout) => layout,
        Err(error) => {
            eprintln!("ccync add: cannot load storage layout: {error}");
            return ExitCode::Error;
        }
    };
    let catalog_path = layout.personal_catalog_path();
    let mut catalog = match fs::read_to_string(&catalog_path) {
        Ok(contents) => match serde_json::from_str::<Value>(&contents) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("ccync add: failed to read personal catalog: {error}");
                return ExitCode::Error;
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            json!({ "schemaVersion": 1, "plugins": [] })
        }
        Err(error) => {
            eprintln!("ccync add: failed to read personal catalog: {error}");
            return ExitCode::Error;
        }
    };
    if !catalog.is_object() {
        eprintln!("ccync add: personal catalog root must be an object");
        return ExitCode::Error;
    }
    if catalog
        .get("plugins")
        .is_some_and(|plugins| !plugins.is_array())
    {
        eprintln!("ccync add: personal catalog plugins must be an array");
        return ExitCode::Error;
    }

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
        let cache_root = layout.cache_path();
        let result = fetch_archive_plugin(&cache_root, &plugin_id, archive_path);
        use ccync_engine::install::PersonalFetchResult;
        let (sha, component_dir) = match result {
            PersonalFetchResult::Cloned { path, sha } => {
                println!("  Extracted '{plugin_id}' @ {}", &sha[..sha.len().min(12)]);
                (sha, path)
            }
            PersonalFetchResult::AlreadyPresent(p) => {
                println!("  Already present: {}", p.display());
                let sha = p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.split('@').nth(1))
                    .unwrap_or("unknown")
                    .to_string();
                (sha, p)
            }
            PersonalFetchResult::NoCacheRoot => {
                eprintln!("ccync add: could not determine cache root");
                println!("{}", AddIntakeOutcome::FetchFailed.as_str());
                return ExitCode::Error;
            }
            PersonalFetchResult::CloneFailed(msg) => {
                eprintln!("ccync add: archive extraction failed: {msg}");
                println!("{}", AddIntakeOutcome::FetchFailed.as_str());
                return ExitCode::Error;
            }
        };

        match inspect_render_contribution(&plugin_id, &component_dir) {
            Ok(true) => {}
            Ok(false) => {
                print_no_contribution_guidance();
                return ExitCode::Error;
            }
            Err(error) => {
                eprintln!("ccync add: could not inspect source contribution: {error}");
                println!("{}", AddIntakeOutcome::FetchFailed.as_str());
                return ExitCode::Error;
            }
        }

        let package_name = match resolve_intake_package_name(&plugin_id, &component_dir) {
            Ok(name) => name,
            Err(error) => {
                eprintln!(
                    "ccync add: could not verify package identity for '{plugin_id}': {error}"
                );
                return ExitCode::Error;
            }
        };
        let mut entry = json!({
            "pluginId": plugin_id,
            "displayName": plugin_id,
            "sourceType": "local",
            "installStrategy": "personal-archive",
            "source": fetch_source,
            "pinnedSha": sha,
        });
        record_package_name(&mut entry, package_name);
        append_catalog_plugin(&mut catalog, entry);
        if let Err(e) = persist_source_catalog(&layout, &catalog) {
            eprintln!("ccync add: failed to write personal catalog: {e}");
            return ExitCode::Error;
        }
        println!("{}", AddIntakeOutcome::Added.as_str());
        println!("Added '{plugin_id}' to personal catalog.");
        if let Err(e) = run_personal_resolve(&layout) {
            eprintln!("ccync add: resolve step failed: {e}");
            return ExitCode::Error;
        }
        for line in format_add_component_messages(
            super::show::count_components(&component_dir),
            payload_shape(&component_dir),
        ) {
            println!("{line}");
        }
    } else if is_plain_directory_snapshot(fetch_source) {
        // Plain local directory (no `.git`): immutable content snapshot via
        // fetch_dir_plugin, not a git clone. `installStrategy` differs from
        // "personal-git-clone" so `ccync upgrade` skips it — refresh is
        // remove-then-add, not a re-clone.
        let cache_root = layout.cache_path();
        let source_path = std::path::Path::new(fetch_source);
        let (sha, component_dir) = match fetch_dir_plugin(&cache_root, &plugin_id, source_path) {
            PersonalFetchResult::Cloned { path, sha } => {
                println!(
                    "  Snapshotted '{plugin_id}' @ {}",
                    &sha[..sha.len().min(12)]
                );
                (sha, path)
            }
            PersonalFetchResult::AlreadyPresent(p) => {
                println!("  Already present: {}", p.display());
                let sha = p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.split('@').nth(1))
                    .unwrap_or("unknown")
                    .to_string();
                (sha, p)
            }
            PersonalFetchResult::NoCacheRoot => {
                eprintln!("ccync add: could not determine cache root");
                println!("{}", AddIntakeOutcome::FetchFailed.as_str());
                return ExitCode::Error;
            }
            PersonalFetchResult::CloneFailed(msg) => {
                eprintln!("ccync add: directory snapshot failed: {msg}");
                println!("{}", AddIntakeOutcome::FetchFailed.as_str());
                return ExitCode::Error;
            }
        };

        match inspect_render_contribution(&plugin_id, &component_dir) {
            Ok(true) => {}
            Ok(false) => {
                print_no_contribution_guidance();
                return ExitCode::Error;
            }
            Err(error) => {
                eprintln!("ccync add: could not inspect source contribution: {error}");
                println!("{}", AddIntakeOutcome::FetchFailed.as_str());
                return ExitCode::Error;
            }
        }

        let package_name = match resolve_intake_package_name(&plugin_id, &component_dir) {
            Ok(name) => name,
            Err(error) => {
                eprintln!(
                    "ccync add: could not verify package identity for '{plugin_id}': {error}"
                );
                return ExitCode::Error;
            }
        };
        let mut entry = json!({
            "pluginId": plugin_id,
            "displayName": plugin_id,
            "sourceType": "local",
            "installStrategy": "personal-dir-snapshot",
            "source": fetch_source,
            "pinnedSha": sha,
        });
        record_package_name(&mut entry, package_name);
        append_catalog_plugin(&mut catalog, entry);
        if let Err(e) = persist_source_catalog(&layout, &catalog) {
            eprintln!("ccync add: failed to write personal catalog: {e}");
            return ExitCode::Error;
        }
        println!("{}", AddIntakeOutcome::Added.as_str());
        println!("Added '{plugin_id}' to personal catalog.");
        if let Err(e) = run_personal_resolve(&layout) {
            eprintln!("ccync add: resolve step failed: {e}");
            return ExitCode::Error;
        }
        for line in format_add_component_messages(
            super::show::count_components(&component_dir),
            payload_shape(&component_dir),
        ) {
            println!("{line}");
        }
    } else {
        let cache_root = layout.cache_path();
        if !ccync_engine::install::is_valid_plugin_id(&plugin_id) {
            eprintln!("ccync add: invalid plugin id: '{plugin_id}'");
            return ExitCode::Error;
        }
        // Fetch, inspect and resolve identity before the catalog write, so a
        // rejected source leaves the personal catalog bytes unchanged.
        let (sha, component_dir, fetched) =
            match fetch_personal_plugin_at(&cache_root, &plugin_id, fetch_source, None) {
                PersonalFetchResult::Cloned { path, sha } => (sha, path, true),
                PersonalFetchResult::AlreadyPresent(path) => {
                    let sha = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .and_then(|n| n.split('@').nth(1))
                        .unwrap_or("unknown")
                        .to_string();
                    (sha, path, false)
                }
                PersonalFetchResult::NoCacheRoot => {
                    eprintln!("ccync add: could not determine cache root");
                    println!("{}", AddIntakeOutcome::FetchFailed.as_str());
                    return ExitCode::Error;
                }
                PersonalFetchResult::CloneFailed(msg) => {
                    eprintln!("ccync add: fetch failed for '{plugin_id}': {msg}");
                    println!("{}", AddIntakeOutcome::FetchFailed.as_str());
                    return ExitCode::Error;
                }
            };

        match inspect_render_contribution(&plugin_id, &component_dir) {
            Ok(true) => {}
            Ok(false) => {
                print_no_contribution_guidance();
                return ExitCode::Error;
            }
            Err(error) => {
                eprintln!("ccync add: could not inspect source contribution: {error}");
                println!("{}", AddIntakeOutcome::FetchFailed.as_str());
                return ExitCode::Error;
            }
        }

        let package_name = match resolve_intake_package_name(&plugin_id, &component_dir) {
            Ok(name) => name,
            Err(error) => {
                eprintln!(
                    "ccync add: could not verify package identity for '{plugin_id}': {error}"
                );
                return ExitCode::Error;
            }
        };
        let mut entry = json!({
            "pluginId": plugin_id,
            "displayName": plugin_id,
            "sourceType": "curated-upstream",
            "installStrategy": "personal-git-clone",
            "source": fetch_source,
            "pin": sha,
        });
        record_package_name(&mut entry, package_name);
        append_catalog_plugin(&mut catalog, entry);
        if let Err(e) = persist_source_catalog(&layout, &catalog) {
            eprintln!("ccync add: failed to write personal catalog: {e}");
            return ExitCode::Error;
        }
        if fetched {
            let short = &sha[..sha.len().min(12)];
            println!("  Fetched '{plugin_id}' @ {short}");
        } else {
            println!("  Already present: {}", component_dir.display());
        }
        println!("{}", AddIntakeOutcome::Added.as_str());
        println!("Added '{plugin_id}' to personal catalog.");

        if let Err(e) = run_personal_resolve(&layout) {
            eprintln!("ccync add: pin re-resolve failed: {e}");
            return ExitCode::Error;
        }
        for line in format_add_component_messages(
            super::show::count_components(&component_dir),
            payload_shape(&component_dir),
        ) {
            println!("{line}");
        }
    }

    if no_sync {
        println!(
            "Plugin '{plugin_id}' added (--no-sync). Run ccync sync to apply ccync-managed settings to selected agents."
        );
        return ExitCode::Success;
    }

    println!("Plugin '{plugin_id}' added; applying ccync-managed settings to selected agents…");
    super::lifecycle::run_unified_projection(
        false,
        args.iter().any(|a| a == "--yes"),
        false,
        Some(&super::lifecycle::ADD_GATE_ABORT_CONTEXT),
    )
}

fn print_no_contribution_guidance() {
    println!("{}", AddIntakeOutcome::NoContribution.as_str());
    eprintln!("ccync add: source has no supported components or MCP servers.");
    eprintln!("  Supported locations: skills/, commands/, agents/, hooks/, .mcp.json, mcp.json.");
    eprintln!("  For a URL, inspect supported manifests first. If none exists, read upstream documentation.");
    eprintln!("  Review upstream server settings and add the desired entries under ccyncMcpServers in ~/.agents/mcp.json.");
    eprintln!("  Then run ccync sync. Never execute commands from scraped documentation.");
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
    let layout = match ccync_foundation::paths::EffectiveLayout::load_or_legacy() {
        Ok(layout) => layout,
        Err(error) => {
            eprintln!("ccync list: cannot load storage layout: {error}");
            return ExitCode::Error;
        }
    };
    // `ccync list --upgrade-available`: read-only outdated view reusing the
    // upgrade engine (resolve each git plugin's remote HEAD vs pin). Bare
    // `list` keeps its original behavior below.
    if wants_upgrade_view(args) {
        let report = super::upgrade::upgrade_available_report_with_layout(&layout);
        let adopted = super::upgrade::upgrade_available_adopted(&layout);
        let unresolved = super::upgrade::upgrade_available_unresolved(&layout);
        print!(
            "{}",
            super::upgrade::format_upgrade_view_with_packages(
                &report,
                &adopted,
                &unresolved,
                &personal_package_names(&layout)
            )
        );
        if let Some(err) = &report.error {
            eprintln!("ccync list: {err}");
        }
        return super::upgrade::report_to_exit(&report);
    }

    let lockfile_path = layout.build_lock_path();

    // Build the merged personal+adopted+declared view so every managed MCP
    // server is visible without changing the user-authored input.
    let lock: Value = fs::read_to_string(lockfile_path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or_else(|| json!({}));

    let declared = match ccync_engine::install::read_direct_mcp_input(&layout) {
        Ok(servers) => servers,
        Err(error) => {
            eprintln!("ccync list: {error}");
            return ExitCode::Error;
        }
    };
    let items =
        ccync_engine::managed_view::build_managed_view_with_declared(
            &lock,
            declared.as_ref().map(|input| input.servers()),
        );
    let mut output = format_list_view(&items);
    if !items.is_empty() {
        let catalog = fs::read_to_string(layout.personal_catalog_path())
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .unwrap_or_else(|| json!({}));
        for line in package_identity_lines(&catalog) {
            output.push_str(&line);
            output.push('\n');
        }
    }
    for diagnostic in load_provider_diagnostics(&lock) {
        output.push_str(&format!(
            "Provider: {} | managed by {} | loader: {} | ownership: {:?} | validation: {:?}\n",
            diagnostic.provider,
            diagnostic.manager,
            diagnostic.effective_loader,
            diagnostic.ownership,
            diagnostic.verification_scope
        ));
    }
    print!("{output}");
    ExitCode::Success
}

/// One line per personal plugin whose verified package name differs from its
/// management ID. The canonical package name is display identity; the
/// management ID is the selector every `ccync` command accepts and the one
/// the suggested command names.
pub(crate) fn package_identity_lines(catalog: &Value) -> Vec<String> {
    catalog
        .get("plugins")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry.get("pluginId").and_then(Value::as_str)?;
            let package = entry.get("packageName").and_then(Value::as_str)?;
            (package != id).then(|| {
                format!("Package: {package} | management ID: {id} (remove with: ccync remove {id})")
            })
        })
        .collect()
}

/// Verified package name per management ID, read from the personal catalog.
/// An entry whose package name equals its ID is omitted, since there is no
/// separate canonical identity to show.
pub(crate) fn personal_package_names(
    layout: &ccync_foundation::paths::EffectiveLayout,
) -> std::collections::BTreeMap<String, String> {
    let catalog = fs::read_to_string(layout.personal_catalog_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or_else(|| json!({}));
    catalog
        .get("plugins")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry.get("pluginId").and_then(Value::as_str)?;
            let package = entry.get("packageName").and_then(Value::as_str)?;
            (package != id).then(|| (id.to_string(), package.to_string()))
        })
        .collect()
}

fn load_provider_diagnostics(lock: &Value) -> Vec<ccync_engine::managed_view::ProviderDiagnostic> {
    ccync_engine::managed_view::provider_diagnostics(lock)
}

/// Render the bare `ccync list` view over the merged managed set (personal +
/// adopted, via `build_managed_view`). Pure (returns the text) so it is
/// testable without a stdout-capture seam — mirrors `format_upgrade_view`.
///
/// Empty managed view (no personal AND no adopted items) → the original
/// "nothing installed" hint. Otherwise a table with an ORIGIN/KIND column
/// pair so adopted items are visibly distinguished from personal ones; the
/// PIN column shows the resolved sha (truncated to 12 chars) for personal
/// entries and `-` for adopted entries (which carry no pin in the model).
pub(crate) fn format_list_view(items: &[ccync_engine::ManagedItem]) -> String {
    if items.is_empty() {
        return "No personal plugins installed.\nAdd one with: ccync add <git-url>\n".to_string();
    }

    let mut out = String::new();
    out.push_str(&format!(
        "{:<30} {:<10} {:<10} {:<14} SOURCE\n",
        "ID", "ORIGIN", "KIND", "PIN"
    ));
    out.push_str(&format!("{}\n", "-".repeat(90)));
    for item in items {
        let origin = match item.origin {
            ccync_engine::ManagedOrigin::Personal => "personal",
            ccync_engine::ManagedOrigin::Adopted => "adopted",
            ccync_engine::ManagedOrigin::Declared => "declared",
        };
        let kind = match item.kind {
            ccync_engine::ManagedItemKind::Plugin => "plugin",
            ccync_engine::ManagedItemKind::McpServer => "mcp",
            ccync_engine::ManagedItemKind::Skill => "skill",
        };
        let pin_display = match &item.pin {
            Some(sha) => {
                let s: String = sha.chars().take(12).collect();
                s
            }
            None => "-".to_string(),
        };
        let source = item
            .source
            .as_deref()
            .unwrap_or_else(|| missing_source_label(item.origin, item.kind));
        out.push_str(&format!(
            "{:<30} {:<10} {:<10} {:<14} {}\n",
            item.id, origin, kind, pin_display, source
        ));
    }
    out
}

/// Stable SOURCE-column label for a managed item with no resolvable
/// `source` reference. Adopted MCP servers and loose skills never carry a
/// source (config entry / local content, no upstream) — label them instead
/// of falling back to a bare `?`. Personal and adopted plugin rows keep the
/// bare `?` fallback: an unresolved plugin source is an actionable gap
/// (`ccync repair` for an adopted row, `ccync search` + `ccync add` for a
/// personal one), not an inherent kind property.
fn missing_source_label(
    origin: ccync_engine::ManagedOrigin,
    kind: ccync_engine::ManagedItemKind,
) -> &'static str {
    match (origin, kind) {
        (ccync_engine::ManagedOrigin::Adopted, ccync_engine::ManagedItemKind::McpServer) => {
            "(agent MCP config)"
        }
        (ccync_engine::ManagedOrigin::Adopted, ccync_engine::ManagedItemKind::Skill) => {
            "(agent loose skill)"
        }
        _ => "?",
    }
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

    // Machine-initialization preflight: every remove auto-syncs to live agent
    // surfaces (unlike `add`, there is no `--no-sync`-style exception), so it
    // must refuse before the first personal-catalog/adopted-lockfile read-to-
    // rewrite below — a true zero-mutation abort on an uninitialized machine.
    if let Err(code) = super::lifecycle::require_initialized_machine() {
        return code;
    }

    let layout = match ccync_foundation::paths::EffectiveLayout::load_or_legacy() {
        Ok(layout) => layout,
        Err(error) => {
            eprintln!("ccync remove: cannot load storage layout: {error}");
            return ExitCode::Error;
        }
    };
    let catalog_path = layout.personal_catalog_path();

    // Try the personal catalog first. If the catalog is absent or the item is
    // not in it, fall through to the adopted fallback below.
    let mut catalog =
        read_personal_catalog(&catalog_path).unwrap_or_else(|| json!({ "plugins": [] }));
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

    if before_len > after_len {
        // Found in personal catalog — existing personal removal path unchanged.
        if let Err(e) = persist_source_catalog(&layout, &catalog) {
            eprintln!("ccync remove: failed to write personal catalog: {e}");
            return ExitCode::Error;
        }
        if let Err(e) = run_personal_resolve(&layout) {
            eprintln!("ccync remove: resolve step failed: {e}");
            return ExitCode::Error;
        }
        if let Err(e) = run_render() {
            eprintln!("ccync remove: render step failed: {e}");
            return ExitCode::Error;
        }
        println!(
            "Plugin '{plugin_id}' removed; applying ccync-managed settings to selected agents…"
        );
        return super::lifecycle::run_unified_projection(
            false,
            args.iter().any(|a| a == "--yes"),
            false,
            Some(&super::lifecycle::REMOVE_GATE_ABORT_CONTEXT),
        );
    }

    // Not in personal catalog — try adopted fallback via the lockfile.
    let adopted_result = ccync_engine::remove_adopted_item(plugin_id, &layout.build_lock_path());

    match adopted_result {
        Ok(true) => {
            // Adopted item removed — skip personal resolve; prune and re-project.
            if let Err(e) = run_render() {
                eprintln!("ccync remove: render step failed: {e}");
                return ExitCode::Error;
            }
            println!("Adopted item '{plugin_id}' removed; applying ccync-managed settings to selected agents…");
            super::lifecycle::run_unified_projection(
                false,
                args.iter().any(|a| a == "--yes"),
                false,
                Some(&super::lifecycle::REMOVE_GATE_ABORT_CONTEXT),
            )
        }
        Ok(false) => {
            eprintln!("ccync remove: '{plugin_id}' not found (not personal or adopted).");
            ExitCode::Error
        }
        Err(e) => {
            eprintln!("ccync remove: failed to remove adopted item: {e}");
            ExitCode::Error
        }
    }
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
    let no_git = last.trim_end_matches(".git");
    sanitize_plugin_id(strip_archive_suffix(no_git))
}

/// Strip a trailing archive extension (`.zip`, `.tar.gz`, `.tgz`,
/// case-insensitive) so an archive-derived plugin ID matches the git/local
/// forms of the same name. The source filename stays the only ID authority —
/// this never inspects archive contents/metadata.
fn strip_archive_suffix(name: &str) -> &str {
    let lower = name.to_ascii_lowercase();
    for suffix in [".tar.gz", ".tgz", ".zip"] {
        if lower.ends_with(suffix) {
            return &name[..name.len() - suffix.len()];
        }
    }
    name
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

pub(crate) use ccync_engine::install::{read_personal_catalog, write_json_atomic};

fn append_catalog_plugin(catalog: &mut Value, entry: Value) {
    catalog
        .as_object_mut()
        .expect("catalog root was validated before add")
        .entry("plugins")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .expect("catalog plugins were validated before add")
        .push(entry);
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

/// The package name a fetched source declares, resolved by the same engine
/// resolver that projection uses. Runs after fetch and before any catalog
/// write, so a rejected identity leaves the personal catalog unchanged.
fn resolve_intake_package_name(
    plugin_id: &str,
    component_dir: &std::path::Path,
) -> Result<Option<String>, String> {
    authoritative_plugin_name(plugin_id, component_dir, None).map_err(|error| error.to_string())
}

/// Record the verified package name beside the management `pluginId`. The
/// key matches the catalog field the engine resolver reads back, and a source
/// that declares no name leaves the entry unchanged.
fn record_package_name(entry: &mut Value, package_name: Option<String>) {
    if let Some(name) = package_name {
        entry["packageName"] = json!(name);
    }
}

/// Run personal catalog resolution to update `~/.ccync/build/lock.json`.
/// The public catalog is embedded in the binary.
pub(crate) fn run_personal_resolve(
    layout: &ccync_foundation::paths::EffectiveLayout,
) -> Result<(), String> {
    use ccync_engine::catalog::run_resolve_catalog;

    let config_path = layout.machine_config_path();
    let lockfile_path = layout.build_lock_path();

    let opts = ResolveCatalogOptions {
        catalog_path: None,
        config_path,
        lockfile_path,
        dry_run: false,
        personal_catalog_path_override: Some(layout.personal_catalog_path()),
    };
    let resolution = run_resolve_catalog(&opts)?;
    for e in &resolution.errors {
        eprintln!("ccync: catalog resolve warning: {e}");
    }
    Ok(())
}

fn persist_source_catalog(
    layout: &ccync_foundation::paths::EffectiveLayout,
    catalog: &Value,
) -> Result<(), String> {
    ccync_engine::catalog::persist_source_change(
        layout,
        |stored| {
            *stored = catalog.clone();
            Ok(())
        },
        || Ok(()),
    )
}

/// Re-render canonical root so personal plugin components are (re)projected.
fn run_render() -> Result<(), String> {
    use ccync_engine::{config::CcyncConfig, install::run_update_with_evidence};
    let config = CcyncConfig::load();
    run_update_with_evidence(&config)
        .map(|_| ())
        .map_err(|e| format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct LayoutEnvGuard(Vec<(String, Option<std::ffi::OsString>)>);

    impl LayoutEnvGuard {
        fn new(home: &std::path::Path) -> Self {
            let values = [
                ("USERPROFILE", home.to_path_buf()),
                ("HOME", home.to_path_buf()),
                ("APPDATA", home.join("AppData").join("Roaming")),
                ("LOCALAPPDATA", home.join("AppData").join("Local")),
                ("XDG_CONFIG_HOME", home.join(".config")),
                ("XDG_STATE_HOME", home.join(".local").join("state")),
                ("XDG_CACHE_HOME", home.join(".cache")),
            ];
            let saved = values
                .iter()
                .map(|(key, _)| ((*key).to_string(), std::env::var_os(key)))
                .collect();
            for (key, value) in values {
                unsafe { std::env::set_var(key, value) };
            }
            Self(saved)
        }
    }

    impl Drop for LayoutEnvGuard {
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

    #[test]
    fn handler_layout_paths_committed_fixture_and_usage_errors() {
        let _env_lock = crate::test_io_lock();
        let home = tempfile::tempdir().unwrap();
        let _env = LayoutEnvGuard::new(home.path());

        let resolved = ccync_foundation::paths::StorageRoots::resolve().unwrap();
        let roots = [
            ("portable", resolved.portable),
            ("config", resolved.config),
            ("state", resolved.state),
            ("cache", resolved.cache),
        ];
        let commit = ccync_foundation::provider::StorageLayoutCommit {
            layout_version: 1,
            roots: roots
                .iter()
                .map(
                    |(name, path)| ccync_foundation::provider::LayoutRootIdentity {
                        name: (*name).to_string(),
                        path: path.clone(),
                    },
                )
                .collect(),
        };
        let bytes = serde_json::to_vec(&commit).unwrap();
        let sentinel = home.path().join(".ccync").join("migration.json");
        let state_layout = roots[2].1.join("layout.json");
        std::fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
        std::fs::create_dir_all(state_layout.parent().unwrap()).unwrap();
        std::fs::write(sentinel, &bytes).unwrap();
        std::fs::write(state_layout, &bytes).unwrap();

        let catalog_path = roots[1].1.join("plugins.json");
        let lock_path = roots[2].1.join("build").join("lock.json");
        let mcp_path = roots[1].1.join("mcp.json");
        let cache_entry = roots[2].1.join("sources").join("layout-probe@abc123");
        std::fs::create_dir_all(catalog_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(cache_entry.join("skills")).unwrap();
        std::fs::write(
            &catalog_path,
            r#"{"schemaVersion":1,"plugins":[{"pluginId":"layout-probe","source":"https://example.invalid/layout-probe.git","installStrategy":"personal-git-clone","pin":"abc123"}]}"#,
        )
        .unwrap();
        std::fs::write(
            &lock_path,
            r#"{"_personalPlugins":[{"pluginId":"layout-probe","source":"https://example.invalid/layout-probe.git","pin":"abc123"}]}"#,
        )
        .unwrap();
        std::fs::write(
            &mcp_path,
            r#"{"mcpServers":{"layout-probe-server":{"command":"fixture"}}}"#,
        )
        .unwrap();
        std::fs::write(cache_entry.join("skills").join("probe.md"), "# probe\n").unwrap();
        std::fs::write(roots[1].1.join("config.json"), r#"{"targets":[]}"#).unwrap();

        let legacy = home.path().join(".ccync");
        let legacy_files = [
            (
                "plugins.json",
                br#"{"plugins":[{"pluginId":"legacy-decoy"}]}"#.as_slice(),
            ),
            (
                "build/lock.json",
                br#"{"_personalPlugins":[{"pluginId":"legacy-decoy"}]}"#.as_slice(),
            ),
            ("mcp.json", b"not valid mcp json".as_slice()),
            (
                "cache/legacy-decoy@different",
                b"legacy cache decoy".as_slice(),
            ),
        ];
        for (relative, content) in legacy_files {
            let path = legacy.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }

        fn legacy_snapshot(
            root: &std::path::Path,
        ) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
            fn is_live_surface(relative: &std::path::Path) -> bool {
                let path = relative.to_string_lossy().replace('\\', "/");
                [
                    "plugins/",
                    "source/",
                    "build/render/",
                    "build/active/",
                    "data/",
                    "install-receipts/",
                ]
                .iter()
                .any(|prefix| path.starts_with(prefix))
                    || path == "build/lifecycle.lock"
            }

            let mut files = std::collections::BTreeMap::new();
            let mut pending = vec![root.to_path_buf()];
            while let Some(directory) = pending.pop() {
                for entry in std::fs::read_dir(directory).unwrap() {
                    let path = entry.unwrap().path();
                    let relative = path.strip_prefix(root).unwrap().to_path_buf();
                    if is_live_surface(&relative) {
                        continue;
                    }
                    if path.is_dir() {
                        pending.push(path);
                    } else {
                        files.insert(relative, std::fs::read(path).unwrap());
                    }
                }
            }
            files
        }

        let legacy_before = legacy_snapshot(&legacy);

        let layout = ccync_foundation::paths::EffectiveLayout::load_or_legacy().unwrap();
        assert_eq!(
            layout.authority(),
            ccync_foundation::paths::EffectiveLayoutAuthority::Committed
        );
        assert_eq!(
            layout.personal_catalog_path(),
            roots[1].1.join("plugins.json")
        );
        assert_eq!(
            layout.build_lock_path(),
            roots[2].1.join("build").join("lock.json")
        );
        assert_eq!(layout.cache_path(), roots[2].1.join("sources"));
        assert_eq!(layout.mcp_input_path(), home.path().join(".agents/mcp.json"));
        assert_eq!(
            layout.tool_operation_journal_path(),
            roots[2].1.join("build/tools/operation.txn.json")
        );

        assert_eq!(
            super::super::show::cmd_show(&["show".into(), "layout-probe".into()]),
            ExitCode::Success
        );
        assert_eq!(cmd_list(&["list".into()]), ExitCode::Success);
        assert_eq!(
            super::super::pin::cmd_pin(&["pin".into(), "layout-probe".into()], None),
            ExitCode::Success
        );
        let held: Value = serde_json::from_slice(&std::fs::read(&catalog_path).unwrap()).unwrap();
        assert_eq!(held["plugins"][0]["held"], true);
        assert_eq!(
            super::super::pin::cmd_pin(
                &["pin".into(), "--remove".into(), "layout-probe".into()],
                None
            ),
            ExitCode::Success
        );
        let released: Value =
            serde_json::from_slice(&std::fs::read(&catalog_path).unwrap()).unwrap();
        assert_ne!(released["plugins"][0]["held"], true);
        assert_eq!(
            cmd_remove(&["remove".into(), "layout-probe".into(), "--yes".into()]),
            ExitCode::Success
        );
        let removed: Value =
            serde_json::from_slice(&std::fs::read(&catalog_path).unwrap()).unwrap();
        assert_eq!(removed["plugins"].as_array().unwrap().len(), 0);
        assert_eq!(
            super::super::cleanup::cmd_cleanup(&["cleanup".into(), "--dry-run".into()]),
            ExitCode::Success
        );
        assert_eq!(legacy_snapshot(&legacy), legacy_before);

        assert_eq!(
            super::super::cleanup::cmd_cleanup(&["cleanup".into(), "--invalid".into()]),
            ExitCode::Usage
        );
        assert_eq!(
            super::super::pin::cmd_pin(&["pin".into(), "--invalid".into()], None),
            ExitCode::Usage
        );
        assert_eq!(
            super::super::repair::cmd_repair(&["repair".into(), "--invalid".into()]),
            ExitCode::Usage
        );
        assert_eq!(
            super::super::upgrade::cmd_upgrade(&["upgrade".into(), "--invalid".into()], None),
            ExitCode::Usage
        );
        assert_eq!(
            cmd_add(&["add".into(), "--invalid".into()]),
            ExitCode::Usage
        );
        assert_eq!(
            cmd_list(&["list".into(), "--invalid".into()]),
            ExitCode::Usage
        );
        assert_eq!(
            cmd_remove(&["remove".into(), "--invalid".into()]),
            ExitCode::Usage
        );
        assert_eq!(
            super::super::roster::cmd_roster(&["roster".into()]),
            ExitCode::Usage
        );
        assert_eq!(
            super::super::show::cmd_show(&["show".into(), "--invalid".into()]),
            ExitCode::Usage
        );
        assert_eq!(
            super::super::doctor::cmd_doctor(&["doctor".into(), "--invalid".into()]),
            ExitCode::Usage
        );
    }

    // ── reworded sync-hint literals ────────────────────────────────────────────

    #[test]
    fn sync_hint_literals_are_byte_exact_and_old_literals_are_gone() {
        let this_file = include_str!("plugin.rs");
        for needle in [
            "Plugin '{plugin_id}' added (--no-sync). Run ccync sync to apply ccync-managed settings to selected agents.",
            "Plugin '{plugin_id}' added; applying ccync-managed settings to selected agents…",
            "Plugin '{plugin_id}' removed; applying ccync-managed settings to selected agents…",
            "Adopted item '{plugin_id}' removed; applying ccync-managed settings to selected agents…",
        ] {
            assert!(
                this_file.contains(needle),
                "reworded sync-hint literal must be present byte-exact: {needle}"
            );
        }

        // Needles are assembled at runtime (split across literals) so this
        // test's own source text does not trip its own assertion via
        // include_str!, mirroring lifecycle.rs's removed_suppression_helper_never_reappears.
        let old_no_sync = ["Run `ccync sync`", " to project to all runtimes."].concat();
        let old_add = ["added; syncing", " to all runtimes…"].concat();
        let old_remove = ["' removed; syncing", " to all runtimes…"].concat();
        assert_eq!(
            this_file.matches(&old_no_sync).count(),
            0,
            "old --no-sync hint must be gone"
        );
        assert_eq!(
            this_file.matches(&old_add).count(),
            0,
            "old add sync-hint must be gone"
        );
        assert_eq!(
            this_file.matches(&old_remove).count(),
            0,
            "old remove sync-hint (both plugin and adopted-item variants) must be gone"
        );
    }

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

        let layout = ccync_foundation::paths::EffectiveLayout::load_or_legacy().unwrap();
        let result = run_personal_resolve(&layout);

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
    // rollback.rs) that serializes every test in this binary that
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

        /// Mark `home` as an already-initialized machine (`~/.ccync/config.json`
        /// present) so a projecting (non-`--no-sync`) `cmd_add` call reaches its
        /// existing gate/projection behavior instead of being refused by the
        /// machine-initialization preflight. Tests that exercise `add`'s
        /// pre-existing gate/fetch/live-surface behavior need this; the
        /// preflight itself is covered separately by the `pre_init_add_guard`
        /// tests below.
        fn seed_initialized_config(home: &std::path::Path) {
            let ccync = home.join(".ccync");
            std::fs::create_dir_all(&ccync).unwrap();
            std::fs::write(ccync.join("config.json"), r#"{}"#).unwrap();
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
        fn make_source_repo_with_files(dir: &std::path::Path, files: &[(&str, &str)]) {
            fs::create_dir_all(dir).unwrap();
            fs::write(dir.join("README.md"), "# fixture plugin\n").unwrap();
            for (rel, contents) in files {
                let path = dir.join(rel);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).unwrap();
                }
                fs::write(path, contents).unwrap();
            }
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

        fn make_source_repo(dir: &std::path::Path) {
            make_source_repo_with_files(dir, &[]);
        }

        /// The foundation-crate-resolved live surfaces that a completed
        /// projection would populate: canonical render root and the
        /// Claude skills dir, both scoped under the fake home.
        fn canonical_root_exists(fake_home: &std::path::Path) -> bool {
            fake_home
                .join(".ccync")
                .join("build")
                .join("render")
                .exists()
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
            seed_initialized_config(fake_home.path());

            let source = TempDir::new().unwrap();
            make_source_repo_with_files(source.path(), &[("skills/demo.md", "# skill\n")]);

            let args = vec!["add".to_string(), source.path().display().to_string()];
            let exit = cmd_add(&args);

            assert_eq!(
                exit,
                ExitCode::Error,
                "non-interactive decline must return Error so gate abort is distinguishable from full success"
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

            // The detailed gate-abort wording is asserted in lifecycle.rs helper tests;
            // this integration-style test focuses on exit code plus written/unwritten state.
        }

        /// A source whose folder name differs from its declared package name
        /// records both identities: `pluginId` stays derived from the source,
        /// and `packageName` holds the verified manifest name. A source with
        /// an invalid manifest name is rejected with the catalog bytes unchanged
        /// and no live surface written.
        #[test]
        fn p7_t03_intake_records_package_identity_apart_from_plugin_id() {
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());
            let sources = TempDir::new().unwrap();
            let skill = (
                "skills/craft/SKILL.md",
                "---\nname: craft\ndescription: Craft\n---\n",
            );

            let good = sources.path().join("impeccable-kit");
            make_source_repo_with_files(
                &good,
                &[
                    (
                        ".claude-plugin/plugin.json",
                        r#"{"name": "impeccable", "version": "1.0.0"}"#,
                    ),
                    skill,
                ],
            );
            let args = vec![
                "add".to_string(),
                good.display().to_string(),
                "--no-sync".to_string(),
            ];
            assert_eq!(cmd_add(&args), ExitCode::Success);

            let catalog_path =
                ccync_foundation::paths::personal_catalog_path().expect("personal catalog path");
            let before = fs::read(&catalog_path).expect("catalog after accepted add");
            let catalog: Value = serde_json::from_slice(&before).expect("catalog JSON");
            let entry = &catalog["plugins"][0];
            assert_eq!(entry["pluginId"], "impeccable-kit");
            assert_eq!(entry["packageName"], "impeccable");

            let bad = sources.path().join("impeccable-bad");
            make_source_repo_with_files(
                &bad,
                &[
                    (
                        ".claude-plugin/plugin.json",
                        r#"{"name": "bad\u0001name", "version": "1.0.0"}"#,
                    ),
                    skill,
                ],
            );
            let args = vec![
                "add".to_string(),
                bad.display().to_string(),
                "--no-sync".to_string(),
            ];
            assert_eq!(cmd_add(&args), ExitCode::Error);
            assert_eq!(
                fs::read(&catalog_path).expect("catalog after rejected add"),
                before,
                "a rejected identity must leave the personal catalog bytes unchanged"
            );
            assert!(!canonical_root_exists(fake_home.path()));
            assert!(!claude_skills_ccync_exists(fake_home.path()));
        }

        #[test]
        fn p7_t03_bare_skill_source_records_package_identity_apart_from_plugin_id() {
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());
            let sources = TempDir::new().unwrap();

            let good = sources.path().join("graphify-agents");
            make_source_repo_with_files(
                &good,
                &[(
                    "SKILL.md",
                    "---\nname: graphify\ndescription: Graphify skill\n---\n",
                )],
            );
            let args = vec![
                "add".to_string(),
                good.display().to_string(),
                "--no-sync".to_string(),
            ];
            assert_eq!(cmd_add(&args), ExitCode::Success);

            let catalog_path =
                ccync_foundation::paths::personal_catalog_path().expect("personal catalog path");
            let catalog_bytes = fs::read(&catalog_path).expect("catalog after accepted add");
            let catalog: Value = serde_json::from_slice(&catalog_bytes).expect("catalog JSON");
            let entry = &catalog["plugins"][0];
            assert_eq!(entry["pluginId"], "graphify-agents");
            assert_eq!(entry["packageName"], "graphify");
        }

        #[test]
        fn p7_t03_malformed_manifest_source_is_rejected_without_mutation() {
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());
            let sources = TempDir::new().unwrap();

            let catalog_path =
                ccync_foundation::paths::personal_catalog_path().expect("personal catalog path");
            fs::create_dir_all(catalog_path.parent().unwrap()).unwrap();
            let initial_catalog = br#"{"schemaVersion":1,"plugins":[]}"#.to_vec();
            fs::write(&catalog_path, &initial_catalog).unwrap();

            let malformed = sources.path().join("malformed-plugin");
            make_source_repo_with_files(
                &malformed,
                &[
                    (".claude-plugin/plugin.json", "{ invalid json syntax"),
                    (
                        "skills/demo/SKILL.md",
                        "---\nname: demo\ndescription: Demo\n---\n",
                    ),
                ],
            );
            let args = vec![
                "add".to_string(),
                malformed.display().to_string(),
                "--no-sync".to_string(),
            ];
            assert_eq!(cmd_add(&args), ExitCode::Error);
            assert_eq!(
                fs::read(&catalog_path).expect("catalog bytes after rejected add"),
                initial_catalog,
                "malformed manifest must leave the personal catalog bytes unchanged"
            );
            assert!(!canonical_root_exists(fake_home.path()));
            assert!(!claude_skills_ccync_exists(fake_home.path()));
        }

        #[test]
        fn cmd_add_git_fetch_failure_leaves_catalog_unchanged_and_retryable() {
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());
            seed_initialized_config(fake_home.path());

            let ccync = fake_home.path().join(".ccync");
            std::fs::create_dir_all(&ccync).unwrap();
            let catalog = ccync.join("plugins.json");
            let before = "{\"schemaVersion\":1,\"plugins\":[]}";
            std::fs::write(&catalog, before).unwrap();

            let missing = fake_home.path().join("missing-plugin");
            let args = vec!["add".to_string(), missing.display().to_string()];
            let plugin_id = derive_plugin_id(&missing.display().to_string());

            let first = cmd_add(&args);
            assert_eq!(
                first,
                ExitCode::Error,
                "failed fetch must surface as Error, not Usage"
            );
            assert_eq!(
                std::fs::read_to_string(&catalog).unwrap(),
                before,
                "failed fetch must leave plugins.json byte-unchanged"
            );
            assert!(
                !catalog_has_plugin(fake_home.path(), &plugin_id),
                "failed fetch must not leave an orphan catalog entry"
            );

            let second = cmd_add(&args);
            assert_eq!(
                second,
                ExitCode::Error,
                "retry after failed fetch must re-enter fetch instead of duplicate-Usage"
            );
            assert_eq!(
                std::fs::read_to_string(&catalog).unwrap(),
                before,
                "retry after failed fetch must still leave plugins.json byte-unchanged"
            );
            assert!(
                !catalog_has_plugin(fake_home.path(), &plugin_id),
                "retry after failed fetch must still have no catalog entry"
            );
        }

        #[test]
        fn cmd_add_gate_accepted_writes_live_surfaces() {
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());
            seed_initialized_config(fake_home.path());

            let source = TempDir::new().unwrap();
            make_source_repo_with_files(source.path(), &[("skills/demo.md", "# skill\n")]);

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
                !claude_skills_ccync_exists(fake_home.path()),
                "accepted gate must retire the combined Claude skills surface once direct publication succeeds"
            );
            // Live MCP config surfaces (~/.claude.json etc.) are intentionally
            // NOT asserted here: the `mcp` crate now resolves home via
            // `ccync_foundation::paths` (env-based), so this fake-HOME
            // override does reach it — assertion coverage lives in `mcp`'s
            // own env-override integration test instead of duplicating it here.
        }

        // ── pre-init add guard (machine-initialization preflight) ─────────────
        //
        // Mirrors lifecycle.rs's pre-init sync guard tests: a projecting
        // (non-`--no-sync`) `add` must refuse before ANY domain mutation on an
        // uninitialized machine, while `add <source> --no-sync` (the
        // exception: cache/catalog only, never a live agent surface) stays
        // available pre-init.

        /// Recursive snapshot of every file under `dir`: (relative path, byte
        /// length, modified time). Empty when `dir` does not exist. Used to
        /// prove a refused pre-init add leaves the entire home tree byte- and
        /// mtime-identical (true zero mutation across cache/catalog/lock/live
        /// surfaces), not just "no new catalog entry".
        fn snapshot_tree(dir: &std::path::Path) -> Vec<(String, u64, std::time::SystemTime)> {
            fn walk(
                base: &std::path::Path,
                dir: &std::path::Path,
                out: &mut Vec<(String, u64, std::time::SystemTime)>,
            ) {
                let Ok(read) = std::fs::read_dir(dir) else {
                    return;
                };
                for entry in read.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        walk(base, &path, out);
                    } else if let Ok(meta) = path.metadata() {
                        let rel = path
                            .strip_prefix(base)
                            .unwrap_or(&path)
                            .to_string_lossy()
                            .into_owned();
                        out.push((
                            rel,
                            meta.len(),
                            meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                        ));
                    }
                }
            }
            let mut out = Vec::new();
            walk(dir, dir, &mut out);
            out.sort();
            out
        }

        #[test]
        fn pre_init_add_guard_refuses_projecting_add_with_zero_mutation() {
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());
            // Deliberately NOT seeded — fresh, uninitialized machine.

            let source = TempDir::new().unwrap();
            make_source_repo_with_files(source.path(), &[("skills/demo.md", "# skill\n")]);
            let before = snapshot_tree(fake_home.path());

            let args = vec!["add".to_string(), source.path().display().to_string()];
            let exit = cmd_add(&args);

            let after = snapshot_tree(fake_home.path());

            assert_eq!(
                exit,
                ExitCode::Error,
                "a projecting add on an uninitialized machine must refuse with Error/1"
            );
            assert!(
                before.is_empty(),
                "sanity: nothing should exist in the fresh home before the refused add"
            );
            assert_eq!(
                before, after,
                "a refused pre-init add must leave every file under home byte- and \
                 mtime-identical — zero cache/catalog/lock/live writes"
            );
        }

        #[test]
        fn pre_init_add_guard_no_sync_still_succeeds_pre_init() {
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());
            // Deliberately NOT seeded — fresh, uninitialized machine.

            let source = TempDir::new().unwrap();
            make_source_repo_with_files(source.path(), &[("skills/demo.md", "# skill\n")]);

            let args = vec![
                "add".to_string(),
                source.path().display().to_string(),
                "--no-sync".to_string(),
            ];
            let exit = cmd_add(&args);

            assert_eq!(
                exit,
                ExitCode::Success,
                "add <source> --no-sync (the pre-init exception) must still succeed pre-init"
            );
            let plugin_id = derive_plugin_id(&source.path().display().to_string());
            assert!(
                catalog_has_plugin(fake_home.path(), &plugin_id),
                "pre-init add --no-sync must still create the personal catalog entry"
            );
            assert!(
                fake_home
                    .path()
                    .path_join_ccync("build")
                    .join("lock.json")
                    .exists()
                    || fake_home.path().path_join_ccync("build").exists(),
                "pre-init add --no-sync must still create the resolved lockfile/build state"
            );
        }

        #[test]
        fn pre_init_add_guard_refusal_names_init_and_no_sync_escape_hatch() {
            // Needles are assembled at runtime from fragments so this test's own
            // source text does not trip its own assertion via include_str! (see
            // cmd_add_wires_component_count_at_both_print_sites above for the
            // same technique).
            let this_file = include_str!("plugin.rs");
            let guard_call_needle = ["require_initialized", "_machine()"].concat();
            assert!(
                this_file.contains(&guard_call_needle),
                "cmd_add must call the shared machine-initialization preflight"
            );
            let no_sync_needle = ["ccync add {source}", " --no-sync`"].concat();
            assert!(
                this_file.contains(&no_sync_needle),
                "the pre-init add refusal must name the --no-sync escape hatch"
            );
        }

        #[test]
        fn cmd_add_no_sync_flag_skips_gate_and_leaves_no_live_surfaces() {
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());

            let source = TempDir::new().unwrap();
            make_source_repo_with_files(source.path(), &[("skills/demo.md", "# skill\n")]);

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

        /// A source with no renderable components or MCP servers is refused
        /// before its catalog can change.
        #[test]
        fn cmd_add_no_sync_zero_contribution_leaves_catalog_unchanged() {
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());

            let source = TempDir::new().unwrap();
            make_source_repo(source.path()); // README only → 0 components

            let args = vec![
                "add".to_string(),
                source.path().display().to_string(),
                "--no-sync".to_string(),
            ];
            let catalog_path = fake_home.path().join(".ccync/plugins.json");
            fs::create_dir_all(catalog_path.parent().unwrap()).unwrap();
            let before = br#"{"schemaVersion":1,"plugins":[]}"#.to_vec();
            fs::write(&catalog_path, &before).unwrap();
            assert_eq!(cmd_add(&args), ExitCode::Error);
            assert_eq!(fs::read(&catalog_path).unwrap(), before);
        }

        /// A populated source counts its components without panicking and
        /// still succeeds.
        #[test]
        fn cmd_add_no_sync_nonzero_component_repo_stays_success() {
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());

            let source = TempDir::new().unwrap();
            make_source_repo_with_files(
                source.path(),
                &[
                    ("skills/demo.md", "# skill\n"),
                    ("commands/demo.md", "# command\n"),
                ],
            );

            let args = vec![
                "add".to_string(),
                source.path().display().to_string(),
                "--no-sync".to_string(),
            ];
            assert_eq!(
                cmd_add(&args),
                ExitCode::Success,
                "a populated plugin add must succeed and count its components"
            );
        }

        /// Print-site coverage proves `cmd_add` wires the component-count
        /// message through `count_components` at all add branches (archive,
        /// directory snapshot, and git/plugin), so the pure-helper coverage
        /// above is actually reachable at runtime.
        #[test]
        fn cmd_add_wires_component_count_at_both_print_sites() {
            // Normalize CRLF so the needle fragments below, which are
            // written with bare newlines, match on Windows checkouts too.
            let this_file = include_str!("plugin.rs").replace("\r\n", "\n");
            // Build the needle from fragments so this test's own source literal
            // never contains the full contiguous string and is not self-counted.
            let needle = format!(
                "{}{}",
                "for line in format_add_component_messages(",
                "\n            super::show::count_components(&component_dir),\n            payload_shape(&component_dir)"
            );
            let wired = this_file.matches(&needle).count();
            assert_eq!(
                wired, 3,
                "cmd_add must wire the component-count message at the archive, directory-snapshot, and git/plugin branches"
            );
        }

        #[test]
        fn cmd_remove_dry_run_flag_is_rejected_without_deleting() {
            // `remove <id> --dry-run` must return Usage (64) and leave the
            // personal catalog byte-for-byte unchanged, instead of taking the
            // first non-flag argument and performing a real delete.
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());

            let ccync = fake_home.path().join(".ccync");
            std::fs::create_dir_all(&ccync).unwrap();
            let catalog = ccync.join("plugins.json");
            let before =
                r#"{"plugins":[{"pluginId":"demo","source":"https://example.com/demo.git"}]}"#;
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

        #[test]
        fn cmd_remove_not_found_in_personal_or_adopted_returns_error() {
            // An unknown id is an execution-time lookup failure (Error/1), not
            // a usage/syntax error (Usage/64). This is distinct from the
            // --dry-run unknown-flag rejection above, which stays Usage.
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());

            // Empty personal catalog and empty lockfile → not found anywhere.
            let ccync = fake_home.path().join(".ccync");
            std::fs::create_dir_all(&ccync).unwrap();
            std::fs::write(ccync.join("plugins.json"), r#"{"plugins":[]}"#).unwrap();
            std::fs::write(ccync.join("plugins.lock"), r#"{"_adoptedItems":[]}"#).unwrap();

            let exit = cmd_remove(&["remove".to_string(), "ghost-plugin".to_string()]);
            assert_eq!(exit, ExitCode::Error);
        }

        // ── route local sources by filesystem identity ──────────────

        fn catalog_install_strategy(
            fake_home: &std::path::Path,
            plugin_id: &str,
        ) -> Option<String> {
            let catalog_path = fake_home.path_join_ccync("plugins.json");
            let catalog = read_personal_catalog(&catalog_path)?;
            catalog
                .get("plugins")?
                .as_array()?
                .iter()
                .find(|p| p.get("pluginId").and_then(Value::as_str) == Some(plugin_id))?
                .get("installStrategy")
                .and_then(Value::as_str)
                .map(str::to_string)
        }

        #[test]
        fn local_directory_source_snapshots_without_git_fatal() {
            // A plain directory with no `.git` must not be handed to `git
            // clone` at all — no `fatal:` failure — and must be routed to
            // the immutable-snapshot fetcher with a non-git installStrategy
            // so `ccync upgrade` skips it.
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());

            let source = TempDir::new().unwrap();
            fs::create_dir_all(source.path()).unwrap();
            fs::write(source.path().join("README.md"), "# plain dir\n").unwrap();
            fs::create_dir_all(source.path().join("skills")).unwrap();
            fs::write(source.path().join("skills/demo.md"), "# skill\n").unwrap();

            let args = vec![
                "add".to_string(),
                source.path().display().to_string(),
                "--no-sync".to_string(),
            ];
            let exit = cmd_add(&args);

            assert_eq!(
                exit,
                ExitCode::Success,
                "a plain local directory must add successfully, not fail with a git error"
            );

            let plugin_id = derive_plugin_id(&source.path().display().to_string());
            assert!(
                catalog_has_plugin(fake_home.path(), &plugin_id),
                "the directory snapshot must be recorded in the personal catalog"
            );
            assert_eq!(
                catalog_install_strategy(fake_home.path(), &plugin_id).as_deref(),
                Some("personal-dir-snapshot"),
                "a plain directory must use a non-git installStrategy so upgrade skips it"
            );
        }

        #[test]
        fn local_git_working_copy_stays_on_git_fetcher() {
            // A directory containing `.git` (a normal clone) must keep using
            // the existing `git clone` pipeline, not the directory-snapshot path.
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());

            let source = TempDir::new().unwrap();
            make_source_repo_with_files(source.path(), &[("skills/demo.md", "# skill\n")]);

            let args = vec![
                "add".to_string(),
                source.path().display().to_string(),
                "--no-sync".to_string(),
            ];
            assert_eq!(cmd_add(&args), ExitCode::Success);

            let plugin_id = derive_plugin_id(&source.path().display().to_string());
            assert_eq!(
                catalog_install_strategy(fake_home.path(), &plugin_id).as_deref(),
                Some("personal-git-clone"),
                "a `.git`-directory working copy must stay on the git-clone installStrategy"
            );
        }

        #[test]
        fn local_git_linked_worktree_stays_on_git_fetcher() {
            // A linked worktree has a `.git` *file* (not a directory) at its
            // root, pointing at the real git dir — must still be classified
            // as a git working copy, not a plain snapshot.
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());

            let source = TempDir::new().unwrap();
            fs::create_dir_all(source.path()).unwrap();
            fs::write(source.path().join("README.md"), "# fixture plugin\n").unwrap();
            // Simulate a linked worktree's `.git` file (real content format is
            // `gitdir: <path>`, but is_git_working_copy only checks existence).
            fs::write(
                source.path().join(".git"),
                "gitdir: /somewhere/.git/worktrees/x\n",
            )
            .unwrap();

            assert!(
                is_git_working_copy(source.path()),
                "a `.git` file must be recognized as a git working copy"
            );
            assert!(
                !is_plain_directory_snapshot(&source.path().display().to_string()),
                "a linked worktree must not be routed to the directory-snapshot fetcher"
            );
        }

        // ── pre-init remove guard (machine-initialization preflight) ──────────
        //
        // Mirrors pre_init_add_guard above, but `remove` has no
        // `--no-sync`-style exception: unlike `add`, it is *unconditionally*
        // guarded pre-init — there is no escape hatch to name in the refusal.

        #[test]
        fn pre_init_remove_guard_refuses_with_zero_mutation() {
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());
            // Deliberately NOT seeded — fresh, uninitialized machine.

            let ccync = fake_home.path().join(".ccync");
            fs::create_dir_all(&ccync).unwrap();
            fs::write(
                ccync.join("plugins.json"),
                r#"{"schemaVersion":1,"plugins":[{"pluginId":"demo","source":"https://example.com/demo.git"}]}"#,
            )
            .unwrap();
            fs::write(
                ccync.join("plugins.lock"),
                r#"{"_adoptedItems":[{"name":"adopted-tool","sourceId":"claude-plugin","source":"mkt:adopted-tool@1.0.0"}]}"#,
            )
            .unwrap();
            let cache_dir = ccync.join("cache").join("demo@abc123");
            fs::create_dir_all(&cache_dir).unwrap();
            fs::write(cache_dir.join("marker.txt"), "cached\n").unwrap();
            let live_dir = fake_home
                .path()
                .join(".claude")
                .join("skills")
                .join("ccync");
            fs::create_dir_all(&live_dir).unwrap();
            fs::write(live_dir.join("marker.txt"), "live\n").unwrap();

            let before = snapshot_tree(fake_home.path());

            let exit = cmd_remove(&["remove".to_string(), "demo".to_string()]);

            let after = snapshot_tree(fake_home.path());

            assert_eq!(
                exit,
                ExitCode::Error,
                "remove on an uninitialized machine must refuse with Error/1"
            );
            assert_eq!(
                before, after,
                "a refused pre-init remove must leave every file under home byte- and \
                 mtime-identical — zero personal/adopted/catalog/cache/live mutation"
            );
        }

        #[test]
        fn pre_init_remove_guard_refusal_calls_shared_preflight() {
            // Needle assembled from fragments so this file's own literal text
            // does not self-match — same technique as
            // pre_init_add_guard_refusal_names_init_and_no_sync_escape_hatch.
            let this_file = include_str!("plugin.rs");
            let guard_call_needle = ["require_initialized", "_machine()"].concat();
            let occurrences = this_file.matches(&guard_call_needle).count();
            assert!(
                occurrences >= 2,
                "cmd_remove must also call the shared machine-initialization preflight \
                 (expected at least one call site each for add and remove)"
            );
        }

        #[test]
        fn pre_init_remove_guard_unknown_id_after_init_still_errors() {
            // Regression: the pre-init guard must not change the
            // already-initialized unknown-id lookup-failure behavior — still
            // an execution-time Error(1), not a usage/syntax error.
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());
            seed_initialized_config(fake_home.path());

            let ccync = fake_home.path().join(".ccync");
            fs::create_dir_all(&ccync).unwrap();
            fs::write(ccync.join("plugins.json"), r#"{"plugins":[]}"#).unwrap();
            fs::write(ccync.join("plugins.lock"), r#"{"_adoptedItems":[]}"#).unwrap();

            let exit = cmd_remove(&["remove".to_string(), "ghost-plugin".to_string()]);
            assert_eq!(
                exit,
                ExitCode::Error,
                "unknown-id remove on an initialized machine must still be Error/1"
            );
        }

        #[test]
        fn cmd_add_outcome_added_no_sync_exits_zero() {
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());

            let source = TempDir::new().unwrap();
            make_source_repo_with_files(source.path(), &[("skills/demo.md", "# skill\n")]);

            let args = vec![
                "add".to_string(),
                source.path().display().to_string(),
                "--no-sync".to_string(),
            ];
            let exit = cmd_add(&args);
            assert_eq!(
                exit,
                ExitCode::Success,
                "added --no-sync must return ExitCode::Success (0)"
            );
            let plugin_id = derive_plugin_id(&source.path().display().to_string());
            assert!(
                catalog_has_plugin(fake_home.path(), &plugin_id),
                "added plugin must be recorded in the personal catalog"
            );
        }

        #[test]
        fn cmd_add_outcome_projection_failure_exits_one_and_retains_catalog_row() {
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());
            seed_initialized_config(fake_home.path());

            let source = TempDir::new().unwrap();
            make_source_repo_with_files(source.path(), &[("skills/demo.md", "# skill\n")]);

            // Without --yes and without --no-sync in non-interactive environment:
            // projection gate is declined, returning Error (1)
            let args = vec!["add".to_string(), source.path().display().to_string()];
            let exit = cmd_add(&args);
            assert_eq!(
                exit,
                ExitCode::Error,
                "projection failure must return ExitCode::Error (1)"
            );
            let plugin_id = derive_plugin_id(&source.path().display().to_string());
            assert!(
                catalog_has_plugin(fake_home.path(), &plugin_id),
                "projection failure must retain the committed catalog row"
            );
        }

        #[test]
        fn cmd_add_outcome_directory_pre_mutation_failures() {
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());

            let catalog_path = fake_home.path().join(".ccync/plugins.json");
            fs::create_dir_all(catalog_path.parent().unwrap()).unwrap();
            let before = br#"{"schemaVersion":1,"plugins":[]}"#.to_vec();
            fs::write(&catalog_path, &before).unwrap();

            // Case 1: Plain directory zero contribution (only README.md)
            let zero_dir = TempDir::new().unwrap();
            fs::write(zero_dir.path().join("README.md"), "# empty\n").unwrap();
            let args_zero = vec![
                "add".to_string(),
                zero_dir.path().display().to_string(),
                "--no-sync".to_string(),
            ];
            assert_eq!(cmd_add(&args_zero), ExitCode::Error);
            assert_eq!(
                fs::read(&catalog_path).unwrap(),
                before,
                "zero contribution plain directory must fail before catalog mutation"
            );

            // Case 2: Plain directory with corrupt manifest (.mcp.json)
            let corrupt_dir = TempDir::new().unwrap();
            fs::write(corrupt_dir.path().join(".mcp.json"), "{ invalid json").unwrap();
            let args_corrupt = vec![
                "add".to_string(),
                corrupt_dir.path().display().to_string(),
                "--no-sync".to_string(),
            ];
            assert_eq!(cmd_add(&args_corrupt), ExitCode::Error);
            assert_eq!(
                fs::read(&catalog_path).unwrap(),
                before,
                "corrupt manifest plain directory must fail before catalog mutation"
            );
        }

        #[test]
        fn cmd_add_outcome_archive_pre_mutation_failures() {
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());

            let catalog_path = fake_home.path().join(".ccync/plugins.json");
            fs::create_dir_all(catalog_path.parent().unwrap()).unwrap();
            let before = br#"{"schemaVersion":1,"plugins":[]}"#.to_vec();
            fs::write(&catalog_path, &before).unwrap();

            // Minimal valid zip containing only README.md (zero contribution)
            let zero_zip_bytes: &[u8] = &[
                0x50, 0x4b, 0x03, 0x04, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x0c, 0x7e, 0x7f, 0xd8, 0x04, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x09, 0x00,
                0x00, 0x00, 0x52, 0x45, 0x41, 0x44, 0x4d, 0x45, 0x2e, 0x6d, 0x64, 0x74, 0x65, 0x73,
                0x74, 0x50, 0x4b, 0x01, 0x02, 0x1e, 0x03, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x0c, 0x7e, 0x7f, 0xd8, 0x04, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00,
                0x00, 0x09, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x52, 0x45, 0x41, 0x44, 0x4d, 0x45, 0x2e, 0x6d, 0x64,
                0x50, 0x4b, 0x05, 0x06, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x37, 0x00,
                0x00, 0x00, 0x2b, 0x00, 0x00, 0x00, 0x00, 0x00,
            ];

            let tmp = TempDir::new().unwrap();
            let archive_file = tmp.path().join("empty-plugin.zip");
            fs::write(&archive_file, zero_zip_bytes).unwrap();

            let args = vec![
                "add".to_string(),
                archive_file.display().to_string(),
                "--no-sync".to_string(),
            ];
            let exit = cmd_add(&args);
            assert_eq!(
                exit,
                ExitCode::Error,
                "zero-contribution archive must exit with Error"
            );
            assert_eq!(
                fs::read(&catalog_path).unwrap(),
                before,
                "zero-contribution archive must fail before catalog mutation"
            );

            // Fetch failure (corrupt / truncated archive file)
            let corrupt_file = tmp.path().join("corrupt.zip");
            fs::write(&corrupt_file, b"not a zip file").unwrap();
            let args_corrupt = vec![
                "add".to_string(),
                corrupt_file.display().to_string(),
                "--no-sync".to_string(),
            ];
            let exit_corrupt = cmd_add(&args_corrupt);
            assert_eq!(
                exit_corrupt,
                ExitCode::Error,
                "corrupt archive fetch failure must exit with Error"
            );
            assert_eq!(
                fs::read(&catalog_path).unwrap(),
                before,
                "corrupt archive fetch failure must leave catalog unchanged"
            );
        }

        #[test]
        fn cmd_add_outcome_git_pre_mutation_failures() {
            let _env = crate::test_io_lock();
            clear_assume_yes();
            let fake_home = TempDir::new().unwrap();
            set_home(fake_home.path());

            let catalog_path = fake_home.path().join(".ccync/plugins.json");
            fs::create_dir_all(catalog_path.parent().unwrap()).unwrap();
            let before = br#"{"schemaVersion":1,"plugins":[]}"#.to_vec();
            fs::write(&catalog_path, &before).unwrap();

            // Zero-contribution git repo
            let source = TempDir::new().unwrap();
            make_source_repo(source.path()); // README only
            let args = vec![
                "add".to_string(),
                source.path().display().to_string(),
                "--no-sync".to_string(),
            ];
            assert_eq!(cmd_add(&args), ExitCode::Error);
            assert_eq!(
                fs::read(&catalog_path).unwrap(),
                before,
                "zero contribution git repo must fail before catalog mutation"
            );
        }
    }

    #[test]
    fn cmd_add_outcome_tokens_are_stable() {
        assert_eq!(AddIntakeOutcome::Added.as_str(), "added");
        assert_eq!(AddIntakeOutcome::FetchFailed.as_str(), "fetch-failed");
        assert_eq!(AddIntakeOutcome::NoContribution.as_str(), "no-contribution");
    }

    #[test]
    fn format_add_component_messages_nonzero_only_reports_count() {
        let lines = format_add_component_messages(3, PayloadShape::PluginRoot);
        assert_eq!(lines, vec!["Components: 3".to_string()]);
    }

    #[test]
    fn format_add_component_messages_zero_adds_warning() {
        let lines = format_add_component_messages(0, PayloadShape::Unrecognized);
        assert_eq!(lines[0], "Components: 0");
        assert!(lines[1].contains("detected 0 components"));
        assert!(lines[1].contains("may not be a plugin repo"));
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
            assert!(
                !id.is_empty(),
                "derived id from {crafted:?} must not be empty"
            );
        }
    }

    #[test]
    fn derive_plugin_id_bare_dotdot_falls_back() {
        assert_eq!(derive_plugin_id(".."), "plugin");
        assert_eq!(derive_plugin_id("foo/.."), "plugin");
    }

    #[test]
    fn archive_plugin_id_strips_all_three_extensions() {
        for source in [
            "plugin-v1.zip",
            "plugin-v1.tar.gz",
            "plugin-v1.tgz",
            "plugin-v1.ZIP",
            "plugin-v1.TAR.GZ",
            "plugin-v1.TGZ",
            "/path/to/plugin-v1.zip",
            "https://example.com/dist/plugin-v1.tar.gz",
        ] {
            assert_eq!(
                derive_plugin_id(source),
                "plugin-v1",
                "archive source {source:?} must derive to 'plugin-v1' regardless of extension case"
            );
        }
    }

    #[test]
    fn archive_plugin_id_ignores_internal_metadata() {
        // The ID authority is the source filename only — never anything from
        // inside the archive (a differently-named manifest, embedded plugin
        // id, etc). derive_plugin_id never reads file contents, so this is
        // proven by construction: two different source filenames with
        // unrelated internal content still derive independently from their
        // own filenames, not from a shared/embedded identity.
        assert_eq!(derive_plugin_id("plugin-v1.zip"), "plugin-v1");
        assert_eq!(derive_plugin_id("plugin-v2.zip"), "plugin-v2");
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
        assert_eq!(classify_source("http://example.com/repo"), SourceKind::Git);
    }

    #[test]
    fn classify_source_git_subpath_before_archive_suffix() {
        assert_eq!(
            classify_source("https://github.com/user/repo//plugins/foo.zip"),
            SourceKind::Git
        );
        assert_eq!(
            classify_source("https://host/o/r//plugins/x.zip"),
            SourceKind::Git
        );
        assert_eq!(
            classify_source("git@host:o/r//plugins/x.zip"),
            SourceKind::Git
        );
        assert_eq!(
            classify_source("https://host/o/r//plugins/x"),
            SourceKind::Git
        );
        assert_eq!(classify_source("https://host/o/r.zip"), SourceKind::Archive);
    }

    #[test]
    fn derive_plugin_id_git_subpath_uses_last_segment() {
        assert_eq!(
            derive_plugin_id("https://github.com/user/repo//plugins/foo.zip"),
            "foo"
        );
        assert_eq!(
            derive_plugin_id("https://github.com/upstash/context7//plugins/claude/context7"),
            "context7"
        );
        assert_eq!(derive_plugin_id("git@host:o/r//plugins/x"), "x");
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

    // ── format_list_view: managed view (personal + adopted) rendering ────────

    #[test]
    fn package_identity_lines_name_package_and_management_selector() {
        let catalog = json!({
            "plugins": [
                { "pluginId": "plugin", "packageName": "Impeccable" },
                { "pluginId": "graphify-agents", "packageName": "graphify" },
                { "pluginId": "bare" }
            ]
        });
        assert_eq!(
            package_identity_lines(&catalog),
            vec![
                "Package: Impeccable | management ID: plugin (remove with: ccync remove plugin)"
                    .to_string(),
                "Package: graphify | management ID: graphify-agents (remove with: ccync remove graphify-agents)"
                    .to_string(),
            ]
        );
        assert!(package_identity_lines(&json!({})).is_empty());
    }

    #[test]
    fn format_list_view_empty_shows_original_empty_state() {
        let out = format_list_view(&[]);
        assert!(out.contains("No personal plugins installed."));
        assert!(out.contains("Add one with: ccync add <git-url>"));
    }

    #[test]
    fn format_list_view_adopted_only_suppresses_empty_state_and_shows_item() {
        let lock = json!({
            "_adoptedItems": [
                { "name": "adopted-tool", "sourceId": "claude-plugin", "source": "mkt:adopted-tool@1.0.0" }
            ]
        });
        let items = ccync_engine::build_managed_view(&lock);
        let out = format_list_view(&items);
        assert!(
            !out.contains("No personal plugins installed."),
            "adopted-only view must NOT print the personal-only empty-state text: {out}"
        );
        assert!(out.contains("adopted-tool"), "adopted item id must appear");
        assert!(out.contains("adopted"), "origin column must show adopted");
        assert!(out.contains("plugin"), "kind column must show plugin");
    }

    #[test]
    fn format_list_view_personal_and_adopted_both_shown_and_distinguished() {
        let lock = json!({
            "_personalPlugins": [
                { "pluginId": "my-plugin", "source": "https://x/my-plugin.git", "pin": "abcdef1234567890" }
            ],
            "_adoptedItems": [
                { "name": "their-mcp", "sourceId": "claude-mcp", "source": "mkt:their-mcp@2.0.0" }
            ]
        });
        let items = ccync_engine::build_managed_view(&lock);
        let out = format_list_view(&items);

        assert!(out.contains("my-plugin"), "personal item must appear");
        assert!(out.contains("personal"), "personal origin must appear");
        assert!(
            out.contains("abcdef123456"),
            "personal pin must be truncated to 12 chars"
        );

        assert!(out.contains("their-mcp"), "adopted item must appear");
        assert!(out.contains("adopted"), "adopted origin must appear");
        assert!(out.contains("mcp"), "adopted kind (mcp) must appear");
    }

    #[test]
    fn format_list_view_neither_personal_nor_adopted_shows_empty_state() {
        let lock = json!({});
        let items = ccync_engine::build_managed_view(&lock);
        let out = format_list_view(&items);
        assert!(out.contains("No personal plugins installed."));
        assert!(out.contains("Add one with: ccync add <git-url>"));
    }

    #[test]
    fn adopted_source_labels_mcp_and_skill_replace_bare_question_mark() {
        // Adopted MCP servers and loose skills have no `source` field at all
        // (config entry / local content, no upstream) — they must get a
        // kind-specific label, never the bare `?` fallback.
        let lock = json!({
            "_adoptedItems": [
                { "name": "their-mcp", "sourceId": "claude-mcp" },
                { "name": "their-skill", "sourceId": "claude-skill" }
            ]
        });
        let items = ccync_engine::build_managed_view(&lock);
        let out = format_list_view(&items);

        assert!(
            out.contains("(agent MCP config)"),
            "adopted MCP row must show the MCP-specific label: {out}"
        );
        assert!(
            out.contains("(agent loose skill)"),
            "adopted skill row must show the skill-specific label: {out}"
        );
        assert!(
            !out.lines().any(|l| l.trim_end().ends_with(" ?")),
            "no row may end in a bare '?' source: {out}"
        );
    }

    #[test]
    fn adopted_plugin_and_personal_plugin_keep_bare_question_mark_fallback() {
        // Only adopted MCP/skill rows get a label. An
        // unresolved *plugin* source (personal or adopted) stays the bare
        // `?` — it is an actionable gap (`ccync repair` when adopted,
        // `ccync search` + `ccync add` when personal), not a kind property
        // to label away.
        let lock = json!({
            "_personalPlugins": [
                { "pluginId": "no-source-personal" }
            ],
            "_adoptedItems": [
                { "name": "no-source-adopted", "sourceId": "claude-plugin" }
            ]
        });
        let items = ccync_engine::build_managed_view(&lock);
        let out = format_list_view(&items);

        assert!(
            out.lines()
                .any(|l| l.contains("no-source-personal") && l.trim_end().ends_with(" ?")),
            "personal plugin with no source must keep the bare '?' fallback: {out}"
        );
        assert!(
            out.lines()
                .any(|l| l.contains("no-source-adopted") && l.trim_end().ends_with(" ?")),
            "adopted plugin with no source must keep the bare '?' fallback: {out}"
        );
    }

    // ── declared_mcp_list: unit tests for declared MCP listing ─────────────

    fn isolate_test_home(tmp: &std::path::Path) -> Vec<(String, Option<String>)> {
        let keys = [
            "USERPROFILE",
            "HOME",
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
        ];
        let mut saved = Vec::new();
        for k in keys {
            saved.push((k.to_string(), std::env::var(k).ok()));
            unsafe {
                std::env::set_var(k, tmp);
            }
        }
        saved
    }

    fn restore_test_home(saved: &[(String, Option<String>)]) {
        for (k, v) in saved {
            unsafe {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    fn snapshot_test_tree(dir: &std::path::Path) -> Vec<(String, u64, std::time::SystemTime)> {
        fn walk(
            base: &std::path::Path,
            dir: &std::path::Path,
            out: &mut Vec<(String, u64, std::time::SystemTime)>,
        ) {
            let Ok(read) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in read.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(base, &path, out);
                } else if let Ok(meta) = path.metadata() {
                    let rel = path
                        .strip_prefix(base)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned();
                    out.push((
                        rel,
                        meta.len(),
                        meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                    ));
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }

    #[test]
    fn declared_mcp_list_renders_deterministic_rows_with_declared_label() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_test_home(tmp.path());

        let ccync_dir = tmp.path().join(".ccync");
        let build_dir = ccync_dir.join("build");
        std::fs::create_dir_all(&build_dir).unwrap();

        std::fs::write(
            ccync_dir.join("mcp.json"),
            r#"{
                "mcpServers": {
                    "zeta-server": { "command": "run-zeta" },
                    "alpha-server": { "url": "http://127.0.0.1:8000/sse" }
                }
            }"#,
        )
        .unwrap();

        std::fs::write(
            build_dir.join("lock.json"),
            r#"{
                "_personalPlugins": [
                    { "pluginId": "beta-plugin", "source": "https://example.com/beta.git", "pin": "0123456789abcdef" }
                ],
                "_adoptedItems": [
                    { "name": "gamma-mcp", "sourceId": "claude-mcp" }
                ]
            }"#,
        )
        .unwrap();

        let exit = cmd_list(&["list".to_string()]);
        assert_eq!(exit, ExitCode::Success);

        let lock_val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(build_dir.join("lock.json")).unwrap())
                .unwrap();
        let decl = ccync_engine::install::read_user_mcp_input(&ccync_dir.join("mcp.json")).unwrap();
        let items =
            ccync_engine::managed_view::build_managed_view_with_declared(&lock_val, decl.as_ref());
        let view = format_list_view(&items);

        restore_test_home(&saved);

        let lines: Vec<&str> = view
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('-') && !l.starts_with("ID"))
            .collect();
        assert_eq!(lines.len(), 4, "expected 4 rows: {view}");

        // Row 0: alpha-server (declared) - sorted first by name
        assert!(lines[0].contains("alpha-server"));
        assert!(lines[0].contains("declared"));
        assert!(lines[0].contains("mcp"));

        // Row 1: zeta-server (declared) - sorted second by name
        assert!(lines[1].contains("zeta-server"));
        assert!(lines[1].contains("declared"));
        assert!(lines[1].contains("mcp"));

        // Row 2: beta-plugin (personal)
        assert!(lines[2].contains("beta-plugin"));
        assert!(lines[2].contains("personal"));
        assert!(lines[2].contains("plugin"));

        // Row 3: gamma-mcp (adopted)
        assert!(lines[3].contains("gamma-mcp"));
        assert!(lines[3].contains("adopted"));
        assert!(lines[3].contains("mcp"));
    }

    #[test]
    fn declared_mcp_list_retains_duplicate_ids_without_collapsing() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_test_home(tmp.path());

        let ccync_dir = tmp.path().join(".ccync");
        let build_dir = ccync_dir.join("build");
        std::fs::create_dir_all(&build_dir).unwrap();

        std::fs::write(
            ccync_dir.join("mcp.json"),
            r#"{
                "mcpServers": {
                    "shared-id": { "command": "run-shared" }
                }
            }"#,
        )
        .unwrap();

        std::fs::write(
            build_dir.join("lock.json"),
            r#"{
                "_personalPlugins": [
                    { "pluginId": "shared-id", "source": "https://example.com/shared.git", "pin": "abcdef123456" }
                ]
            }"#,
        )
        .unwrap();

        let exit = cmd_list(&["list".to_string()]);
        assert_eq!(exit, ExitCode::Success);

        let lock_val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(build_dir.join("lock.json")).unwrap())
                .unwrap();
        let decl = ccync_engine::install::read_user_mcp_input(&ccync_dir.join("mcp.json")).unwrap();
        let items =
            ccync_engine::managed_view::build_managed_view_with_declared(&lock_val, decl.as_ref());
        let view = format_list_view(&items);

        restore_test_home(&saved);

        let shared_rows: Vec<&str> = view.lines().filter(|l| l.contains("shared-id")).collect();
        assert_eq!(
            shared_rows.len(),
            2,
            "duplicate ID must appear twice without collapsing: {view}"
        );
        assert!(
            shared_rows[0].contains("declared"),
            "first row must be declared: {}",
            shared_rows[0]
        );
        assert!(
            shared_rows[1].contains("personal"),
            "second row must be personal: {}",
            shared_rows[1]
        );
    }

    #[test]
    fn declared_mcp_list_fails_on_invalid_input() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_test_home(tmp.path());

        let ccync_dir = tmp.path().join(".ccync");
        std::fs::create_dir_all(&ccync_dir).unwrap();

        // Case 1: Malformed JSON syntax
        std::fs::write(ccync_dir.join("mcp.json"), "{ invalid json").unwrap();
        let exit_malformed = cmd_list(&["list".to_string()]);
        assert_eq!(
            exit_malformed,
            ExitCode::Error,
            "malformed JSON must fail cmd_list"
        );

        // Case 2: Missing top-level mcpServers or not an object
        std::fs::write(
            ccync_dir.join("mcp.json"),
            r#"{"mcpServers": "not-an-object"}"#,
        )
        .unwrap();
        let exit_not_obj = cmd_list(&["list".to_string()]);
        assert_eq!(
            exit_not_obj,
            ExitCode::Error,
            "mcpServers as non-object must fail cmd_list"
        );

        // Case 3: Invalid server with both command and url
        std::fs::write(
            ccync_dir.join("mcp.json"),
            r#"{"mcpServers": {"bad": {"command": "echo", "url": "http://localhost"}}}"#,
        )
        .unwrap();
        let exit_both = cmd_list(&["list".to_string()]);
        assert_eq!(
            exit_both,
            ExitCode::Error,
            "server with both command and url must fail cmd_list"
        );

        // Case 4: Invalid server with neither command nor url
        std::fs::write(
            ccync_dir.join("mcp.json"),
            r#"{"mcpServers": {"bad": {"type": "stdio"}}}"#,
        )
        .unwrap();
        let exit_neither = cmd_list(&["list".to_string()]);
        assert_eq!(
            exit_neither,
            ExitCode::Error,
            "server with neither command nor url must fail cmd_list"
        );

        restore_test_home(&saved);
    }

    #[test]
    fn declared_mcp_list_is_read_only_and_makes_no_writes() {
        let _guard = crate::test_io_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let saved = isolate_test_home(tmp.path());

        let ccync_dir = tmp.path().join(".ccync");
        let build_dir = ccync_dir.join("build");
        std::fs::create_dir_all(&build_dir).unwrap();

        // 1. With valid mcp.json and lock.json
        std::fs::write(
            ccync_dir.join("mcp.json"),
            r#"{"mcpServers": {"srv": {"command": "run"}}}"#,
        )
        .unwrap();
        std::fs::write(build_dir.join("lock.json"), r#"{"_personalPlugins": []}"#).unwrap();

        let before = snapshot_test_tree(tmp.path());
        let exit_valid = cmd_list(&["list".to_string()]);
        let after = snapshot_test_tree(tmp.path());

        assert_eq!(exit_valid, ExitCode::Success);
        assert_eq!(
            before, after,
            "cmd_list with valid mcp.json must make zero writes to filesystem"
        );

        // 2. With invalid mcp.json
        std::fs::write(ccync_dir.join("mcp.json"), "invalid syntax").unwrap();
        let before_inv = snapshot_test_tree(tmp.path());
        let exit_inv = cmd_list(&["list".to_string()]);
        let after_inv = snapshot_test_tree(tmp.path());

        assert_eq!(exit_inv, ExitCode::Error);
        assert_eq!(
            before_inv, after_inv,
            "cmd_list with invalid mcp.json must make zero writes to filesystem"
        );

        // 3. With absent mcp.json
        std::fs::remove_file(ccync_dir.join("mcp.json")).unwrap();
        let before_absent = snapshot_test_tree(tmp.path());
        let exit_absent = cmd_list(&["list".to_string()]);
        let after_absent = snapshot_test_tree(tmp.path());

        assert_eq!(exit_absent, ExitCode::Success);
        assert_eq!(
            before_absent, after_absent,
            "cmd_list with absent mcp.json must make zero writes to filesystem"
        );

        restore_test_home(&saved);
    }
}
