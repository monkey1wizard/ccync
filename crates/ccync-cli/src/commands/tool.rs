#![allow(dead_code)]

use ccync_engine::{
    tool_manager::{
        adapters::{self, ToolAdapter},
        discovery::{
            canonical_executable_path, filter_by_executable_selector, merge_candidates, select,
            DiscoveryCandidate, DiscoveryQuery,
        },
        lock::LifecycleGuard,
        model::DeliveryId,
        store,
    },
    ExitCode,
};

const USAGE: &str = "Usage: ccync tool <discover|track|untrack|refresh> ...";

fn discover_exit(incomplete: bool, unique_trackable: bool) -> ExitCode {
    if incomplete {
        ExitCode::Error
    } else if unique_trackable {
        ExitCode::Success
    } else {
        ExitCode::UpdatesAvailable
    }
}

fn discover_candidate_sort_key(candidate: &DiscoveryCandidate) -> (String, String, String) {
    let executable = candidate
        .identity
        .fields
        .get("executable")
        .or_else(|| candidate.identity.fields.get("invokedPath"))
        .map(|value| canonical_executable_path(value))
        .unwrap_or_default();
    (
        serde_json::to_string(&candidate.identity.adapter_kind).unwrap_or_default(),
        executable,
        candidate.candidate_id.clone(),
    )
}

fn discover_outcome(
    candidates: &[DiscoveryCandidate],
    incomplete: bool,
) -> super::tool_output::ToolOutcome {
    use super::tool_output::ToolOutcome;
    if incomplete {
        ToolOutcome::ProbeError
    } else if candidates.is_empty() {
        ToolOutcome::NotFound
    } else if candidates.len() == 1 {
        ToolOutcome::Selected
    } else {
        ToolOutcome::Ambiguous
    }
}

fn tracked_action(id: Option<&str>) -> String {
    id.map(|id| format!("ccync outdated (tracked tool {id})"))
        .unwrap_or_else(|| "ccync tool track <id>".into())
}

fn known_subcommand(value: &str) -> bool {
    matches!(value, "discover" | "track" | "untrack" | "refresh")
}

pub(crate) fn cmd_tool(args: &[String], guard: Option<&LifecycleGuard>) -> ExitCode {
    if args.len() < 2 {
        eprintln!("{USAGE}");
        return ExitCode::Usage;
    }
    if !known_subcommand(&args[1]) {
        eprintln!("ccync tool: unknown subcommand '{}'.\n{USAGE}", args[1]);
        return ExitCode::Usage;
    }
    match args[1].as_str() {
        "discover" => cmd_discover(&args[1..]),
        "track" => match guard {
            Some(guard) => super::tool_track::cmd_track(&args[1..], guard),
            None => missing_lifecycle_guard("track"),
        },
        "untrack" => match guard {
            Some(guard) => super::tool_track::cmd_untrack(&args[1..], guard),
            None => missing_lifecycle_guard("untrack"),
        },
        "refresh" => match guard {
            Some(guard) => super::tool_refresh::cmd_refresh(&args[1..], guard),
            None => missing_lifecycle_guard("refresh"),
        },
        _ => unreachable!("known subcommand exhaustively matched"),
    }
}

fn missing_lifecycle_guard(subcommand: &str) -> ExitCode {
    eprintln!("ccync tool {subcommand}: lifecycle lock is required");
    ExitCode::Error
}

fn adapters_for(command: &str, delivery: DeliveryId) -> Vec<Box<dyn ToolAdapter>> {
    vec![
        Box::new(adapters::cargo::CargoAdapter::default()),
        Box::new(adapters::uv::UvAdapter::default()),
        Box::new(adapters::pipx::PipxAdapter::default()),
        Box::new(adapters::pip::PipAdapter::default()),
        Box::new(adapters::npm::NpmAdapter::default()),
        Box::new(adapters::pnpm::PnpmAdapter::default()),
        Box::new(adapters::brew_formula::BrewFormulaAdapter::default()),
        Box::new(adapters::brew_cask::BrewCaskAdapter::default()),
        Box::new(adapters::winget_offline::WingetOfflineAdapter::default()),
        Box::new(adapters::winget_online::WingetOnlineAdapter::default()),
        Box::new(adapters::ccync_receipt::CcyncReceiptAdapter::new()),
        Box::new(adapters::manual_path::ManualPathAdapter::new(
            command, delivery,
        )),
    ]
}

