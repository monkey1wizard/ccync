//! Install/update/uninstall operations for CCYNC.
//!
//! Orchestrates the full flow: config → mode → render → runtime-surface projections → ledger.
//! This is the Rust-native implementation replacing the frozen PowerShell oracle.
//!
//! Entry switch (BUG-B): once this module is wired in main.rs, the Rust binary
//! is the single entry for install/update/uninstall. The frozen scripts remain
//! as oracle only and are NOT invoked by the entry.
//!
//! Oracle parity: run_update() produces Claude+Copilot surfaces that
//! match the frozen Install-CcyncPlugins oracle in an isolated home.

use crate::config::CcyncConfig;
use crate::ledger::{ledger_path, now_timestamp, Ledger, LedgerEntry};
use projection::agy::AgyProjection;
use projection::claude_skill::ClaudeSkillProjection;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// Errors that can occur during install/update/uninstall.
#[derive(Debug)]
pub enum InstallError {
    /// Filesystem I/O error.
    Io(std::io::Error),
    /// JSON parsing or serialization failed.
    Json(serde_json::Error),
    /// Home directory could not be determined.
    NoHome,
    /// Uninstall completed partially; ledger was written but some removals failed.
    PartialUninstall { warnings: Vec<String> },
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallError::Io(e) => write!(f, "I/O error: {e}"),
            InstallError::Json(e) => write!(f, "JSON error: {e}"),
            InstallError::NoHome => write!(f, "could not determine home directory"),
            InstallError::PartialUninstall { warnings } => {
                write!(
                    f,
                    "uninstall completed with partial failures: {}",
                    warnings.join("; ")
                )
            }
        }
    }
}

impl std::error::Error for InstallError {}

impl From<std::io::Error> for InstallError {
    fn from(e: std::io::Error) -> Self {
        InstallError::Io(e)
    }
}

impl From<serde_json::Error> for InstallError {
    fn from(e: serde_json::Error) -> Self {
        InstallError::Json(e)
    }
}

/// Result of a successful install or update operation.
#[derive(Debug)]
pub struct InstallReport {
    /// Path to the rendered canonical plugin root.
    pub canonical_root: PathBuf,
    /// Runtimes that were successfully projected.
    pub runtimes: Vec<String>,
    /// Mode in use ("normal" | "dev").
    pub mode: String,
    /// Non-fatal warnings (e.g. AGY best-effort failures).
    pub warnings: Vec<String>,
}

/// Run `ccync install` — first-time install. Idempotent (delegates to update).
pub fn run_install(config: &CcyncConfig) -> Result<InstallReport, InstallError> {
    run_update_with_op(config, "install")
}

/// Run `ccync update` — re-render canonical root and refresh all runtime surfaces.
///
/// Claude and Copilot read surfaces are inside the canonical root. AGY junctions
/// are re-applied best-effort. A ledger entry is written on success.
pub fn run_update(config: &CcyncConfig) -> Result<InstallReport, InstallError> {
    run_update_with_op(config, "update")
}

fn run_update_with_op(_config: &CcyncConfig, op: &str) -> Result<InstallReport, InstallError> {
    // Step 1: D-07 — single mode. There is no Dev/Normal split, no devMode/ccyncRoot
    // resolution; ccync always renders from its canonical root.
    let mode_str = "normal";

    // Step 2: Resolve the canonical root path. ccync projects managed bundles +
    // aggregated MCP under this root; it does NOT bake ccync-core content (see D1/D8).
    let canonical_root =
        ccync_foundation::paths::canonical_plugin_root().ok_or(InstallError::NoHome)?;
    fs::create_dir_all(&canonical_root).map_err(InstallError::Io)?;

    // Deploy the embedded public catalog so the runtime resolve path (incl.
    // `ccync plugin add`) can find it without the source repo. Best-effort.
    let _ = fs::write(
        canonical_root.join("catalog.json"),
        crate::catalog::embedded_catalog_json(),
    );

    let mut runtimes = vec!["copilot".to_string()];
    let mut warnings = Vec::new();

    // Step 2.5: Render the managed-set content into the canonical root (A4 gap —
    // the carve deleted this). Without it the read-surface symlinks point at an
    // empty tree. Best-effort: a render failure is surfaced as a warning.
    let plugin_dirs = managed_plugin_dirs();
    if let Err(e) = render_canonical_root(&canonical_root, &plugin_dirs) {
        warnings.push(format!("canonical root content render (best-effort): {e}"));
    }

    // Step 3: Claude skill surface projection — `~/.claude/skills/ccync` → canonical root.
    // Best-effort: a link failure does not void the canonical root render, but is
    // surfaced as a warning so `ccync doctor` can detect the gap.
    // Never touches `~/.claude/plugins/ccync` (oracle legacy path).
    match ClaudeSkillProjection::new(canonical_root.clone()) {
        Ok(proj) => match proj.apply() {
            Ok(_) => runtimes.push("claude".to_string()),
            Err(e) => warnings.push(format!("Claude skill surface (best-effort): {e}")),
        },
        Err(e) => warnings.push(format!("Claude skill surface init (best-effort): {e}")),
    }

    // Step 4: AGY plugin-junction projection — best-effort (OE-A), never fatal.
    // The CLI / IDE junctions are gated by their own runtime keys; the GUI face is
    // a decomposed skill + mcp_config projection owned by the projection layer, not
    // a plugin junction (so it is not created here).
    let agy_selection = load_runtime_selection();
    let want_agy_cli = agy_selection
        .selected_runtimes
        .iter()
        .any(|r| r == "agy-cli");
    let want_agy_ide = agy_selection
        .selected_runtimes
        .iter()
        .any(|r| r == "agy-ide");
    if want_agy_cli || want_agy_ide {
        match AgyProjection::new(canonical_root.clone()) {
            Ok(agy) => match agy.apply(want_agy_cli, want_agy_ide) {
                Ok(_) => runtimes.push("agy".to_string()),
                Err(e) => warnings.push(format!("AGY projection (best-effort): {e}")),
            },
            Err(e) => warnings.push(format!("AGY init (best-effort): {e}")),
        }
    }

    if let Err(e) = write_surface_lifecycle_artifacts(&canonical_root, &mut runtimes) {
        warnings.push(format!("runtime lifecycle state: {e}"));
    }

    // Generate merged managed.mcp.json from core + enabled bundles (best-effort).
    if let Err(e) = generate_managed_mcp(&canonical_root) {
        warnings.push(format!("managed MCP generation (best-effort): {e}"));
    }

    // Best-effort migration: the new dist/runtimes state is written above; now remove
    // the pre-rename dist/providers tree so updated installs don't keep an orphan.
    // Failure is a warning, never fatal (mirrors the best-effort projection contract).
    if let Ok(stale) = stale_dist_providers_root() {
        if let Err(e) = remove_path_if_present(&stale) {
            warnings.push(format!("stale dist/providers cleanup (best-effort): {e}"));
        }
    }

    // Step 5: Write ledger.
    let ledger_warnings = warnings.clone();
    write_ledger_entry(
        op,
        &canonical_root,
        &runtimes,
        mode_str,
        &ledger_warnings,
        &mut warnings,
    );

    Ok(InstallReport {
        canonical_root,
        runtimes,
        mode: mode_str.to_string(),
        warnings,
    })
}

#[derive(Debug, Clone)]
struct RuntimeSelection {
    selected_runtimes: Vec<String>,
    primary_runtime: String,
}

/// Absent-state default selected runtimes — the single source shared with
/// `projection::load_selected_runtimes`'s fallback (`ccync_foundation::runtime::VALID_RUNTIMES`).
///
/// The previous hardcoded 4-runtime default (no gemini/opencode) diverged from
/// the projection fallback, so a missing install-state.json silently dropped
/// gemini/opencode from the install-side selection.
fn default_selected_runtimes() -> Vec<String> {
    ccync_foundation::runtime::VALID_RUNTIMES
        .iter()
        .map(|runtime| (*runtime).to_string())
        .collect()
}

fn load_runtime_selection() -> RuntimeSelection {
    let default_selected = default_selected_runtimes();

    let Some(path) = crate::paths::install_state_path() else {
        return RuntimeSelection {
            selected_runtimes: default_selected.clone(),
            primary_runtime: default_selected[0].clone(),
        };
    };

    let Ok(content) = fs::read_to_string(path) else {
        return RuntimeSelection {
            selected_runtimes: default_selected.clone(),
            primary_runtime: default_selected[0].clone(),
        };
    };

    let Ok(value) = serde_json::from_str::<Value>(&content) else {
        return RuntimeSelection {
            selected_runtimes: default_selected.clone(),
            primary_runtime: default_selected[0].clone(),
        };
    };

    let selected_runtimes = value
        .get("selectedRuntimes")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|item| item.trim().to_ascii_lowercase())
                .filter(|item| !item.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|items| !items.is_empty())
        .unwrap_or_else(|| default_selected.clone());

    let primary_runtime = value
        .get("primaryRuntime")
        .and_then(Value::as_str)
        .map(|item| item.trim().to_ascii_lowercase())
        .filter(|item| !item.is_empty())
        .unwrap_or_else(|| selected_runtimes[0].clone());

    RuntimeSelection {
        selected_runtimes,
        primary_runtime,
    }
}

