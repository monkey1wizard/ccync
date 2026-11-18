//! Safe declaration of a discovered external tool.
use super::{
    adapters::{self, ToolAdapter},
    discovery::{filter_by_executable_selector, merge_candidates, select, DiscoveryQuery},
    lock::LifecycleGuard,
    model::{DeliveryId, EvidenceLevel, HoldPolicy, SelectionState, ToolDeclaration, ToolId},
    probe::ProbeRunner,
    store::{self, InsertResult},
};
use sha2::Digest;
use std::path::PathBuf;

pub struct TrackRequest<'a> {
    /// Caller supplied stable identifier. It is validated before any path access.
    pub stable_id: String,
    pub adapters: &'a [&'a dyn ToolAdapter],
    pub delivery: DeliveryId,
    pub command_selection: bool,
    pub candidate_id: Option<String>,
    pub accept_limited_evidence: bool,
    pub confirmed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackOutcome {
    pub inserted: bool,
    pub external_action: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum TrackError {
    #[error("invalid tool id")]
    InvalidId,
    #[error("discovery did not produce the requested selection")]
    Selection,
    #[error("limited evidence requires an explicit flag and caller confirmation")]
    LimitedEvidence,
    #[error("unknown evidence cannot be tracked")]
    UnknownEvidence,
    #[error("delivery is incompatible with the discovered installation")]
    Incompatible,
    #[error("tracking capability is gated")]
    Gated,
    #[error("tool declaration store failed: {0}")]
    Store(#[from] store::StoreError),
    #[error("manual-path baseline could not be verified: {0}")]
    Baseline(String),
}

/// Re-discover and validate the chosen installation before atomically inserting it.
pub fn track_tool(
    _guard: &LifecycleGuard,
    request: TrackRequest<'_>,
    runner: &dyn ProbeRunner,
) -> Result<TrackOutcome, TrackError> {
    track_tool_with_selector(_guard, request, runner, None)
}

/// Track a candidate selected by the exact command or path supplied by the caller.
pub fn track_tool_with_selector(
    _guard: &LifecycleGuard,
    request: TrackRequest<'_>,
    runner: &dyn ProbeRunner,
    command_selector: Option<&str>,
) -> Result<TrackOutcome, TrackError> {
    let stable_id = request.stable_id;
    let id = ToolId::new(stable_id.clone()).map_err(|_| TrackError::InvalidId)?;
    let _ = runner;
    let mut observations = Vec::new();
    for adapter in request.adapters {
        observations.extend(
            adapter
                .discover(&DiscoveryQuery { online: false })
                .unwrap_or_default(),
        );
    }
    let candidates = merge_candidates(observations);
    let selected_candidates = command_selector
        .map(|selector| filter_by_executable_selector(&candidates, selector))
        .unwrap_or_else(|| candidates.iter().collect());
    let exact_candidates = selected_candidates
        .iter()
        .map(|candidate| (*candidate).clone())
        .collect::<Vec<_>>();
    let selection = select(&exact_candidates);
    let candidate = if request.command_selection {
        if selection.state != SelectionState::Unique || request.candidate_id.is_some() {
            return Err(TrackError::Selection);
        }
        let selected = selection.selected.as_deref().ok_or(TrackError::Selection)?;
        exact_candidates
            .iter()
            .find(|candidate| candidate.candidate_id == selected)
    } else {
        let requested = request
            .candidate_id
            .as_deref()
            .ok_or(TrackError::Selection)?;
        exact_candidates
            .iter()
            .find(|candidate| candidate.candidate_id == requested)
    }
    .ok_or(TrackError::Selection)?;
    let adapter = request
        .adapters
        .iter()
        .copied()
        .find(|adapter| adapter.kind() == candidate.identity.adapter_kind)
        .ok_or(TrackError::Selection)?;
    let reprobed = adapter
        .reprobe(&DiscoveryQuery { online: false })
        .map_err(|_| TrackError::Selection)?;
    let reprobed = merge_candidates(reprobed);
    let reprobed = command_selector
        .map(|selector| filter_by_executable_selector(&reprobed, selector))
        .unwrap_or_else(|| reprobed.iter().collect());
    let candidate = reprobed
        .into_iter()
        .find(|current| current.candidate_id == candidate.candidate_id)
        .ok_or(TrackError::Selection)?;
    if candidate.evidence_level == EvidenceLevel::Unknown || !candidate.trackable {
        return Err(TrackError::UnknownEvidence);
    }
    let limited = matches!(
        candidate.evidence_level,
        EvidenceLevel::Manageable | EvidenceLevel::Heuristic
    );
    if limited && (!request.accept_limited_evidence || !request.confirmed) {
        return Err(TrackError::LimitedEvidence);
    }
    if request.delivery == DeliveryId::ObserveOnly {
        // Observe-only is a deliberate manual-path declaration whose row remains gated
        // until the table opens tracking for that exact adapter/delivery pair.
        if adapters::gates::GATE_TABLE.iter().any(|row| {
            matches!(row,
            adapters::gates::GateRow::Tracking(g) if g.adapter == candidate.identity.adapter_kind
                && g.delivery == request.delivery && g.phase_b == adapters::CapabilityState::Gated)
        }) {
            return Err(TrackError::Gated);
        }
    } else if !candidate.recipe_compatible
        || !super::delivery::compatible(request.delivery, &candidate.identity)
    {
        return Err(TrackError::Incompatible);
    }
    let baseline = if request.delivery == DeliveryId::ObserveOnly {
        Some(manual_baseline(&candidate.identity)?)
    } else {
        None
    };
    let declaration = ToolDeclaration {
        id,
        identity: candidate.identity.clone(),
        delivery_id: request.delivery,
        hold_policy: HoldPolicy::Unheld,
        limited_evidence_acceptance: limited.then(|| {
            serde_json::json!({
                "candidateId": candidate.candidate_id,
                "evidenceLevel": candidate.evidence_level,
            })
        }),
        accepted_baseline: baseline,
        manager_takeover: None,
    };
    let inserted = store::insert_declaration(declaration)? == InsertResult::Inserted;
    let external_action = loose_skill_master_source(&stable_id);
    Ok(TrackOutcome {
        inserted,
        external_action,
    })
}

fn loose_skill_master_source(name: &str) -> Option<PathBuf> {
    let lock = store::ToolPaths::load()
        .ok()?
        .snapshot_base
        .join("lock.json");
    let bytes = std::fs::read(lock).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let mut matching = value
        .get("_looseSkills")?
        .as_array()?
        .iter()
        .filter(|entry| entry.get("name").and_then(serde_json::Value::as_str) == Some(name));
    let entry = matching.next()?;
    if matching.next().is_some() {
        return None;
    }
    let origin = entry.get("origin")?.as_str()?;
    let path = PathBuf::from(origin);
    path.is_dir().then_some(path)
}

fn manual_baseline(
    identity: &super::model::InstallationIdentity,
) -> Result<serde_json::Value, TrackError> {
    use std::{fs::File, io::Read};
    let invoked = identity
        .fields
        .get("invokedPath")
        .ok_or_else(|| TrackError::Baseline("invoked path missing".into()))?;
    let path = PathBuf::from(invoked);
    let resolved = std::fs::canonicalize(&path).map_err(|e| TrackError::Baseline(e.to_string()))?;
    let mut file = File::open(&resolved).map_err(|e| TrackError::Baseline(e.to_string()))?;
    let metadata = file
        .metadata()
        .map_err(|e| TrackError::Baseline(e.to_string()))?;
    if !metadata.is_file() {
        return Err(TrackError::Baseline("target is not a regular file".into()));
    }
    let mut hash = sha2::Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|e| TrackError::Baseline(e.to_string()))?;
        if n == 0 {
            break;
        }
        sha2::Digest::update(&mut hash, &buffer[..n]);
    }
    let sha256 = hex::encode(sha2::Digest::finalize(hash));
    let file_identity = identity
        .fields
        .get("fileIdentity")
        .cloned()
        .ok_or_else(|| TrackError::Baseline("file identity missing".into()))?;
    let link_chain = identity
        .fields
        .get("linkChain")
        .cloned()
        .unwrap_or_else(|| "[]".into());
    let chain: serde_json::Value =
        serde_json::from_str(&link_chain).map_err(|e| TrackError::Baseline(e.to_string()))?;
    Ok(
        serde_json::json!({ "invokedPath": invoked, "linkChain": chain, "fileIdentity": file_identity, "sha256": sha256 }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_manager::{
        adapters::{AdapterCapabilities, AdapterError, CapabilityState},
        discovery::DiscoveryCandidate,
        lock::LifecycleLock,
        model::{AdapterKind, InstallationIdentity},
        probe::FixtureProbeRunner,
    };
    use std::collections::BTreeMap;
    struct One(DiscoveryCandidate);
    impl ToolAdapter for One {
        fn kind(&self) -> AdapterKind {
            self.0.identity.adapter_kind
        }
        fn capabilities(&self) -> AdapterCapabilities {
            AdapterCapabilities::for_adapter(
                self.kind(),
                CapabilityState::Supported,
                CapabilityState::Supported,
                CapabilityState::Supported,
                CapabilityState::Unsupported,
            )
        }
        fn discover(&self, _: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
            Ok(vec![self.0.clone()])
        }
        fn reprobe(&self, q: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
            self.discover(q)
        }
    }

    struct SelectorSequence {
        initial: Vec<DiscoveryCandidate>,
        reprobe: Vec<DiscoveryCandidate>,
    }

    impl ToolAdapter for SelectorSequence {
        fn kind(&self) -> AdapterKind {
            AdapterKind::Cargo
        }
        fn capabilities(&self) -> AdapterCapabilities {
            AdapterCapabilities::for_adapter(
                self.kind(),
                CapabilityState::Supported,
                CapabilityState::Supported,
                CapabilityState::Supported,
                CapabilityState::Unsupported,
            )
        }
        fn discover(&self, _: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
            Ok(self.initial.clone())
        }
        fn reprobe(&self, _: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
            Ok(self.reprobe.clone())
        }
    }
    fn setup() -> (
        crate::test_support::TestHome,
        super::super::lock::LifecycleGuard,
    ) {
        let home = crate::test_support::TestHome::new();
        let guard = LifecycleLock::acquire().unwrap();
        (home, guard)
    }

    fn cargo_candidate(name: &str, executable: &str) -> DiscoveryCandidate {
        DiscoveryCandidate::new(
            InstallationIdentity {
                adapter_kind: AdapterKind::Cargo,
                fields: BTreeMap::from([
                    ("name".into(), name.into()),
                    ("executable".into(), executable.into()),
                ]),
            },
            EvidenceLevel::Verified,
            true,
            true,
            false,
        )
    }

    #[test]
    fn exact_selector_filters_initial_selection_and_reprobe() {
        let (_home, guard) = setup();
        let root = std::env::current_exe().unwrap();
        let suffix = std::env::consts::EXE_SUFFIX;
        let selected_path = root
            .parent()
            .unwrap()
            .join("selector-one")
            .join(format!("rtk{suffix}"));
        let other_path = root
            .parent()
            .unwrap()
            .join("selector-two")
            .join(format!("other{suffix}"));
        let selected = cargo_candidate("rtk", &selected_path.to_string_lossy());
        let other = cargo_candidate("rtk", &other_path.to_string_lossy());
        let adapter = SelectorSequence {
            initial: vec![selected.clone(), other.clone()],
            reprobe: vec![selected.clone(), other],
        };
        let adapters: [&dyn ToolAdapter; 1] = [&adapter];
        let runner = FixtureProbeRunner::default();

        let outcome = track_tool_with_selector(
            &guard,
            TrackRequest {
                stable_id: "selector-rtk".into(),
                adapters: &adapters,
                delivery: DeliveryId::RtkPath,
                command_selection: true,
                candidate_id: None,
                accept_limited_evidence: false,
                confirmed: false,
            },
            &runner,
            Some("rtk"),
        )
        .unwrap();
        assert!(outcome.inserted);

        let err = track_tool_with_selector(
            &guard,
            TrackRequest {
                stable_id: "selector-missing".into(),
                adapters: &adapters,
                delivery: DeliveryId::RtkPath,
                command_selection: true,
                candidate_id: None,
                accept_limited_evidence: false,
                confirmed: false,
            },
            &runner,
            Some("missing"),
        )
        .unwrap_err();
        assert!(matches!(err, TrackError::Selection));

        let ambiguous = SelectorSequence {
            initial: vec![
                cargo_candidate(
                    "rtk",
                    &root
                        .parent()
                        .unwrap()
                        .join("selector-one")
                        .join(format!("rtk{suffix}"))
                        .to_string_lossy(),
                ),
                cargo_candidate(
                    "rtk",
                    &root
                        .parent()
                        .unwrap()
                        .join("selector-two")
                        .join(format!("rtk{suffix}"))
                        .to_string_lossy(),
                ),
            ],
            reprobe: vec![],
        };
        let ambiguous_adapters: [&dyn ToolAdapter; 1] = [&ambiguous];
        let err = track_tool_with_selector(
            &guard,
            TrackRequest {
                stable_id: "selector-ambiguous".into(),
                adapters: &ambiguous_adapters,
                delivery: DeliveryId::RtkPath,
                command_selection: true,
                candidate_id: None,
                accept_limited_evidence: false,
                confirmed: false,
            },
            &runner,
            Some("rtk"),
        )
        .unwrap_err();
        assert!(matches!(err, TrackError::Selection));
    }
    #[test]
    fn tracks_unique_verified_candidate_without_manager_requests() {
        let (_home, guard) = setup();
        let exe = std::env::current_exe().unwrap();
        let identity = InstallationIdentity {
            adapter_kind: AdapterKind::Cargo,
            fields: BTreeMap::from([
                ("name".into(), "rtk".into()),
                ("executable".into(), exe.to_string_lossy().into_owned()),
            ]),
        };
        let candidate =
            DiscoveryCandidate::new(identity, EvidenceLevel::Verified, true, true, false);
        let adapter = One(candidate);
        let adapters: [&dyn ToolAdapter; 1] = [&adapter];
        let runner = FixtureProbeRunner::default();
        let outcome = track_tool(
            &guard,
            TrackRequest {
                stable_id: "rtk".into(),
                adapters: &adapters,
                delivery: DeliveryId::RtkPath,
                command_selection: true,
                candidate_id: None,
                accept_limited_evidence: false,
                confirmed: false,
            },
            &runner,
        )
        .unwrap();
        assert!(outcome.inserted);
        assert!(runner.requests().is_empty());
        assert!(
            !track_tool(
                &guard,
                TrackRequest {
                    stable_id: "rtk".into(),
                    adapters: &adapters,
                    delivery: DeliveryId::RtkPath,
                    command_selection: true,
                    candidate_id: None,
                    accept_limited_evidence: false,
                    confirmed: false
                },
                &runner
            )
            .unwrap()
            .inserted
        );
    }
    #[test]
    fn refuses_unsafe_id_before_discovery() {
        let (_home, guard) = setup();
        let runner = FixtureProbeRunner::default();
        assert!(matches!(
            track_tool(
                &guard,
                TrackRequest {
                    stable_id: "../bad".into(),
                    adapters: &[],
                    delivery: DeliveryId::ObserveOnly,
                    command_selection: false,
                    candidate_id: None,
                    accept_limited_evidence: false,
                    confirmed: false
                },
                &runner
            ),
            Err(TrackError::InvalidId)
        ));
        assert!(runner.requests().is_empty());
    }

    #[test]
    fn retrack_conflict_fails_with_store_error() {
        let (_home, guard) = setup();
        let exe = std::env::current_exe().unwrap();
        let identity = InstallationIdentity {
            adapter_kind: AdapterKind::Cargo,
            fields: BTreeMap::from([
                ("name".into(), "rtk".into()),
                ("executable".into(), exe.to_string_lossy().into_owned()),
            ]),
        };
        let candidate =
            DiscoveryCandidate::new(identity, EvidenceLevel::Verified, true, true, false);
        let adapter = One(candidate);
        let adapters: [&dyn ToolAdapter; 1] = [&adapter];
        let runner = FixtureProbeRunner::default();
        let outcome = track_tool(
            &guard,
            TrackRequest {
                stable_id: "rtk".into(),
                adapters: &adapters,
                delivery: DeliveryId::RtkPath,
                command_selection: true,
                candidate_id: None,
                accept_limited_evidence: false,
                confirmed: false,
            },
            &runner,
        )
        .unwrap();
        assert!(outcome.inserted);

        let other_exe = exe.parent().unwrap().join("other_rtk.exe");
        let conflict_identity = InstallationIdentity {
            adapter_kind: AdapterKind::Cargo,
            fields: BTreeMap::from([
                ("name".into(), "rtk".into()),
                (
                    "executable".into(),
                    other_exe.to_string_lossy().into_owned(),
                ),
            ]),
        };
        let conflict_candidate = DiscoveryCandidate::new(
            conflict_identity,
            EvidenceLevel::Verified,
            true,
            true,
            false,
        );
        let conflict_adapter = One(conflict_candidate);
        let conflict_adapters: [&dyn ToolAdapter; 1] = [&conflict_adapter];

        let err = track_tool(
            &guard,
            TrackRequest {
                stable_id: "rtk".into(),
                adapters: &conflict_adapters,
                delivery: DeliveryId::RtkPath,
                command_selection: true,
                candidate_id: None,
                accept_limited_evidence: false,
                confirmed: false,
            },
            &runner,
        )
        .unwrap_err();

        assert!(matches!(
            err,
            TrackError::Store(store::StoreError::ConflictRequiresUntrack(_))
        ));
        assert!(runner.requests().is_empty());
    }

    #[test]
    fn manageable_and_heuristic_candidates_require_limited_flag_and_confirmation() {
        let (_home, guard) = setup();
        let exe = std::env::current_exe().unwrap();
        let identity = InstallationIdentity {
            adapter_kind: AdapterKind::Cargo,
            fields: BTreeMap::from([
                ("name".into(), "rtk".into()),
                ("executable".into(), exe.to_string_lossy().into_owned()),
            ]),
        };
        let candidate =
            DiscoveryCandidate::new(identity, EvidenceLevel::Heuristic, true, true, false);
        let cand_id = candidate.candidate_id.clone();
        let adapter = One(candidate);
        let adapters: [&dyn ToolAdapter; 1] = [&adapter];
        let runner = FixtureProbeRunner::default();

        let err1 = track_tool(
            &guard,
            TrackRequest {
                stable_id: "rtk".into(),
                adapters: &adapters,
                delivery: DeliveryId::RtkPath,
                command_selection: false,
                candidate_id: Some(cand_id.clone()),
                accept_limited_evidence: false,
                confirmed: false,
            },
            &runner,
        )
        .unwrap_err();
        assert!(matches!(err1, TrackError::LimitedEvidence));

        let err2 = track_tool(
            &guard,
            TrackRequest {
                stable_id: "rtk".into(),
                adapters: &adapters,
                delivery: DeliveryId::RtkPath,
                command_selection: false,
                candidate_id: Some(cand_id.clone()),
                accept_limited_evidence: true,
                confirmed: false,
            },
            &runner,
        )
        .unwrap_err();
        assert!(matches!(err2, TrackError::LimitedEvidence));

        let outcome = track_tool(
            &guard,
            TrackRequest {
                stable_id: "rtk".into(),
                adapters: &adapters,
                delivery: DeliveryId::RtkPath,
                command_selection: false,
                candidate_id: Some(cand_id.clone()),
                accept_limited_evidence: true,
                confirmed: true,
            },
            &runner,
        )
        .unwrap();
        assert!(outcome.inserted);
        assert!(runner.requests().is_empty());

        let declarations = store::load_declarations().unwrap();
        let decl = declarations
            .get(&ToolId::new("rtk").unwrap())
            .expect("declaration must be stored");
        let acceptance = decl
            .limited_evidence_acceptance
            .as_ref()
            .expect("limitedEvidenceAcceptance must be present");
        assert_eq!(acceptance["candidateId"], cand_id);
        assert_eq!(acceptance["evidenceLevel"], "heuristic");
    }

    #[test]
    fn unknown_and_gated_refusals() {
        let (_home, guard) = setup();
        let runner = FixtureProbeRunner::default();

        let unknown_identity = InstallationIdentity {
            adapter_kind: AdapterKind::Cargo,
            fields: BTreeMap::new(),
        };
        let unknown_cand = DiscoveryCandidate::new(
            unknown_identity,
            EvidenceLevel::Unknown,
            false,
            false,
            false,
        );
        let unknown_cand_id = unknown_cand.candidate_id.clone();
        let unknown_adapter = One(unknown_cand);
        let adapters: [&dyn ToolAdapter; 1] = [&unknown_adapter];

        let err = track_tool(
            &guard,
            TrackRequest {
                stable_id: "unknown-tool".into(),
                adapters: &adapters,
                delivery: DeliveryId::RtkPath,
                command_selection: false,
                candidate_id: Some(unknown_cand_id),
                accept_limited_evidence: true,
                confirmed: true,
            },
            &runner,
        )
        .unwrap_err();
        assert!(matches!(err, TrackError::UnknownEvidence));

        let untrackable_identity = InstallationIdentity {
            adapter_kind: AdapterKind::Cargo,
            fields: BTreeMap::new(),
        };
        let untrackable_cand = DiscoveryCandidate::new(
            untrackable_identity,
            EvidenceLevel::Verified,
            false,
            false,
            false,
        );
        let untrackable_cand_id = untrackable_cand.candidate_id.clone();
        let untrackable_adapter = One(untrackable_cand);
        let adapters2: [&dyn ToolAdapter; 1] = [&untrackable_adapter];

        let err2 = track_tool(
            &guard,
            TrackRequest {
                stable_id: "untrackable-tool".into(),
                adapters: &adapters2,
                delivery: DeliveryId::RtkPath,
                command_selection: false,
                candidate_id: Some(untrackable_cand_id),
                accept_limited_evidence: true,
                confirmed: true,
            },
            &runner,
        )
        .unwrap_err();
        assert!(matches!(err2, TrackError::UnknownEvidence));

        let manual_identity = InstallationIdentity {
            adapter_kind: AdapterKind::ManualPath,
            fields: BTreeMap::new(),
        };
        let manual_cand = DiscoveryCandidate::new(
            manual_identity,
            EvidenceLevel::Heuristic,
            true,
            false,
            false,
        );
        let manual_cand_id = manual_cand.candidate_id.clone();
        let manual_adapter = One(manual_cand);
        let adapters3: [&dyn ToolAdapter; 1] = [&manual_adapter];

        let err3 = track_tool(
            &guard,
            TrackRequest {
                stable_id: "manual-tool".into(),
                adapters: &adapters3,
                delivery: DeliveryId::ObserveOnly,
                command_selection: false,
                candidate_id: Some(manual_cand_id),
                accept_limited_evidence: true,
                confirmed: true,
            },
            &runner,
        )
        .unwrap_err();
        assert!(matches!(err3, TrackError::Gated));
    }

    #[test]
    fn gated_package_update_still_allows_tracking() {
        let (_home, guard) = setup();
        let exe = std::env::current_exe().unwrap();
        let identity = InstallationIdentity {
            adapter_kind: AdapterKind::Cargo,
            fields: BTreeMap::from([
                ("name".into(), "rtk".into()),
                ("executable".into(), exe.to_string_lossy().into_owned()),
            ]),
        };
        let candidate =
            DiscoveryCandidate::new(identity, EvidenceLevel::Verified, true, true, false);

        struct GatedUpdateAdapter(DiscoveryCandidate);
        impl ToolAdapter for GatedUpdateAdapter {
            fn kind(&self) -> AdapterKind {
                self.0.identity.adapter_kind
            }
            fn capabilities(&self) -> AdapterCapabilities {
                AdapterCapabilities::for_adapter(
                    self.kind(),
                    CapabilityState::Supported,
                    CapabilityState::Supported,
                    CapabilityState::Supported,
                    CapabilityState::Gated,
                )
            }
            fn discover(
                &self,
                _: &DiscoveryQuery,
            ) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
                Ok(vec![self.0.clone()])
            }
            fn reprobe(&self, q: &DiscoveryQuery) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
                self.discover(q)
            }
        }

        let adapter = GatedUpdateAdapter(candidate);
        let adapters: [&dyn ToolAdapter; 1] = [&adapter];
        let runner = FixtureProbeRunner::default();
        let outcome = track_tool(
            &guard,
            TrackRequest {
                stable_id: "rtk".into(),
                adapters: &adapters,
                delivery: DeliveryId::RtkPath,
                command_selection: true,
                candidate_id: None,
                accept_limited_evidence: false,
                confirmed: false,
            },
            &runner,
        )
        .unwrap();

        assert!(outcome.inserted);
        assert!(runner.requests().is_empty());
    }

    #[test]
    fn observe_only_writes_baseline_in_same_file_write() {
        let (_home, guard) = setup();
        let exe = std::env::current_exe().unwrap();
        let identity = InstallationIdentity {
            adapter_kind: AdapterKind::CcyncReceipt,
            fields: BTreeMap::from([
                ("invokedPath".into(), exe.to_string_lossy().into_owned()),
                ("fileIdentity".into(), "mock-file-identity".into()),
                ("linkChain".into(), "[]".into()),
            ]),
        };
        let candidate =
            DiscoveryCandidate::new(identity, EvidenceLevel::Verified, true, false, false);
        let cand_id = candidate.candidate_id.clone();
        let adapter = One(candidate);
        let adapters: [&dyn ToolAdapter; 1] = [&adapter];
        let runner = FixtureProbeRunner::default();

        let outcome = track_tool(
            &guard,
            TrackRequest {
                stable_id: "receipt-tool".into(),
                adapters: &adapters,
                delivery: DeliveryId::ObserveOnly,
                command_selection: false,
                candidate_id: Some(cand_id),
                accept_limited_evidence: false,
                confirmed: false,
            },
            &runner,
        )
        .unwrap();

        assert!(outcome.inserted);
        let decl = store::load_declarations()
            .unwrap()
            .remove(&ToolId::new("receipt-tool").unwrap())
            .expect("declaration must exist");
        let baseline = decl
            .accepted_baseline
            .expect("baseline must be recorded in same write");
        assert_eq!(baseline["invokedPath"], exe.to_string_lossy().into_owned());
        assert_eq!(baseline["fileIdentity"], "mock-file-identity");
        assert!(baseline["sha256"].as_str().is_some_and(|s| !s.is_empty()));
    }

    #[test]
    fn loose_skill_shadow_notice_emitted_when_loose_skill_exists() {
        let (home, guard) = setup();
        let exe = std::env::current_exe().unwrap();

        let build_dir = home.path().join(".ccync").join("build");
        std::fs::create_dir_all(&build_dir).unwrap();
        let lock_json = serde_json::json!({
            "_looseSkills": [
                { "name": "graphify", "origin": home.path().join(".agents").join("skills").join("graphify") }
            ]
        });
        std::fs::write(
            build_dir.join("lock.json"),
            serde_json::to_vec(&lock_json).unwrap(),
        )
        .unwrap();

        let agents_skill_dir = home.path().join(".agents").join("skills").join("graphify");
        std::fs::create_dir_all(&agents_skill_dir).unwrap();

        let identity = InstallationIdentity {
            adapter_kind: AdapterKind::InterpreterPip,
            fields: BTreeMap::from([
                ("distribution".into(), "graphifyy".into()),
                ("interpreter".into(), exe.to_string_lossy().into_owned()),
            ]),
        };
        let candidate =
            DiscoveryCandidate::new(identity, EvidenceLevel::Verified, true, true, false);
        let adapter = One(candidate);
        let adapters: [&dyn ToolAdapter; 1] = [&adapter];
        let runner = FixtureProbeRunner::default();

        let outcome = track_tool(
            &guard,
            TrackRequest {
                stable_id: "graphify".into(),
                adapters: &adapters,
                delivery: DeliveryId::GraphifyAgents,
                command_selection: true,
                candidate_id: None,
                accept_limited_evidence: false,
                confirmed: false,
            },
            &runner,
        )
        .unwrap();

        assert!(outcome.inserted);
        assert_eq!(outcome.external_action, Some(agents_skill_dir));
    }

    #[test]
    fn manual_baseline_rejects_lowercase_keys_without_camel_case() {
        let exe = std::env::current_exe().unwrap();
        let lowercase_identity = InstallationIdentity {
            adapter_kind: AdapterKind::CcyncReceipt,
            fields: BTreeMap::from([
                ("invokedpath".into(), exe.to_string_lossy().into_owned()),
                ("fileidentity".into(), "mock-file-identity".into()),
                ("linkchain".into(), "[]".into()),
            ]),
        };
        let err = manual_baseline(&lowercase_identity).unwrap_err();
        assert!(matches!(err, TrackError::Baseline(msg) if msg.contains("invoked path missing")));

        let missing_file_id = InstallationIdentity {
            adapter_kind: AdapterKind::CcyncReceipt,
            fields: BTreeMap::from([
                ("invokedPath".into(), exe.to_string_lossy().into_owned()),
                ("fileidentity".into(), "mock-file-identity".into()),
            ]),
        };
        let err2 = manual_baseline(&missing_file_id).unwrap_err();
        assert!(matches!(err2, TrackError::Baseline(msg) if msg.contains("file identity missing")));
    }
}
