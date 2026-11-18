//! `ccync doctor` — read-only health checks for CCYNC state.
//!
//! Checks canonical root freshness, runtime paths and links, and
//! plan lifecycle drift. Exit-code grading: warning-only → 0, any error → non-zero.
//!
//!
//! `ccync doctor --dry-run` must not modify any files.
//! missing projection or stale steward → error exit naming the fix target.
//! warning-only → exit 0; any error → exit non-zero.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::delivery::NativeRouteStatus;
use crate::managed_view::{
    LayoutEvidence, NativeDiagnostic, NativeIdentity, NativeState, StorageFaceEvidence,
};
use crate::plugin_native::{
    NativeCommandRunner, NativeExecutable, NativeRegistration, ProcessNativeRunner,
};
use ccync_foundation::paths::EffectiveLayout;
use ccync_foundation::provider::{NativeObservation, ProviderLifecycleLock};

// Doctor finding/report vocabulary + the `HealthCheck` trait now live in
// `ccync_foundation::health` (doctor dependency inversion). Re-export so
// `ccync_engine::doctor::*` and the in-module tests keep resolving; each domain
// implements `HealthCheck` and `cli` aggregates the trait objects.
pub use ccync_foundation::health::{DoctorFinding, DoctorReport, HealthCheck, Severity};

/// Options for the doctor command.
#[derive(Debug, Default)]
pub struct DoctorOptions {
    /// When `true`, do NOT modify any files (read-only mode).
    pub dry_run: bool,
    /// Include release-gate checks.
    pub release_gate: bool,
}

/// Run `ccync doctor` — check canonical root, runtime paths and links.
///
/// Always read-only when `dry_run` is true. Returns a report with findings.
/// The caller is responsible for printing and using `report.exit_code()`.
pub fn run_doctor(opts: &DoctorOptions) -> DoctorReport {
    let _ = opts.dry_run; // All checks are read-only; dry_run has no effect here.

    // Resolve the layout at call time for paths owned by this invocation.
    // Root validation selects legacy paths; other load failures leave reads closed.
    let layout = EffectiveLayout::load_or_legacy().ok();
    let canonical_root = canonical_root_path();

    // Assemble the checks as `HealthCheck` trait objects in the exact prior order.
    // Conditional checks (canonical-root-dependent, release-gate) are selected here;
    // each check's logic lives in its own `HealthCheck` impl.
    let checks: Vec<Box<dyn HealthCheck>> = vec![
        // Check 1: Canonical root existence (ccync content render target).
        Box::new(CanonicalRootCheck {
            canonical_root: canonical_root.clone(),
        }),
        // Check 2: provider state is recorded (`ccync init`).
        Box::new(MasterAdoptedCheck),
        // Check 3: AGY links (warning only, best-effort and never fatal).
        Box::new(AgyLinksCheck {
            root: ccync_foundation::paths::user_home().map(|home| home.join(".gemini")),
            lockfile: layout.as_ref().map(EffectiveLayout::build_lock_path),
        }),
        // Check 4: pre-existing vendor-native entries under adopt ownership.
        Box::new(VendorNativeAdoptedCheck),
        // Check 5: personal-catalog entries whose pinned cache dir renders nothing.
        Box::new(NoRenderContributionCheck),
        // Check 6: committed external-tool snapshots and lifecycle journal.
        Box::new(ToolArtifactCheck),
        Box::new(ProviderDiagnosticCheck),
        // Check 7: selected routes in the committed route record are published and owned.
        Box::new(RouteRecordCheck),
        // Check 7b: committed native packages, re-observed read-only.
        Box::new(NativeRouteCheck),
        // Check 8: user-authored MCP input is silent unless it is invalid.
        Box::new(UserMcpInputCheck { layout }),
    ];

    // The baked-binary / runtime-manifest checks were removed: ccync does not
    // bake a binary or CCYNC plugin manifests into the canonical root (its content
    // render is the managed-set skills/commands/agents + .mcp.json), so those
    // checks were false positives on a freshly-synced ccync canonical root.
    let _ = opts.release_gate;

    let mut report = DoctorReport::new();
    for check in &checks {
        report.findings.extend(check.check());
    }
    report
}

// ---------------------------------------------------------------------------
// Individual checks
// ---------------------------------------------------------------------------

struct CanonicalRootCheck {
    canonical_root: PathBuf,
}

impl HealthCheck for CanonicalRootCheck {
    fn name(&self) -> &str {
        "canonical-root"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        let mut findings = Vec::new();
        if !self.canonical_root.exists() {
            findings.push(DoctorFinding::error(
                format!(
                    "canonical root not found: {}",
                    self.canonical_root.display()
                ),
                "run `ccync init <claude|codex>` then `ccync sync` to create + project it",
            ));
        }
        // No required-subdirectory check: the ccync content render only
        // materializes the subtrees the managed set actually contains (a
        // master with no commands legitimately has no `commands/`), so a
        // missing subdir is not an error.
        findings
    }
}

/// Check that provider state is recorded (`ccync init`): committed provider
/// records, or a legacy `_adoptMaster` in the lockfile. Without either, `ccync
/// sync` has no provider state to publish.
struct MasterAdoptedCheck;

/// Read-only integrity check for committed external-tool artifacts.
struct ToolArtifactCheck;
struct ProviderDiagnosticCheck;

impl HealthCheck for ProviderDiagnosticCheck {
    fn name(&self) -> &str {
        "provider-diagnostics"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        let Some(path) = EffectiveLayout::load_or_legacy()
            .ok()
            .map(|layout| layout.build_lock_path())
        else {
            return vec![DoctorFinding::warning("provider diagnostics unavailable; ownership is ambiguous and loader validation NotRun")];
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            return vec![DoctorFinding::warning("provider lock state is unreadable; ownership is ambiguous and loader validation NotRun")];
        };
        // Committed provider records are published and owned facts, not faults.
        // Their loader state is NotRun until a loader check runs, and an
        // undetected loader never escalates to a warning. The route view in
        // `managed_view::route_evidence_line` shows those facts instead.
        match ccync_foundation::provider::ProviderLifecycleLock::from_lock(&value) {
            Ok(_) => Vec::new(),
            Err(_) => vec![DoctorFinding::warning("provider lifecycle state is ambiguous; ownership is unverified and loader validation NotRun")],
        }
    }
}

/// Checks the committed route record against the files and the lockfile.
///
/// An unreadable record is a warning. An absent record is not a finding: no
/// sync has recorded routes yet. A readable record yields the findings from
/// [`route_findings`].
struct RouteRecordCheck;

impl HealthCheck for RouteRecordCheck {
    fn name(&self) -> &str {
        "route-record"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        match recorded_routes(EffectiveLayout::load_or_legacy().ok().as_ref()) {
            Ok(routes) => {
                route_findings_with_home(&routes, ccync_foundation::paths::user_home().as_deref())
            }
            Err(error) => vec![DoctorFinding::warning(format!(
                "route record unreadable ({error}); rerun `ccync sync` to record routes again"
            ))],
        }
    }
}

/// Health findings for the selected routes that have an output path.
///
/// A selected route whose output is not published is an error, because the
/// output is missing or failed. A published route whose path ccync does not
/// own is an error, because the output is not proven managed. A route that
/// was not selected, or that has no output path, yields nothing. Loader
/// NotRun is never a finding: it is shown in the route evidence lines.
///
/// This does not check the combined fallback copies. Use
/// [`route_findings_with_home`] to check them under a home directory.
pub fn route_findings(routes: &[crate::managed_view::RouteEvidence]) -> Vec<DoctorFinding> {
    route_findings_with_home(routes, None)
}

/// [`route_findings`], plus the combined fallback check under `home`.
///
/// An unpublished selected route keeps its target's combined copy as the
/// fallback. When that copy is missing under `home`, one finding names the
/// target and the path, however many of its routes are unpublished. A route
/// that is unsupported needs no fallback, and a published route never checks
/// it. A `None` home skips the fallback check.
pub fn route_findings_with_home(
    routes: &[crate::managed_view::RouteEvidence],
    home: Option<&Path>,
) -> Vec<DoctorFinding> {
    let mut findings = Vec::new();
    let mut checked_fallbacks = BTreeSet::new();
    for route in routes {
        let Some(path) = &route.path else {
            continue;
        };
        if !route.selected {
            continue;
        }
        let output = format!(
            "required output {:?} {} for {} from source {} at {path}",
            route.kind, route.original_name, route.target, route.source_key,
        );
        if !route.published {
            findings.push(DoctorFinding::error(
                format!("{output} is not published"),
                route.repair_action(),
            ));
            let Some(home) = home else {
                continue;
            };
            if route.unsupported_reason.is_none() && checked_fallbacks.insert(route.target) {
                let fallback = crate::install::fallback_copy_path(home, route.target);
                if std::fs::symlink_metadata(&fallback).is_err() {
                    findings.push(DoctorFinding::error(
                        format!(
                            "{} combined fallback is missing at {}, so its unpublished routes have no fallback",
                            route.target,
                            fallback.display(),
                        ),
                        "restore the combined copy or fix the unpublished route, then run `ccync sync`",
                    ));
                }
            }
        } else if !route.owned {
            findings.push(DoctorFinding::error(
                format!("{output} is not owned by ccync"),
                "move or remove the unowned path, then run `ccync sync`",
            ));
        }
    }
    findings
}