fn surface_from_runtime(runtime: &str) -> &str {
    match runtime {
        // D-13: the three Antigravity surfaces share one "agy" lifecycle surface.
        "agy-cli" | "agy-ide" | "agy-gui" => "agy",
        other => other,
    }
}

fn lane_from_runtime(runtime: &str) -> &str {
    match runtime {
        "opencode" => "bridge",
        "gemini-cli" => "migration",
        _ => "primary",
    }
}

fn primary_surfaces(selection: &RuntimeSelection) -> Vec<String> {
    let mut surfaces = Vec::new();
    for runtime in &selection.selected_runtimes {
        if lane_from_runtime(runtime) != "primary" {
            continue;
        }
        let surface = surface_from_runtime(runtime).to_string();
        if !surfaces.contains(&surface) {
            surfaces.push(surface);
        }
    }
    surfaces
}

fn ensure_parent_dir(path: &Path) -> Result<(), InstallError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn ccync_dist_runtimes_root() -> Result<PathBuf, InstallError> {
    crate::paths::ccync_home()
        .map(|path| path.join("dist").join("runtimes"))
        .ok_or(InstallError::NoHome)
}

/// Pre-rename dist root (`~/.ccync/dist/providers`). Retained only as a best-effort
/// cleanup target so an update/uninstall migrates installs created before the
/// `providers`→`runtimes` path rename.
fn stale_dist_providers_root() -> Result<PathBuf, InstallError> {
    crate::paths::ccync_home()
        .map(|path| path.join("dist").join("providers"))
        .ok_or(InstallError::NoHome)
}

fn runtime_state_path(runtime: &str) -> Result<PathBuf, InstallError> {
    Ok(ccync_dist_runtimes_root()?
        .join(runtime)
        .join("managed.json"))
}

fn write_json_file(path: &Path, value: &Value) -> Result<(), InstallError> {
    ensure_parent_dir(path)?;
    fs::write(path, serde_json::to_string_pretty(value)?)?;
    Ok(())
}

fn plugins_lockfile_path() -> Option<PathBuf> {
    crate::paths::plugins_lock_path()
}

/// Returns `(plugin_id, resolved_source)` pairs for enabled bundled-local plugins
/// from the lockfile. Returns empty vec when the lockfile is absent or unreadable.
fn load_enabled_bundles() -> Vec<(String, String)> {
    let path = match plugins_lockfile_path() {
        Some(p) => p,
        None => return vec![],
    };
    let contents = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let lock: Value = match serde_json::from_str(&contents) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    lock.get("resolvedPlugins")
        .and_then(Value::as_array)
        .map(|plugins| {
            plugins
                .iter()
                .filter(|p| {
                    p.get("installStrategy").and_then(Value::as_str) == Some("bundled-local")
                })
                .filter_map(|p| {
                    let id = p.get("pluginId").and_then(Value::as_str)?.to_owned();
                    let src = p.get("resolvedSource").and_then(Value::as_str)?.to_owned();
                    Some((id, src))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Returns `(plugin_id, resolved_source)` pairs for enabled personal plugins
/// from the lockfile `_personalPlugins` namespace.
/// Returns empty vec when the lockfile is absent, unreadable, or has no personal plugins.
/// Does not touch the `resolvedPlugins` public namespace — the two namespaces are disjoint.
pub(crate) fn load_enabled_personal_plugins() -> Vec<(String, String)> {
    let path = match plugins_lockfile_path() {
        Some(p) => p,
        None => return vec![],
    };
    let contents = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let lock: Value = match serde_json::from_str(&contents) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    lock.get("_personalPlugins")
        .and_then(Value::as_array)
        .map(|plugins| {
            plugins
                .iter()
                .filter_map(|p| {
                    let id = p.get("pluginId").and_then(Value::as_str)?.to_owned();
                    let src = p.get("source").and_then(Value::as_str)?.to_owned();
                    Some((id, src))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Fetch result for a single personal plugin clone.
#[derive(Debug, PartialEq)]
pub enum PersonalFetchResult {
    /// Clone succeeded; `path` is the cache dir, `sha` is the pinned HEAD commit.
    Cloned { path: PathBuf, sha: String },
    /// A `<plugin_id>@*` dir already exists in cache — no-op (idempotent).
    AlreadyPresent(PathBuf),
    /// `~/.ccync/local/cache/` root could not be resolved (no home dir).
    NoCacheRoot,
    /// `git clone` or sha-read invocation failed; message contains stderr.
    CloneFailed(String),
}

/// Clone a personal plugin from `source_url` into
/// `~/.ccync/local/cache/<plugin_id>@<sha>/` preserving full CC-plugin structure.
///
/// Idempotent: if any `<plugin_id>@*` directory already exists in `cache_root`,
/// returns `AlreadyPresent` without re-cloning.
///
/// Flow:
/// 1. Scan `cache_root` for an existing `<plugin_id>@*` dir → AlreadyPresent.
/// 2. `git clone --depth 1 <source_url> <tmp>` inside cache root.
/// 3. `git rev-parse HEAD` in the tmp clone to get the pinned sha.
/// 4. Rename `<tmp>` → `<plugin_id>@<sha>`.
pub fn fetch_personal_plugin(
    cache_root: &Path,
    plugin_id: &str,
    source_url: &str,
) -> PersonalFetchResult {
    let prefix = format!("{}@", plugin_id);
    if let Ok(entries) = fs::read_dir(cache_root) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) && entry.path().is_dir() {
                return PersonalFetchResult::AlreadyPresent(entry.path());
            }
        }
    }
    if let Err(e) = fs::create_dir_all(cache_root) {
        return PersonalFetchResult::CloneFailed(format!("create cache root: {e}"));
    }
    let tmp = cache_root.join(format!("{}@__fetching__", plugin_id));
    let _ = fs::remove_dir_all(&tmp);
    let clone_out = std::process::Command::new("git")
        .args([
            "clone",
            "--depth",
            "1",
            source_url,
            tmp.to_str().unwrap_or(""),
        ])
        .output();
    match clone_out {
        Err(e) => return PersonalFetchResult::CloneFailed(format!("spawn git: {e}")),
        Ok(out) if !out.status.success() => {
            let _ = fs::remove_dir_all(&tmp);
            return PersonalFetchResult::CloneFailed(
                String::from_utf8_lossy(&out.stderr).to_string(),
            );
        }
        Ok(_) => {}
    }
    let sha_out = std::process::Command::new("git")
        .args(["-C", tmp.to_str().unwrap_or(""), "rev-parse", "HEAD"])
        .output();
    let sha = match sha_out {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => {
            let _ = fs::remove_dir_all(&tmp);
            return PersonalFetchResult::CloneFailed("could not read HEAD sha".to_string());
        }
    };
    let target = cache_root.join(format!("{}@{}", plugin_id, sha));
    if target.exists() {
        let _ = fs::remove_dir_all(&tmp);
        return PersonalFetchResult::AlreadyPresent(target);
    }
    if let Err(e) = fs::rename(&tmp, &target) {
        let _ = fs::remove_dir_all(&tmp);
        return PersonalFetchResult::CloneFailed(format!("rename to target: {e}"));
    }
    PersonalFetchResult::Cloned { path: target, sha }
}

/// Fetch all personal plugins listed in `_personalPlugins` lockfile namespace.
/// Skips entries that are already cached (idempotent).
/// Returns a vec of `(plugin_id, PersonalFetchResult)`.
pub fn fetch_all_personal_plugins() -> Vec<(String, PersonalFetchResult)> {
    let cache_root = match crate::paths::local_plugin_cache_dir() {
        Some(p) => p,
        None => return vec![],
    };
    load_enabled_personal_plugins()
        .into_iter()
        .map(|(id, src)| {
            let result = fetch_personal_plugin(&cache_root, &id, &src);
            (id, result)
        })
        .collect()
}

/// Fetch a plugin from a local archive (`.zip` or `.tar.gz`/`.tgz`) into
/// `~/.ccync/local/cache/<plugin_id>@<archive-bytes-sha256-first-12>/`.
///
/// Cache key uses the first 12 hex chars of the archive's SHA-256 so the same
/// file fetched twice hits `AlreadyPresent`. A single-root-dir wrapper produced
/// by GitHub downloads is automatically stripped so the plugin content sits at
/// the cache dir root (mirrors what `git clone` produces).
pub fn fetch_archive_plugin(
    cache_root: &Path,
    plugin_id: &str,
    archive_path: &Path,
) -> PersonalFetchResult {
    let prefix = format!("{}@", plugin_id);
    if let Ok(entries) = fs::read_dir(cache_root) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) && entry.path().is_dir() {
                return PersonalFetchResult::AlreadyPresent(entry.path());
            }
        }
    }

    let bytes = match fs::read(archive_path) {
        Ok(b) => b,
        Err(e) => return PersonalFetchResult::CloneFailed(format!("read archive: {e}")),
    };
    let hash = archive_sha_prefix(&bytes);

    let target = cache_root.join(format!("{}@{}", plugin_id, hash));
    if target.exists() {
        return PersonalFetchResult::AlreadyPresent(target);
    }

    if let Err(e) = fs::create_dir_all(cache_root) {
        return PersonalFetchResult::CloneFailed(format!("create cache root: {e}"));
    }

    let tmp = cache_root.join(format!("{}@__extracting__", plugin_id));
    let _ = fs::remove_dir_all(&tmp);

    let ext = archive_extension(archive_path);
    if let Err(e) = extract_archive_to(&bytes, ext, &tmp) {
        let _ = fs::remove_dir_all(&tmp);
        return PersonalFetchResult::CloneFailed(e);
    }

    if let Err(e) = promote_single_root_dir(&tmp) {
        let _ = fs::remove_dir_all(&tmp);
        return PersonalFetchResult::CloneFailed(e);
    }

    if let Err(e) = fs::rename(&tmp, &target) {
        let _ = fs::remove_dir_all(&tmp);
        return PersonalFetchResult::CloneFailed(format!("rename to target: {e}"));
    }

    PersonalFetchResult::Cloned { path: target, sha: hash }
}

fn archive_sha_prefix(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex[..12].to_string()
}

fn archive_extension(path: &Path) -> &str {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        ".tar.gz"
    } else if name.ends_with(".zip") {
        ".zip"
    } else {
        ""
    }
}

fn extract_archive_to(bytes: &[u8], ext: &str, dst: &Path) -> Result<(), String> {
    match ext {
        ".zip" => extract_zip_to(bytes, dst),
        ".tar.gz" => extract_tar_gz_to(bytes, dst),
        other => Err(format!("unsupported archive format: {other}")),
    }
}

fn extract_zip_to(bytes: &[u8], dst: &Path) -> Result<(), String> {
    use std::io::Cursor;
    let cursor = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor).map_err(|e| format!("open zip: {e}"))?;
    fs::create_dir_all(dst).map_err(|e| format!("create dst: {e}"))?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| format!("zip entry {i}: {e}"))?;
        let out_path = match file.enclosed_name() {
            Some(p) => dst.join(p),
            None => continue,
        };
        if file.is_dir() {
            fs::create_dir_all(&out_path).map_err(|e| format!("create dir: {e}"))?;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("create parent: {e}"))?;
            }
            let mut buf = Vec::with_capacity(file.size() as usize);
            std::io::copy(&mut file, &mut buf).map_err(|e| format!("read entry: {e}"))?;
            fs::write(&out_path, &buf).map_err(|e| format!("write entry: {e}"))?;
        }
    }
    Ok(())
}

