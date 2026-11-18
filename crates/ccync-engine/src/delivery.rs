//! Pure delivery planning for provider components.
//!
//! This module decides where a component should be published. It does not
//! inspect or modify the filesystem.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use ccync_foundation::provider::{
    ClientFaceSupportDisposition, ComponentIdentity, ComponentKind, TargetAlias,
};
use ccync_foundation::target::{ClientFace, ProjectionTargetId};
use projection::ProviderProjectionInventory;

use crate::plugin_render::RenderedProviderGeneration;

/// Official consumer documentation and check date for each shared root.
///
/// Copilot CLI is reached by `~/.agents/skills`; Copilot is deliberately not
/// served by that root because its VS Code bridge disables that location.
pub const SHARED_ROOT_CONSUMERS: &[SharedRootConsumers] = &[
    SharedRootConsumers {
        root: "~/.agents/skills",
        component_kinds: &[ComponentKind::Skill, ComponentKind::Command],
        scope: "user",
        served_faces: &[
            ClientFace::Codex,
            ClientFace::CopilotCli,
            ClientFace::OpenCode,
        ],
        incidental_readers: &[],
        face_support: &[
            (ClientFace::Codex, ClientFaceSupportDisposition::Supported),
            (
                ClientFace::Claude,
                ClientFaceSupportDisposition::Unsupported,
            ),
            (
                ClientFace::CopilotCli,
                ClientFaceSupportDisposition::Supported,
            ),
            (
                ClientFace::CopilotVsCode,
                ClientFaceSupportDisposition::Unsupported,
            ),
            (
                ClientFace::OpenCode,
                ClientFaceSupportDisposition::Supported,
            ),
            (
                ClientFace::AgyCli,
                ClientFaceSupportDisposition::Unsupported,
            ),
            (
                ClientFace::AgyIde,
                ClientFaceSupportDisposition::Unsupported,
            ),
            (
                ClientFace::AgyGui,
                ClientFaceSupportDisposition::Unsupported,
            ),
        ],
        consumers: &[
            ProjectionTargetId::Codex,
            ProjectionTargetId::Copilot,
            ProjectionTargetId::Opencode,
        ],
        served_by: &[ProjectionTargetId::Codex, ProjectionTargetId::Opencode],
        sources: &[
            "https://learn.chatgpt.com/docs/build-skills",
            "https://docs.github.com/en/copilot/concepts/agents/about-agent-skills",
            "https://opencode.ai/docs/skills/",
        ],
        checked: "2026-10-16",
    },
    SharedRootConsumers {
        root: "~/.claude/skills",
        component_kinds: &[ComponentKind::Skill, ComponentKind::Command],
        scope: "user",
        served_faces: &[ClientFace::Claude, ClientFace::OpenCode],
        incidental_readers: &[],
        face_support: &[
            (ClientFace::Claude, ClientFaceSupportDisposition::Supported),
            (
                ClientFace::OpenCode,
                ClientFaceSupportDisposition::Supported,
            ),
        ],
        consumers: &[ProjectionTargetId::Claude, ProjectionTargetId::Opencode],
        served_by: &[ProjectionTargetId::Claude, ProjectionTargetId::Opencode],
        sources: &[
            "https://docs.anthropic.com/en/docs/claude-code/skills",
            "https://opencode.ai/docs/skills/",
        ],
        checked: "2026-10-16",
    },
    SharedRootConsumers {
        root: "~/.gemini/config/skills",
        component_kinds: &[ComponentKind::Skill, ComponentKind::Command],
        scope: "user",
        served_faces: &[ClientFace::AgyIde, ClientFace::AgyGui],
        incidental_readers: &[],
        face_support: &[
            (ClientFace::AgyIde, ClientFaceSupportDisposition::Supported),
            (ClientFace::AgyGui, ClientFaceSupportDisposition::Supported),
        ],
        consumers: &[ProjectionTargetId::AgyIde, ProjectionTargetId::AgyGui],
        served_by: &[ProjectionTargetId::AgyIde, ProjectionTargetId::AgyGui],
        sources: &["https://antigravity.google/docs/skills"],
        checked: "2026-10-16",
    },
    SharedRootConsumers {
        root: "~/.gemini/config/agents",
        component_kinds: &[ComponentKind::Agent],
        scope: "user",
        served_faces: &[ClientFace::AgyCli, ClientFace::AgyGui],
        incidental_readers: &[],
        face_support: &[
            (ClientFace::AgyCli, ClientFaceSupportDisposition::Supported),
            (ClientFace::AgyGui, ClientFaceSupportDisposition::Supported),
        ],
        consumers: &[ProjectionTargetId::AgyCli, ProjectionTargetId::AgyGui],
        served_by: &[ProjectionTargetId::AgyCli, ProjectionTargetId::AgyGui],
        sources: &["https://antigravity.google/docs/cli/commands/agents/"],
        checked: "2026-10-16",
    },
];

/// Consumer policy for one physical shared root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SharedRootConsumers {
    /// Home-relative physical root.
    pub root: &'static str,
    /// Component kinds this root can represent.
    pub component_kinds: &'static [ComponentKind],
    /// Configuration scope for this discovery surface.
    pub scope: &'static str,
    /// Client faces with confirmed support through this root.
    pub served_faces: &'static [ClientFace],
    /// Faces that may read this root without being served by it.
    pub incidental_readers: &'static [ClientFace],
    /// Support disposition for each client face declared by this row.
    pub face_support: &'static [(ClientFace, ClientFaceSupportDisposition)],
    /// Targets that discover this root, whether selected or not.
    pub consumers: &'static [ProjectionTargetId],
    /// Targets for which this root counts as serving the component.
    pub served_by: &'static [ProjectionTargetId],
    /// Source URLs checked for this row.
    pub sources: &'static [&'static str],
    /// Date the source row was checked.
    pub checked: &'static str,
}

/// One provider component and the routing facts needed by the planner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryComponent {
    /// Stable source and upstream component identity.
    pub identity: ComponentIdentity,
    /// Whether the provider declares hooks, reported on every target.
    pub provider_hooks_declared: bool,
    /// Disabled components receive no publication route.
    pub enabled: bool,
    /// False when target alias rewriting cannot preserve component references.
    pub references_preserved: bool,
}

/// Provider components plus any external target names that may cause aliases.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeliveryInventory {
    /// Components produced by provider generations.
    pub components: Vec<DeliveryComponent>,
    /// Existing foreign components that occupy native names.
    pub foreign_components: Vec<ForeignComponent>,
}

/// A foreign component that must not be overwritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignComponent {
    /// Identity used to make the existing alias planner account for the name.
    pub identity: ComponentIdentity,
    /// Existing path shown when the provider component is aliased.
    pub path: String,
}