/// The committed route record, with each route's publication and ownership
/// re-checked against the files and the lockfile. An absent record yields no
/// routes. A record that cannot be read is an error.
fn recorded_routes(
    layout: Option<&EffectiveLayout>,
) -> Result<Vec<crate::managed_view::RouteEvidence>, String> {
    let Some(path) = layout.map(EffectiveLayout::routes_path) else {
        return Ok(Vec::new());
    };
    let Some(routes) = crate::managed_view::read_route_record(&path)? else {
        return Ok(Vec::new());
    };
    Ok(
        match (
            ccync_foundation::paths::user_home(),
            layout.map(EffectiveLayout::build_lock_path),
        ) {
            (Some(home), Some(lockfile)) => {
                crate::delivery_apply::verify_recorded_routes(routes, &home, &lockfile)
            }
            _ => routes,
        },
    )
}

/// Route facts from the committed route record, one `route:` line per route,
/// re-checked against the files and the lockfile without writing anything.
///
/// Each line shows the origin, the route, the original name, the selection,
/// publication, ownership, and loader states, served-by-root and multi-root
/// discovery, and the next action. A route whose loader did not run reads
/// `loader NotRun`; that state is a fact, not a warning. No record yields no
/// lines.
pub fn route_evidence_lines() -> Result<Vec<String>, String> {
    let layout = EffectiveLayout::load_or_legacy().map_err(|error| error.to_string())?;
    let routes = recorded_routes(Some(&layout))?;
    let observations = NativeObservations::observe_committed(&ProcessNativeRunner);
    let mut lines = route_lines(&routes, &observations);
    lines.extend(
        layout_evidence(&layout)
            .map(|evidence| crate::managed_view::layout_evidence_lines(&evidence))?,
    );
    Ok(lines)
}

/// Route evidence lines that reuse native observations the caller already
/// computed. A doctor run that shares one [`NativeObservations`] between its
/// native-routes check and these lines observes each committed record once.
pub fn route_evidence_lines_with(observations: &NativeObservations) -> Result<Vec<String>, String> {
    let layout = EffectiveLayout::load_or_legacy().map_err(|error| error.to_string())?;
    let routes = recorded_routes(Some(&layout))?;
    let mut lines = route_lines(&routes, observations);
    lines.extend(
        layout_evidence(&layout)
            .map(|evidence| crate::managed_view::layout_evidence_lines(&evidence))?,
    );
    Ok(lines)
}

fn layout_evidence(layout: &EffectiveLayout) -> Result<LayoutEvidence, String> {
    use ccync_foundation::paths::EffectiveLayoutAuthority as Authority;
    use ccync_foundation::provider::ClientFaceSupportDisposition;
    use ccync_foundation::target::ClientFace;

    let roots = layout.storage_roots();
    let authority = layout.authority();
    let client_faces = [
        ClientFace::Claude,
        ClientFace::Codex,
        ClientFace::CopilotCli,
        ClientFace::CopilotVsCode,
        ClientFace::OpenCode,
        ClientFace::AgyCli,
        ClientFace::AgyIde,
        ClientFace::AgyGui,
    ];
    let faces = client_faces
        .into_iter()
        .map(|face| {
            let mut served = Vec::new();
            let mut incidental = Vec::new();
            let mut preserved = Vec::new();
            let mut fallback_roots = Vec::new();
            let mut blocked_roots = Vec::new();
            for root in crate::delivery::SHARED_ROOT_CONSUMERS {
                let root_path = PathBuf::from(root.root);
                if root.served_faces.contains(&face) {
                    served.push(root_path.clone());
                }
                if root.incidental_readers.contains(&face) {
                    incidental.push(root_path.clone());
                }
                if let Some((_, disposition)) = root
                    .face_support
                    .iter()
                    .find(|(supported_face, _)| supported_face == &face)
                {
                    match disposition {
                        ClientFaceSupportDisposition::Unsupported => {
                            preserved.push(root_path.clone());
                            fallback_roots.push(root.root);
                        }
                        ClientFaceSupportDisposition::Blocked => {
                            preserved.push(root_path);
                            blocked_roots.push(root.root);
                        }
                        ClientFaceSupportDisposition::Supported => {}
                    }
                }
            }
            StorageFaceEvidence {
                face: format!("{face:?}"),
                served,
                incidental,
                preserved,
                fallback_reason: (!fallback_roots.is_empty())
                    .then(|| format!("unsupported roots: {}", fallback_roots.join(", "))),
                block_reason: (!blocked_roots.is_empty())
                    .then(|| format!("blocked roots: {}", blocked_roots.join(", "))),
            }
        })
        .collect();

    let migration_phase = if let Ok(text) = std::fs::read_to_string(layout.migration_journal_path())
    {
        serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| {
                value
                    .get("stage")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "unknown".into())
    } else {
        match authority {
            Authority::Legacy => "legacy-active".into(),
            Authority::Fresh => "not-started".into(),
            Authority::Committed => "committed".into(),
            Authority::MutationBlocked => "unknown".into(),
        }
    };
    let historical_row_counts = historical_layout_row_counts(layout);
    let home = ccync_foundation::paths::user_home()
        .ok_or_else(|| "user home is unavailable".to_string())?;
    let retained_live_surfaces = [
        ".claude",
        ".codex",
        ".copilot",
        ".gemini",
        ".github",
        ".opencode",
        ".cursor",
        ".vscode",
        ".agents",
    ]
    .into_iter()
    .map(|surface| home.join(surface))
    .filter(|surface| std::fs::symlink_metadata(surface).is_ok())
    .collect();
    let safe_retry_actions = match authority {
        Authority::MutationBlocked => vec![
            "inspect the layout metadata and resolve its conflict before writing".into(),
            "rerun `ccync doctor` to refresh read-only evidence".into(),
        ],
        Authority::Legacy if layout.root_error().is_some() => {
            vec!["correct the storage root configuration, then rerun `ccync doctor`".into()]
        }
        _ => vec!["rerun `ccync doctor` to refresh read-only evidence".into()],
    };
    Ok(LayoutEvidence {
        authority,
        migration_phase,
        effective_roots: vec![
            ("portable".into(), roots.portable.clone()),
            ("config".into(), roots.config.clone()),
            ("state".into(), roots.state.clone()),
            ("cache".into(), roots.cache.clone()),
        ],
        faces,
        retained_live_surfaces,
        historical_row_counts,
        safe_retry_actions,
    })
}

fn historical_layout_row_counts(
    layout: &EffectiveLayout,
) -> std::collections::BTreeMap<String, usize> {
    let mut counts = std::collections::BTreeMap::new();
    if let Ok(text) = std::fs::read_to_string(layout.build_lock_path()) {
        if let Ok(lock) = serde_json::from_str::<serde_json::Value>(&text) {
            let rows = lock
                .pointer("/_ccyncProjection/historicalOwnership")
                .and_then(serde_json::Value::as_array)
                .map_or(0, Vec::len);
            if rows > 0 {
                counts.insert("historicalOwnership".into(), rows);
            }
        }
    }
    let historical_path = layout.state_base().join("mcp/historical-managed.json");
    if let Ok(text) = std::fs::read_to_string(historical_path) {
        if let Ok(rows) = serde_json::from_str::<Vec<serde_json::Value>>(&text) {
            if !rows.is_empty() {
                counts.insert("historicalManagedMcp".into(), rows.len());
            }
        }
    }
    counts
}

fn route_lines(
    routes: &[crate::managed_view::RouteEvidence],
    observations: &NativeObservations,
) -> Vec<String> {
    let mut lines: Vec<String> = routes
        .iter()
        .map(|route| format!("route: {}", crate::managed_view::route_evidence_line(route)))
        .collect();
    lines.extend(observations.evidence_lines());
    lines
}

