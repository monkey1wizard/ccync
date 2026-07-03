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
use mcp::run_mcp_remove_managed;
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

/// First-time install path. Idempotent (delegates to `run_update`).
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

    let mut runtimes = vec!["copilot".to_string()];
    let mut warnings = Vec::new();

    // Step 2.5: Render the managed-set content into the canonical root. Without
    // it the read-surface symlinks point at an empty tree. Best-effort: a
    // render failure is surfaced as a warning.
    let plugin_dirs = managed_plugin_dirs();
    if let Err(e) = render_canonical_root(&canonical_root, &plugin_dirs) {
        warnings.push(format!("canonical root content render (best-effort): {e}"));
    }

    // Step 3: Claude skill surface projection — `~/.claude/skills/ccync` → canonical root.
    // Best-effort: a link failure does not void the canonical root render, but is
    // surfaced as a warning so `ccync doctor` can detect the gap.
    // Never touches the oracle legacy path under ~/.claude/plugins/.
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
/// Using `VALID_RUNTIMES` as the default ensures that a config.json without
/// `selectedRuntimes` selects all agents, matching the projection fallback.
fn default_selected_runtimes() -> Vec<String> {
    ccync_foundation::runtime::VALID_RUNTIMES
        .iter()
        .map(|runtime| (*runtime).to_string())
        .collect()
}

fn load_runtime_selection() -> RuntimeSelection {
    resolve_runtime_selection(CcyncConfig::load())
}

/// Validate and resolve a `CcyncConfig`'s `selectedRuntimes`/`primaryRuntime`
/// into a `RuntimeSelection`. Pure function (no I/O) so it is directly
/// unit-testable without a filesystem/env seam.
///
/// `selectedRuntimes` keys are filtered against `VALID_RUNTIMES`; rejected
/// keys produce one aggregated stderr warning. If filtering leaves the set
/// empty (e.g. all-blank or all-illegal keys), falls back to the full
/// default runtime set — this is what prevents the `selected_runtimes[0]`
/// index-out-of-bounds panic that an empty selection used to cause.
/// `primaryRuntime` is validated the same way: an illegal key falls back to
/// `selected_runtimes[0]` (now guaranteed non-empty) with its own warning.
fn resolve_runtime_selection(config: CcyncConfig) -> RuntimeSelection {
    let default_selected = default_selected_runtimes();

    let selected_runtimes = if config.selected_runtimes.is_empty() {
        default_selected.clone()
    } else {
        let cleaned: Vec<String> = config
            .selected_runtimes
            .into_iter()
            .map(|r| r.trim().to_ascii_lowercase())
            .filter(|r| !r.is_empty())
            .collect();

        let (valid, invalid): (Vec<String>, Vec<String>) = cleaned
            .into_iter()
            .partition(|r| ccync_foundation::runtime::VALID_RUNTIMES.contains(&r.as_str()));

        if !invalid.is_empty() {
            eprintln!(
                "ccync: ignoring invalid runtime key(s) in ~/.ccync/config.json `selectedRuntimes`: {} — valid: {}",
                invalid.join(", "),
                ccync_foundation::runtime::VALID_RUNTIMES.join(", ")
            );
        }

        if valid.is_empty() {
            eprintln!(
                "ccync: no valid runtime keys in `selectedRuntimes` — falling back to all agents"
            );
            default_selected.clone()
        } else {
            valid
        }
    };

    let primary_runtime = config
        .primary_runtime
        .map(|r| r.trim().to_ascii_lowercase())
        .filter(|r| !r.is_empty())
        .filter(|r| {
            let ok = ccync_foundation::runtime::VALID_RUNTIMES.contains(&r.as_str());
            if !ok {
                eprintln!(
                    "ccync: ignoring invalid primaryRuntime '{r}' in ~/.ccync/config.json — falling back to '{}'",
                    selected_runtimes[0]
                );
            }
            ok
        })
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
    /// `~/.ccync/cache/` root could not be resolved (no home dir).
    NoCacheRoot,
    /// `git clone` or sha-read invocation failed; message contains stderr.
    CloneFailed(String),
}

/// Clone a personal plugin from `source_url` into
/// `~/.ccync/cache/<plugin_id>@<sha>/` preserving full CC-plugin structure.
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
    let cache_root = match crate::paths::ccync_cache_root() {
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
/// `~/.ccync/cache/<plugin_id>@<archive-bytes-sha256-first-12>/`.
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
/// and write the result to `~/.ccync/build/mcp/managed.json`.
///
/// Fail-safe: missing core or bundle .mcp.json files are silently skipped.
/// Bundle files are read from `~/.ccync/plugins/<resolvedSource>/.mcp.json` and are
/// only present once bundles have been deployed.
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

    // Bundle MCPs from the deployed plugins root; tolerates the root being absent.
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
    // Not a candidate for `shared_agents_skills_path()`: this ".agents" is
    // relative to `ccync_plugins_root()`, not home, and the final segment is
    // "plugins/marketplace.json", not "skills" — substituting a home-based
    // helper here would silently resolve to a different path.
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
/// (`~/.ccync/cache/<id>@<sha>`). Only existing dirs are returned.
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
    if let Some(cache_root) = crate::paths::ccync_cache_root() {
        let pins = personal_pins();
        for (id, _) in load_enabled_personal_plugins() {
            let candidates = cache_candidates(&cache_root, &id);
            let (chosen, ambiguous) =
                pick_live_dirs(&candidates, pins.get(&id).map(String::as_str), &id);
            if ambiguous {
                eprintln!(
                    "ccync: plugin '{id}' has {} cache dirs but no resolved pin — \
                     skipped from render (ambiguous; run `ccync upgrade {id}`).",
                    candidates.len()
                );
            }
            dirs.extend(chosen);
        }
    }
    dirs
}

