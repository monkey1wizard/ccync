//! Projection support: health check, frontmatter rendering, file/list/link helpers.

use super::*;
use ccync_foundation::health::{DoctorFinding, HealthCheck};
use ccync_foundation::json_util::read_json_map;
use ccync_foundation::platform::{create_dir_link, is_symlink_or_junction, remove_dir_link};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const PROJECTION_LOCK_KEY: &str = "_ccyncProjection";
const PROJECTION_LOCK_SCHEMA_VERSION: u64 = 1;
const MANAGED_SOURCE_ATTRIBUTION: &str = "sourceAttribution";

pub(crate) const MANAGED_AGENT_PATHS: &str = "agentProjectionPaths";
pub(crate) const MANAGED_CODEX_AGENT_PATHS: &str = "codexAgentProjectionPaths";
pub(crate) const MANAGED_COMMAND_PATHS: &str = "commandProjectionPaths";
pub(crate) const MANAGED_DISCUSS_SKILL_PATHS: &str = "discussSkillProjectionPaths";
pub(crate) const MANAGED_LEGACY_PATHS: &str = "legacyProjectionPaths";
pub(crate) const MANAGED_SKILL_PATHS: &str = "skillProjectionPaths";

pub struct SkillsProjectionHealthCheck {
    shared_skills_root: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ManagedArtifactRegistry {
    lockfile_path: PathBuf,
    lockfile_root: Option<Map<String, Value>>,
    prior: BTreeMap<String, BTreeSet<String>>,
    next: BTreeMap<String, BTreeSet<String>>,
    /// Maps normalized path → source id recorded in the prior lockfile.
    /// Populated from `"sourceAttribution"` on load; used by per-source prune.
    source_attribution_prior: BTreeMap<String, String>,
    /// Maps normalized path → source id being built for the next lockfile write.
    source_attribution_next: BTreeMap<String, String>,
}

impl ManagedArtifactRegistry {
    pub(crate) fn load(lockfile_path: &Path, report: &mut ProjectionReport) -> Self {
        let mut registry = Self {
            lockfile_path: lockfile_path.to_path_buf(),
            ..Self::default()
        };
        if !lockfile_path.is_file() {
            return registry;
        }

        let raw = match fs::read_to_string(lockfile_path) {
            Ok(raw) => raw,
            Err(err) => {
                report.warnings.push(ProjectionWarning {
                    message: format!(
                        "could not read projection lockfile marker, skipped managed cleanup: {} ({err})",
                        lockfile_path.display()
                    ),
                });
                return registry;
            }
        };

        let root = match serde_json::from_str::<Value>(&raw) {
            Ok(Value::Object(root)) => root,
            Ok(_) => {
                report.warnings.push(ProjectionWarning {
                    message: format!(
                        "projection lockfile marker is not a JSON object, skipped managed cleanup: {}",
                        lockfile_path.display()
                    ),
                });
                return registry;
            }
            Err(err) => {
                report.warnings.push(ProjectionWarning {
                    message: format!(
                        "could not parse projection lockfile marker, skipped managed cleanup: {} ({err})",
                        lockfile_path.display()
                    ),
                });
                return registry;
            }
        };

        let mut projection = BTreeMap::new();
        let mut attribution = BTreeMap::new();
        if let Some(Value::Object(meta)) = root.get(PROJECTION_LOCK_KEY) {
            for category in [
                MANAGED_AGENT_PATHS,
                MANAGED_CODEX_AGENT_PATHS,
                MANAGED_COMMAND_PATHS,
                MANAGED_LEGACY_PATHS,
                MANAGED_SKILL_PATHS,
            ] {
                projection.insert(
                    category.to_string(),
                    load_managed_artifact_set(meta, category),
                );
            }
            if let Some(Value::Object(attr_map)) = meta.get(MANAGED_SOURCE_ATTRIBUTION) {
                for (path, src) in attr_map {
                    if let Some(src_str) = src.as_str() {
                        attribution.insert(path.clone(), src_str.to_string());
                    }
                }
            }
        }

        registry.lockfile_root = Some(root);
        registry.prior = projection.clone();
        registry.next = projection;
        registry.source_attribution_prior = attribution.clone();
        registry.source_attribution_next = attribution;
        registry
    }

