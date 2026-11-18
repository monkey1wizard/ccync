//! Apply the pure provider delivery plan through the projection writers.

use crate::delivery::{
    command_skill_root, prepare_delivery_with_native_routes, reader_coverage, ComponentBodies,
    ComponentRouteChoice, ComponentTargetRoute, DeliveryInventory, DeliveryPlan, ForeignComponent,
    NativeAdapterState, NativeAdapters, NativeRouteStatus, PhysicalIdentity, PreparedPayload,
    ReaderCoverage, UnconvertibleCommands,
};
use crate::install::{
    clear_native_intent, commit_native_ownership, drop_committed_ownership, pending_native_intent,
    record_native_intent, RouteConflict, RouteRetirement,
};
use crate::managed_view::{route_evidence_line, RouteEvidence, VerificationScope};
use crate::plugin_native::{
    committed_view_path, install_native_registration, observe_view_digest,
    remove_committed_registration, update_native_registration, NativeCommandRunner,
    NativeExecutable, NativeInstall, NativePathEvidence, NativeReconcileError,
};
use crate::plugin_render::{
    generate_native_package_view, NativePackageTarget, RenderedProviderGeneration,
};
use ccync_foundation::provider::{
    CapabilityInventory, ClientScope, CommittedOwnership, ComponentIdentity, ComponentKind,
    EnablementIntent, IntentPhase, NativeObservation, NativeSelector, OwnershipFingerprint,
    PendingIntent, ProviderLifecycleLock, ProviderSourceKey,
};
use ccync_foundation::target::{ProjectionTargetId, CANONICAL_PROJECTION_TARGETS};
use projection::{
    retire_shared_path, shared_path_fingerprint, SharedPathReader, SharedPathReaderState,
    SharedPathRetirement, SharedPathRetirementOutcome,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Source key and plugin name for foreign entries found in a native root.
/// They are not provider components, so no real source owns them.
const FOREIGN_SOURCE: &str = "native-foreign";

/// Publish provider components through their target-native direct surfaces.
///
/// The third element lists every target with a required route that did not
/// publish: a writer reported an error, or the written output is missing or
/// not recorded as owned. Callers use it to decide which combined surfaces may
/// be retired.
pub fn publish(
    rendered: &[RenderedProviderGeneration],
    selected: &[ProjectionTargetId],
    home: &std::path::Path,
    lockfile: &std::path::Path,
) -> (
    projection::ProjectionEvidence,
    crate::delivery::DeliveryPlan,
    std::collections::BTreeSet<ProjectionTargetId>,
) {
    publish_inner(rendered, selected, home, lockfile, None)
}

/// Publish like [`publish`], and first activate each native plugin group whose
/// adapter is ready. `runner` is the only path to native CLIs; a group that
/// cannot be proven falls back to its loose copies.
pub fn publish_with_native(
    rendered: &[RenderedProviderGeneration],
    selected: &[ProjectionTargetId],
    home: &std::path::Path,
    lockfile: &std::path::Path,
    runner: &dyn NativeCommandRunner,
) -> (
    projection::ProjectionEvidence,
    crate::delivery::DeliveryPlan,
    std::collections::BTreeSet<ProjectionTargetId>,
) {
    publish_inner(rendered, selected, home, lockfile, Some(runner))
}

fn publish_inner(
    rendered: &[RenderedProviderGeneration],
    selected: &[ProjectionTargetId],
    home: &std::path::Path,
    lockfile: &std::path::Path,
    native: Option<&dyn NativeCommandRunner>,
) -> (
    projection::ProjectionEvidence,
    crate::delivery::DeliveryPlan,
    std::collections::BTreeSet<ProjectionTargetId>,
) {
    let mut inventory = DeliveryInventory::from_generations(rendered);
    let identities = inventory
        .components
        .iter()
        .map(|component| component.identity.clone())
        .collect::<Vec<_>>();
    // Unowned entries stay in place. Foreign discovery below includes equal
    // content, because bytes alone do not prove ccync ownership.
    let legacy_notes = leftover_agent_notes(rendered, selected, home, lockfile);
    inventory.foreign_components = foreign_components(&identities, selected, home, lockfile);
    let unconvertible = unconvertible_commands(rendered);
    let adapters = native
        .map(|runner| native_adapter_states(runner, selected))
        .unwrap_or_default();
    let prepared = prepare_delivery_with_native_routes(
        &inventory,
        selected,
        &[],
        &unconvertible,
        &component_bodies(rendered),
        &adapters,
        fill_physical_identities,
    );
    let withheld_warnings = prepared
        .withheld
        .iter()
        .map(|item| {
            let blocked = item
                .blocked_consumers
                .iter()
                .map(|target| target.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let consumers = item
                .consumers
                .iter()
                .map(|target| target.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "{} withheld from {}: blocked consumers [{blocked}]; affected consumers [{consumers}]",
                item.pair.identity.component_id, item.pair.root
            )
        })
        .collect::<Vec<_>>();
    let payloads = prepared
        .payloads
        .into_iter()
        .map(|payload| ((payload.identity.clone(), payload.target), payload))
        .collect::<BTreeMap<_, PreparedPayload>>();
    let mut plan = prepared.plan;
    let mut evidence = projection::ProjectionEvidence::default();
    evidence.run_warnings.extend(legacy_notes);
    evidence.run_warnings.extend(withheld_warnings);
    // Native activation runs before any loose write, so a group that cannot
    // activate is written loose in this same run.
    let mut retained = BTreeSet::new();
    if let Some(runner) = native {
        let notes =
            cut_over_native_groups(&mut plan, rendered, runner, home, lockfile, &mut retained);
        evidence.run_warnings.extend(notes);
    }
    let mut failed = std::collections::BTreeSet::new();
    let mut stage_failures: Vec<StageFailure> = Vec::new();
    let mut served = std::collections::BTreeSet::new();
    // Explicit package/root pairs, so a package reaches only the roots its
    // routes assigned. A cross product of packages and roots would write too much.
    let mut skill_assignments: Vec<projection::DirectSkillAssignment> = Vec::new();
    let mut agent_assignments: Vec<projection::DirectAgentAssignment> = Vec::new();
    let mut command_assignments: Vec<projection::DirectCommandAssignment> = Vec::new();
    for (index, route) in plan.routes.iter().enumerate() {
        // A native-activated route is served by its registration, and a
        // retained route keeps the prior native registration. Neither writes.
        if route.native == NativeRouteStatus::Native || retained.contains(&index) {
            continue;
        }
        let root = match &route.choice {
            ComponentRouteChoice::Direct { root } | ComponentRouteChoice::ServedByRoot { root } => {
                if matches!(route.choice, ComponentRouteChoice::ServedByRoot { .. }) {
                    served.insert(route.target);
                }
                root
            }
            ComponentRouteChoice::Unsupported { reason }
            | ComponentRouteChoice::Blocked { reason } => {
                evidence.run_warnings.push(format!(
                    "{} provider {} {:?} {}: {reason}",
                    route.target,
                    route.identity.plugin_name,
                    route.identity.kind,
                    route.identity.component_id
                ));
                continue;
            }
            ComponentRouteChoice::Disabled => continue,
        };
        let Some(provider) = rendered
            .iter()
            .find(|p| p.generation.source == route.identity.source)
        else {
            continue;
        };
        let alias = route
            .alias
            .as_deref()
            .unwrap_or(&route.identity.component_id);
        match route.identity.kind {
            ComponentKind::Skill => {
                let Some(path) = skill_source_dir(provider, &route.identity.component_id) else {
                    continue;
                };
                let package = projection::DirectSkillPackage {
                    source_id: route.identity.source.as_str().to_string(),
                    original_name: route.identity.component_id.clone(),
                    alias: alias.to_string(),
                    source_path: path,
                };
                // One assignment per package and physical root. Targets that
                // share the root join the same assignment, so the root is
                // written once.
                let physical = physical_root(home, root);
                let index = match skill_assignments.iter().position(|assignment| {
                    assignment.package == package && assignment.root.path == physical
                }) {
                    Some(index) => index,
                    None => {
                        skill_assignments.push(projection::DirectSkillAssignment {
                            package,
                            root: projection::DirectSkillRoot {
                                path: physical,
                                targets: Vec::new(),
                            },
                        });
                        skill_assignments.len() - 1
                    }
                };
                let targets = &mut skill_assignments[index].root.targets;
                if !targets.contains(&route.target) {
                    targets.push(route.target);
                }
            }
            ComponentKind::Agent => {
                if let Some(payload) = payloads.get(&(route.identity.clone(), route.target)) {
                    agent_assignments.push(projection::DirectAgentAssignment {
                        target: route.target,
                        input: projection::DirectAgentInput {
                            source_id: route.identity.source.as_str().to_string(),
                            name: alias.to_string(),
                            content: payload.body.clone(),
                        },
                    });
                }
            }
            ComponentKind::Command => {
                if let Some(payload) = payloads.get(&(route.identity.clone(), route.target)) {
                    let destination = command_destination(route.target);
                    // Only a command converted to a skill has the positional and
                    // argument-expansion limits. A native command keeps its body.
                    let input = projection::DirectCommandSkillInput {
                        name: alias.to_string(),
                        body: payload.body.clone(),
                    };
                    if destination == projection::DirectCommandDestination::CommandSkill {
                        if let Some(reason) = input.unsupported_reason() {
                            evidence.run_warnings.push(format!(
                                "{} provider command {}: {}",
                                route.target, input.name, reason
                            ));
                            continue;
                        }
                    }
                    command_assignments.push(projection::DirectCommandAssignment {
                        target: route.target,
                        source_id: route.identity.source.as_str().to_string(),
                        alias: input.name,
                        content: input.body,
                        destination,
                    });
                }
            }
            ComponentKind::Hook | ComponentKind::Resource => {}
        }
    }
    let skill_publication = projection::DirectSkillPublication {
        assignments: skill_assignments,
        served_targets: served.into_iter().collect(),
    };
    let skill_evidence = projection::publish_direct_skill_plan(&skill_publication, lockfile, false);
    stage_failures.extend(StageFailure::collect(ComponentKind::Skill, &skill_evidence));
    merge(&mut evidence, skill_evidence, &mut failed);
    // One agent batch across every target, so each target's ownership proof
    // persists together.
    let agent_publication = projection::DirectAgentPublication {
        assignments: agent_assignments,
    };
    let agent_evidence =
        projection::publish_direct_agent_plan(&agent_publication, home, lockfile, false);
    stage_failures.extend(StageFailure::collect(ComponentKind::Agent, &agent_evidence));
    merge(&mut evidence, agent_evidence, &mut failed);
    // One batch across every provider and target. Prepared bodies are written
    // as given, and no command source is rescanned.
    let command_publication = projection::DirectCommandPublication {
        assignments: command_assignments,
    };
    let command_evidence =
        projection::publish_direct_command_plan(&command_publication, home, lockfile, false);
    stage_failures.extend(StageFailure::collect(
        ComponentKind::Command,
        &command_evidence,
    ));
    merge(&mut evidence, command_evidence, &mut failed);
    // Ownership is read after every writer rewrote the lockfile, so it reflects
    // what this run recorded.
    let owned = projection::ManagedDeprojection::from_lockfile(lockfile);
    for (index, route) in plan.routes.iter_mut().enumerate() {
        if route.native == NativeRouteStatus::Native || retained.contains(&index) {
            continue;
        }
        route.publication_failure = publication_failure(route, &stage_failures, home, &owned);
        // A written route that did not publish is a required failure for its
        // target, even when no writer reported an error for it.
        if route.publication_failure.is_some() {
            failed.insert(route.target);
        }
    }
    record_unpublished_routes(&mut evidence, &plan.routes, &failed);
    (evidence, plan, failed)
}

/// Give each failed target a failure that names its unpublished routes, unless
/// a batch already reported one. The target status then reflects every
/// publication failure, whichever batch or route check caused it.
fn record_unpublished_routes(
    evidence: &mut projection::ProjectionEvidence,
    routes: &[ComponentTargetRoute],
    failed: &std::collections::BTreeSet<ProjectionTargetId>,
) {
    for target in failed {
        let entry = evidence.by_target.entry(*target).or_default();
        if entry.failure.is_some() {
            continue;
        }
        let reasons = routes
            .iter()
            .filter(|route| route.target == *target)
            .filter_map(|route| {
                route
                    .publication_failure
                    .as_ref()
                    .map(|reason| format!("{} ({reason})", route.identity.component_id))
            })
            .collect::<Vec<_>>();
        // A failure excludes a skip, as `ProjectionTargetEvidence` requires.
        entry.skipped = None;
        entry.failure = Some(projection::ProjectionExpectedFailure {
            path: None,
            reason: format!("routes did not publish: {}", reasons.join("; ")),
        });
    }
}

/// Legacy machine writer that owns skills and agents, as the projection crate names it.
const SKILL_LEGACY_WRITER: &str = "update_skills";
/// Legacy machine writer that owns commands, as the projection crate names it.
const COMMAND_LEGACY_WRITER: &str = "update_commands";

/// The direct delegation scope for one run.
///
/// Each route that names a destination becomes one decision there, including
/// blocked, unsupported, and disabled routes, so the machine writer that would
/// otherwise own that destination stays out of it. An aliased agent route also
/// reserves its plain-name file, so that writer never adds a second copy
/// beside the aliased one. Without a route plan or a
/// home directory, the scope is unavailable and machine writers withhold every
/// direct component write.
pub(crate) fn delegation_scope(
    plan: Option<&DeliveryPlan>,
    home: Option<&Path>,
) -> projection::MachineProjectionScope {
    let (Some(plan), Some(home)) = (plan, home) else {
        return projection::MachineProjectionScope {
            decisions: Vec::new(),
            unavailable: true,
        };
    };
    projection::MachineProjectionScope {
        decisions: plan
            .routes
            .iter()
            .filter_map(|route| delegation_decision(route, home).map(|decision| (route, decision)))
            .flat_map(|(route, decision)| {
                let plain = plain_agent_reservation(route, &decision);
                std::iter::once(decision).chain(plain)
            })
            .collect(),
        unavailable: false,
    }
}

/// A second decision at the plain-name file of an aliased agent route.
///
/// The plain-name agent writer otherwise still writes the agent under its
/// original name beside the aliased copy, and records that file under its own
/// category. The direct writer does not accept that record once the alias
/// drops, so the plain file would flip between owners on later syncs.
fn plain_agent_reservation(
    route: &ComponentTargetRoute,
    decision: &projection::DirectDelegationDecision,
) -> Option<projection::DirectDelegationDecision> {
    if route.identity.kind != ComponentKind::Agent || route.alias.is_none() {
        return None;
    }
    let directory = decision.destination.parent()?;
    Some(projection::DirectDelegationDecision {
        destination: directory.join(agent_file_name(route.target, &route.identity.component_id)),
        ..decision.clone()
    })
}

/// The decision one route makes, or `None` when no direct writer publishes
/// the route's kind. A non-published route takes its kind's native root.
fn delegation_decision(
    route: &ComponentTargetRoute,
    home: &Path,
) -> Option<projection::DirectDelegationDecision> {
    let (outcome, root) = match &route.choice {
        ComponentRouteChoice::Direct { root } | ComponentRouteChoice::ServedByRoot { root } => {
            (projection::DirectDelegationOutcome::Delegated, root.clone())
        }
        ComponentRouteChoice::Blocked { .. } => (
            projection::DirectDelegationOutcome::Blocked,
            fallback_root(route)?,
        ),
        ComponentRouteChoice::Unsupported { .. } => (
            projection::DirectDelegationOutcome::Unsupported,
            fallback_root(route)?,
        ),
        ComponentRouteChoice::Disabled => (
            projection::DirectDelegationOutcome::Disabled,
            fallback_root(route)?,
        ),
    };
    let name = route
        .alias
        .as_deref()
        .unwrap_or(&route.identity.component_id);
    let directory = physical_root(home, &root);
    let (component, legacy_writer, destination) = match route.identity.kind {
        ComponentKind::Skill => (
            projection::DirectDelegationComponent::Skill,
            SKILL_LEGACY_WRITER,
            directory.join(name),
        ),
        ComponentKind::Agent => (
            projection::DirectDelegationComponent::Agent,
            SKILL_LEGACY_WRITER,
            directory.join(agent_file_name(route.target, name)),
        ),
        ComponentKind::Command if command_skill_root(route.target) == Some(root.as_str()) => (
            projection::DirectDelegationComponent::CommandSkill,
            COMMAND_LEGACY_WRITER,
            directory.join(name).join("SKILL.md"),
        ),
        ComponentKind::Command => (
            projection::DirectDelegationComponent::Command,
            COMMAND_LEGACY_WRITER,
            directory.join(format!("{name}.md")),
        ),
        ComponentKind::Hook | ComponentKind::Resource => return None,
    };
    Some(projection::DirectDelegationDecision {
        producer: route.identity.source.as_str().to_string(),
        component,
        target: route.target,
        destination,
        legacy_writer: legacy_writer.to_string(),
        outcome,
    })
}

/// The root a route of its kind would publish under on its target, for a
/// route that has none of its own.
fn fallback_root(route: &ComponentTargetRoute) -> Option<String> {
    native_entry_roots(route.target)
        .into_iter()
        .find(|(kind, _)| *kind == route.identity.kind)
        .map(|(_, root)| root.to_string())
}

/// File name a direct agent publication gives one agent on one target.
fn agent_file_name(target: ProjectionTargetId, name: &str) -> String {
    match target {
        ProjectionTargetId::Codex => format!("{name}.toml"),
        ProjectionTargetId::Copilot => format!("{name}.agent.md"),
        _ => format!("{name}.md"),
    }
}

/// Form a prepared command takes on one target. A target with a command-to-skill
/// root publishes every command as a skill, and any other target publishes a
/// native command file.
fn command_destination(target: ProjectionTargetId) -> projection::DirectCommandDestination {
    if command_skill_root(target).is_some() {
        projection::DirectCommandDestination::CommandSkill
    } else {
        projection::DirectCommandDestination::NativeCommand
    }
}

/// One failure a publication stage reported for one target.
struct StageFailure {
    kind: ComponentKind,
    target: ProjectionTargetId,
    path: Option<std::path::PathBuf>,
    reason: String,
}

impl StageFailure {
    /// Failures one stage's evidence reports, tagged with the component kind
    /// that stage writes.
    fn collect(kind: ComponentKind, evidence: &projection::ProjectionEvidence) -> Vec<Self> {
        evidence
            .by_target
            .iter()
            .filter_map(|(target, target_evidence)| {
                target_evidence.failure.as_ref().map(|failure| Self {
                    kind,
                    target: *target,
                    path: failure.path.clone(),
                    reason: failure.reason.clone(),
                })
            })
            .collect()
    }

    /// Whether this failure covers the route's written copy. A failure with a
    /// path covers the copy at, under, or above that path. A failure without
    /// a path covers every route of its kind on its target, unless its reason
    /// quotes another component's name.
    fn covers(&self, route: &ComponentTargetRoute, destination: &Path) -> bool {
        if self.kind != route.identity.kind || self.target != route.target {
            return false;
        }
        match &self.path {
            // The failed path is the copy itself, a file inside it, or a root
            // above it.
            Some(path) => {
                copy_candidates(destination).contains(path)
                    || path.starts_with(destination)
                    || destination.starts_with(path)
            }
            None => match quoted_name(&self.reason) {
                Some(name) => name == route_name(route),
                None => true,
            },
        }
    }
}

/// The first single-quoted name in a projection failure reason.
fn quoted_name(reason: &str) -> Option<&str> {
    let start = reason.find('\'')? + 1;
    let length = reason[start..].find('\'')?;
    Some(&reason[start..start + length])
}

/// Name the route publishes under: its alias, or its original name.
fn route_name(route: &ComponentTargetRoute) -> &str {
    route
        .alias
        .as_deref()
        .unwrap_or(&route.identity.component_id)
}

/// Physical copy of a written route, without a file extension: the skill or
/// command-skill directory, or the agent or command file stem.
fn route_destination(route: &ComponentTargetRoute, home: &Path) -> Option<std::path::PathBuf> {
    let (ComponentRouteChoice::Direct { root } | ComponentRouteChoice::ServedByRoot { root }) =
        &route.choice
    else {
        return None;
    };
    Some(physical_root(home, root).join(route_name(route)))
}

/// Files that may hold a route's copy: the directory itself, its `SKILL.md`,
/// and the file forms the native writers use for agents and commands.
fn copy_candidates(destination: &Path) -> Vec<std::path::PathBuf> {
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let sibling = |suffix: &str| destination.with_file_name(format!("{name}{suffix}"));
    vec![
        destination.to_path_buf(),
        destination.join("SKILL.md"),
        sibling(".md"),
        sibling(".toml"),
        sibling(".agent.md"),
    ]
}

/// Why a written route's copy is not published, judged per written path.
///
/// A failure another route's write reported on the same target does not
/// affect this route. A route whose copy is absent after publication is not
/// published either, even when no writer reported a failure for it. A copy
/// the lockfile does not record as owned is not published either, because
/// retirement would then rest on output ccync cannot prove it wrote.
fn publication_failure(
    route: &ComponentTargetRoute,
    failures: &[StageFailure],
    home: &Path,
    owned: &projection::ManagedDeprojection,
) -> Option<String> {
    let destination = route_destination(route, home)?;
    if let Some(failure) = failures
        .iter()
        .find(|failure| failure.covers(route, &destination))
    {
        return Some(failure.reason.clone());
    }
    let present = copy_candidates(&destination)
        .into_iter()
        .filter(|candidate| candidate.exists())
        .collect::<Vec<_>>();
    if present.is_empty() {
        return Some(format!(
            "no copy exists at {} after publication",
            destination.display()
        ));
    }
    if present
        .iter()
        .any(|candidate| owned.is_authorized_for_removal(candidate))
    {
        return None;
    }
    Some(format!(
        "the copy at {} is not recorded as owned in the lockfile",
        destination.display()
    ))
}

/// Physical path of a `~/`-relative root.
/// Source directory of one skill in a rendered generation: `skills/<id>`, or
/// the generation root itself when the provider is a single skill with its
/// `SKILL.md` at the root. `None` when neither layout holds the skill.
fn skill_source_dir(provider: &RenderedProviderGeneration, id: &str) -> Option<PathBuf> {
    let nested = provider.path.join("skills").join(id);
    if nested.is_dir() {
        return Some(nested);
    }
    let bare = provider.path.join("SKILL.md").is_file()
        && !provider.path.join("skills").exists()
        && !provider.path.join("commands").exists()
        && provider
            .components
            .iter()
            .any(|identity| identity.kind == ComponentKind::Skill && identity.component_id == id);
    bare.then(|| provider.path.clone())
}

fn physical_root(home: &Path, root: &str) -> std::path::PathBuf {
    home.join(
        root.trim_start_matches("~/")
            .replace('/', std::path::MAIN_SEPARATOR_STR),
    )
}

/// Source text of every agent and command in the rendered generations. A
/// component whose source file is absent gets no entry, which the preparation
/// reports as pending.
fn component_bodies(rendered: &[RenderedProviderGeneration]) -> ComponentBodies {
    let mut bodies = ComponentBodies::new();
    for provider in rendered {
        for identity in &provider.components {
            let body = match identity.kind {
                ComponentKind::Agent => agent_body(provider, &identity.component_id),
                ComponentKind::Command => command_body(provider, &identity.component_id),
                ComponentKind::Skill | ComponentKind::Hook | ComponentKind::Resource => None,
            };
            if let Some(body) = body {
                bodies.insert(identity.clone(), body);
            }
        }
    }
    bodies
}

/// Body of one agent in a rendered generation, `agents/<name>.md`. `None`
/// when the file is absent; `Some(Err)` when it exists but cannot be read.
fn agent_body(provider: &RenderedProviderGeneration, name: &str) -> Option<Result<String, String>> {
    let path = provider.path.join("agents").join(format!("{name}.md"));
    path.is_file().then(|| {
        fs::read_to_string(&path)
            .map_err(|error| format!("agent source {} is unreadable: {error}", path.display()))
    })
}

/// Body of one command in a rendered generation: the flat `<name>.md` file,
/// or the directory form's `SKILL.md` or `SKILL.template.md`. `None` when no
/// form exists; `Some(Err)` when the file exists but cannot be read.
fn command_body(
    provider: &RenderedProviderGeneration,
    name: &str,
) -> Option<Result<String, String>> {
    let commands = provider.path.join("commands");
    [
        commands.join(format!("{name}.md")),
        commands.join(name).join("SKILL.md"),
        commands.join(name).join("SKILL.template.md"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .map(|path| {
        fs::read_to_string(&path)
            .map_err(|error| format!("command source {} is unreadable: {error}", path.display()))
    })
}

/// Commands whose body cannot become an equivalent skill, read from each
/// rendered provider before planning. An unreadable command body cannot be
/// checked, so it does not convert either.
fn unconvertible_commands(rendered: &[RenderedProviderGeneration]) -> UnconvertibleCommands {
    let mut found = UnconvertibleCommands::new();
    for provider in rendered {
        for identity in provider
            .components
            .iter()
            .filter(|identity| identity.kind == ComponentKind::Command)
        {
            let reason = match command_body(provider, &identity.component_id) {
                Some(Ok(body)) => projection::DirectCommandSkillInput {
                    name: identity.component_id.clone(),
                    body,
                }
                .unsupported_reason()
                .map(|reason| reason.to_string()),
                Some(Err(reason)) => Some(reason),
                None => None,
            };
            if let Some(reason) = reason {
                found.insert(identity.clone(), reason);
            }
        }
    }
    found
}

/// Route facts for the routes one enabled rollout planned.
///
/// A route is published when publication wrote its own copy: its choice names
/// a root and publication recorded no failure for its path. A failed write of
/// another route on the same target does not unpublish it, so the per-target
/// `_failed` set is not consulted here; callers use it for retirement only. A
/// route is owned when the lockfile, read after publication rewrote it,
/// records its copy in any of the forms the writers use. Sync never runs a
/// loader check, so every route reports `LoaderNotRun`. Disabled routes are
/// not reported, because the user did not select them.
pub(crate) fn route_evidence(
    plan: &DeliveryPlan,
    home: &Path,
    lockfile: &Path,
) -> Vec<RouteEvidence> {
    let deprojection = projection::ManagedDeprojection::from_lockfile(lockfile);
    plan.routes
        .iter()
        .filter(|route| !matches!(route.choice, ComponentRouteChoice::Disabled))
        .map(|route| {
            let written = matches!(
                route.choice,
                ComponentRouteChoice::Direct { .. } | ComponentRouteChoice::ServedByRoot { .. }
            );
            let published = written && route.publication_failure.is_none();
            let mut evidence = RouteEvidence::from_route(
                route,
                true,
                published,
                false,
                VerificationScope::LoaderNotRun,
            );
            evidence.owned = route_destination(route, home).is_some_and(|destination| {
                copy_candidates(&destination)
                    .iter()
                    .any(|candidate| deprojection.is_authorized_for_removal(candidate))
            });
            evidence
        })
        .collect()
}

/// Re-check recorded route facts against the files and the lockfile as they
/// are now, without writing anything.
///
/// A route recorded as published is no longer published when its copy is
/// gone, and ownership is read again from the lockfile. Every other recorded
/// fact, origin and route included, is kept as the last sync recorded it.
pub fn verify_recorded_routes(
    routes: Vec<RouteEvidence>,
    home: &Path,
    lockfile: &Path,
) -> Vec<RouteEvidence> {
    let deprojection = projection::ManagedDeprojection::from_lockfile(lockfile);
    routes
        .into_iter()
        .map(|mut route| {
            let Some(destination) = route.path.as_deref().map(|path| physical_root(home, path))
            else {
                return route;
            };
            let candidates = copy_candidates(&destination);
            if route.published && !candidates.iter().any(|candidate| candidate.exists()) {
                route.published = false;
                route.failure_reason = Some(format!(
                    "the copy at {} is gone since the last sync",
                    destination.display()
                ));
            }
            route.owned = candidates
                .iter()
                .any(|candidate| deprojection.is_authorized_for_removal(candidate));
            route
        })
        .collect()
}

/// Routes the route plan blocked. A blocked route is never published, so each
/// conflict names the reason the plan gave.
pub(crate) fn route_conflicts(plan: &DeliveryPlan) -> Vec<RouteConflict> {
    plan.routes
        .iter()
        .filter_map(|route| match &route.choice {
            ComponentRouteChoice::Blocked { reason } => Some(RouteConflict {
                target: route.target,
                component: route.identity.component_id.clone(),
                reason: reason.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// Targets with at least one component that no route makes discoverable
/// without a user step, each with the reason for its first such component.
///
/// A blocked route is never written, so the target cannot discover that
/// component directly. Such a target has not been replaced and keeps its
/// combined surface. A disabled or unsupported route does not count: the
/// user did not select a disabled component, and an unsupported kind has no
/// direct surface on any route. A blocked component of a plugin group that
/// activates natively on the same target does not count either, when that
/// target's native loader is known to load the component's kind from the
/// package.
pub(crate) fn undiscoverable_targets(plan: &DeliveryPlan) -> BTreeMap<ProjectionTargetId, String> {
    let native_groups = plan
        .routes
        .iter()
        .filter(|route| route.native == NativeRouteStatus::Native)
        .map(|route| (route.target, route.identity.plugin_name.as_str()))
        .collect::<BTreeSet<_>>();
    let mut found = BTreeMap::new();
    for route in &plan.routes {
        if native_groups.contains(&(route.target, route.identity.plugin_name.as_str()))
            && native_package_loads(route.target, route.identity.kind)
        {
            continue;
        }
        if let ComponentRouteChoice::Blocked { reason } = &route.choice {
            found.entry(route.target).or_insert_with(|| {
                format!(
                    "{:?} {} has a blocked route ({reason})",
                    route.identity.kind, route.identity.component_id
                )
            });
        }
    }
    found
}

/// Component kinds a native plugin package is observed to load. Claude Code
/// loads skills, agents, and commands from a plugin. Codex is only relied on
/// for skills.
fn native_package_loads(target: ProjectionTargetId, kind: ComponentKind) -> bool {
    match target {
        ProjectionTargetId::Claude => matches!(
            kind,
            ComponentKind::Skill | ComponentKind::Agent | ComponentKind::Command
        ),
        ProjectionTargetId::Codex => kind == ComponentKind::Skill,
        _ => false,
    }
}

/// One text line for a route with its retirement and conflict labels. A
/// conflicting route shows its conflict and no publication, and it does not
/// repeat the write-error action, because a blocked route never wrote.
pub fn route_outcome_line(
    evidence: &RouteEvidence,
    retirement: Option<&RouteRetirement>,
    conflict: Option<&str>,
) -> String {
    let mut line = match conflict {
        Some(reason) => format!(
            "{} {:?} {}; {}; not published; conflict: {reason}; action: resolve the conflict, then rerun `ccync sync`",
            evidence.target,
            evidence.kind,
            evidence.original_name,
            if evidence.selected {
                "selected"
            } else {
                "not selected"
            },
        ),
        None => route_evidence_line(evidence),
    };
    match retirement {
        Some(RouteRetirement::Done) => line.push_str("; retirement: done"),
        Some(RouteRetirement::Kept { reason }) => {
            line.push_str(&format!("; retirement: kept ({reason})"));
        }
        None => {}
    }
    line
}

/// Find same-name entries in each selected target's native roots that the
/// projection lockfile does not prove ccync-owned.
///
/// Each entry becomes a foreign component. The alias planner then renames the
/// provider component, and the route records the foreign path that caused it.
/// Entries ccync wrote earlier are skipped, so a re-sync never aliases itself.
pub fn foreign_components(
    components: &[ComponentIdentity],
    selected: &[ProjectionTargetId],
    home: &Path,
    lockfile: &Path,
) -> Vec<ForeignComponent> {
    let owned = projection::ManagedDeprojection::from_lockfile(lockfile);
    let Ok(source) = ProviderSourceKey::new(FOREIGN_SOURCE) else {
        return Vec::new();
    };
    let mut found: Vec<ForeignComponent> = Vec::new();
    for target in selected {
        for (kind, root) in native_entry_roots(*target) {
            let physical = home.join(
                root.trim_start_matches("~/")
                    .replace('/', std::path::MAIN_SEPARATOR_STR),
            );
            for component in components.iter().filter(|item| item.kind == kind) {
                let name = &component.component_id;
                let candidates = [
                    physical.join(name),
                    physical.join(format!("{name}.md")),
                    physical.join(format!("{name}.toml")),
                    physical.join(format!("{name}.agent.md")),
                ];
                for candidate in candidates {
                    // The command-to-skill writer records the `SKILL.md` file,
                    // not its directory, so either record proves ownership.
                    if !candidate.exists()
                        || owned.is_authorized_for_removal(&candidate)
                        || owned.is_authorized_for_removal(&candidate.join("SKILL.md"))
                    {
                        continue;
                    }
                    let Ok(identity) = ComponentIdentity::new(
                        source.clone(),
                        FOREIGN_SOURCE,
                        None,
                        kind,
                        name.clone(),
                    ) else {
                        continue;
                    };
                    if found.iter().any(|item| item.identity == identity) {
                        continue;
                    }
                    found.push(ForeignComponent {
                        identity,
                        path: candidate.display().to_string(),
                    });
                }
            }
        }
    }
    found
}

/// One note per unowned agent file that still holds a provider agent's plain
/// name in a selected target's native agent root. ccync never deletes these
/// files. The note names the file and gives the command that removes it, for
/// the owner to run when the file is a leftover copy.
fn leftover_agent_notes(
    rendered: &[RenderedProviderGeneration],
    selected: &[ProjectionTargetId],
    home: &Path,
    lockfile: &Path,
) -> Vec<String> {
    let owned = projection::ManagedDeprojection::from_lockfile(lockfile);
    plain_agent_files(rendered, selected, home)
        .into_iter()
        .filter(|(_, _, _, candidate)| is_unowned_file(candidate, &owned))
        .map(|(_, _, name, candidate)| {
            let command = if cfg!(windows) {
                format!("Remove-Item -LiteralPath '{}'", candidate.display())
            } else {
                format!("rm '{}'", candidate.display())
            };
            format!(
                "kept unowned agent {}: it has no ownership record and holds the name of provider agent '{name}'. If it is a leftover copy from an older ccync, remove it with: {command}",
                candidate.display()
            )
        })
        .collect()
}

/// Plain-name paths of every provider agent in each selected target's native
/// agent root, each root once: target, root, agent name, and path.
fn plain_agent_files(
    rendered: &[RenderedProviderGeneration],
    selected: &[ProjectionTargetId],
    home: &Path,
) -> Vec<(ProjectionTargetId, &'static str, String, PathBuf)> {
    let names = rendered
        .iter()
        .flat_map(|provider| provider.components.iter())
        .filter(|identity| identity.kind == ComponentKind::Agent)
        .map(|identity| identity.component_id.clone())
        .collect::<BTreeSet<_>>();
    let mut seen = BTreeSet::new();
    let mut found = Vec::new();
    for target in selected {
        for (kind, root) in native_kind_roots(*target) {
            if kind != ComponentKind::Agent || !seen.insert(root) {
                continue;
            }
            let physical = physical_root(home, root);
            for name in &names {
                let candidate = physical.join(agent_file_name(*target, name));
                found.push((*target, root, name.clone(), candidate));
            }
        }
    }
    found
}

/// Whether `path` is a regular file, not a link, that the lockfile does not
/// prove ccync-owned.
fn is_unowned_file(path: &Path, owned: &projection::ManagedDeprojection) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
        && !owned.is_authorized_for_removal(path)
}

/// Native skill, command, and agent roots for one target. This mirrors the
/// rows of `delivery::native_root`, which the delivery module does not export.
/// A target without a native command root lists its command-to-skill root for
/// commands, because a converted command shares that skill root's names.
fn native_entry_roots(target: ProjectionTargetId) -> Vec<(ComponentKind, &'static str)> {
    let mut roots = native_kind_roots(target);
    if let Some(root) = command_skill_root(target) {
        roots.push((ComponentKind::Command, root));
    }
    roots
}

fn native_kind_roots(target: ProjectionTargetId) -> Vec<(ComponentKind, &'static str)> {
    match target {
        ProjectionTargetId::Claude => vec![
            (ComponentKind::Skill, "~/.claude/skills"),
            (ComponentKind::Command, "~/.claude/commands"),
            (ComponentKind::Agent, "~/.claude/agents"),
        ],
        ProjectionTargetId::Codex => vec![
            (ComponentKind::Skill, "~/.agents/skills"),
            (ComponentKind::Agent, "~/.codex/agents"),
        ],
        ProjectionTargetId::Copilot => vec![
            (ComponentKind::Skill, "~/.copilot/skills"),
            (ComponentKind::Agent, "~/.copilot/agents"),
        ],
        ProjectionTargetId::Opencode => vec![
            (ComponentKind::Skill, "~/.config/opencode/skills"),
            (ComponentKind::Command, "~/.config/opencode/commands"),
            (ComponentKind::Agent, "~/.config/opencode/agents"),
        ],
        ProjectionTargetId::AgyCli => vec![
            (ComponentKind::Skill, "~/.gemini/antigravity-cli/skills"),
            (ComponentKind::Agent, "~/.gemini/config/agents"),
        ],
        ProjectionTargetId::AgyIde => vec![(ComponentKind::Skill, "~/.gemini/config/skills")],
        ProjectionTargetId::AgyGui => vec![
            (ComponentKind::Skill, "~/.gemini/config/skills"),
            (ComponentKind::Agent, "~/.gemini/config/agents"),
        ],
    }
}

fn fill_physical_identities(plan: &mut DeliveryPlan) {
    let layout = ccync_foundation::paths::EffectiveLayout::load_or_legacy();
    for route in &mut plan.routes {
        let root = match &route.choice {
            ComponentRouteChoice::Direct { root } | ComponentRouteChoice::ServedByRoot { root } => {
                root
            }
            _ => continue,
        };
        route.physical_identity = PhysicalIdentity::Rejected;
        if layout.is_err() {
            continue;
        }
        let Some(relative) = root.strip_prefix("~/") else {
            continue;
        };
        let Ok(path) = ccync_foundation::provider::RelativePath::new(relative) else {
            continue;
        };
        let scoped = ccync_foundation::provider::ScopedPath {
            root: ccync_foundation::provider::PathRoot::UserHome,
            path,
        };
        route.physical_identity = match scoped.resolve_validated() {
            Ok(Some(path)) => PhysicalIdentity::Validated(path),
            Ok(None) | Err(_) => PhysicalIdentity::Rejected,
        };
    }
}

fn merge(
    into: &mut projection::ProjectionEvidence,
    other: projection::ProjectionEvidence,
    failed: &mut std::collections::BTreeSet<ProjectionTargetId>,
) {
    for (target, target_evidence) in other.by_target {
        if target_evidence.failure.is_some() {
            failed.insert(target);
        }
        // A later batch appends to the target's evidence. An earlier failure
        // stays, so a later success on the same target cannot hide it.
        let entry = into.by_target.entry(target).or_default();
        entry.paths.extend(target_evidence.paths);
        entry.warnings.extend(target_evidence.warnings);
        entry.failure = entry.failure.take().or(target_evidence.failure);
        entry.skipped = if entry.failure.is_some() {
            None
        } else {
            entry.skipped.take().or(target_evidence.skipped)
        };
    }
    into.completed_targets.extend(other.completed_targets);
    into.run_warnings.extend(other.run_warnings);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill_route(root: &str) -> ComponentTargetRoute {
        ComponentTargetRoute {
            identity: ComponentIdentity::new(
                ProviderSourceKey::new("example").unwrap(),
                "example",
                None,
                ComponentKind::Skill,
                "example",
            )
            .unwrap(),
            physical_identity: PhysicalIdentity::Unchecked,
            target: ProjectionTargetId::Copilot,
            choice: ComponentRouteChoice::Direct {
                root: root.to_string(),
            },
            alias: None,
            foreign_alias_path: None,
            discovered_root_count: 1,
            multi_root: false,
            unsupported_hooks_provider: None,
            command_to_skill: false,
            publication_failure: None,
            native: crate::delivery::NativeRouteStatus::Loose,
        }
    }

    #[test]
    fn face_capabilities_reject_escaping_route_identity() {
        let mut plan = DeliveryPlan {
            routes: vec![skill_route("~/../outside")],
            shared_roots: Vec::new(),
        };

        fill_physical_identities(&mut plan);

        assert_eq!(plan.routes[0].physical_identity, PhysicalIdentity::Rejected);
    }

    /// A route of plugin `example` on `target`, with the given kind, choice,
    /// and native status.
    fn group_route(
        target: ProjectionTargetId,
        kind: ComponentKind,
        choice: ComponentRouteChoice,
        native: crate::delivery::NativeRouteStatus,
    ) -> ComponentTargetRoute {
        let mut route = skill_route("~/.claude/skills");
        route.identity = ComponentIdentity::new(
            ProviderSourceKey::new("example").unwrap(),
            "example",
            None,
            kind,
            "helper",
        )
        .unwrap();
        route.target = target;
        route.choice = choice;
        route.native = native;
        route
    }

    #[test]
    fn native_group_covers_blocked_components_its_loader_reads() {
        use crate::delivery::NativeRouteStatus;
        let blocked = || ComponentRouteChoice::Blocked {
            reason: "conflict".to_string(),
        };
        let written = || ComponentRouteChoice::Direct {
            root: "~/.agents/skills".to_string(),
        };
        let plan = |target, kind, native| DeliveryPlan {
            routes: vec![
                group_route(target, ComponentKind::Skill, written(), native),
                group_route(target, kind, blocked(), NativeRouteStatus::Loose),
            ],
            shared_roots: Vec::new(),
        };

        // Claude loads agents from its native package.
        let claude = plan(
            ProjectionTargetId::Claude,
            ComponentKind::Agent,
            NativeRouteStatus::Native,
        );
        assert!(undiscoverable_targets(&claude).is_empty());
        // Codex is only relied on for skills, so a blocked agent still counts.
        let codex = plan(
            ProjectionTargetId::Codex,
            ComponentKind::Agent,
            NativeRouteStatus::Native,
        );
        assert!(undiscoverable_targets(&codex).contains_key(&ProjectionTargetId::Codex));
        let codex_skill = plan(
            ProjectionTargetId::Codex,
            ComponentKind::Skill,
            NativeRouteStatus::Native,
        );
        assert!(undiscoverable_targets(&codex_skill).is_empty());
        // Without native activation the blocked agent counts.
        let loose = plan(
            ProjectionTargetId::Claude,
            ComponentKind::Agent,
            NativeRouteStatus::Loose,
        );
        assert!(undiscoverable_targets(&loose).contains_key(&ProjectionTargetId::Claude));
    }

    #[test]
    fn aliased_agent_route_also_reserves_its_plain_name() {
        use crate::delivery::NativeRouteStatus;
        let written = || ComponentRouteChoice::Direct {
            root: "~/.config/opencode/agents".to_string(),
        };
        let mut aliased = group_route(
            ProjectionTargetId::Opencode,
            ComponentKind::Agent,
            written(),
            NativeRouteStatus::Loose,
        );
        aliased.alias = Some("example-example-helper".to_string());
        let plain = group_route(
            ProjectionTargetId::Opencode,
            ComponentKind::Agent,
            written(),
            NativeRouteStatus::Loose,
        );
        let home = Path::new("home");
        let destinations = |route: ComponentTargetRoute| {
            let plan = DeliveryPlan {
                routes: vec![route],
                shared_roots: Vec::new(),
            };
            let mut found = delegation_scope(Some(&plan), Some(home))
                .decisions
                .into_iter()
                .map(|decision| decision.destination)
                .collect::<Vec<_>>();
            found.sort();
            found
        };
        let agents = physical_root(home, "~/.config/opencode/agents");

        assert_eq!(
            destinations(aliased),
            vec![
                agents.join("example-example-helper.md"),
                agents.join("helper.md")
            ],
        );
        assert_eq!(destinations(plain), vec![agents.join("helper.md")]);
    }

    #[test]
    fn written_route_without_output_or_owned_record_is_not_published() {
        let home =
            std::env::temp_dir().join(format!("ccync-publication-failure-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        let owned = projection::ManagedDeprojection::from_lockfile(&home.join("lock.json"));
        let route = skill_route("~/.copilot/skills");

        // No writer error, and no copy on disk: the output is missing.
        let missing = publication_failure(&route, &[], &home, &owned);
        assert!(
            missing
                .as_deref()
                .is_some_and(|reason| reason.contains("no copy exists")),
            "{missing:?}"
        );

        // A copy on disk that the lockfile does not record as owned.
        let copy = home.join(".copilot/skills/example");
        fs::create_dir_all(&copy).unwrap();
        fs::write(copy.join("SKILL.md"), "body\n").unwrap();
        let unowned = publication_failure(&route, &[], &home, &owned);
        assert!(
            unowned
                .as_deref()
                .is_some_and(|reason| reason.contains("not recorded as owned")),
            "{unowned:?}"
        );

        let _ = fs::remove_dir_all(&home);
    }

    fn failed_batch(target: ProjectionTargetId, reason: &str) -> projection::ProjectionEvidence {
        let mut evidence = projection::ProjectionEvidence::default();
        evidence.by_target.insert(
            target,
            projection::ProjectionTargetEvidence {
                failure: Some(projection::ProjectionExpectedFailure {
                    path: None,
                    reason: reason.to_string(),
                }),
                ..Default::default()
            },
        );
        evidence
    }

    #[test]
    fn later_success_keeps_earlier_target_failure() {
        let target = ProjectionTargetId::Copilot;
        let mut merged = projection::ProjectionEvidence::default();
        let mut failed = std::collections::BTreeSet::new();
        merge(
            &mut merged,
            failed_batch(target, "skill write held"),
            &mut failed,
        );

        let mut agent_batch = projection::ProjectionEvidence::default();
        agent_batch.by_target.insert(
            target,
            projection::ProjectionTargetEvidence {
                warnings: vec!["agent written".to_string()],
                ..Default::default()
            },
        );
        merge(&mut merged, agent_batch, &mut failed);

        let entry = &merged.by_target[&target];
        assert_eq!(
            entry
                .failure
                .as_ref()
                .map(|failure| failure.reason.as_str()),
            Some("skill write held")
        );
        assert_eq!(entry.warnings, vec!["agent written".to_string()]);
        assert!(failed.contains(&target));
    }

    #[test]
    fn unpublished_route_gives_failed_target_a_failure_naming_it() {
        let target = ProjectionTargetId::Copilot;
        let mut evidence = projection::ProjectionEvidence::default();
        let mut route = skill_route("~/.copilot/skills");
        route.publication_failure = Some("no copy exists at x after publication".to_string());
        let failed = std::collections::BTreeSet::from([target]);

        record_unpublished_routes(&mut evidence, &[route], &failed);

        let reason = evidence.by_target[&target]
            .failure
            .as_ref()
            .map(|failure| failure.reason.clone())
            .unwrap_or_default();
        assert!(reason.contains("example"), "{reason}");
        assert!(reason.contains("no copy exists"), "{reason}");
    }

    #[test]
    fn existing_target_failure_is_not_replaced_by_route_summary() {
        let target = ProjectionTargetId::Copilot;
        let mut evidence = failed_batch(target, "skill write held");
        let mut route = skill_route("~/.copilot/skills");
        route.publication_failure = Some("no copy exists".to_string());
        let failed = std::collections::BTreeSet::from([target]);

        record_unpublished_routes(&mut evidence, &[route], &failed);

        assert_eq!(
            evidence.by_target[&target]
                .failure
                .as_ref()
                .map(|failure| failure.reason.as_str()),
            Some("skill write held")
        );
    }

    #[test]
    fn accumulator_derives_failed_for_target_after_skill_failure_and_agent_success() {
        let target = ProjectionTargetId::Copilot;
        let mut merged = projection::ProjectionEvidence::default();
        let mut failed = std::collections::BTreeSet::new();
        merge(
            &mut merged,
            failed_batch(target, "skill write held"),
            &mut failed,
        );
        let mut agent_batch = projection::ProjectionEvidence::default();
        agent_batch.completed_targets.insert(target);
        merge(&mut merged, agent_batch, &mut failed);

        let mut accumulator = crate::install::ProjectionRunAccumulator::new("normal", [target]);
        accumulator.record(crate::install::ProjectionPhase::Machine, &merged);
        let report = accumulator.finish();

        assert!(
            matches!(
                report.targets[&target].status,
                crate::install::ProjectionTargetStatus::Failed { .. }
            ),
            "{:?}",
            report.targets[&target].status
        );
    }
}

/// Marketplace name every probe uses. The probe only checks that the native
/// CLI answers; it never registers this name.
const NATIVE_PROBE_MARKETPLACE: &str = "ccync-probe";

/// Directory under the provider render root that holds one stable marketplace
/// root per native target and plugin. Stable roots keep the marketplace name
/// bound to one location across updates.
const NATIVE_MARKETPLACE_DIR: &str = "native-marketplaces";

/// Prefix of every native package view directory name.
const NATIVE_VIEW_PREFIX: &str = "v-";

/// A native CLI that is never reachable. Callers that must not touch any
/// host CLI use it; every native route then falls back to its loose copy.
pub struct UnavailableNativeRunner;

impl NativeCommandRunner for UnavailableNativeRunner {
    fn run(&self, executable: NativeExecutable, _args: &[&str]) -> Result<Vec<u8>, String> {
        Err(format!(
            "{} is not available to this run",
            executable.command()
        ))
    }

    fn observe(
        &self,
        executable: NativeExecutable,
        _selector: &NativeSelector,
    ) -> Result<crate::plugin_native::NativeRegistration, String> {
        Err(format!(
            "{} is not available to this run",
            executable.command()
        ))
    }

    fn marketplace_root(
        &self,
        executable: NativeExecutable,
        _marketplace: &str,
    ) -> Result<Option<String>, String> {
        Err(format!(
            "{} is not available to this run",
            executable.command()
        ))
    }
}

/// Adapter state for each selected target that has a native CLI. A target
/// whose loader probe is not verified is reported as failed, so its routes
/// keep their loose copies with a stated reason.
pub fn native_adapter_states(
    runner: &dyn NativeCommandRunner,
    targets: &[ProjectionTargetId],
) -> NativeAdapters {
    targets
        .iter()
        .filter_map(|&target| {
            let executable = NativeExecutable::for_target(target)?;
            let state = if !runner.mutation_verified(executable) {
                NativeAdapterState::Failed {
                    reason: runner.mutation_block_reason(executable),
                }
            } else {
                match runner.marketplace_root(executable, NATIVE_PROBE_MARKETPLACE) {
                    Ok(_) => NativeAdapterState::Ready,
                    Err(reason) => NativeAdapterState::Failed { reason },
                }
            };
            Some((target, state))
        })
        .collect()
}

/// Read-only preview of the native cutover for a sync run. It writes nothing
/// and calls no mutating native command. Each returned line says what a real
/// sync would do with a native group, or why the group keeps its loose route.
pub fn preview_native_cutover(
    rendered: &[RenderedProviderGeneration],
    selected: &[ProjectionTargetId],
    home: &Path,
    lockfile: &Path,
    runner: &dyn NativeCommandRunner,
) -> Vec<String> {
    let mut inventory = DeliveryInventory::from_generations(rendered);
    let identities = inventory
        .components
        .iter()
        .map(|component| component.identity.clone())
        .collect::<Vec<_>>();
    inventory.foreign_components = foreign_components(&identities, selected, home, lockfile);
    let adapters = native_adapter_states(runner, selected);
    let prepared = prepare_delivery_with_native_routes(
        &inventory,
        selected,
        &[],
        &unconvertible_commands(rendered),
        &component_bodies(rendered),
        &adapters,
        fill_physical_identities,
    );
    let plan = prepared.plan;
    let lines = plan
        .routes
        .iter()
        .filter_map(|route| match &route.native {
            NativeRouteStatus::Native => Some(format!(
                "{} plugin {}: native activation planned",
                route.target, route.identity.plugin_name
            )),
            NativeRouteStatus::Fallback { reason } => Some(format!(
                "{} plugin {}: loose route kept: {reason}",
                route.target, route.identity.plugin_name
            )),
            NativeRouteStatus::Loose | NativeRouteStatus::ObservedLoose => None,
        })
        .collect::<BTreeSet<_>>();
    lines.into_iter().collect()
}

/// Activate every native group in `plan` and mark its routes `Native`. A group
/// that cannot be proven becomes `Fallback`. A group that had a committed
/// native registration is also added to `retained`, so its loose routes are
/// not written over the still-active native registration.
fn cut_over_native_groups(
    plan: &mut DeliveryPlan,
    rendered: &[RenderedProviderGeneration],
    runner: &dyn NativeCommandRunner,
    home: &Path,
    lockfile: &Path,
    retained: &mut BTreeSet<usize>,
) -> Vec<String> {
    let mut groups: BTreeMap<(ProjectionTargetId, String), Vec<usize>> = BTreeMap::new();
    for (index, route) in plan.routes.iter().enumerate() {
        if route.native == NativeRouteStatus::Native {
            groups
                .entry((route.target, route.identity.plugin_name.clone()))
                .or_default()
                .push(index);
        }
    }
    let committed = committed_rows();
    let mut notes = Vec::new();
    for ((target, plugin), members) in groups {
        let prior = committed
            .iter()
            .find(|row| row.target == target && row.plugin_name == plugin)
            .cloned();
        let activation = activate_group(
            target,
            &plugin,
            &members,
            plan,
            rendered,
            runner,
            home,
            lockfile,
            prior.as_ref(),
        );
        match activation {
            Ok(note) => notes.push(note),
            Err(reason) => {
                // A committed legacy registration for this management ID keeps
                // its native route too, so the loose copy never loads beside it.
                let legacy_kept = !legacy_selector_rows(
                    target,
                    &plan.routes[members[0]].identity.source,
                    &plugin,
                )
                .is_empty();
                let keeps_prior = prior.is_some() || legacy_kept;
                let outcome = if prior.is_some() {
                    "prior native route kept"
                } else if legacy_kept {
                    "legacy native route kept"
                } else {
                    "loose route kept"
                };
                let text = format!("{target} plugin {plugin}: {outcome}: {reason}");
                notes.push(text.clone());
                for &index in &members {
                    plan.routes[index].native = NativeRouteStatus::Fallback {
                        reason: text.clone(),
                    };
                    if keeps_prior {
                        retained.insert(index);
                    }
                }
            }
        }
    }
    notes
}

/// Activate one plugin group. The routes stay `Native` only after the native
/// registration is verified, its ownership committed, and every loose copy it
/// replaces is removed. Any earlier failure leaves the loose state untouched.
#[allow(clippy::too_many_arguments)]
fn activate_group(
    target: ProjectionTargetId,
    plugin: &str,
    members: &[usize],
    plan: &mut DeliveryPlan,
    rendered: &[RenderedProviderGeneration],
    runner: &dyn NativeCommandRunner,
    home: &Path,
    lockfile: &Path,
    prior: Option<&CommittedOwnership>,
) -> Result<String, String> {
    let source = plan.routes[members[0]].identity.source.clone();
    let generation = rendered
        .iter()
        .find(|provider| provider.generation.source == source)
        .ok_or("provider generation is not rendered")?;
    let package_target = package_target(target).ok_or("no native package for this target")?;
    let executable = NativeExecutable::for_target(target).ok_or("no native adapter")?;

    let view = generate_native_package_view(generation, package_target)
        .map_err(|error| error.to_string())?;
    let root = native_marketplace_root(target, plugin)?;
    let view_id = view.view_id.clone();
    stage_view(&view.path, &root, &view_id)?;
    write_marketplace(&root, package_target, plugin, &view_id)?;

    let selector = NativeSelector::new(format!("{plugin}@ccync-{plugin}"))
        .map_err(|error| error.to_string())?;
    // Read the enablement the user left the owned registration in. A
    // registration that is not installed yet starts enabled.
    let observed = runner.observe(executable, &selector)?;
    let enablement = if observed.present && !observed.enabled {
        EnablementIntent::Disabled
    } else {
        EnablementIntent::Enabled
    };
    // Committed registrations for this management ID under another selector are
    // migrated only when each one is proven to be ccync's own. Any unproven one
    // blocks the group before a native change, so both registrations stay.
    let legacy = legacy_selector_rows(target, &source, plugin);
    for row in &legacy {
        prove_legacy_registration(runner, executable, row, generation)?;
    }
    let desired = CommittedOwnership {
        target,
        selector: selector.clone(),
        plugin_name: plugin.to_string(),
        upstream_revision: generation.upstream_revision.clone(),
        generation: generation.generation.clone(),
        capabilities: CapabilityInventory::new(),
        client_scope: Some(
            ClientScope::new(format!("ccync-{}", target.as_str())).map_err(|e| e.to_string())?,
        ),
        source_digest: Some(generation.generation.package_digest.clone()),
        view_digest: Some(view.view_digest.clone()),
        observed_preimage: None,
        enablement_intent: Some(enablement),
        group_substep: None,
    };

    // The committed package matches when only the recorded enablement differs,
    // so the switch changes nothing and only the enablement is recorded.
    let same_package = prior.is_some_and(|row| {
        let mut row = row.clone();
        row.enablement_intent = desired.enablement_intent;
        row == desired
    });
    let current = same_package
        && observed.available
        && observed.present
        && !observed.broken
        && observed.selector.as_ref() == Some(&selector);
    // A legacy registration still needs its migration, which runs through the
    // journaled install path, so the shortcut never skips it.
    let current = current && legacy.is_empty();
    // A disabled registration cannot be replaced through the native switch,
    // because the switch removes only enabled registrations. It is kept as is.
    if !current && enablement == EnablementIntent::Disabled {
        return Err("owned registration is disabled and its package changed".to_string());
    }
    // Every reader that the shared copy serves gets its private replacement
    // on disk and verified before the native change or any retirement.
    let replacements = replacement_targets(plan, members);
    publish_replacements(plan, &replacements, generation, home, lockfile)?;
    // Re-read ownership and complete reader coverage after the final routes
    // have been published and verified. This evidence gates every retirement.
    let copies = loose_copies(plan, members, home, lockfile)?;
    if current {
        if prior != Some(&desired) {
            commit_native_ownership(desired.clone()).map_err(|error| error.to_string())?;
        }
        retire_copies(&copies, lockfile)?;
    } else {
        // Each loose copy is saved before the intent names it, so an
        // interrupted switch can restore the prior route from the journal.
        let group = format!("{target}-{plugin}");
        // An earlier intent for this selector is the only record that can
        // prove a registration an interrupted run left. It stays journaled
        // unchanged, with its preimage references, until it is adopted or
        // cleared, so a blocked retry never replaces it.
        let pending =
            pending_native_intent(target, &selector).map_err(|error| error.to_string())?;
        // The new install and the legacy removal share this one intent, so an
        // interrupted migration names both selectors for recovery.
        let mut fingerprints = vec![OwnershipFingerprint {
            selector: selector.clone(),
            package_digest: generation.generation.package_digest.clone(),
        }];
        fingerprints.extend(legacy.iter().map(|row| OwnershipFingerprint {
            selector: row.selector.clone(),
            package_digest: row.generation.package_digest.clone(),
        }));
        let mut intent = PendingIntent {
            target,
            ownership_fingerprints: fingerprints,
            scoped_paths: Vec::new(),
            preimage_refs: Vec::new(),
            previous_generation: prior.map(|row| row.generation.clone()),
            desired_generation: generation.generation.clone(),
            phase: IntentPhase::Publishing,
            client_scope: desired.client_scope.clone(),
            source_digest: desired.source_digest.clone(),
            view_digest: desired.view_digest.clone(),
            observed_preimage: None,
            enablement_intent: Some(enablement),
            group_substep: None,
        };
        // An earlier intent is reused only when it describes this operation.
        // Its saved preimages live at fixed slots, so re-saving would overwrite
        // them, and the reused intent keeps them as they are.
        let keeps_earlier = pending.as_ref().is_some_and(|earlier| {
            !earlier.preimage_refs.is_empty() && same_operation(earlier, &intent)
        });
        let recorded = match pending.clone() {
            Some(earlier) if keeps_earlier => earlier,
            earlier => {
                // The journal must name the generation being installed. An
                // earlier intent for another generation keeps its saved
                // preimages, which hold the prior route, so they carry over
                // to the new intent without saving again.
                let carried = earlier.filter(|earlier| !earlier.preimage_refs.is_empty());
                intent.preimage_refs = match &carried {
                    Some(earlier) => earlier.preimage_refs.clone(),
                    None => save_preimages(&copies, home, &group)?,
                };
                // The intent is recorded before the first native call. If it
                // cannot be recorded, no native call happens.
                record_native_intent(intent.clone()).map_err(|error| error.to_string())?;
                // The new intent now owns the carried preimages, so the earlier
                // intent is dropped and the journal names only this generation.
                if let Some(earlier) = carried {
                    clear_native_intent(&earlier).map_err(|error| error.to_string())?;
                }
                intent
            }
        };
        let marketplace_root = root.to_string_lossy().into_owned();
        let install = NativeInstall {
            selector: &selector,
            marketplace_root: &marketplace_root,
            generation: &desired.generation,
            upstream_revision: desired.upstream_revision.as_deref(),
            capabilities: &desired.capabilities,
        };
        // Every failure after the new intent is recorded goes through
        // `restore`, so the earlier intent is put back before the error leaves.
        let replaced = if keeps_earlier {
            None
        } else {
            pending.as_ref()
        };
        // Once a registration change has succeeded, the native state may be
        // the attempt's generation, so the attempt intent is the accurate
        // record and the earlier intent is not restored over it.
        let tracked = MutationTracker::new(runner);
        let restore = |error: String| {
            if tracked.registration_changed() {
                error
            } else {
                restore_earlier_intent(error, replaced, &recorded)
            }
        };
        let installed = match prior {
            Some(prior) => update_native_registration(&tracked, executable, prior, &install),
            None => install_native_registration(&tracked, executable, &install),
        };
        match installed {
            Ok(()) => {}
            Err(
                error @ NativeReconcileError::Conflict(
                    NativeObservation::External | NativeObservation::Disabled,
                ),
            ) => {
                // A present registration is adopted only when the earlier
                // intent proves it with fresh view evidence. Anything else
                // keeps the conflict and the intent.
                let proven = adopt_present_registration(
                    &tracked,
                    executable,
                    &selector,
                    pending.as_ref(),
                    target,
                    &desired,
                    &view.path,
                )
                .map_err(&restore)?;
                if !proven {
                    return Err(restore(format!(
                        "native install failed, intent kept: {error}"
                    )));
                }
            }
            Err(error) => {
                return Err(restore(format!(
                    "native install failed, intent kept: {error}"
                )));
            }
        }
        // The new registration has verified, so each legacy registration is
        // removed with fresh proof before the new ownership is committed.
        retire_legacy_registrations(&tracked, executable, &legacy).map_err(&restore)?;
        commit_native_ownership(desired.clone()).map_err(|error| error.to_string())?;
        retire_copies(&copies, lockfile)?;
        clear_native_intent(&recorded).map_err(|error| error.to_string())?;
        remove_saved_preimages(&recorded.preimage_refs);
    }
    prune_stale_views(&root, &view_id);
    // The shared copy is gone, so each replaced reader now writes to its own
    // private root, and its route is verified against that root.
    for (index, private) in replacements {
        plan.routes[index].choice = ComponentRouteChoice::Direct {
            root: private.to_string(),
        };
    }
    for &index in members {
        plan.routes[index].native = NativeRouteStatus::Native;
    }
    Ok(format!(
        "{target} plugin {plugin}: native activation {}",
        if current { "unchanged" } else { "verified" }
    ))
}

/// Committed registrations for the same target and management ID that run under
/// a selector other than the canonical `{plugin}@ccync-{plugin}`. The management
/// ID is the provider source key. A same-name registration with no committed
/// record is never returned, so a generic package name alone selects nothing.
fn legacy_selector_rows(
    target: ProjectionTargetId,
    source: &ProviderSourceKey,
    plugin: &str,
) -> Vec<CommittedOwnership> {
    let canonical = format!("{plugin}@ccync-{plugin}");
    committed_rows()
        .into_iter()
        .filter(|row| {
            row.target == target
                && row.generation.source == *source
                && row.selector.as_str() != canonical
        })
        .collect()
}

/// Prove that a committed legacy registration is ccync's own before it is
/// migrated. The record must be pinned to the same upstream revision as the
/// group, and it must name the selector its own package name yields. Its
/// registration must be enabled and owned by a fresh observation of its
/// committed view digest. An absent registration needs no removal.
fn prove_legacy_registration(
    runner: &dyn NativeCommandRunner,
    executable: NativeExecutable,
    row: &CommittedOwnership,
    generation: &RenderedProviderGeneration,
) -> Result<(), String> {
    let selector = row.selector.as_str();
    let pinned =
        row.upstream_revision.is_some() && row.upstream_revision == generation.upstream_revision;
    if !pinned {
        return Err(format!(
            "legacy selector {selector} is not pinned to the group's revision"
        ));
    }
    if selector != format!("{}@ccync-{}", row.plugin_name, row.plugin_name) {
        return Err(format!(
            "legacy selector {selector} does not match its package name"
        ));
    }
    if row.validate().is_err()
        || row.client_scope.is_none()
        || row.source_digest.is_none()
        || row.view_digest.is_none()
    {
        return Err(format!(
            "legacy selector {selector} has no complete ownership proof"
        ));
    }
    let observed = runner.observe(executable, &row.selector)?;
    if !observed.available {
        return Err(format!("legacy selector {selector} cannot be observed"));
    }
    if !observed.present {
        return Ok(());
    }
    // Native-reported fields are cross-checks: a field the CLI omits is no
    // evidence either way, and a field that disagrees means drift.
    let native_agrees = observed
        .upstream_revision
        .as_deref()
        .is_none_or(|revision| Some(revision) == row.upstream_revision.as_deref())
        && observed
            .package_digest
            .as_deref()
            .is_none_or(|digest| digest == row.generation.package_digest.as_str())
        && observed
            .source
            .as_deref()
            .is_none_or(|source| source == row.generation.source.as_str());
    if observed.broken || observed.selector.as_ref() != Some(&row.selector) || !native_agrees {
        return Err(format!(
            "legacy selector {selector} is not provably owned: registration drifted"
        ));
    }
    if !observed.enabled {
        return Err(format!(
            "legacy selector {selector} is disabled, so its registration is kept"
        ));
    }
    // The installed view must hash to the digest the ownership record commits.
    let evidence = match committed_view_path(row) {
        Some(view) => observe_view_digest(&view),
        None => NativePathEvidence::Missing,
    };
    match (evidence, row.view_digest.as_ref()) {
        (NativePathEvidence::Digest(digest), Some(expected)) if digest == *expected => Ok(()),
        (evidence, _) => Err(format!(
            "legacy selector {selector} is not provably owned: view is {evidence:?}"
        )),
    }
}

/// Remove each legacy registration with fresh proof, then drop its committed
/// record. A registration already gone only loses its record. The caller has
/// verified the new registration and holds the intent that names these.
fn retire_legacy_registrations(
    runner: &dyn NativeCommandRunner,
    executable: NativeExecutable,
    legacy: &[CommittedOwnership],
) -> Result<(), String> {
    for row in legacy {
        remove_committed_registration(runner, executable, row).map_err(|error| {
            format!(
                "legacy selector {} removal failed, intent kept: {error}",
                row.selector
            )
        })?;
        drop_committed_ownership(row).map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Forwards every call to the wrapped runner and records whether a
/// registration-changing CLI call returned success. Marketplace calls and
/// failed calls leave the record unchanged.
struct MutationTracker<'a> {
    inner: &'a dyn NativeCommandRunner,
    changed: std::cell::Cell<bool>,
}

impl<'a> MutationTracker<'a> {
    fn new(inner: &'a dyn NativeCommandRunner) -> Self {
        Self {
            inner,
            changed: std::cell::Cell::new(false),
        }
    }

    /// Whether a successful registration change ran through this tracker.
    fn registration_changed(&self) -> bool {
        self.changed.get()
    }
}

impl NativeCommandRunner for MutationTracker<'_> {
    fn mutation_verified(&self, executable: NativeExecutable) -> bool {
        self.inner.mutation_verified(executable)
    }

    fn mutation_block_reason(&self, executable: NativeExecutable) -> String {
        self.inner.mutation_block_reason(executable)
    }

    fn run(&self, executable: NativeExecutable, args: &[&str]) -> Result<Vec<u8>, String> {
        let output = self.inner.run(executable, args)?;
        if matches!(
            args,
            [
                "plugin",
                "install" | "add" | "remove" | "uninstall" | "enable" | "disable",
                ..
            ]
        ) {
            self.changed.set(true);
        }
        Ok(output)
    }

    fn observe(
        &self,
        executable: NativeExecutable,
        selector: &ccync_foundation::provider::NativeSelector,
    ) -> Result<crate::plugin_native::NativeRegistration, String> {
        self.inner.observe(executable, selector)
    }

    fn marketplace_root(
        &self,
        executable: NativeExecutable,
        marketplace: &str,
    ) -> Result<Option<String>, String> {
        self.inner.marketplace_root(executable, marketplace)
    }
}

/// Whether the earlier pending intent proves the registration that a native
/// install reported as already present. The proof is a fresh observation of
/// the selector, classified against the intent, plus the digest of the view
/// directory on disk. No intent, or any other classification, proves nothing.
fn adopt_present_registration(
    runner: &dyn NativeCommandRunner,
    executable: NativeExecutable,
    selector: &NativeSelector,
    pending: Option<&PendingIntent>,
    target: ProjectionTargetId,
    desired: &CommittedOwnership,
    view: &Path,
) -> Result<bool, String> {
    let Some(pending) = pending else {
        return Ok(false);
    };
    let fresh = runner.observe(executable, selector)?;
    let state = fresh.classify_adopted_view(
        target,
        pending,
        &desired.generation,
        desired.upstream_revision.as_deref(),
        &desired.capabilities,
        &observe_view_digest(view),
    );
    Ok(state == NativeObservation::Desired)
}

/// Copies that keep the readers of a group's shared copies covered, each with
/// the root it is written to. The group publishes and verifies them before
/// any native change or retirement.
fn replacement_targets(plan: &DeliveryPlan, members: &[usize]) -> Vec<(usize, &'static str)> {
    let retires = |route: &ComponentTargetRoute| route.native == NativeRouteStatus::Native;
    let mut found = BTreeSet::new();
    for &member in members {
        let route = &plan.routes[member];
        let (ComponentRouteChoice::Direct { root } | ComponentRouteChoice::ServedByRoot { root }) =
            &route.choice
        else {
            continue;
        };
        let Some(contents) = plan
            .shared_roots
            .iter()
            .find(|contents| contents.root == *root)
        else {
            continue;
        };
        for &consumer in &contents.reached_consumers {
            if consumer == route.target {
                continue;
            }
            if let Some(ReaderCoverage::Copy { route: index, root }) =
                reader_coverage(plan, consumer, &route.identity, root, &retires)
            {
                found.insert((index, root));
            }
        }
    }
    found.into_iter().collect()
}

/// Publish each covering copy and verify that its content matches the
/// generation's skill source. A copy the registry does not own, one that is
/// not written, or one whose content differs fails the group before any
/// native change or retirement.
fn publish_replacements(
    plan: &DeliveryPlan,
    replacements: &[(usize, &'static str)],
    generation: &RenderedProviderGeneration,
    home: &Path,
    lockfile: &Path,
) -> Result<(), String> {
    if replacements.is_empty() {
        return Ok(());
    }
    let mut assignments = Vec::new();
    for &(index, root) in replacements {
        let route = &plan.routes[index];
        let source_path =
            skill_source_dir(generation, &route.identity.component_id).ok_or_else(|| {
                format!(
                    "skill {} has no source in the rendered generation",
                    route.identity.component_id
                )
            })?;
        assignments.push(projection::DirectSkillAssignment {
            package: projection::DirectSkillPackage {
                source_id: route.identity.source.as_str().to_string(),
                original_name: route.identity.component_id.clone(),
                alias: route_name(route).to_string(),
                source_path,
            },
            root: projection::DirectSkillRoot {
                path: physical_root(home, root),
                targets: vec![route.target],
            },
        });
    }
    let publication = projection::DirectSkillPublication {
        assignments,
        served_targets: Vec::new(),
    };
    let evidence = projection::publish_direct_skill_plan(&publication, lockfile, false);
    for (target, entry) in &evidence.by_target {
        if let Some(failure) = &entry.failure {
            return Err(format!(
                "{target} replacement not published: {}",
                failure.reason
            ));
        }
        if let Some(reason) = &entry.skipped {
            return Err(format!("{target} replacement not published: {reason}"));
        }
    }
    for assignment in &publication.assignments {
        let copy = assignment.root.path.join(&assignment.package.alias);
        if !copy.join("SKILL.md").is_file() {
            return Err(format!("replacement {} is missing", copy.display()));
        }
        let expected = shared_path_fingerprint(&assignment.package.source_path);
        if expected.is_none() || shared_path_fingerprint(&copy) != expected {
            return Err(format!(
                "replacement {} differs from its source",
                copy.display()
            ));
        }
    }
    Ok(())
}

/// One loose copy a native group replaces, with its proven retirement.
struct LooseCopy {
    retirement: SharedPathRetirement,
}

/// Find each loose copy a group's routes own and prove that it retires. A copy
/// with a reader that is not replaced, or a copy the lockfile does not own,
/// blocks the group before any native change.
fn loose_copies(
    plan: &DeliveryPlan,
    members: &[usize],
    home: &Path,
    lockfile: &Path,
) -> Result<Vec<LooseCopy>, String> {
    let retires = |route: &ComponentTargetRoute| route.native == NativeRouteStatus::Native;
    let mut copies = Vec::new();
    for &index in members {
        let route = &plan.routes[index];
        let Some(destination) = route_destination(route, home) else {
            continue;
        };
        let Some(present) = copy_candidates(&destination)
            .into_iter()
            .find(|candidate| fs::symlink_metadata(candidate).is_ok())
        else {
            continue;
        };
        let root = match &route.choice {
            ComponentRouteChoice::Direct { root } | ComponentRouteChoice::ServedByRoot { root } => {
                root.clone()
            }
            _ => continue,
        };
        let mut readers: Vec<SharedPathReader> = plan
            .shared_roots
            .iter()
            .find(|contents| contents.root == root)
            .map(|contents| {
                contents
                    .reached_consumers
                    .iter()
                    .map(|&consumer| SharedPathReader {
                        group: consumer.to_string(),
                        state: if consumer == route.target
                            || reader_coverage(plan, consumer, &route.identity, &root, &retires)
                                .is_some()
                        {
                            SharedPathReaderState::ReplacementVerified
                        } else {
                            SharedPathReaderState::Unselected
                        },
                    })
                    .collect()
            })
            .unwrap_or_default();
        if readers.is_empty() {
            readers.push(SharedPathReader {
                group: route.target.to_string(),
                state: SharedPathReaderState::ReplacementVerified,
            });
        }
        let owned_fingerprint = shared_path_fingerprint(&present)
            .ok_or_else(|| format!("{} is unreadable", present.display()))?;
        let retirement = SharedPathRetirement {
            path: present,
            owned_fingerprint,
            readers,
        };
        match retire_shared_path(&retirement, lockfile, true) {
            Ok(SharedPathRetirementOutcome::Removed) => copies.push(LooseCopy { retirement }),
            Ok(SharedPathRetirementOutcome::Kept { reason }) => {
                return Err(format!("loose copy kept: {reason}"));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(copies)
}

/// Remove the proven loose copies. A retirement that does not remove its copy
/// is an error, so the caller keeps its intent.
fn retire_copies(copies: &[LooseCopy], lockfile: &Path) -> Result<(), String> {
    for copy in copies {
        match retire_shared_path(&copy.retirement, lockfile, false) {
            Ok(SharedPathRetirementOutcome::Removed) => {}
            Ok(SharedPathRetirementOutcome::Kept { reason }) => {
                return Err(format!("loose copy kept: {reason}"));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

/// Save the prior content of each loose copy under the build root and return
/// one reference per copy. A directory is saved as a tree, a file as a file,
/// so the saved set is enough to put the prior route back.
fn save_preimages(
    copies: &[LooseCopy],
    home: &Path,
    group: &str,
) -> Result<Vec<ccync_foundation::provider::PreimageRef>, String> {
    use ccync_foundation::provider::{PathRoot, PreimageRef, RelativePath, ScopedPath};
    if copies.is_empty() {
        return Ok(Vec::new());
    }
    let build =
        crate::install::provider_state_base().ok_or("the provider state root is unavailable")?;
    let group_dir = build.join("preimages").join(group);
    fs::create_dir_all(&group_dir).map_err(|error| error.to_string())?;
    let mut refs = Vec::with_capacity(copies.len());
    for (index, copy) in copies.iter().enumerate() {
        let live = &copy.retirement.path;
        let relative = live
            .strip_prefix(home)
            .map_err(|_| format!("{} is outside the home directory", live.display()))?;
        let saved_rel = format!("preimages/{group}/{index}");
        let saved = group_dir.join(index.to_string());
        let _ = fs::remove_dir_all(&saved);
        let _ = fs::remove_file(&saved);
        let metadata = fs::symlink_metadata(live).map_err(|error| error.to_string())?;
        if metadata.is_dir() {
            copy_tree(live, &saved).map_err(|error| error.to_string())?;
        } else {
            fs::copy(live, &saved).map_err(|error| error.to_string())?;
        }
        refs.push(PreimageRef {
            path: ScopedPath {
                root: PathRoot::UserHome,
                path: RelativePath::new(relative.to_string_lossy())
                    .map_err(|error| error.to_string())?,
            },
            saved: RelativePath::new(&saved_rel).map_err(|error| error.to_string())?,
        });
    }
    Ok(refs)
}

/// Whether two intents describe the same operation. They do when they differ
/// at most in their preimage references and phase: the attempt then only adds
/// the saved preimages the earlier intent lacked.
fn same_operation(earlier: &PendingIntent, attempt: &PendingIntent) -> bool {
    let mut normalized = earlier.clone();
    normalized.preimage_refs = attempt.preimage_refs.clone();
    normalized.phase = attempt.phase;
    normalized == *attempt
}

/// Settle the journal after an install attempt that recorded `attempt` over an
/// earlier pending intent has failed. A same-operation attempt supersedes the
/// earlier intent and is kept, since it is the earlier evidence plus its saved
/// preimages. An attempt that describes a different operation is ambiguous
/// evidence, so the earlier intent is written back and the attempt's preimages
/// are dropped, except those the restored intent still names. When that
/// write-back fails, the returned error carries both the original error and
/// the write-back failure, and the attempt's preimages stay on disk because
/// the journal still names them.
fn restore_earlier_intent(
    error: String,
    earlier: Option<&PendingIntent>,
    attempt: &PendingIntent,
) -> String {
    let Some(earlier) = earlier else {
        return error;
    };
    if same_operation(earlier, attempt) {
        return error;
    }
    // The attempt has another fingerprint when it names a newer generation, so
    // recording the earlier intent alone would leave the attempt in the journal.
    match record_native_intent(earlier.clone()).and_then(|()| clear_native_intent(attempt)) {
        Ok(()) => {
            let unnamed: Vec<_> = attempt
                .preimage_refs
                .iter()
                .filter(|preimage| !earlier.preimage_refs.contains(preimage))
                .cloned()
                .collect();
            remove_saved_preimages(&unnamed);
            error
        }
        Err(restore_error) => {
            format!("{error}; restoring the earlier pending intent also failed: {restore_error}")
        }
    }
}

/// Delete the saved preimages once the switch has completed and the intent is
/// cleared. Best effort: a leftover copy is never read again.
fn remove_saved_preimages(refs: &[ccync_foundation::provider::PreimageRef]) {
    let Some(build) = crate::install::provider_state_base() else {
        return;
    };
    for preimage in refs {
        let saved = build.join(preimage.saved.as_str());
        let _ = fs::remove_dir_all(&saved);
        let _ = fs::remove_file(&saved);
    }
}

fn package_target(target: ProjectionTargetId) -> Option<NativePackageTarget> {
    match target {
        ProjectionTargetId::Claude => Some(NativePackageTarget::Claude),
        ProjectionTargetId::Codex => Some(NativePackageTarget::Codex),
        _ => None,
    }
}

/// Stable marketplace root for one native target and plugin.
fn native_marketplace_root(target: ProjectionTargetId, plugin: &str) -> Result<PathBuf, String> {
    let providers = ccync_foundation::paths::provider_render_root()
        .ok_or("the provider render root is unavailable")?;
    Ok(providers
        .join(NATIVE_MARKETPLACE_DIR)
        .join(target.as_str())
        .join(plugin))
}

/// Copy the content-addressed view into the marketplace root. A view already
/// present under the same id has the same content, so it is reused.
fn stage_view(source: &Path, root: &Path, view_id: &str) -> Result<(), String> {
    fs::create_dir_all(root).map_err(|error| error.to_string())?;
    let dest = root.join(view_id);
    if dest.exists() {
        return Ok(());
    }
    let staging = root.join(format!(".staging-{view_id}"));
    let _ = fs::remove_dir_all(&staging);
    copy_tree(source, &staging).map_err(|error| error.to_string())?;
    fs::rename(&staging, &dest).map_err(|error| error.to_string())
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(std::io::Error::other(
                "a native view must not contain links",
            ));
        }
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Write the marketplace document that lists the plugin's view. The file is
/// rewritten only when its bytes change.
fn write_marketplace(
    root: &Path,
    target: NativePackageTarget,
    plugin: &str,
    view_id: &str,
) -> Result<(), String> {
    let marketplace = format!("ccync-{plugin}");
    let source = format!("./{view_id}");
    let (file, document) = match target {
        NativePackageTarget::Claude => (
            ".claude-plugin/marketplace.json",
            json!({
                "name": marketplace,
                "owner": {"name": "ccync"},
                "plugins": [{"name": plugin, "source": source}],
            }),
        ),
        NativePackageTarget::Codex => (
            ".agents/plugins/marketplace.json",
            json!({
                "name": marketplace,
                "plugins": [{
                    "name": plugin,
                    "source": {"source": "local", "path": source},
                    "policy": {"installation": "AVAILABLE", "authentication": "ON_INSTALL"},
                    "category": "Productivity",
                }],
            }),
        ),
    };
    let bytes = serde_json::to_vec_pretty(&document).map_err(|error| error.to_string())?;
    let path = root.join(file);
    if fs::read(&path).is_ok_and(|current| current == bytes) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    ccync_foundation::platform::atomic_write_bytes(&path, &bytes).map_err(|e| e.to_string())
}

/// Remove superseded view directories under a marketplace root. Only
/// directories named with the view prefix are removed.
fn prune_stale_views(root: &Path, keep: &str) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(NATIVE_VIEW_PREFIX) && name != keep {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Committed native ownership rows from the build lock.
fn committed_rows() -> Vec<CommittedOwnership> {
    let Some(path) = crate::install::provider_state_base().map(|root| root.join("lock.json"))
    else {
        return Vec::new();
    };
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(lock) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    ProviderLifecycleLock::from_lock(&lock)
        .map(|section| section.committed)
        .unwrap_or_default()
}

#[cfg(test)]
mod mutation_tracker_tests {
    use super::*;
    use crate::plugin_native::NativeRegistration;

    /// A runner whose CLI calls all succeed without touching any client.
    struct SucceedingRunner;

    impl NativeCommandRunner for SucceedingRunner {
        fn run(&self, _executable: NativeExecutable, _args: &[&str]) -> Result<Vec<u8>, String> {
            Ok(Vec::new())
        }

        fn observe(
            &self,
            _executable: NativeExecutable,
            _selector: &NativeSelector,
        ) -> Result<NativeRegistration, String> {
            Err("observation is not used by this test".to_string())
        }

        fn marketplace_root(
            &self,
            _executable: NativeExecutable,
            _marketplace: &str,
        ) -> Result<Option<String>, String> {
            Ok(None)
        }
    }

    #[test]
    fn p7_t13_mutation_tracker_counts_claude_uninstall_as_registration_change() {
        let inner = SucceedingRunner;
        let tracker = MutationTracker::new(&inner);
        tracker
            .run(
                NativeExecutable::Claude,
                &["plugin", "uninstall", "probe@ccync"],
            )
            .expect("uninstall succeeds");
        assert!(tracker.registration_changed());
    }

    #[test]
    fn p7_t13_mutation_tracker_ignores_marketplace_calls() {
        let inner = SucceedingRunner;
        let tracker = MutationTracker::new(&inner);
        tracker
            .run(
                NativeExecutable::Claude,
                &["plugin", "marketplace", "add", "dir"],
            )
            .expect("marketplace call succeeds");
        assert!(!tracker.registration_changed());
    }
}

/// One physical standalone root is published once for all its selected readers.
pub(crate) struct StandaloneRun {
    pub evidence: projection::ProjectionEvidence,
    pub routes: Vec<RouteEvidence>,
    pub scope: projection::MachineProjectionScope,
    pub failed: BTreeSet<ProjectionTargetId>,
}

fn standalone_path_key(path: &Path) -> String {
    #[cfg(windows)]
    {
        path.to_string_lossy().replace('/', "\\").to_lowercase()
    }
    #[cfg(not(windows))]
    {
        projection::normalize_managed_artifact_path(path)
    }
}

fn standalone_roots(
    selected: &[ProjectionTargetId],
) -> BTreeMap<&'static str, Vec<ProjectionTargetId>> {
    let mut roots = BTreeMap::new();
    for target in selected {
        let root = match target {
            ProjectionTargetId::Claude => ".claude/skills",
            ProjectionTargetId::Codex
            | ProjectionTargetId::Copilot
            | ProjectionTargetId::Opencode => ".agents/skills",
            ProjectionTargetId::AgyCli => ".gemini/antigravity-cli/skills",
            ProjectionTargetId::AgyIde | ProjectionTargetId::AgyGui => ".gemini/config/skills",
        };
        roots.entry(root).or_insert_with(Vec::new).push(*target);
    }
    roots
}

fn standalone_reserve(
    run: &mut StandaloneRun,
    destination: &Path,
    source: &str,
    targets: &[ProjectionTargetId],
    blocked: bool,
) {
    for target in targets {
        run.scope
            .decisions
            .push(projection::DirectDelegationDecision {
                producer: source.to_string(),
                component: projection::DirectDelegationComponent::Skill,
                target: *target,
                destination: destination.to_path_buf(),
                legacy_writer: SKILL_LEGACY_WRITER.into(),
                outcome: if blocked {
                    projection::DirectDelegationOutcome::Blocked
                } else {
                    projection::DirectDelegationOutcome::Delegated
                },
            });
    }
}

fn standalone_report(
    run: &mut StandaloneRun,
    destination: &Path,
    name: &str,
    source: &crate::install::StandaloneSkillSource,
    targets: &[ProjectionTargetId],
    result: Result<(), String>,
) {
    let published = result.is_ok();
    let reason = result
        .err()
        .map(|reason| format!("identity unresolved: {reason}"));
    for target in targets {
        if let Some(reason) = &reason {
            run.failed.insert(*target);
            run.evidence
                .run_warnings
                .push(format!("{target} skill {name}: {reason}"));
            let entry = run.evidence.by_target.entry(*target).or_default();
            entry.skipped = None;
            entry.failure = Some(projection::ProjectionExpectedFailure {
                path: Some(destination.to_path_buf()),
                reason: reason.clone(),
            });
        }
        run.routes.push(RouteEvidence {
            target: *target,
            kind: ComponentKind::Skill,
            original_name: name.to_string(),
            source_key: source.source_id.clone(),
            origin_path: source.path.as_ref().map(|path| path.display().to_string()),
            origin_reference: source.revision.as_ref().ok().cloned(),
            command_to_skill: false,
            failure_reason: reason.clone(),
            unsupported_reason: None,
            alias: None,
            path: Some(destination.display().to_string()),
            served_by_root: None,
            multi_root: false,
            foreign_alias_path: None,
            selected: true,
            published,
            owned: published,
            loader: VerificationScope::LoaderNotRun,
            loader_version: None,
            registration: None,
        });
    }
    standalone_reserve(run, destination, &source.source_id, targets, !published);
}

/// Attribute a legacy preimage only when its exact registry row and physical
/// shape prove that it is the old managed route for this winning source.
fn standalone_prior_owner(
    lockfile: &Path,
    destination: &Path,
    source: &Path,
    canonical_root: &Path,
    legacy: &BTreeMap<String, LegacyReferent>,
) -> Result<(Option<String>, Option<String>), String> {
    let raw: Value = serde_json::from_slice(&fs::read(lockfile).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let key = standalone_path_key(destination);
    let section = &raw["_ccyncProjection"];
    let matches_key = |candidate: &str| {
        #[cfg(windows)]
        {
            candidate == key
        }
        #[cfg(not(windows))]
        {
            projection::decode_managed_artifact_key(candidate).is_some_and(|decoded| decoded == key)
        }
    };
    let owner = section["sourceAttribution"]
        .as_object()
        .and_then(|owners| owners.iter().find(|(path, _)| matches_key(path)))
        .and_then(|(_, owner)| owner.as_str());
    let categories = [
        "skillProjectionPaths",
        "directSkillProjectionPaths",
        "sourceLinkProjectionPaths",
    ];
    let recorded = categories.iter().any(|category| {
        section[*category].as_array().is_some_and(|paths| {
            paths
                .iter()
                .any(|path| path.as_str().is_some_and(&matches_key))
        })
    });
    if !recorded || owner.is_none() {
        return Ok((None, None));
    }
    let same_source = if ccync_foundation::platform::is_symlink_or_junction(destination) {
        let expected = canonical_root
            .join("skills")
            .join(destination.file_name().ok_or("skill name absent")?);
        let same = match (fs::canonicalize(destination), fs::canonicalize(expected)) {
            (Ok(actual), Ok(expected)) => actual == expected,
            _ => false,
        };
        // The referent identity must be the one captured before this render.
        let proven = legacy.get(&key).is_some_and(|before| {
            fs::read_link(destination).is_ok_and(|raw| raw == before.raw_target)
                && fs::canonicalize(destination)
                    .ok()
                    .and_then(|referent| projection::standalone_source_fingerprint(&referent).ok())
                    .as_deref()
                    == Some(before.fingerprint.as_str())
        });
        if same && !proven {
            return Err(
                "legacy link referent changed during this render; no preimage was captured".into(),
            );
        }
        same
    } else if destination.is_dir() {
        projection::standalone_source_fingerprint(destination).ok()
            == Some(projection::standalone_source_fingerprint(source).map_err(|e| e.to_string())?)
    } else {
        false
    };
    if !same_source {
        return Err("legacy registry row does not prove this source preimage".into());
    }
    Ok((
        owner.map(str::to_string),
        projection::shared_path_fingerprint(destination),
    ))
}

fn finish_standalone(
    request: projection::StandalonePublicationRequest,
    pending: Option<&ccync_foundation::provider::StandalonePublicationIntent>,
) -> Result<(), String> {
    let stage = request.build_root.join(request.stage_path.as_str());
    let destination = request.destination.path.join_to(&request.destination_root);
    let backup = request.build_root.join(request.backup_path.as_str());
    let prepared = match pending {
        Some(intent) => projection::resume_standalone_publication(request, intent),
        None => projection::prepare_standalone_publication(request),
    }
    .map_err(|e| format!("recovery blocked: {e}"))?;
    let mut intent = pending
        .cloned()
        .unwrap_or_else(|| prepared.intent().clone());
    crate::install::record_standalone_intent(intent.clone()).map_err(|e| e.to_string())?;
    // A published tree with no stage resumes at publication's idempotent check.
    let switched = !stage.exists()
        && projection::standalone_source_fingerprint(&destination)
            .is_ok_and(|fingerprint| fingerprint == intent.expected_tree_fingerprint.as_str())
        && (matches!(
            intent.preimage,
            ccync_foundation::provider::StandalonePreimage::Absent
        ) || backup.symlink_metadata().is_ok());
    if !switched && !stage.exists() {
        projection::stage_standalone_publication(&prepared, &intent).map_err(|e| e.to_string())?;
    }
    intent.phase = IntentPhase::Publishing;
    crate::install::record_standalone_intent(intent.clone()).map_err(|e| e.to_string())?;
    projection::publish_standalone_publication(&prepared, &intent).map_err(|e| e.to_string())?;
    intent.phase = IntentPhase::Verifying;
    crate::install::record_standalone_intent(intent.clone()).map_err(|e| e.to_string())?;
    projection::complete_standalone_publication(&prepared, &intent).map_err(|e| e.to_string())?;
    intent.phase = IntentPhase::Pruning;
    crate::install::record_standalone_intent(intent.clone()).map_err(|e| e.to_string())?;
    crate::install::clear_standalone_intent(&intent).map_err(|e| e.to_string())?;
    projection::cleanup_standalone_publication(&prepared, &intent).map_err(|e| e.to_string())?;
    // Only an emptied operation directory is removed, never retained evidence.
    if let Some(operation) = backup.parent() {
        let _ = fs::remove_dir(operation);
    }
    Ok(())
}

/// Fresh desired-state evidence for one standalone pass.
pub(crate) struct StandaloneEvidence {
    pub selected: Vec<ProjectionTargetId>,
    /// Source identities that remain desired. `None` means the set is unproven,
    /// so no missing source can count as intentional removal.
    pub desired_origins: Option<Vec<String>>,
    /// Targets whose native activation failed in this pass.
    pub native_failed: BTreeSet<ProjectionTargetId>,
    /// Legacy links into the canonical root, captured before this pass rendered.
    pub legacy_referents: BTreeMap<String, LegacyReferent>,
}

/// A legacy link and the referent identity it had before the canonical render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LegacyReferent {
    raw_target: PathBuf,
    fingerprint: String,
}

/// Record every standalone-root link that resolves into the canonical render.
/// This must run before the render replaces the referents, so a later intent
/// is never captured from a preimage the render already changed.
pub(crate) fn capture_legacy_referents(
    home: &Path,
    canonical_root: &Path,
) -> BTreeMap<String, LegacyReferent> {
    let mut captured = BTreeMap::new();
    for root in standalone_roots(CANONICAL_PROJECTION_TARGETS).keys() {
        let Ok(entries) = fs::read_dir(home.join(root)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !ccync_foundation::platform::is_symlink_or_junction(&path) {
                continue;
            }
            let expected = canonical_root.join("skills").join(entry.file_name());
            let (Ok(actual), Ok(expected)) = (fs::canonicalize(&path), fs::canonicalize(expected))
            else {
                continue;
            };
            if let (true, Ok(raw_target), Some(fingerprint)) = (
                actual == expected,
                fs::read_link(&path),
                projection::standalone_source_fingerprint(&actual).ok(),
            ) {
                captured.insert(
                    standalone_path_key(&path),
                    LegacyReferent {
                        raw_target,
                        fingerprint,
                    },
                );
            }
        }
    }
    captured
}

/// Everything a withdrawal needs besides the committed row itself.
struct WithdrawalContext<'a> {
    evidence: &'a StandaloneEvidence,
    plan: Option<&'a DeliveryPlan>,
    home: &'a Path,
    build: PathBuf,
    lockfile: &'a Path,
    discovery: Vec<PathBuf>,
}

/// How a journaled withdrawal ended after this pass looked at it.
enum WithdrawalResume {
    /// Forward-completed: the path is absent and the intent is cleared.
    Completed,
    /// Nothing had moved and the evidence changed, so the intent was dropped.
    Abandoned,
}

/// True only when a proven desired set no longer names the committed source.
fn source_intentionally_removed(evidence: &StandaloneEvidence, source_id: &str) -> bool {
    let Some(desired) = &evidence.desired_origins else {
        return false;
    };
    if source_id.starts_with("plugin:") {
        !desired
            .iter()
            .any(|prefix| prefix.starts_with("plugin:") && source_id.starts_with(prefix.as_str()))
    } else if source_id.starts_with("loose-origin:") {
        !desired.iter().any(|origin| origin == source_id)
    } else {
        false
    }
}

/// A route that moved to a native registration which this pass activated.
fn native_replacement(
    context: &WithdrawalContext<'_>,
    target: ProjectionTargetId,
    name: &str,
) -> bool {
    !context.evidence.native_failed.contains(&target)
        && context.plan.is_some_and(|plan| {
            plan.routes.iter().any(|route| {
                route.target == target
                    && route.identity.kind == ComponentKind::Skill
                    && route.identity.component_id == name
                    && route.native == crate::delivery::NativeRouteStatus::Native
                    && route.publication_failure.is_none()
            })
        })
}

/// Every known client that reads the physical root holding `path`, whether or
/// not it was ever selected.
fn root_consumers(home: &Path, path: &Path) -> Vec<String> {
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    let parent = standalone_path_key(parent);
    CANONICAL_PROJECTION_TARGETS
        .iter()
        .filter(|target| {
            standalone_roots(&[**target])
                .keys()
                .any(|root| standalone_path_key(&home.join(root)) == parent)
        })
        .map(ToString::to_string)
        .collect()
}

/// One proof per reader of the physical root: the committed readers plus every
/// other known consumer of that root. A reader that is unselected, or whose
/// source still stands without a verified replacement, withholds withdrawal
/// authority.
fn withdrawal_readers(
    context: &WithdrawalContext<'_>,
    name: &str,
    record: &projection::StandaloneProjectionRecord,
) -> Result<Vec<ccync_foundation::provider::StandaloneWithdrawalReaderProof>, String> {
    use ccync_foundation::provider::{
        StandaloneWithdrawalReaderIntent, StandaloneWithdrawalReaderProof,
    };
    let source_id = record.source_id.as_str();
    let removed = source_intentionally_removed(context.evidence, source_id);
    let inventory = record
        .targets
        .iter()
        .cloned()
        .chain(root_consumers(context.home, &record.path))
        .collect::<BTreeSet<_>>();
    inventory
        .iter()
        .map(|text| {
            let target = text
                .parse::<ProjectionTargetId>()
                .map_err(|_| format!("reader {text} is unknown"))?;
            if !context.evidence.selected.contains(&target) {
                return Err(format!(
                    "reader {text} is not selected and may still depend on the shared path"
                ));
            }
            let (intent, evidence) = if removed {
                (
                    StandaloneWithdrawalReaderIntent::Disabled,
                    format!("source-removed:{source_id}"),
                )
            } else if native_replacement(context, target, name) {
                (
                    StandaloneWithdrawalReaderIntent::ReplacementVerified,
                    format!("native-route:{text}:{name}"),
                )
            } else {
                return Err(format!(
                    "reader {text} has no removal intent or verified replacement"
                ));
            };
            Ok(StandaloneWithdrawalReaderProof {
                reader: text.clone(),
                intent,
                evidence,
            })
        })
        .collect()
}

fn withdrawal_request(
    context: &WithdrawalContext<'_>,
    operation_id: String,
    record: &projection::StandaloneProjectionRecord,
    readers: Vec<ccync_foundation::provider::StandaloneWithdrawalReaderProof>,
    reserved: &[PathBuf],
) -> Result<projection::StandaloneWithdrawalRequest, String> {
    use ccync_foundation::provider::{PathRoot, RelativePath, ScopedPath};
    let relative = record
        .path
        .strip_prefix(context.home)
        .map_err(|_| "committed destination is outside the home".to_string())?;
    let relative = relative.to_string_lossy().replace('\\', "/");
    Ok(projection::StandaloneWithdrawalRequest {
        backup_path: RelativePath::new(format!("standalone-withdrawals/{operation_id}/backup"))
            .map_err(|e| e.to_string())?,
        operation_id,
        committed_record: record.clone(),
        readers,
        destination: ScopedPath {
            root: PathRoot::UserHome,
            path: RelativePath::new(relative).map_err(|e| e.to_string())?,
        },
        destination_root: context.home.to_path_buf(),
        build_root: context.build.clone(),
        discovery_roots: context.discovery.clone(),
        reserved_paths: reserved
            .iter()
            .filter(|path| standalone_path_key(path) != standalone_path_key(&record.path))
            .cloned()
            .collect(),
    })
}

fn reserved_destinations(run: &StandaloneRun) -> Vec<PathBuf> {
    run.scope
        .decisions
        .iter()
        .map(|decision| decision.destination.clone())
        .collect()
}

/// Durable intent, rename to private backup, registry removal, targeted clear,
/// then backup cleanup. Every later boundary only moves forward.
fn finish_withdrawal(
    build: &Path,
    prepared: projection::PreparedStandaloneWithdrawal,
    pending: Option<ccync_foundation::provider::StandaloneWithdrawalIntent>,
) -> Result<(), String> {
    let mut intent = pending.unwrap_or_else(|| prepared.intent().clone());
    let rank = |phase: IntentPhase| match phase {
        IntentPhase::Staged => 0,
        IntentPhase::Publishing => 1,
        IntentPhase::Verifying => 2,
        IntentPhase::Pruning | IntentPhase::Reloading => 3,
    };
    // A resumed intent never moves back to an earlier phase.
    let persist = |intent: &mut ccync_foundation::provider::StandaloneWithdrawalIntent,
                   phase: IntentPhase| {
        if rank(phase) > rank(intent.phase) {
            intent.phase = phase;
        }
        crate::install::record_withdrawal_intent(intent.clone()).map_err(|e| e.to_string())
    };
    let first = intent.phase;
    persist(&mut intent, first)?;
    persist(&mut intent, IntentPhase::Publishing)?;
    projection::apply_standalone_withdrawal(&prepared, &intent).map_err(|e| e.to_string())?;
    persist(&mut intent, IntentPhase::Verifying)?;
    projection::complete_standalone_withdrawal(&prepared, &intent).map_err(|e| e.to_string())?;
    persist(&mut intent, IntentPhase::Pruning)?;
    crate::install::clear_withdrawal_intent(&intent).map_err(|e| e.to_string())?;
    projection::cleanup_standalone_withdrawal(&prepared, &intent).map_err(|e| e.to_string())?;
    // Only an emptied operation directory is removed, never retained evidence.
    if let Some(operation) = intent.backup_path.join_to(build).parent() {
        let _ = fs::remove_dir(operation);
    }
    Ok(())
}

/// Withdraw one committed standalone path with fresh reader evidence.
fn withdraw_committed(
    context: &WithdrawalContext<'_>,
    record: &projection::StandaloneProjectionRecord,
    reserved: &[PathBuf],
) -> Result<(), String> {
    let name = record
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("committed destination has no skill name")?;
    let readers = withdrawal_readers(context, name, record)?;
    let request = withdrawal_request(
        context,
        format!("withdrawal-{}", uuid::Uuid::new_v4().simple()),
        record,
        readers,
        reserved,
    )?;
    let prepared = projection::prepare_standalone_withdrawal(request).map_err(|e| e.to_string())?;
    finish_withdrawal(&context.build, prepared, None)
}

/// Resume one journaled withdrawal. Before the registry row is removed the
/// reader evidence is derived again. After removal only the stored evidence is
/// used, because the commit already happened and recovery moves forward.
fn resume_withdrawal(
    context: &WithdrawalContext<'_>,
    intent: &ccync_foundation::provider::StandaloneWithdrawalIntent,
    reserved: &[PathBuf],
) -> Result<WithdrawalResume, String> {
    resume_withdrawal_with_abandon(context, intent, reserved, true)
}

fn resume_withdrawal_with_abandon(
    context: &WithdrawalContext<'_>,
    intent: &ccync_foundation::provider::StandaloneWithdrawalIntent,
    reserved: &[PathBuf],
    allow_abandon: bool,
) -> Result<WithdrawalResume, String> {
    let record: projection::StandaloneProjectionRecord =
        serde_json::from_value(intent.registry_before["standalone"].clone())
            .map_err(|e| format!("recovery blocked: {e}"))?;
    let name = record
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("recovery blocked: committed destination has no skill name")?;
    let committed_removed =
        projection::standalone_publication_records(context.lockfile).is_ok_and(|records| {
            !records
                .iter()
                .any(|row| standalone_path_key(&row.path) == standalone_path_key(&record.path))
        });
    let fresh = if committed_removed {
        Ok(intent.readers.clone())
    } else {
        withdrawal_readers(context, name, &record)
    };
    let readers = match fresh {
        Ok(readers) if readers == intent.readers => readers,
        other => {
            // Nothing has moved yet when the destination is present and the
            // backup is absent, so dropping the intent changes no state.
            let backup = intent.backup_path.join_to(&context.build);
            if record.path.symlink_metadata().is_ok() && backup.symlink_metadata().is_err() {
                if !allow_abandon {
                    return Err("recovery blocked: reader evidence changed".into());
                }
                crate::install::clear_withdrawal_intent(intent).map_err(|e| e.to_string())?;
                return Ok(WithdrawalResume::Abandoned);
            }
            return Err(match other {
                Err(reason) => format!("recovery blocked: {reason}"),
                Ok(_) => {
                    "recovery blocked: reader evidence changed after the preimage moved".to_string()
                }
            });
        }
    };
    let request = withdrawal_request(
        context,
        intent.operation_id.clone(),
        &record,
        readers,
        reserved,
    )?;
    let prepared = projection::resume_standalone_withdrawal(request, intent)
        .map_err(|e| format!("recovery blocked: {e}"))?;
    finish_withdrawal(&context.build, prepared, Some(intent.clone()))?;
    Ok(WithdrawalResume::Completed)
}

/// Recover journaled standalone work without selecting sources or planning routes.
/// A publication is replayable only when its stage or destination proves the
/// recorded source tree. A withdrawal is replayable only when its saved reader
/// proofs all show removal.
pub(crate) fn recover_standalone_intents(
    layout: &ccync_foundation::paths::EffectiveLayout,
    _guard: &crate::tool_manager::lock::LifecycleGuard,
) -> Result<(), String> {
    use ccync_foundation::provider::StandaloneWithdrawalReaderIntent;

    let journal = crate::install::read_standalone_journal().map_err(|error| error.to_string())?;
    if journal.standalone_intents.is_empty() && journal.standalone_withdrawal_intents.is_empty() {
        return Ok(());
    }
    let home = crate::paths::user_home().ok_or("user home is unavailable")?;
    let build = layout.state_base();
    let mut publications = Vec::new();
    let mut recovery_sources = Vec::new();
    let mut publication_targets = BTreeSet::new();
    for intent in &journal.standalone_intents {
        let source: Value = serde_json::from_str(&intent.source)
            .map_err(|error| format!("standalone publication recovery blocked: {error}"))?;
        let source_id = source
            .get("origin")
            .and_then(Value::as_str)
            .ok_or("standalone publication recovery blocked: source origin is missing")?;
        let source_revision = source
            .get("revision")
            .and_then(Value::as_str)
            .ok_or("standalone publication recovery blocked: source revision is missing")?;
        let destination_root =
            intent.destination.root.resolve().ok_or(
                "standalone publication recovery blocked: destination root is unavailable",
            )?;
        let destination = intent.destination.path.join_to(&destination_root);
        let stage = intent.stage_path.join_to(&build);
        let source_path = [stage.clone(), destination.clone()]
            .into_iter()
            .find(|path| {
                projection::standalone_source_fingerprint(path).is_ok_and(|fingerprint| {
                    fingerprint == intent.expected_tree_fingerprint.as_str()
                })
            })
            .ok_or_else(|| {
                format!(
                    "standalone publication recovery blocked: saved source tree evidence is missing for {}",
                    destination.display()
                )
            })?;
        let source_path = if standalone_path_key(&source_path) == standalone_path_key(&stage) {
            let scratch = RecoverySourceScratch::create()?;
            copy_recovery_source_tree(&stage, scratch.path())?;
            let copied_fingerprint = projection::standalone_source_fingerprint(scratch.path())
                .map_err(|error| format!("standalone publication recovery blocked: {error}"))?;
            if copied_fingerprint != intent.expected_tree_fingerprint.as_str() {
                return Err(format!(
                    "standalone publication recovery blocked: copied source proof differs for {}",
                    destination.display()
                ));
            }
            let copied_path = scratch.path().to_path_buf();
            recovery_sources.push(scratch);
            copied_path
        } else {
            source_path
        };
        let targets = intent
            .targets
            .iter()
            .map(|target| {
                target.parse::<ProjectionTargetId>().map_err(|_| {
                    format!("standalone publication recovery blocked: unknown target {target}")
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        publication_targets.extend(targets);
        let operation_id = intent.operation_id.clone();
        let mut reserved_paths = Vec::new();
        for pending in &journal.standalone_intents {
            let root =
                pending.destination.root.resolve().ok_or(
                    "standalone publication recovery blocked: reserved root is unavailable",
                )?;
            let reserved = pending.destination.path.join_to(&root);
            if standalone_path_key(&reserved) != standalone_path_key(&destination) {
                reserved_paths.push(reserved);
            }
        }
        for pending in &journal.standalone_withdrawal_intents {
            let root =
                pending.destination.root.resolve().ok_or(
                    "standalone publication recovery blocked: reserved root is unavailable",
                )?;
            let reserved = pending.destination.path.join_to(&root);
            if standalone_path_key(&reserved) != standalone_path_key(&destination) {
                reserved_paths.push(reserved);
            }
        }
        let request = projection::StandalonePublicationRequest {
            operation_id,
            source_path,
            source_id: source_id.to_string(),
            source_revision: source_revision.to_string(),
            destination: intent.destination.clone(),
            destination_root,
            build_root: build.clone(),
            stage_path: intent.stage_path.clone(),
            backup_path: intent.backup_path.clone(),
            discovery_roots: Vec::new(),
            targets: intent.targets.clone(),
            reserved_paths,
            prior_owner: intent
                .registry_before
                .as_ref()
                .and_then(|before| before.get("source"))
                .and_then(Value::as_str)
                .map(str::to_string),
            owned_fingerprint: intent
                .preimage_fingerprint
                .as_ref()
                .map(|fingerprint| fingerprint.as_str().to_string()),
            manifest_proofs: Vec::new(),
        };
        publications.push((request, intent.clone()));
    }
    let mut selected = BTreeSet::new();
    for intent in &journal.standalone_withdrawal_intents {
        for reader in &intent.readers {
            if reader.intent != StandaloneWithdrawalReaderIntent::Disabled {
                return Err(format!(
                    "standalone withdrawal recovery blocked: reader {} requires route planning evidence",
                    reader.reader
                ));
            }
            selected.insert(reader.reader.parse::<ProjectionTargetId>().map_err(|_| {
                format!(
                    "standalone withdrawal recovery blocked: unknown reader {}",
                    reader.reader
                )
            })?);
        }
    }
    selected.extend(publication_targets);
    let selected = selected.into_iter().collect::<Vec<_>>();
    let evidence = StandaloneEvidence {
        selected: selected.clone(),
        desired_origins: Some(Vec::new()),
        native_failed: BTreeSet::new(),
        legacy_referents: BTreeMap::new(),
    };
    let discovery = standalone_roots(&selected)
        .keys()
        .map(|root| home.join(root))
        .filter(|root| root.is_dir())
        .collect();
    let context = WithdrawalContext {
        evidence: &evidence,
        plan: None,
        home: &home,
        build,
        lockfile: &layout.build_lock_path(),
        discovery,
    };
    for (mut request, intent) in publications {
        request.discovery_roots = context.discovery.clone();
        finish_standalone(request, Some(&intent))?;
    }
    for intent in &journal.standalone_withdrawal_intents {
        let reserved = Vec::new();
        resume_withdrawal_with_abandon(&context, intent, &reserved, false)?;
    }
    drop(recovery_sources);
    Ok(())
}

struct RecoverySourceScratch {
    path: PathBuf,
}

impl RecoverySourceScratch {
    fn create() -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!(
            "ccync-standalone-recovery-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir(&path).map_err(|error| {
            format!("standalone publication recovery scratch unavailable: {error}")
        })?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for RecoverySourceScratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn copy_recovery_source_tree(source: &Path, destination: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(source).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(
            "standalone publication recovery blocked: stage root is not a directory".into(),
        );
    }
    for entry in fs::read_dir(source).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&from).map_err(|error| error.to_string())?;
        if metadata.file_type().is_symlink() {
            return Err(
                "standalone publication recovery blocked: stage contains a symbolic link".into(),
            );
        }
        if metadata.is_dir() {
            fs::create_dir(&to).map_err(|error| error.to_string())?;
            copy_recovery_source_tree(&from, &to)?;
            fs::set_permissions(&to, metadata.permissions()).map_err(|error| error.to_string())?;
        } else if metadata.is_file() {
            fs::copy(&from, &to).map_err(|error| error.to_string())?;
        } else {
            return Err(
                "standalone publication recovery blocked: stage contains a special file".into(),
            );
        }
    }
    fs::set_permissions(destination, metadata.permissions()).map_err(|error| error.to_string())?;
    Ok(())
}

/// Engine-owned lifecycle coordination. Projection validates every phase and
/// the actual durable intent, including registry ancestor reservations.
pub(crate) fn publish_standalone_sources(
    _held: &crate::tool_manager::lock::HeldLifecycleLock,
    sources: &BTreeMap<String, crate::install::StandaloneSkillSource>,
    plan: Option<&DeliveryPlan>,
    evidence: &StandaloneEvidence,
    home: &Path,
    canonical_root: &Path,
    lockfile: &Path,
) -> StandaloneRun {
    use ccync_foundation::provider::{PathRoot, RelativePath, ScopedPath};
    let mut run = StandaloneRun {
        evidence: Default::default(),
        routes: Vec::new(),
        scope: delegation_scope(plan, Some(home)),
        failed: BTreeSet::new(),
    };
    let selected = evidence.selected.as_slice();
    let Some(build) = crate::install::provider_state_base() else {
        run.failed.extend(selected.iter().copied());
        run.evidence
            .run_warnings
            .push("provider state layout is unavailable".into());
        return run;
    };
    let roots = standalone_roots(selected);
    let discovery_roots = roots.keys().map(|root| home.join(root)).collect::<Vec<_>>();
    let roots_ready = discovery_roots
        .iter()
        .try_for_each(|root| -> Result<(), String> {
            if root
                .ancestors()
                .any(ccync_foundation::platform::is_symlink_or_junction)
            {
                return Err("linked discovery-root ancestor".into());
            }
            fs::create_dir_all(root).map_err(|error| error.to_string())
        });
    // Pending withdrawals resume first, so the registry and journal read below
    // already reflect any forward completion.
    let context = WithdrawalContext {
        evidence,
        plan,
        home,
        build: build.clone(),
        lockfile,
        discovery: standalone_roots(CANONICAL_PROJECTION_TARGETS)
            .keys()
            .map(|root| home.join(root))
            .filter(|root| root.is_dir())
            .collect(),
    };
    let mut withdrawal_blocked = BTreeSet::new();
    let mut completed_withdrawals = Vec::new();
    if let Ok(pending) = crate::install::read_standalone_journal() {
        for intent in &pending.standalone_withdrawal_intents {
            let Some(root) = intent.destination.root.resolve() else {
                continue;
            };
            let destination = intent.destination.path.join_to(&root);
            let reserved = reserved_destinations(&run);
            let targets = intent
                .readers
                .iter()
                .filter_map(|reader| reader.reader.parse().ok())
                .collect::<Vec<_>>();
            let name = destination
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("unknown")
                .to_string();
            match resume_withdrawal(&context, intent, &reserved) {
                // Reserved after the publication loop, so a source that is
                // desired again can publish in this same pass.
                Ok(WithdrawalResume::Completed) => {
                    completed_withdrawals.push((destination, intent.source_id.clone(), targets))
                }
                Ok(WithdrawalResume::Abandoned) => {}
                Err(reason) => {
                    withdrawal_blocked.insert(standalone_path_key(&destination));
                    let source = crate::install::StandaloneSkillSource {
                        path: None,
                        source_id: intent.source_id.clone(),
                        revision: Err(reason.clone()),
                    };
                    standalone_report(
                        &mut run,
                        &destination,
                        &name,
                        &source,
                        &targets,
                        Err(reason),
                    );
                }
            }
        }
    }
    let journal = crate::install::read_standalone_journal().map_err(|e| e.to_string());
    let committed = projection::standalone_publication_records(lockfile).map_err(|e| e.to_string());
    let mut processed = withdrawal_blocked.clone();
    for (name, source) in sources {
        for (root, targets) in &roots {
            let destination = home.join(root).join(name);
            processed.insert(standalone_path_key(&destination));
            // Existing routes own every outcome. In particular a native failure
            // or disabled component never returns authority to the legacy writer.
            let delegated = plan.is_none()
                || plan.is_some_and(|plan| {
                    plan.routes.iter().any(|route| {
                        route.identity.kind == ComponentKind::Skill
                            && route.identity.component_id == *name
                    })
                });
            if withdrawal_blocked.contains(&standalone_path_key(&destination)) {
                continue;
            }
            if delegated {
                // A route that moved to verified native delivery may retire the
                // committed standalone path. Anything else stays reserved.
                let record = committed.as_ref().ok().and_then(|records| {
                    records.iter().find(|record| {
                        standalone_path_key(&record.path) == standalone_path_key(&destination)
                    })
                });
                let withdrawn = record.is_some_and(|record| {
                    let reserved = reserved_destinations(&run);
                    withdraw_committed(&context, record, &reserved).is_ok()
                });
                standalone_reserve(
                    &mut run,
                    &destination,
                    &source.source_id,
                    targets,
                    !withdrawn,
                );
                continue;
            }
            // A skill whose recorded origin is the destination itself is already
            // served by its owner. Nothing is published, owned or failed.
            if let (Some(origin), Ok(physical)) = (&source.path, fs::canonicalize(&destination)) {
                if fs::canonicalize(origin).is_ok_and(|origin| origin == physical) {
                    // Keep the committed reader inventory current even though
                    // this source already is the destination and needs no copy.
                    // The caller holds the lifecycle lock for this reconciliation.
                    let reconciled = (|| -> Result<(), String> {
                        let mut records = projection::standalone_publication_records(lockfile)
                            .map_err(|error| error.to_string())?;
                        if let Some(record) = records.iter_mut().find(|record| {
                            standalone_path_key(&record.path) == standalone_path_key(&destination)
                        }) {
                            let current = projection::standalone_source_fingerprint(&destination)
                                .map_err(|error| error.to_string())?;
                            if current == record.tree_fingerprint {
                                let mut readers =
                                    record.targets.iter().cloned().collect::<BTreeSet<_>>();
                                readers.extend(targets.iter().map(ToString::to_string));
                                if readers.len() != record.targets.len() {
                                    let mut lock: Value = serde_json::from_slice(
                                        &fs::read(lockfile).map_err(|error| error.to_string())?,
                                    )
                                    .map_err(|error| error.to_string())?;
                                    let key = standalone_path_key(&record.path);
                                    let proof = lock["_ccyncProjection"]["standalonePublications"]
                                        .get_mut(&key)
                                        .ok_or("committed standalone proof disappeared")?;
                                    proof["targets"] = serde_json::to_value(readers)
                                        .map_err(|error| error.to_string())?;
                                    ccync_foundation::platform::atomic_write_bytes(
                                        lockfile,
                                        serde_json::to_string_pretty(&lock)
                                            .map_err(|error| error.to_string())?
                                            .as_bytes(),
                                    )
                                    .map_err(|error| error.to_string())?;
                                }
                            } else {
                                return Err(
                                    "committed standalone tree drift during reader reconciliation"
                                        .into(),
                                );
                            }
                        }
                        Ok(())
                    })();
                    if let Err(reason) = reconciled {
                        let source = crate::install::StandaloneSkillSource {
                            path: Some(origin.clone()),
                            source_id: source.source_id.clone(),
                            revision: Err(reason.clone()),
                        };
                        standalone_report(
                            &mut run,
                            &destination,
                            name,
                            &source,
                            targets,
                            Err(reason),
                        );
                        continue;
                    }
                    standalone_reserve(&mut run, &destination, &source.source_id, targets, false);
                    continue;
                }
            }
            let result = (|| -> Result<(), String> {
                roots_ready.as_ref().map_err(Clone::clone)?;
                let journal = journal.as_ref().map_err(Clone::clone)?;
                let committed = committed.as_ref().map_err(Clone::clone)?;
                let source_path = source.path.as_ref().ok_or_else(|| {
                    source
                        .revision
                        .as_ref()
                        .err()
                        .cloned()
                        .unwrap_or("source origin unavailable".into())
                })?;
                let revision = source.revision.as_ref().map_err(Clone::clone)?;
                let pending = journal
                    .standalone_intents
                    .iter()
                    .filter(|intent| {
                        intent.destination.root == PathRoot::UserHome
                            && standalone_path_key(&home.join(intent.destination.path.as_str()))
                                == standalone_path_key(&destination)
                    })
                    .collect::<Vec<_>>();
                if pending.len() > 1 {
                    return Err(
                        "recovery blocked: multiple pending intents reserve destination".into(),
                    );
                }
                let pending = pending.first().copied();
                let record = committed.iter().find(|record| {
                    standalone_path_key(&record.path) == standalone_path_key(&destination)
                });
                let (prior_owner, owned_fingerprint) = if let Some(intent) = pending {
                    (
                        intent
                            .registry_before
                            .as_ref()
                            .and_then(|row| row["source"].as_str())
                            .map(str::to_string),
                        intent
                            .preimage_fingerprint
                            .as_ref()
                            .map(|digest| digest.as_str().to_string()),
                    )
                } else if let Some(record) = record {
                    if projection::standalone_source_fingerprint(&destination)
                        .map_err(|e| e.to_string())?
                        != record.tree_fingerprint
                    {
                        return Err("recovery blocked: committed standalone tree drift".into());
                    }
                    if record.source_id == source.source_id
                        && record.source_revision == *revision
                        && targets.iter().all(|target| {
                            record
                                .targets
                                .iter()
                                .any(|prior| prior == &target.to_string())
                        })
                        && projection::standalone_source_fingerprint(source_path)
                            .map_err(|e| e.to_string())?
                            == record.source_fingerprint
                    {
                        return Ok(());
                    }
                    (
                        Some(record.source_id.clone()),
                        Some(record.tree_fingerprint.clone()),
                    )
                } else {
                    standalone_prior_owner(
                        lockfile,
                        &destination,
                        source_path,
                        canonical_root,
                        &evidence.legacy_referents,
                    )?
                };
                let operation_id = pending
                    .map(|intent| intent.operation_id.clone())
                    .unwrap_or_else(|| format!("standalone-{}", uuid::Uuid::new_v4().simple()));
                let mut committed_targets = record
                    .map(|record| record.targets.iter().cloned().collect::<BTreeSet<_>>())
                    .unwrap_or_default();
                committed_targets.extend(targets.iter().map(ToString::to_string));
                let request = projection::StandalonePublicationRequest {
                    operation_id: operation_id.clone(),
                    source_path: source_path.clone(),
                    source_id: source.source_id.clone(),
                    source_revision: revision.clone(),
                    destination: ScopedPath {
                        root: PathRoot::UserHome,
                        path: RelativePath::new(format!("{root}/{name}"))
                            .map_err(|e| e.to_string())?,
                    },
                    destination_root: home.to_path_buf(),
                    build_root: build.clone(),
                    stage_path: RelativePath::new(format!(
                        "standalone-publications/{operation_id}/stage"
                    ))
                    .map_err(|e| e.to_string())?,
                    backup_path: RelativePath::new(format!(
                        "standalone-publications/{operation_id}/backup"
                    ))
                    .map_err(|e| e.to_string())?,
                    discovery_roots: discovery_roots.clone(),
                    targets: committed_targets.into_iter().collect(),
                    reserved_paths: run
                        .scope
                        .decisions
                        .iter()
                        .map(|decision| decision.destination.clone())
                        .collect(),
                    prior_owner,
                    owned_fingerprint,
                    manifest_proofs: Vec::new(),
                };
                finish_standalone(request, pending)
            })();
            standalone_report(&mut run, &destination, name, source, targets, result);
        }
    }
    for (destination, source_id, targets) in &completed_withdrawals {
        if !processed.contains(&standalone_path_key(destination)) {
            standalone_reserve(&mut run, destination, source_id, targets, false);
        }
    }
    // Missing sources and changed selection do not erase interrupted ownership.
    // Reserve the original paths and report retained state without calling it delivered.
    if let Ok(journal) = &journal {
        for intent in &journal.standalone_intents {
            let Some(root) = intent.destination.root.resolve() else {
                continue;
            };
            let destination = intent.destination.path.join_to(&root);
            if processed.contains(&standalone_path_key(&destination)) {
                continue;
            }
            let targets = intent
                .targets
                .iter()
                .filter_map(|target| target.parse().ok())
                .collect::<Vec<_>>();
            let proof: Value = serde_json::from_str(&intent.source).unwrap_or(Value::Null);
            let source = crate::install::StandaloneSkillSource {
                path: None,
                source_id: proof["origin"]
                    .as_str()
                    .unwrap_or("unresolved-origin")
                    .into(),
                revision: Err(
                    "recovery blocked: winning source or original route is unavailable".into(),
                ),
            };
            let name = destination
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("unknown");
            standalone_report(&mut run, &destination, name, &source, &targets,
                Err("recovery blocked: winning source or original route is unavailable; pending evidence retained".into()));
        }
    }
    if let Ok(records) = &committed {
        for record in records {
            if processed.contains(&standalone_path_key(&record.path)) {
                continue;
            }
            let targets = record
                .targets
                .iter()
                .filter_map(|target| target.parse().ok())
                .collect::<Vec<_>>();
            let name = record
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("unknown");
            // A missing source or an unselected reader alone never authorizes
            // removal. Only complete fresh evidence reaches the withdrawal.
            let reserved = reserved_destinations(&run);
            match withdraw_committed(&context, record, &reserved) {
                Ok(()) => {
                    standalone_reserve(&mut run, &record.path, &record.source_id, &targets, false)
                }
                Err(reason) => {
                    let reason = format!("withdrawal blocked: {reason}; committed proof retained");
                    let source = crate::install::StandaloneSkillSource {
                        path: None,
                        source_id: record.source_id.clone(),
                        revision: Err(reason.clone()),
                    };
                    standalone_report(&mut run, &record.path, name, &source, &targets, Err(reason));
                }
            }
        }
    }
    run
}


#[cfg(test)]
mod projection_key_regression_tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn standalone_path_key_preserves_windows_golden_values() {
        for (input, expected) in [
            (r"C:/Users/ALICE/Skill", r"c:\users\alice\skill"),
            (r"\\SERVER/Share/Item", r"\\server\share\item"),
            (r"\\?\C:/Folder/Item", r"\\?\c:\folder\item"),
            (r"C:/Ä/İ", "c:\\ä\\i\u{307}"),
        ] {
            assert_eq!(standalone_path_key(Path::new(input)), expected);
        }
    }

    #[test]
    fn standalone_prior_owner_matches_existing_lock_row() {
        use sha2::{Digest, Sha256};
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("destination");
        let source = root.path().join("source");
        let user = root.path().join("user.txt");
        fs::create_dir_all(&destination).unwrap();
        fs::create_dir_all(&source).unwrap();
        fs::write(destination.join("SKILL.md"), "managed").unwrap();
        fs::write(source.join("SKILL.md"), "managed").unwrap();
        fs::write(&user, "user content").unwrap();
        let before = Sha256::digest(fs::read(&user).unwrap());
        let lock = root.path().join("lock.json");
        let key = destination.to_string_lossy().replace('/', "\\");
        #[cfg(windows)]
        let key = key.to_lowercase();
        fs::write(&lock, serde_json::json!({"_ccyncProjection": {
            "skillProjectionPaths": [key], "sourceAttribution": {key: "plugin:example"}
        }}).to_string()).unwrap();
        let (owner, fingerprint) = standalone_prior_owner(&lock, &destination, &source, root.path(), &BTreeMap::new()).unwrap();
        assert_eq!(owner.as_deref(), Some("plugin:example"));
        assert_eq!(fingerprint, projection::shared_path_fingerprint(&destination));
        assert_eq!(before, Sha256::digest(fs::read(&user).unwrap()));
    }
}
