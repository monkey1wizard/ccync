//! Projection support: health check, frontmatter rendering, file/list/link helpers.

use super::*;
use ccync_foundation::health::{DoctorFinding, HealthCheck};
use ccync_foundation::json_util::read_json_map;
use ccync_foundation::platform::{create_dir_link, is_symlink_or_junction, remove_dir_link};
use ccync_foundation::target::ProjectionTargetId;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

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

/// Registry-backed de-projection authority for uninstall (Protected Path).
///
/// Wraps the prior `build/lock.json` managed-artifact registry and exposes a
/// positive-membership authorization check for engine-side removal of
/// ccync-owned projections. Authorization uses [`ManagedArtifactRegistry::is_managed`]
/// (recorded-in-prior membership), deliberately NOT `can_mutate`: `can_mutate`
/// is allow-all when the prior lockfile is empty/missing, and that empty state is
/// the common partial-uninstall case — using it would authorize deleting
/// same-named user-owned siblings. With `is_managed`, an empty/missing prior
/// authorizes nothing.
pub struct ManagedDeprojection {
    registry: ManagedArtifactRegistry,
}

impl ManagedDeprojection {
    /// Load the prior managed registry from `build/lock.json`. A missing or
    /// unreadable lockfile yields an authority that authorizes nothing.
    pub fn from_lockfile(lockfile_path: &Path) -> Self {
        let mut report = ProjectionReport::default();
        Self {
            registry: ManagedArtifactRegistry::load(lockfile_path, &mut report),
        }
    }

    /// True iff `path` was recorded as ccync-managed in the prior lockfile.
    /// False on empty/missing prior (no allow-all) and for any path ccync never
    /// projected — so a non-managed same-named user sibling is never authorized.
    pub fn is_authorized_for_removal(&self, path: &Path) -> bool {
        self.registry.is_managed(path)
    }

    /// True when the prior lockfile recorded no managed paths — nothing to
    /// de-project; callers may short-circuit.
    pub fn is_empty(&self) -> bool {
        self.registry.prior_is_empty()
    }
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