impl DeliveryInventory {
    /// Build routing inventory from immutable provider generations.
    pub fn from_generations(generations: &[RenderedProviderGeneration]) -> Self {
        let mut components = Vec::new();
        for generation in generations {
            for identity in &generation.components {
                let enabled = generation
                    .component_resources
                    .iter()
                    .find(|resources| resources.component == *identity)
                    .map(|resources| resources.enabled)
                    .unwrap_or(true);
                components.push(DeliveryComponent {
                    identity: identity.clone(),
                    provider_hooks_declared: generation.hooks_declared,
                    enabled,
                    references_preserved: true,
                });
            }
        }
        Self {
            components,
            foreign_components: Vec::new(),
        }
    }
}

/// Selected destination for one component on one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentRouteChoice {
    /// Write directly to this private root.
    Direct { root: String },
    /// The target discovers the component through a shared root.
    ServedByRoot { root: String },
    /// The target has no safe native representation for this component.
    Unsupported { reason: String },
    /// Publishing would lose references or overwrite an ambiguous route unit.
    Blocked { reason: String },
    /// The component is disabled by its provider.
    Disabled,
}

/// Physical destination identity supplied to the native group planner.
///
/// Planner-only callers keep `Unchecked`, which preserves root-string
/// comparison. Publication records either a validated path or a rejection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhysicalIdentity {
    /// No filesystem-backed identity has been checked.
    Unchecked,
    /// Absolute destination identity validated during publication planning.
    Validated(PathBuf),
    /// The destination could not be safely resolved.
    Rejected,
}

/// Route decision for one component and one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentTargetRoute {
    /// Original identity, retained for later projection mapping.
    pub identity: ComponentIdentity,
    /// Validated physical destination identity, when supplied by the caller.
    pub physical_identity: PhysicalIdentity,
    /// Target receiving or discovering the component.
    pub target: ProjectionTargetId,
    /// The single selected route outcome.
    pub choice: ComponentRouteChoice,
    /// Alias chosen by the existing provider alias planner, if any.
    pub alias: Option<String>,
    /// Foreign path causing the alias, when applicable.
    pub foreign_alias_path: Option<String>,
    /// Number of written roots this target discovers this component in.
    pub discovered_root_count: usize,
    /// True when the target discovers more than one written root.
    pub multi_root: bool,
    /// Provider name when hooks are unsupported on this target.
    pub unsupported_hooks_provider: Option<String>,
    /// True when a command is routed into the target's skill root as its
    /// equivalent skill, because the target has no native command root.
    pub command_to_skill: bool,
    /// Why the planned write did not produce the route's copy. The planner
    /// leaves it empty; publication sets it per written path.
    pub publication_failure: Option<String>,
    /// Native activation status. The planner leaves it
    /// [`NativeRouteStatus::Loose`]; [`plan_native_routes`] sets it.
    pub native: NativeRouteStatus,
}

/// Contents and reached consumers for one written shared root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedRootContents {
    /// Physical root.
    pub root: String,
    /// Components planned into this root, each at most once.
    pub components: Vec<ComponentIdentity>,
    /// Consumers reached by the root, including unselected targets.
    pub reached_consumers: Vec<ProjectionTargetId>,
}

/// Complete pure-data route plan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeliveryPlan {
    /// Per-component, per-target route choice.
    pub routes: Vec<ComponentTargetRoute>,
    /// Shared root contents, computed before target served-ness.
    pub shared_roots: Vec<SharedRootContents>,
}

/// Final delivery decision for one enabled component on one client face.
///
/// Copilot produces separate CLI and VS Code results. Disabled components do
/// not appear in this list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalFaceRoute {
    /// Component whose route was finalized.
    pub identity: ComponentIdentity,
    /// Individual client face receiving or discovering the component.
    pub face: ClientFace,
    /// Final native or loose route status for this face.
    pub native: NativeRouteStatus,
    /// The component's target-level loose route choice.
    pub choice: ComponentRouteChoice,
    /// Validated physical destination identity when available.
    pub physical_identity: PhysicalIdentity,
}

/// Commands whose body cannot become an equivalent skill, each with the
/// reason. Such a command keeps its native command routes but gets no
/// command-to-skill route.
pub type UnconvertibleCommands = BTreeMap<ComponentIdentity, String>;

/// Compute shared-root contents first, then derive target served-ness.
///
/// Every command is treated as convertible to an equivalent skill. Use
/// [`plan_delivery_with_commands`] when some commands cannot convert.
pub fn plan_delivery(
    inventory: &DeliveryInventory,
    selected_targets: &[ProjectionTargetId],
    persisted_aliases: &[TargetAlias],
) -> DeliveryPlan {
    plan_delivery_with_commands(
        inventory,
        selected_targets,
        persisted_aliases,
        &UnconvertibleCommands::new(),
    )
}