    pub(crate) fn can_mutate(&self, path: &Path) -> bool {
        // No prior lockfile → first-run; allow all mutations (backward-compat with is_ccync_managed_file guard).
        if self.prior.is_empty() {
            return true;
        }
        let needle = normalize_managed_artifact_path(path);
        self.prior.values().any(|paths| paths.contains(&needle))
    }

    /// Positive test: was this exact path recorded as CCYNC-managed in the prior
    /// lockfile? Unlike [`can_mutate`], this is `false` on first run (empty prior),
    /// so a pre-existing path that CCYNC never wrote is treated as user-owned even
    /// before any lockfile exists — used to guard command-skill writes from
    /// clobbering a genuine user directory.
    pub(crate) fn is_managed(&self, path: &Path) -> bool {
        let needle = normalize_managed_artifact_path(path);
        self.prior.values().any(|paths| paths.contains(&needle))
    }

    pub(crate) fn replace_category(&mut self, category: &str) {
        self.next.insert(category.to_string(), BTreeSet::new());
    }

    pub(crate) fn mark(&mut self, category: &str, path: &Path) {
        self.next
            .entry(category.to_string())
            .or_default()
            .insert(normalize_managed_artifact_path(path));
    }

    /// Like [`mark`], but also records which source owns this path.
    /// Used by multi-source projection so per-source prune can
    /// distinguish "source removed from installed set" from "source absent this invocation".
    pub(crate) fn mark_with_source(&mut self, category: &str, path: &Path, source_id: &str) {
        let normalized = normalize_managed_artifact_path(path);
        self.next
            .entry(category.to_string())
            .or_default()
            .insert(normalized.clone());
        self.source_attribution_next
            .insert(normalized, source_id.to_string());
    }

    /// Return the source id that owned `path` according to the prior lockfile.
    /// Returns `None` when attribution was not recorded (single-source or pre-attribution lockfile).
    pub(crate) fn owning_source_of(&self, path: &Path) -> Option<&str> {
        let needle = normalize_managed_artifact_path(path);
        self.source_attribution_prior
            .get(&needle)
            .map(String::as_str)
    }

    pub(crate) fn persist(
        &self,
        dry_run: bool,
        report: &mut ProjectionReport,
    ) -> Result<(), AdapterError> {
        // Bootstrap a fresh root when no prior lockfile existed.
        let mut root = self.lockfile_root.clone().unwrap_or_default();
        if self.prior == self.next && self.source_attribution_prior == self.source_attribution_next
        {
            return Ok(());
        }

        let non_empty = self.next.values().any(|paths| !paths.is_empty());
        if non_empty {
            let mut meta = Map::new();
            meta.insert(
                "schemaVersion".into(),
                Value::Number(PROJECTION_LOCK_SCHEMA_VERSION.into()),
            );
            for category in [
                MANAGED_AGENT_PATHS,
                MANAGED_CODEX_AGENT_PATHS,
                MANAGED_COMMAND_PATHS,
                MANAGED_LEGACY_PATHS,
                MANAGED_SKILL_PATHS,
            ] {
                let values = self
                    .next
                    .get(category)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(Value::String)
                    .collect();
                meta.insert(category.into(), Value::Array(values));
            }
            if !self.source_attribution_next.is_empty() {
                let attr_obj: Map<String, Value> = self
                    .source_attribution_next
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                    .collect();
                meta.insert(MANAGED_SOURCE_ATTRIBUTION.into(), Value::Object(attr_obj));
            }
            root.insert(PROJECTION_LOCK_KEY.into(), Value::Object(meta));
        } else {
            root.remove(PROJECTION_LOCK_KEY);
        }

        let rendered = serde_json::to_string_pretty(&Value::Object(root))
            .map_err(|err| AdapterError::Message(err.to_string()))?;
        if write_text(&self.lockfile_path, &rendered, dry_run)? {
            report.written_files.push(self.lockfile_path.clone());
        }
        Ok(())
    }
}

impl SkillsProjectionHealthCheck {
    pub fn with_path(shared_skills_root: PathBuf) -> Self {
        Self { shared_skills_root }
    }
}

impl HealthCheck for SkillsProjectionHealthCheck {
    fn name(&self) -> &str {
        "skills-projection"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        if !self.shared_skills_root.exists() {
            return vec![DoctorFinding::warning(format!(
                "shared skill projection not found: {} — run `ccync sync`",
                self.shared_skills_root.display()
            ))];
        }
        if !self.shared_skills_root.is_dir() {
            return vec![DoctorFinding::error(
                format!(
                    "shared skill projection is not a directory: {}",
                    self.shared_skills_root.display()
                ),
                "remove the path and rerun `ccync sync`",
            )];
        }
        vec![]
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Frontmatter {
    pub(crate) description: Option<String>,
    pub(crate) color: Option<String>,
    pub(crate) tools: Vec<String>,
}

pub(crate) fn parse_frontmatter(raw: &str) -> Frontmatter {
    let mut lines = raw.lines();
    if lines.next() != Some("---") {
        return Frontmatter::default();
    }

    let mut description = None;
    let mut color = None;
    let mut tools = Vec::new();
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            let value = value.trim().trim_matches('"');
            if key == "description" {
                description = Some(value.to_string());
            } else if key == "color" {
                color = Some(value.to_string());
            } else if key == "tools" {
                tools = value
                    .trim_matches(['[', ']'])
                    .split(',')
                    .map(|part| part.trim().trim_matches('"').trim_matches('\''))
                    .filter(|part| !part.is_empty())
                    .map(str::to_string)
                    .collect();
            }
        }
    }

