//! Unified managed-item view over the lockfile.
//!
//! ccync's lockfile carries two independent namespaces that both describe
//! items the user actually has installed:
//! - `_personalPlugins` — ccync-catalog-managed plugins (reference-is-truth;
//!   `pluginId` / `source` / `pin` / `held`).
//! - `_adoptedItems` — items `ccync init` found already installed in a master
//!   agent and adopted read-only (`name` / `sourceId` / `source` / `version`).
//!
//! Every other command (`list`, `show`, `upgrade`, `remove`) should see items
//! through a single merged view rather than re-deriving namespace logic per
//! verb. This module provides that single merged view: pure, read-only, and
//! panic-free over an arbitrary `serde_json::Value`.

use crate::delivery::{ComponentRouteChoice, ComponentTargetRoute, NativeRouteStatus};
use ccync_foundation::provider::{ComponentKind, NativeObservation};
use ccync_foundation::target::ProjectionTargetId;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

const PERSONAL_KEY: &str = "_personalPlugins";
const ADOPTED_KEY: &str = "_adoptedItems";

/// Where a managed item's truth is recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Declared in the user's MCP input.
    Declared,
    /// Came from `_personalPlugins` (ccync-catalog-managed).
    Personal,
    /// Came from `_adoptedItems` (adopted from a master agent, read-only).
    Adopted,
}

/// What kind of managed content this item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Plugin,
    McpServer,
    Skill,
}

impl ItemKind {
    /// Classify from an adopted entry's `sourceId` tag (see `adopt.rs::source_id`).
    fn from_source_id(source_id: &str) -> ItemKind {
        if source_id.contains("mcp") {
            ItemKind::McpServer
        } else if source_id.contains("skill") {
            ItemKind::Skill
        } else {
            ItemKind::Plugin
        }
    }
}

/// A single item in the merged managed view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedItem {
    /// Stable identifier: `pluginId` for personal entries, `name` for adopted
    /// entries.
    pub id: String,
    pub kind: ItemKind,
    pub origin: Origin,
    /// Resolvable source reference, when known (absent for MCP/skill items
    /// and for unresolved adopted plugin sources).
    pub source: Option<String>,
    /// Pinned/resolved commit sha, when recorded (personal entries only).
    pub pin: Option<String>,
    /// `true` when the user has pinned this item against `upgrade` (personal
    /// entries only; `ccync pin`).
    pub held: bool,
    /// `true` when an adopted entry with this id is shadowed by a personal
    /// entry of the same id (personal always wins duplicate ids). Always
    /// `false` for personal entries and for adopted entries with no personal
    /// duplicate.
    pub shadowed: bool,
}

/// Provider details kept separate from the stable merged managed-item API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDiagnostic {
    pub provider: String,
    pub source_key: String,
    pub manager: String,
    pub alias: Option<String>,
    pub effective_loader: String,
    pub ownership: DiagnosticOwnership,
    pub verification_scope: VerificationScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticOwnership {
    Managed,
    ExternalConflict,
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationScope {
    FilesystemOnly,
    LoaderNotRun,
}

/// Read-only provider diagnostics from committed lifecycle records. Secrets and
/// arbitrary lockfile values are deliberately never copied into this view.
pub fn provider_diagnostics(lock: &Value) -> Vec<ProviderDiagnostic> {
    use ccync_foundation::provider::ProviderLifecycleLock;
    let Ok(section) = ProviderLifecycleLock::from_lock(lock) else {
        return Vec::new();
    };
    section
        .committed
        .into_iter()
        .map(|record| ProviderDiagnostic {
            provider: record.plugin_name,
            source_key: record.generation.source.to_string(),
            manager: "ccync".to_string(),
            alias: None,
            effective_loader: record.selector.to_string(),
            ownership: DiagnosticOwnership::Managed,
            verification_scope: VerificationScope::LoaderNotRun,
        })
        .collect()
}

/// Route facts for one component on one target. Each fact has its own field
/// and its own label, so a published, owned component whose loader was not
/// run never reads as loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteEvidence {
    pub target: ProjectionTargetId,
    pub kind: ComponentKind,
    /// Provider name before any alias.
    pub original_name: String,
    /// Provider source key the component comes from.
    pub source_key: String,
    /// Recorded origin path of the component's source, when known.
    pub origin_path: Option<String>,
    /// Recorded origin reference (package or repository), when known.
    pub origin_reference: Option<String>,
    /// True when a command is published as its equivalent skill.
    pub command_to_skill: bool,
    /// Why the route is not published, when publication reported a reason.
    pub failure_reason: Option<String>,
    /// Why the route plan left the route unsupported. Set only for an
    /// unsupported route, which is never written, so no write failed.
    pub unsupported_reason: Option<String>,
    /// Name the target uses, when it differs from `original_name`.
    pub alias: Option<String>,
    /// Directory where ccync writes or discovers the component, with its name.
    pub path: Option<String>,
    /// Shared root that serves the component to this target.
    pub served_by_root: Option<String>,
    /// True when the target discovers the component in more than one root.
    pub multi_root: bool,
    /// Foreign path that caused the alias, when a foreign artifact did.
    pub foreign_alias_path: Option<String>,
    /// True when the target is in the desired selection.
    pub selected: bool,
    /// True when the write for this route succeeded.
    pub published: bool,
    /// True when ccync's ownership record covers the written path.
    pub owned: bool,
    /// Loader check result. `FilesystemOnly` and `LoaderNotRun` never mean loaded.
    pub loader: VerificationScope,
    /// Loader version, when the loader reported one.
    pub loader_version: Option<String>,
    /// User-level registration that ccync does not write.
    pub registration: Option<RouteRegistration>,
}