/// Compute shared-root contents first, then derive target served-ness.
///
/// A command on a target without a native command root is routed into that
/// target's command-to-skill root as its equivalent skill, unless it is in
/// `unconvertible` or the target has no command-to-skill surface. Such a
/// command is written into its skill root like a skill: at most once per
/// physical root, alias-checked against same-name entries there, and counted
/// for served-by-root and multi-root discovery.
pub fn plan_delivery_with_commands(
    inventory: &DeliveryInventory,
    selected_targets: &[ProjectionTargetId],
    persisted_aliases: &[TargetAlias],
    unconvertible: &UnconvertibleCommands,
) -> DeliveryPlan {
    let provider_inventory = provider_inventory(inventory, persisted_aliases);

    // Aliases are target-specific. A failed alias plan blocks every enabled
    // component on that target because reference preservation is then unknown.
    let aliases_by_target = selected_targets
        .iter()
        .map(|target| {
            (
                *target,
                projection::plan_provider_aliases(*target, &provider_inventory),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let alias_of = |target: ProjectionTargetId, identity: &ComponentIdentity| {
        aliases_by_target
            .get(&target)
            .and_then(|result| result.as_ref().ok())
            .and_then(|aliases| {
                aliases
                    .iter()
                    .find(|entry| entry.identity == *identity)
                    .map(|entry| entry.alias.clone())
            })
    };

    // Phase one: determine all written roots and their contents. A command
    // converted into a skill root counts there like a skill, unless its
    // command-to-skill route is blocked and so never written.
    let mut root_content: BTreeMap<String, BTreeSet<ComponentIdentity>> = BTreeMap::new();
    for component in &inventory.components {
        if !component.enabled
            || component.identity.kind == ComponentKind::Hook
            || component.identity.kind == ComponentKind::Resource
        {
            continue;
        }
        for target in selected_targets {
            let root = if converts_to_skill(&component.identity, *target, unconvertible) {
                let writable = component.references_preserved
                    && aliases_by_target
                        .get(target)
                        .is_some_and(|result| result.is_ok())
                    && alias_of(*target, &component.identity)
                        .is_none_or(|alias| alias == component.identity.component_id);
                command_skill_root(*target).filter(|_| writable)
            } else {
                native_root(component.identity.kind, *target)
            };
            if let Some(root) = root {
                if is_shared_root(root) {
                    root_content
                        .entry(root.to_owned())
                        .or_default()
                        .insert(component.identity.clone());
                }
            }
        }
    }
    let shared_roots = root_content
        .iter()
        .map(|(root, identities)| {
            let row = SHARED_ROOT_CONSUMERS
                .iter()
                .find(|row| row.root == root)
                .expect("every shared root has a consumer row");
            SharedRootContents {
                root: root.clone(),
                components: identities.iter().cloned().collect(),
                reached_consumers: row.consumers.to_vec(),
            }
        })
        .collect::<Vec<_>>();

    // Phase two: only roots computed above can serve a target.
    let mut routes = Vec::new();
    for component in &inventory.components {
        for target in selected_targets {
            let alias_result = aliases_by_target
                .get(target)
                .expect("selected target has alias result");
            let alias = alias_of(*target, &component.identity);
            let foreign_alias_path = if alias
                .as_deref()
                .is_some_and(|name| name != component.identity.component_id)
            {
                inventory
                    .foreign_components
                    .iter()
                    .find(|foreign| {
                        foreign.identity.kind == component.identity.kind
                            && foreign.identity.component_id == component.identity.component_id
                    })
                    .map(|foreign| foreign.path.clone())
            } else {
                None
            };
            let roots = roots_discovered_by(*target, component.identity.kind);
            let shared_discovered = roots
                .iter()
                .filter(|root| {
                    root_content
                        .get(**root)
                        .is_some_and(|items| items.contains(&component.identity))
                })
                .count();
            let hooks = (component.provider_hooks_declared
                || component.identity.kind == ComponentKind::Hook)
                .then(|| component.identity.plugin_name.clone());
            let choice = if !component.enabled {
                ComponentRouteChoice::Disabled
            } else if !component.references_preserved {
                ComponentRouteChoice::Blocked {
                    reason: "component references cannot be preserved".into(),
                }
            } else if let Err(error) = alias_result {
                ComponentRouteChoice::Blocked {
                    reason: format!("provider aliases cannot be planned: {error}"),
                }
            } else if component.identity.kind == ComponentKind::Hook {
                ComponentRouteChoice::Unsupported {
                    reason: "hooks are unsupported on every target".into(),
                }
            } else if component.identity.kind == ComponentKind::Resource {
                ComponentRouteChoice::Unsupported {
                    reason: "resources require an owning component route".into(),
                }
            } else if component.identity.kind == ComponentKind::Command {
                match native_root(ComponentKind::Command, *target) {
                    // A native command root wins over a converted skill copy
                    // in a shared root; that copy counts as multi-root
                    // discovery instead.
                    Some(root) => ComponentRouteChoice::Direct {
                        root: root.to_owned(),
                    },
                    None => command_choice(
                        &component.identity,
                        *target,
                        alias.as_deref(),
                        &roots,
                        &root_content,
                        unconvertible,
                    ),
                }
            } else if let Some(root) = roots.iter().find(|root| {
                root_content
                    .get(**root)
                    .is_some_and(|items| items.contains(&component.identity))
                    && root_serves(*target, root)
            }) {
                ComponentRouteChoice::ServedByRoot {
                    root: (*root).to_string(),
                }
            } else if let Some(root) = native_root(component.identity.kind, *target) {
                if roots.iter().any(|root| {
                    root_content
                        .get(*root)
                        .is_some_and(|items| items.contains(&component.identity))
                        && !root_serves(*target, root)
                }) || is_route_supported(component.identity.kind, *target)
                {
                    ComponentRouteChoice::Direct {
                        root: root.to_owned(),
                    }
                } else {
                    ComponentRouteChoice::Unsupported {
                        reason: "no supported native surface for this component kind".into(),
                    }
                }
            } else {
                ComponentRouteChoice::Unsupported {
                    reason: "no supported native surface for this component kind".into(),
                }
            };
            let command_to_skill = converts_to_skill(&component.identity, *target, unconvertible)
                && matches!(
                    choice,
                    ComponentRouteChoice::Direct { .. }
                        | ComponentRouteChoice::ServedByRoot { .. }
                        | ComponentRouteChoice::Blocked { .. }
                );
            let private_root_added = match &choice {
                ComponentRouteChoice::Direct { root } => {
                    !is_shared_root(root)
                        || !root_content
                            .get(root)
                            .is_some_and(|items| items.contains(&component.identity))
                }
                _ => false,
            };
            let discovered = shared_discovered + usize::from(private_root_added);
            let multi_root = discovered > 1;
            routes.push(ComponentTargetRoute {
                identity: component.identity.clone(),
                physical_identity: PhysicalIdentity::Unchecked,
                target: *target,
                choice,
                alias,
                foreign_alias_path,
                discovered_root_count: discovered,
                multi_root,
                unsupported_hooks_provider: hooks,
                command_to_skill,
                publication_failure: None,
                native: NativeRouteStatus::Loose,
            });
        }
    }
    DeliveryPlan {
        routes,
        shared_roots,
    }
}

/// Native plugin adapter state for one target, supplied by the caller.
///
/// A target absent from [`NativeAdapters`] is [`NativeAdapterState::Loose`],
/// so a caller with no native routing information keeps the loose plan.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum NativeAdapterState {
    /// No native adapter is verified for this target.
    #[default]
    Loose,
    /// The target's native surface was observed. Its loose route stays.
    Observed,
    /// The verified adapter's registration and policy checks passed.
    Ready,
    /// The verified adapter's registration or policy check failed.
    Failed { reason: String },
}

/// Adapter state for each target that has native routing information.
pub type NativeAdapters = BTreeMap<ProjectionTargetId, NativeAdapterState>;

/// Native route status for one component on one target.
///
/// The status is separate from [`ComponentRouteChoice`]. A written route keeps
/// its loose choice, and the status says whether native activation may replace
/// it. Only [`NativeRouteStatus::Native`] permits activation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum NativeRouteStatus {
    /// No native activation is planned. The loose route is the only route.
    #[default]
    Loose,
    /// Native capability was observed. The loose route stays active.
    ObservedLoose,
    /// Native activation is planned for the plugin group on this target.
    Native,
    /// Native activation was not planned. The loose route stays active, and
    /// the reason says why.
    Fallback { reason: String },
}

/// True when `target` has a verified native plugin adapter.
///
/// Claude Code and Codex have verified adapters. OpenCode keeps component
/// delivery at its proven root. Copilot and AGY record observed capability
/// only, so they never activate a plugin group.
pub fn native_plugin_capable(target: ProjectionTargetId) -> bool {
    matches!(
        target,
        ProjectionTargetId::Claude | ProjectionTargetId::Codex
    )
}

/// Set each written route's native status from the target adapter states.
///
/// Routes keep their loose choice and their roots. Only the status changes.
/// Applying native activation is left to the caller.
///
/// A native-capable target with a [`NativeAdapterState::Ready`] adapter groups
/// its written routes by plugin name. A group activates as one unit, so one
/// blocked member falls back the whole group. A group is blocked while a
/// shared copy it would load stays retained for another reader. A reader,
/// selected or not, counts as retained unless [`reader_coverage`] finds the
/// component for it elsewhere: a native route of its own, a shared root whose
/// copy stays, or its private root. Every group starts as native. The first
/// blocked group in order falls back, and the check repeats until no native
/// group is blocked, so each native group is judged against the final set of
/// copies that retire.
pub fn plan_native_routes(plan: &mut DeliveryPlan, adapters: &NativeAdapters) {
    let mut statuses = vec![NativeRouteStatus::Loose; plan.routes.len()];
    let mut groups: BTreeMap<(ProjectionTargetId, String), Vec<usize>> = BTreeMap::new();
    for (index, route) in plan.routes.iter().enumerate() {
        if matches!(route.choice, ComponentRouteChoice::Disabled) {
            continue;
        }
        let state = adapters.get(&route.target).cloned().unwrap_or_default();
        match state {
            NativeAdapterState::Loose => {}
            NativeAdapterState::Observed => statuses[index] = NativeRouteStatus::ObservedLoose,
            NativeAdapterState::Failed { reason } if native_plugin_capable(route.target) => {
                statuses[index] = NativeRouteStatus::Fallback {
                    reason: format!("native registration or policy failed: {reason}"),
                };
            }
            NativeAdapterState::Ready if native_plugin_capable(route.target) => {
                groups
                    .entry((route.target, route.identity.plugin_name.clone()))
                    .or_default()
                    .push(index);
            }
            _ => {}
        }
    }

    let mut native = groups.keys().cloned().collect::<BTreeSet<_>>();
    let mut blocked = BTreeMap::new();
    loop {
        let retires = |route: &ComponentTargetRoute| {
            written_root(&route.choice).is_some()
                && native.contains(&(route.target, route.identity.plugin_name.clone()))
        };
        let first = native.iter().find_map(|key| {
            groups[key]
                .iter()
                .find_map(|&index| {
                    final_group_member_reason(plan, &plan.routes[index])
                        .or_else(|| duplicate_loading_reason(plan, &plan.routes[index], &retires))
                })
                .map(|reason| (key.clone(), reason))
        });
        let Some((key, reason)) = first else {
            break;
        };
        native.remove(&key);
        blocked.insert(key, reason);
    }
    for (key, members) in &groups {
        let (target, plugin) = key;
        let status = match blocked.get(key) {
            Some(reason) => NativeRouteStatus::Fallback {
                reason: format!(
                    "native activation for plugin '{plugin}' on {target} blocked: {reason}"
                ),
            },
            None => NativeRouteStatus::Native,
        };
        for &index in members {
            statuses[index] = status.clone();
        }
    }
    for (route, status) in plan.routes.iter_mut().zip(statuses) {
        route.native = status;
    }
}

/// Compute one final result per enabled client face without accessing disk.
///
/// The supplied plan may contain validated physical identities from its
/// caller. Native statuses are finalized on a copy so the caller's plan is
/// unchanged.
pub fn plan_final_group_routes(
    plan: &DeliveryPlan,
    adapters: &NativeAdapters,
) -> Vec<FinalFaceRoute> {
    let mut finalized = plan.clone();
    plan_native_routes(&mut finalized, adapters);
    let mut routes = Vec::new();
    for route in finalized.routes {
        if matches!(route.choice, ComponentRouteChoice::Disabled) {
            continue;
        }
        for &face in route.target.client_faces() {
            routes.push(FinalFaceRoute {
                identity: route.identity.clone(),
                face,
                native: route.native.clone(),
                choice: route.choice.clone(),
                physical_identity: route.physical_identity.clone(),
            });
        }
    }
    routes.sort_by(|left, right| (&left.identity, left.face).cmp(&(&right.identity, right.face)));
    routes
}

/// Reason an enabled group member cannot safely join native activation.
fn final_group_member_reason(plan: &DeliveryPlan, route: &ComponentTargetRoute) -> Option<String> {
    match &route.choice {
        ComponentRouteChoice::Unsupported { reason } => {
            return Some(format!("required member is unsupported: {reason}"));
        }
        ComponentRouteChoice::Blocked { reason } => {
            return Some(format!("required member is blocked: {reason}"));
        }
        ComponentRouteChoice::Disabled => return None,
        ComponentRouteChoice::Direct { .. } | ComponentRouteChoice::ServedByRoot { .. } => {}
    }

    match &route.physical_identity {
        PhysicalIdentity::Rejected => {
            return Some("route has an escaping or unresolved physical identity".into());
        }
        PhysicalIdentity::Validated(path) if !valid_physical_identity(path) => {
            return Some("route has an escaping or unresolved physical identity".into());
        }
        PhysicalIdentity::Unchecked | PhysicalIdentity::Validated(_) => {}
    }
    if route.foreign_alias_path.is_some() && route.alias.is_none() {
        return Some("a foreign namespace collision has no resolved alias".into());
    }

    let root = written_root(&route.choice).expect("written route matched above");
    let name = route_name(route);
    let collides = plan.routes.iter().any(|other| {
        other.identity != route.identity
            && other.target == route.target
            && written_root(&other.choice)
                .is_some_and(|other_root| same_physical_root(route, root, other, other_root))
            && route_name(other).eq_ignore_ascii_case(name)
    });
    collides.then(|| format!("namespace collision for '{name}' in {root}"))
}

fn valid_physical_identity(path: &std::path::Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
}

fn same_physical_root(
    left: &ComponentTargetRoute,
    left_root: &str,
    right: &ComponentTargetRoute,
    right_root: &str,
) -> bool {
    match (&left.physical_identity, &right.physical_identity) {
        (PhysicalIdentity::Validated(left), PhysicalIdentity::Validated(right)) => {
            physical_paths_equal(left, right)
        }
        _ => left_root == right_root,
    }
}

fn physical_paths_equal(left: &std::path::Path, right: &std::path::Path) -> bool {
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

/// Reason the route's plugin group cannot activate natively, when a shared
/// copy the route's target loads has a reader that would lose the component
/// once that copy retires. `retires` says which routes' copies retire.
fn duplicate_loading_reason(
    plan: &DeliveryPlan,
    route: &ComponentTargetRoute,
    retires: &dyn Fn(&ComponentTargetRoute) -> bool,
) -> Option<String> {
    for root in roots_discovered_by(route.target, route.identity.kind) {
        if !is_shared_root(root) {
            continue;
        }
        let Some(contents) = plan
            .shared_roots
            .iter()
            .find(|contents| contents.root == root)
        else {
            continue;
        };
        if !contents.components.contains(&route.identity) {
            continue;
        }
        let retained = contents
            .reached_consumers
            .iter()
            .filter(|&&consumer| {
                consumer != route.target
                    && reader_coverage(plan, consumer, &route.identity, root, retires).is_none()
            })
            .map(|consumer| consumer.to_string())
            .collect::<Vec<_>>();
        if !retained.is_empty() {
            return Some(format!(
                "shared copy of {} at {root} stays for {} and would load twice",
                route.identity.component_id,
                retained.join(", ")
            ));
        }
    }
    None
}

/// How a reader of a shared root keeps a component once a native group
/// retires the shared copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReaderCoverage {
    /// The reader activates the component natively in the same run.
    Native,
    /// The reader still discovers the copy that the route at index `route`
    /// writes into `root`. The group publishes that copy and verifies its
    /// content before the shared copy retires.
    Copy { route: usize, root: &'static str },
}

/// How `consumer` keeps `identity` once the copy in the shared root `shared`
/// retires, or `None` when that copy is the only one the reader would have.
///
/// `retires` says whether a route's copy leaves in this run because its
/// target activates natively. A reader is covered by its own native route, by
/// another shared root it discovers whose copy stays, or by a copy in its
/// private root. Selected and unselected readers count the same way. Only a
/// skill copy can cover a reader.
pub(crate) fn reader_coverage(
    plan: &DeliveryPlan,
    consumer: ProjectionTargetId,
    identity: &ComponentIdentity,
    shared: &str,
    retires: &dyn Fn(&ComponentTargetRoute) -> bool,
) -> Option<ReaderCoverage> {
    let reader_routes = || {
        plan.routes
            .iter()
            .enumerate()
            .filter(move |(_, route)| route.target == consumer && route.identity == *identity)
    };
    if reader_routes().any(|(_, route)| retires(route)) {
        return Some(ReaderCoverage::Native);
    }
    if identity.kind != ComponentKind::Skill {
        return None;
    }
    // Another shared root the reader discovers keeps its copy while no
    // retiring route writes the component there.
    for root in roots_discovered_by(consumer, identity.kind) {
        if root == shared || !is_shared_root(root) {
            continue;
        }
        let writers = plan
            .routes
            .iter()
            .enumerate()
            .filter(|(_, route)| {
                route.identity == *identity && written_root(&route.choice) == Some(root)
            })
            .collect::<Vec<_>>();
        if writers.iter().any(|(_, route)| retires(route)) {
            continue;
        }
        // The target whose own root this is names the copy. Any writer will
        // do when that target has no route here.
        let writer = writers
            .iter()
            .find(|(_, route)| native_root(identity.kind, route.target) == Some(root))
            .or_else(|| writers.first());
        if let Some((index, _)) = writer {
            return Some(ReaderCoverage::Copy {
                route: *index,
                root,
            });
        }
    }
    // A copy in the reader's private root.
    let (index, route) = reader_routes().next()?;
    let private = match &route.choice {
        ComponentRouteChoice::Direct { root } if !is_shared_root(root) => {
            native_root(identity.kind, consumer).filter(|private| private == root)
        }
        ComponentRouteChoice::ServedByRoot { .. } => {
            replacement_root_for_shared_reader(consumer, identity.kind)
        }
        _ => None,
    }?;
    Some(ReaderCoverage::Copy {
        route: index,
        root: private,
    })
}

/// Private root that a shared-root reader loads from once a native group
/// retires the shared copy it is served by. Only OpenCode's skill root
/// qualifies: OpenCode reads skills from its own root, so a published copy
/// there replaces the shared one for it. A reader with a direct private route
/// already has its copy and needs no replacement root.
pub(crate) fn replacement_root_for_shared_reader(
    target: ProjectionTargetId,
    kind: ComponentKind,
) -> Option<&'static str> {
    if target == ProjectionTargetId::Opencode && kind == ComponentKind::Skill {
        native_root(kind, target)
    } else {
        None
    }
}

/// Source text for each agent and command, keyed by component.
///
/// An absent entry means the source content is not available yet, so its
/// generated references cannot be checked. An `Err` means the content exists
/// but cannot be read. Both keep the route out of every write.
pub type ComponentBodies = BTreeMap<ComponentIdentity, Result<String, String>>;

/// One component and one physical root it may be written into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentRootPair {
    /// Component written into the root.
    pub identity: ComponentIdentity,
    /// Home-relative physical root.
    pub root: String,
}