/// The committed provider lifecycle section of `build/lock.json`, or `None`
/// when the file is absent or unreadable. Read-only.
fn committed_lock_value() -> Option<serde_json::Value> {
    let layout = EffectiveLayout::load_or_legacy().ok()?;
    let path = layout.build_lock_path();
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// The native CLI that owns a committed record, when ccync has a native adapter
/// for that client. Other clients keep their loose route and have no native
/// observation here.
fn native_executable(
    target: ccync_foundation::target::ProjectionTargetId,
) -> Option<NativeExecutable> {
    match target {
        ccync_foundation::target::ProjectionTargetId::Claude => Some(NativeExecutable::Claude),
        ccync_foundation::target::ProjectionTargetId::Codex => Some(NativeExecutable::Codex),
        _ => None,
    }
}

/// Fragment of the error text that `ProcessNativeRunner::run` returns when the
/// CLI executable cannot be started. The runner reports failures as text, so
/// doctor matches this fragment to tell a missing CLI from a failed listing.
const SPAWN_FAILURE_MARKER: &str = "could not start executable";

/// Observation for a committed record whose executable listing failed. A CLI
/// that cannot be started is `Unavailable`; any other failure, such as a
/// timeout or non-zero exit, leaves the native state `Unreadable`.
fn failed_listing_observation(message: &str) -> NativeObservation {
    if message.contains(SPAWN_FAILURE_MARKER) {
        NativeObservation::Unavailable
    } else {
        NativeObservation::Unreadable
    }
}

/// Classify one committed record against the registration its executable
/// reported. Returns the observation and the enabled state the client
/// reported. The view content is not re-read here, so an enabled registration
/// is never reported as loaded.
fn classify_registration(
    registration: &NativeRegistration,
    record: &ccync_foundation::provider::CommittedOwnership,
) -> (NativeObservation, Option<bool>) {
    if !registration.available {
        return (NativeObservation::Unavailable, None);
    }
    if !registration.present {
        return (NativeObservation::Absent, None);
    }
    if registration.broken {
        return (NativeObservation::Broken, Some(registration.enabled));
    }
    if registration.selector.as_ref() != Some(&record.selector) {
        return (NativeObservation::External, Some(registration.enabled));
    }
    if !registration.enabled {
        return (NativeObservation::Disabled, Some(false));
    }
    let reported_digest = registration.package_digest.as_deref();
    if reported_digest.is_some_and(|digest| digest != record.generation.package_digest.as_str()) {
        return (NativeObservation::Drifted, Some(true));
    }
    (NativeObservation::Desired, Some(true))
}

/// Warning for a `_providerLifecycle` section that is present but does not
/// parse under the current schema, such as a legacy or malformed shape. The
/// records cannot be read, so native state is unknown and no native route is
/// reported as success. An absent section is not a finding.
pub fn native_section_findings(lock: &serde_json::Value) -> Vec<DoctorFinding> {
    match ProviderLifecycleLock::from_lock(lock) {
        Ok(_) => Vec::new(),
        Err(_) => vec![DoctorFinding::warning(
            "native route state is unknown: the _providerLifecycle section of build/lock.json is legacy or unreadable, so native routes are NotRun and never full success; run `ccync sync` to rewrite the record",
        )],
    }
}

/// Native diagnostics for every committed Claude Code and Codex record in
/// `lock`. Each record is observed through `runner.observe` only, so the
/// result never comes from a mutating CLI call. When an executable's
/// observation fails, its remaining records take that failure without a
/// further call to the executable. A section that cannot be parsed yields no
/// diagnostics here, because there is no record to name;
/// [`native_section_findings`] reports that state as a warning.
pub fn native_diagnostics(
    runner: &dyn NativeCommandRunner,
    lock: &serde_json::Value,
) -> Vec<NativeDiagnostic> {
    let Ok(section) = ProviderLifecycleLock::from_lock(lock) else {
        return Vec::new();
    };
    let mut failures: HashMap<NativeExecutable, NativeObservation> = HashMap::new();
    section
        .committed
        .iter()
        .filter_map(|record| {
            let executable = native_executable(record.target)?;
            let (observation, enabled) = if let Some(&failure) = failures.get(&executable) {
                (failure, None)
            } else {
                match runner.observe(executable, &record.selector) {
                    Ok(registration) => classify_registration(&registration, record),
                    Err(message) => {
                        let failure = failed_listing_observation(&message);
                        failures.insert(executable, failure);
                        (failure, None)
                    }
                }
            };
            Some(NativeDiagnostic::from_observation(
                record.target,
                NativeIdentity {
                    package: record.plugin_name.clone(),
                    management_id: Some(record.generation.source.to_string()),
                    component: record.selector.to_string(),
                },
                NativeRouteStatus::Native,
                observation,
                enabled,
                Some(record.generation.package_digest.as_str().to_string()),
            ))
        })
        .collect()
}

/// Findings for native diagnostics. A blocked state is an error, a state that
/// needs a reload or sync or could not be read is a warning, and a
/// registration or a deliberate disable is not a finding.
pub fn native_findings(diagnostics: &[NativeDiagnostic]) -> Vec<DoctorFinding> {
    diagnostics
        .iter()
        .filter_map(|diagnostic| {
            let subject = format!(
                "native {} package {} ({})",
                diagnostic.client, diagnostic.identity.package, diagnostic.identity.component
            );
            match diagnostic.state {
                NativeState::Broken
                | NativeState::Drifted
                | NativeState::ForeignRegistration
                | NativeState::Absent => Some(DoctorFinding::error(
                    format!("{subject}: {}", diagnostic.reason),
                    diagnostic.action.clone(),
                )),
                NativeState::OwnedOutdated | NativeState::Unavailable | NativeState::Unreadable => {
                    Some(DoctorFinding::warning(format!(
                        "{subject}: {}; {}",
                        diagnostic.reason, diagnostic.action
                    )))
                }
                NativeState::RegisteredNotLoaded | NativeState::Disabled => None,
            }
        })
        .collect()
}

/// Read-only native observations of the committed provider records, computed
/// once per doctor run. The native-routes check and the native evidence lines
/// both read this value, so a run shares one observation instead of observing
/// each committed record again for every consumer.
#[derive(Debug)]
pub struct NativeObservations {
    section_findings: Vec<DoctorFinding>,
    diagnostics: Vec<NativeDiagnostic>,
}

impl NativeObservations {
    /// Observe the committed records of `lock` through `runner`. Read-only.
    pub fn observe(runner: &dyn NativeCommandRunner, lock: &serde_json::Value) -> Self {
        Self {
            section_findings: native_section_findings(lock),
            diagnostics: native_diagnostics(runner, lock),
        }
    }

    /// Observe the committed records of the lockfile under the ccync build
    /// root. An absent or unreadable lockfile observes nothing.
    pub fn observe_committed(runner: &dyn NativeCommandRunner) -> Self {
        match committed_lock_value() {
            Some(lock) => Self::observe(runner, &lock),
            None => Self {
                section_findings: Vec::new(),
                diagnostics: Vec::new(),
            },
        }
    }

    /// Per-record native diagnostics, in committed-record order.
    pub fn diagnostics(&self) -> &[NativeDiagnostic] {
        &self.diagnostics
    }

    /// Findings for the native-routes check: section warnings, then the
    /// per-record findings.
    pub fn findings(&self) -> Vec<DoctorFinding> {
        let mut findings = self.section_findings.clone();
        findings.extend(native_findings(&self.diagnostics));
        findings
    }

    /// Native evidence lines for the route evidence output.
    pub fn evidence_lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .section_findings
            .iter()
            .map(|finding| format!("native: {}", finding.message))
            .collect();
        lines.extend(self.diagnostics.iter().map(|diagnostic| {
            format!(
                "native: {}",
                crate::managed_view::native_diagnostic_line(diagnostic)
            )
        }));
        lines
    }
}

/// Read-only native check over the committed provider records.
struct NativeRouteCheck;

impl HealthCheck for NativeRouteCheck {
    fn name(&self) -> &str {
        "native-routes"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        NativeObservations::observe_committed(&ProcessNativeRunner).findings()
    }
}

/// Reports malformed user-authored MCP input without treating absence as a finding.
struct UserMcpInputCheck {
    layout: Option<EffectiveLayout>,
}

impl HealthCheck for UserMcpInputCheck {
    fn name(&self) -> &str {
        "user-mcp-input"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        let Some(layout) = &self.layout else {
            return Vec::new();
        };

        match crate::install::read_direct_mcp_input(layout) {
            Ok(_) => Vec::new(),
            Err(error) => vec![DoctorFinding::error(
                format!("user MCP input is invalid: {error}"),
                "correct the user MCP input file",
            )],
        }
    }
}

impl HealthCheck for ToolArtifactCheck {
    fn name(&self) -> &str {
        "tool-artifacts"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        use crate::tool_manager::{artifact_tree, store};

        let mut findings = Vec::new();
        match store::load_state() {
            Ok(records) => {
                for (id, record) in records {
                    let Some(generation) = record.committed_generation else {
                        continue;
                    };
                    let snapshot = EffectiveLayout::load_or_legacy().ok().map(|layout| {
                        layout
                            .tool_artifacts_root()
                            .join(id.as_str())
                            .join(generation.to_string())
                    });
                    let Some(snapshot) = snapshot else {
                        findings.push(DoctorFinding::error(
                            format!("committed tool snapshot path unavailable for {id}"),
                            "restore CCYNC home path configuration",
                        ));
                        continue;
                    };
                    if !snapshot.exists() {
                        findings.push(DoctorFinding::error(
                            format!(
                                "committed tool snapshot missing for {id}: {}",
                                snapshot.display()
                            ),
                            "run `ccync tool refresh` to create a new committed snapshot",
                        ));
                        continue;
                    }
                    match artifact_tree::scan_tree(&snapshot) {
                        Err(error) => findings.push(DoctorFinding::error(
                            format!("committed tool snapshot unreadable for {id}: {error}"),
                            "inspect the committed snapshot and restore its files",
                        )),
                        Ok(manifest) => {
                            let actual = format!("sha256:{}", manifest.sha256);
                            if record.artifact_fingerprint.as_deref() != Some(actual.as_str()) {
                                findings.push(DoctorFinding::error(
                                    format!(
                                        "committed tool snapshot changed for {id}: {}",
                                        snapshot.display()
                                    ),
                                    "run `ccync tool refresh` to reconcile the committed snapshot",
                                ));
                            }
                        }
                    }
                }
            }
            Err(error) => findings.push(DoctorFinding::error(
                format!("external-tool state unreadable: {error}"),
                "inspect the tool state file under CCYNC build tools",
            )),
        }

        if let Some(path) = EffectiveLayout::load_or_legacy()
            .ok()
            .map(|layout| layout.tool_operation_journal_path())
        {
            match std::fs::metadata(&path) {
                Ok(_) => findings.push(DoctorFinding::error(
                    format!(
                        "external-tool operation journal is active or recovery-required: {}",
                        path.display()
                    ),
                    "review the operation and use `ccync tool recover`",
                )),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => findings.push(DoctorFinding::error(
                    format!("external-tool operation journal unreadable: {error}"),
                    "inspect the operation journal under CCYNC build tools",
                )),
            }
        }
        findings
    }
}