    Frontmatter {
        description,
        color,
        tools,
    }
}

pub(crate) fn strip_frontmatter(raw: &str) -> String {
    let mut lines = raw.lines();
    if lines.next() != Some("---") {
        return raw.trim().to_string();
    }

    let mut body = String::new();
    let mut in_frontmatter = true;
    for line in raw.lines().skip(1) {
        if in_frontmatter {
            if line.trim() == "---" {
                in_frontmatter = false;
            }
            continue;
        }
        body.push_str(line);
        body.push('\n');
    }
    body.trim().to_string()
}

pub(crate) fn render_opencode_agent(_name: &str, frontmatter: &Frontmatter, body: &str) -> String {
    let mut permissions = Vec::new();
    for tool in &frontmatter.tools {
        match tool.as_str() {
            "read" => {
                push_permission(&mut permissions, "read");
                push_permission(&mut permissions, "list");
            }
            "search" => {
                push_permission(&mut permissions, "read");
                push_permission(&mut permissions, "list");
                push_permission(&mut permissions, "grep");
                push_permission(&mut permissions, "glob");
            }
            "list" => {
                push_permission(&mut permissions, "list");
            }
            "edit" => {
                push_permission(&mut permissions, "edit");
            }
            "execute" => {
                push_permission(&mut permissions, "bash");
            }
            _ => {}
        }
    }

    if permissions.is_empty() {
        push_permission(&mut permissions, "read");
        push_permission(&mut permissions, "list");
    }

    let description = frontmatter
        .description
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "CCYNC golem agent".into());

    let mut lines = vec![
        CCYNC_MANAGED_FILE_HEADER.to_string(),
        "---".into(),
        "description: |".into(),
    ];
    for line in description.lines() {
        lines.push(format!("  {line}"));
    }
    lines.push("mode: subagent".into());
    if let Some(color) = &frontmatter.color {
        lines.push(format!("color: {color}"));
    }
    lines.push("permission:".into());
    for permission in permissions {
        lines.push(format!("  {permission}: allow"));
    }
    lines.push("---".into());
    lines.push(String::new());
    lines.push(body.trim().to_string());
    lines.push(String::new());
    lines.join("\n")
}

pub(crate) fn render_gemini_command(_name: &str, frontmatter: &Frontmatter, body: &str) -> String {
    let description = frontmatter
        .description
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "CCYNC command".into());
    let prompt = format!(
        "User command arguments, if any: {{{{args}}}}\n\n{}",
        body.trim()
    );
    format!(
        "{header}\ndescription = \"{description}\"\nprompt = '''\n{prompt}\n'''\n",
        header = CCYNC_MANAGED_FILE_HEADER,
        description = escape_toml_basic_string(&description),
        prompt = prompt.replace("'''", "\\'\\'\\'")
    )
}