fn cmd_discover(args: &[String]) -> ExitCode {
    cmd_discover_to(args, None, &mut std::io::stdout(), &mut std::io::stderr())
}

fn cmd_discover_to(
    args: &[String],
    injected_adapters: Option<Vec<Box<dyn ToolAdapter>>>,
    stdout: &mut dyn std::io::Write,
    stderr: &mut dyn std::io::Write,
) -> ExitCode {
    if args.len() < 2
        || args.len() > 4
        || args[2..].iter().any(|a| a != "--online" && a != "--json")
    {
        let _ = writeln!(
            stderr,
            "Usage: ccync tool discover <command-or-path> [--online] [--json]"
        );
        return ExitCode::Usage;
    }
    let command = &args[1];
    let online = args.iter().any(|a| a == "--online");
    let json = args.iter().any(|a| a == "--json");
    if online {
        let _ = writeln!(
            stderr,
            "online probe: may access the network; package-manager cache may be read or updated"
        );
    }
    let adapters =
        injected_adapters.unwrap_or_else(|| adapters_for(command, DeliveryId::GraphifyAgents));
    let refs: Vec<&dyn ToolAdapter> = adapters.iter().map(|a| a.as_ref()).collect();
    let query = DiscoveryQuery { online };
    let mut candidates = Vec::new();
    let mut incomplete = false;
    let mut probe_errors = Vec::new();
    for adapter in &refs {
        match adapter.discover(&query) {
            Ok(found) => candidates.extend(found),
            Err(adapters::AdapterError::Failed(reason)) => {
                incomplete = true;
                probe_errors.push((adapter.kind(), reason.clone()));
                let _ = writeln!(
                    stderr,
                    "{} probe incomplete: {reason}",
                    serde_json::to_string(&adapter.kind()).unwrap_or_default()
                );
            }
            Err(adapters::AdapterError::NotChecked | adapters::AdapterError::Unsupported) => {}
        }
    }
    let candidates = merge_candidates(candidates);
    let mut candidates: Vec<DiscoveryCandidate> =
        filter_by_executable_selector(&candidates, command)
            .into_iter()
            .cloned()
            .collect();
    candidates.sort_by_key(discover_candidate_sort_key);
    let selection = select(&candidates);
    let outcome = discover_outcome(&candidates, incomplete);
    let tracked = store::load_declarations().ok();
    let mut report_candidates = Vec::new();
    for candidate in &candidates {
        let tracked_id = tracked
            .as_ref()
            .and_then(|map| map.values().find(|d| d.identity == candidate.identity))
            .map(|d| d.id.to_string());
        let next = if tracked_id.is_some() {
            tracked_action(tracked_id.as_deref())
        } else if candidate.trackable
            && candidate.recipe_compatible
            && candidate.evidence_level
                == ccync_engine::tool_manager::model::EvidenceLevel::Verified
        {
            tracked_action(None)
        } else {
            "select a candidate and explicitly accept limited evidence".to_owned()
        };
        let capabilities = refs
            .iter()
            .find(|adapter| adapter.kind() == candidate.identity.adapter_kind)
            .map(|adapter| super::tool_output::capabilities_json(adapter.capabilities()))
            .unwrap_or(serde_json::Value::Null);
        let executable = candidate
            .identity
            .fields
            .get("executable")
            .or_else(|| candidate.identity.fields.get("invokedPath"))
            .map(|value| canonical_executable_path(value));
        report_candidates.push(serde_json::json!({"candidateId":candidate.candidate_id,"adapter":candidate.identity.adapter_kind,"executable":executable,"identity":candidate.identity,"evidence":candidate.evidence_level,"shadowedEvidence":candidate.shadowed_evidence,"trackable":candidate.trackable,"recipeCompatible":candidate.recipe_compatible,"capabilities":capabilities,"nextAction":next}));
    }
    let next_action = match outcome {
        super::tool_output::ToolOutcome::Selected => report_candidates
            .first()
            .and_then(|row| row.get("nextAction"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        super::tool_output::ToolOutcome::NotFound => Some(format!(
            "install or provide a supported command path for selector '{command}'"
        )),
        super::tool_output::ToolOutcome::Ambiguous => Some(format!(
            "rerun ccync tool track --command {command} --candidate <candidate-id>"
        )),
        super::tool_output::ToolOutcome::ProbeError => {
            Some("retry the discovery probe with --online after fixing the reported adapter".into())
        }
        _ => None,
    };
    let mut facts = super::tool_output::ToolOutcomeFacts::new(
        super::tool_output::ToolCommand::Discover,
        outcome,
    );
    facts.selection_state = Some(
        match selection.state {
            ccync_engine::tool_manager::model::SelectionState::Unique => "unique",
            ccync_engine::tool_manager::model::SelectionState::Ambiguous => "ambiguous",
            ccync_engine::tool_manager::model::SelectionState::None => "none",
        }
        .into(),
    );
    facts.next_action = next_action;
    if json {
        let mut document = super::tool_output::outcome_json(&facts);
        document["selector"] = serde_json::json!(command);
        document["incomplete"] = serde_json::json!(incomplete);
        document["candidates"] = serde_json::Value::Array(report_candidates);
        document["probeErrors"] = serde_json::Value::Array(
            probe_errors
                .iter()
                .map(|(adapter, reason)| serde_json::json!({"adapter": adapter, "error": reason}))
                .collect(),
        );
        let _ = writeln!(
            stdout,
            "{}",
            serde_json::to_string_pretty(&document).unwrap_or_else(|_| "{}".into())
        );
    } else if outcome == super::tool_output::ToolOutcome::ProbeError {
        let _ = write!(
            stderr,
            "{}",
            super::tool_output::render_outcome_human(&facts)
        );
        for (adapter, reason) in probe_errors {
            let _ = writeln!(
                stderr,
                "probe adapter: {adapter:?}\nincomplete: true\nerror: {reason}"
            );
        }
    } else {
        let _ = write!(
            stdout,
            "{}",
            super::tool_output::render_outcome_human(&facts)
        );
        if outcome == super::tool_output::ToolOutcome::Ambiguous {
            for row in report_candidates {
                let _ = writeln!(stdout, "candidate: {}", row);
            }
        }
    }
    outcome.exit_code()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccync_engine::tool_manager::{
        adapters::{AdapterCapabilities, AdapterError, CapabilityState, ToolAdapter},
        discovery::{DiscoveryCandidate, DiscoveryQuery},
        model::{
            AdapterKind, DeliveryId, EvidenceLevel, InstallationIdentity, ToolDeclaration, ToolId,
        },
    };
    use std::collections::BTreeMap;

    struct EnvHomeGuard {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvHomeGuard {
        fn set(home: &std::path::Path) -> Self {
            let keys = [
                "USERPROFILE",
                "HOME",
                "APPDATA",
                "LOCALAPPDATA",
                "XDG_CONFIG_HOME",
            ];
            let saved = keys
                .into_iter()
                .map(|key| {
                    let value = std::env::var_os(key);
                    unsafe { std::env::set_var(key, home) };
                    (key, value)
                })
                .collect();
            Self { saved }
        }
    }

    impl Drop for EnvHomeGuard {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    #[derive(Debug, Clone)]
    struct MockDiscoveryAdapter {
        kind: AdapterKind,
        candidates: Result<Vec<DiscoveryCandidate>, AdapterError>,
    }

    impl ToolAdapter for MockDiscoveryAdapter {
        fn kind(&self) -> AdapterKind {
            self.kind
        }
        fn capabilities(&self) -> AdapterCapabilities {
            AdapterCapabilities::for_adapter(
                self.kind,
                CapabilityState::Supported,
                CapabilityState::Supported,
                CapabilityState::Supported,
                CapabilityState::Supported,
            )
        }
        fn discover(
            &self,
            _query: &DiscoveryQuery,
        ) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
            self.candidates.clone()
        }
        fn reprobe(
            &self,
            _query: &DiscoveryQuery,
        ) -> Result<Vec<DiscoveryCandidate>, AdapterError> {
            self.candidates.clone()
        }
    }

    fn make_candidate(
        adapter_kind: AdapterKind,
        executable: &str,
        level: EvidenceLevel,
    ) -> DiscoveryCandidate {
        let mut fields = BTreeMap::new();
        fields.insert("executable".into(), fixture_path(executable));
        let identity = InstallationIdentity {
            adapter_kind,
            fields,
        };
        DiscoveryCandidate::new(identity, level, true, true, false)
    }

    fn fixture_path(path: &str) -> String {
        if cfg!(windows) {
            path.to_owned()
        } else {
            let normalized = path.replace('\\', "/").replace("C:", "");
            normalized
                .strip_suffix(".exe")
                .unwrap_or(&normalized)
                .to_owned()
        }
    }

    #[test]
    fn unknown_subcommand_returns_usage() {
        let _io_guard = crate::test_io_lock();
        assert!(!known_subcommand("bogus"));
        for command in ["discover", "track", "untrack", "refresh"] {
            assert!(known_subcommand(command));
        }

        let temp = tempfile::tempdir().unwrap();
        let _env_guard = EnvHomeGuard::set(temp.path());
        let guard = ccync_engine::tool_manager::lock::LifecycleLock::acquire()
            .expect("lifecycle lock must be available for isolated test home");
        assert_eq!(
            cmd_tool(&["ccync".into(), "unknown".into()], Some(&guard)),
            ExitCode::Usage
        );
        assert_eq!(cmd_tool(&["ccync".into()], Some(&guard)), ExitCode::Usage);
        assert_eq!(
            cmd_tool(&["ccync".into(), "unknown".into()], Some(&guard)) as i32,
            64
        );
    }

    #[test]
    fn mutating_subcommand_without_lifecycle_guard_returns_error() {
        for subcommand in ["track", "untrack", "refresh"] {
            assert_eq!(
                cmd_tool(&["tool".into(), subcommand.into()], None),
                ExitCode::Error,
                "{subcommand} must fail cleanly when its lifecycle guard is missing"
            );
        }
    }

    #[test]
    fn discovery_exit_codes_distinguish_verified_selection_action_and_incomplete_probe() {
        assert_eq!(discover_exit(false, true), ExitCode::Success);
        assert_eq!(discover_exit(false, false), ExitCode::UpdatesAvailable);
        assert_eq!(discover_exit(true, true), ExitCode::Error);
        assert_eq!(cmd_discover(&[]), ExitCode::Usage);
        assert_eq!(
            cmd_discover(&["discover".into(), "cmd".into(), "--invalid".into()]),
            ExitCode::Usage
        );
        assert_eq!(ExitCode::Success as i32, 0);
        assert_eq!(ExitCode::Error as i32, 1);
        assert_eq!(ExitCode::UpdatesAvailable as i32, 3);
        assert_eq!(ExitCode::Usage as i32, 64);
    }

    #[test]
    fn online_disclosure_and_discovery_schema_do_not_claim_remote_updates() {
        let disclosure =
            "online probe: may access the network; package-manager cache may be read or updated";
        assert!(disclosure.contains("network") && disclosure.contains("cache"));
        let report =
            serde_json::json!({"schemaVersion":1,"candidates":[],"nextAction":"ccync outdated"});
        let text = serde_json::to_string(&report).unwrap();
        assert!(!text.contains("remoteUpdate"));
        assert!(!text.contains("updateAvailable"));
    }

    #[test]
    fn tracked_identity_points_to_outdated() {
        assert_eq!(
            tracked_action(Some("demo")),
            "ccync outdated (tracked tool demo)"
        );
    }

    #[test]
    fn discover_outcomes_and_exits() {
        let cand1 = make_candidate(
            AdapterKind::Cargo,
            "C:\\bin\\tool_a.exe",
            EvidenceLevel::Verified,
        );
        let cand2 = make_candidate(
            AdapterKind::Cargo,
            "C:\\bin2\\tool_a.exe",
            EvidenceLevel::Verified,
        );

        // 1. Selected outcome: exactly 1 match
        let adapter_single = Box::new(MockDiscoveryAdapter {
            kind: AdapterKind::Cargo,
            candidates: Ok(vec![cand1.clone()]),
        });
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = cmd_discover_to(
            &["discover".into(), "tool_a".into()],
            Some(vec![adapter_single]),
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(exit, ExitCode::Success);
        let out_str = String::from_utf8(stdout).unwrap();
        assert!(out_str.contains("outcome: selected"));
        assert!(out_str.contains("selection state: unique"));

        // 2. NotFound outcome: 0 matches
        let adapter_empty = Box::new(MockDiscoveryAdapter {
            kind: AdapterKind::Cargo,
            candidates: Ok(vec![]),
        });
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = cmd_discover_to(
            &["discover".into(), "missing_tool".into()],
            Some(vec![adapter_empty]),
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(exit, ExitCode::UpdatesAvailable);
        let out_str = String::from_utf8(stdout).unwrap();
        assert!(out_str.contains("outcome: not-found"));
        assert!(out_str.contains("selection state: none"));

        // 3. Ambiguous outcome: >1 matches
        let adapter_multi = Box::new(MockDiscoveryAdapter {
            kind: AdapterKind::Cargo,
            candidates: Ok(vec![cand1.clone(), cand2.clone()]),
        });
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = cmd_discover_to(
            &["discover".into(), "tool_a".into()],
            Some(vec![adapter_multi]),
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(exit, ExitCode::UpdatesAvailable);
        let out_str = String::from_utf8(stdout).unwrap();
        assert!(out_str.contains("outcome: ambiguous"));
        assert!(out_str.contains("selection state: ambiguous"));

        // 4. ProbeError outcome: adapter failed
        let adapter_err = Box::new(MockDiscoveryAdapter {
            kind: AdapterKind::Cargo,
            candidates: Err(AdapterError::Failed("mock probe failed".into())),
        });
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = cmd_discover_to(
            &["discover".into(), "tool_a".into()],
            Some(vec![adapter_err]),
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(exit, ExitCode::Error);
        let err_str = String::from_utf8(stderr).unwrap();
        assert!(err_str.contains("outcome: probe-error"));
        assert!(
            stdout.is_empty(),
            "Human ProbeError must write to stderr, not stdout"
        );
    }

    #[test]
    fn discover_exact_name_and_path_filtering() {
        let cand_a = make_candidate(
            AdapterKind::Cargo,
            "C:\\bin\\ripgrep.exe",
            EvidenceLevel::Verified,
        );
        let cand_b = make_candidate(
            AdapterKind::Cargo,
            "C:\\bin\\other.exe",
            EvidenceLevel::Verified,
        );
        let cand_c = make_candidate(
            AdapterKind::Cargo,
            "C:\\opt\\ripgrep.exe",
            EvidenceLevel::Verified,
        );

        // Bare name selector matches basename with or without extension
        let adapters = vec![Box::new(MockDiscoveryAdapter {
            kind: AdapterKind::Cargo,
            candidates: Ok(vec![cand_a.clone(), cand_b.clone(), cand_c.clone()]),
        }) as Box<dyn ToolAdapter>];

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = cmd_discover_to(
            &["discover".into(), "ripgrep".into(), "--json".into()],
            Some(adapters),
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(exit, ExitCode::UpdatesAvailable);
        let doc: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(doc["outcome"], "ambiguous");
        let candidates = doc["candidates"].as_array().unwrap();
        assert_eq!(
            candidates.len(),
            2,
            "must match both ripgrep candidates and exclude other"
        );

        // Exact path selector matches only the specific canonical path
        let adapters2 = vec![Box::new(MockDiscoveryAdapter {
            kind: AdapterKind::Cargo,
            candidates: Ok(vec![cand_a.clone(), cand_b.clone(), cand_c.clone()]),
        }) as Box<dyn ToolAdapter>];

        let mut stdout2 = Vec::new();
        let mut stderr2 = Vec::new();
        let exit2 = cmd_discover_to(
            vec![
                "discover".into(),
                fixture_path("C:\\bin\\ripgrep.exe"),
                "--json".into(),
            ]
            .as_slice(),
            Some(adapters2),
            &mut stdout2,
            &mut stderr2,
        );
        assert_eq!(exit2, ExitCode::Success);
        let doc2: serde_json::Value = serde_json::from_slice(&stdout2).unwrap();
        assert_eq!(doc2["outcome"], "selected");
        let candidates2 = doc2["candidates"].as_array().unwrap();
        assert_eq!(
            candidates2.len(),
            1,
            "path selector must match only C:\\bin\\ripgrep.exe"
        );
    }

    #[test]
    fn discover_stable_order() {
        let cand_uv = make_candidate(
            AdapterKind::UvTool,
            "C:\\bin\\a\\tool.exe",
            EvidenceLevel::Verified,
        );
        let cand_cargo_b = make_candidate(
            AdapterKind::Cargo,
            "C:\\bin\\b\\tool.exe",
            EvidenceLevel::Verified,
        );
        let cand_cargo_a = make_candidate(
            AdapterKind::Cargo,
            "C:\\bin\\a\\tool.exe",
            EvidenceLevel::Verified,
        );

        let adapters = vec![
            Box::new(MockDiscoveryAdapter {
                kind: AdapterKind::UvTool,
                candidates: Ok(vec![cand_uv.clone()]),
            }) as Box<dyn ToolAdapter>,
            Box::new(MockDiscoveryAdapter {
                kind: AdapterKind::Cargo,
                candidates: Ok(vec![cand_cargo_b.clone(), cand_cargo_a.clone()]),
            }) as Box<dyn ToolAdapter>,
        ];

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = cmd_discover_to(
            &["discover".into(), "tool".into(), "--json".into()],
            Some(adapters),
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(exit, ExitCode::UpdatesAvailable);
        let doc: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        let candidates = doc["candidates"].as_array().unwrap();
        assert_eq!(candidates.len(), 3);

        // Sorted by (adapterKind, canonical executable path, candidateId)
        // AdapterKind::Cargo sorts before AdapterKind::UvTool
        // C:\bin\a\tool.exe sorts before C:\bin\b\tool.exe
        assert_eq!(candidates[0]["candidateId"], cand_cargo_a.candidate_id);
        assert_eq!(candidates[1]["candidateId"], cand_cargo_b.candidate_id);
        assert_eq!(candidates[2]["candidateId"], cand_uv.candidate_id);
    }

    #[test]
    fn discover_evidence_and_guidance() {
        let _io_guard = crate::test_io_lock();
        let temp = tempfile::tempdir().unwrap();
        let _env_guard = EnvHomeGuard::set(temp.path());

        // 1. NotFound guidance includes selector and safe next action
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = cmd_discover_to(
            &["discover".into(), "missing_binary".into()],
            Some(vec![]),
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(exit, ExitCode::UpdatesAvailable);
        let out_str = String::from_utf8(stdout).unwrap();
        assert!(out_str
            .contains("install or provide a supported command path for selector 'missing_binary'"));

        // 2. Ambiguity guidance includes candidate ID and --candidate guidance
        let cand1 = make_candidate(
            AdapterKind::Cargo,
            "C:\\bin1\\ambig.exe",
            EvidenceLevel::Verified,
        );
        let cand2 = make_candidate(
            AdapterKind::Cargo,
            "C:\\bin2\\ambig.exe",
            EvidenceLevel::Manageable,
        );
        let adapters = vec![Box::new(MockDiscoveryAdapter {
            kind: AdapterKind::Cargo,
            candidates: Ok(vec![cand1.clone(), cand2.clone()]),
        }) as Box<dyn ToolAdapter>];
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = cmd_discover_to(
            &["discover".into(), "ambig".into()],
            Some(adapters),
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(exit, ExitCode::UpdatesAvailable);
        let out_str = String::from_utf8(stdout).unwrap();
        assert!(
            out_str.contains("rerun ccync tool track --command ambig --candidate <candidate-id>")
        );
        assert!(out_str.contains(&cand1.candidate_id));
        assert!(out_str.contains(&cand2.candidate_id));

        // 3. ProbeError guidance includes safe retry
        let adapters_err = vec![Box::new(MockDiscoveryAdapter {
            kind: AdapterKind::Cargo,
            candidates: Err(AdapterError::Failed("adapter crashed".into())),
        }) as Box<dyn ToolAdapter>];
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = cmd_discover_to(
            &["discover".into(), "probe_fail".into()],
            Some(adapters_err),
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(exit, ExitCode::Error);
        let err_str = String::from_utf8(stderr).unwrap();
        assert!(err_str
            .contains("retry the discovery probe with --online after fixing the reported adapter"));
        assert!(err_str.contains("adapter crashed"));

        // 4. Tracked tool points to ccync outdated (tracked tool <id>)
        let cand_tracked = make_candidate(
            AdapterKind::Cargo,
            "C:\\bin\\mytool.exe",
            EvidenceLevel::Verified,
        );
        let tool_id = ToolId::new("mytool").unwrap();
        let decl = ToolDeclaration {
            id: tool_id.clone(),
            identity: cand_tracked.identity.clone(),
            delivery_id: DeliveryId::GraphifyAgents,
            hold_policy: ccync_engine::tool_manager::model::HoldPolicy::Unheld,
            limited_evidence_acceptance: None,
            accepted_baseline: None,
            manager_takeover: None,
        };
        store::replace_declarations(&BTreeMap::from([(tool_id.clone(), decl)])).unwrap();

        let adapters_tracked = vec![Box::new(MockDiscoveryAdapter {
            kind: AdapterKind::Cargo,
            candidates: Ok(vec![cand_tracked.clone()]),
        }) as Box<dyn ToolAdapter>];
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = cmd_discover_to(
            &["discover".into(), "mytool".into()],
            Some(adapters_tracked),
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(exit, ExitCode::Success);
        let out_str = String::from_utf8(stdout).unwrap();
        assert!(out_str.contains("ccync outdated (tracked tool mytool)"));
    }

    #[test]
    fn discover_streams_human_and_json() {
        let cand = make_candidate(
            AdapterKind::Cargo,
            "C:\\bin\\stream_test.exe",
            EvidenceLevel::Verified,
        );

        // Human Selected: stdout has output, stderr is empty
        let adapters = vec![Box::new(MockDiscoveryAdapter {
            kind: AdapterKind::Cargo,
            candidates: Ok(vec![cand.clone()]),
        }) as Box<dyn ToolAdapter>];
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = cmd_discover_to(
            &["discover".into(), "stream_test".into()],
            Some(adapters),
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(exit, ExitCode::Success);
        assert!(!stdout.is_empty());
        assert!(stderr.is_empty(), "Human Selected stderr must be empty");

        // Human ProbeError: stdout is empty, stderr has output
        let adapters_err = vec![Box::new(MockDiscoveryAdapter {
            kind: AdapterKind::Cargo,
            candidates: Err(AdapterError::Failed("probe fault".into())),
        }) as Box<dyn ToolAdapter>];
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = cmd_discover_to(
            &["discover".into(), "stream_test".into()],
            Some(adapters_err),
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(exit, ExitCode::Error);
        assert!(stdout.is_empty(), "Human ProbeError stdout must be empty");
        assert!(!stderr.is_empty());

        // JSON modes: stdout has exactly one JSON document, parseable
        let outcomes_to_test = [
            ("selected", Ok(vec![cand.clone()])),
            ("not-found", Ok(vec![])),
            (
                "ambiguous",
                Ok(vec![
                    cand.clone(),
                    make_candidate(
                        AdapterKind::Cargo,
                        "C:\\bin2\\stream_test.exe",
                        EvidenceLevel::Verified,
                    ),
                ]),
            ),
            ("probe-error", Err(AdapterError::Failed("err".into()))),
        ];

        for (expected_outcome, res) in outcomes_to_test {
            let adapters = vec![Box::new(MockDiscoveryAdapter {
                kind: AdapterKind::Cargo,
                candidates: res,
            }) as Box<dyn ToolAdapter>];
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let _ = cmd_discover_to(
                &["discover".into(), "stream_test".into(), "--json".into()],
                Some(adapters),
                &mut stdout,
                &mut stderr,
            );
            let doc: serde_json::Value =
                serde_json::from_slice(&stdout).expect("stdout must be valid JSON");
            assert_eq!(doc["outcome"], expected_outcome);
        }
    }

    #[test]
    fn discover_json_types_and_schema() {
        let cand1 = make_candidate(
            AdapterKind::Cargo,
            "C:\\bin\\schema_tool.exe",
            EvidenceLevel::Verified,
        );
        let cand2 = make_candidate(
            AdapterKind::Cargo,
            "C:\\bin2\\schema_tool.exe",
            EvidenceLevel::Verified,
        );

        let cases: Vec<(&str, Result<Vec<DiscoveryCandidate>, AdapterError>)> = vec![
            ("selected", Ok(vec![cand1.clone()])),
            ("not-found", Ok(vec![])),
            ("ambiguous", Ok(vec![cand1.clone(), cand2.clone()])),
            (
                "probe-error",
                Err(AdapterError::Failed("schema probe failed".into())),
            ),
        ];

        for (expected_outcome, res) in cases {
            let adapters = vec![Box::new(MockDiscoveryAdapter {
                kind: AdapterKind::Cargo,
                candidates: res,
            }) as Box<dyn ToolAdapter>];
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let exit = cmd_discover_to(
                &["discover".into(), "schema_tool".into(), "--json".into()],
                Some(adapters),
                &mut stdout,
                &mut stderr,
            );

            let doc: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
            assert_eq!(doc["schemaVersion"], 1);
            assert_eq!(doc["command"], "tool discover");
            assert_eq!(doc["outcome"], expected_outcome);
            assert!(doc["selectionState"].is_string());
            assert!(doc["updateState"].is_null());
            assert!(doc["updateFreshness"].is_null());
            assert!(doc["nextAction"].is_string());
            assert!(doc["affectedAgents"].is_array());
            assert_eq!(doc["projectionChanged"], false);
            assert!(doc["reloadAdvice"].is_null());
            assert_eq!(doc["selector"], "schema_tool");
            assert!(doc["incomplete"].is_boolean());
            assert!(doc["candidates"].is_array());
            assert!(doc["probeErrors"].is_array());

            if expected_outcome == "probe-error" {
                assert_eq!(exit, ExitCode::Error);
                assert_eq!(doc["incomplete"], true);
                let errors = doc["probeErrors"].as_array().unwrap();
                assert_eq!(errors.len(), 1);
                assert_eq!(errors[0]["error"], "schema probe failed");
            } else if expected_outcome == "selected" {
                assert_eq!(exit, ExitCode::Success);
                assert_eq!(doc["incomplete"], false);
                let candidates = doc["candidates"].as_array().unwrap();
                assert_eq!(candidates.len(), 1);
                assert!(candidates[0]["candidateId"].is_string());
                assert!(candidates[0]["adapter"].is_string());
                assert!(candidates[0]["executable"].is_string());
                assert!(candidates[0]["trackable"].is_boolean());
                assert!(candidates[0]["recipeCompatible"].is_boolean());
            } else {
                assert_eq!(exit, ExitCode::UpdatesAvailable);
                assert_eq!(doc["incomplete"], false);
            }
        }
    }
}