impl HealthCheck for MasterAdoptedCheck {
    fn name(&self) -> &str {
        "master-adopted"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        let mut findings = Vec::new();
        let Some(lock_path) = EffectiveLayout::load_or_legacy()
            .ok()
            .map(|layout| layout.build_lock_path())
        else {
            return findings;
        };
        if !lock_path.is_file() {
            findings.push(DoctorFinding::warning(
                "no provider state recorded (lockfile absent) — run `ccync init <claude|codex>`",
            ));
            return findings;
        }
        let Some(lock) = std::fs::read_to_string(&lock_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        else {
            return findings;
        };
        // Origin-based: committed provider lifecycle records name their own
        // source, so a master is not required to have a truth source.
        if committed_provider_records(&lock) {
            return findings;
        }
        if lock.get("_adoptMaster").and_then(|m| m.as_str()).is_none() {
            findings.push(DoctorFinding::warning(
                "no provider state recorded (no committed provider records, no legacy _adoptMaster) — run `ccync init <claude|codex>`",
            ));
        }
        findings
    }
}

/// True when the lockfile holds at least one committed provider lifecycle record.
fn committed_provider_records(lock: &serde_json::Value) -> bool {
    ccync_foundation::provider::ProviderLifecycleLock::from_lock(lock)
        .map(|section| !section.committed.is_empty())
        .unwrap_or(false)
}

/// Checks all three AGY (Antigravity) whole-plugin roots: CLI, IDE, and GUI.
/// Warning-only and best-effort — symbolic links and *owned* directory copies
/// are healthy, while absent roots, legacy junctions, and real directories
/// ccync does not own need another sync or the user's attention.
///
/// The ownership half matters as much as the shape half: when a user's own
/// directory occupies an agy root, `publish_agy_root` preserves it and
/// publishes nothing, so that face is dark. Reporting it healthy would move
/// the invisible-surface failure this check exists to catch from the scanner
/// into the health check itself.
struct AgyLinksCheck {
    /// Injectable `.gemini` root so the health check can be tested directly.
    root: Option<PathBuf>,
    /// Injectable `lock.json` path — the ownership proof that separates a
    /// managed copy from a user's own directory at the same path.
    lockfile: Option<PathBuf>,
}

impl HealthCheck for AgyLinksCheck {
    fn name(&self) -> &str {
        "agy-links"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        let mut findings = Vec::new();
        let Some(root) = &self.root else {
            return findings;
        };

        let roots = [
            ("CLI", root.join("antigravity-cli/plugins/ccync")),
            ("IDE", root.join("antigravity-ide/plugins/ccync")),
            ("GUI", root.join("config/plugins/ccync")),
        ];
        let ownership = self
            .lockfile
            .as_ref()
            // Only a lockfile that exists *and* parses can disprove ownership.
            // `AgyRootOwnership::from_lockfile` tolerates both faults by owning
            // nothing, which reads identically to "ccync owns none of these
            // roots" — so without this filter, deleting `~/.ccync/build/` or a
            // truncated write would make doctor tell the user to remove three
            // healthy managed directories.
            .filter(|lock| {
                std::fs::read_to_string(lock)
                    .ok()
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                    .is_some_and(|value| value.is_object())
            })
            .map(|lock| projection::AgyRootOwnership::from_lockfile(lock));

        for (face, path) in roots {
            let shape = ccync_foundation::platform::classify_link(&path);
            let message = match shape {
                // Ordinary sync retires this combined plugin root once direct
                // publication replaces it, so an absent root is the expected
                // layout, not a missing one.
                ccync_foundation::platform::LinkShape::Absent => None,
                ccync_foundation::platform::LinkShape::Junction => Some(format!(
                    "AGY {face} root is a legacy junction at {}; run `ccync sync` to replace it with a symbolic link or managed copy",
                    path.display()
                )),
                ccync_foundation::platform::LinkShape::Directory => {
                    // Silent only when the copy is provably ccync's. With no
                    // lockfile to consult, ownership cannot be disproved
                    // either, so stay silent rather than warn on every copy.
                    match &ownership {
                        Some(ownership) if !ownership.is_owned(&path) => Some(format!(
                            "AGY {face} root at {} is a directory ccync does not own; it was preserved rather than published, so this face is not live — move or remove it, then run `ccync sync`",
                            path.display()
                        )),
                        _ => None,
                    }
                }
                ccync_foundation::platform::LinkShape::Symlink => None,
            };
            if let Some(message) = message {
                findings.push(DoctorFinding::warning(message));
            }
        }
        findings
    }
}

/// Check the lockfile's `_adoptedItems` and `_mcpServers` namespaces for names
/// that collide with the recorded master's [`crate::adopt::vendor_native_names`]
/// list — i.e. entries adopted before that name was surveyed as vendor-native.
/// Report only, per adopt's own read-boundary contract: this check never
/// rewrites the lockfile, it only points at `ccync remove <name>` for the
/// human to run. No `_adoptMaster` recorded yields no finding here — that gap
/// is already reported by [`MasterAdoptedCheck`], not duplicated.
struct VendorNativeAdoptedCheck;

impl HealthCheck for VendorNativeAdoptedCheck {
    fn name(&self) -> &str {
        "vendor-native-adopted"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        let mut findings = Vec::new();
        let Some(lock_path) = EffectiveLayout::load_or_legacy()
            .ok()
            .map(|layout| layout.build_lock_path())
        else {
            return findings;
        };
        if !lock_path.is_file() {
            return findings;
        }
        let Some(lock) = std::fs::read_to_string(&lock_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        else {
            return findings;
        };
        let Some(master) = lock.get("_adoptMaster").and_then(|v| v.as_str()) else {
            return findings;
        };
        let agent = match master {
            "claude" => crate::adopt::AgentKind::Claude,
            "codex" => crate::adopt::AgentKind::Codex,
            _ => return findings,
        };
        let vendor_names = crate::adopt::vendor_native_names(&agent);
        if vendor_names.is_empty() {
            return findings;
        }

        let mut hit_names: Vec<String> = Vec::new();
        if let Some(items) = lock.get("_adoptedItems").and_then(|v| v.as_array()) {
            for item in items {
                if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
                    if vendor_names.contains(&name) && !hit_names.iter().any(|n| n == name) {
                        hit_names.push(name.to_string());
                    }
                }
            }
        }
        if let Some(servers) = lock.get("_mcpServers").and_then(|v| v.as_object()) {
            for name in servers.keys() {
                if vendor_names.contains(&name.as_str()) && !hit_names.iter().any(|n| n == name) {
                    hit_names.push(name.clone());
                }
            }
        }

        if !hit_names.is_empty() {
            let names = hit_names.join(", ");
            findings.push(DoctorFinding::warning(format!(
                "pre-existing vendor-native entries adopted under {master}: {names} — run `ccync remove <name>` for each to release them"
            )));
        }
        findings
    }
}

/// Check every personal-catalog entry's pinned cache dir for render
/// contribution. A pinned entry whose cache dir exists yet renders neither a
/// component nor an MCP server is invisible to every other check — it never
/// fails a read, it just contributes nothing — so this walks the catalog and
/// calls [`no_contribution_finding`] once per entry. An absent or unparsable
/// catalog or lockfile yields no findings here, mirroring the [`AgyLinksCheck`]
/// ownership-filter rule: a failed read must never be treated as a positive
/// result (i.e. "nothing to warn about").
struct NoRenderContributionCheck;

impl HealthCheck for NoRenderContributionCheck {
    fn name(&self) -> &str {
        "render-contribution"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        let mut findings = Vec::new();

        let Some(layout) = EffectiveLayout::load_or_legacy().ok() else {
            return findings;
        };
        let catalog_path = layout.personal_catalog_path();
        let Some(catalog_entries) = std::fs::read_to_string(&catalog_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("plugins").and_then(|p| p.as_array()).cloned())
        else {
            return findings;
        };

        let lock_path = layout.build_lock_path();
        let Some(lock_entries) = std::fs::read_to_string(&lock_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| {
                v.get("_personalPlugins")
                    .and_then(|p| p.as_array())
                    .cloned()
            })
        else {
            return findings;
        };

        let cache_root = layout.cache_path();

        for entry in &catalog_entries {
            if let Some(finding) = no_contribution_finding(entry, &lock_entries, &cache_root) {
                findings.push(finding);
            }
        }
        findings
    }
}

/// Pure per-entry seam mirroring `setup::health::personal_plugin_finding`'s
/// shape: resolves the catalog entry's pin from the lockfile only, resolves
/// the pinned cache dir, and reports a finding only when that cache dir
/// exists yet contributes neither a component nor an MCP server to the
/// render. An unpinned entry or an absent cache dir stays silent here —
/// those gaps belong to other checks.
fn no_contribution_finding(
    catalog_entry: &serde_json::Value,
    lock_entries: &[serde_json::Value],
    cache_root: &std::path::Path,
) -> Option<DoctorFinding> {
    let id = catalog_entry.get("pluginId").and_then(|v| v.as_str())?;
    let pin = lock_entries
        .iter()
        .find(|e| e.get("pluginId").and_then(|v| v.as_str()) == Some(id))
        .and_then(ccync_foundation::pins::entry_pin)?;

    let plugin_dir = cache_root.join(format!("{id}@{pin}"));
    if !plugin_dir.is_dir() {
        return None;
    }

    if crate::install::contributes_to_render(id, &plugin_dir) {
        return None;
    }

    Some(DoctorFinding {
        severity: Severity::Warning,
        message: format!(
            "personal plugin '{id}' at {} contributes no component and no MCP server",
            plugin_dir.display()
        ),
        fix_hint: Some(
            "the source is not a plugin, skill, or MCP payload, or the running binary \
             predates the payload-shape support that source needs"
                .to_string(),
        ),
    })
}

fn canonical_root_path() -> PathBuf {
    // The canonical render root the install/sync engine actually writes
    // (`~/.ccync/build/render`). Resolve it from the shared foundation helper so
    // doctor never drifts from the layout used by `install::run_update`; the old
    // hardcoded `~/.ccync/plugins/ccync` path predates the plugins→build layout
    // migration and is never created anymore.
    ccync_foundation::paths::canonical_plugin_root().unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_user_mcp_input_is_silent_when_path_is_none() {
        let check = UserMcpInputCheck { layout: None };
        assert!(check.check().is_empty());
    }

    #[test]
    fn doctor_user_mcp_input_is_silent_for_absent_and_valid_files() {
        let (directory, _guard, prior) = tool_check_home();
        let path = directory.path().join(".ccync/mcp.json");
        let check = UserMcpInputCheck {
            layout: Some(EffectiveLayout::load_or_legacy().unwrap()),
        };

        assert!(check.check().is_empty());
        assert!(!path.exists(), "doctor must not create absent input");

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let valid_samples = [
            r#"{"mcpServers":{"demo":{"command":"node","args":["server.js"]}}}"#,
            r#"{"mcpServers":{"remote":{"url":"https://example.com/sse","type":"sse"}}}"#,
            r#"{"mcpServers":{}}"#,
        ];
        for valid in valid_samples {
            std::fs::write(&path, valid).unwrap();
            assert!(
                check.check().is_empty(),
                "expected silence for valid input: {valid}"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), valid);
        }
        restore_tool_check_home(prior);
    }