/// Storage authority and reader placement, reported separately from route
/// publication and loader observations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutEvidence {
    pub authority: ccync_foundation::paths::EffectiveLayoutAuthority,
    pub migration_phase: String,
    pub effective_roots: Vec<(String, PathBuf)>,
    pub faces: Vec<StorageFaceEvidence>,
    pub retained_live_surfaces: Vec<PathBuf>,
    pub historical_row_counts: BTreeMap<String, usize>,
    pub safe_retry_actions: Vec<String>,
}

/// Reader state for one storage face. `served` identifies the active reader,
/// while incidental and preserved paths never imply authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageFaceEvidence {
    pub face: String,
    pub served: Vec<PathBuf>,
    pub incidental: Vec<PathBuf>,
    pub preserved: Vec<PathBuf>,
    pub fallback_reason: Option<String>,
    pub block_reason: Option<String>,
}

/// Render storage evidence without exposing the contents of stored records.
pub fn layout_evidence_lines(evidence: &LayoutEvidence) -> Vec<String> {
    let mut lines = vec![format!(
        "layout: authority {:?} | migration {} | publication and loading are separate evidence",
        evidence.authority, evidence.migration_phase
    )];
    lines.extend(
        evidence
            .effective_roots
            .iter()
            .map(|(name, path)| format!("layout root {name}: {}", path.display())),
    );
    for face in &evidence.faces {
        let served = display_paths(&face.served);
        let incidental = display_paths(&face.incidental);
        let preserved = display_paths(&face.preserved);
        let fallback = face.fallback_reason.as_deref().unwrap_or("none");
        let blocked = face.block_reason.as_deref().unwrap_or("none");
        lines.push(format!(
            "layout face {}: served {} | incidental {} | preserved {} | fallback {} | blocked {}",
            face.face, served, incidental, preserved, fallback, blocked
        ));
    }
    lines.push(format!(
        "layout retained live surfaces: {}",
        display_paths(&evidence.retained_live_surfaces)
    ));
    for (category, count) in &evidence.historical_row_counts {
        lines.push(format!("layout historical rows {category}: {count}"));
    }
    lines.extend(
        evidence
            .safe_retry_actions
            .iter()
            .map(|action| format!("layout safe retry: {action}")),
    );
    lines
}

fn display_paths(paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        "none".to_string()
    } else {
        paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Native status text for a route, or `None` when no native state applies.
///
/// A fallback reports its reason and says the loose route stays active. It
/// never reads as a native success.
pub fn native_status_note(route: &ComponentTargetRoute) -> Option<String> {
    match &route.native {
        NativeRouteStatus::Loose => None,
        NativeRouteStatus::ObservedLoose => {
            Some("native capability observed; loose route kept".into())
        }
        NativeRouteStatus::Native => Some("native activation planned".into()),
        NativeRouteStatus::Fallback { reason } => {
            Some(format!("native fallback, loose route active: {reason}"))
        }
    }
}

/// Registration state that ccync reports but never writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteRegistration {
    /// The target lists the component, but ccync did not register it.
    ListedNotRegistered,
    /// The user registered the component. Remove it with the uninstall command.
    RegisteredByUser { uninstall_command: String },
}

impl RouteEvidence {
    /// Build evidence from one route. `published` and `owned` come from the
    /// write result and the ownership record, not from the route itself.
    pub fn from_route(
        route: &ComponentTargetRoute,
        selected: bool,
        published: bool,
        owned: bool,
        loader: VerificationScope,
    ) -> Self {
        let name = route
            .alias
            .clone()
            .unwrap_or_else(|| route.identity.component_id.clone());
        let (path, served_by_root) = match &route.choice {
            ComponentRouteChoice::Direct { root } => (Some(format!("{root}/{name}")), None),
            ComponentRouteChoice::ServedByRoot { root } => {
                (Some(format!("{root}/{name}")), Some(root.clone()))
            }
            _ => (None, None),
        };
        let origin = route.identity.origin.as_ref();
        Self {
            target: route.target,
            kind: route.identity.kind,
            original_name: route.identity.component_id.clone(),
            source_key: route.identity.source.to_string(),
            origin_path: origin
                .and_then(|origin| origin.resolved_path.as_ref())
                .map(|path| path.display().to_string()),
            origin_reference: origin.and_then(|origin| origin.canonical_reference.clone()),
            command_to_skill: route.command_to_skill,
            failure_reason: if published {
                None
            } else {
                route.publication_failure.clone()
            },
            unsupported_reason: match &route.choice {
                ComponentRouteChoice::Unsupported { reason } => Some(reason.clone()),
                _ => None,
            },
            alias: route
                .alias
                .clone()
                .filter(|alias| alias != &route.identity.component_id),
            path,
            served_by_root,
            multi_root: route.multi_root,
            foreign_alias_path: route.foreign_alias_path.clone(),
            selected,
            published,
            owned,
            loader,
            loader_version: None,
            registration: None,
        }
    }

    /// The next repair step for this route. A route that is not published or
    /// is registered by the user needs a fix. Any other route still lacks a
    /// loaded state, because `VerificationScope` has no loaded variant, so its
    /// step is a loader check. That step is informational, never a warning.
    pub fn repair_action(&self) -> String {
        if !self.published && self.unsupported_reason.is_some() {
            return "no write is needed for this unsupported route".to_string();
        }
        if !self.published {
            return match &self.failure_reason {
                Some(reason) => {
                    format!("fix the reported write error ({reason}), then rerun `ccync sync`")
                }
                None => "fix the reported write error, then rerun `ccync sync`".to_string(),
            };
        }
        if let Some(RouteRegistration::RegisteredByUser { uninstall_command }) = &self.registration
        {
            return format!(
                "remove the user registration with `{uninstall_command}`, then rerun `ccync sync`"
            );
        }
        format!(
            "start {} and confirm `{}` is listed; the loader check did not run",
            self.target, self.original_name
        )
    }
}