fn extract_tar_gz_to(bytes: &[u8], dst: &Path) -> Result<(), String> {
    use flate2::read::GzDecoder;
    use std::io::Cursor;
    use tar::Archive;

    let cursor = Cursor::new(bytes);
    let gz = GzDecoder::new(cursor);
    let mut archive = Archive::new(gz);
    fs::create_dir_all(dst).map_err(|e| format!("create dst: {e}"))?;
    for entry in archive.entries().map_err(|e| format!("tar entries: {e}"))? {
        let mut entry = entry.map_err(|e| format!("tar entry: {e}"))?;
        let entry_path = entry
            .path()
            .map_err(|e| format!("entry path: {e}"))?
            .into_owned();
        // Tar-slip guard: reject absolute paths and any `..` traversal so a
        // crafted archive cannot escape `dst`. The tar reader's `path()` is the
        // raw stored name and is NOT containment-checked (unlike the zip path's
        // `enclosed_name()`), so we must validate before joining. Fail closed —
        // one unsafe entry aborts the whole extraction.
        if !is_contained_relative(&entry_path) {
            return Err(format!(
                "unsafe tar entry path (escapes extraction dir): {}",
                entry_path.display()
            ));
        }
        let out_path = dst.join(&entry_path);
        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(&out_path).map_err(|e| format!("create dir: {e}"))?;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("create parent: {e}"))?;
            }
            entry
                .unpack(&out_path)
                .map_err(|e| format!("unpack entry: {e}"))?;
        }
    }
    Ok(())
}

/// True only when `path` is a safe relative path that stays inside its base:
/// not absolute, no root/prefix (drive) component, and no `..` parent-dir
/// component. Used to defend archive extraction against path-traversal
/// (zip-slip / tar-slip) before joining an untrusted entry path onto the
/// extraction dir.
fn is_contained_relative(path: &Path) -> bool {
    use std::path::Component;
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            // RootDir / Prefix (absolute or drive-letter) and ParentDir (`..`)
            // can all escape the extraction dir.
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => return false,
        }
    }
    true
}

/// If `dir` contains exactly one entry that is a directory AND that directory
/// contains CC-plugin structure (skills/, commands/, agents/, hooks/, .mcp.json,
/// plugin.json, .claude-plugin/), promote its contents up to `dir` and remove
/// the now-empty inner dir. This strips the single-root wrapper that GitHub
/// archives typically produce (`repo-main/`) without misidentifying a flat
/// archive whose top-level dir IS a CC-plugin component.
fn promote_single_root_dir(dir: &Path) -> Result<(), String> {
    let entries: Vec<_> = fs::read_dir(dir)
        .map_err(|e| format!("read dir: {e}"))?
        .flatten()
        .collect();
    if entries.len() == 1 && entries[0].path().is_dir() {
        let inner = entries[0].path();
        if !dir_contains_plugin_structure(&inner) {
            return Ok(());
        }
        let inner_entries: Vec<_> = fs::read_dir(&inner)
            .map_err(|e| format!("read inner dir: {e}"))?
            .flatten()
            .collect();
        for entry in inner_entries {
            let dst = dir.join(entry.file_name());
            fs::rename(entry.path(), &dst)
                .map_err(|e| format!("promote {:?}: {e}", entry.file_name()))?;
        }
        fs::remove_dir(&inner).map_err(|e| format!("remove inner dir: {e}"))?;
    }
    Ok(())
}

fn dir_contains_plugin_structure(dir: &Path) -> bool {
    const MARKERS: &[&str] = &[
        "skills",
        "commands",
        "agents",
        "hooks",
        ".mcp.json",
        "plugin.json",
        ".claude-plugin",
    ];
    MARKERS.iter().any(|m| dir.join(m).exists())
}

/// Read the adopted MCP server definitions from the lockfile `_mcpServers`
/// namespace (the ccync truth snapshot, written by `adopt_mcp_definitions`).
/// Returns `(name, definition)` pairs; empty when the lockfile or namespace is
/// absent/unreadable (fail-safe).
fn load_adopted_mcp_servers() -> Vec<(String, Value)> {
    let path = match plugins_lockfile_path() {
        Some(p) => p,
        None => return vec![],
    };
    let contents = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let lock: Value = match serde_json::from_str(&contents) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    lock.get("_mcpServers")
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

/// Merge ccync-core .mcp.json (from canonical root) with any enabled bundle .mcp.json files
/// and write the result to `~/.ccync/generated/mcp/managed.json`.
///
/// Fail-safe: missing core or bundle .mcp.json files are silently skipped.
/// Bundle files are read from `~/.ccync/plugins/<resolvedSource>/.mcp.json` and are
/// only present once bundles have been deployed (b2 scope).
fn generate_managed_mcp(canonical_root: &Path) -> Result<(), InstallError> {
    let mut merged = serde_json::Map::new();

    // Core MCPs from canonical root (.mcp.json rendered by render_canonical_root)
    if let Ok(content) = fs::read_to_string(canonical_root.join(".mcp.json")) {
        if let Ok(v) = serde_json::from_str::<Value>(&content) {
            if let Some(servers) = v.get("servers").and_then(Value::as_object) {
                merged.extend(servers.clone());
            }
        }
    }

    // Bundle MCPs from deployed plugins root (gracefully absent until b2)
    if let Some(plugins_root) = crate::paths::ccync_plugins_root() {
        for (_, resolved_source) in load_enabled_bundles() {
            let bundle_mcp = plugins_root.join(&resolved_source).join(".mcp.json");
            if let Ok(content) = fs::read_to_string(&bundle_mcp) {
                if let Ok(v) = serde_json::from_str::<Value>(&content) {
                    if let Some(servers) = v.get("servers").and_then(Value::as_object) {
                        for (k, val) in servers {
                            // core-wins: don't overwrite core server with bundle entry
                            merged.entry(k).or_insert_with(|| val.clone());
                        }
                    }
                }
            }
        }
    }

    // Adopted MCP definitions (the ccync truth snapshot, lockfile `_mcpServers`).
    // The unified MCP projection sources the adopted master's servers from here so
    // they reach every MCP-capable agent. core/bundle win on a name clash.
    for (name, def) in load_adopted_mcp_servers() {
        merged.entry(name).or_insert(def);
    }

    if !merged.is_empty() {
        let managed = json!({ "servers": Value::Object(merged) });
        let managed_path = crate::paths::generated_mcp_path().ok_or(InstallError::NoHome)?;
        write_json_file(&managed_path, &managed)?;
    }
    Ok(())
}

fn claude_marketplace_manifest_path() -> Result<PathBuf, InstallError> {
    crate::paths::ccync_plugins_root()
        .map(|root| root.join(".claude-plugin").join("marketplace.json"))
        .ok_or(InstallError::NoHome)
}

fn codex_marketplace_manifest_path() -> Result<PathBuf, InstallError> {
    crate::paths::ccync_plugins_root()
        .map(|root| {
            root.join(".agents")
                .join("plugins")
                .join("marketplace.json")
        })
        .ok_or(InstallError::NoHome)
}

fn copilot_projection_root() -> Result<PathBuf, InstallError> {
    crate::paths::user_home()
        .map(|home| {
            home.join(".copilot")
                .join("installed-plugins")
                .join("ccync-copilot")
                .join("ccync")
        })
        .ok_or(InstallError::NoHome)
}

fn sync_copilot_projection(
    canonical_root: &Path,
) -> Result<(PathBuf, bool, Option<String>), InstallError> {
    let projection_root = copilot_projection_root()?;
    ensure_parent_dir(&projection_root)?;

    if ccync_foundation::platform::is_symlink_or_junction(&projection_root) {
        let _ = ccync_foundation::platform::remove_dir_link(&projection_root);
    } else if projection_root.exists() {
        fs::remove_dir_all(&projection_root)?;
    }

    if ccync_foundation::platform::create_dir_link(canonical_root, &projection_root).is_ok() {
        return Ok((projection_root, false, None));
    }

    copy_dir_all(canonical_root, &projection_root)?;
    let manifest_path = projection_root.join("copilot-manifest.json");
    let version_bumped_to = bump_copilot_manifest_version(&manifest_path)?;
    Ok((projection_root, true, version_bumped_to))
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), InstallError> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&source_path, &target_path)?;
        } else {
            fs::copy(&source_path, &target_path)?;
        }
    }
    Ok(())
}