    #[test]
    fn doctor_user_mcp_input_reports_invalid_file_as_error_read_only() {
        let (directory, _guard, prior) = tool_check_home();
        let path = directory.path().join(".ccync/mcp.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let invalid_samples = [
            r#"{"mcpServers":{"demo":{"command":7}}}"#,
            r#"{"mcpServers":{broken"#,
            r#"[]"#,
            r#"{"mcpServers":{"demo":{}}}"#,
            r#"{"mcpServers":{"demo":{"command":"node","url":"https://example.com"}}}"#,
            r#"{"mcpServers":{"demo":{"command":"node","type":"http"}}}"#,
            r#"{"mcpServers":{"demo":{"command":"node","type":"unknown"}}}"#,
        ];

        for invalid in invalid_samples {
            std::fs::write(&path, invalid).unwrap();
            let check = UserMcpInputCheck {
                layout: Some(EffectiveLayout::load_or_legacy().unwrap()),
            };

            let findings = check.check();

            assert_eq!(
                findings.len(),
                1,
                "expected exactly 1 finding for {invalid}"
            );
            assert_eq!(
                findings[0].severity,
                Severity::Error,
                "expected Severity::Error for {invalid}"
            );
            assert!(findings[0].message.contains("user MCP input is invalid"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
        }
        restore_tool_check_home(prior);
    }

    #[test]
    fn run_doctor_includes_user_mcp_input_check() {
        let (home, _guard, prior) = tool_check_home();
        let mcp_path = home.path().join(".ccync/mcp.json");
        std::fs::create_dir_all(mcp_path.parent().unwrap()).unwrap();
        let invalid = r#"{"mcpServers":{"demo":{"command":123}}}"#;
        std::fs::write(&mcp_path, invalid).unwrap();

        let opts = DoctorOptions::default();
        let report = run_doctor(&opts);

        restore_tool_check_home(prior);

        let mcp_finding = report
            .findings
            .iter()
            .find(|f| f.message.contains("user MCP input is invalid"));
        assert!(
            mcp_finding.is_some(),
            "run_doctor must include invalid user MCP input finding"
        );
        assert_eq!(mcp_finding.unwrap().severity, Severity::Error);
        assert_eq!(
            std::fs::read_to_string(&mcp_path).unwrap(),
            invalid,
            "run_doctor must be read-only"
        );
    }

    fn tool_check_home() -> (
        tempfile::TempDir,
        std::sync::MutexGuard<'static, ()>,
        Option<std::ffi::OsString>,
    ) {
        let guard = crate::test_support::env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        let home = tempfile::tempdir().unwrap();
        std::env::set_var(var, home.path());
        (home, guard, prior)
    }

    fn restore_tool_check_home(prior: Option<std::ffi::OsString>) {
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        match prior {
            Some(value) => std::env::set_var(var, value),
            None => std::env::remove_var(var),
        }
    }

    fn write_tool_state(home: &std::path::Path, fingerprint: &str) {
        let path = home.join(".ccync/build/tools/state.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!(r#"{{"version":1,"records":{{"graphify":{{"committedGeneration":1,"artifactFingerprint":"{fingerprint}","sourceEvidence":{{}},"operationId":null,"facets":{{"evidenceLevel":"verified","selectionState":"unique","managerAvailability":"available","installState":"installed","packageState":"matches-artifact-source","updateState":"not-checked","artifactState":"current","projectionState":"current","holdPolicy":"unheld","managerPinState":"unknown","operationState":"none","lastCheck":null}}}}}}}}"#)).unwrap();
    }

    #[test]
    fn tool_artifact_check_is_clean_without_committed_tools_or_journal() {
        let (home, _guard, prior) = tool_check_home();
        let snapshot = home.path().join(".ccync/build/tools/artifacts/graphify/1");
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::write(snapshot.join("artifact.txt"), "committed").unwrap();
        let manifest = crate::tool_manager::artifact_tree::scan_tree(&snapshot).unwrap();
        write_tool_state(home.path(), &format!("sha256:{}", manifest.sha256));
        let findings = ToolArtifactCheck.check();
        restore_tool_check_home(prior);
        assert!(findings.is_empty(), "unexpected findings: {findings:?}");
        drop(home);
    }

    #[test]
    fn tool_artifact_check_reports_missing_snapshot() {
        let (home, _guard, prior) = tool_check_home();
        write_tool_state(home.path(), "sha256:expected");
        let findings = ToolArtifactCheck.check();
        restore_tool_check_home(prior);
        assert!(findings
            .iter()
            .any(|f| f.message.contains("snapshot missing")));
        drop(home);
    }

    #[test]
    fn tool_artifact_check_reports_changed_snapshot() {
        let (home, _guard, prior) = tool_check_home();
        let snapshot = home.path().join(".ccync/build/tools/artifacts/graphify/1");
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::write(snapshot.join("artifact.txt"), "changed").unwrap();
        write_tool_state(home.path(), "sha256:expected");
        let findings = ToolArtifactCheck.check();
        restore_tool_check_home(prior);
        assert!(findings
            .iter()
            .any(|f| f.message.contains("snapshot changed")));
        drop(home);
    }

    #[test]
    fn tool_artifact_check_reports_unreadable_snapshot() {
        let (home, _guard, prior) = tool_check_home();
        let snapshot = home.path().join(".ccync/build/tools/artifacts/graphify/1");
        std::fs::create_dir_all(snapshot.parent().unwrap()).unwrap();
        std::fs::write(&snapshot, "not a directory").unwrap();
        write_tool_state(home.path(), "sha256:expected");
        let findings = ToolArtifactCheck.check();
        restore_tool_check_home(prior);
        assert!(findings
            .iter()
            .any(|f| f.message.contains("snapshot unreadable")));
        drop(home);
    }

    #[test]
    fn tool_artifact_check_reports_unreadable_state_and_journal() {
        let (home, _guard, prior) = tool_check_home();
        let state = home.path().join(".ccync/build/tools/state.json");
        std::fs::create_dir_all(state.parent().unwrap()).unwrap();
        std::fs::write(&state, "not json").unwrap();
        std::fs::write(
            home.path().join(".ccync/build/tools/operation.txn.json"),
            "{}",
        )
        .unwrap();
        let findings = ToolArtifactCheck.check();
        restore_tool_check_home(prior);
        assert!(findings
            .iter()
            .any(|f| f.message.contains("state unreadable")));
        assert!(findings
            .iter()
            .any(|f| f.message.contains("journal is active or recovery-required")));
        drop(home);
    }

    #[test]
    fn tool_artifact_check_is_clean_without_state_file() {
        let (home, _guard, prior) = tool_check_home();
        let findings = ToolArtifactCheck.check();
        restore_tool_check_home(prior);
        assert!(
            findings.is_empty(),
            "expected no findings for absent state and journal: {findings:?}"
        );
        drop(home);
    }

    #[test]
    fn tool_artifact_check_reports_active_journal_with_valid_snapshot() {
        let (home, _guard, prior) = tool_check_home();
        let snapshot = home.path().join(".ccync/build/tools/artifacts/graphify/1");
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::write(snapshot.join("artifact.txt"), "committed").unwrap();
        let manifest = crate::tool_manager::artifact_tree::scan_tree(&snapshot).unwrap();
        write_tool_state(home.path(), &format!("sha256:{}", manifest.sha256));
        std::fs::write(
            home.path().join(".ccync/build/tools/operation.txn.json"),
            "{}",
        )
        .unwrap();
        let findings = ToolArtifactCheck.check();
        restore_tool_check_home(prior);
        assert_eq!(
            findings.len(),
            1,
            "expected only the journal finding: {findings:?}"
        );
        assert!(findings[0]
            .message
            .contains("journal is active or recovery-required"));
        drop(home);
    }

    #[test]
    fn run_doctor_aggregates_tool_artifact_check() {
        let (home, _guard, prior) = tool_check_home();
        write_tool_state(home.path(), "sha256:expected");
        let report = run_doctor(&DoctorOptions::default());
        restore_tool_check_home(prior);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.message.contains("committed tool snapshot missing")),
            "expected run_doctor to include tool artifact check findings: {:?}",
            report.findings
        );
        drop(home);
    }

    #[test]
    fn doctor_finding_display_includes_severity_and_message() {
        let f = DoctorFinding::error("canonical root missing", "run `ccync sync`");
        let s = f.to_string();
        assert!(s.contains("ERROR"), "display should include ERROR: {s}");
        assert!(
            s.contains("canonical root missing"),
            "display should include message: {s}"
        );
        assert!(
            s.contains("ccync sync"),
            "display should include fix hint: {s}"
        );
    }

    #[test]
    fn doctor_finding_warning_has_no_fix_hint() {
        let f = DoctorFinding::warning("AGY link missing");
        let s = f.to_string();
        assert!(s.contains("WARNING"), "display should include WARNING: {s}");
        assert!(f.fix_hint.is_none());
    }

    #[test]
    fn report_exit_code_zero_when_only_warnings() {
        let mut report = DoctorReport::new();
        report.push(DoctorFinding::warning("some warning"));
        assert_eq!(report.exit_code(), 0, "warning-only should exit 0");
        assert!(!report.has_errors());
    }

    #[test]
    fn report_exit_code_nonzero_when_error_present() {
        let mut report = DoctorReport::new();
        report.push(DoctorFinding::error("critical failure", "fix it"));
        assert_eq!(report.exit_code(), 1, "error should exit non-zero");
        assert!(report.has_errors());
    }

    #[test]
    fn report_exit_code_zero_when_empty() {
        let report = DoctorReport::new();
        assert_eq!(report.exit_code(), 0);
        assert!(!report.has_errors());
    }

    #[test]
    fn dry_run_option_is_constructible() {
        let opts = DoctorOptions {
            dry_run: true,
            release_gate: false,
        };
        // run_doctor is read-only anyway; dry_run is a no-op for checks.
        // This test verifies the struct is usable.
        assert!(opts.dry_run);
    }

    #[test]
    fn run_doctor_returns_report_without_panic() {
        // Doctor may find errors (canonical root missing on CI/test machine),
        // but must not panic regardless of filesystem state.
        let opts = DoctorOptions::default();
        let report = run_doctor(&opts);
        // We don't assert exit code here — it depends on filesystem state.
        // We only verify no panic and findings is a Vec.
        let _ = report.findings.len();
        let _ = report.exit_code();
    }

    // ─── retired combined link is not a health requirement ───

    #[test]
    fn run_doctor_never_requires_the_retired_claude_skill_link() {
        use crate::test_support::env_guard;
        use tempfile::TempDir;

        let _guard = env_guard();
        let fake_home = TempDir::new().unwrap();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        std::env::set_var(var, fake_home.path());

        let report = run_doctor(&DoctorOptions::default());

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }

        assert!(
            !report
                .findings
                .iter()
                .any(|f| f.message.contains("skills/ccync") || f.message.contains("skill path")),
            "an absent retired combined link must not be a finding: {:?}",
            report.findings
        );
    }

