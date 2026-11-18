//! Projection support: health check, frontmatter rendering, file/list/link helpers.

use super::*;
use ccync_foundation::health::{DoctorFinding, HealthCheck};
use ccync_foundation::json_util::read_json_map;
use ccync_foundation::platform::{
    atomic_write_bytes, classify_link, create_dir_link, create_symlink_dir, is_symlink_or_junction,
    remove_dir_link, AtomicWriteError, LinkShape,
};
use ccync_foundation::provider::ProviderSourceKey;
use ccync_foundation::target::ProjectionTargetId;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Physical root, legacy category, and direct category for one agent target.
/// `None` means the target has no agent surface.
fn direct_agent_surface(
    target: ProjectionTargetId,
    user_home: &Path,
) -> Option<(PathBuf, &'static str, &'static str)> {
    // `legacy_category` holds rows written before direct ownership existed.
    // A successful direct write to one of its paths moves that row to `category`.
    Some(match target {
        ProjectionTargetId::Claude => (
            user_home.join(".claude/agents"),
            MANAGED_AGENT_PATHS,
            MANAGED_DIRECT_AGENT_PATHS,
        ),
        ProjectionTargetId::Codex => (
            user_home.join(".codex/agents"),
            MANAGED_CODEX_AGENT_PATHS,
            MANAGED_DIRECT_AGENT_PATHS,
        ),
        ProjectionTargetId::Copilot => (
            user_home.join(".copilot/agents"),
            MANAGED_AGENT_PATHS,
            MANAGED_DIRECT_AGENT_PATHS,
        ),
        ProjectionTargetId::Opencode => (
            user_home.join(".config/opencode/agents"),
            MANAGED_OPENCODE_AGENT_PATHS,
            MANAGED_DIRECT_AGENT_PATHS,
        ),
        ProjectionTargetId::AgyIde => return None,
        ProjectionTargetId::AgyCli | ProjectionTargetId::AgyGui => (
            user_home.join(".gemini/config/agents"),
            MANAGED_AGENT_PATHS,
            MANAGED_DIRECT_AGENT_PATHS,
        ),
    })
}

fn push_agent_warning(
    evidence: &mut ProjectionEvidence,
    target: ProjectionTargetId,
    message: String,
) {
    evidence
        .by_target
        .entry(target)
        .or_default()
        .warnings
        .push(message);
}

/// Render the bytes `target` receives for `input`, recording field warnings
/// against that target. `None` means the target skips the input or has no surface.
fn render_direct_agent(
    target: ProjectionTargetId,
    input: &DirectAgentInput,
    dest: &Path,
    evidence: &mut ProjectionEvidence,
) -> Option<String> {
    let rendered = match target {
        ProjectionTargetId::Codex => {
            let Some(meta) = parse_agent_meta(&input.content) else {
                push_agent_warning(
                    evidence,
                    target,
                    format!(
                        "agent '{}' is unsupported on Codex: missing valid name and frontmatter",
                        input.name
                    ),
                );
                return None;
            };
            if input
                .content
                .lines()
                .any(|line| line.trim_start().starts_with("tools:"))
            {
                push_agent_warning(
                    evidence,
                    target,
                    format!(
                        "agent '{}' tools restriction is unsupported on Codex",
                        input.name
                    ),
                );
            }
            if input
                .content
                .lines()
                .any(|line| line.trim_start().starts_with("color:"))
            {
                push_agent_warning(
                    evidence,
                    target,
                    format!("agent '{}' color is unsupported on Codex", input.name),
                );
            }
            let mut rendered = codex_agent::serialize_codex_toml(
                &meta,
                meta.model.as_deref().unwrap_or(""),
                meta.reasoning_effort.as_deref(),
                tool_map::invocation_mode_to_codex_sandbox(tool_map::InvocationMode::Consult),
            );
            // Keep sections this writer does not own, such as user `[features]`.
            if let Ok(existing) = fs::read_to_string(dest) {
                let unknown_sections = existing
                    .lines()
                    .scan(false, |in_unknown, line| {
                        if line.starts_with('[') {
                            *in_unknown = line.trim() != "[agent]";
                        }
                        Some((*in_unknown, line))
                    })
                    .filter_map(|(keep, line)| keep.then_some(line))
                    .collect::<Vec<_>>();
                if !unknown_sections.is_empty() {
                    rendered.push('\n');
                    rendered.push_str(&unknown_sections.join("\n"));
                    rendered.push('\n');
                }
            }
            rendered
        }
        ProjectionTargetId::Opencode => {
            let meta = parse_frontmatter(&input.content);
            if input
                .content
                .lines()
                .any(|line| line.trim_start().starts_with("model_reasoning_effort:"))
            {
                push_agent_warning(
                    evidence,
                    target,
                    format!(
                        "agent '{}' model_reasoning_effort is unsupported on OpenCode",
                        input.name
                    ),
                );
            }
            let supported = ["read", "search", "list", "edit", "execute"];
            for tool in &meta.tools {
                if !supported.contains(&tool.as_str()) {
                    push_agent_warning(
                        evidence,
                        target,
                        format!(
                            "agent '{}' tool '{}' is unsupported on OpenCode",
                            input.name, tool
                        ),
                    );
                }
            }
            render_opencode_agent(&input.name, &meta, &strip_frontmatter(&input.content))
        }
        ProjectionTargetId::Claude
        | ProjectionTargetId::Copilot
        | ProjectionTargetId::AgyCli
        | ProjectionTargetId::AgyGui => input.content.clone(),
        ProjectionTargetId::AgyIde => return None,
    };
    Some(rendered)
}

/// One physical agent destination, the single definition written to it, and
/// every target that receives that definition.
struct DirectAgentDestination {
    dest: PathBuf,
    source_id: String,
    output: String,
    targets: Vec<ProjectionTargetId>,
    /// Set when another assignment names this path with a different source or
    /// different rendered bytes. A conflicting destination is never written.
    conflict: Option<String>,
}

/// Publish every assigned agent definition in one batch. All renders and
/// ownership checks happen before the first write, so a conflict blocks its
/// destination without partial output. Shared AGY destinations are written
/// once. A failed write keeps the previous bytes and ownership row, and the
/// registry persists once for the whole batch.
pub(crate) fn publish_direct_agent_plan(
    publication: &DirectAgentPublication,
    user_home: &Path,
    lockfile_path: &Path,
    dry_run: bool,
) -> ProjectionEvidence {
    let mut evidence = ProjectionEvidence::default();
    let mut report = ProjectionReport::default();
    let mut registry = ManagedArtifactRegistry::load(lockfile_path, &mut report);
    let mut failed_sources = BTreeSet::new();

    let mut destinations: Vec<DirectAgentDestination> = Vec::new();
    let mut slot_by_path: BTreeMap<String, usize> = BTreeMap::new();
    for assignment in &publication.assignments {
        let target = assignment.target;
        let input = &assignment.input;
        let Some((root, _, _)) = direct_agent_surface(target, user_home) else {
            evidence.by_target.entry(target).or_default().failure =
                Some(ProjectionExpectedFailure {
                    path: None,
                    reason: "agent definitions are unsupported on agy-ide".into(),
                });
            continue;
        };
        let source_valid = ProviderSourceKey::new(input.source_id.clone()).is_ok();
        let name_valid = !input.name.is_empty()
            && input
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !source_valid || !name_valid {
            failed_sources.insert(input.source_id.clone());
            evidence.by_target.entry(target).or_default().failure =
                Some(ProjectionExpectedFailure {
                    path: None,
                    reason: format!("invalid direct agent input: {}", input.name),
                });
            continue;
        }
        let filename = match target {
            ProjectionTargetId::Codex => format!("{}.toml", input.name),
            ProjectionTargetId::Opencode | ProjectionTargetId::Claude => {
                format!("{}.md", input.name)
            }
            ProjectionTargetId::Copilot => format!("{}.agent.md", input.name),
            _ => format!("{}.md", input.name),
        };
        let dest = root.join(filename);
        let Some(output) = render_direct_agent(target, input, &dest, &mut evidence) else {
            continue;
        };
        let key = normalize_managed_artifact_path(&dest);
        match slot_by_path.get(&key) {
            Some(&slot) => {
                let destination = &mut destinations[slot];
                if destination.source_id != input.source_id || destination.output != output {
                    let reason = if destination.source_id == input.source_id {
                        format!(
                            "incompatible rendered contents for shared agent destination: {}",
                            dest.display()
                        )
                    } else {
                        format!(
                            "agent destination is claimed by sources '{}' and '{}'",
                            destination.source_id, input.source_id
                        )
                    };
                    failed_sources.insert(destination.source_id.clone());
                    failed_sources.insert(input.source_id.clone());
                    destination.conflict.get_or_insert(reason);
                }
                push_unique_target(&mut destination.targets, target);
            }
            None => {
                slot_by_path.insert(key, destinations.len());
                destinations.push(DirectAgentDestination {
                    dest,
                    source_id: input.source_id.clone(),
                    output,
                    targets: vec![target],
                    conflict: None,
                });
            }
        }
    }

    for destination in &destinations {
        let dest = &destination.dest;
        if let Some(reason) = &destination.conflict {
            fail_targets(&mut evidence, &destination.targets, dest, reason.clone());
            continue;
        }
        let owner = registry.owning_source_of(dest);
        let destination_exists = dest.exists();
        let managed = registry.is_managed(dest);
        if destination_exists && (owner != Some(destination.source_id.as_str()) || !managed) {
            for target in &destination.targets {
                push_agent_warning(
                    &mut evidence,
                    *target,
                    format!("preserved unowned agent destination: {}", dest.display()),
                );
            }
            continue;
        }
        if dry_run {
            continue;
        }
        if let Err(error) = write_text_atomically(dest, &destination.output, false) {
            failed_sources.insert(destination.source_id.clone());
            fail_targets(&mut evidence, &destination.targets, dest, error.to_string());
            continue;
        }
        for target in &destination.targets {
            if let Some((_, legacy_category, category)) = direct_agent_surface(*target, user_home) {
                registry.transfer_with_source(
                    legacy_category,
                    category,
                    dest,
                    &destination.source_id,
                );
            }
            evidence
                .by_target
                .entry(*target)
                .or_default()
                .paths
                .push(ProjectionPathResult {
                    path: dest.clone(),
                    change: ProjectionPathChange::Written,
                });
        }
    }
    if !dry_run {
        let sources = publication
            .assignments
            .iter()
            .map(|assignment| assignment.input.source_id.as_str())
            .filter(|source| !failed_sources.contains(*source))
            .collect::<Vec<_>>();
        if let Err(error) = registry.persist(&sources, false, &mut report) {
            evidence.run_warnings.push(error.to_string());
        }
    }
    merge_report_as(&mut evidence, &report, None);
    evidence
}

/// One physical command destination, the single content written to it, and
/// every target that receives that content.
struct DirectCommandDestinationPlan {
    dest: PathBuf,
    kind: DirectArtifactKind,
    source_id: String,
    content: String,
    targets: Vec<ProjectionTargetId>,
    /// Set when another assignment names this path with a different source or
    /// different content. A conflicting destination is never written.
    conflict: Option<String>,
}

/// Physical path and registry kind for one prepared command on `target`, or
/// `None` when the target has no surface for that destination form.
fn direct_command_surface(
    target: ProjectionTargetId,
    destination: DirectCommandDestination,
    user_home: &Path,
    alias: &str,
) -> Option<(PathBuf, DirectArtifactKind)> {
    match (target, destination) {
        (ProjectionTargetId::Claude, DirectCommandDestination::NativeCommand) => Some((
            user_home
                .join(".claude/commands")
                .join(format!("{alias}.md")),
            DirectArtifactKind::Command,
        )),
        (ProjectionTargetId::Opencode, DirectCommandDestination::NativeCommand) => Some((
            user_home
                .join(".config/opencode/commands")
                .join(format!("{alias}.md")),
            DirectArtifactKind::Command,
        )),
        (ProjectionTargetId::Copilot, DirectCommandDestination::CommandSkill) => Some((
            user_home
                .join(".copilot/skills")
                .join(alias)
                .join("SKILL.md"),
            DirectArtifactKind::Skill,
        )),
        (ProjectionTargetId::Codex, DirectCommandDestination::CommandSkill) => Some((
            user_home
                .join(".agents/skills")
                .join(alias)
                .join("SKILL.md"),
            DirectArtifactKind::Skill,
        )),
        _ => None,
    }
}

/// Publish every prepared command in one batch. All destinations are checked
/// before the first write, so a conflict blocks its destination without partial
/// output. A write that fails keeps the previous bytes and ownership row, and
/// the registry persists once for the whole batch.
///
/// A legacy `commandProjectionPaths` row on a `SKILL.md` or native command
/// authorizes replacing that one file only. Sibling files in the same
/// directory are never read, moved, or removed here.
pub(crate) fn publish_direct_command_plan(
    publication: &DirectCommandPublication,
    user_home: &Path,
    lockfile_path: &Path,
    dry_run: bool,
) -> ProjectionEvidence {
    let mut evidence = ProjectionEvidence::default();
    let mut report = ProjectionReport::default();
    let mut registry = ManagedArtifactRegistry::load(lockfile_path, &mut report);
    let mut failed_sources = BTreeSet::new();

    let mut destinations: Vec<DirectCommandDestinationPlan> = Vec::new();
    let mut slot_by_path: BTreeMap<String, usize> = BTreeMap::new();
    for assignment in &publication.assignments {
        let target = assignment.target;
        let alias = assignment.alias.as_str();
        let source_valid = ProviderSourceKey::new(assignment.source_id.clone()).is_ok();
        let alias_valid = !alias.is_empty()
            && alias
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !source_valid || !alias_valid {
            failed_sources.insert(assignment.source_id.clone());
            evidence.by_target.entry(target).or_default().failure =
                Some(ProjectionExpectedFailure {
                    path: None,
                    reason: format!("invalid direct command input: {alias}"),
                });
            continue;
        }
        let Some((dest, kind)) =
            direct_command_surface(target, assignment.destination, user_home, alias)
        else {
            evidence.by_target.entry(target).or_default().failure =
                Some(ProjectionExpectedFailure {
                    path: None,
                    reason: format!(
                        "command '{alias}' was not published: {:?} is unsupported on {target:?}",
                        assignment.destination
                    ),
                });
            continue;
        };
        let key = normalize_managed_artifact_path(&dest);
        match slot_by_path.get(&key) {
            Some(&slot) => {
                let destination = &mut destinations[slot];
                if destination.source_id != assignment.source_id
                    || destination.content != assignment.content
                {
                    let reason = if destination.source_id == assignment.source_id {
                        format!(
                            "incompatible prepared contents for shared command destination: {}",
                            dest.display()
                        )
                    } else {
                        format!(
                            "command destination is claimed by sources '{}' and '{}'",
                            destination.source_id, assignment.source_id
                        )
                    };
                    failed_sources.insert(destination.source_id.clone());
                    failed_sources.insert(assignment.source_id.clone());
                    destination.conflict.get_or_insert(reason);
                }
                push_unique_target(&mut destination.targets, target);
            }
            None => {
                slot_by_path.insert(key, destinations.len());
                destinations.push(DirectCommandDestinationPlan {
                    dest,
                    kind,
                    source_id: assignment.source_id.clone(),
                    content: assignment.content.clone(),
                    targets: vec![target],
                    conflict: None,
                });
            }
        }
    }

    for destination in &destinations {
        let dest = &destination.dest;
        if let Some(reason) = &destination.conflict {
            fail_targets(&mut evidence, &destination.targets, dest, reason.clone());
            continue;
        }
        // Writing through a linked directory would land outside the projection root.
        if dest.parent().is_some_and(is_symlink_or_junction) {
            for target in &destination.targets {
                push_agent_warning(
                    &mut evidence,
                    *target,
                    format!("preserved linked command destination: {}", dest.display()),
                );
            }
            continue;
        }
        let key = normalize_managed_artifact_path(dest);
        let owner = registry.owning_source_of(dest);
        let legacy_owned = registry
            .prior_paths(MANAGED_COMMAND_PATHS)
            .any(|path| path == key);
        let owned = match owner {
            Some(owner) => owner == destination.source_id && registry.is_managed(dest),
            None => legacy_owned,
        };
        if dest.exists() && !owned {
            for target in &destination.targets {
                push_agent_warning(
                    &mut evidence,
                    *target,
                    format!("preserved unowned command destination: {}", dest.display()),
                );
            }
            continue;
        }
        if dry_run {
            continue;
        }
        match write_text_atomically(dest, &destination.content, false) {
            Err(error) => {
                failed_sources.insert(destination.source_id.clone());
                fail_targets(&mut evidence, &destination.targets, dest, error.to_string());
            }
            Ok(changed) => {
                registry.transfer_with_source(
                    MANAGED_COMMAND_PATHS,
                    destination.kind.category(),
                    dest,
                    &destination.source_id,
                );
                if changed {
                    for target in &destination.targets {
                        record_evidence_path(
                            &mut evidence,
                            dest,
                            ProjectionPathChange::Written,
                            Some(*target),
                        );
                    }
                }
            }
        }
    }
    if !dry_run {
        let sources = publication
            .assignments
            .iter()
            .map(|assignment| assignment.source_id.as_str())
            .filter(|source| !failed_sources.contains(*source))
            .collect::<Vec<_>>();
        if let Err(error) = registry.persist(&sources, false, &mut report) {
            evidence.run_warnings.push(error.to_string());
        }
    }
    merge_report_as(&mut evidence, &report, None);
    evidence
}

/// Construct deterministic target aliases while honoring persisted aliases.
pub(crate) fn plan_provider_aliases(
    target: ProjectionTargetId,
    inventory: &ProviderProjectionInventory,
) -> Result<Vec<TargetAlias>, ProjectionError> {
    validate_native_provider_namespaces(target, &inventory.identities)?;
    let mut counts = BTreeMap::<(String, String), usize>::new();
    for identity in &inventory.identities {
        identity
            .validate()
            .map_err(|error| ProjectionError::Message(error.to_string()))?;
        *counts
            .entry((
                format!("{:?}", identity.kind),
                identity.component_id.clone(),
            ))
            .or_default() += 1;
    }
    let persisted = inventory
        .aliases
        .iter()
        .filter(|alias| alias.target == target)
        .map(|alias| (alias.identity.clone(), alias.alias.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut used = BTreeSet::new();
    let mut output = Vec::with_capacity(inventory.identities.len());
    for identity in &inventory.identities {
        let key = (
            format!("{:?}", identity.kind),
            identity.component_id.clone(),
        );
        // Persisted aliases win even when this identity is currently unique.
        // This keeps its public invocation stable if an overlapping provider
        // disappears and is later selected again.
        let alias = if let Some(value) = persisted.get(identity) {
            value.clone()
        } else if counts[&key] == 1 {
            identity.component_id.clone()
        } else {
            let raw = format!(
                "{}-{}-{}",
                identity.plugin_name, identity.source, identity.component_id
            );
            let mut normalized = raw
                .chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() || ch == '-' {
                        ch.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .collect::<String>();
            while normalized.contains("--") {
                normalized = normalized.replace("--", "-");
            }
            normalized = normalized.trim_matches('-').to_string();
            if normalized.is_empty() {
                return Err(ProjectionError::Message(
                    "provider alias has no valid target name".into(),
                ));
            }
            normalized
        };
        if !used.insert(alias.clone()) {
            return Err(ProjectionError::Message(format!(
                "provider alias collision for '{alias}'"
            )));
        }
        output.push(
            TargetAlias::new(target, identity.clone(), alias)
                .map_err(|error| ProjectionError::Message(error.to_string()))?,
        );
    }
    // Keep historical entries in the returned map. Callers persist this full
    // target map so an identity that is temporarily unselected keeps its name.
    for alias in inventory
        .aliases
        .iter()
        .filter(|alias| alias.target == target)
    {
        if output
            .iter()
            .any(|current| current.identity == alias.identity)
        {
            continue;
        }
        if !used.insert(alias.alias.clone()) {
            return Err(ProjectionError::Message(format!(
                "provider alias collision for '{}'",
                alias.alias
            )));
        }
        output.push(alias.clone());
    }
    Ok(output)
}

/// Native plugin loaders have one plugin-name namespace and cannot safely
/// represent same-named plugins from distinct provider sources.
pub(crate) fn validate_native_provider_namespaces(
    target: ProjectionTargetId,
    identities: &[ComponentIdentity],
) -> Result<(), ProjectionError> {
    // Claude's marketplace loader keys native installs by plugin name. The
    // other current publishers use provider-scoped roots and can coexist.
    if target != ProjectionTargetId::Claude {
        return Ok(());
    }
    let mut owners = BTreeMap::<&str, &ComponentIdentity>::new();
    for identity in identities {
        if let Some(previous) = owners.get(identity.plugin_name.as_str()) {
            if previous.source != identity.source {
                return Err(ProjectionError::Message(format!(
                    "native provider namespace conflict on {target}: plugin '{}' is supplied by sources '{}' and '{}'",
                    identity.plugin_name, previous.source, identity.source
                )));
            }
        } else {
            owners.insert(&identity.plugin_name, identity);
        }
    }
    Ok(())
}

/// Resolve a generated reference only when the original component name is unique.
pub(crate) fn resolve_provider_reference(
    name: &str,
    identities: &[ComponentIdentity],
    aliases: &[TargetAlias],
    target: ProjectionTargetId,
) -> Result<Option<String>, ProjectionError> {
    let matches = identities
        .iter()
        .filter(|identity| identity.component_id == name)
        .collect::<Vec<_>>();
    if matches.len() > 1 {
        return Err(ProjectionError::Message(format!(
            "ambiguous provider reference '{name}'"
        )));
    }
    let Some(identity) = matches.first() else {
        return Ok(None);
    };
    Ok(aliases
        .iter()
        .find(|alias| alias.target == target && &alias.identity == *identity)
        .map(|alias| alias.alias.clone()))
}

/// Rewrite a generated command reference through the alias map. Only explicit
/// slash-command tokens are rewritten. Opaque body text is preserved, except
/// that an ambiguous token that appears as a generated command reference is
/// rejected by `resolve_provider_reference`.
///
/// A `/` starts a command token only at the start of the body or after
/// whitespace, `(`, `[`, a backtick, or a quote. A `/` after any other
/// character is a path separator, and a token followed by `/` is the head of
/// a path. Both are copied unchanged, so URLs and file paths keep their names.
pub(crate) fn rewrite_generated_provider_references(
    body: &str,
    identities: &[ComponentIdentity],
    aliases: &[TargetAlias],
    target: ProjectionTargetId,
) -> Result<String, ProjectionError> {
    let starts_reference = |prev: Option<char>| {
        prev.is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '[' | '`' | '"' | '\''))
    };
    let mut output = String::with_capacity(body.len());
    let mut token = String::new();
    let mut in_reference = false;
    let mut prev = None;
    for ch in body.chars() {
        if in_reference && (ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')) {
            token.push(ch);
            prev = Some(ch);
            continue;
        }
        if in_reference {
            if ch == '/' {
                output.push_str(&token);
            } else {
                let replacement = resolve_provider_reference(&token, identities, aliases, target)?;
                output.push_str(&replacement.unwrap_or_else(|| token.clone()));
            }
            token.clear();
            in_reference = false;
        }
        output.push(ch);
        if ch == '/' && starts_reference(prev) {
            in_reference = true;
        }
        prev = Some(ch);
    }
    if in_reference {
        let replacement = resolve_provider_reference(&token, identities, aliases, target)?;
        output.push_str(&replacement.unwrap_or(token));
    }
    Ok(output)
}

/// Publish one complete package per source into a target-specific discovery root.
pub(crate) fn publish_provider_surfaces(
    target: ProjectionTargetId,
    packages: &[ProviderSurfacePackage],
    user_home: &Path,
    lockfile_path: &Path,
    dry_run: bool,
) -> ProjectionEvidence {
    let mut evidence = ProjectionEvidence::default();
    let target_name = match target {
        ProjectionTargetId::Opencode => "opencode",
        ProjectionTargetId::Copilot => "copilot",
        ProjectionTargetId::AgyCli => "agy-cli",
        ProjectionTargetId::AgyIde => "agy-ide",
        ProjectionTargetId::AgyGui => "agy-gui",
        _ => {
            evidence
                .run_warnings
                .push(format!("unsupported provider surface target: {target}"));
            return evidence;
        }
    };
    let surface = match target {
        ProjectionTargetId::Opencode => user_home.join(".config/opencode/skills"),
        ProjectionTargetId::Copilot => user_home.join(".copilot/installed-plugins/ccync-copilot"),
        ProjectionTargetId::AgyCli => user_home.join(".gemini/antigravity-cli/plugins"),
        ProjectionTargetId::AgyIde => user_home.join(".gemini/antigravity-ide/plugins"),
        ProjectionTargetId::AgyGui => user_home.join(".gemini/config/plugins"),
        _ => unreachable!(),
    };
    let mut report = ProjectionReport::default();
    let mut registry = ManagedArtifactRegistry::load(lockfile_path, &mut report);
    let category = match target {
        ProjectionTargetId::Opencode => MANAGED_OPENCODE_AGENT_PATHS,
        ProjectionTargetId::Copilot => MANAGED_AGENT_PATHS,
        _ => MANAGED_AGY_ROOT_PATHS,
    };
    let mut failed = BTreeSet::new();
    for package in packages {
        if let Err(error) = ProviderSourceKey::new(package.source_id.clone()) {
            evidence.by_target.entry(target).or_default().failure =
                Some(ProjectionExpectedFailure {
                    path: Some(package.root.clone()),
                    reason: error.to_string(),
                });
            continue;
        }
        let path_key = if target == ProjectionTargetId::Opencode {
            &package.alias
        } else {
            &package.source_id
        };
        if let Err(error) = ProviderSourceKey::new(path_key.clone()) {
            evidence.by_target.entry(target).or_default().failure =
                Some(ProjectionExpectedFailure {
                    path: Some(package.root.clone()),
                    reason: error.to_string(),
                });
            continue;
        }
        let dest = surface.join(path_key);
        let payload = if target == ProjectionTargetId::Opencode {
            package.root.join("skills").join(&package.component_id)
        } else {
            package.root.clone()
        };
        let valid = package.root.is_dir()
            && match target {
                ProjectionTargetId::Opencode => payload.join("SKILL.md").is_file(),
                _ => {
                    package.root.join("plugin.json").is_file()
                        || package.root.join(".claude-plugin/plugin.json").is_file()
                }
            };
        if !valid {
            failed.insert(package.source_id.clone());
            evidence.by_target.entry(target).or_default().failure =
                Some(ProjectionExpectedFailure {
                    path: Some(package.root.clone()),
                    reason: format!("provider package has invalid {target_name} discovery shape"),
                });
            continue;
        }
        let prior_owner = registry.owning_source_of(&dest);
        if dest.exists() && prior_owner != Some(package.source_id.as_str()) {
            evidence.by_target.entry(target).or_default().skipped = Some(format!(
                "preserved unowned provider path: {}",
                dest.display()
            ));
            continue;
        }
        if dry_run {
            continue;
        }
        if let Some(parent) = dest.parent() {
            if let Err(error) = fs::create_dir_all(parent) {
                failed.insert(package.source_id.clone());
                evidence.by_target.entry(target).or_default().failure =
                    Some(ProjectionExpectedFailure {
                        path: Some(parent.to_path_buf()),
                        reason: error.to_string(),
                    });
                continue;
            }
        }
        sweep_staging_orphans(&dest);
        match publish_staged_tree_copy(&payload, &dest) {
            Ok(true) => {
                registry.mark_with_source(category, &dest, &package.source_id);
                evidence
                    .by_target
                    .entry(target)
                    .or_default()
                    .paths
                    .push(ProjectionPathResult {
                        path: dest,
                        change: ProjectionPathChange::Written,
                    });
            }
            Ok(false) => {
                registry.mark_with_source(category, &dest, &package.source_id);
            }
            Err(error) => {
                failed.insert(package.source_id.clone());
                evidence.by_target.entry(target).or_default().failure =
                    Some(ProjectionExpectedFailure {
                        path: Some(dest),
                        reason: error.to_string(),
                    });
            }
        }
    }
    if !dry_run {
        if let Err(error) = registry.persist(
            &packages
                .iter()
                .filter(|p| !failed.contains(&p.source_id))
                .map(|p| p.source_id.as_str())
                .collect::<Vec<_>>(),
            false,
            &mut report,
        ) {
            evidence.run_warnings.push(error.to_string());
        }
    }
    merge_report_as(&mut evidence, &report, Some(target));
    evidence
}

/// One physical destination and the package that owns it for this run.
struct DirectSkillDestination<'a> {
    dest: PathBuf,
    package: &'a DirectSkillPackage,
    targets: Vec<ProjectionTargetId>,
}