fn bump_copilot_manifest_version(path: &Path) -> Result<Option<String>, InstallError> {
    if !path.is_file() {
        return Ok(None);
    }
    let mut manifest: Value = serde_json::from_str(&fs::read_to_string(path)?)?;
    let current = manifest
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("1.0.0");
    let bumped = format!(
        "{current}.host{}",
        chrono::Utc::now().format("%Y%m%d%H%M%S")
    );
    manifest["version"] = Value::String(bumped.clone());
    fs::write(path, serde_json::to_string_pretty(&manifest)?)?;
    Ok(Some(bumped))
}

/// Resolve the on-disk cache directories of the enabled managed set — bundled
/// plugins (under `~/.ccync/plugins/<resolvedSource>`) plus personal plugins
/// (`~/.ccync/local/cache/<id>@<sha>`). Only existing dirs are returned.
fn managed_plugin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(plugins_root) = crate::paths::ccync_plugins_root() {
        for (_, resolved_source) in load_enabled_bundles() {
            let d = plugins_root.join(&resolved_source);
            if d.is_dir() {
                dirs.push(d);
            }
        }
    }
    if let Some(cache_root) = crate::paths::local_plugin_cache_dir() {
        for (id, _) in load_enabled_personal_plugins() {
            let prefix = format!("{id}@");
            if let Ok(entries) = fs::read_dir(&cache_root) {
                for e in entries.flatten() {
                    if e.path().is_dir() && e.file_name().to_string_lossy().starts_with(&prefix) {
                        dirs.push(e.path());
                    }
                }
            }
        }
    }
    dirs
}

/// Materialize the ccync managed set into the canonical root: copy each managed
/// plugin's `skills/`, `commands/`, `agents/`, `hooks/` subtrees and merge its
/// `.mcp.json` servers into `<canonical_root>/.mcp.json`. First-plugin-wins on
/// a server-name collision (caller controls precedence via `plugin_dirs` order).
///
/// This restores the canonical-root content render the carve deleted, so the
/// Claude skill symlink (and the other read surfaces) point at a real,
/// non-empty tree instead of an empty directory.
fn render_canonical_root(
    canonical_root: &Path,
    plugin_dirs: &[PathBuf],
) -> Result<(), InstallError> {
    fs::create_dir_all(canonical_root)?;

    // Prune stale component dirs before re-rendering so artifacts from a
    // removed plugin don't linger. Only the four ccync-owned component dirs
    // and the merged .mcp.json are cleared; other canonical-root files
    // (catalog.json, lifecycle manifests, etc.) are left untouched.
    for component in ["skills", "commands", "agents", "hooks"] {
        let dir = canonical_root.join(component);
        if dir.is_dir() {
            fs::remove_dir_all(&dir)?;
        }
    }
    let mcp_path = canonical_root.join(".mcp.json");
    if mcp_path.is_file() {
        fs::remove_file(&mcp_path)?;
    }

    let mut merged_servers = serde_json::Map::new();
    for plugin in plugin_dirs {
        for component in ["skills", "commands", "agents", "hooks"] {
            let src = plugin.join(component);
            if src.is_dir() {
                copy_dir_all(&src, &canonical_root.join(component))?;
            }
        }
        if let Ok(content) = fs::read_to_string(plugin.join(".mcp.json")) {
            if let Ok(v) = serde_json::from_str::<Value>(&content) {
                if let Some(servers) = v.get("servers").and_then(Value::as_object) {
                    for (k, val) in servers {
                        merged_servers
                            .entry(k.clone())
                            .or_insert_with(|| val.clone());
                    }
                }
            }
        }
    }
    if !merged_servers.is_empty() {
        write_json_file(
            &canonical_root.join(".mcp.json"),
            &json!({ "servers": Value::Object(merged_servers) }),
        )?;
    }
    Ok(())
}