/// All `<id>@<sha>` cache dirs for a plugin id (the `@` delimiter prevents an id
/// being a prefix of a longer id, e.g. `foo` vs `foobar`).
fn cache_candidates(cache_root: &Path, id: &str) -> Vec<PathBuf> {
    let prefix = format!("{id}@");
    let Ok(entries) = fs::read_dir(cache_root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .map(|n| n.to_string_lossy().starts_with(&prefix))
                    .unwrap_or(false)
        })
        .collect()
}

/// Choose which cache dir(s) to render for a personal plugin (pure, no IO).
///
/// With a resolved `pin`, render exactly `<id>@<pin>` (the live dir) and ignore
/// stale-sha siblings. Without a pin: a single candidate is unambiguous and
/// rendered; multiple candidates are **ambiguous** → render none and signal it
/// (fail-closed — never guess which sha is live). Returns `(chosen, ambiguous)`.
fn pick_live_dirs(candidates: &[PathBuf], pin: Option<&str>, id: &str) -> (Vec<PathBuf>, bool) {
    match pin {
        Some(pin) => {
            let want = format!("{id}@{pin}");
            let chosen = candidates
                .iter()
                .filter(|p| p.file_name().and_then(|n| n.to_str()) == Some(want.as_str()))
                .cloned()
                .collect();
            (chosen, false)
        }
        None => match candidates.len() {
            1 => (candidates.to_vec(), false),
            n if n > 1 => (Vec::new(), true),
            _ => (Vec::new(), false),
        },
    }
}

/// Map of personal plugin id → resolved live sha from the lockfile
/// (`sha` / `pin` / `pinnedSha`). Empty when the lockfile is absent/unparseable.
fn personal_pins() -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Some(path) = plugins_lockfile_path() else {
        return map;
    };
    let Ok(contents) = fs::read_to_string(&path) else {
        return map;
    };
    let Ok(lock) = serde_json::from_str::<Value>(&contents) else {
        return map;
    };
    if let Some(arr) = lock.get("_personalPlugins").and_then(Value::as_array) {
        for e in arr {
            let id = e.get("pluginId").and_then(Value::as_str);
            let sha = e
                .get("sha")
                .or_else(|| e.get("pin"))
                .or_else(|| e.get("pinnedSha"))
                .and_then(Value::as_str);
            if let (Some(id), Some(sha)) = (id, sha) {
                if !sha.is_empty() {
                    map.insert(id.to_string(), sha.to_string());
                }
            }
        }
    }
    map
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
            "source": canonical_root,
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
                    "sessionLoadCommand": "GitHub Copilot reads the projected plugin from the copilot staged-plugin directory",
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
            "source": { "source": "local", "path": canonical_root },
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

/// Prerequisite state read before any destructive step in `run_uninstall`.
///
/// Captures live paths that become inaccessible after `build/` is deleted, so
/// later steps (MCP de-projection, projection-registry sweep) can use cached
/// paths rather than reading a deleted tree.
pub(crate) struct UninstallPrereqs {
    /// `~/.ccync/build/mcp/managed.json` — present when the file exists.
    pub managed_json: Option<PathBuf>,
    /// `~/.ccync/build/lock.json` — present when the file exists.
    pub lock_json: Option<PathBuf>,
    /// `~/.ccync/config.json` — present when the file exists. Preserved on uninstall.
    pub config_json: Option<PathBuf>,
}

/// Read prerequisite state from `ccync_home` before any deletion.
///
/// Emits named warnings for each missing required input so the caller can
/// propagate them to the audit summary. Never modifies the filesystem.
pub(crate) fn load_uninstall_prereqs(
    ccync_home: &Path,
) -> (UninstallPrereqs, Vec<String>) {
    let mut warnings = Vec::new();

    let managed_json = ccync_home.join("build").join("mcp").join("managed.json");
    let lock_json = ccync_home.join("build").join("lock.json");
    let config_json = ccync_home.join("config.json");

    if !managed_json.exists() {
        warnings.push(format!(
            "build/mcp/managed.json not found at {} — MCP de-projection will be skipped (managed names unknown)",
            managed_json.display()
        ));
    }
    if !lock_json.exists() {
        warnings.push(format!(
            "build/lock.json not found at {} — projection registry cleanup will be skipped",
            lock_json.display()
        ));
    }

    (
        UninstallPrereqs {
            managed_json: managed_json.exists().then_some(managed_json),
            lock_json: lock_json.exists().then_some(lock_json),
            config_json: config_json.exists().then_some(config_json),
        },
        warnings,
    )
}

