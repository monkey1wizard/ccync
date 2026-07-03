//! Plugin upgrade support for `ccync upgrade`.
//!
//! Layers: `resolve_upgrades` classifies the personal-plugin set and compares
//! each git plugin's pinned sha against its remote HEAD via an injected
//! `ls_remote` seam; `apply_upgrade` re-clones a changed plugin and re-pins it
//! (pin-after-cache); `run_upgrade` orchestrates filter / dry-run / projection.

use serde_json::Value;

/// Per-plugin classification produced by [`resolve_upgrades`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeOutcome {
    /// A git plugin whose remote HEAD differs from its pinned sha.
    Changed {
        plugin_id: String,
        source: String,
        old_sha: Option<String>,
        new_sha: String,
    },
    /// A git plugin already at the latest remote HEAD.
    Unchanged { plugin_id: String },
    /// A non-git plugin (archive/local): no upstream "latest" concept.
    Skipped { plugin_id: String, reason: String },
    /// Resolving this plugin failed (e.g. `git ls-remote` errored).
    Failed { plugin_id: String, reason: String },
    /// The user pinned (`ccync pin`) this plugin: hold it at its current sha,
    /// never querying the remote or re-cloning.
    Held { plugin_id: String },
}

impl UpgradeOutcome {
    /// The plugin id this outcome refers to.
    pub fn plugin_id(&self) -> &str {
        match self {
            UpgradeOutcome::Changed { plugin_id, .. }
            | UpgradeOutcome::Unchanged { plugin_id }
            | UpgradeOutcome::Skipped { plugin_id, .. }
            | UpgradeOutcome::Failed { plugin_id, .. }
            | UpgradeOutcome::Held { plugin_id } => plugin_id,
        }
    }
}

/// Extract the personal-plugin entry array from a lockfile (`_personalPlugins`)
/// or personal catalog (`plugins`) value, or treat `plugins` as an array itself.
fn personal_entries(plugins: &Value) -> Vec<&Value> {
    if let Some(arr) = plugins.as_array() {
        return arr.iter().collect();
    }
    for key in ["_personalPlugins", "plugins"] {
        if let Some(arr) = plugins.get(key).and_then(Value::as_array) {
            return arr.iter().collect();
        }
    }
    Vec::new()
}

/// True when an entry is a git-cloned plugin (re-resolvable against a remote).
/// Prefers the explicit `installStrategy`, falling back to the source URL shape.
fn is_git_entry(entry: &Value) -> bool {
    if let Some(strategy) = entry.get("installStrategy").and_then(Value::as_str) {
        return strategy == "personal-git-clone";
    }
    match entry.get("source").and_then(Value::as_str) {
        Some(s) => s.ends_with(".git") || s.starts_with("git@") || s.contains("github.com"),
        None => false,
    }
}

/// The currently pinned commit sha for an entry, if recorded.
fn current_pin(entry: &Value) -> Option<String> {
    for key in ["pin", "pinnedSha"] {
        if let Some(sha) = entry.get(key).and_then(Value::as_str) {
            if !sha.is_empty() {
                return Some(sha.to_string());
            }
        }
    }
    None
}