pub(crate) fn render_opencode_command(name: &str, frontmatter: &Frontmatter, body: &str) -> String {
    let description = frontmatter
        .description
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "CCYNC command".into());
    let rendered_body = if name == "git-commit-msg" {
        "Use the baseline below for the type(scope) prefix (it is a no-hijack path/status classifier that never reads the diff body), then read the staged diff and write a subject describing what actually changed. Do not ship the generic baseline subject verbatim. Keep the type(scope) prefix unless the diff clearly contradicts it. Add up to three body bullets only for broader changes. Do not add explanations, markdown fences, reasoning tags, JSON, or any extra prose. If the baseline reports No changes staged for commit. or Not a git repository., return that text exactly. Apply extra instructions if provided: $ARGUMENTS\n\n!`ccync commit-msg --print`".to_string()
    } else {
        body.trim().to_string()
    };

    let mut lines = vec![
        CCYNC_MANAGED_FILE_HEADER.to_string(),
        "---".into(),
        "description: |".into(),
    ];
    for line in description.lines() {
        lines.push(format!("  {line}"));
    }
    lines.push("---".into());
    lines.push(String::new());
    lines.push("User command arguments, if any: $ARGUMENTS".into());
    lines.push(String::new());
    lines.push(rendered_body);
    lines.push(String::new());
    lines.join("\n")
}

pub(crate) fn push_permission(permissions: &mut Vec<&'static str>, value: &'static str) {
    if !permissions.contains(&value) {
        permissions.push(value);
    }
}

pub(crate) fn list_named_children(root: &Path) -> Result<Vec<String>, AdapterError> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            names.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    names.sort();
    Ok(names)
}

pub(crate) fn list_files_with_extension(
    root: &Path,
    suffix: &str,
) -> Result<Vec<PathBuf>, AdapterError> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry
                .path()
                .file_name()
                .and_then(OsStr::to_str)
                .map(|name| name.ends_with(suffix))
                .unwrap_or(false)
        {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

pub(crate) fn list_command_dirs(root: &Path) -> Result<Vec<PathBuf>, AdapterError> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut dirs = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        if path.join("SKILL.md").is_file() || path.join("SKILL.template.md").is_file() {
            dirs.push(path);
        }
    }
    dirs.sort();
    Ok(dirs)
}

pub(crate) fn read_markdown_required(path: &Path) -> Result<String, AdapterError> {
    fs::read_to_string(path).map_err(AdapterError::from)
}

pub(crate) fn bake_command_content(
    command_dir: &Path,
    repo_root: &Path,
) -> Result<String, AdapterError> {
    let template = command_dir.join("SKILL.template.md");
    if !template.is_file() {
        return read_markdown_required(&command_dir.join("SKILL.md"));
    }

    let mut baked = read_markdown_required(&template)?;
    baked = baked.replace("{{CCYNC_ROOT}}", &repo_root.display().to_string());

    let local_override = command_dir.join("SKILL.local.md");
    if !local_override.is_file() {
        return Ok(baked);
    }

    let local = read_markdown_required(&local_override)?;
    if local.trim().is_empty() {
        return Ok(baked);
    }

    Ok(format!(
        "{}\n\n<!-- CCYNC LOCAL OVERRIDE START -->\n<!-- Source: SKILL.local.md (gitignored machine-local overlay) -->\n{}\n<!-- CCYNC LOCAL OVERRIDE END -->\n",
        baked.trim_end(),
        local.trim()
    ))
}

pub(crate) fn escape_toml_basic_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

pub(crate) fn prune_stale_links<'a>(
    root: &Path,
    keep_names: impl IntoIterator<Item = &'a str>,
    dry_run: bool,
    report: &mut ProjectionReport,
    registry: &ManagedArtifactRegistry,
    installed_source_ids: &[&str],
) -> Result<(), AdapterError> {
    if !root.is_dir() {
        return Ok(());
    }

    let keep: Vec<&str> = keep_names.into_iter().collect();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if keep.contains(&name) {
            continue;
        }

        let path = entry.path();
        let symlink = is_symlink_or_junction(&path);

        // Per-source guard: if the owning source is still in the installed set,
        // do not prune even when the name is absent from this invocation.
        // This prevents cross-source misfire when a source is temporarily absent.
        let owning_source_installed = registry
            .owning_source_of(&path)
            .map(|src| installed_source_ids.contains(&src))
            .unwrap_or(false);
        if owning_source_installed {
            continue;
        }

        // Authorize removal when the entry is positively CCYNC-identified by content
        // (header marker or golem-agent frontmatter — the a2 escape hatch, so even
        // a pre-lockfile orphan the fail-safe would otherwise protect is cleaned),
        // when it is a CCYNC-managed symlink the lockfile permits mutating,
        // or when it is a plain file the prior lockfile explicitly recorded as written by CCYNC
        // (is_managed is false on first-run/empty-prior, preventing user-file deletion).
        let authorized = is_ccync_owned_real_path(&path)
            || (symlink && registry.can_mutate(&path))
            || (!symlink && !path.is_dir() && registry.is_managed(&path));
        if authorized {
            if !dry_run {
                if symlink {
                    remove_link(&path)?;
                } else if path.is_dir() {
                    fs::remove_dir_all(&path)?;
                } else {
                    fs::remove_file(&path)?;
                }
            }
            report.removed_paths.push(path);
        }
    }
    Ok(())
}