    // ─── agy-links doctor tests ───────────────────────────────

    #[test]
    fn check_agy_links_is_silent_for_retired_absent_roots() {
        use tempfile::TempDir;

        // Ordinary sync retires the AGY plugin roots, so none of them being
        // present is the expected layout and nothing is reported.
        let root = TempDir::new().unwrap();
        let findings = AgyLinksCheck {
            root: Some(root.path().to_path_buf()),
            lockfile: None,
        }
        .check();

        assert!(findings.is_empty(), "{findings:?}");
    }

    /// Seed real ownership rows through the production write path, so the
    /// check is driven by the same lockfile shape `publish_agy_root` writes.
    fn seed_agy_ownership(root: &std::path::Path, lockfile: &std::path::Path) {
        let ownership = projection::AgyRootOwnership::from_lockfile(lockfile);
        for relative in [
            "antigravity-cli/plugins/ccync",
            "antigravity-ide/plugins/ccync",
            "config/plugins/ccync",
        ] {
            ownership.record_now(&root.join(relative), false).unwrap();
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn doctor_recognizes_legacy_key_and_preserves_user_file() {
        use sha2::{Digest, Sha256};
        let root = tempfile::tempdir().unwrap();
        let managed = root.path().join("antigravity-cli/plugins/ccync");
        let user = root.path().join("antigravity-ide/plugins/ccync/user.txt");
        std::fs::create_dir_all(&managed).unwrap();
        std::fs::write(managed.join("managed.txt"), "managed").unwrap();
        std::fs::create_dir_all(user.parent().unwrap()).unwrap();
        std::fs::write(&user, "user content").unwrap();
        let before = Sha256::digest(std::fs::read(&user).unwrap());
        let lock = root.path().join("lock.json");
        std::fs::write(&lock, serde_json::json!({"_ccyncProjection": {"agyRootProjectionPaths": [managed.to_string_lossy().replace('/', "\\")]}}).to_string()).unwrap();
        let findings = AgyLinksCheck { root: Some(root.path().to_path_buf()), lockfile: Some(lock) }.check();
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].message.contains("IDE"));
        assert!(managed.join("managed.txt").exists());
        assert_eq!(before, Sha256::digest(std::fs::read(&user).unwrap()));
    }

    #[test]
    fn check_agy_links_is_silent_for_owned_directory_copies() {
        use tempfile::TempDir;

        let root = TempDir::new().unwrap();
        let lockfile = root.path().join("lock.json");
        for relative in [
            "antigravity-cli/plugins/ccync",
            "antigravity-ide/plugins/ccync",
            "config/plugins/ccync",
        ] {
            std::fs::create_dir_all(root.path().join(relative)).unwrap();
        }
        seed_agy_ownership(root.path(), &lockfile);

        assert!(AgyLinksCheck {
            root: Some(root.path().to_path_buf()),
            lockfile: Some(lockfile),
        }
        .check()
        .is_empty());
    }

    #[test]
    fn check_agy_links_warns_for_unowned_directories_at_agy_roots() {
        use tempfile::TempDir;

        // Same on-disk shape as the owned case above — real directories at all
        // three roots — but no ownership row. `publish_agy_root` preserves such
        // a path and publishes nothing, so the face is dark and the check must
        // say so rather than report health.
        let root = TempDir::new().unwrap();
        let lockfile = root.path().join("lock.json");
        for relative in [
            "antigravity-cli/plugins/ccync",
            "antigravity-ide/plugins/ccync",
            "config/plugins/ccync",
        ] {
            std::fs::create_dir_all(root.path().join(relative)).unwrap();
        }
        // A real, parseable lockfile that owns *something* — just not these
        // roots. An absent lockfile is the separate "ownership unprovable"
        // case and must stay silent; this case is a positive disproof.
        projection::AgyRootOwnership::from_lockfile(&lockfile)
            .record_now(&root.path().join("some/other/root"), false)
            .unwrap();

        let findings = AgyLinksCheck {
            root: Some(root.path().to_path_buf()),
            lockfile: Some(lockfile),
        }
        .check();

        assert_eq!(findings.len(), 3);
        assert!(findings
            .iter()
            .all(|finding| finding.severity == Severity::Warning));
        assert!(findings
            .iter()
            .all(|finding| finding.message.contains("ccync does not own")));
    }

    #[test]
    fn check_agy_links_stays_silent_for_directories_when_the_lockfile_is_missing_or_malformed() {
        use tempfile::TempDir;

        // The production wiring always supplies a lockfile *path* (it is pure
        // path construction), so "no lockfile" reaches this check as a path
        // that does not exist or does not parse — not as `None`. Both must
        // read as "ownership unprovable", never as "ccync owns nothing".
        for seed in [None, Some("{ not json")] {
            let root = TempDir::new().unwrap();
            let lockfile = root.path().join("lock.json");
            if let Some(raw) = seed {
                std::fs::write(&lockfile, raw).unwrap();
            }
            for relative in [
                "antigravity-cli/plugins/ccync",
                "antigravity-ide/plugins/ccync",
                "config/plugins/ccync",
            ] {
                std::fs::create_dir_all(root.path().join(relative)).unwrap();
            }

            let findings = AgyLinksCheck {
                root: Some(root.path().to_path_buf()),
                lockfile: Some(lockfile),
            }
            .check();

            assert!(
                findings.is_empty(),
                "an unreadable lockfile must not turn healthy managed roots into \
                 'remove this directory' advice (seed: {seed:?}): {findings:?}"
            );
        }
    }