/// One text line per route, with every fact under its own label.
pub fn route_evidence_line(evidence: &RouteEvidence) -> String {
    let mut parts = vec![
        format!(
            "{} {:?} {}",
            evidence.target, evidence.kind, evidence.original_name
        ),
        if evidence.selected {
            "selected"
        } else {
            "not selected"
        }
        .to_string(),
        match (&evidence.unsupported_reason, evidence.published) {
            (Some(reason), false) => format!("not published because unsupported: {reason}"),
            (_, true) => "published".to_string(),
            (_, false) => "not published".to_string(),
        },
        if evidence.owned {
            "owned by ccync"
        } else {
            "not owned by ccync"
        }
        .to_string(),
        match evidence.loader {
            VerificationScope::LoaderNotRun => "loader NotRun".to_string(),
            VerificationScope::FilesystemOnly => "loader FilesystemOnly".to_string(),
        },
    ];
    if let Some(version) = &evidence.loader_version {
        parts.push(format!("loader version {version}"));
    }
    parts.push(origin_label(evidence));
    parts.push(route_label(evidence).to_string());
    if let Some(alias) = &evidence.alias {
        parts.push(format!("alias {alias}"));
    }
    if let Some(path) = &evidence.path {
        parts.push(format!("path {path}"));
    }
    if let Some(root) = &evidence.served_by_root {
        parts.push(format!("served by root {root}"));
    }
    if evidence.multi_root {
        parts.push("discovered in multiple roots".to_string());
    }
    if let Some(foreign) = &evidence.foreign_alias_path {
        parts.push(format!("alias caused by foreign path {foreign}"));
    }
    match &evidence.registration {
        Some(RouteRegistration::ListedNotRegistered) => {
            parts.push("listed, not registered by ccync".to_string());
        }
        Some(RouteRegistration::RegisteredByUser { uninstall_command }) => {
            parts.push(format!(
                "registered by user, uninstall with `{uninstall_command}`"
            ));
        }
        None => {}
    }
    parts.push(format!("action: {}", evidence.repair_action()));
    parts.join("; ")
}

/// The component's origin: its source key, then the recorded origin path, or
/// the recorded reference when no path was recorded.
fn origin_label(evidence: &RouteEvidence) -> String {
    match (&evidence.origin_path, &evidence.origin_reference) {
        (Some(path), _) => format!("origin {} at {path}", evidence.source_key),
        (None, Some(reference)) => format!("origin {} from {reference}", evidence.source_key),
        (None, None) => format!("origin {} (no recorded origin)", evidence.source_key),
    }
}

/// The kind of route: a command published as its skill, a shared root that
/// serves the target, a direct private copy, or no route.
fn route_label(evidence: &RouteEvidence) -> &'static str {
    if evidence.path.is_none() {
        "route none"
    } else if evidence.command_to_skill {
        "route command-to-skill"
    } else if evidence.served_by_root.is_some() {
        "route served by root"
    } else {
        "route direct"
    }
}

/// Schema version of the committed route record.
const ROUTE_RECORD_SCHEMA_VERSION: u64 = 1;

/// Path of the committed route record: the route facts of the last sync.
pub fn route_record_path() -> Option<std::path::PathBuf> {
    ccync_foundation::paths::EffectiveLayout::load_or_legacy()
        .ok()
        .map(|layout| layout.routes_path())
}