pub(crate) fn prune_stale_command_links(
    root: &Path,
    active_names: &[&str],
    dry_run: bool,
    report: &mut ProjectionReport,
    registry: &ManagedArtifactRegistry,
) -> Result<(), AdapterError> {
    if !root.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        if active_names.contains(&name)
            || !is_ccync_command_link(&path, &path_needle_for_commands())
            || !registry.can_mutate(&path)
        {
            continue;
        }
        remove_link_if_present(&path, dry_run, report)?;
    }
    Ok(())
}

pub(crate) fn prune_legacy_ccync_prefixed_dirs(
    root: &Path,
    active_names: &[&str],
    dry_run: bool,
    report: &mut ProjectionReport,
    registry: &ManagedArtifactRegistry,
) -> Result<(), AdapterError> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        if !name.starts_with("ccync-")
            || active_names.contains(&name)
            || !registry.can_mutate(&path)
        {
            continue;
        }
        if !dry_run {
            fs::remove_dir_all(&path)?;
        }
        report.removed_paths.push(path);
    }
    Ok(())
}

pub(crate) fn prune_stale_managed_files(
    root: &Path,
    extension: &str,
    active_names: &[&str],
    dry_run: bool,
    report: &mut ProjectionReport,
    registry: &ManagedArtifactRegistry,
) -> Result<(), AdapterError> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let is_extension_match = path
            .extension()
            .and_then(OsStr::to_str)
            .map(|value| value.eq_ignore_ascii_case(extension))
            .unwrap_or(false);
        if !is_extension_match || !is_ccync_managed_file(&path) || !registry.can_mutate(&path) {
            continue;
        }
        let Some(name) = path.file_stem().and_then(OsStr::to_str) else {
            continue;
        };
        if active_names.contains(&name) {
            continue;
        }
        remove_managed_file_if_present(&path, dry_run, report, registry)?;
    }
    Ok(())
}

pub(crate) fn ensure_dir(path: &Path, dry_run: bool) -> Result<(), AdapterError> {
    if dry_run || path.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(path)?;
    Ok(())
}

/// Project a baked command as a runtime skill at `<skills_root>/<name>/SKILL.md`.
///
/// A baked command (`SKILL.md` with `name`/`description` frontmatter) is already
/// a valid skill, so projection is a verbatim write — this exposes the CCYNC
/// command as a skill on runtimes whose capability unit is a skill, not a slash
/// command (Copilot `/ccync-status`, Codex `$ccync-status`).
///
/// Preserves a genuine user directory at the same name: a real (non-symlink)
/// directory that CCYNC never recorded (`!is_managed`, false even on first run) and
/// whose `SKILL.md` differs from our output is left untouched with a warning (the
/// a2 user-content guard). Returns the written path so the caller can record it in
/// the registry, or `None` when preserved.
pub(crate) fn write_command_skill(
    skills_root: &Path,
    name: &str,
    content: &str,
    dry_run: bool,
    report: &mut ProjectionReport,
    registry: &ManagedArtifactRegistry,
) -> Result<Option<PathBuf>, AdapterError> {
    let dir = skills_root.join(name);
    let skill_file = dir.join("SKILL.md");

    if dir.exists() && !is_symlink_or_junction(&dir) && !registry.is_managed(&skill_file) {
        let normalized = format!("{}\n", content.trim_end());
        let is_ours = fs::read_to_string(&skill_file)
            .ok()
            .map(|existing| existing == normalized)
            .unwrap_or(false);
        if !is_ours {
            report.warnings.push(ProjectionWarning {
                message: format!("preserved user-owned path: {}", dir.display()),
            });
            return Ok(None);
        }
    }

    if is_symlink_or_junction(&dir) && !dry_run {
        remove_link(&dir)?;
    }
    if write_text(&skill_file, content, dry_run)? {
        report.written_files.push(skill_file.clone());
    }
    Ok(Some(skill_file))
}