/// Publish each assigned package/root pair. Assignments that name the same
/// destination with the same package collapse into one write for the union of
/// their targets. Ownership is checked per destination, and only a successful
/// write moves its row, so a failed replacement keeps the previous bytes and
/// proof. Stale rows for the whole run stay with the engine.
pub(crate) fn publish_direct_skill_plan(
    publication: &DirectSkillPublication,
    lockfile_path: &Path,
    dry_run: bool,
) -> ProjectionEvidence {
    let mut evidence = ProjectionEvidence::default();
    let mut report = ProjectionReport::default();
    let mut registry = ManagedArtifactRegistry::load(lockfile_path, &mut report);
    let mut failed_sources = BTreeSet::new();

    let mut destinations: Vec<DirectSkillDestination> = Vec::new();
    let mut slot_by_path: BTreeMap<String, usize> = BTreeMap::new();
    for assignment in &publication.assignments {
        let package = &assignment.package;
        if ProviderSourceKey::new(package.source_id.clone()).is_err()
            || ProviderSourceKey::new(package.alias.clone()).is_err()
            || !package.source_path.is_dir()
        {
            failed_sources.insert(package.source_id.clone());
            evidence.run_warnings.push(format!(
                "invalid direct skill package: {}",
                package.source_path.display()
            ));
            continue;
        }
        let dest = assignment.root.path.join(&package.alias);
        let key = normalize_managed_artifact_path(&dest);
        match slot_by_path.get(&key) {
            Some(&slot) if destinations[slot].package == package => {
                for target in &assignment.root.targets {
                    push_unique_target(&mut destinations[slot].targets, *target);
                }
            }
            Some(_) => {
                failed_sources.insert(package.source_id.clone());
                fail_targets(
                    &mut evidence,
                    &assignment.root.targets,
                    &dest,
                    format!(
                        "existing skill destination is already assigned to another package than source '{}'",
                        package.source_id
                    ),
                );
            }
            None => {
                let mut targets = Vec::new();
                for target in &assignment.root.targets {
                    push_unique_target(&mut targets, *target);
                }
                slot_by_path.insert(key, destinations.len());
                destinations.push(DirectSkillDestination {
                    dest,
                    package,
                    targets,
                });
            }
        }
    }

    for destination in &destinations {
        let package = destination.package;
        let dest = &destination.dest;
        let owner = registry.owning_source_of(dest);
        let destination_exists = dest.exists() || is_symlink_or_junction(dest);
        let managed = registry.is_managed(dest);
        let is_owned_by_source = owner == Some(package.source_id.as_str());
        if destination_exists && !(managed && is_owned_by_source) {
            failed_sources.insert(package.source_id.clone());
            fail_targets(
                &mut evidence,
                &destination.targets,
                dest,
                format!(
                    "existing skill destination is not owned by source '{}'",
                    package.source_id
                ),
            );
            continue;
        }
        if dry_run {
            continue;
        }
        if let Err(error) = publish_staged_tree_copy(&package.source_path, dest) {
            failed_sources.insert(package.source_id.clone());
            fail_targets(&mut evidence, &destination.targets, dest, error.to_string());
            continue;
        }
        registry.transfer_with_source(
            MANAGED_SKILL_PATHS,
            MANAGED_DIRECT_SKILL_PATHS,
            dest,
            &package.source_id,
        );
        for target in &destination.targets {
            evidence
                .by_target
                .entry(*target)
                .or_default()
                .paths
                .push(ProjectionPathResult {
                    path: dest.clone(),
                    change: ProjectionPathChange::Written,
                });
        }
    }
    for target in &publication.served_targets {
        evidence.by_target.entry(*target).or_default().skipped =
            Some("served by a planned shared skill root".to_string());
    }
    if !dry_run {
        let sources = publication
            .assignments
            .iter()
            .map(|a| a.package.source_id.as_str())
            .filter(|id| !failed_sources.contains(*id))
            .collect::<Vec<_>>();
        if let Err(error) = registry.persist(&sources, false, &mut report) {
            evidence.run_warnings.push(error.to_string());
        }
    }
    merge_report_as(&mut evidence, &report, None);
    evidence
}

fn push_unique_target(targets: &mut Vec<ProjectionTargetId>, target: ProjectionTargetId) {
    if !targets.contains(&target) {
        targets.push(target);
    }
}

fn fail_targets(
    evidence: &mut ProjectionEvidence,
    targets: &[ProjectionTargetId],
    dest: &Path,
    reason: String,
) {
    for target in targets {
        evidence.by_target.entry(*target).or_default().failure = Some(ProjectionExpectedFailure {
            path: Some(dest.to_path_buf()),
            reason: reason.clone(),
        });
    }
}

const PROJECTION_LOCK_KEY: &str = "_ccyncProjection";
const PROJECTION_LOCK_SCHEMA_VERSION: u64 = 1;
const MANAGED_SOURCE_ATTRIBUTION: &str = "sourceAttribution";
const MANAGED_STANDALONE_PROOFS: &str = "standalonePublications";
#[allow(dead_code)]
const HISTORICAL_OWNERSHIP: &str = "historicalOwnership";

/// Read the historical ownership section for diagnostics and migrations.
/// Historical rows are metadata only and are never consulted by mutation or
/// deletion authorization.
#[allow(dead_code)]
pub fn read_historical_ownership(lockfile_path: &Path) -> Option<Value> {
    let raw = fs::read_to_string(lockfile_path).ok()?;
    let root = serde_json::from_str::<Value>(&raw).ok()?;
    root.get(PROJECTION_LOCK_KEY)?
        .get(HISTORICAL_OWNERSHIP)
        .cloned()
}

/// Append a migration row to `historicalOwnership`. The registry deliberately
/// does not load this section into its ownership state.
#[allow(dead_code)]
pub fn append_historical_ownership(
    lockfile_path: &Path,
    row: Value,
) -> Result<(), ProjectionError> {
    let mut root = match fs::read_to_string(lockfile_path) {
        Ok(raw) => match serde_json::from_str::<Value>(&raw)
            .map_err(|err| ProjectionError::Message(err.to_string()))?
        {
            Value::Object(root) => root,
            _ => {
                return Err(ProjectionError::Message(
                    "projection ownership document must be a JSON object".into(),
                ));
            }
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => Map::new(),
        Err(err) => return Err(ProjectionError::Io(err)),
    };
    let meta = root
        .entry(PROJECTION_LOCK_KEY.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let meta = meta.as_object_mut().ok_or_else(|| {
        ProjectionError::Message(format!(
            "projection ownership field '{PROJECTION_LOCK_KEY}' must be a JSON object"
        ))
    })?;
    meta.entry("schemaVersion".to_string())
        .or_insert_with(|| Value::Number(PROJECTION_LOCK_SCHEMA_VERSION.into()));
    let rows = meta
        .entry(HISTORICAL_OWNERSHIP.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    rows.as_array_mut()
        .ok_or_else(|| {
            ProjectionError::Message(format!(
                "projection ownership field '{HISTORICAL_OWNERSHIP}' must be a JSON array"
            ))
        })?
        .push(row);

    let rendered = serde_json::to_string_pretty(&Value::Object(root))
        .map_err(|err| ProjectionError::Message(err.to_string()))?;
    write_text_atomically(lockfile_path, &rendered, false)?;
    Ok(())
}

pub(crate) const MANAGED_AGENT_PATHS: &str = "agentProjectionPaths";
pub(crate) const MANAGED_CODEX_AGENT_PATHS: &str = "codexAgentProjectionPaths";
pub(crate) const MANAGED_OPENCODE_AGENT_PATHS: &str = "opencodeAgentProjectionPaths";
pub(crate) const MANAGED_COMMAND_PATHS: &str = "commandProjectionPaths";
pub(crate) const MANAGED_DISCUSS_SKILL_PATHS: &str = "discussSkillProjectionPaths";
pub(crate) const MANAGED_LEGACY_PATHS: &str = "legacyProjectionPaths";
pub(crate) const MANAGED_SKILL_PATHS: &str = "skillProjectionPaths";
pub(crate) const MANAGED_AGY_ROOT_PATHS: &str = "agyRootProjectionPaths";
pub(crate) const MANAGED_DIRECT_SKILL_PATHS: &str = "directSkillProjectionPaths";
pub(crate) const MANAGED_DIRECT_AGENT_PATHS: &str = "directAgentProjectionPaths";
pub(crate) const MANAGED_DIRECT_COMMAND_PATHS: &str = "directCommandProjectionPaths";
pub(crate) const MANAGED_SOURCE_LINK_PATHS: &str = "sourceLinkProjectionPaths";

/// Kind of artifact a direct publication owns. Each kind has its own registry
/// category, so a machine writer that replaces its own categories cannot drop
/// direct ownership rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectArtifactKind {
    /// A skill tree. Commands converted into skills are recorded here too.
    Skill,
    Agent,
    /// A native command file.
    Command,
}

impl DirectArtifactKind {
    fn category(self) -> &'static str {
        match self {
            Self::Skill => MANAGED_DIRECT_SKILL_PATHS,
            Self::Agent => MANAGED_DIRECT_AGENT_PATHS,
            Self::Command => MANAGED_DIRECT_COMMAND_PATHS,
        }
    }
}

/// One path recorded as owned by a direct publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectProjectionRecord {
    pub kind: DirectArtifactKind,
    /// Normalized path as recorded in the lockfile.
    pub path: PathBuf,
    /// Source recorded for `path`, or `None` when no attribution was recorded.
    pub source_id: Option<String>,
}

/// Every path recorded in the direct categories of the lockfile at
/// `lockfile_path`. Only recorded rows are returned: no directory scan and no
/// name or digest matching. A missing or unreadable lockfile yields no rows.
pub fn direct_records(lockfile_path: &Path) -> Vec<DirectProjectionRecord> {
    let mut report = ProjectionReport::default();
    let registry = ManagedArtifactRegistry::load(lockfile_path, &mut report);
    let mut records = Vec::new();
    for kind in [
        DirectArtifactKind::Skill,
        DirectArtifactKind::Agent,
        DirectArtifactKind::Command,
    ] {
        for path in registry.prior_paths(kind.category()) {
            let path = PathBuf::from(path);
            let source_id = registry.owning_source_of(&path).map(str::to_string);
            records.push(DirectProjectionRecord {
                kind,
                path,
                source_id,
            });
        }
    }
    records
}

/// Remove one direct row and its source attribution from the lockfile at
/// `lockfile_path`. Call only after the artifact at `path` is already gone:
/// a failed deletion must keep its row so the next run can retry it. Absent
/// rows are a no-op. `dry_run` suppresses the write.
pub fn forget_direct_record(
    lockfile_path: &Path,
    kind: DirectArtifactKind,
    path: &Path,
    dry_run: bool,
) -> Result<(), ProjectionError> {
    if dry_run {
        return Ok(());
    }
    remove_managed_artifact_row_on_disk(lockfile_path, kind.category(), path)
}

/// Every source link recorded in the lockfile at `lockfile_path`. Only
/// recorded rows are returned, as with [`direct_records`]. A missing or
/// unreadable lockfile yields no rows.
pub fn source_link_records(lockfile_path: &Path) -> Vec<PathBuf> {
    let mut report = ProjectionReport::default();
    let registry = ManagedArtifactRegistry::load(lockfile_path, &mut report);
    registry
        .prior_paths(MANAGED_SOURCE_LINK_PATHS)
        .map(PathBuf::from)
        .collect()
}

/// Remove one recorded source link and forget its row. The link is removed
/// only while `path` is still a symlink or junction, so its target is never
/// touched. A path that is no longer a link is forgotten without deletion.
/// An I/O error during removal returns before the row is forgotten, so the
/// next run can retry. `dry_run` removes nothing and writes nothing.
pub fn remove_source_link(
    lockfile_path: &Path,
    path: &Path,
    dry_run: bool,
) -> Result<(), ProjectionError> {
    if dry_run {
        return Ok(());
    }
    if is_symlink_or_junction(path) {
        remove_link(path)?;
    }
    remove_managed_artifact_row_on_disk(lockfile_path, MANAGED_SOURCE_LINK_PATHS, path)
}

/// Whether one reader of a shared path has a verified replacement route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedPathReaderState {
    /// The reader's own replacement was published and verified present.
    ReplacementVerified,
    /// The reader is not selected, so no replacement was published for it.
    Unselected,
    /// The reader's replacement could not be verified.
    Unverified,
}

/// One consumer of a shared path, named by its exact reader group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedPathReader {
    pub group: String,
    pub state: SharedPathReaderState,
}

/// Argument for [`retire_shared_path`]: a shared path, the owned fingerprint it
/// must still match, and the complete set of its readers. An empty reader set
/// or an empty fingerprint is an unknown proof and authorizes nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedPathRetirement {
    pub path: PathBuf,
    /// Fingerprint ccync recorded when it last wrote `path`. Obtain it from
    /// [`shared_path_fingerprint`] at that time.
    pub owned_fingerprint: String,
    /// Every reader of `path`, selected or not.
    pub readers: Vec<SharedPathReader>,
}

/// Result of [`retire_shared_path`]. `Kept` names the first reason the path
/// was preserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SharedPathRetirementOutcome {
    Removed,
    Kept { reason: String },
}

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv_write(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(FNV_PRIME);
    }
    // Field separator, so adjacent fields cannot shift into each other.
    *hash ^= 0xff;
    *hash = hash.wrapping_mul(FNV_PRIME);
}

fn fnv_hash_node(root: &Path, node: &Path, hash: &mut u64) -> io::Result<()> {
    let relative = node
        .strip_prefix(root)
        .unwrap_or(node)
        .to_string_lossy()
        .replace('\\', "/");
    if is_symlink_or_junction(node) {
        fnv_write(hash, b"L");
        fnv_write(hash, relative.as_bytes());
        fnv_write(hash, fs::read_link(node)?.to_string_lossy().as_bytes());
    } else if fs::symlink_metadata(node)?.is_dir() {
        fnv_write(hash, b"D");
        fnv_write(hash, relative.as_bytes());
        let mut children = fs::read_dir(node)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()?;
        children.sort();
        for child in &children {
            fnv_hash_node(root, child, hash)?;
        }
    } else {
        let bytes = fs::read(node)?;
        fnv_write(hash, b"F");
        fnv_write(hash, relative.as_bytes());
        fnv_write(hash, &(bytes.len() as u64).to_le_bytes());
        fnv_write(hash, &bytes);
    }
    Ok(())
}

/// Content fingerprint of the file, directory tree, or link at `path`, in the
/// form `fnv1a64:<hex>`. Covers relative names, entry kinds, link targets and
/// file bytes, in sorted order. `None` when `path` is absent or unreadable.
pub fn shared_path_fingerprint(path: &Path) -> Option<String> {
    fs::symlink_metadata(path).ok()?;
    let mut hash = FNV_OFFSET_BASIS;
    fnv_hash_node(path, path, &mut hash).ok()?;
    Some(format!("fnv1a64:{hash:016x}"))
}

/// Remove a shared path only when every reader of it is accounted for and the
/// path still matches its owned fingerprint. It is then removed, and its
/// registry rows are forgotten after the removal succeeds.
///
/// Keeps the path, with a reason, when the proof is empty, a reader is not
/// `ReplacementVerified`, the path is not recorded as ccync-owned, the path is
/// absent, or its fingerprint has changed. `dry_run` reports the outcome and
/// writes nothing.
pub fn retire_shared_path(
    retirement: &SharedPathRetirement,
    lockfile_path: &Path,
    dry_run: bool,
) -> Result<SharedPathRetirementOutcome, ProjectionError> {
    let kept = |reason: String| Ok(SharedPathRetirementOutcome::Kept { reason });
    let path = retirement.path.as_path();
    if retirement.owned_fingerprint.trim().is_empty() {
        return kept("no owned fingerprint was recorded".to_string());
    }
    if retirement.readers.is_empty() {
        return kept("no reader proof was supplied".to_string());
    }
    for reader in &retirement.readers {
        if reader.group.trim().is_empty() {
            return kept("a reader has no group identity".to_string());
        }
        if reader.state != SharedPathReaderState::ReplacementVerified {
            return kept(format!(
                "reader {} is not preserved ({:?})",
                reader.group, reader.state
            ));
        }
    }

    let mut report = ProjectionReport::default();
    let registry = ManagedArtifactRegistry::load(lockfile_path, &mut report);
    if !registry.is_managed(path) {
        return kept(format!("{} is not recorded as ccync-owned", path.display()));
    }
    match shared_path_fingerprint(path) {
        None => return kept(format!("{} is absent", path.display())),
        Some(current) if current != retirement.owned_fingerprint => {
            return kept(format!("{} changed since ccync wrote it", path.display()));
        }
        Some(_) => {}
    }
    if dry_run {
        return Ok(SharedPathRetirementOutcome::Removed);
    }

    let needle = normalize_managed_artifact_path(path);
    let categories: Vec<String> = registry
        .prior
        .iter()
        .filter(|(_, paths)| paths.contains(&needle))
        .map(|(category, _)| category.clone())
        .collect();

    if is_symlink_or_junction(path) {
        remove_link(path)?;
    } else if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    for category in &categories {
        remove_managed_artifact_row_on_disk(lockfile_path, category, path)?;
    }
    Ok(SharedPathRetirementOutcome::Removed)
}

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

/// Registry-backed ownership authority for `agy-*` plugin roots (Protected Path).
///
/// Modelled on [`ManagedDeprojection`], but adds a second property the raw
/// [`ManagedArtifactRegistry`] does not provide on its own: a forced,
/// immediate row write. Two properties in total:
///
/// - **Strict membership.** [`is_owned`](Self::is_owned) uses
///   [`ManagedArtifactRegistry::is_managed`], deliberately not `can_mutate`:
///   `can_mutate` is allow-all when the prior lockfile is empty or missing,
///   which would authorize deleting a user's pre-existing
///   `~/.gemini/config/plugins/ccync` on a first run. `is_managed` authorizes
///   nothing until a row has actually been recorded, so an empty or missing
///   prior owns nothing.
/// - **Forced pre-swap write.** [`record_now`](Self::record_now) writes a
///   single row to `lock.json` immediately, instead of waiting for the
///   run's own end-of-run [`ManagedArtifactRegistry::persist`] — so a row
///   can be made durable before a tree becomes visible at its target path,
///   ahead of that run's own registry instance reaching `persist`. The write
///   re-reads the lockfile fresh from disk and merges into it; it never
///   publishes a snapshot of this (or any other) registry instance's
///   in-flight `next` state, so it cannot clear a category `replace_category`
///   has emptied but the run has not yet repopulated, and cannot make one
///   registry instance overwrite another's categories.
pub struct AgyRootOwnership {
    lockfile_path: PathBuf,
    registry: ManagedArtifactRegistry,
}

impl AgyRootOwnership {
    /// Load the prior managed registry from `build/lock.json`. A missing or
    /// unreadable lockfile yields an authority that owns nothing.
    pub fn from_lockfile(lockfile_path: &Path) -> Self {
        let mut report = ProjectionReport::default();
        Self {
            lockfile_path: lockfile_path.to_path_buf(),
            registry: ManagedArtifactRegistry::load(lockfile_path, &mut report),
        }
    }

    /// True iff `path` was recorded as ccync-owned in the prior lockfile.
    /// False on an empty or missing prior (no allow-all), so a pre-existing
    /// path ccync never projected is never authorized for removal.
    pub fn is_owned(&self, path: &Path) -> bool {
        self.registry.is_managed(path)
    }

    /// Record `path` as owned under the `agy-*` root category
    /// ([`MANAGED_AGY_ROOT_PATHS`]), writing the row to `lock.json`
    /// immediately rather than waiting for this (or any) registry
    /// instance's end-of-run `persist`. The write merges the single row
    /// into the lockfile as currently stored on disk, leaving every other
    /// category and every other on-disk key untouched. `dry_run` suppresses
    /// the write entirely.
    pub fn record_now(&self, path: &Path, dry_run: bool) -> Result<(), ProjectionError> {
        if dry_run {
            return Ok(());
        }
        merge_managed_artifact_row_on_disk(&self.lockfile_path, MANAGED_AGY_ROOT_PATHS, path)
    }

    /// Remove `path`'s row from `lock.json` immediately — the mirror of
    /// [`Self::record_now`], applied only after the tree at `path` is
    /// already gone. Never the other way around: a row-without-tree window
    /// is the accepted, harmless crash state the publish ordering already
    /// tolerates; a tree-without-row window is the failure that ordering
    /// exists to prevent, so this method's caller is responsible for
    /// deleting the tree first and calling this only once that succeeded.
    /// Like `record_now`, the write is a direct disk read-modify-write —
    /// never through any `ManagedArtifactRegistry` instance's in-memory
    /// `next` — so it cannot be reverted by a sibling instance's later
    /// `persist` (see `support.rs`'s disk-authoritative `persist` for why
    /// that guarantee matters here). `dry_run` suppresses the write.
    pub fn forget_now(&self, path: &Path, dry_run: bool) -> Result<(), ProjectionError> {
        if dry_run {
            return Ok(());
        }
        remove_managed_artifact_row_on_disk(&self.lockfile_path, MANAGED_AGY_ROOT_PATHS, path)
    }
}