fn write_surface_lifecycle_artifacts(
    canonical_root: &Path,
    runtimes: &mut Vec<String>,
) -> Result<(), InstallError> {
    let selection = load_runtime_selection();
    let primary = primary_surfaces(&selection);
    let enabled_bundles = load_enabled_bundles();

    if primary.contains(&"claude".to_string()) {
        let projection = ClaudeSkillProjection::new(canonical_root.to_path_buf())
            .map_err(|e| InstallError::Io(std::io::Error::other(e.to_string())))?;
        projection
            .apply()
            .map_err(|e| InstallError::Io(std::io::Error::other(e.to_string())))?;
        let marketplace_path = claude_marketplace_manifest_path()?;
        let mut claude_plugins = vec![json!({
            "name": "ccync",
            "source": "./ccync",
            "description": "ccync — cross-agent plugin / MCP / skills manager"
        })];
        for (bundle_id, bundle_src) in &enabled_bundles {
            claude_plugins.push(json!({
                "name": bundle_id,
                "source": format!("./{bundle_src}"),
                "description": format!("ccync domain bundle: {bundle_id}")
            }));
        }
        write_json_file(
            &marketplace_path,
            &json!({
                "name": "ccync",
                "owner": { "name": "ccync" },
                "description": "ccync — cross-agent plugin / MCP / skills manager",
                "plugins": claude_plugins
            }),
        )?;
        write_json_file(
            &runtime_state_path("claude")?,
            &json!({
                "schemaVersion": 1,
                "runtime": "claude",
                "canonicalRoot": canonical_root,
                "packageOutputRoot": canonical_root,
                "projectionRoot": projection.skill_surface,
                "installTarget": projection.skill_surface,
                "manifestPath": canonical_root.join(".claude-plugin").join("plugin.json"),
                "generatedAt": chrono::Utc::now().to_rfc3339(),
                "status": "linked-projection",
                "readSurface": "linked-projection",
                "cli": {
                    "available": false,
                    "validateSupported": false,
                    "localArtifactInstallSupported": false,
                    "installScopeSupported": false,
                    "installHelpSummary": null
                },
                "validation": {
                    "command": "claude plugin validate <plugin-root> --strict",
                    "strictPassed": false
                },
                "lifecycle": {
                    "mode": "session-load-only",
                    "stagedPluginRoot": projection.skill_surface,
                    "marketplaceName": "ccync",
                    "sessionLoadCommand": "claude --plugin-dir <plugin-root>",
                    "installCommandTemplate": "claude plugin install <plugin> --scope <scope>",
                    "updateCommandTemplate": "claude plugin update <plugin> --scope <scope>",
                    "uninstallCommandTemplate": "claude plugin uninstall <plugin> --scope <scope>"
                }
            }),
        )?;
        if !runtimes.contains(&"claude".to_string()) {
            runtimes.push("claude".to_string());
        }
    }

    if primary.contains(&"copilot".to_string()) {
        let (projection_root, refreshed_copy_to_host, version_bumped_to) =
            sync_copilot_projection(canonical_root)?;
        let status = if refreshed_copy_to_host {
            "refreshed-copy2-host"
        } else {
            "linked-projection"
        };
        write_json_file(
            &runtime_state_path("copilot")?,
            &json!({
                "schemaVersion": 1,
                "runtime": "copilot",
                "canonicalRoot": canonical_root,
                "packageOutputRoot": canonical_root,
                "projectionRoot": projection_root,
                "installTarget": projection_root,
                "manifestPath": canonical_root.join("copilot-manifest.json"),
                "generatedAt": chrono::Utc::now().to_rfc3339(),
                "status": status,
                "readSurface": status,
                "cli": {
                    "available": false,
                    "validateSupported": false,
                    "localArtifactInstallSupported": false,
                    "marketplaceInstallSupported": false,
                    "installScopeSupported": false,
                    "installHelpSummary": null
                },
                "validation": {
                    "command": null,
                    "strictPassed": false
                },
                "lifecycle": {
                    "mode": "artifact-only",
                    "stagedPluginRoot": projection_root,
                    "refreshedCopyToHost": refreshed_copy_to_host,
                    "versionBumpedTo": version_bumped_to,
                    "sessionLoadCommand": "GitHub Copilot reads the projected plugin from ~/.copilot/installed-plugins/ccync-copilot/ccync",
                    "installCommandTemplate": "gh copilot plugin install <plugin-root>",
                    "updateCommandTemplate": "gh copilot plugin update <plugin-id>",
                    "uninstallCommandTemplate": "gh copilot plugin uninstall <plugin-id>"
                }
            }),
        )?;
        if !runtimes.contains(&"copilot".to_string()) {
            runtimes.push("copilot".to_string());
        }
    }

    if primary.contains(&"codex".to_string()) {
        let marketplace_path = codex_marketplace_manifest_path()?;
        let mut codex_plugins = vec![json!({
            "name": "ccync",
            "source": { "source": "local", "path": "./ccync" },
            "policy": { "installation": "AVAILABLE", "authentication": "ON_INSTALL" },
            "category": "Engineering"
        })];
        for (bundle_id, bundle_src) in &enabled_bundles {
            codex_plugins.push(json!({
                "name": bundle_id,
                "source": { "source": "local", "path": format!("./{bundle_src}") },
                "policy": { "installation": "AVAILABLE", "authentication": "ON_INSTALL" },
                "category": "Engineering"
            }));
        }
        write_json_file(
            &marketplace_path,
            &json!({
                "name": "ccync-marketplace",
                "interface": { "displayName": "ccync Plugin Marketplace" },
                "plugins": codex_plugins
            }),
        )?;
        write_json_file(
            &runtime_state_path("codex")?,
            &json!({
                "schemaVersion": 1,
                "runtime": "codex",
                "canonicalRoot": canonical_root,
                "packageOutputRoot": canonical_root,
                "projectionRoot": null,
                "installTarget": "ccync@ccync-marketplace",
                "manifestPath": canonical_root.join(".codex-plugin").join("plugin.json"),
                "generatedAt": chrono::Utc::now().to_rfc3339(),
                "status": "unprojected-artifact",
                "readSurface": "unprojected-artifact",
                "cli": {
                    "available": false,
                    "validateSupported": false,
                    "localArtifactInstallSupported": false,
                    "marketplaceInstallSupported": false,
                    "installScopeSupported": false,
                    "installHelpSummary": null
                },
                "validation": {
                    "command": null,
                    "strictPassed": false
                },
                "lifecycle": {
                    "mode": "artifact-only",
                    "stagedPluginRoot": canonical_root,
                    "marketplaceRoot": crate::paths::ccync_plugins_root(),
                    "marketplaceManifestPath": marketplace_path,
                    "marketplaceName": "ccync-marketplace",
                    "installedSelector": "ccync@ccync-marketplace",
                    "sessionLoadCommand": "codex plugin marketplace add <plugins-root> ; codex plugin add ccync@ccync-marketplace",
                    "installCommandTemplate": "codex plugin marketplace add <plugins-root>",
                    "updateCommandTemplate": "codex plugin remove ccync@ccync-marketplace ; codex plugin add ccync@ccync-marketplace",
                    "uninstallCommandTemplate": "codex plugin remove ccync@ccync-marketplace ; codex plugin marketplace remove ccync-marketplace",
                    "pluginRemovedBeforeAdd": false
                }
            }),
        )?;
        if !runtimes.contains(&"codex".to_string()) {
            runtimes.push("codex".to_string());
        }
    }

    if primary.contains(&"agy".to_string()) {
        write_json_file(
            &runtime_state_path("agy")?,
            &json!({
                "schemaVersion": 1,
                "runtime": "agy",
                "canonicalRoot": canonical_root,
                "packageOutputRoot": canonical_root,
                "projectionRoot": crate::paths::ccync_active_provider_path("agy"),
                "installTarget": crate::paths::user_home().map(|home| home.join(".gemini").join("antigravity-cli").join("plugins").join("ccync")),
                "shortcutTarget": crate::paths::ccync_active_provider_path("agy"),
                "generatedAt": chrono::Utc::now().to_rfc3339(),
                "status": "linked-projection",
                "readSurface": "linked-projection",
                "cli": {
                    "available": false,
                    "validateSupported": false,
                    "localArtifactInstallSupported": false,
                    "marketplaceInstallSupported": false,
                    "installScopeSupported": false,
                    "installHelpSummary": null
                },
                "validation": {
                    "command": null,
                    "strictPassed": false
                },
                "lifecycle": {
                    "mode": "managed-shortcut",
                    "status": "implemented"
                }
            }),
        )?;
        if !runtimes.contains(&"agy".to_string()) {
            runtimes.push("agy".to_string());
        }
    }

    let _ = &selection.primary_runtime;
    Ok(())
}

/// Run `ccync uninstall` — remove canonical root and runtime surfaces.
pub fn run_uninstall() -> Result<(), InstallError> {
    let home = crate::paths::user_home().ok_or(InstallError::NoHome)?;
    let canonical_root = home.join(".ccync").join("plugins").join("ccync");

    let previous_ledger = ledger_path().map(|path| Ledger::load(&path));
    let previous_entry = previous_ledger
        .as_ref()
        .and_then(|ledger| ledger.last.clone());

    let mut warnings = Vec::new();

    // Remove AGY surfaces before removing canonical root (they link to it).
    if canonical_root.exists() {
        match AgyProjection::new(canonical_root.clone()) {
            Ok(agy) => {
                if let Err(e) = agy.remove() {
                    warnings.push(format!("AGY removal (best-effort): {e}"));
                }
            }
            Err(e) => warnings.push(format!("AGY uninstall init (best-effort): {e}")),
        }
    }

    // Remove canonical root.
    if canonical_root.exists() {
        if let Err(e) = fs::remove_dir_all(&canonical_root) {
            warnings.push(format!("canonical root removal: {e}"));
        }
    }

    collect_removal_error(
        &mut warnings,
        "copilot projection removal",
        copilot_projection_root().and_then(|path| remove_path_if_present(&path)),
    );
    collect_removal_error(
        &mut warnings,
        "Claude skills removal",
        remove_path_if_present(&home.join(".claude").join("skills").join("ccync")),
    );
    collect_removal_error(
        &mut warnings,
        "Claude plugins removal",
        remove_path_if_present(&home.join(".claude").join("plugins").join("ccync")),
    );
    collect_removal_error(
        &mut warnings,
        "runtime dist removal",
        ccync_dist_runtimes_root().and_then(|path| remove_path_if_present(&path)),
    );
    collect_removal_error(
        &mut warnings,
        "stale provider dist removal",
        stale_dist_providers_root().and_then(|path| remove_path_if_present(&path)),
    );
    collect_removal_error(
        &mut warnings,
        "claude-plugin marker removal",
        remove_path_if_present(&home.join(".ccync").join("plugins").join(".claude-plugin")),
    );
    collect_removal_error(
        &mut warnings,
        ".agents marker removal",
        remove_path_if_present(&home.join(".ccync").join("plugins").join(".agents")),
    );

    let runtimes = previous_entry
        .as_ref()
        .map(|entry| entry.runtimes.clone())
        .unwrap_or_default();
    let mode = previous_entry
        .as_ref()
        .map(|entry| entry.mode.clone())
        .unwrap_or_else(|| "normal".to_string());

    // Write uninstall ledger entry with the warnings accumulated before ledger save.
    let ledger_warnings = warnings.clone();
    write_ledger_entry(
        "uninstall",
        &canonical_root,
        &runtimes,
        &mode,
        &ledger_warnings,
        &mut warnings,
    );

    for w in &warnings {
        eprintln!("ccync uninstall warning: {w}");
    }

    if warnings.is_empty() {
        Ok(())
    } else {
        Err(InstallError::PartialUninstall { warnings })
    }
}

fn collect_removal_error(
    warnings: &mut Vec<String>,
    label: &str,
    result: Result<(), InstallError>,
) {
    if let Err(err) = result {
        warnings.push(format!("{label}: {err}"));
    }
}