    #[test]
    fn check_agy_links_stays_silent_for_directories_when_no_lockfile_is_available() {
        use tempfile::TempDir;

        // Ownership cannot be disproved without a lockfile, so an unprovable
        // copy must not be warned about — that would fire on every root of a
        // healthy install whose lockfile path could not be resolved.
        let root = TempDir::new().unwrap();
        for relative in [
            "antigravity-cli/plugins/ccync",
            "antigravity-ide/plugins/ccync",
            "config/plugins/ccync",
        ] {
            std::fs::create_dir_all(root.path().join(relative)).unwrap();
        }

        assert!(AgyLinksCheck {
            root: Some(root.path().to_path_buf()),
            lockfile: None,
        }
        .check()
        .is_empty());
    }

    #[test]
    fn check_agy_links_is_silent_for_symbolic_links() {
        use tempfile::TempDir;

        let root = TempDir::new().unwrap();
        let target = root.path().join("canonical");
        std::fs::create_dir(&target).unwrap();
        for relative in [
            "antigravity-cli/plugins/ccync",
            "antigravity-ide/plugins/ccync",
            "config/plugins/ccync",
        ] {
            let path = root.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            if ccync_foundation::platform::create_symlink_dir(&target, &path).is_err() {
                return;
            }
        }

        assert!(AgyLinksCheck {
            root: Some(root.path().to_path_buf()),
            lockfile: None,
        }
        .check()
        .is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn check_agy_links_warns_for_legacy_junctions() {
        use tempfile::TempDir;

        let root = TempDir::new().unwrap();
        let target = root.path().join("canonical");
        std::fs::create_dir(&target).unwrap();
        for relative in [
            "antigravity-cli/plugins/ccync",
            "antigravity-ide/plugins/ccync",
            "config/plugins/ccync",
        ] {
            let path = root.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let command = format!(
                "New-Item -ItemType Junction -Path '{}' -Target '{}' | Out-Null",
                path.display(),
                target.display()
            );
            assert!(std::process::Command::new("powershell")
                .args(["-NoProfile", "-Command", &command])
                .status()
                .unwrap()
                .success());
        }

        let findings = AgyLinksCheck {
            root: Some(root.path().to_path_buf()),
            lockfile: None,
        }
        .check();
        assert_eq!(findings.len(), 3);
        assert!(findings
            .iter()
            .all(|finding| finding.message.contains("legacy junction")));
    }

    #[test]
    fn agy_links_check_id_and_struct_are_renamed() {
        let check = AgyLinksCheck {
            root: None,
            lockfile: None,
        };
        assert_eq!(check.name(), "agy-links");
    }

    // ─── vendor-native-adopted doctor tests ───────────────────────────────

    /// Point the process-wide home env var (`USERPROFILE`/`HOME`) at `fake_home`
    /// and write `lock_json` to the resolved `plugins_lock_path()` (mirrors how
    /// `skill_path_check_follows_injected_env_home` injects home for path tests).
    fn write_injected_lock(fake_home: &std::path::Path, lock_json: &str) {
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        std::env::set_var(var, fake_home);
        let layout = EffectiveLayout::load_or_legacy().unwrap();
        let lock_path = layout.build_lock_path();
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        std::fs::write(&lock_path, lock_json).unwrap();
    }

    #[test]
    fn vendor_native_adopted_check_warns_on_codex_hit_in_adopted_items() {
        use crate::test_support::env_guard;
        use tempfile::TempDir;

        let _guard = env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        let fake_home = TempDir::new().unwrap();
        write_injected_lock(
            fake_home.path(),
            r#"{"_adoptMaster":"codex","_adoptedItems":[{"name":"node_repl","adoptedFrom":"codex","sourceId":"codex-mcp"}]}"#,
        );

        let findings = VendorNativeAdoptedCheck.check();

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }

        assert_eq!(findings.len(), 1);
        let msg = &findings[0].message;
        assert!(msg.contains("node_repl"), "message: {msg}");
        assert!(msg.contains("ccync remove"), "message: {msg}");
    }

    #[test]
    fn vendor_native_adopted_check_warns_on_codex_hit_in_mcp_servers() {
        use crate::test_support::env_guard;
        use tempfile::TempDir;

        let _guard = env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        let fake_home = TempDir::new().unwrap();
        write_injected_lock(
            fake_home.path(),
            r#"{"_adoptMaster":"codex","_mcpServers":{"node_repl":{"command":"node"}}}"#,
        );

        let findings = VendorNativeAdoptedCheck.check();

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }

        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("node_repl"));
    }

    #[test]
    fn vendor_native_adopted_check_silent_when_no_collision() {
        use crate::test_support::env_guard;
        use tempfile::TempDir;

        let _guard = env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        let fake_home = TempDir::new().unwrap();
        write_injected_lock(
            fake_home.path(),
            r#"{"_adoptMaster":"claude","_adoptedItems":[{"name":"memory","adoptedFrom":"claude","sourceId":"claude-mcp"}]}"#,
        );

        let findings = VendorNativeAdoptedCheck.check();

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }

        assert!(findings.is_empty());
    }

    #[test]
    fn vendor_native_adopted_check_silent_when_no_master_recorded() {
        use crate::test_support::env_guard;
        use tempfile::TempDir;

        let _guard = env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        let fake_home = TempDir::new().unwrap();
        write_injected_lock(fake_home.path(), r#"{}"#);

        let findings = VendorNativeAdoptedCheck.check();

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }

        assert!(
            findings.is_empty(),
            "missing _adoptMaster is MasterAdoptedCheck's finding, not this check's"
        );
    }

    // ─── DoctorReport round-trip coverage for the vendor-native-adopted check ──
    // Independent of the implementer's 4 direct-`check()` tests above: these
    // drive the finding through `DoctorReport` (exit-code contribution) and
    // assert the lockfile is untouched (report-only), plus the
    // finding-disappears-after-removal round trip.

    #[test]
    fn vendor_native_adopted_finding_is_warning_severity_exit_zero() {
        // "exits 0" — a single vendor-native hit must be Severity::Warning,
        // not Error, so DoctorReport::exit_code() stays 0 (warning-only grading).
        use crate::test_support::env_guard;
        use tempfile::TempDir;

        let _guard = env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        let fake_home = TempDir::new().unwrap();
        write_injected_lock(
            fake_home.path(),
            r#"{"_adoptMaster":"codex","_adoptedItems":[{"name":"node_repl","adoptedFrom":"codex","sourceId":"codex-mcp"}]}"#,
        );

        let findings = VendorNativeAdoptedCheck.check();

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }

        let mut report = DoctorReport::new();
        report.findings.extend(findings);
        assert_eq!(
            report.exit_code(),
            0,
            "vendor-native hit must stay warning-only"
        );
        assert!(!report.has_errors());
    }

    #[test]
    fn vendor_native_adopted_check_leaves_lockfile_byte_identical() {
        // "leaves the lockfile byte-identical" — report only, never rewrite.
        use crate::test_support::env_guard;
        use tempfile::TempDir;

        let _guard = env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        let fake_home = TempDir::new().unwrap();
        let lock_json = r#"{"_adoptMaster":"codex","_adoptedItems":[{"name":"node_repl","adoptedFrom":"codex","sourceId":"codex-mcp"}]}"#;
        write_injected_lock(fake_home.path(), lock_json);
        let lock_path = EffectiveLayout::load_or_legacy().unwrap().build_lock_path();
        let before = std::fs::read_to_string(&lock_path).unwrap();

        let _findings = VendorNativeAdoptedCheck.check();

        let after = std::fs::read_to_string(&lock_path).unwrap();

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }

        assert_eq!(before, after, "check() must not rewrite the lockfile");
    }

    #[test]
    fn vendor_native_adopted_finding_disappears_after_entry_removed() {
        // "the finding is gone after the entry is removed" — the round trip
        // a human performs by following the `ccync remove <name>` pointer.
        use crate::test_support::env_guard;
        use tempfile::TempDir;

        let _guard = env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        let fake_home = TempDir::new().unwrap();
        write_injected_lock(
            fake_home.path(),
            r#"{"_adoptMaster":"codex","_adoptedItems":[{"name":"node_repl","adoptedFrom":"codex","sourceId":"codex-mcp"}]}"#,
        );
        let before_removal = VendorNativeAdoptedCheck.check();

        // Simulate `ccync remove node_repl`: the entry is gone from the lockfile.
        write_injected_lock(
            fake_home.path(),
            r#"{"_adoptMaster":"codex","_adoptedItems":[]}"#,
        );
        let after_removal = VendorNativeAdoptedCheck.check();

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }

        assert_eq!(
            before_removal.len(),
            1,
            "hit must be reported before removal"
        );
        assert!(
            after_removal.is_empty(),
            "finding must be gone once the entry is removed from the lockfile"
        );
    }

    // ─── no-contribution per-entry decision ────────────────────────────────

    fn catalog_entry(id: &str) -> serde_json::Value {
        serde_json::json!({ "pluginId": id, "source": "https://example.test/repo" })
    }

    fn lock_entry_with_pin(id: &str, pin: &str) -> serde_json::Value {
        serde_json::json!({ "pluginId": id, "pin": pin })
    }

    #[test]
    fn no_contribution_finding_silent_when_cache_dir_contributes() {
        let cache_root = tempfile::tempdir().unwrap();
        let plugin_dir = cache_root.path().join("my-plugin@abc123");
        std::fs::create_dir_all(plugin_dir.join("skills")).unwrap();
        std::fs::write(plugin_dir.join("skills").join("a"), "a").unwrap();

        let entry = catalog_entry("my-plugin");
        let lock_entries = vec![lock_entry_with_pin("my-plugin", "abc123")];

        assert!(
            no_contribution_finding(&entry, &lock_entries, cache_root.path()).is_none(),
            "a contributing cache dir must not raise a finding"
        );
    }

    #[test]
    fn no_contribution_finding_fires_when_cache_dir_contributes_nothing() {
        let cache_root = tempfile::tempdir().unwrap();
        let plugin_dir = cache_root.path().join("my-plugin@abc123");
        std::fs::create_dir_all(plugin_dir.join("skills")).unwrap();

        let entry = catalog_entry("my-plugin");
        let lock_entries = vec![lock_entry_with_pin("my-plugin", "abc123")];

        let finding = no_contribution_finding(&entry, &lock_entries, cache_root.path())
            .expect("a non-contributing cache dir must raise exactly one finding");
        assert!(
            finding.message.contains("my-plugin"),
            "message must name the plugin id: {}",
            finding.message
        );
        assert!(
            finding.message.contains(&plugin_dir.display().to_string()),
            "message must name the resolved cache dir path: {}",
            finding.message
        );
    }

    #[test]
    fn no_contribution_finding_silent_when_unpinned() {
        let cache_root = tempfile::tempdir().unwrap();
        let entry = catalog_entry("my-plugin");

        assert!(
            no_contribution_finding(&entry, &[], cache_root.path()).is_none(),
            "an unpinned entry must produce no finding from this check"
        );
    }

    #[test]
    fn no_contribution_finding_silent_when_cache_dir_absent() {
        let cache_root = tempfile::tempdir().unwrap();
        let entry = catalog_entry("my-plugin");
        let lock_entries = vec![lock_entry_with_pin("my-plugin", "abc123")];

        assert!(
            no_contribution_finding(&entry, &lock_entries, cache_root.path()).is_none(),
            "a pinned entry whose cache dir is absent must produce no finding"
        );
    }

    #[test]
    fn no_contribution_finding_is_warning_with_fix_hint() {
        let cache_root = tempfile::tempdir().unwrap();
        let plugin_dir = cache_root.path().join("my-plugin@abc123");
        std::fs::create_dir_all(&plugin_dir).unwrap();

        let entry = catalog_entry("my-plugin");
        let lock_entries = vec![lock_entry_with_pin("my-plugin", "abc123")];

        let finding = no_contribution_finding(&entry, &lock_entries, cache_root.path())
            .expect("a non-contributing cache dir must raise a finding");
        assert_eq!(finding.severity, Severity::Warning);
        assert!(finding.fix_hint.is_some(), "finding must carry a fix hint");
    }

    // ─── render-contribution doctor check, aggregated via run_doctor ──────────

    #[test]
    fn doctor_reports_no_contribution() {
        use crate::test_support::env_guard;
        use tempfile::TempDir;

        let _guard = env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        let fake_home = TempDir::new().unwrap();
        std::env::set_var(var, fake_home.path());

        let layout = EffectiveLayout::load_or_legacy().unwrap();
        let catalog_path = layout.personal_catalog_path();
        std::fs::create_dir_all(catalog_path.parent().unwrap()).unwrap();
        std::fs::write(
            &catalog_path,
            r#"{"plugins":[{"pluginId":"my-plugin","source":"https://example.test/repo"}]}"#,
        )
        .unwrap();

        let lock_path = layout.build_lock_path();
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        std::fs::write(
            &lock_path,
            r#"{"_personalPlugins":[{"pluginId":"my-plugin","pin":"abc123"}]}"#,
        )
        .unwrap();

        let cache_root = layout.cache_path();
        std::fs::create_dir_all(cache_root.join("my-plugin@abc123")).unwrap();

        let report = run_doctor(&DoctorOptions::default());

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }

        let finding = report
            .findings
            .iter()
            .find(|f| f.message.contains("my-plugin"))
            .expect("aggregated report must carry the no-contribution finding");
        assert!(
            finding
                .message
                .contains("contributes no component and no MCP server"),
            "message: {}",
            finding.message
        );
        assert_eq!(finding.severity, Severity::Warning);
    }

    #[test]
    fn doctor_no_contribution_silent_on_absent_and_empty_catalog() {
        use crate::test_support::env_guard;
        use tempfile::TempDir;

        // The check must stay silent when it cannot prove a non-contribution:
        // a missing catalog and an empty plugin list are both zero-evidence
        // states, never an accusation.
        const MARKER: &str = "contributes no component and no MCP server";

        let _guard = env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);

        // Leg 1 - no personal catalog on disk at all.
        let absent_home = TempDir::new().unwrap();
        std::env::set_var(var, absent_home.path());
        let absent_report = run_doctor(&DoctorOptions::default());

        // Leg 2 - catalog present but its plugins array is empty.
        let empty_home = TempDir::new().unwrap();
        std::env::set_var(var, empty_home.path());
        let layout = EffectiveLayout::load_or_legacy().unwrap();
        let catalog_path = layout.personal_catalog_path();
        std::fs::create_dir_all(catalog_path.parent().unwrap()).unwrap();
        std::fs::write(&catalog_path, r#"{"plugins":[]}"#).unwrap();
        let lock_path = layout.build_lock_path();
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        std::fs::write(&lock_path, r#"{"_personalPlugins":[]}"#).unwrap();
        let empty_report = run_doctor(&DoctorOptions::default());

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }

        assert!(
            !absent_report
                .findings
                .iter()
                .any(|f| f.message.contains(MARKER)),
            "absent catalog must draw no no-contribution finding"
        );
        assert!(
            !empty_report
                .findings
                .iter()
                .any(|f| f.message.contains(MARKER)),
            "empty plugins array must draw no no-contribution finding"
        );
    }

    // after removing ClaudeMarketplaceState, doctor compiles and exit grading is correct.
    #[test]
    fn tp08_no_marketplace_classification_residue() {
        // Compile-time check: this test file does not reference ClaudeMarketplaceState.
        // If the enum still exists, this test serves as a reminder to remove it.
        // The real check is: `cargo test` compiles without any ClaudeMarketplaceState usage.
        let opts = DoctorOptions::default();
        let report = run_doctor(&opts);
        // Exit grading: 0 when warning-only, 1 when any error.
        let code = report.exit_code();
        assert!(
            code == 0 || code == 1,
            "exit code must be 0 or 1, got {code}"
        );
    }

    #[test]
    fn doctor_provider_diagnostic_reports_committed_records_and_preserves_secrets() {
        let lock = serde_json::json!({
            "_providerLifecycle": {
                "schemaVersion": 1,
                "committed": [{
                    "target": "codex",
                    "selector": "probe@ccync-marketplace",
                    "pluginName": "probe-provider",
                    "upstreamRevision": "rev-1",
                    "generation": {
                        "source": "probe-source",
                        "generation": "gen-1",
                        "packageDigest": "sha256:abc"
                    },
                    "capabilities": ["skill"]
                }],
                "secret_token": "super-secret-pass-token"
            }
        });
        // Committed records are not doctor findings: the loader is NotRun and
        // that state never escalates to a warning.
        assert!(committed_provider_records(&lock));
        let rows = crate::managed_view::provider_diagnostics(&lock);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider, "probe-provider");
        assert_eq!(rows[0].source_key, "probe-source");
        assert_eq!(rows[0].effective_loader, "probe@ccync-marketplace");
        assert_eq!(rows[0].alias, None);
        assert_eq!(
            rows[0].verification_scope,
            crate::managed_view::VerificationScope::LoaderNotRun
        );
        let rendered = format!("{rows:?}");
        assert!(!rendered.contains("super-secret-pass-token"));
    }

    #[test]
    fn doctor_provider_diagnostic_check_warns_on_ambiguous_or_unreadable_state() {
        use crate::test_support::env_guard;
        use tempfile::TempDir;

        let _guard = env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        let fake_home = TempDir::new().unwrap();
        std::env::set_var(var, fake_home.path());

        let build_root = ccync_foundation::paths::ccync_build_root().unwrap();
        std::fs::create_dir_all(&build_root).unwrap();
        let lock_path = build_root.join("lock.json");

        // Leg 1: Unreadable/malformed JSON
        std::fs::write(&lock_path, "not-valid-json").unwrap();
        let check = ProviderDiagnosticCheck;
        let findings = check.check();
        assert_eq!(findings.len(), 1);
        assert!(findings[0]
            .message
            .contains("provider lock state is unreadable"));
        assert!(findings[0]
            .message
            .contains("ownership is ambiguous and loader validation NotRun"));

        // Leg 2: Corrupted/invalid provider lifecycle schema
        std::fs::write(
            &lock_path,
            r#"{"_providerLifecycle": {"schemaVersion": 999}}"#,
        )
        .unwrap();
        let findings = check.check();
        assert_eq!(findings.len(), 1);
        assert!(findings[0]
            .message
            .contains("provider lifecycle state is ambiguous"));
        assert!(findings[0]
            .message
            .contains("ownership is unverified and loader validation NotRun"));

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }
    }
}