/// Merge a single managed-artifact path into `category`'s array in the
/// lockfile at `lockfile_path`, reading and writing the file directly rather
/// than going through any [`ManagedArtifactRegistry`] instance's in-memory
/// `next` state. Every other key already on disk — every other category,
/// `sourceAttribution`, `schemaVersion`, and any top-level key outside
/// `_ccyncProjection` — is carried through untouched; only `category`'s
/// array gains `path`, and only if it is not already present. A missing or
/// unparseable lockfile is treated as an empty starting point, matching
/// [`ManagedArtifactRegistry::load`]'s own tolerance.
fn merge_managed_artifact_row_on_disk(
    lockfile_path: &Path,
    category: &str,
    path: &Path,
) -> Result<(), ProjectionError> {
    let mut root: Map<String, Value> = fs::read_to_string(lockfile_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| match value {
            Value::Object(root) => Some(root),
            _ => None,
        })
        .unwrap_or_default();

    let meta = root
        .entry(PROJECTION_LOCK_KEY.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !meta.is_object() {
        *meta = Value::Object(Map::new());
    }
    let meta = meta
        .as_object_mut()
        .expect("meta was just ensured to be an object");
    meta.entry("schemaVersion".to_string())
        .or_insert_with(|| Value::Number(PROJECTION_LOCK_SCHEMA_VERSION.into()));

    let needle = normalize_managed_artifact_path(path);
    let entry = meta
        .entry(category.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !entry.is_array() {
        *entry = Value::Array(Vec::new());
    }
    let array = entry
        .as_array_mut()
        .expect("entry was just ensured to be an array");
    let already_present = array
        .iter()
        .any(|value| value.as_str() == Some(needle.as_str()));
    if !already_present {
        array.push(Value::String(needle));
    }

    let rendered = serde_json::to_string_pretty(&Value::Object(root))
        .map_err(|err| ProjectionError::Message(err.to_string()))?;
    write_text(lockfile_path, &rendered, false)?;
    Ok(())
}

/// Remove a single managed-artifact path from `category`'s array in the
/// lockfile at `lockfile_path` — the mirror of
/// [`merge_managed_artifact_row_on_disk`], same direct-disk-read-modify-write
/// discipline (never through any [`ManagedArtifactRegistry`] instance's
/// in-memory state). The path's `sourceAttribution` row is removed with it.
/// Absent from both, or a missing/unparseable lockfile, is treated as
/// already-removed: a no-op, not an error. Every other key already on disk
/// is carried through untouched.
fn remove_managed_artifact_row_on_disk(
    lockfile_path: &Path,
    category: &str,
    path: &Path,
) -> Result<(), ProjectionError> {
    let Some(raw) = fs::read_to_string(lockfile_path).ok() else {
        return Ok(());
    };
    let Some(Value::Object(mut root)) = serde_json::from_str::<Value>(&raw).ok() else {
        return Ok(());
    };
    let Some(Value::Object(meta)) = root.get_mut(PROJECTION_LOCK_KEY) else {
        return Ok(());
    };

    let needle = normalize_managed_artifact_path(path);
    let mut changed = false;
    if let Some(Value::Array(array)) = meta.get_mut(category) {
        let before = array.len();
        array.retain(|value| {
            value
                .as_str()
                .and_then(decode_managed_artifact_key)
                .as_deref()
                != Some(needle.as_str())
        });
        changed = array.len() != before;
    }
    if let Some(Value::Object(attribution)) = meta.get_mut(MANAGED_SOURCE_ATTRIBUTION) {
        let before = attribution.len();
        attribution
            .retain(|key, _| decode_managed_artifact_key(key).as_deref() != Some(needle.as_str()));
        changed |= attribution.len() != before;
    }
    if let Some(Value::Object(proofs)) = meta.get_mut(MANAGED_STANDALONE_PROOFS) {
        let before = proofs.len();
        proofs
            .retain(|key, _| decode_managed_artifact_key(key).as_deref() != Some(needle.as_str()));
        changed |= proofs.len() != before;
    }
    if !changed {
        return Ok(());
    }

    let rendered = serde_json::to_string_pretty(&Value::Object(root))
        .map_err(|err| ProjectionError::Message(err.to_string()))?;
    write_text(lockfile_path, &rendered, false)?;
    Ok(())
}

/// Rewrite the lockfile down to its projection-ownership namespace, dropping
/// every other top-level key — `_mcpServers` above all.
///
/// `ccync uninstall` retains the lockfile when an owned `agy-*` root survives
/// teardown, because the rows inside `_ccyncProjection` are the only proof that
/// authorizes removing ccync's leftovers on a later retry. What must not
/// survive an uninstall is everything outside that namespace: `_mcpServers`
/// holds the adopted MCP server definitions verbatim, `env` and `headers`
/// included, which is where API keys and bearer tokens live.
///
/// The whole `_ccyncProjection` object is kept, not just the `agy-*` category.
/// Its siblings are the ownership proof `ManagedDeprojection` needs for the
/// other managed surfaces (`~/.claude/commands`, `~/.codex/skills`,
/// `~/.gemini/commands`, and the rest), which have no unconditional removal
/// path — narrowing the retention to the agy rows would let a partial
/// uninstall's other leftovers become permanently unprovable. Those siblings
/// are path arrays plus `sourceAttribution`; they carry no credentials, so
/// keeping them costs nothing this function exists to prevent.
///
/// A missing or unparseable lockfile is a no-op: there is nothing to strip and
/// nothing was proven, so this never manufactures a file.
pub fn strip_lockfile_to_agy_rows(lockfile_path: &Path) -> Result<(), ProjectionError> {
    let Some(raw) = fs::read_to_string(lockfile_path).ok() else {
        return Ok(());
    };
    let Some(Value::Object(root)) = serde_json::from_str::<Value>(&raw).ok() else {
        return Ok(());
    };
    let Some(meta @ Value::Object(_)) = root.get(PROJECTION_LOCK_KEY) else {
        return Ok(());
    };

    let mut kept_root = Map::new();
    kept_root.insert(PROJECTION_LOCK_KEY.to_string(), meta.clone());

    let rendered = serde_json::to_string_pretty(&Value::Object(kept_root))
        .map_err(|err| ProjectionError::Message(err.to_string()))?;
    write_text(lockfile_path, &rendered, false)?;
    Ok(())
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
    standalone_proofs_prior: BTreeMap<String, Value>,
    standalone_proofs_next: BTreeMap<String, Value>,
    quarantine: BTreeMap<String, BTreeSet<String>>,
    quarantine_attribution: BTreeMap<String, Value>,
    quarantine_proofs: BTreeMap<String, Value>,
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
        let mut standalone_proofs = BTreeMap::new();
        if let Some(Value::Object(meta)) = root.get(PROJECTION_LOCK_KEY) {
            for category in [
                MANAGED_AGENT_PATHS,
                MANAGED_CODEX_AGENT_PATHS,
                MANAGED_OPENCODE_AGENT_PATHS,
                MANAGED_COMMAND_PATHS,
                MANAGED_LEGACY_PATHS,
                MANAGED_SKILL_PATHS,
                MANAGED_AGY_ROOT_PATHS,
                MANAGED_DIRECT_SKILL_PATHS,
                MANAGED_DIRECT_AGENT_PATHS,
                MANAGED_DIRECT_COMMAND_PATHS,
                MANAGED_SOURCE_LINK_PATHS,
            ] {
                let mut paths = BTreeSet::new();
                for key in load_managed_artifact_set(meta, category) {
                    if let Some(decoded) = decode_managed_artifact_key(&key) {
                        paths.insert(decoded);
                    } else {
                        registry
                            .quarantine
                            .entry(category.to_string())
                            .or_default()
                            .insert(key);
                    }
                }
                projection.insert(category.to_string(), paths);
            }
            if let Some(Value::Object(proofs)) = meta.get(MANAGED_STANDALONE_PROOFS) {
                for (key, value) in proofs {
                    if let Some(decoded) = decode_managed_artifact_key(key) {
                        standalone_proofs.insert(decoded, value.clone());
                    } else {
                        registry
                            .quarantine_proofs
                            .insert(key.clone(), value.clone());
                    }
                }
            }
            if let Some(Value::Object(attr_map)) = meta.get(MANAGED_SOURCE_ATTRIBUTION) {
                for (path, src) in attr_map {
                    if let Some(decoded) = decode_managed_artifact_key(path) {
                        if let Some(src_str) = src.as_str() {
                            attribution.insert(decoded, src_str.to_string());
                        }
                    } else {
                        registry
                            .quarantine_attribution
                            .insert(path.clone(), src.clone());
                    }
                }
            }
        }

        registry.lockfile_root = Some(root);
        registry.prior = projection.clone();
        registry.next = projection;
        registry.source_attribution_prior = attribution.clone();
        registry.source_attribution_next = attribution;
        registry.standalone_proofs_prior = standalone_proofs.clone();
        registry.standalone_proofs_next = standalone_proofs;
        registry
    }

    pub(crate) fn can_mutate(&self, path: &Path) -> bool {
        // No prior lockfile → first-run; allow all mutations (backward-compat with is_ccync_managed_file guard).
        if self.prior.is_empty() && self.prior_is_empty() {
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

    pub(crate) fn prior_paths(&self, category: &str) -> impl Iterator<Item = &str> {
        self.prior
            .get(category)
            .into_iter()
            .flatten()
            .map(String::as_str)
    }

    /// True when the prior lockfile recorded no managed paths (missing/empty
    /// lockfile, or a lockfile with no projection marker).
    pub(crate) fn prior_is_empty(&self) -> bool {
        self.prior.values().all(BTreeSet::is_empty)
            && self.quarantine.values().all(BTreeSet::is_empty)
            && self.quarantine_attribution.is_empty()
            && self.quarantine_proofs.is_empty()
    }

    pub(crate) fn replace_category(&mut self, category: &str) {
        self.next.insert(category.to_string(), BTreeSet::new());
    }

    pub(crate) fn mark(&mut self, category: &str, path: &Path) {
        self.standalone_proofs_next
            .remove(&normalize_managed_artifact_path(path));
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
        self.standalone_proofs_next.remove(&normalized);
        self.next
            .entry(category.to_string())
            .or_default()
            .insert(normalized.clone());
        self.source_attribution_next
            .insert(normalized, source_id.to_string());
    }

    /// Move `path` from `from` to `to`, recording `source_id` for it. Call only
    /// after a direct write to `path` succeeded and the caller already proved
    /// ownership from committed membership plus matching attribution. A row
    /// still in `from` that this call never reaches stays recorded.
    pub(crate) fn transfer_with_source(
        &mut self,
        from: &str,
        to: &str,
        path: &Path,
        source_id: &str,
    ) {
        let normalized = normalize_managed_artifact_path(path);
        if let Some(paths) = self.next.get_mut(from) {
            paths.remove(&normalized);
        }
        self.mark_with_source(to, path, source_id);
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
        self.standalone_proofs_next.retain(|path, proof| {
            self.next
                .get(MANAGED_DIRECT_SKILL_PATHS)
                .is_some_and(|paths| paths.contains(path))
                && self.source_attribution_next.get(path).map(String::as_str)
                    == proof.get("sourceId").and_then(Value::as_str)
        });
        if self.prior == self.next
            && self.source_attribution_prior == self.source_attribution_next
            && self.standalone_proofs_prior == self.standalone_proofs_next
        {
            return Ok(());
        }

        // `MANAGED_AGY_ROOT_PATHS` rows are written out-of-band by
        // `AgyRootOwnership::record_now`'s forced pre-swap write, ahead of —
        // and possibly by a sibling registry instance rather than — this
        // run's own end-of-run persist. This instance's own `next` for that
        // one category is therefore never authoritative: unioning it back in
        // would resurrect a row a later teardown removed whenever this
        // instance loaded before the removal, and serializing it verbatim
        // would let this instance overwrite rows a sibling instance recorded
        // meanwhile. Disk is read fresh, right before serialization, and is
        // the sole source for this category.
        let agy_root_paths_on_disk =
            read_managed_artifact_set_from_disk(&self.lockfile_path, MANAGED_AGY_ROOT_PATHS);

        let other_categories_non_empty = self
            .next
            .iter()
            .filter(|(category, _)| category.as_str() != MANAGED_AGY_ROOT_PATHS)
            .any(|(_, paths)| !paths.is_empty());
        let mut meta = root
            .get(PROJECTION_LOCK_KEY)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let unknown_fields = meta.keys().any(|key| {
            key != "schemaVersion"
                && key != MANAGED_SOURCE_ATTRIBUTION
                && key != MANAGED_STANDALONE_PROOFS
                && !self.next.contains_key(key)
        });
        let non_empty = other_categories_non_empty
            || !agy_root_paths_on_disk.is_empty()
            || !self.source_attribution_next.is_empty()
            || !self.quarantine.values().all(BTreeSet::is_empty)
            || !self.quarantine_attribution.is_empty()
            || !self.quarantine_proofs.is_empty()
            || unknown_fields;
        if non_empty {
            meta.remove(MANAGED_SOURCE_ATTRIBUTION);
            meta.remove(MANAGED_STANDALONE_PROOFS);
            meta.insert(
                "schemaVersion".into(),
                Value::Number(PROJECTION_LOCK_SCHEMA_VERSION.into()),
            );
            for category in [
                MANAGED_AGENT_PATHS,
                MANAGED_CODEX_AGENT_PATHS,
                MANAGED_OPENCODE_AGENT_PATHS,
                MANAGED_COMMAND_PATHS,
                MANAGED_LEGACY_PATHS,
                MANAGED_SKILL_PATHS,
                MANAGED_AGY_ROOT_PATHS,
                MANAGED_DIRECT_SKILL_PATHS,
                MANAGED_DIRECT_AGENT_PATHS,
                MANAGED_DIRECT_COMMAND_PATHS,
                MANAGED_SOURCE_LINK_PATHS,
            ] {
                let mut rows: BTreeSet<String> = if category == MANAGED_AGY_ROOT_PATHS {
                    agy_root_paths_on_disk
                        .iter()
                        .map(|key| decode_managed_artifact_key(key).unwrap_or_else(|| key.clone()))
                        .collect()
                } else {
                    self.next.get(category).cloned().unwrap_or_default()
                };
                if category != MANAGED_AGY_ROOT_PATHS {
                    if let Some(quarantined) = self.quarantine.get(category) {
                        rows.extend(quarantined.iter().cloned());
                    }
                }
                let values = rows.into_iter().map(Value::String).collect();
                meta.insert(category.into(), Value::Array(values));
            }
            if !self.source_attribution_next.is_empty() || !self.quarantine_attribution.is_empty() {
                let mut attr_obj: Map<String, Value> = self
                    .source_attribution_next
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                    .collect();
                attr_obj.extend(self.quarantine_attribution.clone());
                meta.insert(MANAGED_SOURCE_ATTRIBUTION.into(), Value::Object(attr_obj));
            }
            if !self.standalone_proofs_next.is_empty() || !self.quarantine_proofs.is_empty() {
                let mut proofs = self.standalone_proofs_next.clone();
                proofs.extend(self.quarantine_proofs.clone());
                meta.insert(
                    MANAGED_STANDALONE_PROOFS.into(),
                    Value::Object(proofs.into_iter().collect()),
                );
            }
            root.insert(PROJECTION_LOCK_KEY.into(), Value::Object(meta));
        } else {
            root.remove(PROJECTION_LOCK_KEY);
        }

        let rendered = serde_json::to_string_pretty(&Value::Object(root))
            .map_err(|err| ProjectionError::Message(err.to_string()))?;
        if write_text_atomically(&self.lockfile_path, &rendered, dry_run)? {
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
    pub(crate) model: Option<String>,
}

pub(crate) fn parse_frontmatter(raw: &str) -> Frontmatter {
    let mut lines = raw.lines();
    if lines.next() != Some("---") {
        return Frontmatter::default();
    }

    let mut description = None;
    let mut color = None;
    let mut tools = Vec::new();
    let mut model = None;
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
            } else if key == "model" {
                model = Some(value.to_string());
            }
        }
    }

    Frontmatter {
        description,
        color,
        tools,
        model,
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

/// Remove `key:` entries from the first frontmatter block, preserving all other
/// bytes and leaving occurrences in the body untouched.
pub(crate) fn remove_frontmatter_key(raw: &str, key: &str) -> String {
    let mut lines = raw.lines();
    if lines.next() != Some("---") {
        return raw.to_string();
    }

    let prefix = format!("{key}:");
    let mut output = String::new();
    for (index, line) in raw.lines().enumerate() {
        if index > 0 && line.trim() == "---" {
            output.push_str(line);
            output.push('\n');
            output.extend(raw.lines().skip(index + 1).flat_map(|line| [line, "\n"]));
            return output.trim_end_matches('\n').to_string();
        }
        if index == 0 || !line.trim_start().starts_with(&prefix) {
            output.push_str(line);
            output.push('\n');
        }
    }
    raw.to_string()
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
    if let Some(model) = &frontmatter.model {
        lines.push(format!("model: {model}"));
    }
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

pub(crate) fn list_flat_command_files(root: &Path) -> Result<Vec<PathBuf>, ProjectionError> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry
                .path()
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| extension == "md")
        {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

/// Find Markdown commands below a namespace directory. The current native
/// command writers cannot preserve namespace-qualified invocation paths, so
/// callers report these paths as unsupported instead of silently ignoring
/// them.
pub(crate) fn list_nested_command_files(root: &Path) -> Result<Vec<PathBuf>, ProjectionError> {
    fn visit(
        root: &Path,
        current: &Path,
        output: &mut Vec<PathBuf>,
    ) -> Result<(), ProjectionError> {
        if !current.is_dir() {
            return Ok(());
        }
        for entry in fs::read_dir(current)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                if path.join("SKILL.md").is_file() || path.join("SKILL.template.md").is_file() {
                    if path != root {
                        output.push(path);
                    }
                } else {
                    visit(root, &path, output)?;
                }
            } else if path.extension().and_then(OsStr::to_str) == Some("md")
                && path.parent() != Some(root)
            {
                output.push(path);
            }
        }
        Ok(())
    }

    let mut output = Vec::new();
    visit(root, root, &mut output)?;
    output.sort();
    Ok(output)
}

pub(crate) fn ensure_name_frontmatter(content: &str, name: &str) -> String {
    if !content.starts_with("---\n") && !content.starts_with("---\r\n") {
        return format!("---\nname: {name}\n---\n{content}");
    }

    let newline = if content.starts_with("---\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let opening_len = 3 + newline.len();
    let Some(end) = content[opening_len..].find(&format!("{newline}---")) else {
        return format!("---{newline}name: {name}{newline}---{newline}{content}");
    };
    let frontmatter = &content[opening_len..opening_len + end];
    if frontmatter
        .lines()
        .any(|line| line.trim_start().starts_with("name:"))
    {
        return content.to_string();
    }
    format!(
        "---{newline}name: {name}{newline}{}",
        &content[opening_len..]
    )
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
/// during its final projection pass (see `docs/architecture.md`) so its stale
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

/// Prune stale real command-skill directories under `root`. A directory is
/// removable only when its root was not previously projected as a skill and
/// its `SKILL.md` was previously recorded as a command projection.
pub(crate) fn prune_stale_command_skill_dirs(
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

    let prior_skill_paths: BTreeSet<&str> = registry.prior_paths(MANAGED_SKILL_PATHS).collect();
    let prior_command_paths: BTreeSet<&str> = registry.prior_paths(MANAGED_COMMAND_PATHS).collect();

    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if is_symlink_or_junction(&path) || !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if active_names.contains(&name.as_str()) {
            continue;
        }

        let skill_root = normalize_managed_artifact_path(&path);
        let skill_file = normalize_managed_artifact_path(&path.join("SKILL.md"));
        if prior_skill_paths.contains(skill_root.as_str())
            || !prior_command_paths.contains(skill_file.as_str())
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

/// Same normalization/short-circuit/parent-creation/dry-run contract as
/// [`write_text`], but delegates real publication to
/// [`ccync_foundation::platform::atomic_write_bytes`] instead of a bare
/// `fs::write` — the sibling-temp-file + durable-sync + rename sequence, so a
/// crash mid-write cannot leave `path` truncated or half-written. Returns
/// `true` when a write happened (or would happen under `dry_run`).
pub(crate) fn write_text_atomically(
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
        atomic_write_bytes(path, normalized.as_bytes()).map_err(|err| {
            ProjectionError::Io(match err {
                AtomicWriteError::TempWrite(e) => e,
                AtomicWriteError::PreRename(e) => e,
                AtomicWriteError::Rename(e) => e,
            })
        })?;
    }
    Ok(true)
}

/// Report-appending counterpart to [`write_text_atomically`] — same
/// unchanged short circuit, and appends `path` to `report.written_files`
/// only after a real write succeeds, or as a dry-run preview.
pub(crate) fn write_text_atomically_with_report(
    path: &Path,
    content: &str,
    dry_run: bool,
    report: &mut ProjectionReport,
) -> Result<(), ProjectionError> {
    if write_text_atomically(path, content, dry_run)? {
        report.written_files.push(path.to_path_buf());
    }
    Ok(())
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

/// Read `category`'s current array straight off the lockfile at
/// `lockfile_path`, bypassing any [`ManagedArtifactRegistry`] instance's
/// in-memory `next`/`prior`. Used by [`ManagedArtifactRegistry::persist`]
/// for [`MANAGED_AGY_ROOT_PATHS`], the one category whose rows are written
/// out-of-band by `AgyRootOwnership::record_now` — disk is the sole
/// authority for that category, so a registry instance's own (possibly
/// stale, possibly ahead) in-memory copy never determines what gets
/// serialized. A missing or unparseable lockfile reads as empty, matching
/// [`ManagedArtifactRegistry::load`]'s own tolerance.
fn read_managed_artifact_set_from_disk(lockfile_path: &Path, category: &str) -> BTreeSet<String> {
    fs::read_to_string(lockfile_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| match value {
            Value::Object(mut root) => root.remove(PROJECTION_LOCK_KEY),
            _ => None,
        })
        .and_then(|meta| match meta {
            Value::Object(meta) => Some(load_managed_artifact_set(&meta, category)),
            _ => None,
        })
        .unwrap_or_default()
}

pub fn normalize_managed_artifact_path(path: &Path) -> String {
    #[cfg(windows)]
    {
        path.to_string_lossy()
            .replace('/', "\\")
            .to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        path.to_string_lossy().into_owned()
    }
}

pub fn decode_managed_artifact_key(key: &str) -> Option<String> {
    #[cfg(windows)]
    {
        Some(key.to_string())
    }
    #[cfg(not(windows))]
    {
        let decoded = if key.starts_with('\\') && !key.contains('/') {
            key.replace('\\', "/")
        } else {
            key.to_string()
        };
        if !Path::new(&decoded).is_absolute()
            || decoded.split('/').any(|part| part == "." || part == "..")
        {
            return None;
        }
        Some(decoded)
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

// ── staged-copy engine (additive, unwired) ─────────────────────────────────
//
// A full-directory-copy counterpart to `ensure_dir_link`'s symlink/junction
// path: stage a complete copy of a source tree into a sibling temporary
// directory, then swap it into place. The swap is remove-old-then-rename —
// Windows cannot rename a directory over an existing one — so the window
// between the two steps is the only place the final path can be observed
// absent; it is never observed partially written, because nothing renames
// (or otherwise mutates) the final path until the staged copy has finished
// in full off to the side. Nothing calls this yet; production callers land
// in a later task.

/// Sibling staging path for `dest`: same parent directory (so the eventual
/// rename stays on one filesystem), name-prefixed so it never collides with
/// a real projected artifact. Process id + a per-process counter make each
/// call's path unique without pulling in a UUID dependency this crate does
/// not otherwise need.
#[allow(dead_code)] // additive, unwired — production callers land in a later task
static STAGING_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn stage_sibling_path(dest: &Path) -> PathBuf {
    use std::sync::atomic::Ordering;

    let mut name = std::ffi::OsString::from(".ccync-stage-");
    if let Some(dest_name) = dest.file_name() {
        name.push(dest_name);
    }
    name.push(format!(
        "-{}-{}",
        std::process::id(),
        STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    dest.with_file_name(name)
}

/// Recursively copy every file and subdirectory of `source` into `dest`,
/// creating `dest` (and any needed parents) first. Stops at the first I/O
/// fault, leaving whatever was already copied in place — callers that need
/// an all-or-nothing result on the final path use [`stage_tree_copy`], which
/// stages into a throwaway sibling directory rather than the real
/// destination.
#[allow(dead_code)] // additive, unwired — production callers land in a later task
fn copy_tree_recursive(source: &Path, dest: &Path) -> io::Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree_recursive(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Stage a full copy of `source` into a fresh sibling directory of `dest`
/// (see [`stage_sibling_path`]), returning the staged path on success. On
/// any I/O fault the partial staging directory is removed (best-effort)
/// before the error is returned — `dest` itself is never touched by this
/// step, so a fault here cannot leave `dest` partial.
#[allow(dead_code)] // additive, unwired — production callers land in a later task
fn stage_tree_copy(source: &Path, dest: &Path) -> Result<PathBuf, ProjectionError> {
    let staged = stage_sibling_path(dest);
    if let Err(err) = copy_tree_recursive(source, &staged) {
        let _ = fs::remove_dir_all(&staged);
        return Err(ProjectionError::from(err));
    }
    Ok(staged)
}

/// Content-equality comparison of two directory trees: same entry names at
/// every level, same file/directory kind at each name, and byte-identical
/// file contents. An unreadable directory or file compares unequal rather
/// than panicking or silently treating the pair as a match — the caller uses
/// this to decide whether a swap can be skipped at all, and "can't prove
/// equal" must fall through to the safe re-copy path, not the skip path.
#[allow(dead_code)] // additive, unwired — production callers land in a later task
fn trees_content_equal(a: &Path, b: &Path) -> bool {
    let (Ok(mut a_entries), Ok(mut b_entries)) = (list_entry_names(a), list_entry_names(b)) else {
        return false;
    };
    if a_entries.len() != b_entries.len() {
        return false;
    }
    a_entries.sort();
    b_entries.sort();
    if a_entries != b_entries {
        return false;
    }

    a_entries.iter().all(|name| {
        let a_path = a.join(name);
        let b_path = b.join(name);
        let a_is_dir = a_path.is_dir();
        if a_is_dir != b_path.is_dir() {
            return false;
        }
        if a_is_dir {
            trees_content_equal(&a_path, &b_path)
        } else {
            match (fs::read(&a_path), fs::read(&b_path)) {
                (Ok(a_bytes), Ok(b_bytes)) => a_bytes == b_bytes,
                _ => false,
            }
        }
    })
}

/// Entry names directly under `root`, unsorted — [`trees_content_equal`]
/// sorts both sides itself so this stays a plain read.
fn list_entry_names(root: &Path) -> io::Result<Vec<String>> {
    fs::read_dir(root)?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().to_string()))
        .collect()
}

/// Swap `staged` into place at `dest`: remove whatever currently occupies
/// `dest` (a real directory, a symlink/junction, or a plain file — every
/// shape this engine's destinations can take), then `rename` `staged` onto
/// it. Windows cannot rename a directory over an existing one, so this is a
/// remove-then-rename, not a true single-syscall swap: between the removal
/// and the rename, `dest` is briefly absent. That absent-then-complete
/// window is the accepted one — `dest` is never observed holding a partial
/// copy, because nothing writes into `dest` itself; the copy already
/// finished, off to the side, before this function runs.
#[allow(dead_code)] // additive, unwired — production callers land in a later task
fn swap_staged_tree(staged: &Path, dest: &Path) -> Result<(), ProjectionError> {
    let backup = stage_sibling_path(dest).with_file_name(format!(
        ".ccync-backup-{}-{}",
        std::process::id(),
        STAGING_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let had_dest = classify_link(dest) != LinkShape::Absent || dest.exists();
    if had_dest {
        fs::rename(dest, &backup)?;
    }
    if let Err(error) = fs::rename(staged, dest) {
        if had_dest {
            let _ = fs::rename(&backup, dest);
        }
        return Err(error.into());
    }
    if had_dest {
        if is_symlink_or_junction(&backup) {
            let _ = remove_link(&backup);
        } else if backup.is_dir() {
            let _ = fs::remove_dir_all(&backup);
        } else {
            let _ = fs::remove_file(&backup);
        }
    }
    Ok(())
}

/// Publish a full copy of `source` at `dest`: skip entirely when `dest` is
/// already a real directory matching `source` byte-for-byte
/// ([`trees_content_equal`], zero writes), otherwise stage a fresh copy
/// ([`stage_tree_copy`]) and swap it into place ([`swap_staged_tree`]).
/// Returns `true` when a swap happened, `false` when the destination
/// already matched and nothing was written.
#[allow(dead_code)] // additive, unwired — production callers land in a later task
fn publish_staged_tree_copy(source: &Path, dest: &Path) -> Result<bool, ProjectionError> {
    if dest.is_dir() && !is_symlink_or_junction(dest) && trees_content_equal(source, dest) {
        return Ok(false);
    }
    let staged = stage_tree_copy(source, dest)?;
    // A failed swap must not leave the staged tree behind. `stage_sibling_path`
    // deliberately places it beside `dest` so the rename stays on one
    // filesystem, which for an `agy-*` root means inside the very directory
    // Antigravity scans for plugins — an orphan there is a duplicate plugin
    // root, not merely wasted disk. `stage_tree_copy` already cleans up its
    // own faults; this closes the other half.
    if let Err(err) = swap_staged_tree(&staged, dest) {
        let _ = fs::remove_dir_all(&staged);
        return Err(err);
    }
    Ok(true)
}

// Standalone publications use a dedicated build subtree. Legacy orphan sweeps
// inspect discovery siblings named .ccync-stage-* and never visit this subtree.
fn standalone_error(message: impl Into<String>) -> ProjectionError {
    ProjectionError::Message(format!("standalone publication: {}", message.into()))
}

fn standalone_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

// Absence is evidence only when metadata reports NotFound. Dangling links exist.
fn standalone_present(path: &Path) -> Result<bool, ProjectionError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn standalone_read_journal(
    build_root: &Path,
) -> Result<ccync_foundation::provider::ProviderLifecycleJournal, ProjectionError> {
    use ccync_foundation::provider::ProviderLifecycleJournal;
    let path = build_root.join("projection.txn.json");
    if !standalone_present(&path)? {
        return Ok(ProviderLifecycleJournal::default());
    }
    if is_symlink_or_junction(&path) || !fs::metadata(&path)?.is_file() {
        return Err(standalone_error("journal must be a readable regular file"));
    }
    ProviderLifecycleJournal::from_json_str(&fs::read_to_string(path)?)
        .map_err(|error| standalone_error(error.to_string()))
}

fn standalone_scoped_reservation(
    path: &ccync_foundation::provider::ScopedPath,
    destination: &ccync_foundation::provider::ScopedPath,
    destination_root: &Path,
    build_root: &Path,
) -> Result<PathBuf, ProjectionError> {
    use ccync_foundation::provider::PathRoot;
    let root = if path.root == destination.root {
        destination_root.to_path_buf()
    } else if path.root == PathRoot::CcyncHome {
        build_root
            .parent()
            .ok_or_else(|| standalone_error("build root lacks parent"))?
            .to_path_buf()
    } else {
        path.root
            .resolve()
            .ok_or_else(|| standalone_error("pending path root cannot be resolved"))?
    };
    Ok(path.path.join_to(&root))
}

fn standalone_reservations_overlap(a: &Path, b: &Path) -> Result<bool, ProjectionError> {
    standalone_reservations_overlap_with_preimages(a, b, None, None)
}

// A retained raw Link keeps its original resolution base after rename. Its
// private pathname and its intent-anchored referent reserve separate locations.
fn standalone_reservation_physical(
    path: &Path,
    preimage: Option<(
        &Path,
        &ccync_foundation::provider::StandalonePreimage,
        Option<ccync_foundation::provider::StandaloneLinkKind>,
    )>,
) -> Result<PathBuf, ProjectionError> {
    use ccync_foundation::provider::{StandaloneLinkKind, StandalonePreimage};
    if let Some((original, StandalonePreimage::Link { raw_target }, expected_kind)) = preimage {
        if standalone_present(path)? {
            if !is_symlink_or_junction(path) || fs::read_link(path)? != *raw_target {
                return Err(standalone_error(
                    "retained link reservation differs from intent",
                ));
            }
            let target = standalone_withdrawal_link_target(original, raw_target)?;
            let kind = if classify_link(path) == LinkShape::Junction {
                StandaloneLinkKind::Junction
            } else if fs::metadata(&target)?.is_file() {
                StandaloneLinkKind::FileSymlink
            } else {
                StandaloneLinkKind::DirectorySymlink
            };
            if expected_kind != Some(kind) {
                return Err(standalone_error(
                    "retained link reservation kind differs from intent",
                ));
            }
            return standalone_physical_path(&target);
        }
    }
    standalone_physical_path(path)
}

fn standalone_reservations_overlap_with_preimages(
    a: &Path,
    b: &Path,
    a_preimage: Option<(
        &Path,
        &ccync_foundation::provider::StandalonePreimage,
        Option<ccync_foundation::provider::StandaloneLinkKind>,
    )>,
    b_preimage: Option<(
        &Path,
        &ccync_foundation::provider::StandalonePreimage,
        Option<ccync_foundation::provider::StandaloneLinkKind>,
    )>,
) -> Result<bool, ProjectionError> {
    if standalone_paths_overlap(
        &standalone_namespace_path(a)?,
        &standalone_namespace_path(b)?,
    )? {
        return Ok(true);
    }
    standalone_paths_overlap(
        &standalone_reservation_physical(a, a_preimage)?,
        &standalone_reservation_physical(b, b_preimage)?,
    )
}

/// Check every other operation's live and saved paths, using the caller's roots.
fn standalone_journal_reservations(
    journal: &ccync_foundation::provider::ProviderLifecycleJournal,
    destination_scope: (&ccync_foundation::provider::ScopedPath, &Path),
    build_root: &Path,
    operation_id: &str,
    owned_paths: &[&Path],
    publication: Option<&ccync_foundation::provider::StandalonePublicationIntent>,
    withdrawal: Option<&ccync_foundation::provider::StandaloneWithdrawalIntent>,
) -> Result<(), ProjectionError> {
    let (destination, destination_root) = destination_scope;
    let original_destination = destination.path.join_to(destination_root);
    let owned_preimage = publication
        .map(|intent| {
            (
                intent.backup_path.join_to(build_root),
                &intent.preimage,
                intent.link_kind,
            )
        })
        .or_else(|| {
            withdrawal.map(|intent| {
                (
                    intent.backup_path.join_to(build_root),
                    &intent.preimage,
                    intent.link_kind,
                )
            })
        });
    let check = |path: &Path,
                 preimage: Option<(
        &Path,
        &ccync_foundation::provider::StandalonePreimage,
        Option<ccync_foundation::provider::StandaloneLinkKind>,
    )>|
     -> Result<(), ProjectionError> {
        for owned in owned_paths {
            let owned_preimage = owned_preimage
                .as_ref()
                .and_then(|(backup, preimage, kind)| {
                    (*owned == backup).then_some((original_destination.as_path(), *preimage, *kind))
                });
            if standalone_reservations_overlap_with_preimages(
                owned,
                path,
                owned_preimage,
                preimage,
            )? {
                return Err(standalone_error(
                    "pending intent reserves operation namespace",
                ));
            }
        }
        Ok(())
    };
    let scoped =
        |path| standalone_scoped_reservation(path, destination, destination_root, build_root);
    for pending in &journal.intents {
        for path in &pending.scoped_paths {
            check(&scoped(path)?, None)?;
        }
        for reference in &pending.preimage_refs {
            check(&scoped(&reference.path)?, None)?;
            check(&reference.saved.join_to(build_root), None)?;
        }
    }
    for pending in &journal.standalone_intents {
        if publication == Some(pending) {
            continue;
        }
        if pending.operation_id == operation_id {
            return Err(standalone_error("pending operation identity collision"));
        }
        let original = scoped(&pending.destination)?;
        check(&original, None)?;
        check(&pending.stage_path.join_to(build_root), None)?;
        check(
            &pending.backup_path.join_to(build_root),
            Some((&original, &pending.preimage, pending.link_kind)),
        )?;
        if let Some(reference) = &pending.preimage_ref {
            let original = scoped(&reference.path)?;
            check(&original, None)?;
            check(
                &reference.saved.join_to(build_root),
                Some((&original, &pending.preimage, pending.link_kind)),
            )?;
        }
    }
    for pending in &journal.standalone_withdrawal_intents {
        if withdrawal == Some(pending) {
            continue;
        }
        if pending.operation_id == operation_id {
            return Err(standalone_error("pending operation identity collision"));
        }
        let original = scoped(&pending.destination)?;
        check(&original, None)?;
        check(
            &pending.backup_path.join_to(build_root),
            Some((&original, &pending.preimage, pending.link_kind)),
        )?;
        let original = scoped(&pending.preimage_ref.path)?;
        check(&original, None)?;
        check(
            &pending.preimage_ref.saved.join_to(build_root),
            Some((&original, &pending.preimage, pending.link_kind)),
        )?;
    }
    Ok(())
}

fn standalone_clean_ancestors(path: &Path) -> Result<(), ProjectionError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err(standalone_error(
            "paths must be absolute and traversal-free",
        ));
    }
    for ancestor in path.ancestors() {
        if is_symlink_or_junction(ancestor) {
            return Err(standalone_error(format!(
                "linked ancestor: {}",
                ancestor.display()
            )));
        }
        if standalone_exists(ancestor) && !ancestor.is_dir() {
            return Err(standalone_error("non-directory ancestor"));
        }
        for manifest in [
            ".claude-plugin/plugin.json",
            ".codex-plugin/plugin.json",
            "plugin.json",
        ] {
            let manifest = ancestor.join(manifest);
            if manifest.is_file() {
                let value: Value = serde_json::from_slice(&fs::read(&manifest)?)
                    .map_err(|_| standalone_error("unreadable ancestor identity manifest"))?;
                if value.get("name").and_then(Value::as_str) == Some("ccync") {
                    return Err(standalone_error(
                        "destination is beneath a combined ccync manifest",
                    ));
                }
            }
        }
    }
    Ok(())
}

fn standalone_existing_ancestor(path: &Path) -> Result<PathBuf, ProjectionError> {
    for ancestor in path.ancestors() {
        if standalone_present(ancestor)? {
            return Ok(ancestor.to_path_buf());
        }
    }
    Err(standalone_error("no existing filesystem ancestor"))
}

fn standalone_physical_path(path: &Path) -> Result<PathBuf, ProjectionError> {
    let ancestor = standalone_existing_ancestor(path)?;
    let suffix = path
        .strip_prefix(&ancestor)
        .map_err(|_| standalone_error("invalid absolute path"))?;
    Ok(fs::canonicalize(&ancestor)?.join(suffix))
}

// Canonicalize ancestors while retaining the final namespace component. A
// supported root Link must reserve its destination as well as its target.
fn standalone_namespace_path(path: &Path) -> Result<PathBuf, ProjectionError> {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(leaf)) => Ok(standalone_physical_path(parent)?.join(leaf)),
        _ => standalone_physical_path(path),
    }
}

// Compare components rather than string prefixes. On Windows, absent suffixes
// still need the OS ordinal case policy, including non-ASCII case aliases.
fn standalone_component_equal(a: &OsStr, b: &OsStr) -> Result<bool, ProjectionError> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        extern "system" {
            fn CompareStringOrdinal(
                string1: *const u16,
                count1: i32,
                string2: *const u16,
                count2: i32,
                ignore_case: i32,
            ) -> i32;
        }
        let a: Vec<u16> = a.encode_wide().collect();
        let b: Vec<u16> = b.encode_wide().collect();
        if a.is_empty() || b.is_empty() {
            return Ok(a.is_empty() && b.is_empty());
        }
        let a_len = i32::try_from(a.len())
            .map_err(|_| standalone_error("path component exceeds Windows comparison length"))?;
        let b_len = i32::try_from(b.len())
            .map_err(|_| standalone_error("path component exceeds Windows comparison length"))?;
        // SAFETY: Both initialized UTF-16 buffers live throughout this read-only
        // call. Checked positive i32 lengths bound the buffers. The five-argument
        // system ABI matches CompareStringOrdinal and ignore_case is exactly 1.
        match unsafe { CompareStringOrdinal(a.as_ptr(), a_len, b.as_ptr(), b_len, 1) } {
            2 => Ok(true),
            1 | 3 => Ok(false),
            0 => Err(standalone_error(format!(
                "Windows path comparison failed: {}",
                io::Error::last_os_error()
            ))),
            _ => Err(standalone_error("invalid Windows path comparison result")),
        }
    }
    #[cfg(not(windows))]
    {
        Ok(a == b)
    }
}

fn standalone_path_prefix(path: &Path, root: &Path) -> Result<bool, ProjectionError> {
    let mut path = path.components();
    for root in root.components() {
        let Some(component) = path.next() else {
            return Ok(false);
        };
        if !standalone_component_equal(component.as_os_str(), root.as_os_str())? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn standalone_paths_overlap(a: &Path, b: &Path) -> Result<bool, ProjectionError> {
    Ok(standalone_path_prefix(a, b)? || standalone_path_prefix(b, a)?)
}

fn standalone_direct_child(path: &Path, root: &Path) -> Result<bool, ProjectionError> {
    Ok(path
        .components()
        .count()
        .checked_sub(root.components().count())
        == Some(1)
        && standalone_path_prefix(path, root)?)
}

fn standalone_same_filesystem(a: &Path, b: &Path) -> Result<bool, ProjectionError> {
    // A retained relative Link resolves against its original parent, so
    // compare the volume of the link entry's parent rather than its target.
    let anchor = |path: &Path| -> Result<PathBuf, ProjectionError> {
        let ancestor = standalone_existing_ancestor(path)?;
        match ancestor.parent() {
            Some(parent) if is_symlink_or_junction(&ancestor) => Ok(parent.to_path_buf()),
            _ => Ok(ancestor),
        }
    };
    let a = anchor(a)?;
    let b = anchor(b)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(fs::metadata(a)?.dev() == fs::metadata(b)?.dev())
    }
    #[cfg(not(unix))]
    {
        // Linked mount points are rejected independently by the ancestor check.
        let a = fs::canonicalize(a)?;
        let b = fs::canonicalize(b)?;
        Ok(a.components().next() == b.components().next())
    }
}

/// Return whether two paths resolve to the same filesystem volume.
pub fn paths_share_filesystem(a: &Path, b: &Path) -> Result<bool, ProjectionError> {
    standalone_same_filesystem(a, b)
}

fn standalone_lexical_path(path: &Path) -> Result<PathBuf, ProjectionError> {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !result.pop() {
                    return Err(standalone_error("link traversal escapes root"));
                }
            }
            Component::CurDir => {}
            other => result.push(other.as_os_str()),
        }
    }
    #[cfg(windows)]
    {
        let path = result.to_string_lossy().replace('/', "\\");
        let path = path.strip_prefix("\\\\?\\").unwrap_or(&path);
        Ok(PathBuf::from(path.to_ascii_lowercase()))
    }
    #[cfg(not(windows))]
    Ok(result)
}

#[cfg(windows)]
fn standalone_expand_short_path(path: &Path) -> Result<PathBuf, ProjectionError> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::ffi::OsStringExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetLongPathNameW(short_path: *const u16, long_path: *mut u16, buffer_length: u32)
            -> u32;
    }

    let path: Vec<u16> = path.as_os_str().encode_wide().collect();
    let mut input = if path.starts_with(&[b'\\' as u16, b'\\' as u16]) {
        let mut extended = "\\\\?\\UNC\\".encode_utf16().collect::<Vec<_>>();
        extended.extend_from_slice(&path[2..]);
        extended
    } else {
        let mut extended = "\\\\?\\".encode_utf16().collect::<Vec<_>>();
        extended.extend_from_slice(&path);
        extended
    };
    input.push(0);
    let mut capacity = input.len().max(260);
    loop {
        let buffer_length = u32::try_from(capacity)
            .map_err(|_| standalone_error("Windows path exceeds long-path buffer limit"))?;
        let mut buffer = vec![0u16; capacity];
        // SAFETY: `input` is null-terminated and remains alive for the call.
        // `buffer` has the stated capacity and is writable. The extended
        // prefix supports paths beyond MAX_PATH. The API expands short-name
        // components without canonicalizing or resolving link hops.
        let length = unsafe { GetLongPathNameW(input.as_ptr(), buffer.as_mut_ptr(), buffer_length) }
            as usize;
        if length == 0 {
            return Err(standalone_error(format!(
                "could not expand Windows short path: {}",
                io::Error::last_os_error()
            )));
        }
        if length < capacity {
            buffer.truncate(length);
            return Ok(PathBuf::from(std::ffi::OsString::from_wide(&buffer)));
        }
        capacity = length.saturating_add(1);
    }
}

#[cfg(not(windows))]
fn standalone_expand_short_path(path: &Path) -> Result<PathBuf, ProjectionError> {
    Ok(path.to_path_buf())
}

fn standalone_resolve_inside(
    boundary: &Path,
    physical: &Path,
    chain: &mut BTreeSet<PathBuf>,
) -> Result<PathBuf, ProjectionError> {
    let boundary = standalone_lexical_path(boundary)?;
    let physical = standalone_expand_short_path(&standalone_lexical_path(physical)?)?;
    let physical = standalone_lexical_path(&physical)?;
    let relative = physical
        .strip_prefix(&boundary)
        .map_err(|_| standalone_error("source link escapes the skill subtree"))?;
    let mut resolved = boundary.clone();
    for component in relative.components() {
        resolved.push(component.as_os_str());
        if is_symlink_or_junction(&resolved) {
            if !chain.insert(resolved.clone()) {
                return Err(standalone_error("cyclic source link chain"));
            }
            let raw = fs::read_link(&resolved)?;
            let target = if raw.is_absolute() {
                raw
            } else {
                resolved
                    .parent()
                    .ok_or_else(|| standalone_error("link has no parent"))?
                    .join(raw)
            };
            let linked = resolved.clone();
            resolved = standalone_resolve_inside(&boundary, &target, chain)?;
            chain.remove(&linked);
        }
    }
    let canonical = standalone_lexical_path(&fs::canonicalize(resolved)?)?;
    if !canonical.starts_with(&boundary) {
        return Err(standalone_error("source link escapes the skill subtree"));
    }
    Ok(canonical)
}

/// Portable permissions carried by a standalone copy, never ownership or ACLs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StandalonePermissions {
    #[cfg(unix)]
    mode: u32,
    #[cfg(not(unix))]
    readonly: bool,
}