/// A shared-root pair withheld before any write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithheldPair {
    /// The pair that must not be written.
    pub pair: ComponentRootPair,
    /// Selected consumers whose blocked route vetoes the pair. Empty when no
    /// selected target writes the component into the root.
    pub blocked_consumers: Vec<ProjectionTargetId>,
    /// Every target that discovers the root, selected or not.
    pub consumers: Vec<ProjectionTargetId>,
}

/// Rewritten agent or command text for one component on one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedPayload {
    /// Component the text belongs to.
    pub identity: ComponentIdentity,
    /// Target that publishes the text.
    pub target: ProjectionTargetId,
    /// Invocation name on the target, after alias selection.
    pub name: String,
    /// Text with generated references rewritten for the target.
    pub body: String,
}

/// An agent or command whose source content is not available, so its
/// references cannot be validated before publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingReference {
    /// Component whose validation waits.
    pub identity: ComponentIdentity,
    /// Target the route is planned for.
    pub target: ProjectionTargetId,
}

/// Read-only preparation of the routes and payloads a publisher may write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreparedDelivery {
    /// Routes after reference checks, consumer vetoes, and alternate roots.
    /// Shared roots list only the components that remain written.
    pub plan: DeliveryPlan,
    /// Every component and root pair the publisher may write.
    pub pairs: Vec<ComponentRootPair>,
    /// Shared-root pairs withheld before any write.
    pub withheld: Vec<WithheldPair>,
    /// Rewritten text for agent and command routes that remain written.
    pub payloads: Vec<PreparedPayload>,
    /// Agent and command routes waiting for their source content.
    pub pending: Vec<PendingReference>,
}