/// Write the route facts of one sync as the committed route record. The write
/// is atomic, so a reader sees either the previous record or this one.
pub fn write_route_record(path: &std::path::Path, routes: &[RouteEvidence]) -> Result<(), String> {
    let rows = routes.iter().map(route_row).collect::<Vec<_>>();
    let document = serde_json::json!({
        "schemaVersion": ROUTE_RECORD_SCHEMA_VERSION,
        "routes": rows,
    });
    let bytes = serde_json::to_vec_pretty(&document).map_err(|error| error.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    ccync_foundation::platform::atomic_write_bytes(path, &bytes).map_err(|error| error.to_string())
}

/// Read the committed route record. An absent record is `Ok(None)`: no sync
/// has recorded routes yet. A record that cannot be read or parsed is an
/// error, so a corrupt record is never mistaken for "no routes".
pub fn read_route_record(path: &std::path::Path) -> Result<Option<Vec<RouteEvidence>>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let document: Value =
        serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    if document.get("schemaVersion").and_then(Value::as_u64) != Some(ROUTE_RECORD_SCHEMA_VERSION) {
        return Err(format!("{}: unsupported schema version", path.display()));
    }
    document
        .get("routes")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{}: no routes array", path.display()))?
        .iter()
        .map(|row| {
            parse_route_row(row).ok_or_else(|| format!("{}: malformed route row", path.display()))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn kind_name(kind: ComponentKind) -> &'static str {
    match kind {
        ComponentKind::Skill => "skill",
        ComponentKind::Command => "command",
        ComponentKind::Agent => "agent",
        ComponentKind::Hook => "hook",
        ComponentKind::Resource => "resource",
    }
}

fn parse_kind(name: &str) -> Option<ComponentKind> {
    match name {
        "skill" => Some(ComponentKind::Skill),
        "command" => Some(ComponentKind::Command),
        "agent" => Some(ComponentKind::Agent),
        "hook" => Some(ComponentKind::Hook),
        "resource" => Some(ComponentKind::Resource),
        _ => None,
    }
}

fn route_row(evidence: &RouteEvidence) -> Value {
    let (registration, uninstall_command) = match &evidence.registration {
        Some(RouteRegistration::ListedNotRegistered) => (Some("listedNotRegistered"), None),
        Some(RouteRegistration::RegisteredByUser { uninstall_command }) => {
            (Some("registeredByUser"), Some(uninstall_command.clone()))
        }
        None => (None, None),
    };
    serde_json::json!({
        "target": evidence.target.as_str(),
        "kind": kind_name(evidence.kind),
        "originalName": evidence.original_name,
        "sourceKey": evidence.source_key,
        "originPath": evidence.origin_path,
        "originReference": evidence.origin_reference,
        "commandToSkill": evidence.command_to_skill,
        "failureReason": evidence.failure_reason,
        "unsupportedReason": evidence.unsupported_reason,
        "alias": evidence.alias,
        "path": evidence.path,
        "servedByRoot": evidence.served_by_root,
        "multiRoot": evidence.multi_root,
        "foreignAliasPath": evidence.foreign_alias_path,
        "selected": evidence.selected,
        "published": evidence.published,
        "owned": evidence.owned,
        "loader": match evidence.loader {
            VerificationScope::LoaderNotRun => "loaderNotRun",
            VerificationScope::FilesystemOnly => "filesystemOnly",
        },
        "loaderVersion": evidence.loader_version,
        "registration": registration,
        "uninstallCommand": uninstall_command,
    })
}

fn parse_route_row(row: &Value) -> Option<RouteEvidence> {
    let text = |key: &str| row.get(key).and_then(Value::as_str).map(str::to_string);
    let flag = |key: &str| row.get(key).and_then(Value::as_bool);
    let registration = match row.get("registration").and_then(Value::as_str) {
        None => None,
        Some("listedNotRegistered") => Some(RouteRegistration::ListedNotRegistered),
        Some("registeredByUser") => Some(RouteRegistration::RegisteredByUser {
            uninstall_command: text("uninstallCommand")?,
        }),
        Some(_) => return None,
    };
    Some(RouteEvidence {
        target: row.get("target")?.as_str()?.parse().ok()?,
        kind: parse_kind(row.get("kind")?.as_str()?)?,
        original_name: text("originalName")?,
        source_key: text("sourceKey")?,
        origin_path: text("originPath"),
        origin_reference: text("originReference"),
        command_to_skill: flag("commandToSkill")?,
        failure_reason: text("failureReason"),
        unsupported_reason: text("unsupportedReason"),
        alias: text("alias"),
        path: text("path"),
        served_by_root: text("servedByRoot"),
        multi_root: flag("multiRoot")?,
        foreign_alias_path: text("foreignAliasPath"),
        selected: flag("selected")?,
        published: flag("published")?,
        owned: flag("owned")?,
        loader: match row.get("loader")?.as_str()? {
            "loaderNotRun" => VerificationScope::LoaderNotRun,
            "filesystemOnly" => VerificationScope::FilesystemOnly,
            _ => return None,
        },
        loader_version: text("loaderVersion"),
        registration,
    })
}

/// Read `_personalPlugins` as a raw entry array. Missing/malformed → empty.
fn personal_array(lock: &Value) -> &[Value] {
    lock.get(PERSONAL_KEY)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Read `_adoptedItems` as a raw entry array. Missing/malformed → empty.
fn adopted_array(lock: &Value) -> &[Value] {
    lock.get(ADOPTED_KEY)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn personal_item(entry: &Value) -> Option<ManagedItem> {
    let id = entry.get("pluginId").and_then(Value::as_str)?.to_string();
    let source = entry
        .get("source")
        .and_then(Value::as_str)
        .map(str::to_string);
    let pin = ccync_foundation::pins::entry_pin(entry);
    let held = entry.get("held").and_then(Value::as_bool).unwrap_or(false);
    Some(ManagedItem {
        id,
        kind: ItemKind::Plugin,
        origin: Origin::Personal,
        source,
        pin,
        held,
        shadowed: false,
    })
}

fn adopted_item(entry: &Value) -> Option<ManagedItem> {
    let id = entry.get("name").and_then(Value::as_str)?.to_string();
    let kind = entry
        .get("sourceId")
        .and_then(Value::as_str)
        .map(ItemKind::from_source_id)
        .unwrap_or(ItemKind::Plugin);
    let source = entry
        .get("source")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(ManagedItem {
        id,
        kind,
        origin: Origin::Adopted,
        source,
        pin: None,
        held: false,
        shadowed: false,
    })
}

/// Build the merged managed view from a lockfile `Value`.
///
/// Reads `_personalPlugins` and `_adoptedItems`; either namespace missing or
/// malformed is treated as empty (no panic). When the same id appears in both
/// namespaces, the personal entry wins: it is the one returned, and the
/// shadowed adopted duplicate is omitted from this view. Use
/// [`build_managed_view_with_shadows`] for a view that keeps shadowed adopted
/// entries visible (marked `shadowed: true`) instead of dropping them.
pub fn build_managed_view(lock: &Value) -> Vec<ManagedItem> {
    build_managed_view_with_declared(lock, None)
}

/// Build the merged managed view with already-parsed user MCP declarations.
///
/// Declarations are emitted first in name order. They are accepted as data so
/// this view remains pure and performs no I/O. Duplicate identifiers are kept.
pub fn build_managed_view_with_declared(
    lock: &Value,
    declared: Option<&serde_json::Map<String, Value>>,
) -> Vec<ManagedItem> {
    build_managed_view_with_shadows_and_declared(lock, declared)
        .into_iter()
        .filter(|item| !(item.origin == Origin::Adopted && item.shadowed))
        .collect()
}

/// Build the merged managed view, keeping shadowed adopted entries in the
/// output (marked `shadowed: true`) rather than dropping them. Personal
/// entries always win duplicate ids: for a shared id the personal entry is
/// unmodified and the adopted entry is retained only with `shadowed: true`.
///
/// Order: personal entries first (in lockfile order), then adopted entries
/// (in lockfile order, shadowed ones included at the end).
pub fn build_managed_view_with_shadows(lock: &Value) -> Vec<ManagedItem> {
    build_managed_view_with_shadows_and_declared(lock, None)
}

/// Shadow-inclusive variant of [`build_managed_view_with_declared`].
pub fn build_managed_view_with_shadows_and_declared(
    lock: &Value,
    declared: Option<&serde_json::Map<String, Value>>,
) -> Vec<ManagedItem> {
    let personal: Vec<ManagedItem> = personal_array(lock)
        .iter()
        .filter_map(personal_item)
        .collect();
    let personal_ids: std::collections::HashSet<String> =
        personal.iter().map(|i| i.id.clone()).collect();

    let mut declared_items: Vec<ManagedItem> = declared
        .into_iter()
        .flat_map(|servers| servers.keys())
        .map(|name| ManagedItem {
            id: name.clone(),
            kind: ItemKind::McpServer,
            origin: Origin::Declared,
            source: None,
            pin: None,
            held: false,
            shadowed: false,
        })
        .collect();
    declared_items.sort_by(|left, right| left.id.cmp(&right.id));

    let mut out = declared_items;
    out.extend(personal);
    for entry in adopted_array(lock) {
        let Some(mut item) = adopted_item(entry) else {
            continue;
        };
        if personal_ids.contains(&item.id) {
            item.shadowed = true;
        }
        out.push(item);
    }
    out
}

/// Native state of one component on one client, read without mutation.
///
/// Variants run from least to most severe, so the greater state is the worse
/// one. Every variant means "not proven loaded": loader verification never
/// runs in this view, so no state reports a load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NativeState {
    /// Owned, enabled, and consistent with the committed record. Loader NotRun.
    RegisteredNotLoaded,
    /// Deliberately disabled. Zero loads are expected, and the component is not missing.
    Disabled,
    /// The owned registration matches an older committed generation.
    OwnedOutdated,
    /// The client CLI is unavailable, so the state was not read. NotRun.
    Unavailable,
    /// The native state could not be read. NotRun.
    Unreadable,
    /// No registration occupies the selector. The component is missing.
    Absent,
    /// The client reports that the registration cannot load. Blocked.
    Broken,
    /// The owned registration no longer matches the committed generation. Blocked.
    Drifted,
    /// A registration that ccync does not own occupies the selector. Blocked.
    ForeignRegistration,
}

impl NativeState {
    /// Short label for the state, shown in diagnostic lines.
    pub fn label(self) -> &'static str {
        match self {
            Self::RegisteredNotLoaded => "registered, loader NotRun",
            Self::Disabled => "deliberately disabled",
            Self::OwnedOutdated => "owned, older generation",
            Self::Unavailable => "unavailable, NotRun",
            Self::Unreadable => "unreadable, NotRun",
            Self::Absent => "absent",
            Self::Broken => "broken",
            Self::Drifted => "drifted from committed generation",
            Self::ForeignRegistration => "foreign registration",
        }
    }
}

/// Native package identity for one diagnostic: the package the client lists,
/// the ccync management identity beside it, and the component or selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeIdentity {
    /// Package name the client reports.
    pub package: String,
    /// ccync management identity (source key), when recorded.
    pub management_id: Option<String>,
    /// Component or selector the client lists for the package.
    pub component: String,
}

/// Native evidence for one package on one client. It is a read-only fact:
/// producing it never runs a mutating CLI call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeDiagnostic {
    pub client: ProjectionTargetId,
    pub identity: NativeIdentity,
    /// Delivery route the diagnostic belongs to. A fallback keeps its reason.
    pub route: NativeRouteStatus,
    /// Enabled state the client reported. `None` when the client did not say.
    pub enabled: Option<bool>,
    pub state: NativeState,
    /// Committed generation digest the observation was compared against.
    pub generation: Option<String>,
    /// Why the state holds, in one sentence.
    pub reason: String,
    /// The next step that resolves or tracks the state.
    pub action: String,
}