impl StandalonePermissions {
    fn from_metadata(metadata: &fs::Metadata) -> Result<Self, ProjectionError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = metadata.permissions().mode();
            // Copying ownership-dependent setuid/setgid or sticky semantics is
            // unsafe without an ownership contract. Refuse rather than strip.
            if mode & 0o7000 != 0 {
                return Err(standalone_error(
                    "Unix special permission bits cannot be preserved safely",
                ));
            }
            Ok(Self { mode: mode & 0o777 })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                readonly: metadata.permissions().readonly(),
            })
        }
    }

    fn apply(&self, path: &Path) -> Result<(), ProjectionError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(self.mode))?;
        }
        #[cfg(not(unix))]
        {
            let mut permissions = fs::metadata(path)?.permissions();
            permissions.set_readonly(self.readonly);
            fs::set_permissions(path, permissions)?;
        }
        Ok(())
    }

    fn fingerprint(&self, hash: &mut u64) {
        #[cfg(unix)]
        {
            fnv_write(hash, b"unix-mode");
            fnv_write(hash, &self.mode.to_le_bytes());
        }
        #[cfg(not(unix))]
        {
            fnv_write(hash, b"readonly");
            fnv_write(hash, &[u8::from(self.readonly)]);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StandaloneSnapshotEntry {
    bytes: Option<Vec<u8>>,
    permissions: StandalonePermissions,
}

/// Snapshot a complete subtree, materializing only links resolving within it.
/// The recursion stack detects directory aliases that introduce cycles.
fn standalone_snapshot(
    root: &Path,
) -> Result<BTreeMap<PathBuf, StandaloneSnapshotEntry>, ProjectionError> {
    fn visit(
        boundary: &Path,
        physical: &Path,
        relative: &Path,
        stack: &mut BTreeSet<PathBuf>,
        out: &mut BTreeMap<PathBuf, StandaloneSnapshotEntry>,
    ) -> Result<(), ProjectionError> {
        let resolved = standalone_resolve_inside(boundary, physical, &mut BTreeSet::new())
            .map_err(|error| {
                standalone_error(format!("unresolvable or cyclic source link: {error}"))
            })?;
        if !resolved.starts_with(boundary) {
            return Err(standalone_error("source link escapes the skill subtree"));
        }
        let metadata = fs::metadata(&resolved)?;
        let permissions = StandalonePermissions::from_metadata(&metadata)?;
        if metadata.is_dir() {
            if !stack.insert(resolved.clone()) {
                return Err(standalone_error("cyclic source directory link"));
            }
            out.insert(
                relative.to_path_buf(),
                StandaloneSnapshotEntry {
                    bytes: None,
                    permissions,
                },
            );
            let mut entries = fs::read_dir(&resolved)?.collect::<Result<Vec<_>, _>>()?;
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                if entry.file_name().to_str().is_none() {
                    return Err(standalone_error("non-Unicode source name cannot be proven"));
                }
                visit(
                    boundary,
                    &entry.path(),
                    &relative.join(entry.file_name()),
                    stack,
                    out,
                )?;
            }
            stack.remove(&resolved);
        } else if metadata.is_file() {
            out.insert(
                relative.to_path_buf(),
                StandaloneSnapshotEntry {
                    bytes: Some(fs::read(resolved)?),
                    permissions,
                },
            );
        } else {
            return Err(standalone_error("unsupported source entry"));
        }
        Ok(())
    }
    let root = standalone_lexical_path(&fs::canonicalize(root)?)?;
    if !root.is_dir() {
        return Err(standalone_error("source must be a directory"));
    }
    let mut out = BTreeMap::new();
    visit(&root, &root, Path::new(""), &mut BTreeSet::new(), &mut out)?;
    Ok(out)
}

fn standalone_snapshot_fingerprint(tree: &BTreeMap<PathBuf, StandaloneSnapshotEntry>) -> String {
    let mut hash = FNV_OFFSET_BASIS;
    for (relative, entry) in tree {
        fnv_write(&mut hash, if entry.bytes.is_some() { b"F" } else { b"D" });
        fnv_write(
            &mut hash,
            relative.to_string_lossy().replace('\\', "/").as_bytes(),
        );
        entry.permissions.fingerprint(&mut hash);
        if let Some(bytes) = &entry.bytes {
            fnv_write(&mut hash, &(bytes.len() as u64).to_le_bytes());
            fnv_write(&mut hash, bytes);
        }
    }
    format!("standalone-v1:fnv1a64:{hash:016x}")
}

pub(crate) fn standalone_source_fingerprint(source: &Path) -> Result<String, ProjectionError> {
    Ok(standalone_snapshot_fingerprint(&standalone_snapshot(
        source,
    )?))
}

fn standalone_file_fingerprint(bytes: &[u8]) -> String {
    let mut hash = FNV_OFFSET_BASIS;
    fnv_write(&mut hash, b"F");
    fnv_write(&mut hash, b"");
    fnv_write(&mut hash, &(bytes.len() as u64).to_le_bytes());
    fnv_write(&mut hash, bytes);
    format!("fnv1a64:{hash:016x}")
}

fn standalone_content_policy(
    request: &StandalonePublicationRequest,
    tree: &BTreeMap<PathBuf, StandaloneSnapshotEntry>,
) -> Result<(), ProjectionError> {
    if !tree
        .get(Path::new("SKILL.md"))
        .is_some_and(|entry| entry.bytes.is_some())
    {
        return Err(standalone_error("complete source requires SKILL.md"));
    }
    for (relative, entry) in tree {
        let Some(bytes) = &entry.bytes else { continue };
        let name = relative.file_name().and_then(OsStr::to_str).unwrap_or("");
        let discovery_manifest =
            matches!(name, "plugin.json" | "marketplace.json" | "extension.json")
                || (name == "SKILL.md" && relative != Path::new("SKILL.md"));
        if discovery_manifest {
            let matching: Vec<_> = request
                .manifest_proofs
                .iter()
                .filter(|proof| {
                    proof.path.join_to(Path::new("")) == *relative
                        && proof.source_id == request.source_id
                        && proof.revision == request.source_revision
                        && proof.fingerprint == standalone_file_fingerprint(bytes)
                        && request.targets.iter().all(|target| {
                            proof
                                .reader_versions
                                .get(target)
                                .is_some_and(|version| !version.trim().is_empty())
                        })
                })
                .collect();
            if matching.len() != 1 {
                return Err(standalone_error(format!(
                    "embedded manifest lacks matching provenance and reader-version proof: {}",
                    relative.display()
                )));
            }
        }
        // This is a bounded portable-subtree policy, not a language parser.
        // Recognizable parent references must resolve to an existing subtree
        // entry. Opaque binary assets remain byte-for-byte unchanged.
        if let Ok(text) = std::str::from_utf8(bytes) {
            if text.contains("CLAUDE_PLUGIN_ROOT")
                || text.contains("CODEX_PLUGIN_ROOT")
                || text.contains("CCYNC_PLUGIN_ROOT")
                || text.contains("plugin://")
                || text.contains("file://")
            {
                return Err(standalone_error(format!(
                    "cross-subtree or plugin-root resource reference: {}",
                    relative.display()
                )));
            }
            for token in text.split(|character: char| {
                character.is_whitespace()
                    || matches!(
                        character,
                        '\'' | '"'
                            | '`'
                            | '('
                            | ')'
                            | '['
                            | ']'
                            | '{'
                            | '}'
                            | '<'
                            | '>'
                            | '='
                            | ','
                            | ';'
                    )
            }) {
                if !token.contains("../") && !token.contains("..\\") {
                    continue;
                }
                let token = token
                    .split(['#', '?'])
                    .next()
                    .unwrap_or(token)
                    .replace('\\', "/");
                let mut resolved = relative.parent().unwrap_or(Path::new("")).to_path_buf();
                for part in token.split('/') {
                    match part {
                        ".." => {
                            if !resolved.pop() {
                                return Err(standalone_error(
                                    "resource escapes the complete skill subtree",
                                ));
                            }
                        }
                        "." | "" => {}
                        part => resolved.push(part),
                    }
                }
                if !tree.contains_key(&resolved) {
                    return Err(standalone_error(
                        "relative resource cannot be preserved inside skill subtree",
                    ));
                }
            }
        }
    }
    Ok(())
}

fn standalone_registry(lockfile: &Path) -> Result<ManagedArtifactRegistry, ProjectionError> {
    let mut report = ProjectionReport::default();
    let registry = ManagedArtifactRegistry::load(lockfile, &mut report);
    if !report.warnings.is_empty() || (standalone_exists(lockfile) && !lockfile.is_file()) {
        return Err(standalone_error("registry cannot be read safely"));
    }
    Ok(registry)
}

fn standalone_check_registry_namespace(
    registry: &ManagedArtifactRegistry,
    destination: &Path,
) -> Result<(), ProjectionError> {
    // A managed ancestor or descendant also reserves the namespace.
    let needle = normalize_managed_artifact_path(destination);
    for path in registry.prior.values().flatten() {
        if path == &needle {
            continue;
        }
        #[cfg(windows)]
        let overlaps = standalone_paths_overlap(
            &standalone_namespace_path(Path::new(path))?,
            &standalone_namespace_path(destination)?,
        )? || standalone_paths_overlap(
            &standalone_physical_path(Path::new(path))?,
            &standalone_physical_path(destination)?,
        )?;
        #[cfg(not(windows))]
        let overlaps = Path::new(path).starts_with(Path::new(&needle))
            || Path::new(&needle).starts_with(Path::new(path));
        // A Windows alias with a different registry key reserves the namespace.
        // Equality here never grants ownership through the exact-key lookup.
        if overlaps {
            return Err(standalone_error("managed path collision"));
        }
    }
    Ok(())
}

fn standalone_registry_row(registry: &ManagedArtifactRegistry, path: &Path) -> Value {
    let needle = normalize_managed_artifact_path(path);
    let categories: Vec<&str> = registry
        .prior
        .iter()
        .filter(|(_, paths)| paths.contains(&needle))
        .map(|(category, _)| category.as_str())
        .collect();
    serde_json::json!({"categories": categories, "source": registry.owning_source_of(path),
        "standalone": registry.standalone_proofs_prior.get(&needle)})
}

fn standalone_record_value(prepared: &PreparedStandalonePublication) -> Value {
    serde_json::json!({
        "path": prepared.destination,
        "sourceId": prepared.request.source_id,
        "sourceRevision": prepared.request.source_revision,
        "sourceFingerprint": prepared.intent.source_fingerprint,
        "treeFingerprint": prepared.intent.expected_tree_fingerprint,
        "targets": prepared.request.targets,
    })
}

pub(crate) fn standalone_publication_records(
    lockfile_path: &Path,
) -> Result<Vec<StandaloneProjectionRecord>, ProjectionError> {
    let registry = standalone_registry(lockfile_path)?;
    let mut records = Vec::new();
    for (path, proof) in &registry.standalone_proofs_prior {
        let record: StandaloneProjectionRecord =
            serde_json::from_value(proof.clone()).map_err(|error| {
                standalone_error(format!("invalid standalone registry proof: {error}"))
            })?;
        if normalize_managed_artifact_path(&record.path) != *path
            || !registry
                .prior
                .get(MANAGED_DIRECT_SKILL_PATHS)
                .is_some_and(|paths| paths.contains(path))
            || registry.owning_source_of(&record.path) != Some(record.source_id.as_str())
        {
            return Err(standalone_error(
                "standalone proof does not match registry ownership",
            ));
        }
        records.push(record);
    }
    Ok(records)
}

fn standalone_paths(prepared: &PreparedStandalonePublication) -> Result<(), ProjectionError> {
    let request = &prepared.request;
    for path in [
        request.build_root.as_path(),
        prepared
            .destination
            .parent()
            .ok_or_else(|| standalone_error("destination lacks parent"))?,
        prepared
            .stage
            .parent()
            .ok_or_else(|| standalone_error("stage lacks parent"))?,
        prepared
            .backup
            .parent()
            .ok_or_else(|| standalone_error("backup lacks parent"))?,
    ] {
        standalone_clean_ancestors(path)?;
    }
    for root in &request.discovery_roots {
        standalone_clean_ancestors(root)?;
        if !root.is_dir() {
            return Err(standalone_error("discovery root must already exist"));
        }
        let root = standalone_physical_path(root)?;
        for private in [&prepared.stage, &prepared.backup, &request.build_root] {
            let private = standalone_physical_path(private)?;
            if standalone_paths_overlap(&private, &root)? {
                return Err(standalone_error("staging overlaps discovery"));
            }
        }
    }
    let physical_destination = standalone_namespace_path(&prepared.destination)?;
    let mut discovered = false;
    for root in &request.discovery_roots {
        let root = standalone_physical_path(root)?;
        if standalone_direct_child(&physical_destination, &root)? {
            discovered = true;
        }
    }
    if !discovered {
        return Err(standalone_error(
            "destination must be directly inside a supplied discovery root",
        ));
    }
    for reserved in &request.reserved_paths {
        let reserved = standalone_physical_path(reserved)?;
        if standalone_paths_overlap(&reserved, &physical_destination)? {
            return Err(standalone_error("destination is delegated or reserved"));
        }
    }
    if !standalone_same_filesystem(&request.build_root, &prepared.destination)? {
        return Err(standalone_error(
            "staging and destination are on different filesystems",
        ));
    }
    let source = fs::canonicalize(&request.source_path)?;
    for private in [&prepared.stage, &prepared.backup] {
        let private = standalone_physical_path(private)?;
        if standalone_paths_overlap(&source, &private)? {
            return Err(standalone_error("source overlaps publication paths"));
        }
    }
    if standalone_paths_overlap(&source, &physical_destination)? {
        return Err(standalone_error("source overlaps destination"));
    }
    Ok(())
}

pub(crate) fn prepare_standalone_publication(
    request: StandalonePublicationRequest,
) -> Result<PreparedStandalonePublication, ProjectionError> {
    use ccync_foundation::provider::{
        IntentPhase, PackageDigest, PreimageRef, RelativePath, StandaloneLinkKind,
        StandalonePreimage, StandalonePublicationIntent,
    };
    if request.operation_id.trim().is_empty()
        || request.source_id.trim().is_empty()
        || request.source_id == "ccync"
        || request.source_revision.trim().is_empty()
        || request.targets.is_empty()
        || request
            .targets
            .iter()
            .any(|target| target.trim().is_empty())
    {
        return Err(standalone_error(
            "actual source, revision, operation and readers are required",
        ));
    }
    RelativePath::new(&request.operation_id)
        .map_err(|error| standalone_error(error.to_string()))?;
    if request.operation_id.contains(['/', '\\'])
        || request.build_root.file_name() != Some(OsStr::new("build"))
        || !request.build_root.is_dir()
        || request.stage_path.as_str()
            != format!("standalone-publications/{}/stage", request.operation_id)
        || request.backup_path.as_str()
            != format!("standalone-publications/{}/backup", request.operation_id)
    {
        return Err(standalone_error(
            "explicit build paths must use this operation's private subtree",
        ));
    }
    standalone_clean_ancestors(&request.destination_root)?;
    let destination = request.destination.path.join_to(&request.destination_root);
    let stage = request.stage_path.join_to(&request.build_root);
    let backup = request.backup_path.join_to(&request.build_root);
    let source_tree = standalone_snapshot(&request.source_path)?;
    standalone_content_policy(&request, &source_tree)?;
    let source_fingerprint = standalone_snapshot_fingerprint(&source_tree);
    let mut prepared = PreparedStandalonePublication {
        intent: StandalonePublicationIntent {
            operation_id: request.operation_id.clone(),
            source: serde_json::json!({"origin": request.source_id, "revision": request.source_revision}).to_string(),
            source_fingerprint: PackageDigest::new(&source_fingerprint).map_err(|error| standalone_error(error.to_string()))?,
            targets: request.targets.clone(),
            destination: request.destination.clone(),
            preimage: StandalonePreimage::Absent,
            link_kind: None, preimage_ref: None, preimage_fingerprint: None,
            stage_path: request.stage_path.clone(), backup_path: request.backup_path.clone(),
            expected_tree_fingerprint: PackageDigest::new(&source_fingerprint).map_err(|error| standalone_error(error.to_string()))?,
            registry_before: None, registry_after: None, phase: IntentPhase::Staged,
        },
        request, source_tree, destination_tree: None, destination, stage, backup,
    };
    standalone_paths(&prepared)?;
    if standalone_exists(&prepared.stage) || standalone_exists(&prepared.backup) {
        return Err(standalone_error(
            "private stage or backup is already occupied",
        ));
    }
    let registry = standalone_registry(&prepared.request.build_root.join("lock.json"))?;
    let row = standalone_registry_row(&registry, &prepared.destination);
    standalone_check_registry_namespace(&registry, &prepared.destination)?;
    let categories = row["categories"]
        .as_array()
        .ok_or_else(|| standalone_error("invalid registry row"))?;
    if categories.iter().any(|category| {
        !matches!(
            category.as_str(),
            Some(MANAGED_SKILL_PATHS | MANAGED_DIRECT_SKILL_PATHS | MANAGED_SOURCE_LINK_PATHS)
        )
    }) {
        return Err(standalone_error(
            "destination belongs to another artifact kind",
        ));
    }
    if standalone_exists(&prepared.destination) || !categories.is_empty() {
        let owner = prepared
            .request
            .prior_owner
            .as_deref()
            .ok_or_else(|| standalone_error("replacement requires committed ownership"))?;
        if categories.is_empty() || registry.owning_source_of(&prepared.destination) != Some(owner)
        {
            return Err(standalone_error("foreign or unattributed destination"));
        }
    }
    if standalone_exists(&prepared.destination) {
        let fingerprint = standalone_preimage_fingerprint(&prepared.destination)?;
        if !standalone_owned_preimage_matches(
            &prepared.request,
            &row,
            &prepared.destination,
            &fingerprint,
        ) {
            return Err(standalone_error(
                "destination differs from committed preimage",
            ));
        }
        let digest = PackageDigest::new(&fingerprint)
            .map_err(|error| standalone_error(error.to_string()))?;
        if is_symlink_or_junction(&prepared.destination) {
            prepared.intent.preimage = StandalonePreimage::Link {
                raw_target: fs::read_link(&prepared.destination)?,
            };
            prepared.intent.link_kind = Some(match classify_link(&prepared.destination) {
                LinkShape::Junction => StandaloneLinkKind::Junction,
                _ if fs::metadata(&prepared.destination)
                    .is_ok_and(|metadata| metadata.is_file()) =>
                {
                    StandaloneLinkKind::FileSymlink
                }
                _ => StandaloneLinkKind::DirectorySymlink,
            });
        } else if prepared.destination.is_dir() {
            prepared.destination_tree = Some(standalone_snapshot(&prepared.destination)?);
            prepared.intent.preimage = StandalonePreimage::Tree {
                fingerprint: digest.clone(),
            };
        } else {
            return Err(standalone_error(
                "unowned file shape cannot be replaced by a skill",
            ));
        }
        prepared.intent.preimage_fingerprint = Some(digest);
        prepared.intent.preimage_ref = Some(PreimageRef {
            path: prepared.request.destination.clone(),
            saved: prepared.request.backup_path.clone(),
        });
    } else if prepared.request.owned_fingerprint.is_some() {
        return Err(standalone_error("committed preimage is now absent"));
    }
    prepared.intent.registry_before = Some(row);
    prepared.intent.registry_after = Some(serde_json::json!({
        "categories": [MANAGED_DIRECT_SKILL_PATHS], "source": prepared.request.source_id,
        "standalone": standalone_record_value(&prepared)
    }));
    prepared
        .intent
        .validate()
        .map_err(|error| standalone_error(error.to_string()))?;
    standalone_journal_reservations(
        &standalone_read_journal(&prepared.request.build_root)?,
        (
            &prepared.request.destination,
            &prepared.request.destination_root,
        ),
        &prepared.request.build_root,
        &prepared.intent.operation_id,
        &[&prepared.destination, &prepared.stage, &prepared.backup],
        None,
        None,
    )?;
    Ok(prepared)
}

fn standalone_match_intent(
    prepared: &PreparedStandalonePublication,
    intent: &ccync_foundation::provider::StandalonePublicationIntent,
) -> Result<(), ProjectionError> {
    intent
        .validate()
        .map_err(|error| standalone_error(error.to_string()))?;
    let mut comparison = intent.clone();
    comparison.phase = prepared.intent.phase;
    if comparison != prepared.intent {
        return Err(standalone_error(
            "durable intent does not match prepared publication",
        ));
    }
    standalone_paths(prepared)
}

fn standalone_durable_intent(
    prepared: &PreparedStandalonePublication,
    intent: &ccync_foundation::provider::StandalonePublicationIntent,
    released: bool,
) -> Result<(), ProjectionError> {
    let journal = standalone_read_journal(&prepared.request.build_root)?;
    if !released
        && journal
            .standalone_intents
            .iter()
            .filter(|pending| *pending == intent)
            .count()
            != 1
    {
        return Err(standalone_error(
            "matching standalone intent is not durable",
        ));
    }
    standalone_journal_reservations(
        &journal,
        (
            &prepared.request.destination,
            &prepared.request.destination_root,
        ),
        &prepared.request.build_root,
        &intent.operation_id,
        &[&prepared.destination, &prepared.stage, &prepared.backup],
        if released { None } else { Some(intent) },
        None,
    )
}

/// Reload only states whose source, paths and ownership match the saved intent.
pub(crate) fn resume_standalone_publication(
    request: StandalonePublicationRequest,
    intent: &ccync_foundation::provider::StandalonePublicationIntent,
) -> Result<PreparedStandalonePublication, ProjectionError> {
    use ccync_foundation::provider::StandalonePreimage;
    // Reuse preparation's validation without mutating or removing pending data.
    // Its empty-path checks are replaced here by intent-bound recovery checks.
    if request.operation_id.contains(['/', '\\'])
        || request.build_root.file_name() != Some(OsStr::new("build"))
        || !request.build_root.is_dir()
        || request.stage_path.as_str()
            != format!("standalone-publications/{}/stage", request.operation_id)
        || request.backup_path.as_str()
            != format!("standalone-publications/{}/backup", request.operation_id)
        || request.source_id.trim().is_empty()
        || request.source_id == "ccync"
        || request.source_revision.trim().is_empty()
    {
        return Err(standalone_error(
            "invalid recovery source or private build paths",
        ));
    }
    let source_tree = standalone_snapshot(&request.source_path)?;
    standalone_content_policy(&request, &source_tree)?;
    let destination = request.destination.path.join_to(&request.destination_root);
    let stage = request.stage_path.join_to(&request.build_root);
    let backup = request.backup_path.join_to(&request.build_root);
    let source_proof =
        serde_json::json!({"origin": request.source_id, "revision": request.source_revision})
            .to_string();
    if intent.operation_id != request.operation_id
        || intent.destination != request.destination
        || intent.stage_path != request.stage_path
        || intent.backup_path != request.backup_path
        || intent.targets != request.targets
        || intent.source != source_proof
        || intent.source_fingerprint.as_str() != standalone_snapshot_fingerprint(&source_tree)
        || intent.expected_tree_fingerprint != intent.source_fingerprint
    {
        return Err(standalone_error(
            "recovery request differs from saved intent",
        ));
    }
    let destination_tree = if matches!(intent.preimage, StandalonePreimage::Tree { .. }) {
        let preimage = if standalone_exists(&backup) {
            &backup
        } else {
            &destination
        };
        // Older pending intents may have accepted links inside a Tree. Keep
        // their evidence and block rather than reinterpret relocated targets.
        standalone_no_tree_links(preimage)?;
        Some(standalone_snapshot(preimage)?)
    } else {
        None
    };
    let prepared = PreparedStandalonePublication {
        request,
        intent: intent.clone(),
        source_tree,
        destination_tree,
        destination,
        stage,
        backup,
    };
    if intent.registry_after
        != Some(serde_json::json!({
            "categories": [MANAGED_DIRECT_SKILL_PATHS], "source": prepared.request.source_id,
            "standalone": standalone_record_value(&prepared)
        }))
    {
        return Err(standalone_error(
            "saved ownership completion proof does not match recovery source",
        ));
    }
    let preimage = if standalone_exists(&prepared.backup) {
        &prepared.backup
    } else {
        &prepared.destination
    };
    match intent.preimage_fingerprint.as_ref() {
        Some(fingerprint)
            if !standalone_owned_preimage_matches(
                &prepared.request,
                intent
                    .registry_before
                    .as_ref()
                    .ok_or_else(|| standalone_error("missing saved registry proof"))?,
                preimage,
                fingerprint.as_str(),
            ) =>
        {
            return Err(standalone_error(
                "recovery ownership proof differs from saved intent",
            ))
        }
        None if prepared.request.owned_fingerprint.is_some() => {
            return Err(standalone_error(
                "absent preimage has ownership fingerprint",
            ))
        }
        _ => {}
    }
    standalone_match_intent(&prepared, intent)?;
    standalone_durable_intent(&prepared, intent, false)?;
    if standalone_check_registry(&prepared, false).is_err() {
        standalone_check_registry(&prepared, true)?;
    }
    if standalone_exists(&prepared.backup) {
        standalone_check_preimage(&prepared, &prepared.backup)?;
        if standalone_exists(&prepared.destination) {
            standalone_check_new_tree(&prepared, &prepared.destination)?;
        }
    } else if !matches!(intent.preimage, StandalonePreimage::Absent) {
        standalone_check_preimage(&prepared, &prepared.destination)?;
    } else if standalone_exists(&prepared.destination) {
        standalone_check_new_tree(&prepared, &prepared.destination)?;
    }
    if standalone_exists(&prepared.stage) {
        standalone_check_new_tree(&prepared, &prepared.stage)?;
    }
    Ok(prepared)
}

fn standalone_check_source(
    prepared: &PreparedStandalonePublication,
) -> Result<(), ProjectionError> {
    if standalone_snapshot(&prepared.request.source_path)? != prepared.source_tree {
        return Err(standalone_error("source changed since preparation"));
    }
    Ok(())
}

fn standalone_check_registry(
    prepared: &PreparedStandalonePublication,
    after: bool,
) -> Result<(), ProjectionError> {
    let registry = standalone_registry(&prepared.request.build_root.join("lock.json"))?;
    standalone_check_registry_namespace(&registry, &prepared.destination)?;
    let expected = if after {
        &prepared.intent.registry_after
    } else {
        &prepared.intent.registry_before
    };
    if Some(standalone_registry_row(&registry, &prepared.destination)) != *expected {
        return Err(standalone_error(
            "recovery blocked: registry ownership drift",
        ));
    }
    Ok(())
}

fn standalone_no_tree_links(path: &Path) -> Result<(), ProjectionError> {
    if is_symlink_or_junction(path) {
        return Err(standalone_error("tree contains a link"));
    }
    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            standalone_no_tree_links(&entry?.path())?;
        }
    }
    Ok(())
}