    /// True when the prior lockfile recorded no managed paths (missing/empty
    /// lockfile, or a lockfile with no projection marker).
    pub(crate) fn prior_is_empty(&self) -> bool {
        self.prior.values().all(BTreeSet::is_empty)
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

    /// GC attribution rows that name an installed source but no longer appear
    /// in any managed-artifact category — an orphan left behind when a path
    /// was pruned/replaced elsewhere without its attribution row being
    /// cleared alongside it. A row owned by a source absent from
    /// `installed_source_ids` is retained untouched: this invocation has no
    /// authority to judge whether it's still live, mirroring the
    /// foreign-source protection in [`prune_stale_links`].
    fn gc_installed_source_orphans(&mut self, installed_source_ids: &[&str]) {
        let live: BTreeSet<String> = self.next.values().flatten().cloned().collect();
        self.source_attribution_next.retain(|path, source_id| {
            live.contains(path) || !installed_source_ids.contains(&source_id.as_str())
        });
    }

    pub(crate) fn persist(
        &mut self,
        installed_source_ids: &[&str],
        dry_run: bool,
        report: &mut ProjectionReport,
    ) -> Result<(), ProjectionError> {
        self.gc_installed_source_orphans(installed_source_ids);

        // Bootstrap a fresh root when no prior lockfile existed.
        let mut root = self.lockfile_root.clone().unwrap_or_default();
        if self.prior == self.next && self.source_attribution_prior == self.source_attribution_next
        {
            return Ok(());
        }

        let non_empty = self.next.values().any(|paths| !paths.is_empty())
            || !self.source_attribution_next.is_empty();
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
            .map_err(|err| ProjectionError::Message(err.to_string()))?;
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
        .unwrap_or_else(|| "CCYNC managed agent".into());

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

pub(crate) fn list_named_children(root: &Path) -> Result<Vec<String>, ProjectionError> {
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
) -> Result<Vec<PathBuf>, ProjectionError> {
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

pub(crate) fn list_command_dirs(root: &Path) -> Result<Vec<PathBuf>, ProjectionError> {
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

pub(crate) fn read_markdown_required(path: &Path) -> Result<String, ProjectionError> {
    fs::read_to_string(path).map_err(ProjectionError::from)
}

pub(crate) fn bake_command_content(
    command_dir: &Path,
    repo_root: &Path,
) -> Result<String, ProjectionError> {
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

/// Record a single touched path into `evidence`, attributing it to `owner`'s
/// own target-scoped bucket when the caller supplies one, or to `common` when
/// `owner` is `None` — the caller's own context (which target it is currently
/// projecting for, if any) decides attribution directly.
fn record_evidence_path(
    evidence: &mut ProjectionEvidence,
    path: &Path,
    change: ProjectionPathChange,
    owner: Option<ProjectionTargetId>,
) {
    let result = ProjectionPathResult {
        path: path.to_path_buf(),
        change,
    };
    match owner {
        Some(target) => evidence
            .by_target
            .entry(target)
            .or_default()
            .paths
            .push(result),
        None => evidence.common.push(result),
    }
}

/// Additive evidence-shaped counterpart to [`write_text_with_report`] — same
/// write, same on-disk effect (delegates to the same [`write_text`]), but
/// attributes the touched path directly via `owner` instead of relying on
/// post-hoc bare-path classification. Nothing calls this yet; it exists
/// alongside the unchanged legacy function.
pub(crate) fn write_text_with_evidence(
    path: &Path,
    content: &str,
    dry_run: bool,
    owner: Option<ProjectionTargetId>,
) -> Result<ProjectionEvidence, ProjectionError> {
    let mut evidence = ProjectionEvidence::default();
    if write_text(path, content, dry_run)? {
        record_evidence_path(&mut evidence, path, ProjectionPathChange::Written, owner);
    }
    Ok(evidence)
}

/// Additive evidence-shaped counterpart to [`write_json_map_with_report`] —
/// same rendering and same write as the legacy function, attributing the
/// touched path via `owner`. Nothing calls this yet.
#[allow(dead_code)] // additive, unwired — production callers land in a later task
pub(crate) fn write_json_map_with_evidence(
    path: &Path,
    data: &Map<String, Value>,
    dry_run: bool,
    owner: Option<ProjectionTargetId>,
) -> Result<ProjectionEvidence, ProjectionError> {
    let rendered = serde_json::to_string_pretty(&Value::Object(data.clone()))
        .map_err(|err| ProjectionError::Message(err.to_string()))?;
    write_text_with_evidence(path, &rendered, dry_run, owner)
}

/// Prune stale entries under `root`, attributing each removed path via
/// `owner` (or `common` when `owner` is `None`). Identical scan,
/// ownership-authorization, and removal logic (same I/O, same faults
/// propagated via `?`) regardless of which target owns the root.
///
/// `sources` is the full currently-installed source list (ids derived here);
/// a source being uninstalled must still appear in it with zero `keep_names`
/// during its final projection pass (see `docs/devguide.md`) so its stale
/// entries fall through to normal authorization instead of being protected.
/// An entry is protected here (skipped) only when its recorded owner is
/// foreign — attributed to a source absent from `sources` entirely, meaning
/// this invocation has no authority over it.
pub(crate) fn prune_stale_links<'a>(
    root: &Path,
    keep_names: impl IntoIterator<Item = &'a str>,
    dry_run: bool,
    registry: &ManagedArtifactRegistry,
    sources: &[ProjectionSource],
    owner: Option<ProjectionTargetId>,
) -> Result<ProjectionEvidence, ProjectionError> {
    let mut evidence = ProjectionEvidence::default();
    if !root.is_dir() {
        return Ok(evidence);
    }

    let installed_source_ids: Vec<&str> = sources.iter().map(|s| s.id.as_str()).collect();
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

        let owning_source_id = registry.owning_source_of(&path);
        let owning_source_is_foreign = owning_source_id
            .map(|src| !installed_source_ids.contains(&src))
            .unwrap_or(false);
        if owning_source_is_foreign {
            continue;
        }

        // Orphan-symlink path: a stale category row can go missing (e.g. a prior
        // run's GC bug) while the attribution row survives. Prove the symlink is
        // still this installed owner's own artifact by resolving its target into
        // the owner's root, rather than trusting `can_mutate`'s category-row test.
        let authorized_by_source_root = symlink
            && owning_source_id
                .and_then(|src_id| sources.iter().find(|source| source.id == src_id))
                .map(|source| is_symlink_target_inside_root(&path, &source.root))
                .unwrap_or(false);

        let authorized = is_ccync_owned_real_path(&path)
            || authorized_by_source_root
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
            record_evidence_path(&mut evidence, &path, ProjectionPathChange::Removed, owner);
        }
    }
    Ok(evidence)
}

/// Prune stale command-skill links under `root`, attributing each removed
/// path via `owner` (or `common` when `owner` is `None`). Identical
/// scan/filter/removal logic regardless of which target owns the root.
pub(crate) fn prune_stale_command_links(
    root: &Path,
    active_names: &[&str],
    dry_run: bool,
    registry: &ManagedArtifactRegistry,
    owner: Option<ProjectionTargetId>,
) -> Result<ProjectionEvidence, ProjectionError> {
    let mut evidence = ProjectionEvidence::default();
    if !root.is_dir() {
        return Ok(evidence);
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
        if is_symlink_or_junction(&path) {
            if !dry_run {
                remove_link(&path)?;
            }
            record_evidence_path(&mut evidence, &path, ProjectionPathChange::Removed, owner);
        }
    }
    Ok(evidence)
}

/// Prune legacy `ccync-`-prefixed directories under `root`, attributing each
/// removed directory via `owner` (or `common` when `owner` is `None`).
pub(crate) fn prune_legacy_ccync_prefixed_dirs(
    root: &Path,
    active_names: &[&str],
    dry_run: bool,
    registry: &ManagedArtifactRegistry,
    owner: Option<ProjectionTargetId>,
) -> Result<ProjectionEvidence, ProjectionError> {
    let mut evidence = ProjectionEvidence::default();
    if !root.is_dir() {
        return Ok(evidence);
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
        record_evidence_path(&mut evidence, &path, ProjectionPathChange::Removed, owner);
    }
    Ok(evidence)
}

/// Prune stale managed files under `root` matching `extension`, attributing
/// each removed path via `owner` (or `common` when `owner` is `None`). The
/// removal itself is inlined (its own `is_ccync_managed_file`/`can_mutate`
/// re-check is redundant here since the caller's loop already established
/// both).
pub(crate) fn prune_stale_managed_files(
    root: &Path,
    extension: &str,
    active_names: &[&str],
    dry_run: bool,
    registry: &ManagedArtifactRegistry,
    owner: Option<ProjectionTargetId>,
) -> Result<ProjectionEvidence, ProjectionError> {
    let mut evidence = ProjectionEvidence::default();
    if !root.is_dir() {
        return Ok(evidence);
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
        if !dry_run {
            fs::remove_file(&path)?;
        }
        record_evidence_path(&mut evidence, &path, ProjectionPathChange::Removed, owner);
    }
    Ok(evidence)
}

pub(crate) fn ensure_dir(path: &Path, dry_run: bool) -> Result<(), ProjectionError> {
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
) -> Result<Option<PathBuf>, ProjectionError> {
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
    evidence: &mut ProjectionEvidence,
    owner: Option<ProjectionTargetId>,
) -> Result<(), ProjectionError> {
    let dir = skills_root.join(name);
    if is_symlink_or_junction(&dir) {
        // Only a CCYNC-owned repo-link is removed; a user's own symlink (e.g. to an
        // external skill) is preserved.
        if is_ccync_repo_link(&dir, repo_root) {
            remove_link_if_present(&dir, dry_run, evidence, owner)?;
        }
    } else if dir.is_dir() && registry.is_managed(&dir.join("SKILL.md")) {
        if !dry_run {
            fs::remove_dir_all(&dir)?;
        }
        record_evidence_path(evidence, &dir, ProjectionPathChange::Removed, owner);
    }
    Ok(())
}

pub(crate) fn write_text(
    path: &Path,
    content: &str,
    dry_run: bool,
) -> Result<bool, ProjectionError> {
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
    evidence: &mut ProjectionEvidence,
    owner: Option<ProjectionTargetId>,
) -> Result<(), ProjectionError> {
    if !is_symlink_or_junction(path) {
        return Ok(());
    }
    if !dry_run {
        remove_link(path)?;
    }
    record_evidence_path(evidence, path, ProjectionPathChange::Removed, owner);
    Ok(())
}

pub(crate) fn remove_managed_file_if_present(
    path: &Path,
    dry_run: bool,
    evidence: &mut ProjectionEvidence,
    owner: Option<ProjectionTargetId>,
    registry: &ManagedArtifactRegistry,
) -> Result<(), ProjectionError> {
    if !is_ccync_managed_file(path) || !registry.can_mutate(path) {
        return Ok(());
    }
    if !dry_run {
        fs::remove_file(path)?;
    }
    record_evidence_path(evidence, path, ProjectionPathChange::Removed, owner);
    Ok(())
}

/// Ensure `link` points at `target`, returning `true` when the link is (or
/// remains) CCYNC-owned and safe to mark in the managed-artifact registry.
/// Returns `false` only when a genuine user-owned path was preserved
/// untouched — every other outcome (already correct, dry-run/replacement of
/// an existing link, adoption of a CCYNC-owned real path, or fresh creation)
/// returns `true`.
pub(crate) fn ensure_dir_link(
    target: &Path,
    link: &Path,
    replace: bool,
    dry_run: bool,
    report: &mut ProjectionReport,
) -> Result<bool, ProjectionError> {
    if same_path(link, target) {
        return Ok(true);
    }
    if is_symlink_or_junction(link) {
        if dry_run {
            report.created_links.push(link.to_path_buf());
            return Ok(true);
        }
        remove_dir_link(link)?;
    } else if link.exists() {
        // Adopt a CCYNC-owned real path (file→symlink migration) without requiring
        // --replace; preserve only genuine user-owned paths (no CCYNC content marker
        // and not a CCYNC-projected managed agent).
        if !replace && !is_ccync_owned_real_path(link) {
            report.warnings.push(ProjectionWarning {
                message: format!("preserved user-owned path: {}", link.display()),
            });
            return Ok(false);
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
    Ok(true)
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

/// Positive CCYNC-ownership test for an existing real path, used by both adopt
/// ([`ensure_dir_link`]) and prune ([`prune_stale_links`]).
///
/// True only when the path carries the CCYNC content header marker
/// ([`is_ccync_managed_path`]). Ownership is determined solely by content header,
/// not by agent frontmatter patterns; lockfile managed-set membership remains
/// the authority for ccync-owned agent mutations.
pub(crate) fn is_ccync_owned_real_path(path: &Path) -> bool {
    is_ccync_managed_path(path)
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

/// Prove a symlink's target lives inside `owner_root`, independent of the
/// managed-category lockfile rows — the authority for pruning an orphan
/// symlink whose attribution row survived a prior run but whose category row
/// did not. A live target is proven via [`is_ccync_repo_link`]'s
/// canonicalize-and-prefix check, reused with the owner's root standing in for
/// the repo root. A dangling target (the linked-to path no longer exists, so
/// canonicalize fails) is proven instead from the raw [`fs::read_link`] value:
/// it must be absolute and free of `..` (`ParentDir`) components — a relative
/// or traversal-bearing target cannot be trusted to resolve inside the root —
/// then checked against the root with a plain prefix test.
pub(crate) fn is_symlink_target_inside_root(path: &Path, owner_root: &Path) -> bool {
    if is_ccync_repo_link(path, owner_root) {
        return true;
    }
    let Ok(raw_target) = fs::read_link(path) else {
        return false;
    };
    if !raw_target.is_absolute()
        || raw_target
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return false;
    }
    // Deliberately not canonicalized: `fs::canonicalize` renders a Windows
    // `\\?\`-prefixed verbatim path, while `read_link`'s raw target and the
    // caller-supplied `owner_root` do not — canonicalizing either side alone
    // would make a genuinely in-root dangling target fail this prefix
    // comparison.
    raw_target.starts_with(owner_root)
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
) -> Result<(), ProjectionError> {
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
) -> Result<Option<Map<String, Value>>, ProjectionError> {
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
        Err(err) => Err(ProjectionError::from(err)),
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
) -> Result<(), ProjectionError> {
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
) -> Result<(), ProjectionError> {
    let rendered = serde_json::to_string_pretty(&Value::Object(data.clone()))
        .map_err(|err| ProjectionError::Message(err.to_string()))?;
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
) -> Result<(), ProjectionError> {
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
) -> Result<(), ProjectionError> {
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

// ── persisted-literal regression pin (behavior-freeze safety net) ─────────
//
// The renames applied across this crate must never touch the on-disk JSON
// keys/values written into `~/.ccync/build/lock.json` and sibling registry
// files. This test hardcodes every literal so a future find-replace that
// accidentally changes one of these strings/numbers fails loudly here,
// instead of silently breaking existing users' persisted lockfiles.
#[cfg(test)]
mod tests {
    use super::*;
    use ccync_foundation::platform::create_dir_link;
    use tempfile::TempDir;

    #[test]
    fn persisted_lock_literals_match_pre_refactor_baseline() {
        assert_eq!(PROJECTION_LOCK_KEY, "_ccyncProjection");
        assert_eq!(PROJECTION_LOCK_SCHEMA_VERSION, 1);
        assert_eq!(MANAGED_SOURCE_ATTRIBUTION, "sourceAttribution");
        assert_eq!(MANAGED_AGENT_PATHS, "agentProjectionPaths");
        assert_eq!(MANAGED_CODEX_AGENT_PATHS, "codexAgentProjectionPaths");
        assert_eq!(MANAGED_COMMAND_PATHS, "commandProjectionPaths");
        assert_eq!(MANAGED_DISCUSS_SKILL_PATHS, "discussSkillProjectionPaths");
        assert_eq!(MANAGED_LEGACY_PATHS, "legacyProjectionPaths");
        assert_eq!(MANAGED_SKILL_PATHS, "skillProjectionPaths");
    }

    // ── Evidence-attribution tests ──────────────────────────────────────────
    //
    // Each test proves the evidence-shaped function attributes its touched
    // path to exactly the caller-supplied `owner` (or `common` when `owner`
    // is `None`) — direct attribution at the call site, not post-hoc
    // bare-path inference.

    #[test]
    fn write_text_with_evidence_attributes_to_owner_and_to_common() {
        let tmp = TempDir::new().unwrap();

        let owned_path = tmp.path().join("owned.md");
        let evidence =
            write_text_with_evidence(&owned_path, "body", false, Some(ProjectionTargetId::Codex))
                .unwrap();
        assert!(evidence.common.is_empty());
        assert_eq!(
            evidence.by_target[&ProjectionTargetId::Codex].paths,
            vec![ProjectionPathResult {
                path: owned_path,
                change: ProjectionPathChange::Written,
            }]
        );

        let common_path = tmp.path().join("common.md");
        let evidence = write_text_with_evidence(&common_path, "body", false, None).unwrap();
        assert!(evidence.by_target.is_empty());
        assert_eq!(
            evidence.common,
            vec![ProjectionPathResult {
                path: common_path,
                change: ProjectionPathChange::Written,
            }]
        );
    }

    #[test]
    fn write_text_with_evidence_unchanged_write_records_nothing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("stable.md");
        write_text_with_evidence(&path, "body", false, Some(ProjectionTargetId::Claude)).unwrap();

        // Identical content on a second write is a no-op on disk (matching
        // `write_text`'s own dedupe) and must record no evidence either.
        let evidence =
            write_text_with_evidence(&path, "body", false, Some(ProjectionTargetId::Claude))
                .unwrap();
        assert_eq!(evidence, ProjectionEvidence::default());
    }

    #[test]
    fn write_json_map_with_evidence_attributes_to_owner() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("settings.json");
        let mut data = Map::new();
        data.insert("key".into(), Value::String("value".into()));

        let evidence =
            write_json_map_with_evidence(&path, &data, false, Some(ProjectionTargetId::GeminiCli))
                .unwrap();

        assert_eq!(
            evidence.by_target[&ProjectionTargetId::GeminiCli].paths,
            vec![ProjectionPathResult {
                path,
                change: ProjectionPathChange::Written,
            }]
        );
        assert!(evidence.common.is_empty());
    }

    #[test]
    fn write_json_map_with_evidence_attributes_to_common_when_no_owner() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("settings.json");
        let mut data = Map::new();
        data.insert("key".into(), Value::String("value".into()));

        let evidence = write_json_map_with_evidence(&path, &data, false, None).unwrap();

        assert!(evidence.by_target.is_empty());
        assert_eq!(
            evidence.common,
            vec![ProjectionPathResult {
                path,
                change: ProjectionPathChange::Written,
            }]
        );
    }

    #[test]
    fn write_json_map_with_evidence_unchanged_write_records_nothing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("settings.json");
        let mut data = Map::new();
        data.insert("key".into(), Value::String("value".into()));

        write_json_map_with_evidence(&path, &data, false, Some(ProjectionTargetId::Claude))
            .unwrap();

        let evidence =
            write_json_map_with_evidence(&path, &data, false, Some(ProjectionTargetId::Claude))
                .unwrap();
        assert_eq!(evidence, ProjectionEvidence::default());
    }

    #[test]
    fn prune_stale_links_with_evidence_attributes_removed_path_to_owner() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("agents");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("stale.toml"),
            format!("{CCYNC_MANAGED_FILE_HEADER}\nx = 1\n"),
        )
        .unwrap();

        let mut report = ProjectionReport::default();
        let registry = ManagedArtifactRegistry::load(&tmp.path().join("nolock.json"), &mut report);

        let evidence = prune_stale_links(
            &root,
            [],
            false,
            &registry,
            &[],
            Some(ProjectionTargetId::Claude),
        )
        .unwrap();

        assert!(!root.join("stale.toml").exists());
        assert_eq!(
            evidence.by_target[&ProjectionTargetId::Claude].paths,
            vec![ProjectionPathResult {
                path: root.join("stale.toml"),
                change: ProjectionPathChange::Removed,
            }]
        );
        assert!(evidence.common.is_empty());
    }

    #[test]
    fn prune_stale_links_with_evidence_attributes_removed_path_to_common_when_no_owner() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("agents");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("stale.toml"),
            format!("{CCYNC_MANAGED_FILE_HEADER}\nx = 1\n"),
        )
        .unwrap();