/// Classify every personal plugin for upgrade. Git plugins are compared against
/// their remote HEAD via `ls_remote(source, ref)`; non-git plugins are
/// [`UpgradeOutcome::Skipped`]. Pure aside from the injected `ls_remote` seam.
pub fn resolve_upgrades(
    plugins: &Value,
    ls_remote: impl Fn(&str, &str) -> Result<String, String>,
) -> Vec<UpgradeOutcome> {
    let mut out = Vec::new();
    for entry in personal_entries(plugins) {
        let plugin_id = entry
            .get("pluginId")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>")
            .to_string();

        // Held plugins are frozen by the user: never query the remote.
        if entry.get("held").and_then(Value::as_bool).unwrap_or(false) {
            out.push(UpgradeOutcome::Held { plugin_id });
            continue;
        }

        if !is_git_entry(entry) {
            let reason = entry
                .get("installStrategy")
                .and_then(Value::as_str)
                .map(|s| format!("non-git source ({s})"))
                .unwrap_or_else(|| "non-git source".to_string());
            out.push(UpgradeOutcome::Skipped { plugin_id, reason });
            continue;
        }

        let source = match entry.get("source").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => {
                out.push(UpgradeOutcome::Failed {
                    plugin_id,
                    reason: "git entry has no source url".to_string(),
                });
                continue;
            }
        };

        let remote = match ls_remote(&source, "HEAD") {
            Ok(sha) => sha,
            Err(e) => {
                out.push(UpgradeOutcome::Failed {
                    plugin_id,
                    reason: format!("ls-remote failed: {e}"),
                });
                continue;
            }
        };

        let pin = current_pin(entry);
        if pin.as_deref() == Some(remote.as_str()) {
            out.push(UpgradeOutcome::Unchanged { plugin_id });
        } else {
            out.push(UpgradeOutcome::Changed {
                plugin_id,
                source,
                old_sha: pin,
                new_sha: remote,
            });
        }
    }
    out
}

// ── apply upgrade (re-clone + pin-after-cache) ────────────────────────────────

use std::path::Path;

/// A confirmed-changed git plugin to re-clone and re-pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeChange {
    pub plugin_id: String,
    pub source: String,
    /// The currently pinned sha, if recorded (carried through from
    /// [`UpgradeOutcome::Changed`] so `upgrade --dry-run` /
    /// `list --upgrade-available` can render `current → latest`).
    pub old_sha: Option<String>,
    pub new_sha: String,
}

/// Failure surface for [`apply_upgrade`].
#[derive(Debug)]
pub enum UpgradeError {
    /// The re-clone seam failed.
    Clone(String),
    /// The clone reported success but the cache dir did not materialize.
    CacheVerify(String),
    /// Writing the new pin into a pin file failed.
    PinWrite(String),
    /// No pin file carried an entry for this plugin.
    PinMissing(String),
}

impl std::fmt::Display for UpgradeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpgradeError::Clone(m) => write!(f, "clone failed: {m}"),
            UpgradeError::CacheVerify(m) => write!(f, "cache verify failed: {m}"),
            UpgradeError::PinWrite(m) => write!(f, "pin write failed: {m}"),
            UpgradeError::PinMissing(id) => write!(f, "no pin entry for plugin: {id}"),
        }
    }
}

impl std::error::Error for UpgradeError {}

/// Re-clone a changed plugin into `cache_root/<id>@<new_sha>` and, **only after**
/// the cache is verified present, atomically re-pin it across `pin_files`
/// (`plugins.json` and/or `lock.json`). The clone is injected so the function is
/// unit-testable without the network. Pin-after-cache order is mandatory: a pin
/// is never written before its cache dir exists.
pub fn apply_upgrade(
    change: &UpgradeChange,
    cache_root: &Path,
    pin_files: &[&Path],
    clone: impl Fn(&str, &Path) -> Result<(), String>,
) -> Result<(), UpgradeError> {
    use std::fs;

    // 1. Re-clone into the new pinned cache dir.
    let dest = cache_root.join(format!("{}@{}", change.plugin_id, change.new_sha));
    clone(&change.source, &dest).map_err(UpgradeError::Clone)?;

    // 2. Verify the cache actually landed before touching any pin.
    let landed = dest.is_dir()
        && fs::read_dir(&dest)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
    if !landed {
        return Err(UpgradeError::CacheVerify(format!(
            "cache dir absent or empty: {}",
            dest.display()
        )));
    }

    // 3. Pin-after-cache: update each pin file that carries this plugin.
    let mut any = false;
    for file in pin_files {
        match update_pin_in_file(file, &change.plugin_id, &change.new_sha) {
            Ok(true) => any = true,
            Ok(false) => {}
            Err(e) => return Err(UpgradeError::PinWrite(e)),
        }
    }
    if !any {
        return Err(UpgradeError::PinMissing(change.plugin_id.clone()));
    }
    Ok(())
}