/// Remove a previously-projected command-skill at `<skills_root>/<name>` when its
/// runtime is no longer selected, so it does not linger in a shared skills dir
/// (`~/.agents/skills`, read by codex+opencode+copilot) and reintroduce a
/// double-load. Removes a legacy symlink, or a CCYNC-written real directory the
/// registry recorded (`is_managed`) — a genuine user directory (never managed)
/// is left untouched.
pub(crate) fn remove_ccync_command_skill(
    skills_root: &Path,
    name: &str,
    repo_root: &Path,
    registry: &ManagedArtifactRegistry,
    dry_run: bool,
    report: &mut ProjectionReport,
) -> Result<(), AdapterError> {
    let dir = skills_root.join(name);
    if is_symlink_or_junction(&dir) {
        // Only a CCYNC-owned repo-link is removed; a user's own symlink (e.g. to an
        // external skill) is preserved.
        if is_ccync_repo_link(&dir, repo_root) {
            remove_link_if_present(&dir, dry_run, report)?;
        }
    } else if dir.is_dir() && registry.is_managed(&dir.join("SKILL.md")) {
        if !dry_run {
            fs::remove_dir_all(&dir)?;
        }
        report.removed_paths.push(dir);
    }
    Ok(())
}

pub(crate) fn write_text(path: &Path, content: &str, dry_run: bool) -> Result<bool, AdapterError> {
    let normalized = format!("{}\n", content.trim_end());
    if fs::read_to_string(path)
        .ok()
        .map(|existing| existing == normalized)
        .unwrap_or(false)
    {
        return Ok(false);
    }
    if !dry_run {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, normalized.as_bytes())?;
    }
    Ok(true)
}

pub(crate) fn remove_link_if_present(
    path: &Path,
    dry_run: bool,
    report: &mut ProjectionReport,
) -> Result<(), AdapterError> {
    if !is_symlink_or_junction(path) {
        return Ok(());
    }
    if !dry_run {
        remove_link(path)?;
    }
    report.removed_paths.push(path.to_path_buf());
    Ok(())
}

pub(crate) fn remove_managed_file_if_present(
    path: &Path,
    dry_run: bool,
    report: &mut ProjectionReport,
    registry: &ManagedArtifactRegistry,
) -> Result<(), AdapterError> {
    if !is_ccync_managed_file(path) || !registry.can_mutate(path) {
        return Ok(());
    }
    if !dry_run {
        fs::remove_file(path)?;
    }
    report.removed_paths.push(path.to_path_buf());
    Ok(())
}

pub(crate) fn ensure_dir_link(
    target: &Path,
    link: &Path,
    replace: bool,
    dry_run: bool,
    report: &mut ProjectionReport,
) -> Result<(), AdapterError> {
    if same_path(link, target) {
        return Ok(());
    }
    if is_symlink_or_junction(link) {
        if dry_run {
            report.created_links.push(link.to_path_buf());
            return Ok(());
        }
        remove_dir_link(link)?;
    } else if link.exists() {
        // Adopt a CCYNC-owned real path (file→symlink migration) without requiring
        // --replace; preserve only genuine user-owned paths (no CCYNC content marker
        // and not a CCYNC-projected golem agent).
        if !replace && !is_ccync_owned_real_path(link) {
            report.warnings.push(ProjectionWarning {
                message: format!("preserved user-owned path: {}", link.display()),
            });
            return Ok(());
        }
        let backup = backup_path(link);
        if !dry_run {
            if backup.exists() {
                if backup.is_dir() {
                    fs::remove_dir_all(&backup)?;
                } else {
                    fs::remove_file(&backup)?;
                }
            }
            fs::rename(link, &backup)?;
        }
        report.removed_paths.push(backup);
    }
    if !dry_run {
        if let Some(parent) = link.parent() {
            fs::create_dir_all(parent)?;
        }
        create_dir_link(target, link)?;
    }
    report.created_links.push(link.to_path_buf());
    Ok(())
}

pub(crate) fn same_path(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

pub(crate) fn backup_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .map(|value| format!("{value}.bak"))
        .unwrap_or_else(|| "backup.bak".into());
    path.with_file_name(name)
}

pub(crate) fn is_ccync_managed_file(path: &Path) -> bool {
    fs::read_to_string(path)
        .ok()
        .and_then(|content| content.lines().next().map(str::to_string))
        .map(|first| first == CCYNC_MANAGED_FILE_HEADER)
        .unwrap_or(false)
}