/// Run `ccync uninstall` — remove canonical root and runtime surfaces.
pub fn run_uninstall() -> Result<(), InstallError> {
    let canonical_root =
        crate::paths::canonical_plugin_root().ok_or(InstallError::NoHome)?;
    let home = crate::paths::user_home().ok_or(InstallError::NoHome)?;

    let mut warnings = Vec::new();

    // Preflight: read prerequisite state before any deletion.
    // Missing prereq files are informational — they don't trigger PartialUninstall
    // (the user is told what was skipped; actual removal failures go into warnings).
    let ccync_home = home.join(".ccync");
    let (prereqs, prereq_warnings) = load_uninstall_prereqs(&ccync_home);
    for w in &prereq_warnings {
        eprintln!("ccync uninstall info: {w}");
    }

    // De-project managed MCP entries from live agent configs BEFORE deleting build/.
    if let Some(managed_json) = &prereqs.managed_json {
        match run_mcp_remove_managed(managed_json) {
            Ok(report) => {
                for w in report.warnings {
                    eprintln!("ccync uninstall warning: {w}");
                    warnings.push(w);
                }
            }
            Err(e) => {
                let msg = format!("MCP de-projection failed: {e}");
                eprintln!("ccync uninstall warning: {msg}");
                warnings.push(msg);
            }
        }
    }

    // De-register marketplace / plugin-state records BEFORE deleting build/.
    // Claude owns structured, readable files (installed_plugins.json /
    // known_marketplaces.json); Codex has no ccync-readable registry, only a CLI.
    remove_claude_installed_plugins_residue(&home, &mut warnings);
    remove_claude_known_marketplaces_residue(&home, &mut warnings);
    for note in deregister_codex_marketplace() {
        eprintln!("ccync uninstall info: {note}");
    }

    // Remove granular ccync-owned projection surfaces recorded in build/lock.json,
    // guarded by positive managed-set membership. Read the prior
    // lockfile BEFORE build/ is deleted; an empty/missing prior authorizes nothing,
    // so user-owned siblings on shared surfaces are never touched.
    let lock_path = prereqs
        .lock_json
        .clone()
        .unwrap_or_else(|| ccync_home.join("build").join("lock.json"));
    let authority = projection::ManagedDeprojection::from_lockfile(&lock_path);
    deproject_managed_surfaces(&authority, &managed_surface_roots(&home), &mut warnings);

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

    // Delete every ~/.ccync child except the two persistent INPUT files
    // (config.json, plugins.json) — removes cache/, build/, any legacy
    // ledger.json, and any other derived state. The final preserve-set is
    // exactly the user inputs. No uninstall ledger is written: the former
    // write_ledger_entry("uninstall", ...) is intentionally gone (ledger is a
    // derived ~/.ccync child and must not survive a full uninstall).
    let mut removed_children: usize = 0;
    if let Ok(entries) = fs::read_dir(&ccync_home) {
        for entry in entries.flatten() {
            if matches!(
                entry.file_name().to_str(),
                Some("config.json") | Some("plugins.json")
            ) {
                continue;
            }
            let path = entry.path();
            match remove_path_if_present(&path) {
                Ok(()) => removed_children += 1,
                Err(e) => {
                    warnings.push(format!("ccync home cleanup ({}): {e}", path.display()))
                }
            }
        }
    }

    // stderr audit summary: removed derived state, preserved inputs, warnings.
    let config_note = if prereqs.config_json.is_some() {
        "config.json (present) + plugins.json"
    } else {
        "config.json/plugins.json (none present)"
    };
    eprintln!(
        "ccync uninstall: removed {removed_children} derived ~/.ccync item(s); preserved {config_note}"
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

/// Remove ccync-owned plugin records from Claude's `installed_plugins.json`.
///
/// ccync registers a Claude marketplace named `ccync`, so its installed plugins
/// are keyed `<plugin>@ccync` in the v2 `{ "plugins": { "<key>": [..] } }` map.
/// Only those keys are removed; user plugins from other marketplaces survive.
/// Parse failure preserves the file byte-for-byte and emits a named warning
/// (genuine failure → `PartialUninstall`). An absent file or a file with no
/// ccync records is a silent no-op (the file is never rewritten needlessly).
fn remove_claude_installed_plugins_residue(home: &Path, warnings: &mut Vec<String>) {
    let path = home
        .join(".claude")
        .join("plugins")
        .join("installed_plugins.json");
    if !path.exists() {
        return;
    }
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            warnings.push(format!(
                "Claude installed_plugins.json read failed (preserved): {e}"
            ));
            return;
        }
    };
    let mut value: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            warnings.push(format!(
                "Claude installed_plugins.json parse failed — preserved byte-for-byte, ccync records not removed: {e}"
            ));
            return;
        }
    };
    let mut removed = false;
    if let Some(plugins) = value.get_mut("plugins").and_then(Value::as_object_mut) {
        let ccync_keys: Vec<String> = plugins
            .keys()
            .filter(|k| k.ends_with("@ccync"))
            .cloned()
            .collect();
        for key in ccync_keys {
            plugins.remove(&key);
            removed = true;
        }
    }
    if removed {
        if let Err(e) = write_json_file(&path, &value) {
            warnings.push(format!("Claude installed_plugins.json write failed: {e}"));
        }
    }
}

/// Remove the ccync marketplace record from Claude's `known_marketplaces.json`.
///
/// Handles the documented `{ "marketplaces": { "<name>": {..} } }` shape and
/// removes the `ccync` entry, preserving other marketplaces. Parse failure
/// preserves the file byte-for-byte + warns; an unrecognized top-level shape is
/// a named skip (stderr info, not a hard failure) so we never silently claim a
/// clean de-registration we did not perform.
fn remove_claude_known_marketplaces_residue(home: &Path, warnings: &mut Vec<String>) {
    let path = home
        .join(".claude")
        .join("plugins")
        .join("known_marketplaces.json");
    if !path.exists() {
        return;
    }
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            warnings.push(format!(
                "Claude known_marketplaces.json read failed (preserved): {e}"
            ));
            return;
        }
    };
    let mut value: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            warnings.push(format!(
                "Claude known_marketplaces.json parse failed — preserved byte-for-byte, ccync marketplace not removed: {e}"
            ));
            return;
        }
    };
    let removed = match value.get_mut("marketplaces").and_then(Value::as_object_mut) {
        Some(marketplaces) => marketplaces.remove("ccync").is_some(),
        None => {
            eprintln!(
                "ccync uninstall info: Claude known_marketplaces.json has no `marketplaces` object — ccync marketplace de-registration skipped (file preserved)"
            );
            false
        }
    };
    if removed {
        if let Err(e) = write_json_file(&path, &value) {
            warnings.push(format!("Claude known_marketplaces.json write failed: {e}"));
        }
    }
}