/// Update the pin of `plugin_id` in a JSON file's personal-plugin array, writing
/// atomically (tmp + rename). Returns `Ok(true)` when an entry was updated,
/// `Ok(false)` when the file has no such entry (not an error — the entry may
/// live only in the other pin file). A missing file is treated as "no entry".
fn update_pin_in_file(path: &Path, plugin_id: &str, new_sha: &str) -> Result<bool, String> {
    use std::fs;

    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Ok(false),
    };
    let mut v: Value = serde_json::from_str(&text).map_err(|e| format!("parse {path:?}: {e}"))?;

    let mut updated = false;
    for key in ["_personalPlugins", "plugins"] {
        if let Some(arr) = v.get_mut(key).and_then(Value::as_array_mut) {
            for entry in arr.iter_mut() {
                if entry.get("pluginId").and_then(Value::as_str) == Some(plugin_id) {
                    // Prefer the key already present; default to `pin`.
                    if entry.get("pinnedSha").is_some() {
                        entry["pinnedSha"] = Value::String(new_sha.to_string());
                    } else {
                        entry["pin"] = Value::String(new_sha.to_string());
                    }
                    updated = true;
                }
            }
        }
    }

    if !updated {
        return Ok(false);
    }

    let pretty =
        serde_json::to_string_pretty(&v).map_err(|e| format!("serialize {path:?}: {e}"))?;
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    fs::write(&tmp, pretty).map_err(|e| format!("write tmp {tmp:?}: {e}"))?;
    fs::rename(&tmp, path).map_err(|e| format!("rename {tmp:?}->{path:?}: {e}"))?;
    Ok(true)
}

// ── upgrade orchestrator ──────────────────────────────────────────────────────

use std::path::PathBuf;

/// Inputs for [`run_upgrade`].
#[derive(Debug, Clone)]
pub struct UpgradeOpts {
    /// The personal-plugin set (lockfile or catalog value).
    pub plugins: Value,
    /// `Some(id)` to upgrade a single plugin, `None` for all git plugins.
    pub filter_id: Option<String>,
    /// `--dry-run`: classify only, never clone, re-pin, or project.
    pub dry_run: bool,
    /// Cache root (`~/.ccync/cache`).
    pub cache_root: PathBuf,
    /// Pin files to update (`plugins.json` and/or `lock.json`).
    pub pin_files: Vec<PathBuf>,
}

/// Aggregate result of [`run_upgrade`] — every plugin is accounted for.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct UpgradeReport {
    /// Plugin ids successfully re-cloned + re-pinned.
    pub upgraded: Vec<String>,
    /// Plugin ids already at the latest remote HEAD.
    pub unchanged: Vec<String>,
    /// `(id, reason)` for non-git plugins skipped.
    pub skipped: Vec<(String, String)>,
    /// `(id, reason)` for plugins that errored (resolve or apply).
    pub failed: Vec<(String, String)>,
    /// Plugin ids held (`ccync pin`): skipped, kept at their current sha.
    pub held: Vec<String>,
    /// Pending changes (populated on dry-run; what *would* be upgraded).
    pub pending: Vec<UpgradeChange>,
    /// Whether `run_unified_projection` was invoked.
    pub projected: bool,
    /// Whether this was a dry run (nothing written).
    pub dry_run: bool,
    /// Top-level error (e.g. a `--plugin <id>` that does not exist).
    pub error: Option<String>,
}