/// Content-marker test for an existing real (non-symlink) path during adopt.
///
/// A file is CCYNC-managed when its first line is [`CCYNC_MANAGED_FILE_HEADER`].
/// A directory is CCYNC-managed when it shallowly contains at least one
/// CCYNC-managed file — this lets the projection re-adopt a directory a prior
/// projection era materialized as real files (file→symlink migration) without
/// ever adopting a genuine user directory (which carries no CCYNC header).
pub(crate) fn is_ccync_managed_path(path: &Path) -> bool {
    if path.is_dir() {
        fs::read_dir(path)
            .ok()
            .map(|entries| {
                entries.flatten().any(|entry| {
                    let child = entry.path();
                    child.is_file() && is_ccync_managed_file(&child)
                })
            })
            .unwrap_or(false)
    } else {
        is_ccync_managed_file(path)
    }
}

/// Does this file look like a CCYNC-projected golem agent?
///
/// Agent projections are verbatim copies of `plugins/ccync-core/agents/<name>.agent.md`:
/// YAML frontmatter whose `name:` value is a `golem-*` agent. They carry **no**
/// [`CCYNC_MANAGED_FILE_HEADER`] (the first line is `---`), so the header check
/// misses them. Detect by the golem-agent frontmatter signature instead — a
/// content identity, not a path/directory exclusion.
pub(crate) fn is_ccync_projected_agent_file(path: &Path) -> bool {
    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };
    let mut lines = content.lines();
    if lines.next() != Some("---") {
        return false;
    }
    for line in lines.take(15) {
        if line == "---" {
            break;
        }
        if let Some(rest) = line.strip_prefix("name:") {
            return rest
                .trim()
                .trim_matches(|c| c == '"' || c == '\'')
                .starts_with("golem-");
        }
    }
    false
}

/// Positive CCYNC-ownership test for an existing real path, used by both adopt
/// ([`ensure_dir_link`]) and prune ([`prune_stale_links`]).
///
/// True when the path carries a CCYNC content marker ([`is_ccync_managed_path`]) or
/// is a CCYNC-projected golem agent ([`is_ccync_projected_agent_file`]). This is the
/// sole criterion for re-adopting / pruning a CCYNC real-file residue: it is a
/// positive content identification (the sanctioned a2 escape hatch), never a
/// path/directory exclusion and never an unconditional overwrite.
pub(crate) fn is_ccync_owned_real_path(path: &Path) -> bool {
    is_ccync_managed_path(path) || (path.is_file() && is_ccync_projected_agent_file(path))
}

pub(crate) fn is_ccync_repo_link(path: &Path, repo_root: &Path) -> bool {
    if !is_symlink_or_junction(path) {
        return false;
    }
    match (fs::canonicalize(path), fs::canonicalize(repo_root)) {
        (Ok(target), Ok(root)) => target.starts_with(root),
        _ => false,
    }
}

pub(crate) fn path_needle_for_commands() -> String {
    format!(
        "{}commands{}",
        std::path::MAIN_SEPARATOR,
        std::path::MAIN_SEPARATOR
    )
}

pub(crate) fn is_ccync_command_link(path: &Path, repo_root_fragment: &str) -> bool {
    if !is_symlink_or_junction(path) {
        return false;
    }
    fs::canonicalize(path)
        .ok()
        .and_then(|target| target.to_str().map(str::to_string))
        .map(|target| target.contains(repo_root_fragment))
        .unwrap_or(false)
}

pub(crate) fn remove_link(path: &Path) -> io::Result<()> {
    // A directory junction / dir-symlink reports `is_dir() == false` via
    // `symlink_metadata` on Windows (a reparse point is a symlink, not a dir), so
    // gating link removal on `is_dir()` sent directory junctions to `fs::remove_file`
    // → `DeleteFile` refuses a directory reparse point with ERROR_ACCESS_DENIED
    // (os error 5). Route any reparse point through `remove_dir_link` (rmdir — removes
    // the link, not the target), falling back to `remove_file` for a file symlink.
    if is_symlink_or_junction(path) {
        return remove_dir_link(path).or_else(|_| fs::remove_file(path));
    }
    let metadata = path.symlink_metadata()?;
    if metadata.file_type().is_dir() {
        remove_dir_link(path)
    } else {
        fs::remove_file(path)
    }
}