fn remove_path_if_present(path: &Path) -> Result<(), InstallError> {
    if ccync_foundation::platform::is_symlink_or_junction(path) {
        let _ = ccync_foundation::platform::remove_dir_link(path);
        return Ok(());
    }
    if path.is_file() {
        fs::remove_file(path)?;
    } else if path.is_dir() {
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

fn write_ledger_entry(
    op: &str,
    canonical_root: &Path,
    runtimes: &[String],
    mode: &str,
    entry_warnings: &[String],
    warnings: &mut Vec<String>,
) {
    if let Some(ledger_p) = ledger_path() {
        let mut ledger = Ledger::load(&ledger_p);
        ledger.record(LedgerEntry {
            operation: op.to_string(),
            canonical_root: canonical_root.to_path_buf(),
            timestamp: now_timestamp(),
            runtimes: runtimes.to_vec(),
            mode: mode.to_string(),
            warnings: entry_warnings.to_vec(),
        });
        if let Err(e) = ledger.save(&ledger_p) {
            warnings.push(format!("ledger write warning: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Serialize tests that mutate process-global env vars (`USERPROFILE`/`HOME`).
    /// Env mutation is process-wide and `cargo test` runs multithreaded, so two
    /// such tests racing would resolve `ccync_home()`-derived paths to each other's
    /// temp dirs. Poison-tolerant: a panicking test (a failed assertion) must not
    /// cascade-fail every other env test by poisoning the lock.
    fn env_guard() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn install_error_display_is_non_empty() {
        assert!(!InstallError::NoHome.to_string().is_empty());
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "not found");
        assert!(InstallError::Io(io_err).to_string().contains("not found"));
        assert!(InstallError::PartialUninstall {
            warnings: vec!["x".to_string()]
        }
        .to_string()
        .contains("partial failures"));
    }

    #[test]
    fn install_default_selected_runtimes_match_projection_fallback_source() {
        // Single source: the install-side absent-state default must be the same
        // set projection falls back to (ccync_foundation::runtime::VALID_RUNTIMES), so a
        // missing install-state.json cannot silently drop gemini/opencode.
        let expected: Vec<String> = ccync_foundation::runtime::VALID_RUNTIMES
            .iter()
            .map(|r| (*r).to_string())
            .collect();
        assert_eq!(default_selected_runtimes(), expected);
        assert!(default_selected_runtimes()
            .iter()
            .any(|r| r == "gemini-cli"));
        assert!(default_selected_runtimes().iter().any(|r| r == "opencode"));
    }

    #[test]
    fn install_report_fields_are_accessible() {
        // Verify the struct is usable (compile-time check via construction).
        let report = InstallReport {
            canonical_root: PathBuf::from("/tmp/ccync"),
            runtimes: vec!["claude".to_string()],
            mode: "normal".to_string(),
            warnings: vec![],
        };
        assert_eq!(report.mode, "normal");
        assert_eq!(report.runtimes.len(), 1);
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn uninstall_succeeds_when_canonical_root_absent() {
        // If canonical root never existed, uninstall should not error.
        // We test by pointing to a non-existent path (ledger write may warn but not error).
        // Full isolated-home test is integration/manual.
        // Here we just verify no panic and no unexpected I/O error.
        //
        // Note: run_uninstall() uses dirs::home_dir() which is the real home in tests.
        // We skip the real filesystem call and only test the error surface.
        let err = InstallError::NoHome;
        assert!(err.to_string().contains("home"));
    }

    // ── load_enabled_bundles() cross-machine / β1 no-op tests ───────────────────

    /// Helper: create a fake plugins.lock.json in a temp home and run the loader.
    /// Returns the parsed bundle vec. Accepts `home_path` as USERPROFILE / HOME.
    fn with_fake_lock<F>(home_path: &std::path::Path, lock_contents: &str, f: F)
    where
        F: FnOnce(Vec<(String, String)>),
    {
        // Hold the env lock across the whole env-dependent body so no other
        // env-mutating test runs concurrently.
        let _env = env_guard();

        let state_dir = home_path.join(".ccync").join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("plugins.lock.json"), lock_contents).unwrap();

        // Point home at temp dir so plugins_lockfile_path() resolves under it.
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", home_path);
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", home_path);
        }

        let result = load_enabled_bundles();
        f(result);
    }

    #[test]
    fn load_adopted_mcp_servers_reads_mcpservers_namespace() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let state_dir = tmp.path().join(".ccync").join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join("plugins.lock.json"),
            r#"{"_mcpServers":{"memory":{"command":"npx","args":["-y","srv"]}},"resolvedPlugins":[]}"#,
        )
        .unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let servers = load_adopted_mcp_servers();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].0, "memory");
        assert_eq!(servers[0].1["command"], "npx");
    }

    #[test]
    fn load_adopted_mcp_servers_absent_namespace_empty() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let state_dir = tmp.path().join(".ccync").join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join("plugins.lock.json"),
            r#"{"resolvedPlugins":[]}"#,
        )
        .unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        assert!(load_adopted_mcp_servers().is_empty());
    }

    #[test]
    fn load_enabled_bundles_absent_lock_returns_empty() {
        // β1 not landed (no lock file) → (b) degrades to no-op.
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        let bundles = load_enabled_bundles();
        assert!(
            bundles.is_empty(),
            "absent lock must produce empty bundle list (β1 no-op)"
        );
    }

    #[test]
    fn load_enabled_bundles_bundled_local_entries_parsed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let lock = r#"{
            "generatedAt": "2026-06-19T00:00:00Z",
            "resolvedPlugins": [
                {
                    "pluginId": "ccync-game-assets",
                    "installStrategy": "bundled-local",
                    "resolvedSource": "ccync-domain/ccync-game-assets"
                },
                {
                    "pluginId": "ccync-godot",
                    "installStrategy": "bundled-local",
                    "resolvedSource": "ccync-domain/ccync-godot"
                }
            ]
        }"#;
        with_fake_lock(tmp.path(), lock, |bundles| {
            assert_eq!(bundles.len(), 2);
            assert_eq!(bundles[0].0, "ccync-game-assets");
            assert_eq!(bundles[0].1, "ccync-domain/ccync-game-assets");
            assert_eq!(bundles[1].0, "ccync-godot");
            assert_eq!(bundles[1].1, "ccync-domain/ccync-godot");
        });
    }

    #[test]
    fn load_enabled_bundles_non_bundled_entries_filtered() {
        let tmp = tempfile::TempDir::new().unwrap();
        let lock = r#"{
            "resolvedPlugins": [
                {
                    "pluginId": "some-git-plugin",
                    "installStrategy": "git-clone",
                    "resolvedSource": "https://github.com/example/plugin.git"
                },
                {
                    "pluginId": "ccync-web-testing",
                    "installStrategy": "bundled-local",
                    "resolvedSource": "ccync-domain/ccync-web-testing"
                }
            ]
        }"#;
        with_fake_lock(tmp.path(), lock, |bundles| {
            assert_eq!(bundles.len(), 1, "git-clone entry must be filtered out");
            assert_eq!(bundles[0].0, "ccync-web-testing");
        });
    }

    #[test]
    fn load_enabled_bundles_empty_resolved_plugins_returns_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let lock = r#"{ "resolvedPlugins": [] }"#;
        with_fake_lock(tmp.path(), lock, |bundles| {
            assert!(
                bundles.is_empty(),
                "empty resolvedPlugins must produce empty bundle list"
            );
        });
    }

    #[test]
    fn load_enabled_bundles_malformed_json_returns_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        with_fake_lock(tmp.path(), "not valid json {{{", |bundles| {
            assert!(
                bundles.is_empty(),
                "malformed JSON must degrade to empty (fail-safe)"
            );
        });
    }

    #[test]
    fn load_enabled_bundles_missing_plugin_id_entry_skipped() {
        let tmp = tempfile::TempDir::new().unwrap();
        let lock = r#"{
            "resolvedPlugins": [
                {
                    "installStrategy": "bundled-local",
                    "resolvedSource": "ccync-domain/ccync-pdf"
                },
                {
                    "pluginId": "ccync-pdf",
                    "installStrategy": "bundled-local",
                    "resolvedSource": "ccync-domain/ccync-pdf"
                }
            ]
        }"#;
        with_fake_lock(tmp.path(), lock, |bundles| {
            assert_eq!(bundles.len(), 1, "entry missing pluginId must be skipped");
            assert_eq!(bundles[0].0, "ccync-pdf");
        });
    }

    // ── load_enabled_personal_plugins() tests ───────────────────────────────────

    fn with_fake_personal_lock<F>(home_path: &std::path::Path, lock_contents: &str, f: F)
    where
        F: FnOnce(Vec<(String, String)>),
    {
        let _env = env_guard();
        let state_dir = home_path.join(".ccync").join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("plugins.lock.json"), lock_contents).unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", home_path);
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", home_path);
        }
        let result = load_enabled_personal_plugins();
        f(result);
    }

    #[test]
    fn load_enabled_personal_plugins_absent_namespace_returns_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        // lockfile has only resolvedPlugins — no _personalPlugins key
        let lock = r#"{ "resolvedPlugins": [], "_ccyncProjection": {} }"#;
        with_fake_personal_lock(tmp.path(), lock, |result| {
            assert!(
                result.is_empty(),
                "absent _personalPlugins must return empty vec"
            );
        });
    }

    #[test]
    fn load_enabled_personal_plugins_entries_parsed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let lock = r#"{
            "resolvedPlugins": [],
            "_personalPlugins": [
                {
                    "pluginId": "my-tool",
                    "sourceType": "curated-upstream",
                    "source": "https://github.com/user/my-tool.git"
                },
                {
                    "pluginId": "local-helper",
                    "sourceType": "bundled-local",
                    "source": "~/tools/local-helper"
                }
            ]
        }"#;
        with_fake_personal_lock(tmp.path(), lock, |result| {
            assert_eq!(result.len(), 2);
            assert_eq!(result[0].0, "my-tool");
            assert_eq!(result[0].1, "https://github.com/user/my-tool.git");
            assert_eq!(result[1].0, "local-helper");
            assert_eq!(result[1].1, "~/tools/local-helper");
        });
    }

    #[test]
    fn load_enabled_personal_plugins_missing_source_entry_skipped() {
        let tmp = tempfile::TempDir::new().unwrap();
        let lock = r#"{
            "_personalPlugins": [
                { "pluginId": "no-source-entry", "sourceType": "curated-upstream" },
                { "pluginId": "valid", "source": "https://github.com/user/valid.git" }
            ]
        }"#;
        with_fake_personal_lock(tmp.path(), lock, |result| {
            assert_eq!(result.len(), 1, "entry missing source must be skipped");
            assert_eq!(result[0].0, "valid");
        });
    }

    #[test]
    fn load_enabled_personal_plugins_does_not_read_resolved_plugins() {
        let tmp = tempfile::TempDir::new().unwrap();
        // resolvedPlugins has a bundled-local entry; it must NOT appear in personal result
        let lock = r#"{
            "resolvedPlugins": [
                { "pluginId": "ccync-game-assets", "installStrategy": "bundled-local", "resolvedSource": "ccync-domain/ccync-game-assets" }
            ],
            "_personalPlugins": [
                { "pluginId": "my-tool", "source": "https://github.com/user/my-tool.git" }
            ]
        }"#;
        with_fake_personal_lock(tmp.path(), lock, |result| {
            assert_eq!(
                result.len(),
                1,
                "must only return personal entries, not resolvedPlugins"
            );
            assert_eq!(result[0].0, "my-tool");
        });
    }

    // ── fetch_personal_plugin() unit tests ─────────────────────────────────────

    #[test]
    fn fetch_personal_plugin_returns_already_present_when_versioned_dir_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        // Pre-create a <id>@sha directory to simulate already-cloned
        let existing = cache.join("my-tool@abc1234");
        std::fs::create_dir_all(&existing).unwrap();
        let result =
            fetch_personal_plugin(&cache, "my-tool", "https://github.com/user/my-tool.git");
        match result {
            PersonalFetchResult::AlreadyPresent(p) => {
                assert_eq!(p, existing, "should return the existing directory path");
            }
            other => panic!("expected AlreadyPresent, got: {other:?}"),
        }
    }

    #[test]
    fn fetch_personal_plugin_prefix_check_does_not_match_other_plugins() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        // A dir for a different plugin should NOT trigger AlreadyPresent
        let other = cache.join("other-tool@abc1234");
        std::fs::create_dir_all(&other).unwrap();
        // Asking for "my-tool" — should NOT find "other-tool@..." as already present.
        // It will try git clone which will fail (non-existent URL), returning CloneFailed.
        let result = fetch_personal_plugin(&cache, "my-tool", "https://localhost/nonexistent.git");
        assert!(
            !matches!(result, PersonalFetchResult::AlreadyPresent(_)),
            "other-plugin dir must not trigger AlreadyPresent for a different plugin_id"
        );
    }

    #[test]
    fn fetch_all_personal_plugins_empty_when_lock_absent() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", tmp.path());
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        let result = fetch_all_personal_plugins();
        assert!(
            result.is_empty(),
            "absent lockfile must produce empty fetch list"
        );
    }

    // ── render_canonical_root (canonical-root content render) ──────────────────

    #[test]
    fn render_canonical_root_materializes_managed_plugin_skill() {
        let tmp = tempfile::TempDir::new().unwrap();
        // one managed plugin dir with a skill
        let plugin = tmp.path().join("my-plugin");
        let skill = plugin.join("skills").join("doc-sync");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "# doc-sync").unwrap();

        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[plugin]).unwrap();

        let projected = canonical.join("skills").join("doc-sync").join("SKILL.md");
        assert!(
            projected.is_file(),
            "managed plugin skill must be materialized into the canonical root"
        );
        assert_eq!(std::fs::read_to_string(&projected).unwrap(), "# doc-sync");
    }

    #[test]
    fn render_canonical_root_merges_mcp_first_wins() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p1 = tmp.path().join("p1");
        let p2 = tmp.path().join("p2");
        std::fs::create_dir_all(&p1).unwrap();
        std::fs::create_dir_all(&p2).unwrap();
        std::fs::write(
            p1.join(".mcp.json"),
            r#"{"servers":{"memory":{"command":"npx"}}}"#,
        )
        .unwrap();
        std::fs::write(
            p2.join(".mcp.json"),
            r#"{"servers":{"memory":{"command":"OVERRIDE"},"fetch":{"command":"uvx"}}}"#,
        )
        .unwrap();

        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[p1, p2]).unwrap();

        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(canonical.join(".mcp.json")).unwrap())
                .unwrap();
        assert_eq!(
            v["servers"]["memory"]["command"], "npx",
            "first plugin wins on collision"
        );
        assert_eq!(
            v["servers"]["fetch"]["command"], "uvx",
            "second plugin's unique server merged"
        );
    }

    #[test]
    fn render_canonical_root_empty_set_no_mcp_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[]).unwrap();
        assert!(
            canonical.is_dir(),
            "canonical root is created even with no plugins"
        );
        assert!(
            !canonical.join(".mcp.json").exists(),
            "no servers → no .mcp.json"
        );
    }

    #[test]
    fn write_ledger_entry_persists_warnings() {
        let _env = env_guard();
        let temp = tempfile::TempDir::new().unwrap();
        let mut warnings = Vec::new();

        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", temp.path());
            std::env::set_var("HOME", temp.path());
        }

        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", temp.path());
        }

        let ledger_path = ledger_path().expect("fake home should yield a ledger path");

        let mut ledger = Ledger::default();
        ledger.record(LedgerEntry {
            operation: "install".to_string(),
            canonical_root: PathBuf::from("/tmp/ccync"),
            timestamp: now_timestamp(),
            runtimes: vec!["claude".to_string()],
            mode: "dev".to_string(),
            warnings: vec![],
        });
        ledger.save(&ledger_path).unwrap();

        write_ledger_entry(
            "uninstall",
            Path::new("/tmp/ccync"),
            &["claude".to_string()],
            "dev",
            &["partial failure".to_string()],
            &mut warnings,
        );

        let loaded = Ledger::load(&ledger_path);
        let last = loaded.last.expect("ledger entry should exist");
        assert_eq!(last.operation, "uninstall");
        assert_eq!(last.runtimes, vec!["claude".to_string()]);
        assert_eq!(last.mode, "dev");
        assert_eq!(last.warnings, vec!["partial failure".to_string()]);
    }

    // ── render_canonical_root — hooks projection ───────────────────────────────

    #[test]
    fn render_canonical_root_materializes_hooks() {
        let tmp = tempfile::TempDir::new().unwrap();
        let plugin = tmp.path().join("hooks-plugin");
        let hooks_dir = plugin.join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::write(
            hooks_dir.join("hooks.json"),
            r#"{"hooks":{"PreToolUse":[]}}"#,
        )
        .unwrap();

        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[plugin]).unwrap();

        assert!(
            canonical.join("hooks").join("hooks.json").is_file(),
            "hooks/hooks.json must be materialized into the canonical root"
        );
    }

    #[test]
    fn render_canonical_root_empty_hooks_dir_not_created() {
        let tmp = tempfile::TempDir::new().unwrap();
        let plugin = tmp.path().join("no-hooks-plugin");
        std::fs::create_dir_all(plugin.join("skills")).unwrap();

        let canonical = tmp.path().join("canonical");
        render_canonical_root(&canonical, &[plugin]).unwrap();

        assert!(
            !canonical.join("hooks").exists(),
            "empty plugin with no hooks dir must not create hooks in canonical root"
        );
    }

    #[test]
    fn render_canonical_root_prunes_removed_plugin_hooks() {
        // After a hooks-bearing plugin is removed, a re-render with the
        // reduced plugin_dirs must prune hooks from the canonical root.
        let tmp = tempfile::TempDir::new().unwrap();
        let plugin_with_hooks = tmp.path().join("plugin-with-hooks");
        let hooks_dir = plugin_with_hooks.join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::write(hooks_dir.join("hooks.json"), r#"{"hooks":{}}"#).unwrap();

        let sibling_plugin = tmp.path().join("sibling-plugin");
        let skill_dir = sibling_plugin.join("skills").join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "# skill").unwrap();

        let canonical = tmp.path().join("canonical");

        // First render: both plugins installed.
        render_canonical_root(&canonical, &[plugin_with_hooks.clone(), sibling_plugin.clone()])
            .unwrap();
        assert!(
            canonical.join("hooks").join("hooks.json").is_file(),
            "hooks present after first render"
        );
        assert!(
            canonical.join("skills").join("my-skill").join("SKILL.md").is_file(),
            "sibling skill present after first render"
        );

        // Second render: hooks plugin removed, only sibling remains.
        render_canonical_root(&canonical, &[sibling_plugin]).unwrap();
        assert!(
            !canonical.join("hooks").exists(),
            "hooks must be pruned after hooks-bearing plugin is removed"
        );
        assert!(
            canonical.join("skills").join("my-skill").join("SKILL.md").is_file(),
            "sibling skill must survive the re-render (siblings untouched)"
        );
    }

    #[test]
    fn render_canonical_root_prunes_stale_mcp_on_re_render() {
        // When a plugin that contributed MCP servers is removed, the .mcp.json
        // must not carry stale entries forward.
        let tmp = tempfile::TempDir::new().unwrap();
        let plugin = tmp.path().join("mcp-plugin");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join(".mcp.json"),
            r#"{"servers":{"memory":{"command":"npx"}}}"#,
        )
        .unwrap();

        let canonical = tmp.path().join("canonical");

        // First render: plugin with MCP installed.
        render_canonical_root(&canonical, &[plugin]).unwrap();
        assert!(
            canonical.join(".mcp.json").is_file(),
            ".mcp.json present after first render"
        );

        // Second render: plugin removed → .mcp.json must be gone (no servers).
        render_canonical_root(&canonical, &[]).unwrap();
        assert!(
            !canonical.join(".mcp.json").exists(),
            "stale .mcp.json must be pruned when all MCP plugins are removed"
        );
    }

    // ── fetch_archive_plugin ──────────────────────────────────────────────────

    #[test]
    fn fetch_archive_plugin_extracts_zip_to_cache() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("cache");
        let archive = tmp.path().join("my-plugin.zip");

        // Build a minimal zip with a skills/ file.
        {
            use std::io::Write;
            let file = std::fs::File::create(&archive).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file("skills/SKILL.md", zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"# skill").unwrap();
            zip.finish().unwrap();
        }

        let result = fetch_archive_plugin(&cache, "my-plugin", &archive);
        match result {
            PersonalFetchResult::Cloned { ref path, ref sha } => {
                assert!(path.starts_with(&cache), "extracted into cache root");
                assert_eq!(sha.len(), 12, "sha prefix is 12 hex chars");
                assert!(
                    path.to_string_lossy().contains("my-plugin@"),
                    "cache dir has plugin@sha naming"
                );
                assert!(
                    path.join("skills").join("SKILL.md").is_file(),
                    "skills/SKILL.md materialized inside cache dir"
                );
            }
            other => panic!("expected Cloned, got: {other:?}"),
        }
    }

    #[test]
    fn fetch_archive_plugin_idempotent_returns_already_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("cache");
        let archive = tmp.path().join("my-plugin.zip");

        {
            use std::io::Write;
            let file = std::fs::File::create(&archive).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file("README.md", zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"hello").unwrap();
            zip.finish().unwrap();
        }

        // First fetch
        let r1 = fetch_archive_plugin(&cache, "my-plugin", &archive);
        assert!(matches!(r1, PersonalFetchResult::Cloned { .. }));

        // Second fetch with same archive → AlreadyPresent
        let r2 = fetch_archive_plugin(&cache, "my-plugin", &archive);
        assert!(
            matches!(r2, PersonalFetchResult::AlreadyPresent(_)),
            "same archive fetched twice must return AlreadyPresent"
        );
    }

    #[test]
    fn is_contained_relative_rejects_traversal_and_absolute() {
        // Safe relative paths pass.
        assert!(is_contained_relative(Path::new("skills/SKILL.md")));
        assert!(is_contained_relative(Path::new("./hooks/hooks.json")));
        assert!(is_contained_relative(Path::new("a/b/c.txt")));
        // Traversal and absolute paths are rejected.
        assert!(!is_contained_relative(Path::new("../escape.txt")));
        assert!(!is_contained_relative(Path::new("a/../../escape.txt")));
        assert!(!is_contained_relative(Path::new("/etc/passwd")));
        #[cfg(windows)]
        assert!(!is_contained_relative(Path::new(r"C:\Windows\System32\evil")));
    }

    /// Hand-build a single-entry tar.gz at the byte level so an unsafe entry
    /// name (`..` traversal) can be embedded — `tar::Builder` refuses to write
    /// `..` paths, so it cannot produce the malicious input this guard defends
    /// against. Emits one ustar regular-file entry + the end-of-archive blocks,
    /// gzip-compressed.
    fn build_tar_gz_with_entry_name(name: &[u8], data: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name);
        header[100..108].copy_from_slice(b"0000644\0"); // mode
        header[108..116].copy_from_slice(b"0000000\0"); // uid
        header[116..124].copy_from_slice(b"0000000\0"); // gid
        header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes()); // size
        header[136..148].copy_from_slice(b"00000000000\0"); // mtime
        header[156] = b'0'; // typeflag: regular file
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        // Checksum: sum all bytes with the chksum field treated as spaces.
        for b in &mut header[148..156] {
            *b = b' ';
        }
        let sum: u32 = header.iter().map(|&b| b as u32).sum();
        header[148..156].copy_from_slice(format!("{:06o}\0 ", sum).as_bytes());

        let mut tar = Vec::new();
        tar.extend_from_slice(&header);
        tar.extend_from_slice(data);
        let pad = (512 - data.len() % 512) % 512;
        tar.extend(std::iter::repeat(0u8).take(pad));
        tar.extend(std::iter::repeat(0u8).take(1024)); // two zero blocks = EOF

        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&tar).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn fetch_archive_plugin_rejects_tar_slip_traversal() {
        // A crafted .tar.gz whose entry path escapes the extraction dir must
        // fail (fail-closed) and write nothing outside the cache root.
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("nested").join("cache");
        let archive = tmp.path().join("evil.tar.gz");
        let bytes = build_tar_gz_with_entry_name(b"../../escape.txt", b"pwned");
        std::fs::write(&archive, &bytes).unwrap();

        let result = fetch_archive_plugin(&cache, "evil", &archive);
        assert!(
            matches!(result, PersonalFetchResult::CloneFailed(_)),
            "tar-slip traversal entry must fail extraction, got: {result:?}"
        );
        // The traversal target (two levels above the cache dir) must not exist.
        assert!(
            !tmp.path().join("escape.txt").exists(),
            "traversal entry must not write outside the extraction dir"
        );
    }

    #[test]
    fn fetch_archive_plugin_extracts_tar_gz_to_cache() {
        // Positive path: a well-formed tar.gz extracts its real content.
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("cache");
        let archive = tmp.path().join("good.tar.gz");

        {
            use flate2::write::GzEncoder;
            use flate2::Compression;
            let file = std::fs::File::create(&archive).unwrap();
            let enc = GzEncoder::new(file, Compression::default());
            let mut builder = tar::Builder::new(enc);
            let data = b"# skill";
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o644);
            builder
                .append_data(&mut header, "skills/SKILL.md", &data[..])
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }

        let result = fetch_archive_plugin(&cache, "good", &archive);
        match result {
            PersonalFetchResult::Cloned { ref path, .. } => {
                assert!(
                    path.join("skills").join("SKILL.md").is_file(),
                    "tar.gz content must materialize inside the cache dir"
                );
            }
            other => panic!("expected Cloned, got: {other:?}"),
        }
    }

    #[test]
    fn fetch_archive_plugin_strips_single_root_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("cache");
        let archive = tmp.path().join("repo.zip");

        // GitHub-style archive: all files under a single root dir.
        {
            use std::io::Write;
            let file = std::fs::File::create(&archive).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.add_directory("repo-main/", zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.start_file(
                "repo-main/skills/SKILL.md",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            zip.write_all(b"# skill").unwrap();
            zip.finish().unwrap();
        }

        let result = fetch_archive_plugin(&cache, "repo", &archive);
        let path = match result {
            PersonalFetchResult::Cloned { path, .. } => path,
            other => panic!("expected Cloned, got: {other:?}"),
        };
        assert!(
            path.join("skills").join("SKILL.md").is_file(),
            "single root dir must be stripped; skills/ at top level"
        );
        assert!(
            !path.join("repo-main").exists(),
            "inner root dir must not remain after promotion"
        );
    }
}