/// Outcome of rewriting one agent or command body for one route.
enum TextOutcome {
    Ready(String),
    Pending,
    Failed(String),
}

/// Prepare the exact component and root pairs a publisher may write.
///
/// Runs the planner, then applies three checks before any write:
///
/// 1. Each agent and command body is rewritten for its target. A reference
///    that cannot be rewritten blocks its whole unit, which is every member of
///    the same provider in the same root. A body whose source is not available
///    yet is pending, and its route is blocked until it is.
/// 2. A shared-root pair is withheld when a selected consumer of that root is
///    blocked for the component, or when no selected target writes it. Each
///    affected route moves to an alternate root that its target serves, or it
///    becomes blocked. The withheld pair and its consumers are reported.
/// 3. Shared-root contents and discovery counts are recomputed from the routes
///    that remain.
///
/// Reads nothing from disk. The caller supplies every body.
pub fn prepare_delivery(
    inventory: &DeliveryInventory,
    selected_targets: &[ProjectionTargetId],
    persisted_aliases: &[TargetAlias],
    unconvertible: &UnconvertibleCommands,
    bodies: &ComponentBodies,
) -> PreparedDelivery {
    let plan = plan_delivery_with_commands(
        inventory,
        selected_targets,
        persisted_aliases,
        unconvertible,
    );
    prepare_delivery_from_plan(
        plan,
        inventory,
        selected_targets,
        persisted_aliases,
        unconvertible,
        bodies,
        None,
    )
}