fn standalone_preimage_fingerprint(path: &Path) -> Result<String, ProjectionError> {
    if is_symlink_or_junction(path) {
        shared_path_fingerprint(path).ok_or_else(|| standalone_error("unreadable link preimage"))
    } else {
        // Tree preimages must remain verifiable after a raw rename to backup.
        // Root Link preimages retain their separate raw-link proof above.
        standalone_no_tree_links(path)?;
        standalone_source_fingerprint(path)
    }
}

fn standalone_owned_preimage_matches(
    request: &StandalonePublicationRequest,
    registry_before: &Value,
    path: &Path,
    fingerprint: &str,
) -> bool {
    let owned = request.owned_fingerprint.as_deref();
    if !registry_before["standalone"].is_null() {
        // A standalone row already has a permission-aware committed proof.
        // Never downgrade its protection to a freshly observed shared digest.
        let committed = registry_before["standalone"]["treeFingerprint"].as_str();
        committed == Some(fingerprint) && owned == committed
    } else {
        owned == Some(fingerprint)
            || (owned.is_some() && owned == shared_path_fingerprint(path).as_deref())
    }
}

fn standalone_check_preimage(
    prepared: &PreparedStandalonePublication,
    path: &Path,
) -> Result<(), ProjectionError> {
    use ccync_foundation::provider::StandalonePreimage;
    if let StandalonePreimage::Absent = &prepared.intent.preimage {
        if standalone_exists(path) {
            return Err(standalone_error(
                "recovery blocked: unexpected destination or backup",
            ));
        }
        return Ok(());
    }
    if Some(standalone_preimage_fingerprint(path)?.as_str())
        != prepared
            .intent
            .preimage_fingerprint
            .as_ref()
            .map(|digest| digest.as_str())
    {
        return Err(standalone_error(
            "recovery blocked: preimage fingerprint drift",
        ));
    }
    match &prepared.intent.preimage {
        StandalonePreimage::Link { raw_target } => {
            if !is_symlink_or_junction(path) || fs::read_link(path)? != *raw_target {
                return Err(standalone_error("recovery blocked: link preimage drift"));
            }
        }
        StandalonePreimage::Tree { .. } => {
            if is_symlink_or_junction(path)
                || Some(standalone_snapshot(path)?) != prepared.destination_tree
            {
                return Err(standalone_error("recovery blocked: exact preimage drift"));
            }
        }
        StandalonePreimage::Absent => {}
    }
    Ok(())
}

fn standalone_check_new_tree(
    prepared: &PreparedStandalonePublication,
    path: &Path,
) -> Result<(), ProjectionError> {
    // A published tree contains no links, including file links or junctions.
    standalone_no_tree_links(path)?;
    if standalone_snapshot(path)? != prepared.source_tree {
        return Err(standalone_error(
            "new tree differs from prepared complete content",
        ));
    }
    Ok(())
}

pub(crate) fn stage_standalone_publication(
    prepared: &PreparedStandalonePublication,
    intent: &ccync_foundation::provider::StandalonePublicationIntent,
) -> Result<(), ProjectionError> {
    standalone_match_intent(prepared, intent)?;
    standalone_durable_intent(prepared, intent, false)?;
    standalone_check_registry(prepared, false)?;
    standalone_check_source(prepared)?;
    standalone_check_preimage(prepared, &prepared.destination)?;
    if standalone_exists(&prepared.backup) {
        return Err(standalone_error("private backup is occupied"));
    }
    if standalone_exists(&prepared.stage) {
        return standalone_check_new_tree(prepared, &prepared.stage);
    }
    fs::create_dir_all(
        prepared
            .stage
            .parent()
            .ok_or_else(|| standalone_error("stage lacks parent"))?,
    )?;
    // Materialize the validated snapshot. Directory entries precede their children.
    fs::create_dir(&prepared.stage)?;
    for (relative, entry) in &prepared.source_tree {
        if relative.as_os_str().is_empty() {
            continue;
        }
        let path = prepared.stage.join(relative);
        if let Some(bytes) = &entry.bytes {
            fs::write(&path, bytes)?;
            entry.permissions.apply(&path)?;
        } else {
            fs::create_dir(&path)?;
        }
    }
    // Apply directory permissions only after children exist. Reverse path order
    // visits descendants first and includes the stage root last.
    for (relative, entry) in prepared.source_tree.iter().rev() {
        if entry.bytes.is_none() {
            entry.permissions.apply(&prepared.stage.join(relative))?;
        }
    }
    standalone_check_new_tree(prepared, &prepared.stage)
}