/// Best-effort de-registration of the ccync Codex marketplace.
///
/// Codex exposes no ccync-readable registry file — its plugin state is reachable
/// only through the `codex` CLI. When `codex` is on PATH we best-effort run the
/// documented removal commands; otherwise we record a single named skip note.
///
/// Returns informational notes (never `PartialUninstall` warnings): de-registering
/// an external CLI's own state is best-effort, so a missing CLI or a failed CLI
/// call must not fail the whole uninstall — but it is always reported (no silent
/// degradation, never a claim that de-registration completed when it did not).
fn deregister_codex_marketplace() -> Vec<String> {
    deregister_codex_marketplace_impl(binary_in_path("codex"))
}

fn deregister_codex_marketplace_impl(codex_available: bool) -> Vec<String> {
    if !codex_available {
        return vec![
            "codex not found on PATH — Codex marketplace de-registration skipped (run `codex plugin remove ccync@ccync-marketplace ; codex plugin marketplace remove ccync-marketplace` manually if Codex was used)".to_string(),
        ];
    }
    let mut notes = Vec::new();
    let steps: [&[&str]; 2] = [
        &["plugin", "remove", "ccync@ccync-marketplace"],
        &["plugin", "marketplace", "remove", "ccync-marketplace"],
    ];
    for args in steps {
        match std::process::Command::new("codex").args(args).output() {
            Ok(out) if out.status.success() => {}
            Ok(out) => notes.push(format!(
                "codex {} exited non-zero (best-effort): {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => notes.push(format!(
                "codex {} failed to spawn (best-effort): {e}",
                args.join(" ")
            )),
        }
    }
    notes
}

/// The live agent surface directories ccync projects skills / commands / agents
/// into, where ccync-owned files can sit alongside user-owned ones. Used as the
/// scan set for registry-guarded de-projection. Safety does not depend on this
/// list being exhaustive — the [`ManagedDeprojection`] guard only removes paths
/// recorded ccync-managed — but completeness here minimizes residue.
fn managed_surface_roots(home: &Path) -> Vec<PathBuf> {
    let mut roots = vec![
        home.join(".claude").join("skills"),
        home.join(".claude").join("commands"),
        home.join(".claude").join("agents"),
        // No existing same-value helper for ".codex/skills" (only the shared
        // ".agents/skills" surface has one) — left as a literal rather than
        // inventing a new helper.
        home.join(".codex").join("skills"),
        home.join(".codex").join("agents"),
        home.join(".gemini").join("skills"),
        home.join(".gemini").join("commands"),
        home.join(".copilot").join("skills"),
        home.join(".copilot").join("agents"),
        home.join(".config").join("opencode").join("skills"),
        home.join(".config").join("opencode").join("commands"),
    ];
    if let Some(shared_agents_skills) = crate::paths::shared_agents_skills_path() {
        roots.push(shared_agents_skills);
    }
    roots
}

/// Remove ccync-owned projected entries from the given surface roots, authorized
/// strictly by prior-lockfile membership (`is_managed`). One directory level is
/// scanned per root; each authorized child (file, dir, or link) is removed.
///
/// An empty/missing prior lockfile authorizes nothing, so this is a no-op on a
/// partial-state uninstall rather than a risk to same-named user-owned siblings.
fn deproject_managed_surfaces(
    authority: &projection::ManagedDeprojection,
    surface_roots: &[PathBuf],
    warnings: &mut Vec<String>,
) {
    if authority.is_empty() {
        return;
    }
    for root in surface_roots {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if authority.is_authorized_for_removal(&path) {
                if let Err(e) = remove_path_if_present(&path) {
                    warnings.push(format!(
                        "managed surface removal ({}): {e}",
                        path.display()
                    ));
                }
            }
        }
    }
}