/// Finalize native routes before rewriting generated references for delivery.
///
/// Unlike [`prepare_delivery`], which remains the route preview used by init,
/// this function prepares payloads for the routes that will actually be
/// published. References in a failed native group therefore resolve against
/// its final loose destinations.
pub fn prepare_delivery_with_native_routes(
    inventory: &DeliveryInventory,
    selected_targets: &[ProjectionTargetId],
    persisted_aliases: &[TargetAlias],
    unconvertible: &UnconvertibleCommands,
    bodies: &ComponentBodies,
    adapters: &NativeAdapters,
    finalize_plan: impl FnOnce(&mut DeliveryPlan),
) -> PreparedDelivery {
    let mut plan = plan_delivery_with_commands(
        inventory,
        selected_targets,
        persisted_aliases,
        unconvertible,
    );
    finalize_plan(&mut plan);
    plan_native_routes(&mut plan, adapters);
    prepare_delivery_from_plan(
        plan,
        inventory,
        selected_targets,
        persisted_aliases,
        unconvertible,
        bodies,
        Some(adapters),
    )
}

fn prepare_delivery_from_plan(
    mut plan: DeliveryPlan,
    inventory: &DeliveryInventory,
    selected_targets: &[ProjectionTargetId],
    persisted_aliases: &[TargetAlias],
    unconvertible: &UnconvertibleCommands,
    bodies: &ComponentBodies,
    native_adapters: Option<&NativeAdapters>,
) -> PreparedDelivery {
    let provider_inventory = provider_inventory(inventory, persisted_aliases);
    // Foreign entries only push provider components onto aliases. No provider
    // body refers to them, so they never take part in reference resolution.
    let aliases = selected_targets
        .iter()
        .filter_map(|target| {
            projection::plan_provider_aliases(*target, &provider_inventory)
                .ok()
                .map(|aliases| (*target, aliases))
        })
        .collect::<BTreeMap<_, _>>();
    let reference_inventories = selected_targets
        .iter()
        .map(|target| {
            (
                *target,
                reference_inventory_for_target(inventory, *target, unconvertible),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let reference_aliases = aliases
        .iter()
        .map(|(target, aliases)| {
            (
                *target,
                reference_aliases_for_target(aliases, *target, unconvertible),
            )
        })
        .collect::<BTreeMap<_, _>>();

    // Step 1: rewrite each text body and group the results by unit.
    let mut ready = Vec::new();
    let mut blocks: Vec<(usize, String)> = Vec::new();
    let mut pending = Vec::new();
    let mut unit_failures: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut member_texts: BTreeMap<
        (String, String, ComponentIdentity),
        BTreeSet<(String, String)>,
    > = BTreeMap::new();
    for (index, route) in plan.routes.iter().enumerate() {
        if !is_text_kind(route.identity.kind) {
            continue;
        }
        let (Some(root), Some(_target_aliases)) =
            (written_root(&route.choice), aliases.get(&route.target))
        else {
            continue;
        };
        let source = route.identity.source.as_str().to_string();
        let outcome = match bodies.get(&route.identity) {
            None => TextOutcome::Pending,
            Some(Err(reason)) => TextOutcome::Failed(reason.clone()),
            Some(Ok(body)) => {
                match projection::rewrite_generated_provider_references(
                    body,
                    &reference_inventories[&route.target],
                    &reference_aliases[&route.target],
                    route.target,
                ) {
                    Ok(rewritten) => TextOutcome::Ready(rewritten),
                    Err(error) => TextOutcome::Failed(format!(
                        "generated reference cannot be rewritten: {error}"
                    )),
                }
            }
        };
        match outcome {
            TextOutcome::Ready(body) => {
                member_texts
                    .entry((source.clone(), root.to_string(), route.identity.clone()))
                    .or_default()
                    .insert((route_name(route).to_string(), body.clone()));
                ready.push((index, body));
            }
            TextOutcome::Pending => {
                blocks.push((
                    index,
                    "reference validation pending: source content is not available".into(),
                ));
                pending.push(PendingReference {
                    identity: route.identity.clone(),
                    target: route.target,
                });
            }
            TextOutcome::Failed(reason) => {
                unit_failures
                    .entry((source, root.to_string()))
                    .or_insert(reason);
            }
        }
    }
    // One component that resolves to different names or text on targets that
    // share a root cannot be written once, so its unit fails.
    for ((source, root, _), texts) in &member_texts {
        if texts.len() > 1 {
            unit_failures
                .entry((source.clone(), root.clone()))
                .or_insert_with(|| "shared root holds incompatible reference rewrites".into());
        }
    }
    for (index, route) in plan.routes.iter().enumerate() {
        let Some(root) = written_root(&route.choice) else {
            continue;
        };
        if !is_text_kind(route.identity.kind) {
            continue;
        }
        let unit = (route.identity.source.as_str().to_string(), root.to_string());
        if let Some(reason) = unit_failures.get(&unit) {
            blocks.push((index, reason.clone()));
        }
    }
    let blocked = blocks
        .iter()
        .map(|(index, _)| *index)
        .collect::<BTreeSet<_>>();
    for (index, reason) in blocks {
        plan.routes[index].choice = ComponentRouteChoice::Blocked { reason };
    }

    // Step 2: withhold each shared-root pair that a blocked selected consumer
    // or a missing writer leaves unwritable, then move its routes.
    let live = plan
        .routes
        .iter()
        .filter_map(|route| {
            let root = written_root(&route.choice)?;
            (own_root(&route.identity, route.target, unconvertible) == Some(root))
                .then(|| (route.identity.clone(), root.to_string()))
        })
        .collect::<BTreeSet<_>>();
    let mut withheld = Vec::new();
    let mut withheld_pairs = BTreeSet::new();
    for (identity, root) in written_pairs(&plan.routes) {
        if !is_shared_root(&root) {
            continue;
        }
        let row = SHARED_ROOT_CONSUMERS
            .iter()
            .find(|row| row.root == root)
            .expect("every shared root has a consumer row");
        let blocked_consumers = row
            .consumers
            .iter()
            .copied()
            .filter(|target| {
                selected_targets.contains(target) && is_blocked(&plan.routes, &identity, *target)
            })
            .collect::<Vec<_>>();
        if blocked_consumers.is_empty() && live.contains(&(identity.clone(), root.clone())) {
            continue;
        }
        withheld_pairs.insert((identity.clone(), root.clone()));
        withheld.push(WithheldPair {
            pair: ComponentRootPair {
                identity,
                root: root.clone(),
            },
            blocked_consumers,
            consumers: row.consumers.to_vec(),
        });
    }
    for route in &mut plan.routes {
        let Some(root) = written_root(&route.choice).map(str::to_string) else {
            continue;
        };
        if withheld_pairs.contains(&(route.identity.clone(), root.clone())) {
            route.choice = alternate_choice(route, &root, &live, &withheld_pairs);
        }
    }

    // Step 3: recompute shared contents and discovery from the remaining routes.
    let final_pairs = written_pairs(&plan.routes);
    let mut contents: BTreeMap<String, BTreeSet<ComponentIdentity>> = BTreeMap::new();
    for (identity, root) in &final_pairs {
        if is_shared_root(root) {
            contents
                .entry(root.clone())
                .or_default()
                .insert(identity.clone());
        }
    }
    plan.shared_roots = contents
        .into_iter()
        .map(|(root, identities)| {
            let row = SHARED_ROOT_CONSUMERS
                .iter()
                .find(|row| row.root == root)
                .expect("every shared root has a consumer row");
            SharedRootContents {
                root,
                components: identities.into_iter().collect(),
                reached_consumers: row.consumers.to_vec(),
            }
        })
        .collect();
    for route in &mut plan.routes {
        let shared_discovered = roots_discovered_by(route.target, route.identity.kind)
            .into_iter()
            .filter(|root| {
                is_shared_root(root)
                    && final_pairs.contains(&(route.identity.clone(), root.to_string()))
            })
            .count();
        let private_added = matches!(
            &route.choice,
            ComponentRouteChoice::Direct { root } if !is_shared_root(root)
        );
        route.discovered_root_count = shared_discovered + usize::from(private_added);
        route.multi_root = route.discovered_root_count > 1;
    }

    if let Some(adapters) = native_adapters {
        // Reference failures can invalidate a native group. Recompute native
        // statuses after those failures so every member falls back together.
        plan_native_routes(&mut plan, adapters);
    }

    let payloads = ready
        .into_iter()
        .filter(|(index, _)| !blocked.contains(index))
        .filter(|(index, _)| written_root(&plan.routes[*index].choice).is_some())
        .map(|(index, body)| {
            let route = &plan.routes[index];
            PreparedPayload {
                identity: route.identity.clone(),
                target: route.target,
                name: route_name(route).to_string(),
                body,
            }
        })
        .collect();
    let pairs = final_pairs
        .into_iter()
        .map(|(identity, root)| ComponentRootPair { identity, root })
        .collect();
    PreparedDelivery {
        plan,
        pairs,
        withheld,
        payloads,
        pending,
    }
}

/// Every provider and foreign identity, with the persisted aliases, as the
/// alias planner reads them.
fn provider_inventory(
    inventory: &DeliveryInventory,
    persisted_aliases: &[TargetAlias],
) -> ProviderProjectionInventory {
    let mut identities = inventory
        .components
        .iter()
        .map(|component| component.identity.clone())
        .collect::<Vec<_>>();
    identities.extend(
        inventory
            .foreign_components
            .iter()
            .map(|item| item.identity.clone()),
    );
    ProviderProjectionInventory {
        identities,
        aliases: persisted_aliases.to_vec(),
    }
}

fn reference_inventory_for_target(
    inventory: &DeliveryInventory,
    target: ProjectionTargetId,
    unconvertible: &UnconvertibleCommands,
) -> ProviderProjectionInventory {
    let identities = inventory
        .components
        .iter()
        .map(|component| {
            let mut identity = component.identity.clone();
            if converts_to_skill(&identity, target, unconvertible) {
                identity.kind = ComponentKind::Skill;
            }
            identity
        })
        .collect::<Vec<_>>();
    ProviderProjectionInventory {
        identities,
        aliases: Vec::new(),
    }
}

fn reference_aliases_for_target(
    aliases: &[TargetAlias],
    target: ProjectionTargetId,
    unconvertible: &UnconvertibleCommands,
) -> Vec<TargetAlias> {
    aliases
        .iter()
        .map(|alias| {
            let mut alias = alias.clone();
            if converts_to_skill(&alias.identity, target, unconvertible) {
                alias.identity.kind = ComponentKind::Skill;
            }
            alias
        })
        .collect()
}

/// Physical root a route writes into, or `None` when it writes nothing.
fn written_root(choice: &ComponentRouteChoice) -> Option<&str> {
    match choice {
        ComponentRouteChoice::Direct { root } | ComponentRouteChoice::ServedByRoot { root } => {
            Some(root)
        }
        _ => None,
    }
}

/// Every (component, root) pair that a written route publishes.
fn written_pairs(routes: &[ComponentTargetRoute]) -> BTreeSet<(ComponentIdentity, String)> {
    routes
        .iter()
        .filter_map(|route| {
            written_root(&route.choice).map(|root| (route.identity.clone(), root.to_string()))
        })
        .collect()
}

/// True when `target` has a blocked route for `identity`.
fn is_blocked(
    routes: &[ComponentTargetRoute],
    identity: &ComponentIdentity,
    target: ProjectionTargetId,
) -> bool {
    routes.iter().any(|route| {
        route.identity == *identity
            && route.target == target
            && matches!(route.choice, ComponentRouteChoice::Blocked { .. })
    })
}

/// Root a target's own planner step reads for a component: its native root,
/// or its command-to-skill root for a converted command.
fn own_root(
    identity: &ComponentIdentity,
    target: ProjectionTargetId,
    unconvertible: &UnconvertibleCommands,
) -> Option<&'static str> {
    if converts_to_skill(identity, target, unconvertible) {
        command_skill_root(target)
    } else {
        native_root(identity.kind, target)
    }
}

/// Source text kinds: the agent and command bodies whose references are
/// rewritten before publication.
fn is_text_kind(kind: ComponentKind) -> bool {
    matches!(kind, ComponentKind::Agent | ComponentKind::Command)
}

/// Choice for a route whose shared root is withheld: an alternate root that
/// its target serves, or a block that names the withheld root.
///
/// A shared alternate must already hold the component from a live writer.
/// A private alternate is the target's own root for the kind, so it needs no
/// consumer check.
fn alternate_choice(
    route: &ComponentTargetRoute,
    withheld_root: &str,
    live: &BTreeSet<(ComponentIdentity, String)>,
    withheld: &BTreeSet<(ComponentIdentity, String)>,
) -> ComponentRouteChoice {
    let kind = route.identity.kind;
    for candidate in roots_discovered_by(route.target, kind) {
        let key = (route.identity.clone(), candidate.to_string());
        if candidate == withheld_root || withheld.contains(&key) {
            continue;
        }
        if is_shared_root(candidate) {
            if root_serves(route.target, candidate) && live.contains(&key) {
                return ComponentRouteChoice::ServedByRoot {
                    root: candidate.to_string(),
                };
            }
        } else if is_route_supported(kind, route.target)
            || command_skill_root(route.target) == Some(candidate)
        {
            return ComponentRouteChoice::Direct {
                root: candidate.to_string(),
            };
        }
    }
    ComponentRouteChoice::Blocked {
        reason: format!(
            "{withheld_root} is withheld for {} because a selected consumer is blocked, and no alternate root serves it on {}",
            route.identity.component_id, route.target
        ),
    }
}

/// Name a route publishes under: its alias, or its original name.
fn route_name(route: &ComponentTargetRoute) -> &str {
    route
        .alias
        .as_deref()
        .unwrap_or(&route.identity.component_id)
}

fn is_shared_root(root: &str) -> bool {
    SHARED_ROOT_CONSUMERS.iter().any(|row| row.root == root)
}

fn root_serves(target: ProjectionTargetId, root: &str) -> bool {
    SHARED_ROOT_CONSUMERS
        .iter()
        .find(|row| row.root == root)
        .is_some_and(|row| row.served_by.contains(&target))
}

fn roots_discovered_by(target: ProjectionTargetId, kind: ComponentKind) -> Vec<&'static str> {
    let mut roots = SHARED_ROOT_CONSUMERS
        .iter()
        .filter(|row| {
            row.consumers.contains(&target)
                && match kind {
                    ComponentKind::Agent => row.root.ends_with("/agents"),
                    ComponentKind::Skill | ComponentKind::Command => row.root.ends_with("/skills"),
                    ComponentKind::Hook | ComponentKind::Resource => false,
                }
        })
        .map(|row| row.root)
        .collect::<Vec<_>>();
    let private = native_root(kind, target).or_else(|| {
        (kind == ComponentKind::Command)
            .then(|| command_skill_root(target))
            .flatten()
    });
    if let Some(private) = private {
        if !roots.contains(&private) {
            roots.push(private);
        }
    }
    roots
}

/// Skill root that receives a command's equivalent skill on a target without
/// a native command root.
///
/// Only Codex and Copilot have a command-to-skill writer. The AGY faces have
/// none, so their commands stay unsupported and are reported.
pub fn command_skill_root(target: ProjectionTargetId) -> Option<&'static str> {
    match target {
        ProjectionTargetId::Codex => Some("~/.agents/skills"),
        ProjectionTargetId::Copilot => Some("~/.copilot/skills"),
        _ => None,
    }
}