pub(crate) fn remove_if_ccync_owned_dir(
    path: &Path,
    dry_run: bool,
    report: &mut ProjectionReport,
    registry: &ManagedArtifactRegistry,
) -> Result<(), AdapterError> {
    if !path.exists() {
        return Ok(());
    }
    if !registry.can_mutate(path) {
        return Ok(());
    }
    if is_symlink_or_junction(path) {
        if !dry_run {
            remove_dir_link(path)?;
        }
        report.removed_paths.push(path.to_path_buf());
        return Ok(());
    }
    if path.is_dir() {
        let marker = path.join(".ccync-managed");
        if marker.exists() || path.file_name().and_then(OsStr::to_str) == Some("skills") {
            if !dry_run {
                fs::remove_dir_all(path)?;
            }
            report.removed_paths.push(path.to_path_buf());
        }
    }
    Ok(())
}

pub(crate) fn read_json_map_or_warn(
    path: &Path,
    report: &mut ProjectionReport,
) -> Result<Option<Map<String, Value>>, AdapterError> {
    match read_json_map(path) {
        Ok(map) => Ok(Some(map)),
        Err(err) if path.exists() => {
            report.warnings.push(ProjectionWarning {
                message: format!(
                    "could not parse JSON file, skipped bridge update: {} ({err})",
                    path.display()
                ),
            });
            Ok(None)
        }
        Err(err) => Err(AdapterError::from(err)),
    }
}

pub(crate) fn merge_context_file_names(settings: &mut Map<String, Value>, required: &[&str]) {
    let context = settings
        .entry("context")
        .or_insert_with(|| Value::Object(Map::new()));
    if !context.is_object() {
        *context = Value::Object(Map::new());
    }
    let context_obj = context.as_object_mut().expect("context object");
    let file_name = context_obj
        .entry("fileName")
        .or_insert_with(|| Value::Array(Vec::new()));
    let mut values = match file_name {
        Value::Array(existing) => existing
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<Vec<_>>(),
        Value::String(existing) => vec![existing.clone()],
        _ => Vec::new(),
    };
    for value in required {
        if !values.iter().any(|existing| existing == value) {
            values.push((*value).to_string());
        }
    }
    *file_name = Value::Array(values.into_iter().map(Value::String).collect());
}

pub(crate) fn set_vscode_skills_bridge(settings: &mut Map<String, Value>) {
    let locations = settings
        .entry("chat.agentSkillsLocations".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !locations.is_object() {
        *locations = Value::Object(Map::new());
    }
    locations
        .as_object_mut()
        .expect("locations object")
        .insert("~/.agents/skills".into(), Value::Bool(false));
}

pub(crate) fn write_text_with_report(
    path: &Path,
    content: &str,
    dry_run: bool,
    report: &mut ProjectionReport,
) -> Result<(), AdapterError> {
    if write_text(path, content, dry_run)? {
        report.written_files.push(path.to_path_buf());
    }
    Ok(())
}

pub(crate) fn write_json_map_with_report(
    path: &Path,
    data: &Map<String, Value>,
    dry_run: bool,
    report: &mut ProjectionReport,
) -> Result<(), AdapterError> {
    let rendered = serde_json::to_string_pretty(&Value::Object(data.clone()))
        .map_err(|err| AdapterError::Message(err.to_string()))?;
    write_text_with_report(path, &rendered, dry_run, report)
}

fn load_managed_artifact_set(meta: &Map<String, Value>, category: &str) -> BTreeSet<String> {
    meta.get(category)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn normalize_managed_artifact_path(path: &Path) -> String {
    let rendered = path.to_string_lossy().replace('/', "\\");
    #[cfg(windows)]
    {
        rendered.to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        rendered
    }
}

pub(crate) fn remove_file_if_present(
    path: &Path,
    dry_run: bool,
    report: &mut ProjectionReport,
) -> Result<(), AdapterError> {
    if !path.is_file() {
        return Ok(());
    }
    if !dry_run {
        fs::remove_file(path)?;
    }
    report.removed_paths.push(path.to_path_buf());
    Ok(())
}

pub(crate) fn remove_empty_dir_if_present(
    path: &Path,
    dry_run: bool,
    report: &mut ProjectionReport,
) -> Result<(), AdapterError> {
    if !path.is_dir() {
        return Ok(());
    }
    if fs::read_dir(path)?.next().is_some() {
        return Ok(());
    }
    if !dry_run {
        fs::remove_dir(path)?;
    }
    report.removed_paths.push(path.to_path_buf());
    Ok(())
}