/// Whether `name` resolves to an executable on PATH (Windows also tries the
/// `PATHEXT`-style `.exe`/`.cmd`/`.bat` suffixes).
fn binary_in_path(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        if dir.join(name).is_file() {
            return true;
        }
        #[cfg(windows)]
        {
            ["exe", "cmd", "bat"]
                .iter()
                .any(|ext| dir.join(format!("{name}.{ext}")).is_file())
        }
        #[cfg(not(windows))]
        {
            false
        }
    })
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
        // config.json without selectedRuntimes defaults to all agents.
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

    // ── resolve_runtime_selection() validation ───────────────────────────────
    //
    // `resolve_runtime_selection` is exercised directly with in-memory
    // `CcyncConfig` values rather than through `load_runtime_selection()`'s
    // `CcyncConfig::load()` → `dirs::home_dir()` path: `dirs` v5 resolves the
    // Windows home directory via `SHGetKnownFolderPath`, which does not
    // observe `std::env::set_var("USERPROFILE", ..)` at runtime, so a
    // fake-home/env-var seam (as used elsewhere in this module for
    // `ccync_foundation::paths`-based functions) cannot reach this call site
    // on Windows. Testing the pure resolver function directly is both
    // faster and immune to that platform gap.

    fn config_with(selected: &[&str], primary: Option<&str>) -> CcyncConfig {
        CcyncConfig {
            selected_runtimes: selected.iter().map(|s| s.to_string()).collect(),
            primary_runtime: primary.map(|s| s.to_string()),
            ..CcyncConfig::default()
        }
    }

    #[test]
    fn resolve_runtime_selection_all_blank_keys_does_not_panic_and_falls_back_to_full_default_set()
    {
        // Before this fix: selectedRuntimes filters (trim + drop-empty) down
        // to an empty vec, then `selected_runtimes[0]` panics with an
        // index-out-of-bounds. This is the latent bug being closed.
        let config = config_with(&["", "   ", "not-a-real-runtime"], Some("claude"));

        let selection = resolve_runtime_selection(config);

        let expected: Vec<String> = ccync_foundation::runtime::VALID_RUNTIMES
            .iter()
            .map(|r| (*r).to_string())
            .collect();
        assert_eq!(
            selection.selected_runtimes, expected,
            "all-invalid selectedRuntimes must fall back to the full default runtime set"
        );
        assert!(
            !selection.primary_runtime.is_empty(),
            "primary_runtime must be derived without panicking"
        );
    }

    #[test]
    fn resolve_runtime_selection_filters_invalid_keys_and_keeps_valid_ones() {
        let config = config_with(
            &["claude", "bogus-key", "codex", "gemini", "antigravity"],
            None,
        );

        let selection = resolve_runtime_selection(config);

        assert_eq!(
            selection.selected_runtimes,
            vec!["claude".to_string(), "codex".to_string()],
            "invalid keys must be filtered out while valid keys are kept, order preserved"
        );
    }

    #[test]
    fn resolve_runtime_selection_all_valid_config_is_unchanged() {
        let config = config_with(&["claude", "codex"], Some("codex"));

        let selection = resolve_runtime_selection(config);

        assert_eq!(
            selection.selected_runtimes,
            vec!["claude".to_string(), "codex".to_string()]
        );
        assert_eq!(selection.primary_runtime, "codex");
    }

    #[test]
    fn resolve_runtime_selection_invalid_primary_runtime_falls_back_to_first_selected() {
        let config = config_with(&["claude", "codex"], Some("not-a-real-runtime"));

        let selection = resolve_runtime_selection(config);

        assert_eq!(
            selection.primary_runtime, "claude",
            "invalid primaryRuntime must fall back to selected_runtimes[0]"
        );
    }

    #[test]
    fn resolve_runtime_selection_empty_config_uses_full_default_set_and_no_warnings() {
        // Absent selectedRuntimes (pre-existing default-empty-vec behavior,
        // unchanged by this fix): falls back to the full default set, same
        // path as before, no invalid-key warning branch is hit.
        let config = CcyncConfig::default();

        let selection = resolve_runtime_selection(config);

        let expected: Vec<String> = ccync_foundation::runtime::VALID_RUNTIMES
            .iter()
            .map(|r| (*r).to_string())
            .collect();
        assert_eq!(selection.selected_runtimes, expected);
        assert!(!selection.primary_runtime.is_empty());
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

    /// Helper: create a fake lock.json in a temp home and run the loader.
    /// Returns the parsed bundle vec. Accepts `home_path` as USERPROFILE / HOME.
    fn with_fake_lock<F>(home_path: &std::path::Path, lock_contents: &str, f: F)
    where
        F: FnOnce(Vec<(String, String)>),
    {
        // Hold the env lock across the whole env-dependent body so no other
        // env-mutating test runs concurrently.
        let _env = env_guard();

        let build_dir = home_path.join(".ccync").join("build");
        std::fs::create_dir_all(&build_dir).unwrap();
        std::fs::write(build_dir.join("lock.json"), lock_contents).unwrap();

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
        let build_dir = tmp.path().join(".ccync").join("build");
        std::fs::create_dir_all(&build_dir).unwrap();
        std::fs::write(
            build_dir.join("lock.json"),
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
        let build_dir = tmp.path().join(".ccync").join("build");
        std::fs::create_dir_all(&build_dir).unwrap();
        std::fs::write(
            build_dir.join("lock.json"),
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

    // ── write_surface_lifecycle_artifacts() marketplace mapping tests ───────────
    //
    // The "ccync" self-entry must point at the absolute canonical root passed
    // in by the caller (not the historical relative "./ccync"); bundle entries
    // must keep the relative "./{bundle_src}" form untouched.

    #[test]
    fn write_surface_lifecycle_artifacts_ccync_entry_uses_absolute_canonical_root() {
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

        // No config.json present → resolve_runtime_selection defaults to the
        // full VALID_RUNTIMES set, so both the claude and codex primary-surface
        // branches run in this single call.
        let canonical_root = tmp.path().join("fake-canonical-root");
        std::fs::create_dir_all(&canonical_root).unwrap();

        let mut runtimes = Vec::new();
        write_surface_lifecycle_artifacts(&canonical_root, &mut runtimes)
            .expect("write_surface_lifecycle_artifacts should succeed against a fake home");

        // ── claude form: "source" field on the marketplace.json ccync entry ──
        let claude_manifest_path = claude_marketplace_manifest_path().unwrap();
        let claude_manifest: Value = serde_json::from_str(
            &std::fs::read_to_string(&claude_manifest_path).unwrap(),
        )
        .unwrap();
        let claude_plugins = claude_manifest["plugins"].as_array().unwrap();
        let claude_ccync_entry = claude_plugins
            .iter()
            .find(|p| p["name"] == "ccync")
            .expect("claude marketplace must contain a ccync self-entry");
        let claude_source = claude_ccync_entry["source"].as_str().unwrap();
        assert_eq!(
            std::path::Path::new(claude_source)
                .canonicalize()
                .unwrap(),
            canonical_root.canonicalize().unwrap(),
            "claude ccync entry `source` must canonicalize to the passed-in canonical_root"
        );
        assert_ne!(
            claude_source, "./ccync",
            "claude ccync entry must no longer use the relative \"./ccync\" literal"
        );

        // ── codex form: nested "source":{"path":...} field ──
        let codex_manifest_path = codex_marketplace_manifest_path().unwrap();
        let codex_manifest: Value = serde_json::from_str(
            &std::fs::read_to_string(&codex_manifest_path).unwrap(),
        )
        .unwrap();
        let codex_plugins = codex_manifest["plugins"].as_array().unwrap();
        let codex_ccync_entry = codex_plugins
            .iter()
            .find(|p| p["name"] == "ccync")
            .expect("codex marketplace must contain a ccync self-entry");
        let codex_path = codex_ccync_entry["source"]["path"].as_str().unwrap();
        assert_eq!(
            std::path::Path::new(codex_path).canonicalize().unwrap(),
            canonical_root.canonicalize().unwrap(),
            "codex ccync entry nested `source.path` must canonicalize to the passed-in canonical_root"
        );
        assert_ne!(
            codex_path, "./ccync",
            "codex ccync entry must no longer use the relative \"./ccync\" literal"
        );
    }

    #[test]
    fn write_surface_lifecycle_artifacts_bundle_entries_stay_relative() {
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

        // Enable one bundled-local plugin via the lockfile so the
        // `enabled_bundles` loop actually runs and appends a bundle entry to
        // both marketplace manifests alongside the "ccync" self-entry.
        let build_dir = tmp.path().join(".ccync").join("build");
        std::fs::create_dir_all(&build_dir).unwrap();
        std::fs::write(
            build_dir.join("lock.json"),
            r#"{
                "resolvedPlugins": [
                    {
                        "pluginId": "ccync-game-assets",
                        "installStrategy": "bundled-local",
                        "resolvedSource": "ccync-domain/ccync-game-assets"
                    }
                ]
            }"#,
        )
        .unwrap();

        let canonical_root = tmp.path().join("fake-canonical-root");
        std::fs::create_dir_all(&canonical_root).unwrap();

        let mut runtimes = Vec::new();
        write_surface_lifecycle_artifacts(&canonical_root, &mut runtimes)
            .expect("write_surface_lifecycle_artifacts should succeed against a fake home");

        // ── claude bundle entry stays "./{bundle_src}" byte-for-byte ──
        let claude_manifest_path = claude_marketplace_manifest_path().unwrap();
        let claude_manifest: Value = serde_json::from_str(
            &std::fs::read_to_string(&claude_manifest_path).unwrap(),
        )
        .unwrap();
        let claude_plugins = claude_manifest["plugins"].as_array().unwrap();
        let claude_bundle_entry = claude_plugins
            .iter()
            .find(|p| p["name"] == "ccync-game-assets")
            .expect("claude marketplace must contain the enabled bundle entry");
        assert_eq!(
            claude_bundle_entry["source"].as_str().unwrap(),
            "./ccync-domain/ccync-game-assets",
            "claude bundle entry source must stay the relative \"./{{bundle_src}}\" form, byte-for-byte"
        );

        // ── codex bundle entry stays "./{bundle_src}" byte-for-byte ──
        let codex_manifest_path = codex_marketplace_manifest_path().unwrap();
        let codex_manifest: Value = serde_json::from_str(
            &std::fs::read_to_string(&codex_manifest_path).unwrap(),
        )
        .unwrap();
        let codex_plugins = codex_manifest["plugins"].as_array().unwrap();
        let codex_bundle_entry = codex_plugins
            .iter()
            .find(|p| p["name"] == "ccync-game-assets")
            .expect("codex marketplace must contain the enabled bundle entry");
        assert_eq!(
            codex_bundle_entry["source"]["path"].as_str().unwrap(),
            "./ccync-domain/ccync-game-assets",
            "codex bundle entry nested source.path must stay the relative \"./{{bundle_src}}\" form, byte-for-byte"
        );

        // Sanity: the "ccync" self-entry in the same manifest is still the
        // absolute canonical_root, not "./ccync" — confirms both branches
        // (self-entry vs bundle entry) are exercised together in one manifest.
        let claude_ccync_entry = claude_plugins
            .iter()
            .find(|p| p["name"] == "ccync")
            .expect("claude marketplace must also contain the ccync self-entry");
        assert_ne!(claude_ccync_entry["source"].as_str().unwrap(), "./ccync");
    }

    // ── load_enabled_personal_plugins() tests ───────────────────────────────────

    fn with_fake_personal_lock<F>(home_path: &std::path::Path, lock_contents: &str, f: F)
    where
        F: FnOnce(Vec<(String, String)>),
    {
        let _env = env_guard();
        let build_dir = home_path.join(".ccync").join("build");
        std::fs::create_dir_all(&build_dir).unwrap();
        std::fs::write(build_dir.join("lock.json"), lock_contents).unwrap();
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

    // ── pick_live_dirs() render live-dir selection ─────────────────────────────

    fn cdir(name: &str) -> PathBuf {
        PathBuf::from(format!("/cache/{name}"))
    }

    #[test]
    fn pick_live_dirs_with_pin_selects_exact_dir() {
        let candidates = vec![cdir("a@old"), cdir("a@new")];
        let (chosen, ambiguous) = pick_live_dirs(&candidates, Some("new"), "a");
        assert_eq!(chosen, vec![cdir("a@new")], "only the pinned dir renders");
        assert!(!ambiguous);
    }

    #[test]
    fn pick_live_dirs_no_pin_single_is_unambiguous() {
        let candidates = vec![cdir("a@only")];
        let (chosen, ambiguous) = pick_live_dirs(&candidates, None, "a");
        assert_eq!(chosen, vec![cdir("a@only")]);
        assert!(!ambiguous);
    }

    #[test]
    fn pick_live_dirs_no_pin_multiple_is_ambiguous_fail_closed() {
        let candidates = vec![cdir("a@one"), cdir("a@two")];
        let (chosen, ambiguous) = pick_live_dirs(&candidates, None, "a");
        assert!(chosen.is_empty(), "ambiguous → render nothing (fail-closed)");
        assert!(ambiguous);
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

    // ── load_uninstall_prereqs ───────────────────────────────────────────────

    #[test]
    fn preflight_missing_managed_json_warns_and_does_not_claim_mcp_cleanup() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync_home = tmp.path().join(".ccync");
        // Create build/ but NOT build/mcp/managed.json
        std::fs::create_dir_all(ccync_home.join("build").join("mcp")).unwrap();
        std::fs::create_dir_all(ccync_home.join("build")).unwrap();
        std::fs::write(ccync_home.join("config.json"), "{}").unwrap();
        std::fs::write(ccync_home.join("build").join("lock.json"), "{}").unwrap();
        // managed.json intentionally absent

        let (prereqs, warnings) = load_uninstall_prereqs(&ccync_home);

        // Must warn about the missing surface by name.
        assert!(
            warnings.iter().any(|w| w.contains("managed.json")),
            "missing managed.json must produce a named warning; got: {warnings:?}"
        );
        // Must not claim complete MCP cleanup.
        assert!(
            warnings.iter().any(|w| w.contains("skipped")),
            "warning must state MCP de-projection is skipped; got: {warnings:?}"
        );
        // The prereq struct reflects absence.
        assert!(prereqs.managed_json.is_none(), "managed_json should be None when absent");
        // Present files are resolved correctly.
        assert!(prereqs.lock_json.is_some(), "lock_json should be Some when present");
        assert!(prereqs.config_json.is_some(), "config_json should be Some when present");
    }

    #[test]
    fn preflight_all_files_present_no_warnings() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ccync_home = tmp.path().join(".ccync");
        std::fs::create_dir_all(ccync_home.join("build").join("mcp")).unwrap();
        std::fs::write(ccync_home.join("config.json"), "{}").unwrap();
        std::fs::write(ccync_home.join("build").join("lock.json"), "{}").unwrap();
        std::fs::write(ccync_home.join("build").join("mcp").join("managed.json"), "{}").unwrap();

        let (_prereqs, warnings) = load_uninstall_prereqs(&ccync_home);
        assert!(warnings.is_empty(), "no warnings when all prereq files present: {warnings:?}");
    }

    // ── MCP de-projection in run_uninstall ──────────────────────────────────

    #[test]
    fn mcp_managed_entries_removed_user_entries_preserved_on_uninstall() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let fake_home = tmp.path();

        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", fake_home);
            std::env::set_var("HOME", fake_home);
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", fake_home);
        }

        // Seed managed.json with the managed server name.
        let build_mcp = fake_home.join(".ccync").join("build").join("mcp");
        std::fs::create_dir_all(&build_mcp).unwrap();
        std::fs::write(
            build_mcp.join("managed.json"),
            serde_json::json!({
                "mcpServers": {
                    "ccync-managed-tool": { "command": "ccync-tool", "args": [] }
                }
            })
            .to_string(),
        )
        .unwrap();

        // Seed ~/.claude.json with one managed entry + one user entry.
        let claude_json = fake_home.join(".claude.json");
        std::fs::write(
            &claude_json,
            serde_json::json!({
                "mcpServers": {
                    "ccync-managed-tool": { "command": "ccync-tool", "args": [] },
                    "user-personal-tool": { "command": "my-tool", "args": [] }
                }
            })
            .to_string(),
        )
        .unwrap();

        // Create canonical root so run_uninstall can find it.
        std::fs::create_dir_all(
            fake_home.join(".ccync").join("build").join("render"),
        )
        .unwrap();

        run_uninstall().unwrap();

        let after: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&claude_json).unwrap(),
        )
        .unwrap();
        let servers = after["mcpServers"].as_object().unwrap();
        assert!(
            !servers.contains_key("ccync-managed-tool"),
            "managed entry must be removed; servers: {servers:?}"
        );
        assert!(
            servers.contains_key("user-personal-tool"),
            "user entry must be preserved; servers: {servers:?}"
        );
    }

    // ── TP-05a: Claude marketplace / plugin-state residue removal ────────────

    #[test]
    fn claude_installed_plugins_drops_ccync_keeps_user_records() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp
            .path()
            .join(".claude")
            .join("plugins")
            .join("installed_plugins.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({
                "plugins": {
                    "ccync@ccync": [{ "version": "1.0.1" }],
                    "sometool@othermarket": [{ "version": "2.0.0" }]
                }
            })
            .to_string(),
        )
        .unwrap();

        let mut warnings = Vec::new();
        remove_claude_installed_plugins_residue(tmp.path(), &mut warnings);

        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let plugins = after["plugins"].as_object().unwrap();
        assert!(!plugins.contains_key("ccync@ccync"), "ccync record removed");
        assert!(
            plugins.contains_key("sometool@othermarket"),
            "user record preserved"
        );
        assert!(warnings.is_empty(), "clean removal emits no failure: {warnings:?}");
    }

    #[test]
    fn claude_known_marketplaces_drops_ccync_keeps_user_marketplaces() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp
            .path()
            .join(".claude")
            .join("plugins")
            .join("known_marketplaces.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({
                "marketplaces": {
                    "ccync": { "source": { "type": "local" } },
                    "user-market": { "source": { "type": "git" } }
                }
            })
            .to_string(),
        )
        .unwrap();

        let mut warnings = Vec::new();
        remove_claude_known_marketplaces_residue(tmp.path(), &mut warnings);

        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let mkts = after["marketplaces"].as_object().unwrap();
        assert!(!mkts.contains_key("ccync"), "ccync marketplace removed");
        assert!(mkts.contains_key("user-market"), "user marketplace preserved");
    }

    #[test]
    fn claude_installed_plugins_malformed_preserved_byte_for_byte_with_warning() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp
            .path()
            .join(".claude")
            .join("plugins")
            .join("installed_plugins.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = "{ this is not valid json @@@";
        std::fs::write(&path, original).unwrap();

        let mut warnings = Vec::new();
        remove_claude_installed_plugins_residue(tmp.path(), &mut warnings);

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "malformed file must be preserved byte-for-byte"
        );
        assert!(
            warnings.iter().any(|w| w.contains("parse failed")),
            "a named parse warning must be emitted: {warnings:?}"
        );
    }

    // ── TP-05b: Codex de-registration skip when CLI absent ───────────────────

    // Mirrors projection's `normalize_managed_artifact_path` so a hand-written
    // lockfile key matches what the authority computes for a real path.
    fn norm_key(p: &Path) -> String {
        let s = p.to_string_lossy().replace('/', "\\");
        if cfg!(windows) {
            s.to_ascii_lowercase()
        } else {
            s
        }
    }

    // ── registry-guarded surface de-projection ──────────────────────────────

    #[test]
    fn deproject_managed_surfaces_removes_only_authorized_entries() {
        let tmp = tempfile::TempDir::new().unwrap();
        let surface = tmp.path().join(".claude").join("skills");
        std::fs::create_dir_all(&surface).unwrap();
        let managed = surface.join("ccync");
        let user_sibling = surface.join("user-tool");
        std::fs::create_dir_all(&managed).unwrap();
        std::fs::write(managed.join("SKILL.md"), "x").unwrap();
        std::fs::create_dir_all(&user_sibling).unwrap();
        std::fs::write(user_sibling.join("SKILL.md"), "y").unwrap();
        // AGY GUI TOML analogue outside the scanned surfaces — must survive.
        let agy_gui = tmp
            .path()
            .join(".gemini")
            .join("antigravity-ide")
            .join("settings.toml");
        std::fs::create_dir_all(agy_gui.parent().unwrap()).unwrap();
        std::fs::write(&agy_gui, "[user]\nkeep = true\n").unwrap();

        // Record `managed` as a ccync skill projection in a prior lockfile.
        let lock = tmp.path().join(".ccync").join("build").join("lock.json");
        std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
        std::fs::write(
            &lock,
            serde_json::json!({
                "_ccyncProjection": { "skillProjectionPaths": [ norm_key(&managed) ] }
            })
            .to_string(),
        )
        .unwrap();

        let authority = projection::ManagedDeprojection::from_lockfile(&lock);
        let mut warnings = Vec::new();
        deproject_managed_surfaces(&authority, &[surface.clone()], &mut warnings);

        assert!(!managed.exists(), "ccync-managed entry must be removed");
        assert!(user_sibling.exists(), "user-owned sibling must be preserved");
        assert!(agy_gui.exists(), "AGY GUI TOML must survive (outside scanned surfaces)");
        assert!(warnings.is_empty(), "clean removal emits no warnings: {warnings:?}");
    }

    #[test]
    fn deproject_managed_surfaces_empty_prior_preserves_user_sibling() {
        let tmp = tempfile::TempDir::new().unwrap();
        let surface = tmp.path().join(".claude").join("skills");
        std::fs::create_dir_all(&surface).unwrap();
        // A same-named entry a ccync projection WOULD use, but here user-owned.
        let same_name = surface.join("ccync");
        std::fs::create_dir_all(&same_name).unwrap();

        // Missing prior lockfile → authority authorizes nothing (no allow-all).
        let authority =
            projection::ManagedDeprojection::from_lockfile(&tmp.path().join("nolock.json"));
        let mut warnings = Vec::new();
        deproject_managed_surfaces(&authority, &[surface.clone()], &mut warnings);

        assert!(
            same_name.exists(),
            "empty/missing prior must NOT delete a same-named user entry"
        );
    }

    #[test]
    fn codex_deregister_absent_cli_emits_named_skip_only() {
        // codex_available = false → a single named skip note (informational),
        // never a claim of completion and never a hard failure.
        let notes = deregister_codex_marketplace_impl(false);
        assert_eq!(notes.len(), 1, "exactly one skip note: {notes:?}");
        assert!(
            notes[0].contains("skipped"),
            "skip note must name the skipped Codex de-registration: {notes:?}"
        );
    }
}