        let mut report = ProjectionReport::default();
        let registry = ManagedArtifactRegistry::load(&tmp.path().join("nolock.json"), &mut report);

        let evidence = prune_stale_links(&root, [], false, &registry, &[], None).unwrap();

        assert!(evidence.by_target.is_empty());
        assert_eq!(
            evidence.common,
            vec![ProjectionPathResult {
                path: root.join("stale.toml"),
                change: ProjectionPathChange::Removed,
            }]
        );
    }

    #[test]
    fn prune_stale_links_with_evidence_no_stale_entries_records_nothing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("agents");
        fs::create_dir_all(&root).unwrap();

        let mut report = ProjectionReport::default();
        let registry = ManagedArtifactRegistry::load(&tmp.path().join("nolock.json"), &mut report);

        let evidence = prune_stale_links(
            &root,
            [],
            false,
            &registry,
            &[],
            Some(ProjectionTargetId::Claude),
        )
        .unwrap();

        assert_eq!(evidence, ProjectionEvidence::default());
    }

    #[test]
    fn prune_stale_command_links_with_evidence_attributes_removed_link_to_owner() {
        let tmp = TempDir::new().unwrap();
        let commands_source = tmp.path().join("commands").join("stale-cmd");
        fs::create_dir_all(&commands_source).unwrap();
        let root = tmp.path().join("skills");
        fs::create_dir_all(&root).unwrap();
        let link = root.join("stale-cmd");
        create_dir_link(&commands_source, &link).unwrap();

        // `entry.file_type()?.is_dir()` (this function's own top-of-loop
        // gate) and `is_ccync_command_link`'s `is_symlink_or_junction`
        // requirement are mutually exclusive under `std::fs`'s own
        // semantics: `fs::read_dir` reports a reparse point (a junction, as
        // created here, or a symlink) as `is_symlink() == true` /
        // `is_dir() == false`, confirmed directly —
        // `fs::symlink_metadata(&link).unwrap().file_type().is_dir()` is
        // `false` for this junction. No entry can ever satisfy both gates at
        // once, so this function's removal branch is a structural no-op on
        // every platform — pre-existing legacy behavior this task mirrors
        // exactly rather than "fixes" (out of scope per this task's own
        // "same operations, same effects on disk" boundary). This test pins
        // that the evidence-shaped sibling reproduces the identical no-op,
        // not a silent behavior change.
        assert!(
            !fs::symlink_metadata(&link).unwrap().file_type().is_dir(),
            "test premise: a junction must read as is_dir()==false via symlink_metadata, \
             matching entry.file_type() during fs::read_dir"
        );

        let mut report = ProjectionReport::default();
        let registry = ManagedArtifactRegistry::load(&tmp.path().join("nolock.json"), &mut report);

        let evidence = prune_stale_command_links(
            &root,
            &[],
            false,
            &registry,
            Some(ProjectionTargetId::Copilot),
        )
        .unwrap();

        assert_eq!(evidence, ProjectionEvidence::default());
        assert!(
            link.exists(),
            "a junction can never satisfy this function's own is_dir() gate, so it is a \
             structural no-op on every platform"
        );
    }

    #[test]
    fn prune_legacy_ccync_prefixed_dirs_with_evidence_attributes_removed_dir_to_common() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("ccync-old")).unwrap();

        let mut report = ProjectionReport::default();
        let registry = ManagedArtifactRegistry::load(&tmp.path().join("nolock.json"), &mut report);

        let evidence =
            prune_legacy_ccync_prefixed_dirs(&root, &[], false, &registry, None).unwrap();

        assert!(!root.join("ccync-old").exists());
        assert!(evidence.by_target.is_empty());
        assert_eq!(
            evidence.common,
            vec![ProjectionPathResult {
                path: root.join("ccync-old"),
                change: ProjectionPathChange::Removed,
            }]
        );
    }

    #[test]
    fn prune_legacy_ccync_prefixed_dirs_with_evidence_attributes_removed_dir_to_owner() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("ccync-old")).unwrap();

        let mut report = ProjectionReport::default();
        let registry = ManagedArtifactRegistry::load(&tmp.path().join("nolock.json"), &mut report);

        let evidence = prune_legacy_ccync_prefixed_dirs(
            &root,
            &[],
            false,
            &registry,
            Some(ProjectionTargetId::Codex),
        )
        .unwrap();

        assert!(!root.join("ccync-old").exists());
        assert_eq!(
            evidence.by_target[&ProjectionTargetId::Codex].paths,
            vec![ProjectionPathResult {
                path: root.join("ccync-old"),
                change: ProjectionPathChange::Removed,
            }]
        );
        assert!(evidence.common.is_empty());
    }

    #[test]
    fn prune_stale_managed_files_with_evidence_attributes_removed_file_to_owner() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("commands");
        fs::create_dir_all(&root).unwrap();
        let stale = root.join("stale.toml");
        fs::write(&stale, format!("{CCYNC_MANAGED_FILE_HEADER}\nx = 1\n")).unwrap();

        let mut report = ProjectionReport::default();
        let registry = ManagedArtifactRegistry::load(&tmp.path().join("nolock.json"), &mut report);

        let evidence = prune_stale_managed_files(
            &root,
            "toml",
            &[],
            false,
            &registry,
            Some(ProjectionTargetId::GeminiCli),
        )
        .unwrap();

        assert!(!stale.exists());
        assert_eq!(
            evidence.by_target[&ProjectionTargetId::GeminiCli].paths,
            vec![ProjectionPathResult {
                path: stale,
                change: ProjectionPathChange::Removed,
            }]
        );
        assert!(evidence.common.is_empty());
    }

    #[test]
    fn prune_stale_managed_files_with_evidence_attributes_removed_file_to_common_when_no_owner() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("commands");
        fs::create_dir_all(&root).unwrap();
        let stale = root.join("stale.toml");
        fs::write(&stale, format!("{CCYNC_MANAGED_FILE_HEADER}\nx = 1\n")).unwrap();

        let mut report = ProjectionReport::default();
        let registry = ManagedArtifactRegistry::load(&tmp.path().join("nolock.json"), &mut report);

        let evidence =
            prune_stale_managed_files(&root, "toml", &[], false, &registry, None).unwrap();

        assert!(!stale.exists());
        assert!(evidence.by_target.is_empty());
        assert_eq!(
            evidence.common,
            vec![ProjectionPathResult {
                path: stale,
                change: ProjectionPathChange::Removed,
            }]
        );
    }

    // ── prune fault payload (locked file) ──────────────────────────────────
    //
    // Forces a genuine I/O fault (the target file held open without
    // delete-sharing, so `fs::remove_file` fails) and asserts the removal
    // does not partially succeed.
    #[test]
    fn prune_stale_managed_files_locked_file_faults_without_partial_removal() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("commands");
        fs::create_dir_all(&root).unwrap();
        let stale = root.join("stale.toml");
        fs::write(&stale, format!("{CCYNC_MANAGED_FILE_HEADER}\nx = 1\n")).unwrap();

        // Hold the file open sharing FILE_SHARE_READ|FILE_SHARE_WRITE but NOT
        // FILE_SHARE_DELETE (`std::fs::OpenOptions`'s own Windows default
        // grants all three, which would let `fs::remove_file` succeed
        // anyway) — both prune functions still read the file's own
        // CCYNC-managed-header content successfully (read sharing is
        // granted), but the later `fs::remove_file` fails with a sharing
        // violation, identically on both call paths below.
        #[cfg(windows)]
        let _locked = {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_SHARE_READ: u32 = 0x0000_0001;
            const FILE_SHARE_WRITE: u32 = 0x0000_0002;
            fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .open(&stale)
                .unwrap()
        };
        #[cfg(not(windows))]
        let _locked = fs::OpenOptions::new().read(true).open(&stale).unwrap();

        let mut report = ProjectionReport::default();
        let registry = ManagedArtifactRegistry::load(&tmp.path().join("nolock.json"), &mut report);

        prune_stale_managed_files(
            &root,
            "toml",
            &[],
            false,
            &registry,
            Some(ProjectionTargetId::GeminiCli),
        )
        .expect_err("locked file must make the prune fail");

        assert!(
            stale.exists(),
            "the removal must not have partially succeeded"
        );
    }
}