/// Route rank for merging: a fallback is the most informative route fact
/// and never reads as native success.
fn route_rank(route: &NativeRouteStatus) -> u8 {
    match route {
        NativeRouteStatus::Loose => 0,
        NativeRouteStatus::ObservedLoose => 1,
        NativeRouteStatus::Native => 2,
        NativeRouteStatus::Fallback { .. } => 3,
    }
}

impl NativeDiagnostic {
    /// Build a diagnostic from one read-only native observation.
    pub fn from_observation(
        client: ProjectionTargetId,
        identity: NativeIdentity,
        route: NativeRouteStatus,
        observation: NativeObservation,
        enabled: Option<bool>,
        generation: Option<String>,
    ) -> Self {
        let package = identity.package.as_str();
        let component = identity.component.as_str();
        let (state, reason, action) = match observation {
            NativeObservation::Unavailable => (
                NativeState::Unavailable,
                format!("NotRun: the {client} CLI is unavailable, so {package} was not read"),
                format!("make the {client} CLI available, then rerun `ccync doctor`"),
            ),
            NativeObservation::Absent => (
                NativeState::Absent,
                format!("no {client} registration occupies {component}"),
                "run `ccync sync` to register it".to_string(),
            ),
            NativeObservation::External => (
                NativeState::ForeignRegistration,
                format!("a {client} registration named {package} exists that ccync does not own"),
                "remove or rename the foreign registration in the client; ccync will not replace it"
                    .to_string(),
            ),
            NativeObservation::Disabled => (
                NativeState::Disabled,
                format!("{client} lists {package} as disabled; zero loads are expected"),
                "enable it in the client to load it, or leave it disabled".to_string(),
            ),
            NativeObservation::Broken => (
                NativeState::Broken,
                format!("{client} reports that {package} cannot load"),
                "fix the load error the client reports, then rerun `ccync sync`".to_string(),
            ),
            NativeObservation::Unreadable => (
                NativeState::Unreadable,
                format!("NotRun: the {client} native state for {package} could not be read"),
                format!("make the {client} plugin list readable, then rerun `ccync doctor`"),
            ),
            NativeObservation::Drifted => (
                NativeState::Drifted,
                format!("the owned {client} registration for {package} no longer matches its committed generation"),
                "run `ccync sync` to reconcile it; ccync does not overwrite the change silently"
                    .to_string(),
            ),
            NativeObservation::OwnedOld => (
                NativeState::OwnedOutdated,
                format!("the {client} registration for {package} matches an older committed generation"),
                "run `ccync sync`, then reload the client".to_string(),
            ),
            NativeObservation::Desired => (
                NativeState::RegisteredNotLoaded,
                format!("{client} registers {package} and it is enabled; the loader did not run and the view was not re-read, so it is not proven loaded"),
                format!("start {client} and confirm `{component}` is listed"),
            ),
        };
        Self {
            client,
            identity,
            route,
            enabled,
            state,
            generation,
            reason,
            action,
        }
    }