/// Orchestrate `ccync upgrade`: resolve → (dry-run diff | apply changed) →
/// project. Network (`ls_remote`), clone, and projection are injected as seams
/// so the control flow is unit-testable. Per-plugin failures are isolated; the
/// projection seam runs once at the end only when a real pin changed.
pub fn run_upgrade(
    opts: &UpgradeOpts,
    ls_remote: impl Fn(&str, &str) -> Result<String, String>,
    clone: impl Fn(&str, &Path) -> Result<(), String>,
    project: impl Fn() -> Result<(), String>,
) -> UpgradeReport {
    let mut report = UpgradeReport {
        dry_run: opts.dry_run,
        ..Default::default()
    };

    let mut outcomes = resolve_upgrades(&opts.plugins, ls_remote);

    if let Some(want) = &opts.filter_id {
        outcomes.retain(|o| o.plugin_id() == want);
        if outcomes.is_empty() {
            report.error = Some(format!("no personal plugin with id '{want}'"));
            return report;
        }
    }

    let pin_files: Vec<&Path> = opts.pin_files.iter().map(PathBuf::as_path).collect();

    for outcome in outcomes {
        match outcome {
            UpgradeOutcome::Unchanged { plugin_id } => report.unchanged.push(plugin_id),
            UpgradeOutcome::Held { plugin_id } => report.held.push(plugin_id),
            UpgradeOutcome::Skipped { plugin_id, reason } => {
                report.skipped.push((plugin_id, reason))
            }
            UpgradeOutcome::Failed { plugin_id, reason } => {
                report.failed.push((plugin_id, reason))
            }
            UpgradeOutcome::Changed {
                plugin_id,
                source,
                old_sha,
                new_sha,
            } => {
                let change = UpgradeChange {
                    plugin_id: plugin_id.clone(),
                    source,
                    old_sha,
                    new_sha,
                };
                if opts.dry_run {
                    report.pending.push(change);
                    continue;
                }
                match apply_upgrade(&change, &opts.cache_root, &pin_files, &clone) {
                    Ok(()) => report.upgraded.push(plugin_id),
                    Err(e) => report.failed.push((plugin_id, e.to_string())),
                }
            }
        }
    }

    if !opts.dry_run && !report.upgraded.is_empty() {
        match project() {
            Ok(()) => report.projected = true,
            Err(e) => report.failed.push(("<projection>".to_string(), e)),
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn git_entry(id: &str, pin: Option<&str>) -> Value {
        let mut e = json!({
            "pluginId": id,
            "installStrategy": "personal-git-clone",
            "source": format!("https://github.com/user/{id}.git"),
        });
        if let Some(p) = pin {
            e["pin"] = json!(p);
        }
        e
    }

    #[test]
    fn changed_when_remote_differs() {
        let plugins = json!({ "_personalPlugins": [git_entry("a", Some("oldsha"))] });
        let out = resolve_upgrades(&plugins, |_, _| Ok("newsha".to_string()));
        assert_eq!(
            out,
            vec![UpgradeOutcome::Changed {
                plugin_id: "a".into(),
                source: "https://github.com/user/a.git".into(),
                old_sha: Some("oldsha".into()),
                new_sha: "newsha".into(),
            }]
        );
    }

    #[test]
    fn held_entry_is_held_even_when_remote_differs() {
        let plugins = json!({ "_personalPlugins": [{
            "pluginId": "a",
            "installStrategy": "personal-git-clone",
            "source": "https://github.com/user/a.git",
            "pin": "old",
            "held": true
        }] });
        // ls_remote returns a *different* sha, but held must short-circuit.
        let out = resolve_upgrades(&plugins, |_, _| Ok("new".to_string()));
        assert_eq!(out, vec![UpgradeOutcome::Held { plugin_id: "a".into() }]);
    }

    #[test]
    fn unchanged_when_remote_matches_pin() {
        let plugins = json!({ "_personalPlugins": [git_entry("a", Some("samesha"))] });
        let out = resolve_upgrades(&plugins, |_, _| Ok("samesha".to_string()));
        assert_eq!(out, vec![UpgradeOutcome::Unchanged { plugin_id: "a".into() }]);
    }

    #[test]
    fn archive_and_local_are_skipped() {
        let plugins = json!({
            "_personalPlugins": [
                { "pluginId": "arch", "installStrategy": "personal-archive", "sourceType": "local", "source": "x.tar.gz", "pinnedSha": "h" },
                { "pluginId": "loc", "installStrategy": "personal-archive", "sourceType": "local", "source": "/path/to/p" }
            ]
        });
        let out = resolve_upgrades(&plugins, |_, _| Ok("never".to_string()));
        assert!(matches!(out[0], UpgradeOutcome::Skipped { .. }));
        assert!(matches!(out[1], UpgradeOutcome::Skipped { .. }));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn ls_remote_failure_is_isolated_failed() {
        let plugins = json!({ "_personalPlugins": [git_entry("a", Some("p")), git_entry("b", Some("q"))] });
        let out = resolve_upgrades(&plugins, |src, _| {
            if src.contains("/a.git") {
                Err("network".to_string())
            } else {
                Ok("q".to_string())
            }
        });
        assert!(matches!(out[0], UpgradeOutcome::Failed { .. }), "a failed");
        assert!(matches!(out[1], UpgradeOutcome::Unchanged { .. }), "b still resolved");
    }

    #[test]
    fn git_entry_without_pin_is_changed() {
        // A git plugin with no recorded pin is always treated as changed.
        let plugins = json!({ "_personalPlugins": [git_entry("a", None)] });
        let out = resolve_upgrades(&plugins, |_, _| Ok("newsha".to_string()));
        match &out[0] {
            UpgradeOutcome::Changed { old_sha, new_sha, .. } => {
                assert_eq!(old_sha, &None);
                assert_eq!(new_sha, "newsha");
            }
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[test]
    fn empty_or_missing_personal_set_yields_nothing() {
        assert!(resolve_upgrades(&json!({}), |_, _| Ok("x".to_string())).is_empty());
        assert!(resolve_upgrades(&json!({ "_personalPlugins": [] }), |_, _| Ok("x".to_string())).is_empty());
    }
}

#[cfg(test)]
mod apply_tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("ccync-apply-{tag}-{:?}", std::thread::current().id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn change(id: &str, new: &str) -> UpgradeChange {
        UpgradeChange {
            plugin_id: id.into(),
            source: format!("https://github.com/user/{id}.git"),
            old_sha: Some("oldsha".into()),
            new_sha: new.into(),
        }
    }

    fn write_lock(dir: &std::path::Path, id: &str, pin: &str) -> std::path::PathBuf {
        let lock = dir.join("lock.json");
        fs::write(
            &lock,
            serde_json::to_string_pretty(&json!({
                "_personalPlugins": [
                    { "pluginId": id, "installStrategy": "personal-git-clone",
                      "source": format!("https://github.com/user/{id}.git"), "pin": pin }
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        lock
    }

    #[test]
    fn clone_then_pin_updates_lock_and_creates_cache() {
        let d = tmp_dir("ok");
        let cache = d.join("cache");
        let lock = write_lock(&d, "a", "oldsha");

        // clone seam: create the cache dir with a file (simulating a real clone)
        let res = apply_upgrade(&change("a", "newsha"), &cache, &[lock.as_path()], |_, dest| {
            fs::create_dir_all(dest).unwrap();
            fs::write(dest.join("plugin.json"), b"{}").unwrap();
            Ok(())
        });
        assert!(res.is_ok(), "got {res:?}");
        assert!(cache.join("a@newsha").is_dir(), "cache dir created");

        let v: Value = serde_json::from_str(&fs::read_to_string(&lock).unwrap()).unwrap();
        assert_eq!(v["_personalPlugins"][0]["pin"], "newsha", "pin re-written");
        // file still valid + retains other fields (no truncate)
        assert_eq!(v["_personalPlugins"][0]["pluginId"], "a");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn clone_failure_leaves_pin_unchanged() {
        let d = tmp_dir("clonefail");
        let cache = d.join("cache");
        let lock = write_lock(&d, "a", "oldsha");

        let res = apply_upgrade(&change("a", "newsha"), &cache, &[lock.as_path()], |_, _| {
            Err("network down".to_string())
        });
        assert!(matches!(res, Err(UpgradeError::Clone(_))));
        let v: Value = serde_json::from_str(&fs::read_to_string(&lock).unwrap()).unwrap();
        assert_eq!(v["_personalPlugins"][0]["pin"], "oldsha", "pin untouched on clone fail");
        assert!(!cache.join("a@newsha").exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn cache_not_landed_leaves_pin_unchanged() {
        let d = tmp_dir("noland");
        let cache = d.join("cache");
        let lock = write_lock(&d, "a", "oldsha");

        // clone seam "succeeds" but creates nothing → cache-verify must catch it.
        let res = apply_upgrade(&change("a", "newsha"), &cache, &[lock.as_path()], |_, _| Ok(()));
        assert!(matches!(res, Err(UpgradeError::CacheVerify(_))));
        let v: Value = serde_json::from_str(&fs::read_to_string(&lock).unwrap()).unwrap();
        assert_eq!(v["_personalPlugins"][0]["pin"], "oldsha", "pin untouched when cache absent");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn missing_pin_entry_errs_after_clone() {
        let d = tmp_dir("nopin");
        let cache = d.join("cache");
        let lock = write_lock(&d, "other", "x"); // entry for a different plugin

        let res = apply_upgrade(&change("a", "newsha"), &cache, &[lock.as_path()], |_, dest| {
            fs::create_dir_all(dest).unwrap();
            fs::write(dest.join("f"), b"x").unwrap();
            Ok(())
        });
        assert!(matches!(res, Err(UpgradeError::PinMissing(_))));
        let _ = fs::remove_dir_all(&d);
    }
}

#[cfg(test)]
mod orchestrator_tests {
    use super::*;
    use serde_json::json;
    use std::cell::Cell;
    use std::fs;

    fn tmp_dir(tag: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("ccync-run-up-{tag}-{:?}", std::thread::current().id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn git(id: &str, pin: &str) -> Value {
        json!({
            "pluginId": id, "installStrategy": "personal-git-clone",
            "source": format!("https://github.com/user/{id}.git"), "pin": pin
        })
    }

    fn opts(plugins: Value, dir: &std::path::Path, filter: Option<&str>, dry: bool) -> UpgradeOpts {
        let lock = dir.join("lock.json");
        fs::write(&lock, serde_json::to_string_pretty(&plugins).unwrap()).unwrap();
        UpgradeOpts {
            plugins,
            filter_id: filter.map(String::from),
            dry_run: dry,
            cache_root: dir.join("cache"),
            pin_files: vec![lock],
        }
    }

    fn ok_clone() -> impl Fn(&str, &Path) -> Result<(), String> {
        |_, dest| {
            fs::create_dir_all(dest).unwrap();
            fs::write(dest.join("p"), b"x").unwrap();
            Ok(())
        }
    }

    #[test]
    fn dry_run_classifies_without_apply_or_project() {
        let d = tmp_dir("dry");
        let plugins = json!({ "_personalPlugins": [git("a", "old")] });
        let o = opts(plugins, &d, None, true);
        let applied = Cell::new(false);
        let projected = Cell::new(false);

        let r = run_upgrade(
            &o,
            |_, _| Ok("new".to_string()),
            |_, _| {
                applied.set(true);
                Ok(())
            },
            || {
                projected.set(true);
                Ok(())
            },
        );
        assert!(r.dry_run);
        assert_eq!(r.pending.len(), 1, "one pending change reported");
        assert!(r.upgraded.is_empty());
        assert!(!applied.get() && !projected.get(), "dry-run touches nothing");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn pending_change_carries_old_sha() {
        // A Changed outcome must thread its old_sha into UpgradeChange so
        // dry-run / list --upgrade-available can render `current → latest`.
        let d = tmp_dir("oldsha");
        let plugins = json!({ "_personalPlugins": [git("a", "old")] });
        let o = opts(plugins, &d, None, true);

        let r = run_upgrade(&o, |_, _| Ok("new".to_string()), ok_clone(), || Ok(()));
        assert_eq!(r.pending.len(), 1);
        assert_eq!(
            r.pending[0].old_sha.as_deref(),
            Some("old"),
            "old_sha must be carried through, not dropped"
        );
        assert_eq!(r.pending[0].new_sha, "new");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn real_run_upgrades_changed_and_projects_once() {
        let d = tmp_dir("real");
        let plugins = json!({ "_personalPlugins": [git("a", "old"), git("b", "same")] });
        let o = opts(plugins, &d, None, false);
        let project_calls = Cell::new(0);

        let r = run_upgrade(
            &o,
            |src, _| Ok(if src.contains("/a.git") { "new" } else { "same" }.to_string()),
            ok_clone(),
            || {
                project_calls.set(project_calls.get() + 1);
                Ok(())
            },
        );
        assert_eq!(r.upgraded, vec!["a".to_string()]);
        assert_eq!(r.unchanged, vec!["b".to_string()]);
        assert!(r.projected);
        assert_eq!(project_calls.get(), 1, "projection runs exactly once");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn no_changes_skips_projection() {
        let d = tmp_dir("nochange");
        let plugins = json!({ "_personalPlugins": [git("a", "same")] });
        let o = opts(plugins, &d, None, false);
        let projected = Cell::new(false);

        let r = run_upgrade(
            &o,
            |_, _| Ok("same".to_string()),
            ok_clone(),
            || {
                projected.set(true);
                Ok(())
            },
        );
        assert!(r.upgraded.is_empty());
        assert!(!r.projected && !projected.get(), "no pin change → no projection");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn single_id_filter_processes_only_that_plugin() {
        let d = tmp_dir("filter");
        let plugins = json!({ "_personalPlugins": [git("a", "old"), git("b", "old")] });
        let o = opts(plugins, &d, Some("b"), false);

        let r = run_upgrade(&o, |_, _| Ok("new".to_string()), ok_clone(), || Ok(()));
        assert_eq!(r.upgraded, vec!["b".to_string()]);
        assert!(r.error.is_none());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn unknown_filter_id_is_actionable_error() {
        let d = tmp_dir("badfilter");
        let plugins = json!({ "_personalPlugins": [git("a", "old")] });
        let o = opts(plugins, &d, Some("nope"), false);

        let r = run_upgrade(&o, |_, _| Ok("new".to_string()), ok_clone(), || Ok(()));
        assert!(r.error.is_some(), "missing id reports an error");
        assert!(r.upgraded.is_empty());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn one_apply_failure_isolated_from_others() {
        let d = tmp_dir("isolate");
        let plugins = json!({ "_personalPlugins": [git("a", "old"), git("b", "old")] });
        let o = opts(plugins, &d, None, false);

        // clone fails only for plugin a.
        let r = run_upgrade(
            &o,
            |_, _| Ok("new".to_string()),
            |src, dest| {
                if src.contains("/a.git") {
                    Err("network".to_string())
                } else {
                    fs::create_dir_all(dest).unwrap();
                    fs::write(dest.join("p"), b"x").unwrap();
                    Ok(())
                }
            },
            || Ok(()),
        );
        assert_eq!(r.upgraded, vec!["b".to_string()], "b still upgraded");
        assert_eq!(r.failed.len(), 1, "a failed but did not abort b");
        assert_eq!(r.failed[0].0, "a");
        let _ = fs::remove_dir_all(&d);
    }
}