pub(crate) fn publish_standalone_publication(
    prepared: &PreparedStandalonePublication,
    intent: &ccync_foundation::provider::StandalonePublicationIntent,
) -> Result<(), ProjectionError> {
    standalone_match_intent(prepared, intent)?;
    standalone_durable_intent(prepared, intent, false)?;
    if standalone_check_registry(prepared, false).is_err() {
        standalone_check_registry(prepared, true)?;
    }
    standalone_check_source(prepared)?;
    if standalone_exists(&prepared.destination)
        && standalone_check_new_tree(prepared, &prepared.destination).is_ok()
        && !standalone_exists(&prepared.stage)
    {
        return standalone_check_preimage(prepared, &prepared.backup);
    }
    standalone_check_new_tree(prepared, &prepared.stage)?;
    let had_preimage = standalone_exists(&prepared.destination);
    if had_preimage {
        standalone_check_preimage(prepared, &prepared.destination)?;
        if standalone_exists(&prepared.backup) {
            return Err(standalone_error("backup is already occupied"));
        }
        fs::rename(&prepared.destination, &prepared.backup)?;
    } else {
        // A process may stop after retaining the old tree but before switching.
        standalone_check_preimage(prepared, &prepared.backup)?;
    }
    if let Err(error) = fs::rename(&prepared.stage, &prepared.destination) {
        // Rename failure preserves evidence. Restore only while both paths
        // still match this operation; otherwise recovery must inspect them.
        if had_preimage
            && !standalone_exists(&prepared.destination)
            && standalone_check_preimage(prepared, &prepared.backup).is_ok()
        {
            let _ = fs::rename(&prepared.backup, &prepared.destination);
        }
        return Err(error.into());
    }
    standalone_check_new_tree(prepared, &prepared.destination)?;
    standalone_check_preimage(prepared, &prepared.backup)
}

pub(crate) fn complete_standalone_publication(
    prepared: &PreparedStandalonePublication,
    intent: &ccync_foundation::provider::StandalonePublicationIntent,
) -> Result<(), ProjectionError> {
    standalone_match_intent(prepared, intent)?;
    standalone_durable_intent(prepared, intent, false)?;
    standalone_check_new_tree(prepared, &prepared.destination)?;
    standalone_check_preimage(prepared, &prepared.backup)?;
    if standalone_check_registry(prepared, true).is_ok() {
        return Ok(());
    }
    standalone_check_registry(prepared, false)?;
    let lockfile = prepared.request.build_root.join("lock.json");
    let mut registry = standalone_registry(&lockfile)?;
    let needle = normalize_managed_artifact_path(&prepared.destination);
    for paths in registry.next.values_mut() {
        paths.remove(&needle);
    }
    registry.mark_with_source(
        MANAGED_DIRECT_SKILL_PATHS,
        &prepared.destination,
        &prepared.request.source_id,
    );
    registry
        .standalone_proofs_next
        .insert(needle, standalone_record_value(prepared));
    registry.persist(&[], false, &mut ProjectionReport::default())?;
    standalone_check_registry(prepared, true)
}

pub(crate) fn cleanup_standalone_publication(
    prepared: &PreparedStandalonePublication,
    intent: &ccync_foundation::provider::StandalonePublicationIntent,
) -> Result<(), ProjectionError> {
    standalone_match_intent(prepared, intent)?;
    standalone_durable_intent(prepared, intent, true)?;
    standalone_check_registry(prepared, true)?;
    standalone_check_new_tree(prepared, &prepared.destination)?;
    if !standalone_exists(&prepared.backup) {
        return Ok(());
    }
    standalone_check_preimage(prepared, &prepared.backup)?;
    if is_symlink_or_junction(&prepared.backup) {
        remove_link(&prepared.backup)?;
    } else {
        fs::remove_dir_all(&prepared.backup)?;
    }
    Ok(())
}

fn standalone_withdrawal_paths(
    request: &StandaloneWithdrawalRequest,
    intent: Option<&ccync_foundation::provider::StandaloneWithdrawalIntent>,
) -> Result<(PathBuf, PathBuf), ProjectionError> {
    use ccync_foundation::provider::RelativePath;
    RelativePath::new(&request.operation_id)
        .map_err(|error| standalone_error(error.to_string()))?;
    if request.operation_id.contains(['/', '\\'])
        || request.build_root.file_name() != Some(OsStr::new("build"))
        || !fs::metadata(&request.build_root)?.is_dir()
        || request.backup_path.as_str()
            != format!("standalone-withdrawals/{}/backup", request.operation_id)
    {
        return Err(standalone_error(
            "invalid withdrawal operation or private backup path",
        ));
    }
    let destination = request.destination.path.join_to(&request.destination_root);
    let backup = request.backup_path.join_to(&request.build_root);
    for path in [
        &request.destination_root,
        &request.build_root,
        destination
            .parent()
            .ok_or_else(|| standalone_error("destination lacks parent"))?,
        backup
            .parent()
            .ok_or_else(|| standalone_error("backup lacks parent"))?,
    ] {
        for ancestor in path.ancestors() {
            standalone_present(ancestor)?;
        }
        standalone_clean_ancestors(path)?;
    }
    let namespace = standalone_namespace_path(&destination)?;
    // A retained raw Link backup reserves its intent-anchored referent.
    let retained = intent.map(|intent| (destination.as_path(), &intent.preimage, intent.link_kind));
    let mut discovered = false;
    for root in &request.discovery_roots {
        for ancestor in root.ancestors() {
            standalone_present(ancestor)?;
        }
        standalone_clean_ancestors(root)?;
        if !fs::metadata(root)?.is_dir() {
            return Err(standalone_error("discovery root must already exist"));
        }
        let root = standalone_physical_path(root)?;
        discovered |= standalone_direct_child(&namespace, &root)?;
        if standalone_reservations_overlap_with_preimages(&backup, &root, retained, None)?
            || standalone_reservations_overlap(&request.build_root, &root)?
        {
            return Err(standalone_error("withdrawal backup overlaps discovery"));
        }
    }
    if !discovered {
        return Err(standalone_error(
            "withdrawal destination is outside discovery",
        ));
    }
    for reserved in &request.reserved_paths {
        if standalone_reservations_overlap(reserved, &destination)?
            || standalone_reservations_overlap_with_preimages(reserved, &backup, None, retained)?
        {
            return Err(standalone_error("withdrawal path is reserved"));
        }
    }
    if !standalone_same_filesystem(&request.build_root, &destination)?
        || !standalone_same_filesystem(&backup, &destination)?
    {
        return Err(standalone_error(
            "withdrawal backup is on another filesystem",
        ));
    }
    if standalone_reservations_overlap_with_preimages(&backup, &destination, retained, None)? {
        return Err(standalone_error("withdrawal backup overlaps destination"));
    }
    Ok((destination, backup))
}

fn standalone_withdrawal_registry(
    prepared: &PreparedStandaloneWithdrawal,
    after: bool,
) -> Result<ManagedArtifactRegistry, ProjectionError> {
    let lockfile = prepared.request.build_root.join("lock.json");
    if standalone_present(&lockfile)?
        && (is_symlink_or_junction(&lockfile) || !fs::metadata(&lockfile)?.is_file())
    {
        return Err(standalone_error(
            "withdrawal registry is not a regular file",
        ));
    }
    let registry = standalone_registry(&lockfile)?;
    standalone_check_registry_namespace(&registry, &prepared.destination)?;
    let expected = if after {
        &prepared.intent.registry_after
    } else {
        &prepared.intent.registry_before
    };
    if standalone_registry_row(&registry, &prepared.destination) != *expected {
        return Err(standalone_error(
            "recovery blocked: withdrawal registry drift",
        ));
    }
    Ok(registry)
}

fn standalone_withdrawal_match(
    prepared: &PreparedStandaloneWithdrawal,
    intent: &ccync_foundation::provider::StandaloneWithdrawalIntent,
) -> Result<(), ProjectionError> {
    intent
        .validate()
        .map_err(|error| standalone_error(error.to_string()))?;
    let mut comparison = intent.clone();
    comparison.phase = prepared.intent.phase;
    if comparison != prepared.intent {
        return Err(standalone_error(
            "withdrawal intent differs from preparation",
        ));
    }
    let (destination, backup) = standalone_withdrawal_paths(&prepared.request, Some(intent))?;
    if destination != prepared.destination || backup != prepared.backup {
        return Err(standalone_error("withdrawal path binding changed"));
    }
    Ok(())
}

fn standalone_withdrawal_durable(
    prepared: &PreparedStandaloneWithdrawal,
    intent: &ccync_foundation::provider::StandaloneWithdrawalIntent,
    released: bool,
) -> Result<(), ProjectionError> {
    standalone_withdrawal_match(prepared, intent)?;
    let journal = standalone_read_journal(&prepared.request.build_root)?;
    if !released
        && journal
            .standalone_withdrawal_intents
            .iter()
            .filter(|pending| *pending == intent)
            .count()
            != 1
    {
        return Err(standalone_error(
            "matching withdrawal intent is not durable",
        ));
    }
    standalone_journal_reservations(
        &journal,
        (
            &prepared.request.destination,
            &prepared.request.destination_root,
        ),
        &prepared.request.build_root,
        &intent.operation_id,
        &[&prepared.destination, &prepared.backup],
        None,
        if released { None } else { Some(intent) },
    )
}

fn standalone_withdrawal_link_target(
    destination: &Path,
    raw_target: &Path,
) -> Result<PathBuf, ProjectionError> {
    Ok(if raw_target.is_absolute() {
        raw_target.to_path_buf()
    } else {
        destination
            .parent()
            .ok_or_else(|| standalone_error("link lacks parent"))?
            .join(raw_target)
    })
}

fn standalone_withdrawal_preimage(
    prepared: &PreparedStandaloneWithdrawal,
    path: &Path,
) -> Result<(), ProjectionError> {
    use ccync_foundation::provider::{StandaloneLinkKind, StandalonePreimage};
    if !standalone_present(path)? {
        return Err(standalone_error(
            "recovery blocked: missing withdrawal preimage",
        ));
    }
    let tree = match &prepared.intent.preimage {
        StandalonePreimage::Tree { .. } => {
            standalone_no_tree_links(path)?;
            standalone_snapshot(path)?
        }
        StandalonePreimage::Link { raw_target } => {
            if !is_symlink_or_junction(path) || fs::read_link(path)? != *raw_target {
                return Err(standalone_error("recovery blocked: withdrawal link drift"));
            }
            let target = standalone_withdrawal_link_target(&prepared.destination, raw_target)?;
            let kind = if classify_link(path) == LinkShape::Junction {
                StandaloneLinkKind::Junction
            } else if fs::metadata(&target)?.is_file() {
                StandaloneLinkKind::FileSymlink
            } else {
                StandaloneLinkKind::DirectorySymlink
            };
            if prepared.intent.link_kind != Some(kind) {
                return Err(standalone_error(
                    "recovery blocked: withdrawal link kind drift",
                ));
            }
            standalone_snapshot(&target)?
        }
        StandalonePreimage::Absent => {
            return Err(standalone_error("withdrawal preimage cannot be absent"))
        }
    };
    if standalone_snapshot_fingerprint(&tree) != prepared.intent.preimage_fingerprint.as_str()
        || prepared.destination_tree.as_ref() != Some(&tree)
    {
        return Err(standalone_error(
            "recovery blocked: withdrawal preimage drift",
        ));
    }
    Ok(())
}

fn standalone_withdrawal_binding(
    request: &StandaloneWithdrawalRequest,
    intent: &ccync_foundation::provider::StandaloneWithdrawalIntent,
    destination: &Path,
) -> Result<(), ProjectionError> {
    intent
        .validate()
        .map_err(|error| standalone_error(error.to_string()))?;
    if request.committed_record.source_id == "ccync"
        || request.operation_id != intent.operation_id
        || request.readers != intent.readers
        || request.destination != intent.destination
        || request.backup_path != intent.backup_path
        || request.committed_record.path != destination
        || serde_json::to_value(&request.committed_record)
            .map_err(|error| standalone_error(error.to_string()))?
            != intent.registry_before["standalone"]
        || intent.registry_before["categories"] != serde_json::json!([MANAGED_DIRECT_SKILL_PATHS])
    {
        return Err(standalone_error(
            "withdrawal request differs from committed intent",
        ));
    }
    // The registry schema is unchanged. Content-only legacy proofs cannot
    // acquire standalone withdrawal authority through a fresh observation.
    if !request
        .committed_record
        .tree_fingerprint
        .starts_with("standalone-v1:fnv1a64:")
    {
        return Err(standalone_error(
            "withdrawal requires permission-aware committed proof",
        ));
    }
    Ok(())
}

pub(crate) fn prepare_standalone_withdrawal(
    request: StandaloneWithdrawalRequest,
) -> Result<PreparedStandaloneWithdrawal, ProjectionError> {
    use ccync_foundation::provider::{
        IntentPhase, PackageDigest, PreimageRef, StandaloneLinkKind, StandalonePreimage,
        StandaloneWithdrawalIntent,
    };
    let (destination, backup) = standalone_withdrawal_paths(&request, None)?;
    if standalone_present(&backup)? || !standalone_present(&destination)? {
        return Err(standalone_error(
            "withdrawal requires a present destination and absent backup",
        ));
    }
    let journal = standalone_read_journal(&request.build_root)?;
    standalone_journal_reservations(
        &journal,
        (&request.destination, &request.destination_root),
        &request.build_root,
        &request.operation_id,
        &[&destination, &backup],
        None,
        None,
    )?;
    let digest = PackageDigest::new(&request.committed_record.tree_fingerprint)
        .map_err(|error| standalone_error(error.to_string()))?;
    let (preimage, link_kind, tree) = if is_symlink_or_junction(&destination) {
        let raw_target = fs::read_link(&destination)?;
        let target = standalone_withdrawal_link_target(&destination, &raw_target)?;
        let kind = if classify_link(&destination) == LinkShape::Junction {
            StandaloneLinkKind::Junction
        } else if fs::metadata(&target)?.is_file() {
            StandaloneLinkKind::FileSymlink
        } else {
            StandaloneLinkKind::DirectorySymlink
        };
        (
            StandalonePreimage::Link { raw_target },
            Some(kind),
            standalone_snapshot(&target)?,
        )
    } else {
        standalone_no_tree_links(&destination)?;
        (
            StandalonePreimage::Tree {
                fingerprint: digest.clone(),
            },
            None,
            standalone_snapshot(&destination)?,
        )
    };
    let registry = standalone_registry(&request.build_root.join("lock.json"))?;
    let intent = StandaloneWithdrawalIntent {
        operation_id: request.operation_id.clone(),
        source_id: request.committed_record.source_id.clone(),
        source_revision: request.committed_record.source_revision.clone(),
        source_fingerprint: PackageDigest::new(&request.committed_record.source_fingerprint)
            .map_err(|error| standalone_error(error.to_string()))?,
        readers: request.readers.clone(),
        destination: request.destination.clone(),
        preimage,
        link_kind,
        preimage_ref: PreimageRef {
            path: request.destination.clone(),
            saved: request.backup_path.clone(),
        },
        preimage_fingerprint: digest,
        backup_path: request.backup_path.clone(),
        registry_before: standalone_registry_row(&registry, &destination),
        registry_after: serde_json::json!({"categories": [], "source": null, "standalone": null}),
        phase: IntentPhase::Staged,
    };
    standalone_withdrawal_binding(&request, &intent, &destination)?;
    let prepared = PreparedStandaloneWithdrawal {
        request,
        intent,
        destination_tree: Some(tree),
        destination,
        backup,
    };
    standalone_withdrawal_registry(&prepared, false)?;
    standalone_withdrawal_preimage(&prepared, &prepared.destination)?;
    Ok(prepared)
}

pub(crate) fn resume_standalone_withdrawal(
    request: StandaloneWithdrawalRequest,
    intent: &ccync_foundation::provider::StandaloneWithdrawalIntent,
) -> Result<PreparedStandaloneWithdrawal, ProjectionError> {
    use ccync_foundation::provider::StandalonePreimage;
    let (destination, backup) = standalone_withdrawal_paths(&request, Some(intent))?;
    standalone_withdrawal_binding(&request, intent, &destination)?;
    let present_destination = standalone_present(&destination)?;
    let present_backup = standalone_present(&backup)?;
    if present_destination == present_backup {
        return Err(standalone_error(
            "recovery blocked: ambiguous withdrawal preimage locations",
        ));
    }
    let path = if present_backup {
        &backup
    } else {
        &destination
    };
    let tree = match &intent.preimage {
        StandalonePreimage::Link { raw_target } => standalone_snapshot(
            &standalone_withdrawal_link_target(&destination, raw_target)?,
        )?,
        StandalonePreimage::Tree { .. } => {
            standalone_no_tree_links(path)?;
            standalone_snapshot(path)?
        }
        StandalonePreimage::Absent => {
            return Err(standalone_error("withdrawal preimage cannot be absent"))
        }
    };
    let prepared = PreparedStandaloneWithdrawal {
        request,
        intent: intent.clone(),
        destination_tree: Some(tree),
        destination,
        backup,
    };
    standalone_withdrawal_durable(&prepared, intent, false)?;
    standalone_withdrawal_preimage(
        &prepared,
        if present_backup {
            &prepared.backup
        } else {
            &prepared.destination
        },
    )?;
    if standalone_withdrawal_registry(&prepared, false).is_err() {
        standalone_withdrawal_registry(&prepared, true)?;
        if present_destination {
            return Err(standalone_error(
                "recovery blocked: destination reappeared after withdrawal commit",
            ));
        }
    }
    Ok(prepared)
}

pub(crate) fn apply_standalone_withdrawal(
    prepared: &PreparedStandaloneWithdrawal,
    intent: &ccync_foundation::provider::StandaloneWithdrawalIntent,
) -> Result<(), ProjectionError> {
    standalone_withdrawal_durable(prepared, intent, false)?;
    if !standalone_present(&prepared.destination)? {
        standalone_withdrawal_preimage(prepared, &prepared.backup)?;
        if standalone_withdrawal_registry(prepared, false).is_err() {
            standalone_withdrawal_registry(prepared, true)?;
        }
        return Ok(());
    }
    standalone_withdrawal_registry(prepared, false)?;
    standalone_withdrawal_preimage(prepared, &prepared.destination)?;
    if standalone_present(&prepared.backup)? {
        return Err(standalone_error("withdrawal backup is already occupied"));
    }
    fs::create_dir_all(
        prepared
            .backup
            .parent()
            .ok_or_else(|| standalone_error("backup lacks parent"))?,
    )?;
    // The durable proof is checked again immediately before the live rename.
    standalone_withdrawal_durable(prepared, intent, false)?;
    standalone_withdrawal_registry(prepared, false)?;
    standalone_withdrawal_preimage(prepared, &prepared.destination)?;
    if standalone_present(&prepared.backup)? {
        return Err(standalone_error("withdrawal backup became occupied"));
    }
    fs::rename(&prepared.destination, &prepared.backup)?;
    if standalone_present(&prepared.destination)? {
        return Err(standalone_error(
            "recovery blocked: withdrawal destination remains present",
        ));
    }
    standalone_withdrawal_preimage(prepared, &prepared.backup)
}

pub(crate) fn complete_standalone_withdrawal(
    prepared: &PreparedStandaloneWithdrawal,
    intent: &ccync_foundation::provider::StandaloneWithdrawalIntent,
) -> Result<(), ProjectionError> {
    standalone_withdrawal_durable(prepared, intent, false)?;
    if standalone_present(&prepared.destination)? {
        return Err(standalone_error(
            "recovery blocked: withdrawal destination is present",
        ));
    }
    standalone_withdrawal_preimage(prepared, &prepared.backup)?;
    if standalone_withdrawal_registry(prepared, true).is_ok() {
        return Ok(());
    }
    let mut registry = standalone_withdrawal_registry(prepared, false)?;
    let needle = normalize_managed_artifact_path(&prepared.destination);
    for paths in registry.next.values_mut() {
        paths.remove(&needle);
    }
    registry.source_attribution_next.remove(&needle);
    registry.standalone_proofs_next.remove(&needle);
    registry.persist(&[], false, &mut ProjectionReport::default())?;
    standalone_withdrawal_registry(prepared, true)?;
    Ok(())
}

pub(crate) fn cleanup_standalone_withdrawal(
    prepared: &PreparedStandaloneWithdrawal,
    intent: &ccync_foundation::provider::StandaloneWithdrawalIntent,
) -> Result<(), ProjectionError> {
    standalone_withdrawal_durable(prepared, intent, true)?;
    standalone_withdrawal_registry(prepared, true)?;
    if standalone_present(&prepared.destination)? {
        return Err(standalone_error(
            "recovery blocked: withdrawal destination reappeared",
        ));
    }
    if !standalone_present(&prepared.backup)? {
        return Ok(());
    }
    standalone_withdrawal_preimage(prepared, &prepared.backup)?;
    if is_symlink_or_junction(&prepared.backup) {
        remove_link(&prepared.backup)?;
    } else {
        fs::remove_dir_all(&prepared.backup)?;
    }
    Ok(())
}