    /// Whether this diagnostic reports full success. Every native observation
    /// leaves the loader NotRun, so no diagnostic reports full success.
    pub fn is_full_success(&self) -> bool {
        false
    }

    /// Merge two diagnostics for the same package and client. The worse state
    /// and its reason win, so a failure survives any later evidence. The more
    /// severe route wins, so a fallback is never merged into native success.
    /// Disagreeing enabled states become unknown.
    pub fn merge(&self, other: &Self) -> Self {
        let (worse, better) = if other.state > self.state {
            (other, self)
        } else {
            (self, other)
        };
        let mut merged = worse.clone();
        merged.route = if route_rank(&other.route) > route_rank(&self.route) {
            other.route.clone()
        } else {
            self.route.clone()
        };
        merged.enabled = if self.enabled == other.enabled {
            self.enabled
        } else {
            None
        };
        merged.generation = worse
            .generation
            .clone()
            .or_else(|| better.generation.clone());
        merged
    }
}

/// One text line for a native diagnostic, with every fact under its own label.
pub fn native_diagnostic_line(diagnostic: &NativeDiagnostic) -> String {
    let management = diagnostic
        .identity
        .management_id
        .as_deref()
        .map(|id| format!(" (management ID {id})"))
        .unwrap_or_default();
    let route = match &diagnostic.route {
        NativeRouteStatus::Loose => "route loose".to_string(),
        NativeRouteStatus::ObservedLoose => "route loose, native capability observed".to_string(),
        NativeRouteStatus::Native => "route native".to_string(),
        NativeRouteStatus::Fallback { reason } => {
            format!("route fallback, loose route active: {reason}")
        }
    };
    let enabled = match diagnostic.enabled {
        Some(true) => "enabled",
        Some(false) => "disabled",
        None => "enabled state unknown",
    };
    let generation = diagnostic
        .generation
        .as_deref()
        .map(|digest| format!("generation {digest}"))
        .unwrap_or_else(|| "generation not recorded".to_string());
    format!(
        "{} package {}{management}; component {}; {route}; {enabled}; state {}; {generation}; reason: {}; action: {}",
        diagnostic.client,
        diagnostic.identity.package,
        diagnostic.identity.component,
        diagnostic.state.label(),
        diagnostic.reason,
        diagnostic.action,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn provider_diagnostic_distinguishes_managed_and_loader_not_run_without_secrets() {
        let lock = json!({"_providerLifecycle":{"schemaVersion":1,"committed":[{
            "target":"codex", "selector":"probe@ccync-marketplace", "pluginName":"probe-provider",
            "upstreamRevision":"rev-1", "generation":{"source":"probe-source","generation":"gen-1","packageDigest":"sha256:abc"}, "capabilities":["skill"]
        }], "token":"fixture-secret"}});
        let rows = provider_diagnostics(&lock);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider, "probe-provider");
        assert_eq!(rows[0].source_key, "probe-source");
        assert_eq!(rows[0].manager, "ccync");
        assert_eq!(rows[0].ownership, DiagnosticOwnership::Managed);
        assert_eq!(rows[0].verification_scope, VerificationScope::LoaderNotRun);
        assert!(!format!("{rows:?}").contains("fixture-secret"));
    }

    #[test]
    fn provider_diagnostic_returns_empty_on_missing_or_invalid_lifecycle_section() {
        let empty_lock = json!({});
        assert!(provider_diagnostics(&empty_lock).is_empty());

        let invalid_lock = json!({ "_providerLifecycle": "not-an-object" });
        assert!(provider_diagnostics(&invalid_lock).is_empty());

        let bad_schema = json!({ "_providerLifecycle": { "schemaVersion": 999 } });
        assert!(provider_diagnostics(&bad_schema).is_empty());
    }

    #[test]
    fn declared_managed_view_is_sorted_and_keeps_duplicate_origins() {
        let lock = json!({
            "_personalPlugins": [{ "pluginId": "shared", "source": "catalog:shared" }],
            "_adoptedItems": [{ "name": "shared", "sourceId": "claude-mcp" }]
        });
        let declared = json!({ "zeta": { "command": "z" }, "shared": { "url": "https://x" }, "alpha": { "command": "a" } });
        let declared = declared.as_object().unwrap();

        let view = build_managed_view_with_shadows_and_declared(&lock, Some(declared));
        assert_eq!(
            view.iter()
                .map(|item| (item.id.as_str(), item.origin))
                .collect::<Vec<_>>(),
            [
                ("alpha", Origin::Declared),
                ("shared", Origin::Declared),
                ("zeta", Origin::Declared),
                ("shared", Origin::Personal),
                ("shared", Origin::Adopted),
            ]
        );
        assert!(!view[1].shadowed);
        assert!(!view[3].shadowed);
        assert!(view[4].shadowed);

        let visible = build_managed_view_with_declared(&lock, Some(declared));
        assert_eq!(visible.iter().filter(|item| item.id == "shared").count(), 2);
        assert!(!visible.iter().any(|item| item.origin == Origin::Adopted));
    }

    #[test]
    fn declared_managed_view_declared_and_adopted_without_personal_does_not_shadow() {
        let lock = json!({
            "_adoptedItems": [{ "name": "dup", "sourceId": "claude-mcp" }]
        });
        let declared = json!({ "dup": { "command": "cmd" } });
        let declared = declared.as_object().unwrap();

        let visible = build_managed_view_with_declared(&lock, Some(declared));
        assert_eq!(
            visible
                .iter()
                .map(|item| (item.id.as_str(), item.origin, item.shadowed))
                .collect::<Vec<_>>(),
            [
                ("dup", Origin::Declared, false),
                ("dup", Origin::Adopted, false),
            ]
        );
    }

    #[test]
    fn no_declared_items_preserves_existing_order_and_behavior() {
        let lock = json!({
            "_personalPlugins": [{ "pluginId": "p" }],
            "_adoptedItems": [{ "name": "a", "sourceId": "claude-mcp" }]
        });
        assert_eq!(
            build_managed_view(&lock)
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["p", "a"]
        );
    }

    #[test]
    fn merges_personal_and_adopted_namespaces() {
        let lock = json!({
            "_personalPlugins": [
                { "pluginId": "p1", "source": "https://x/p1.git", "pin": "sha1" }
            ],
            "_adoptedItems": [
                { "name": "a1", "adoptedFrom": "claude", "version": "1.0.0", "sourceId": "claude-mcp" }
            ]
        });
        let view = build_managed_view(&lock);
        assert_eq!(view.len(), 2);
        let p1 = view.iter().find(|i| i.id == "p1").unwrap();
        assert_eq!(p1.origin, Origin::Personal);
        assert_eq!(p1.kind, ItemKind::Plugin);
        assert_eq!(p1.pin.as_deref(), Some("sha1"));
        assert!(!p1.shadowed);

        let a1 = view.iter().find(|i| i.id == "a1").unwrap();
        assert_eq!(a1.origin, Origin::Adopted);
        assert_eq!(a1.kind, ItemKind::McpServer);
        assert!(!a1.shadowed);
    }

    #[test]
    fn classifies_by_explicit_source_id_namespace() {
        let lock = json!({
            "_adoptedItems": [
                { "name": "plug@mkt", "sourceId": "claude-plugin", "adoptedFrom": "claude", "version": "1.0.0" },
                { "name": "srv", "sourceId": "claude-mcp", "adoptedFrom": "claude", "version": null },
                { "name": "skill-x", "sourceId": "claude-skill", "adoptedFrom": "claude", "version": null }
            ]
        });
        let view = build_managed_view(&lock);
        assert_eq!(view.len(), 3);
        assert_eq!(
            view.iter().find(|i| i.id == "plug@mkt").unwrap().kind,
            ItemKind::Plugin
        );
        assert_eq!(
            view.iter().find(|i| i.id == "srv").unwrap().kind,
            ItemKind::McpServer
        );
        assert_eq!(
            view.iter().find(|i| i.id == "skill-x").unwrap().kind,
            ItemKind::Skill
        );
    }

    #[test]
    fn personal_wins_duplicate_id_and_adopted_is_shadowed() {
        let lock = json!({
            "_personalPlugins": [
                { "pluginId": "dup", "source": "https://x/dup.git", "pin": "newsha" }
            ],
            "_adoptedItems": [
                { "name": "dup", "sourceId": "claude-plugin", "adoptedFrom": "claude", "version": "0.1.0", "source": "mkt:dup@0.1.0" }
            ]
        });

        // Visible view: personal wins, adopted duplicate is suppressed.
        let view = build_managed_view(&lock);
        assert_eq!(view.len(), 1);
        let dup = &view[0];
        assert_eq!(dup.origin, Origin::Personal);
        assert_eq!(dup.pin.as_deref(), Some("newsha"));

        // Shadow-inclusive view: the adopted duplicate is retained, marked shadowed.
        let with_shadows = build_managed_view_with_shadows(&lock);
        assert_eq!(with_shadows.len(), 2);
        let personal_dup = with_shadows
            .iter()
            .find(|i| i.origin == Origin::Personal)
            .unwrap();
        assert!(
            !personal_dup.shadowed,
            "personal entry itself is never shadowed"
        );
        let adopted_dup = with_shadows
            .iter()
            .find(|i| i.origin == Origin::Adopted)
            .unwrap();
        assert!(
            adopted_dup.shadowed,
            "adopted duplicate must be marked shadowed"
        );
    }

    #[test]
    fn missing_namespaces_do_not_panic() {
        // Completely empty object.
        let empty = json!({});
        assert!(build_managed_view(&empty).is_empty());
        assert!(build_managed_view_with_shadows(&empty).is_empty());

        // Only personal present.
        let only_personal = json!({
            "_personalPlugins": [{ "pluginId": "p", "source": "s" }]
        });
        assert_eq!(build_managed_view(&only_personal).len(), 1);

        // Only adopted present.
        let only_adopted = json!({
            "_adoptedItems": [{ "name": "a", "sourceId": "claude-mcp" }]
        });
        assert_eq!(build_managed_view(&only_adopted).len(), 1);

        // Namespaces present but wrong shape (not arrays) must not panic.
        let malformed = json!({
            "_personalPlugins": "not-an-array",
            "_adoptedItems": { "not": "an-array" }
        });
        assert!(build_managed_view(&malformed).is_empty());

        // Lockfile is a non-object Value entirely.
        let not_object = json!("just a string");
        assert!(build_managed_view(&not_object).is_empty());

        // Entries missing required id fields are skipped, not panicking.
        let missing_ids = json!({
            "_personalPlugins": [{ "source": "s" }],
            "_adoptedItems": [{ "sourceId": "claude-mcp" }]
        });
        assert!(build_managed_view(&missing_ids).is_empty());
    }

    #[test]
    fn held_field_is_read_for_personal_entries() {
        let lock = json!({
            "_personalPlugins": [
                { "pluginId": "held-one", "source": "s", "pin": "sha", "held": true },
                { "pluginId": "not-held", "source": "s", "pin": "sha" }
            ]
        });
        let view = build_managed_view(&lock);
        assert!(view.iter().find(|i| i.id == "held-one").unwrap().held);
        assert!(!view.iter().find(|i| i.id == "not-held").unwrap().held);
    }

    #[test]
    fn duplicate_ids_within_same_namespace_do_not_panic_or_dedup() {
        // Two personal entries sharing an id: cross-namespace dedup only
        // targets personal-vs-adopted collisions, so both survive here and
        // neither is marked shadowed.
        let lock = json!({
            "_personalPlugins": [
                { "pluginId": "same", "source": "https://x/a.git" },
                { "pluginId": "same", "source": "https://x/b.git" }
            ],
            "_adoptedItems": [
                { "name": "same-adopted", "sourceId": "claude-plugin" },
                { "name": "same-adopted", "sourceId": "claude-mcp" }
            ]
        });
        let view = build_managed_view(&lock);
        let personal_dups: Vec<_> = view.iter().filter(|i| i.id == "same").collect();
        assert_eq!(
            personal_dups.len(),
            2,
            "same-namespace personal duplicates are not deduped"
        );
        assert!(personal_dups.iter().all(|i| !i.shadowed));

        let adopted_dups: Vec<_> = view.iter().filter(|i| i.id == "same-adopted").collect();
        assert_eq!(
            adopted_dups.len(),
            2,
            "same-namespace adopted duplicates are not deduped"
        );
        assert!(adopted_dups.iter().all(|i| !i.shadowed));
    }

    #[test]
    fn unknown_source_id_defaults_to_plugin_and_missing_source_id_defaults_to_plugin() {
        let lock = json!({
            "_adoptedItems": [
                { "name": "weird", "sourceId": "totally-unrecognized-tag" },
                { "name": "no-tag" }
            ]
        });
        let view = build_managed_view(&lock);
        assert_eq!(
            view.iter().find(|i| i.id == "weird").unwrap().kind,
            ItemKind::Plugin
        );
        assert_eq!(
            view.iter().find(|i| i.id == "no-tag").unwrap().kind,
            ItemKind::Plugin
        );
    }

    #[test]
    fn one_namespace_malformed_does_not_suppress_the_other() {
        // Adopted namespace is malformed (object, not array); personal is
        // well-formed. The malformed namespace must degrade to empty without
        // affecting the valid namespace's entries.
        let lock = json!({
            "_personalPlugins": [
                { "pluginId": "still-here", "source": "s" }
            ],
            "_adoptedItems": { "not": "an-array" }
        });
        let view = build_managed_view(&lock);
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].id, "still-here");
    }

    #[test]
    fn old_lockfile_no_adopted_items_shows_personal_only() {
        // An older lockfile that has only `_personalPlugins` and no `_adoptedItems`
        // key must return the personal items and never panic.
        let lock = json!({
            "_personalPlugins": [
                { "pluginId": "legacy-plugin", "source": "https://github.com/user/legacy.git" }
            ]
        });
        let view = build_managed_view(&lock);
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].id, "legacy-plugin");
        assert_eq!(view[0].origin, Origin::Personal);
    }

    #[test]
    fn personal_pin_preserved_in_managed_view() {
        let lock = json!({
            "_personalPlugins": [
                { "pluginId": "pinned-plugin", "source": "https://github.com/user/pinned.git", "pin": "abc123def456xyz" }
            ]
        });
        let view = build_managed_view(&lock);
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].pin.as_deref(), Some("abc123def456xyz"));
    }

    #[test]
    fn personal_held_flag_preserved_in_managed_view() {
        let lock = json!({
            "_personalPlugins": [
                { "pluginId": "held-plugin", "source": "https://github.com/user/held.git", "held": true }
            ]
        });
        let view = build_managed_view(&lock);
        assert_eq!(view.len(), 1);
        assert!(view[0].held);
    }

    #[test]
    fn old_lockfile_adopted_item_without_baseline_version_is_visible() {
        // Old-format adopted entry has no `baselineVersion` field.
        // `build_managed_view` must still include it (origin=Adopted) and never panic.
        let lock = json!({
            "_adoptedItems": [
                { "name": "old-adopted-plugin", "adoptedFrom": "claude", "version": "1.0.0", "sourceId": "claude-plugin" }
            ]
        });
        let view = build_managed_view(&lock);
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].id, "old-adopted-plugin");
        assert_eq!(view[0].origin, Origin::Adopted);
    }
}