/// True when `identity` is a convertible command and `target` publishes
/// commands only as their equivalent skills.
fn converts_to_skill(
    identity: &ComponentIdentity,
    target: ProjectionTargetId,
    unconvertible: &UnconvertibleCommands,
) -> bool {
    identity.kind == ComponentKind::Command
        && native_root(ComponentKind::Command, target).is_none()
        && command_skill_root(target).is_some()
        && !unconvertible.contains_key(identity)
}

/// Route for a command on a target without a native command root.
///
/// The command-to-skill writer keeps the command name, so an alias forced by
/// a same-name entry in the skill root blocks the route instead of renaming
/// it. A target served by a shared root that holds the converted skill gets no
/// private copy.
fn command_choice(
    identity: &ComponentIdentity,
    target: ProjectionTargetId,
    alias: Option<&str>,
    roots: &[&'static str],
    root_content: &BTreeMap<String, BTreeSet<ComponentIdentity>>,
    unconvertible: &UnconvertibleCommands,
) -> ComponentRouteChoice {
    if let Some(reason) = unconvertible.get(identity) {
        return ComponentRouteChoice::Unsupported {
            reason: format!("command has no equivalent skill: {reason}"),
        };
    }
    let Some(skill_root) = command_skill_root(target) else {
        return ComponentRouteChoice::Unsupported {
            reason: "no command-to-skill surface on this target".into(),
        };
    };
    if let Some(alias) = alias.filter(|alias| *alias != identity.component_id) {
        return ComponentRouteChoice::Blocked {
            reason: format!(
                "command-to-skill conversion keeps the name '{}', so it cannot publish under alias '{alias}'",
                identity.component_id
            ),
        };
    }
    if let Some(root) = roots.iter().find(|root| {
        root_content
            .get(**root)
            .is_some_and(|items| items.contains(identity))
            && root_serves(target, root)
    }) {
        return ComponentRouteChoice::ServedByRoot {
            root: (*root).to_string(),
        };
    }
    ComponentRouteChoice::Direct {
        root: skill_root.to_owned(),
    }
}

fn native_root(kind: ComponentKind, target: ProjectionTargetId) -> Option<&'static str> {
    match kind {
        ComponentKind::Skill => match target {
            ProjectionTargetId::Claude => Some("~/.claude/skills"),
            ProjectionTargetId::Codex => Some("~/.agents/skills"),
            ProjectionTargetId::Copilot => Some("~/.copilot/skills"),
            ProjectionTargetId::Opencode => Some("~/.config/opencode/skills"),
            ProjectionTargetId::AgyCli => Some("~/.gemini/antigravity-cli/skills"),
            ProjectionTargetId::AgyIde | ProjectionTargetId::AgyGui => {
                Some("~/.gemini/config/skills")
            }
        },
        ComponentKind::Command => match target {
            ProjectionTargetId::Claude => Some("~/.claude/commands"),
            ProjectionTargetId::Opencode => Some("~/.config/opencode/commands"),
            _ => None,
        },
        ComponentKind::Agent => match target {
            ProjectionTargetId::Claude => Some("~/.claude/agents"),
            ProjectionTargetId::Codex => Some("~/.codex/agents"),
            ProjectionTargetId::Copilot => Some("~/.copilot/agents"),
            ProjectionTargetId::Opencode => Some("~/.config/opencode/agents"),
            ProjectionTargetId::AgyCli | ProjectionTargetId::AgyGui => {
                Some("~/.gemini/config/agents")
            }
            ProjectionTargetId::AgyIde => None,
        },
        ComponentKind::Hook | ComponentKind::Resource => None,
    }
}

fn is_route_supported(kind: ComponentKind, target: ProjectionTargetId) -> bool {
    match kind {
        ComponentKind::Skill => true,
        ComponentKind::Command => matches!(
            target,
            ProjectionTargetId::Claude | ProjectionTargetId::Opencode
        ),
        ComponentKind::Agent => native_root(kind, target).is_some(),
        ComponentKind::Hook | ComponentKind::Resource => false,
    }
}