/// Remove `.ccync-stage-*` siblings of `dest` left behind by an earlier run
/// that was killed between staging and swapping (a signal, a power loss —
/// the cases no in-process cleanup can reach). Best-effort and silent: a
/// sweep failure must never fail the publish that follows it. Safe to run
/// unconditionally because no live publish ever depends on a staging
/// directory it did not create in the same call — [`stage_tree_copy`]
/// returns its staged path directly to [`publish_staged_tree_copy`], which
/// consumes it before returning.
pub(crate) fn sweep_staging_orphans(dest: &Path) {
    let Some(parent) = dest.parent() else {
        return;
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(".ccync-stage-") {
            continue;
        }
        let path = entry.path();
        if is_symlink_or_junction(&path) {
            let _ = remove_link(&path);
        } else if path.is_dir() {
            let _ = fs::remove_dir_all(&path);
        } else {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Two-rung publish for an `agy-*` plugin root: try a symbolic link
/// ([`create_symlink_dir`]) first, and only on refusal fall back to a staged
/// directory copy ([`publish_staged_tree_copy`]). Never leaves an NTFS
/// junction at `dest` — either rung replaces one outright.
///
/// What already occupies `dest` is read with [`classify_link`], never
/// `exists()`, so a junction whose target has been deleted (which reports
/// `exists() == false` while still occupying the path) is still recognized
/// and replaced. A real, non-link directory is the one shape this function
/// will not touch on its own authority: it is preserved untouched with a
/// warning unless `ownership` already recorded it — `AgyRootOwnership::is_owned`
/// is `false` on an empty or missing lockfile, so an unrecorded directory is
/// preserved even on a machine with no lockfile at all.
///
/// Ownership is recorded through `ownership` before anything at `dest` is
/// touched — durable on disk ahead of both the link removal and the
/// swap/rename that makes the new tree visible — so no crash between the row
/// write and that visibility step can produce a tree without its row, and no
/// successful call can produce a row without (eventually) a tree.
///
/// `dry_run` performs zero disk writes and zero lockfile writes. It still
/// reports the shape a real run would produce where that shape is already
/// knowable without writing anything: an existing owned directory always
/// wins rung 2 (attempting a symlink over an occupied path always fails,
/// independent of platform capability), so `dry_run` predicts `Directory`
/// there; every other starting shape gets the optimistic rung-1 prediction,
/// since capability is only ever known by attempting the real write
/// (deliberately not probed or cached — see the plan's Approach).
///
/// Returns the shape now at `dest` **and** whether this call actually wrote
/// there — the same bare-`bool`-means-wrote convention [`write_text`] and
/// [`publish_staged_tree_copy`] already use in this file. `false` means the
/// path already held exactly the right thing, was preserved as user-owned,
/// or was a `dry_run`; callers must record no path evidence in that case, so
/// a stable rerun reports unchanged instead of a spurious change. The flag
/// describes `dest` only: `ownership.record_now` may still have merged a row
/// into the lockfile, which is separate registry evidence, not a change to
/// the published artifact.
#[allow(dead_code)] // additive, unwired — production callers land in a later task
pub(crate) fn publish_agy_root(
    source: &Path,
    dest: &Path,
    ownership: &AgyRootOwnership,
    dry_run: bool,
    report: &mut ProjectionReport,
) -> Result<(LinkShape, bool), ProjectionError> {
    let existing = classify_link(dest);
    if existing == LinkShape::Symlink && same_path(dest, source) {
        return Ok((LinkShape::Symlink, false));
    }
    if existing == LinkShape::Directory && !ownership.is_owned(dest) {
        report.warnings.push(ProjectionWarning {
            message: format!("preserved user-owned path: {}", dest.display()),
        });
        return Ok((LinkShape::Directory, false));
    }

    if dry_run {
        let predicted = if existing == LinkShape::Directory {
            LinkShape::Directory
        } else {
            LinkShape::Symlink
        };
        return Ok((predicted, false));
    }

    ownership.record_now(dest, dry_run)?;

    // From here the row is durable on disk. Row-before-tree is the required
    // ordering, but it only stays harmless while a failure leaves the
    // path absent: an orphan row is a standing `remove_dir_all` authority
    // over whatever later occupies this path. So every exit below that ends
    // without a tree releases the row again.
    let published = (|| -> Result<(LinkShape, bool), ProjectionError> {
        if is_symlink_or_junction(dest) {
            remove_link(dest)?;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        sweep_staging_orphans(dest);
        if let Ok(shape) = create_symlink_dir(source, dest) {
            return Ok((shape, true));
        }

        // `publish_staged_tree_copy` already compares before swapping and
        // reports whether it actually swapped; propagate that rather than
        // discarding it, or an unchanged copy would read back as a fresh
        // write on every sync.
        let swapped = publish_staged_tree_copy(source, dest)?;
        Ok((LinkShape::Directory, swapped))
    })();

    if published.is_err() && classify_link(dest) == LinkShape::Absent {
        // Best-effort: the publish error is the one worth reporting, and a
        // failed release degrades to exactly the orphan row `remove_agy_root`'s
        // `Absent` arm later self-heals.
        let _ = ownership.forget_now(dest, dry_run);
    }
    published
}

/// Tear down whatever occupies an `agy-*` root, dispatching on its actual
/// shape via [`classify_link`] — the counterpart teardown to
/// [`publish_agy_root`]'s publish.
///
/// - Symbolic link or legacy junction: unlinked via [`remove_link`], which
///   removes the link itself and never follows through to `canonical_root`.
/// - Real directory that `ownership` recorded (`AgyRootOwnership::is_owned`):
///   the tree is deleted with `fs::remove_dir_all`, then — only once that
///   succeeds — [`AgyRootOwnership::forget_now`] removes its row from disk.
///   Row-removal strictly follows tree-removal, never the reverse: a crash
///   in between leaves a harmless row-without-tree, the same accepted
///   ordering `publish_agy_root`'s own row-before-tree write already keys
///   off. If the row removal itself fails, the tree is already
///   gone and the error still propagates — the caller knows to retry rather
///   than believing teardown fully succeeded.
/// - Real directory `ownership` does not recognize: left untouched, with a
///   warning — never deleted on this function's own authority. An empty or
///   missing lockfile authorizes nothing (`AgyRootOwnership::is_owned` is
///   `false` in that case), so a machine with no lockfile at all still
///   preserves a user's own directory rather than treating "nothing
///   recorded" as "safe to delete".
/// - Absent: no-op.
///
/// `dry_run` performs zero disk writes and zero lockfile writes; it reports
/// what a real run would remove without removing it.
pub(crate) fn remove_agy_root(
    path: &Path,
    ownership: &AgyRootOwnership,
    dry_run: bool,
    report: &mut ProjectionReport,
) -> Result<(), ProjectionError> {
    match classify_link(path) {
        LinkShape::Symlink | LinkShape::Junction => {
            if !dry_run {
                remove_link(path)?;
                // `publish_agy_root` records the row before it knows which
                // rung will win, so a symlink publish carries a row too.
                // Releasing it here — after the link is gone, never before,
                // the same ordering the `Directory` arm keeps — is what
                // stops a torn-down root from leaving standing
                // `remove_dir_all` authority over whatever later occupies
                // this path.
                ownership.forget_now(path, dry_run)?;
            }
            report.removed_paths.push(path.to_path_buf());
        }
        LinkShape::Directory => {
            if ownership.is_owned(path) {
                if !dry_run {
                    fs::remove_dir_all(path)?;
                    ownership.forget_now(path, dry_run)?;
                }
                report.removed_paths.push(path.to_path_buf());
            } else {
                report.warnings.push(ProjectionWarning {
                    message: format!("preserved user-owned path: {}", path.display()),
                });
            }
        }
        LinkShape::Absent => {
            // Nothing to remove, but a row may still be here: a publish that
            // recorded its row and then failed before a tree existed, or a
            // rollback that removed the tree, both leave one behind. Teardown
            // is the natural place to make that self-healing — an orphan row
            // is only harmless while the path stays absent, and this is the
            // one moment we know it is.
            // Unconditional, deliberately: `is_owned` reads this handle's
            // in-memory snapshot from construction time, which is stale
            // against rows `record_now` wrote straight to disk afterwards —
            // exactly the rows this arm exists to clear. Removing a row for
            // an absent path is safe regardless of ownership: the row only
            // ever authorizes deleting a tree, and there is no tree. If ccync
            // never owned the path there is no row to remove either.
            if !dry_run {
                ownership.forget_now(path, dry_run)?;
            }
        }
    }
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
    fn managed_path_keys_preserve_platform_encoding() {
        #[cfg(windows)]
        for (path, expected) in [
            (r"C:/Users/ALICE/É", r"c:\users\alice\É"),
            (r"\\Server\Share\Name", r"\\server\share\name"),
            (r"\\?\C:\Dir\File", r"\\?\c:\dir\file"),
        ] {
            assert_eq!(normalize_managed_artifact_path(Path::new(path)), expected);
        }
        #[cfg(not(windows))]
        assert_eq!(
            normalize_managed_artifact_path(Path::new("/Home/É")),
            "/Home/É"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn legacy_keys_are_decoded_and_invalid_rows_remain_quarantined() {
        let tmp = TempDir::new().unwrap();
        let lock = tmp.path().join("lock.json");
        let path = tmp.path().join("Managed");
        let key = normalize_managed_artifact_path(&path);
        let legacy = key.replace('/', "\\");
        let invalid = "\\tmp\\..\\unsafe";
        let root = serde_json::json!({PROJECTION_LOCK_KEY: {
            MANAGED_DIRECT_SKILL_PATHS: [legacy, invalid],
            MANAGED_SOURCE_ATTRIBUTION: {legacy.clone(): "source", invalid: "source"},
            MANAGED_STANDALONE_PROOFS: {legacy.clone(): {"sourceId": "source"}, invalid: {"sourceId": "source"}}
        }});
        fs::write(&lock, serde_json::to_vec(&root).unwrap()).unwrap();
        let mut report = ProjectionReport::default();
        let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
        assert!(registry.can_mutate(&path));
        assert_eq!(registry.owning_source_of(&path), Some("source"));
        assert_eq!(registry.owning_source_of(Path::new(invalid)), None);
        assert_eq!(
            registry
                .source_attribution_prior
                .get(&key)
                .map(String::as_str),
            Some("source")
        );
        assert!(registry.standalone_proofs_prior.contains_key(&key));
        assert!(!registry.prior_is_empty());
        remove_managed_artifact_row_on_disk(&lock, MANAGED_DIRECT_SKILL_PATHS, &path).unwrap();
        let removed: Value = serde_json::from_slice(&fs::read(&lock).unwrap()).unwrap();
        assert!(!removed[PROJECTION_LOCK_KEY][MANAGED_DIRECT_SKILL_PATHS]
            .as_array()
            .unwrap()
            .contains(&Value::String(legacy)));
        fs::write(&lock, serde_json::to_vec(&root).unwrap()).unwrap();
        registry.mark(MANAGED_SKILL_PATHS, &tmp.path().join("other"));
        registry.persist(&[], false, &mut report).unwrap();
        let written: Value = serde_json::from_slice(&fs::read(&lock).unwrap()).unwrap();
        let meta = &written[PROJECTION_LOCK_KEY];
        assert!(meta[MANAGED_DIRECT_SKILL_PATHS]
            .as_array()
            .unwrap()
            .contains(&Value::String(key.clone())));
        assert!(meta[MANAGED_DIRECT_SKILL_PATHS]
            .as_array()
            .unwrap()
            .contains(&Value::String(invalid.into())));
        assert!(meta[MANAGED_SOURCE_ATTRIBUTION].get(invalid).is_some());
        assert!(meta[MANAGED_STANDALONE_PROOFS].get(invalid).is_some());
        remove_managed_artifact_row_on_disk(&lock, MANAGED_DIRECT_SKILL_PATHS, &path).unwrap();
        let written: Value = serde_json::from_slice(&fs::read(&lock).unwrap()).unwrap();
        assert!(!written[PROJECTION_LOCK_KEY][MANAGED_DIRECT_SKILL_PATHS]
            .as_array()
            .unwrap()
            .contains(&Value::String(key)));
    }

    #[cfg(not(windows))]
    #[test]
    fn invalid_keys_do_not_authorize_first_run() {
        let tmp = TempDir::new().unwrap();
        let lock = tmp.path().join("lock.json");
        let root = serde_json::json!({PROJECTION_LOCK_KEY: {MANAGED_SKILL_PATHS: ["relative", "\\tmp\\..\\bad", "\\mixed/path"]}});
        fs::write(&lock, serde_json::to_vec(&root).unwrap()).unwrap();
        let registry = ManagedArtifactRegistry::load(&lock, &mut ProjectionReport::default());
        assert!(!registry.prior_is_empty());
        assert!(!registry.can_mutate(&tmp.path().join("user")));
        assert_eq!(
            decode_managed_artifact_key("/Case/file"),
            Some("/Case/file".into())
        );
        assert_eq!(decode_managed_artifact_key("\\mixed/path"), None);
    }

    #[test]
    fn persisted_lock_literals_match_pre_refactor_baseline() {
        assert_eq!(PROJECTION_LOCK_KEY, "_ccyncProjection");
        assert_eq!(PROJECTION_LOCK_SCHEMA_VERSION, 1);
        assert_eq!(MANAGED_SOURCE_ATTRIBUTION, "sourceAttribution");
        assert_eq!(MANAGED_AGENT_PATHS, "agentProjectionPaths");
        assert_eq!(MANAGED_CODEX_AGENT_PATHS, "codexAgentProjectionPaths");
        assert_eq!(MANAGED_OPENCODE_AGENT_PATHS, "opencodeAgentProjectionPaths");
        assert_eq!(MANAGED_COMMAND_PATHS, "commandProjectionPaths");
        assert_eq!(MANAGED_DISCUSS_SKILL_PATHS, "discussSkillProjectionPaths");
        assert_eq!(MANAGED_LEGACY_PATHS, "legacyProjectionPaths");
        assert_eq!(MANAGED_SKILL_PATHS, "skillProjectionPaths");
        assert_eq!(MANAGED_AGY_ROOT_PATHS, "agyRootProjectionPaths");
        assert_eq!(MANAGED_DIRECT_SKILL_PATHS, "directSkillProjectionPaths");
        assert_eq!(MANAGED_DIRECT_AGENT_PATHS, "directAgentProjectionPaths");
        assert_eq!(MANAGED_DIRECT_COMMAND_PATHS, "directCommandProjectionPaths");
    }

    // Persist/load round-trip for the OpenCode agent category — proves the
    // category participates in both fixed lists, not just the constant.
    #[test]
    fn opencode_agent_category_persists_and_reloads() {
        let tmp = TempDir::new().unwrap();
        let lock = tmp.path().join("lock.json");
        let managed = tmp.path().join("agents").join("reviewer.md");

        let mut report = ProjectionReport::default();
        let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
        registry.mark(MANAGED_OPENCODE_AGENT_PATHS, &managed);
        registry.persist(&[], false, &mut report).unwrap();

        let mut reload_report = ProjectionReport::default();
        let reloaded = ManagedArtifactRegistry::load(&lock, &mut reload_report);
        assert!(reloaded.is_managed(&managed));
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
            write_json_map_with_evidence(&path, &data, false, Some(ProjectionTargetId::Copilot))
                .unwrap();

        assert_eq!(
            evidence.by_target[&ProjectionTargetId::Copilot].paths,
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
            Some(ProjectionTargetId::Copilot),
        )
        .unwrap();

        assert!(!stale.exists());
        assert_eq!(
            evidence.by_target[&ProjectionTargetId::Copilot].paths,
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
    // Windows denies unlinking an open file without FILE_SHARE_DELETE. Unix
    // permits unlinking an open file, so this mandatory-lock behavior only
    // exists on Windows; product and test-environment causes are excluded.
    #[cfg(windows)]
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
        let mut report = ProjectionReport::default();
        let registry = ManagedArtifactRegistry::load(&tmp.path().join("nolock.json"), &mut report);

        prune_stale_managed_files(
            &root,
            "toml",
            &[],
            false,
            &registry,
            Some(ProjectionTargetId::Copilot),
        )
        .expect_err("locked file must make the prune fail");

        assert!(
            stale.exists(),
            "the removal must not have partially succeeded"
        );
    }

    // ── write_text_atomically_with_report ───────────────────────────────────

    #[test]
    fn write_text_atomically_with_report_writes_and_records_report() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("nested").join("target.md");

        let mut report = ProjectionReport::default();
        write_text_atomically_with_report(&dest, "body", false, &mut report).unwrap();

        assert_eq!(fs::read_to_string(&dest).unwrap(), "body\n");
        assert_eq!(report.written_files, vec![dest.clone()]);
        assert!(
            !dest.with_file_name("target.md.tmp").exists(),
            "temp sibling must not linger after a successful write"
        );
    }

    #[test]
    fn write_text_atomically_with_report_unchanged_bytes_short_circuit_no_report() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("target.md");

        let mut report = ProjectionReport::default();
        write_text_atomically_with_report(&dest, "body", false, &mut report).unwrap();

        // Second write with identical (post-normalization) content must be a
        // no-op: no fresh write, and no new report row.
        let mtime_before = fs::metadata(&dest).unwrap().modified().unwrap();
        let mut report = ProjectionReport::default();
        write_text_atomically_with_report(&dest, "body\n", false, &mut report).unwrap();

        assert!(
            report.written_files.is_empty(),
            "unchanged write must not be reported"
        );
        assert_eq!(
            fs::metadata(&dest).unwrap().modified().unwrap(),
            mtime_before
        );
        assert!(!dest.with_file_name("target.md.tmp").exists());
    }

    #[test]
    fn write_text_atomically_with_report_dry_run_previews_without_writing() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("target.md");

        let mut report = ProjectionReport::default();
        write_text_atomically_with_report(&dest, "body", true, &mut report).unwrap();

        assert!(!dest.exists(), "dry-run must not touch disk");
        assert_eq!(report.written_files, vec![dest.clone()]);
        assert!(!dest.with_file_name("target.md.tmp").exists());
    }

    /// Deterministically blocks `<target>.tmp` by pre-creating it as a
    /// directory — the shared writer's temp-file create fails at the
    /// `TempWrite` stage on every platform, without racing a real fault
    /// injection hook. Proves the failure surfaces as an error, leaves the
    /// destination unwritten, records nothing in the report, and never
    /// mistakes the blocking directory for a stray temp file.
    #[test]
    fn write_text_atomically_with_report_blocking_tmp_directory_fails_without_partial_write() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("target.md");
        let tmp_sibling = tmp.path().join("target.md.tmp");
        fs::create_dir_all(&tmp_sibling).unwrap();

        let mut report = ProjectionReport::default();
        let result = write_text_atomically_with_report(&dest, "body", false, &mut report);

        assert!(
            result.is_err(),
            "a blocked temp sibling must surface as an error"
        );
        assert!(!dest.exists(), "destination must remain unwritten");
        assert!(report.written_files.is_empty());
        assert!(
            tmp_sibling.is_dir(),
            "the blocking directory must be left exactly as found, not treated as a stray temp file"
        );
    }

    // ── AgyRootOwnership ────────────────────────────────────────────────────

    #[test]
    fn agy_root_ownership_record_now_is_readable_on_disk_before_any_persist() {
        let tmp = TempDir::new().unwrap();
        let lock = tmp.path().join("lock.json");
        let owned = tmp
            .path()
            .join("gemini")
            .join("config")
            .join("plugins")
            .join("ccync");

        let ownership = AgyRootOwnership::from_lockfile(&lock);
        ownership.record_now(&owned, false).unwrap();

        // Re-read straight off disk — no `persist` was ever called on any
        // registry instance.
        let raw = fs::read_to_string(&lock).unwrap();
        let root: Value = serde_json::from_str(&raw).unwrap();
        let recorded = root[PROJECTION_LOCK_KEY][MANAGED_AGY_ROOT_PATHS]
            .as_array()
            .expect("category array present on disk");
        assert!(recorded
            .iter()
            .any(|value| value.as_str() == Some(&normalize_managed_artifact_path(&owned))));

        // And a freshly loaded registry sees the row as managed.
        let mut report = ProjectionReport::default();
        let reloaded = ManagedArtifactRegistry::load(&lock, &mut report);
        assert!(reloaded.is_managed(&owned));
    }

    #[test]
    fn agy_root_ownership_record_now_preserves_unrelated_category_already_on_disk() {
        let tmp = TempDir::new().unwrap();
        let lock = tmp.path().join("lock.json");
        let untouched = tmp.path().join("skills").join("existing");

        // Pre-seed an unrelated category via the ordinary registry/persist path.
        let mut report = ProjectionReport::default();
        let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
        registry.mark(MANAGED_SKILL_PATHS, &untouched);
        registry.persist(&[], false, &mut report).unwrap();

        let owned = tmp
            .path()
            .join("gemini")
            .join("config")
            .join("plugins")
            .join("ccync");
        let ownership = AgyRootOwnership::from_lockfile(&lock);
        ownership.record_now(&owned, false).unwrap();

        let raw = fs::read_to_string(&lock).unwrap();
        let root: Value = serde_json::from_str(&raw).unwrap();
        let skill_paths = root[PROJECTION_LOCK_KEY][MANAGED_SKILL_PATHS]
            .as_array()
            .expect("pre-seeded category must survive the forced write");
        assert!(skill_paths
            .iter()
            .any(|value| value.as_str() == Some(&normalize_managed_artifact_path(&untouched))));
    }

    #[test]
    fn agy_root_ownership_record_now_dry_run_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        let lock = tmp.path().join("lock.json");
        let owned = tmp
            .path()
            .join("gemini")
            .join("config")
            .join("plugins")
            .join("ccync");

        let ownership = AgyRootOwnership::from_lockfile(&lock);
        ownership.record_now(&owned, true).unwrap();

        assert!(!lock.exists(), "dry_run must suppress the forced write");
        assert!(!ownership.is_owned(&owned));
    }

    #[test]
    fn agy_root_ownership_is_owned_false_on_missing_prior() {
        let tmp = TempDir::new().unwrap();
        let lock = tmp.path().join("nolock.json");
        let path = tmp
            .path()
            .join("gemini")
            .join("config")
            .join("plugins")
            .join("ccync");

        let ownership = AgyRootOwnership::from_lockfile(&lock);
        assert!(!ownership.is_owned(&path));
    }

    #[test]
    fn agy_root_ownership_is_owned_false_on_empty_prior() {
        let tmp = TempDir::new().unwrap();
        let lock = tmp.path().join("lock.json");
        fs::write(&lock, "{}\n").unwrap();
        let path = tmp
            .path()
            .join("gemini")
            .join("config")
            .join("plugins")
            .join("ccync");

        let ownership = AgyRootOwnership::from_lockfile(&lock);
        assert!(!ownership.is_owned(&path));
    }

    // ── persist: disk is sole authority for MANAGED_AGY_ROOT_PATHS ─────────

    // P1 — a row recorded via `record_now` survives a sibling registry
    // instance's own later `persist`, even though that sibling instance
    // never marked the row itself.
    #[test]
    fn agy_root_row_survives_a_sibling_registrys_persist() {
        let tmp = TempDir::new().unwrap();
        let lock = tmp.path().join("lock.json");
        let owned = tmp.path().join("gemini").join("config").join("plugins");

        let ownership = AgyRootOwnership::from_lockfile(&lock);
        ownership.record_now(&owned, false).unwrap();

        // A sibling registry instance, loaded fresh, marks an unrelated
        // category and persists — it never touched `MANAGED_AGY_ROOT_PATHS`.
        let mut report = ProjectionReport::default();
        let mut sibling = ManagedArtifactRegistry::load(&lock, &mut report);
        sibling.mark(MANAGED_SKILL_PATHS, &tmp.path().join("skills").join("s"));
        sibling.persist(&[], false, &mut report).unwrap();

        let raw = fs::read_to_string(&lock).unwrap();
        let root: Value = serde_json::from_str(&raw).unwrap();
        let recorded = root[PROJECTION_LOCK_KEY][MANAGED_AGY_ROOT_PATHS]
            .as_array()
            .expect("agy root category must survive a sibling's persist");
        assert!(recorded
            .iter()
            .any(|value| value.as_str() == Some(&normalize_managed_artifact_path(&owned))));
    }

    // P2 — a persist whose in-memory categories are all empty would drop the
    // whole `_ccyncProjection` marker under the old union-with-`next` logic;
    // reading the agy category from disk keeps the marker alive.
    #[test]
    fn agy_root_row_survives_a_persist_with_all_categories_empty_in_memory() {
        let tmp = TempDir::new().unwrap();
        let lock = tmp.path().join("lock.json");
        let owned = tmp.path().join("gemini").join("config").join("plugins");

        let ownership = AgyRootOwnership::from_lockfile(&lock);
        ownership.record_now(&owned, false).unwrap();

        // Load fresh, then force every category empty via `replace_category`
        // without marking anything back in — the old code path would see
        // `next` entirely empty and drop the marker.
        let mut report = ProjectionReport::default();
        let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
        for category in [
            MANAGED_AGENT_PATHS,
            MANAGED_CODEX_AGENT_PATHS,
            MANAGED_OPENCODE_AGENT_PATHS,
            MANAGED_COMMAND_PATHS,
            MANAGED_LEGACY_PATHS,
            MANAGED_SKILL_PATHS,
            MANAGED_AGY_ROOT_PATHS,
        ] {
            registry.replace_category(category);
        }
        registry.persist(&[], false, &mut report).unwrap();

        let raw = fs::read_to_string(&lock).unwrap();
        let root: Value = serde_json::from_str(&raw).unwrap();
        let meta = root
            .get(PROJECTION_LOCK_KEY)
            .expect("marker must survive: agy root category was non-empty on disk");
        let recorded = meta[MANAGED_AGY_ROOT_PATHS]
            .as_array()
            .expect("agy root category array present");
        assert!(recorded
            .iter()
            .any(|value| value.as_str() == Some(&normalize_managed_artifact_path(&owned))));
    }

    // P3 — a row removed from disk after a registry instance loaded is NOT
    // written back by that instance's later `persist`; disk is read fresh at
    // persist time, not unioned with the stale in-memory `next`/`prior`.
    #[test]
    fn agy_root_row_removed_on_disk_is_not_written_back_by_a_stale_producer() {
        let tmp = TempDir::new().unwrap();
        let lock = tmp.path().join("lock.json");
        let owned = tmp.path().join("gemini").join("config").join("plugins");

        let ownership = AgyRootOwnership::from_lockfile(&lock);
        ownership.record_now(&owned, false).unwrap();

        // Load a registry while the row is still present — its `prior`/`next`
        // both carry the row in memory.
        let mut report = ProjectionReport::default();
        let mut registry = ManagedArtifactRegistry::load(&lock, &mut report);
        assert!(registry.is_managed(&owned), "test premise: row loaded");

        // A later teardown (simulated directly) removes the row on disk.
        merge_managed_artifact_row_on_disk(&lock, MANAGED_SKILL_PATHS, &tmp.path().join("noop"))
            .unwrap(); // touch the file once to prove writes still land
        let raw = fs::read_to_string(&lock).unwrap();
        let mut root: Value = serde_json::from_str(&raw).unwrap();
        root[PROJECTION_LOCK_KEY][MANAGED_AGY_ROOT_PATHS] = Value::Array(Vec::new());
        fs::write(&lock, serde_json::to_string_pretty(&root).unwrap()).unwrap();

        // The stale-loaded registry marks an unrelated category and persists
        // — it must not resurrect the removed agy row from its own `next`.
        registry.mark(
            MANAGED_COMMAND_PATHS,
            &tmp.path().join("commands").join("c"),
        );
        registry.persist(&[], false, &mut report).unwrap();

        let raw = fs::read_to_string(&lock).unwrap();
        let root: Value = serde_json::from_str(&raw).unwrap();
        let recorded = root[PROJECTION_LOCK_KEY][MANAGED_AGY_ROOT_PATHS]
            .as_array()
            .expect("agy root category array present");
        assert!(
            recorded.is_empty(),
            "a stale producer must not write the removed row back"
        );
    }

    // ── staged-copy engine ──────────────────────────────────────────────────

    fn write_nested(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// Lists every entry under `root`, recursively, as parent-relative
    /// forward-slash paths — used to compare a staged/swapped tree against
    /// its source independent of the staging directory's own generated name.
    fn list_tree_relative(root: &Path) -> Vec<String> {
        fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
            let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let path = entry.path();
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if path.is_dir() {
                    walk(base, &path, out);
                } else {
                    out.push(rel);
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out
    }

    #[test]
    fn publish_staged_tree_copy_publishes_full_copy_of_source_tree() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "plugin.json", "{}");
        write_nested(&source, "skills/one/SKILL.md", "one");
        write_nested(&source, "skills/two/SKILL.md", "two");
        let dest = tmp.path().join("dest");

        let swapped = publish_staged_tree_copy(&source, &dest).unwrap();

        assert!(swapped, "an absent destination must trigger a swap");
        assert!(dest.is_dir());
        assert_eq!(list_tree_relative(&dest), list_tree_relative(&source));
        assert_eq!(
            fs::read_to_string(dest.join("skills/one/SKILL.md")).unwrap(),
            "one"
        );

        // No staging sibling left behind next to the published destination.
        for entry in fs::read_dir(tmp.path()).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            assert!(
                !name.starts_with(".ccync-stage-"),
                "staging directory must not linger after a successful swap: {name}"
            );
        }
    }

    #[test]
    fn publish_staged_tree_copy_replaces_a_differing_existing_destination() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "skills/one/SKILL.md", "new content");
        let dest = tmp.path().join("dest");
        write_nested(&dest, "skills/one/SKILL.md", "old content");
        write_nested(&dest, "skills/stale/SKILL.md", "must be gone");

        let swapped = publish_staged_tree_copy(&source, &dest).unwrap();

        assert!(swapped);
        assert_eq!(
            fs::read_to_string(dest.join("skills/one/SKILL.md")).unwrap(),
            "new content"
        );
        assert!(
            !dest.join("skills/stale").exists(),
            "the old destination tree must be fully replaced, not merged"
        );
    }

    #[test]
    fn publish_staged_tree_copy_skips_an_identical_destination_with_zero_writes() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "plugin.json", "{}");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let dest = tmp.path().join("dest");
        write_nested(&dest, "plugin.json", "{}");
        write_nested(&dest, "skills/one/SKILL.md", "one");

        let dest_file = dest.join("skills/one/SKILL.md");
        let mtime_before = fs::metadata(&dest_file).unwrap().modified().unwrap();

        let swapped = publish_staged_tree_copy(&source, &dest).unwrap();

        assert!(!swapped, "an identical destination must be skipped");
        assert_eq!(
            fs::metadata(&dest_file).unwrap().modified().unwrap(),
            mtime_before,
            "a skipped publish must not touch the destination at all"
        );
        for entry in fs::read_dir(tmp.path()).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            assert!(
                !name.starts_with(".ccync-stage-"),
                "a skipped publish must never stage a copy: {name}"
            );
        }
    }

    #[test]
    fn trees_content_equal_detects_a_differing_file() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        write_nested(&a, "skills/one/SKILL.md", "one");
        write_nested(&b, "skills/one/SKILL.md", "different");

        assert!(!trees_content_equal(&a, &b));
    }

    #[test]
    fn trees_content_equal_detects_an_extra_entry() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        write_nested(&a, "skills/one/SKILL.md", "one");
        write_nested(&b, "skills/one/SKILL.md", "one");
        write_nested(&b, "skills/two/SKILL.md", "two");

        assert!(!trees_content_equal(&a, &b));
    }

    #[test]
    fn trees_content_equal_true_for_identical_nested_trees() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        write_nested(&a, "skills/one/SKILL.md", "one");
        write_nested(&a, "plugin.json", "{}");
        write_nested(&b, "skills/one/SKILL.md", "one");
        write_nested(&b, "plugin.json", "{}");

        assert!(trees_content_equal(&a, &b));
    }

    /// Deterministically interrupts a stage partway through by holding a
    /// second source file open without read-sharing, so `fs::copy` fails on
    /// it with a sharing violation after the first file already copied
    /// successfully into the staging directory. Proves the *destination*
    /// path is never observed holding that partial result — it is exactly
    /// whatever it held before the call (absent, here), because the swap
    /// step never runs when staging fails.
    #[test]
    fn interrupted_stage_leaves_an_absent_destination_absent() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let blocked = source.join("skills/two/SKILL.md");
        write_nested(&source, "skills/two/SKILL.md", "two");
        let dest = tmp.path().join("dest");

        #[cfg(windows)]
        let _locked = lock_without_read_sharing(&blocked);
        #[cfg(unix)]
        let _locked = {
            // A dangling source link makes fs::copy return NotFound on Unix,
            // injecting a real staging fault without relying on file locks.
            fs::remove_file(&blocked).unwrap();
            std::os::unix::fs::symlink(tmp.path().join("missing-source"), &blocked).unwrap()
        };

        let result = publish_staged_tree_copy(&source, &dest);

        result.expect_err("the injected source-file fault must make staging fail");

        assert!(
            !dest.exists(),
            "destination must remain absent — the swap step must never run when staging fails"
        );
        for entry in fs::read_dir(tmp.path()).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            assert!(
                !name.starts_with(".ccync-stage-"),
                "a failed stage must clean up its own partial staging directory: {name}"
            );
        }
    }

    #[cfg(windows)]
    fn lock_without_read_sharing(path: &Path) -> fs::File {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_NONE: u32 = 0;
        fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_NONE)
            .open(path)
            .unwrap()
    }

    /// Same interrupted-stage fault, but with a pre-existing destination —
    /// proves the *complete* half of "absent-or-complete": an existing
    /// destination survives a failed stage byte-for-byte, because the swap
    /// step (the only step that touches `dest`) never runs.
    #[test]
    fn interrupted_stage_leaves_an_existing_destination_untouched() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let blocked = source.join("skills/two/SKILL.md");
        write_nested(&source, "skills/two/SKILL.md", "two");
        let dest = tmp.path().join("dest");
        write_nested(&dest, "skills/one/SKILL.md", "old");

        #[cfg(windows)]
        let _locked = lock_without_read_sharing(&blocked);
        #[cfg(not(windows))]
        let _locked = &blocked;

        let result = publish_staged_tree_copy(&source, &dest);

        #[cfg(windows)]
        {
            result.expect_err("a locked source file must make the stage fail");
            assert_eq!(
                fs::read_to_string(dest.join("skills/one/SKILL.md")).unwrap(),
                "old",
                "an existing destination must survive a failed stage untouched"
            );
        }
        #[cfg(not(windows))]
        {
            let _ = result;
        }
    }

    // ── publish_agy_root ─────────────────────────────────────────────────

    #[test]
    fn publish_agy_root_produces_a_directory_with_its_row_already_on_disk() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "plugin.json", "{}");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let dest = tmp.path().join("gemini").join("config").join("plugins");
        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);

        let mut report = ProjectionReport::default();
        let (shape, _wrote) =
            publish_agy_root(&source, &dest, &ownership, false, &mut report).unwrap();

        if shape == LinkShape::Symlink {
            // Symlink-permitting host: not this dev machine, but handled the
            // same way the shape-classifier's own tests handle it.
            assert_eq!(classify_link(&dest), LinkShape::Symlink);
        } else {
            assert_eq!(shape, LinkShape::Directory);
            assert_eq!(list_tree_relative(&dest), list_tree_relative(&source));
        }

        // The ownership row must be readable straight off disk — no `persist`
        // was ever called on any registry instance.
        let raw = fs::read_to_string(&lock).unwrap();
        let root: Value = serde_json::from_str(&raw).unwrap();
        let recorded = root[PROJECTION_LOCK_KEY][MANAGED_AGY_ROOT_PATHS]
            .as_array()
            .expect("category array present on disk");
        assert!(recorded
            .iter()
            .any(|value| value.as_str() == Some(&normalize_managed_artifact_path(&dest))));
    }

    // A junction is a Windows reparse-point type. Unix directory links are
    // symlinks and cannot exercise dangling-junction classification;
    // product and test-environment causes are excluded.
    #[cfg(windows)]
    #[test]
    fn publish_agy_root_replaces_a_junction_whose_target_was_deleted() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let stale_target = tmp.path().join("stale-target");
        fs::create_dir_all(&stale_target).unwrap();
        let dest = tmp.path().join("dest");
        create_dir_link(&stale_target, &dest).unwrap();
        fs::remove_dir_all(&stale_target).unwrap();
        assert_eq!(
            classify_link(&dest),
            LinkShape::Junction,
            "a dangling junction must still classify as Junction, not Absent"
        );

        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);
        let mut report = ProjectionReport::default();
        let (shape, _wrote) =
            publish_agy_root(&source, &dest, &ownership, false, &mut report).unwrap();

        assert_ne!(
            classify_link(&dest),
            LinkShape::Junction,
            "the legacy junction must be replaced"
        );
        if shape != LinkShape::Symlink {
            assert_eq!(shape, LinkShape::Directory);
            assert_eq!(list_tree_relative(&dest), list_tree_relative(&source));
        }
    }

    #[test]
    fn publish_agy_root_preserves_an_unowned_real_directory_even_with_no_lockfile() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let dest = tmp.path().join("dest");
        write_nested(&dest, "users-own-file.txt", "do not touch");
        let lock = tmp.path().join("nolock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);

        let mut report = ProjectionReport::default();
        let (shape, wrote) =
            publish_agy_root(&source, &dest, &ownership, false, &mut report).unwrap();

        assert_eq!(shape, LinkShape::Directory);
        assert!(
            !wrote,
            "preserving an unowned path writes nothing, so callers must record no path evidence"
        );
        assert!(
            dest.join("users-own-file.txt").exists(),
            "an unowned real directory must survive untouched"
        );
        assert!(
            !dest.join("skills").exists(),
            "the source tree must not be published over unowned content"
        );
        assert!(
            !lock.exists(),
            "no lockfile write may happen for a preserved path"
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.message.contains("preserved user-owned path")),
            "preservation must be reported"
        );
    }

    #[test]
    fn publish_agy_root_dry_run_touches_neither_disk_nor_lockfile() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let dest = tmp.path().join("dest");
        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);

        let mut report = ProjectionReport::default();
        let (shape, wrote) =
            publish_agy_root(&source, &dest, &ownership, true, &mut report).unwrap();

        assert!(!dest.exists(), "dry_run must not create anything at dest");
        assert!(!lock.exists(), "dry_run must not write the lockfile");
        assert!(shape == LinkShape::Symlink || shape == LinkShape::Directory);
        assert!(
            !wrote,
            "dry_run writes nothing, so it never reports a write"
        );
    }

    #[test]
    fn publish_agy_root_dry_run_predicts_directory_for_an_already_owned_copy() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let dest = tmp.path().join("dest");
        write_nested(&dest, "skills/one/SKILL.md", "one");
        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);
        ownership.record_now(&dest, false).unwrap();
        let bytes_before = fs::read(&lock).unwrap();

        let mut report = ProjectionReport::default();
        let (shape, wrote) =
            publish_agy_root(&source, &dest, &ownership, true, &mut report).unwrap();

        assert_eq!(
            shape,
            LinkShape::Directory,
            "an occupied, owned directory always falls to rung 2 — knowable without writing"
        );
        assert!(
            !wrote,
            "dry_run writes nothing, so it never reports a write"
        );
        assert_eq!(
            fs::read(&lock).unwrap(),
            bytes_before,
            "dry_run must leave the lockfile bytes untouched"
        );
    }

    /// The property the plan's own Test Cases name ("neither run reports a
    /// spurious change"), at the reporting level rather than the disk level:
    /// republishing an already-correct root must report `wrote == false`, so
    /// its caller records no path evidence and a stable rerun reads back as
    /// unchanged instead of applied.
    #[test]
    fn publish_agy_root_second_identical_publish_reports_no_write() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "plugin.json", "{}");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let dest = tmp.path().join("dest");
        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);

        let mut first_report = ProjectionReport::default();
        let (first_shape, first_wrote) =
            publish_agy_root(&source, &dest, &ownership, false, &mut first_report).unwrap();
        assert!(first_wrote, "the initial publish must report a real write");

        // Re-load ownership so the second call sees the row the first wrote,
        // exactly as a fresh `ccync sync` process would.
        let ownership = AgyRootOwnership::from_lockfile(&lock);
        let mut second_report = ProjectionReport::default();
        let (second_shape, second_wrote) =
            publish_agy_root(&source, &dest, &ownership, false, &mut second_report).unwrap();

        assert_eq!(
            second_shape, first_shape,
            "a stable rerun lands the same shape it already had"
        );
        assert!(
            !second_wrote,
            "an unchanged republish must report no write, or every sync reports a spurious change"
        );
        assert!(
            second_report.warnings.is_empty(),
            "an owned, already-correct root is not a preserved-user-content case"
        );
    }

    /// Forces a failure between the row write and the rename that makes the
    /// tree visible, using the same locked-source-file technique as the
    /// staged-copy engine's own interrupted-stage tests.
    ///
    /// The tree must never appear at `dest`, and — the part this test was
    /// rewritten for — the row must not be left behind either. A row whose
    /// tree never materialized is standing `remove_dir_all` authority over
    /// whatever later occupies that path, so a publish that unwinds cleanly
    /// releases it again. The required row-before-tree ordering is
    /// unobservable after a clean unwind by construction; it survives as the
    /// crash-only state (process killed mid-publish), which
    /// `remove_agy_root`'s `Absent` arm self-heals on the next teardown.
    #[test]
    fn publish_agy_root_injected_failure_leaves_neither_tree_nor_standing_row() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "skills/one/SKILL.md", "one");
        write_nested(&source, "skills/two/SKILL.md", "two");
        #[cfg(windows)]
        let _locked = lock_without_read_sharing(&source.join("skills/two/SKILL.md"));
        #[cfg(not(windows))]
        let dest = {
            // A directory link is the normal Unix publish path. Obstruct its
            // parent so publication reaches and fails the staged-copy rung.
            let parent = tmp.path().join("not-a-directory");
            fs::write(&parent, "parent obstruction").unwrap();
            parent.join("dest")
        };
        #[cfg(windows)]
        let dest = tmp.path().join("dest");
        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);

        let mut report = ProjectionReport::default();
        let result = publish_agy_root(&source, &dest, &ownership, false, &mut report);

        match result {
            Err(_) => {
                assert!(
                    !dest.exists(),
                    "the tree must never become visible when the copy fails"
                );

                // Read the row set back from disk, not from the in-memory
                // handle: `record_now`/`forget_now` both write straight
                // through, and disk is the only authority for this category.
                let raw = fs::read_to_string(&lock).unwrap();
                let root: Value = serde_json::from_str(&raw).unwrap();
                let recorded = root[PROJECTION_LOCK_KEY][MANAGED_AGY_ROOT_PATHS]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                assert!(
                    !recorded.iter().any(
                        |value| value.as_str() == Some(&normalize_managed_artifact_path(&dest))
                    ),
                    "a failed publish must not leave standing removal authority for {}",
                    dest.display()
                );

                // The release is scoped to this publish: a row for a
                // different, still-owned root is untouched.
                let sibling = tmp.path().join("other-root");
                let ownership2 = AgyRootOwnership::from_lockfile(&lock);
                ownership2.record_now(&sibling, false).unwrap();
                let mut report2 = ProjectionReport::default();
                let _ = publish_agy_root(&source, &dest, &ownership2, false, &mut report2);
                let raw = fs::read_to_string(&lock).unwrap();
                let root: Value = serde_json::from_str(&raw).unwrap();
                let recorded = root[PROJECTION_LOCK_KEY][MANAGED_AGY_ROOT_PATHS]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                assert!(
                    recorded
                        .iter()
                        .any(|value| value.as_str()
                            == Some(&normalize_managed_artifact_path(&sibling))),
                    "releasing one root's row must not disturb another root's row"
                );
            }
            #[cfg(windows)]
            Ok((LinkShape::Symlink, _)) => {
                // A symlink-permitting host never reaches the staged-copy
                // rung, so the locked-source fault is not exercised there.
            }
            #[cfg(windows)]
            Ok((shape, _)) => panic!("publication unexpectedly succeeded as {shape:?}"),
            #[cfg(not(windows))]
            Ok((shape, _)) => panic!("publication unexpectedly succeeded as {shape:?}"),
        }
    }

    #[test]
    fn strip_lockfile_to_agy_rows_keeps_the_proof_and_drops_the_credentials() {
        let tmp = TempDir::new().unwrap();
        let lock = tmp.path().join("lock.json");
        let owned = tmp.path().join("agy-root");
        AgyRootOwnership::from_lockfile(&lock)
            .record_now(&owned, false)
            .unwrap();

        // Splice in the shapes an adopted install really carries: the MCP
        // definition namespace (where `env`/`headers` secrets live) and a
        // second managed category that is derived state, not proof.
        let raw = fs::read_to_string(&lock).unwrap();
        let mut root: Map<String, Value> = match serde_json::from_str(&raw).unwrap() {
            Value::Object(root) => root,
            other => panic!("lockfile must be an object, got {other:?}"),
        };
        root.insert(
            "_mcpServers".to_string(),
            serde_json::json!({
                "github": { "command": "gh", "env": { "GITHUB_TOKEN": "ghp_secret" } }
            }),
        );
        if let Some(Value::Object(meta)) = root.get_mut(PROJECTION_LOCK_KEY) {
            meta.insert(
                MANAGED_SKILL_PATHS.to_string(),
                Value::Array(vec![Value::String("some/skill".to_string())]),
            );
        }
        fs::write(
            &lock,
            serde_json::to_string_pretty(&Value::Object(root)).unwrap(),
        )
        .unwrap();

        strip_lockfile_to_agy_rows(&lock).unwrap();

        let raw = fs::read_to_string(&lock).unwrap();
        assert!(
            !raw.contains("ghp_secret") && !raw.contains("_mcpServers"),
            "uninstall must not leave adopted MCP credentials on disk: {raw}"
        );
        assert!(
            raw.contains("some/skill"),
            "sibling ownership categories are the proof `ManagedDeprojection` needs for the \
             other managed surfaces on an uninstall retry — narrowing to the agy rows would \
             orphan a partial uninstall's leftovers permanently"
        );
        assert!(
            AgyRootOwnership::from_lockfile(&lock).is_owned(&owned),
            "the surviving root's ownership proof must still be readable"
        );
    }

    #[test]
    fn strip_lockfile_to_agy_rows_never_manufactures_a_lockfile() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("lock.json");
        strip_lockfile_to_agy_rows(&missing).unwrap();
        assert!(
            !missing.exists(),
            "nothing was proven, so nothing is written"
        );

        let malformed = tmp.path().join("malformed.json");
        fs::write(&malformed, "{ not json").unwrap();
        strip_lockfile_to_agy_rows(&malformed).unwrap();
        assert_eq!(
            fs::read_to_string(&malformed).unwrap(),
            "{ not json",
            "an unparseable lockfile is left exactly as found"
        );
    }

    #[test]
    fn remove_agy_root_forgets_an_orphan_row_at_an_absent_path() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("dest");
        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);
        ownership.record_now(&dest, false).unwrap();
        assert!(
            AgyRootOwnership::from_lockfile(&lock).is_owned(&dest),
            "precondition: the orphan row is on disk"
        );

        let mut report = ProjectionReport::default();
        remove_agy_root(&dest, &ownership, false, &mut report).unwrap();

        assert!(
            !AgyRootOwnership::from_lockfile(&lock).is_owned(&dest),
            "teardown at an absent path must clear the orphan row that authorizes deleting whatever lands there next"
        );
        assert!(
            report.removed_paths.is_empty(),
            "nothing was on disk, so nothing may be reported as removed"
        );
    }

    #[test]
    fn remove_agy_root_forgets_the_row_when_it_removes_a_symlink() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("canonical");
        fs::create_dir_all(&target).unwrap();
        let dest = tmp.path().join("dest");
        if create_symlink_dir(&target, &dest).is_err() {
            return; // symlink-refusing host: the Directory arm covers it instead
        }
        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);
        ownership.record_now(&dest, false).unwrap();

        let mut report = ProjectionReport::default();
        remove_agy_root(&dest, &ownership, false, &mut report).unwrap();

        assert!(!dest.exists(), "the link itself must be gone");
        assert!(target.is_dir(), "removal must never follow through");
        assert!(
            !AgyRootOwnership::from_lockfile(&lock).is_owned(&dest),
            "a symlink teardown must release its row too, not only the copy teardown"
        );
    }

    #[test]
    fn remove_agy_root_dry_run_leaves_the_row_alone() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("dest");
        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);
        ownership.record_now(&dest, false).unwrap();

        let mut report = ProjectionReport::default();
        remove_agy_root(&dest, &ownership, true, &mut report).unwrap();

        assert!(
            AgyRootOwnership::from_lockfile(&lock).is_owned(&dest),
            "dry_run performs zero lockfile writes"
        );
    }

    #[cfg(windows)]
    #[test]
    fn publish_staged_tree_copy_cleans_up_when_the_swap_fails() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let dest = tmp.path().join("dest");
        write_nested(&dest, "held.txt", "held open");
        let _locked = lock_without_read_sharing(&dest.join("held.txt"));

        let result = publish_staged_tree_copy(&source, &dest);
        assert!(
            result.is_err(),
            "an undeletable destination must fail the swap"
        );

        for entry in fs::read_dir(tmp.path()).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            assert!(
                !name.starts_with(".ccync-stage-"),
                "a failed swap must not leave its staged tree behind: {name}"
            );
        }
    }

    #[test]
    fn sweep_staging_orphans_removes_leftovers_and_spares_real_siblings() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("plugins").join("ccync");
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        // A leftover from a run killed between staging and swapping — the case
        // no in-process cleanup can reach.
        let orphan = dest.with_file_name(".ccync-stage-ccync-999-0");
        write_nested(&orphan, "skills/one/SKILL.md", "leftover");
        let neighbour = dest.with_file_name("other-plugin");
        write_nested(&neighbour, "keep.txt", "keep");

        sweep_staging_orphans(&dest);

        assert!(
            !orphan.exists(),
            "a staging orphan inside the scanned plugins directory must be swept"
        );
        assert!(
            neighbour.join("keep.txt").is_file(),
            "a real sibling plugin must be untouched by the sweep"
        );
    }

    // ── remove_agy_root ──────────────────────────────────────────────────

    #[test]
    fn remove_agy_root_removes_a_symlink_without_touching_its_target() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let dest = tmp.path().join("dest");
        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);

        let (shape, wrote) = publish_agy_root(
            &source,
            &dest,
            &ownership,
            false,
            &mut ProjectionReport::default(),
        )
        .unwrap();
        if shape != LinkShape::Symlink {
            return; // this host refuses symlinks — covered by the Directory test below
        }
        assert!(wrote);

        let mut report = ProjectionReport::default();
        remove_agy_root(&dest, &ownership, false, &mut report).unwrap();

        assert!(!dest.exists(), "the link itself must be gone");
        assert!(
            source.join("skills/one/SKILL.md").exists(),
            "removing the link must never follow through to its target"
        );
    }

    #[test]
    fn remove_agy_root_removes_an_owned_directory_copy_and_forgets_its_row() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        write_nested(&source, "skills/one/SKILL.md", "one");
        let dest = tmp.path().join("dest");
        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);

        // Force the copy rung deterministically, independent of platform
        // symlink privilege: pre-seed dest as an owned directory so
        // publish_agy_root's existing==Directory path is exercised, then
        // record ownership directly to set up the removal precondition.
        write_nested(&dest, "skills/one/SKILL.md", "one");
        ownership.record_now(&dest, false).unwrap();

        // `record_now` writes straight to disk without updating this
        // instance's in-memory `registry` — reload, exactly as a fresh
        // `ccync` process would, before checking/using ownership.
        let ownership = AgyRootOwnership::from_lockfile(&lock);
        assert!(ownership.is_owned(&dest));

        let mut report = ProjectionReport::default();
        remove_agy_root(&dest, &ownership, false, &mut report).unwrap();

        assert!(!dest.exists(), "an owned directory copy must be removed");
        let raw = fs::read_to_string(&lock).unwrap();
        let root: Value = serde_json::from_str(&raw).unwrap();
        let recorded = root[PROJECTION_LOCK_KEY][MANAGED_AGY_ROOT_PATHS]
            .as_array()
            .expect("category array present on disk");
        assert!(
            !recorded
                .iter()
                .any(|value| value.as_str() == Some(&normalize_managed_artifact_path(&dest))),
            "the row must be forgotten once the tree is gone"
        );

        // The forgotten row must not come back on a fresh load either —
        // proves the removal reached disk, not just the in-memory registry.
        let reloaded = AgyRootOwnership::from_lockfile(&lock);
        assert!(!reloaded.is_owned(&dest));
    }

    #[test]
    fn remove_agy_root_preserves_an_unowned_real_directory_even_with_no_lockfile() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("dest");
        write_nested(&dest, "users-own-file.txt", "do not touch");
        let lock = tmp.path().join("nolock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);

        let mut report = ProjectionReport::default();
        remove_agy_root(&dest, &ownership, false, &mut report).unwrap();

        assert!(
            dest.join("users-own-file.txt").exists(),
            "an unowned real directory must survive untouched"
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.message.contains("preserved user-owned path")),
            "preservation must be reported"
        );
    }

    #[test]
    fn remove_agy_root_is_a_no_op_when_path_is_absent() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("does-not-exist");
        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);

        let mut report = ProjectionReport::default();
        remove_agy_root(&dest, &ownership, false, &mut report).unwrap();

        assert!(!dest.exists());
        assert!(report.removed_paths.is_empty());
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn remove_agy_root_dry_run_removes_nothing() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("dest");
        write_nested(&dest, "skills/one/SKILL.md", "one");
        let lock = tmp.path().join("lock.json");
        let ownership = AgyRootOwnership::from_lockfile(&lock);
        ownership.record_now(&dest, false).unwrap();

        // Reload so this instance actually sees the row `record_now` just
        // wrote — otherwise the owned-directory removal path this test
        // means to exercise under dry_run is never reached at all.
        let ownership = AgyRootOwnership::from_lockfile(&lock);
        assert!(ownership.is_owned(&dest));
        let bytes_before = fs::read(&lock).unwrap();

        let mut report = ProjectionReport::default();
        remove_agy_root(&dest, &ownership, true, &mut report).unwrap();

        assert!(dest.exists(), "dry_run must not remove the tree");
        assert_eq!(
            fs::read(&lock).unwrap(),
            bytes_before,
            "dry_run must leave the lockfile bytes untouched"
        );
    }

    #[test]
    fn forget_now_row_removal_survives_a_sibling_registrys_persist() {
        // Mirrors the disk-authoritative-persist survival probe
        // (agy_root_row_survives_a_sibling_registrys_persist) for the
        // removal direction: once forgotten, the row must stay gone
        // even when a sibling registry instance that never touched this
        // category later persists — the same disk-authoritative guarantee
        // record_now relies on, now proven for forget_now.
        let tmp = TempDir::new().unwrap();
        let lock = tmp.path().join("lock.json");
        let dest = tmp.path().join("dest");
        let ownership = AgyRootOwnership::from_lockfile(&lock);
        ownership.record_now(&dest, false).unwrap();
        ownership.forget_now(&dest, false).unwrap();

        let mut report = ProjectionReport::default();
        let mut sibling = ManagedArtifactRegistry::load(&lock, &mut report);
        sibling.mark(MANAGED_SKILL_PATHS, &tmp.path().join("unrelated"));
        sibling.persist(&[], false, &mut report).unwrap();

        let reloaded = AgyRootOwnership::from_lockfile(&lock);
        assert!(
            !reloaded.is_owned(&dest),
            "a sibling instance's persist must not resurrect a forgotten row"
        );
    }
}
