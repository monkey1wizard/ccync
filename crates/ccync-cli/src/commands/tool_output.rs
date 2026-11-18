#![allow(dead_code)]

use ccync_engine::tool_manager::{adapters::AdapterCapabilities, model::ToolFacets};

/// The command boundaries that share the external-tool output contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolCommand {
    Discover,
    Outdated,
    Track,
    Untrack,
    Refresh,
    Upgrade,
}

/// The stable outcome vocabulary exposed by external-tool commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolOutcome {
    Selected,
    NotFound,
    Ambiguous,
    ProbeError,
    Current,
    Unsupported,
    NotChecked,
    UpdateAvailable,
    CheckFailed,
    ActionableDrift,
    Tracked,
    Untracked,
    Committed,
    NoOp,
    DryRun,
    Blocked,
    ConfirmationRequired,
    RecoveryRequired,
    Usage,
}

impl ToolOutcome {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Selected => "selected",
            Self::NotFound => "not-found",
            Self::Ambiguous => "ambiguous",
            Self::ProbeError => "probe-error",
            Self::Current => "current",
            Self::Unsupported => "unsupported",
            Self::NotChecked => "not-checked",
            Self::UpdateAvailable => "update-available",
            Self::CheckFailed => "check-failed",
            Self::ActionableDrift => "actionable-drift",
            Self::Tracked => "tracked",
            Self::Untracked => "untracked",
            Self::Committed => "committed",
            Self::NoOp => "no-op",
            Self::DryRun => "dry-run",
            Self::Blocked => "blocked",
            Self::ConfirmationRequired => "confirmation-required",
            Self::RecoveryRequired => "recovery-required",
            Self::Usage => "usage",
        }
    }

    pub(crate) const fn exit_code(self) -> ccync_engine::ExitCode {
        match self {
            Self::ProbeError | Self::CheckFailed => ccync_engine::ExitCode::Error,
            Self::NotFound
            | Self::Ambiguous
            | Self::ActionableDrift
            | Self::UpdateAvailable
            | Self::Blocked
            | Self::ConfirmationRequired
            | Self::RecoveryRequired => ccync_engine::ExitCode::UpdatesAvailable,
            Self::Usage => ccync_engine::ExitCode::Usage,
            _ => ccync_engine::ExitCode::Success,
        }
    }
}

impl ToolCommand {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Discover => "tool discover",
            Self::Outdated => "outdated",
            Self::Track => "tool track",
            Self::Untrack => "tool untrack",
            Self::Refresh => "tool refresh",
            Self::Upgrade => "upgrade tool",
        }
    }

    pub(crate) const fn supports_json(self) -> bool {
        !matches!(self, Self::Track | Self::Untrack)
    }

    pub(crate) const fn allows(self, outcome: ToolOutcome) -> bool {
        match self {
            Self::Discover => matches!(
                outcome,
                ToolOutcome::Selected
                    | ToolOutcome::NotFound
                    | ToolOutcome::Ambiguous
                    | ToolOutcome::ProbeError
                    | ToolOutcome::Usage
            ),
            Self::Outdated => matches!(
                outcome,
                ToolOutcome::Current
                    | ToolOutcome::Unsupported
                    | ToolOutcome::NotChecked
                    | ToolOutcome::UpdateAvailable
                    | ToolOutcome::CheckFailed
                    | ToolOutcome::ActionableDrift
                    | ToolOutcome::Usage
            ),
            Self::Track => matches!(
                outcome,
                ToolOutcome::Tracked
                    | ToolOutcome::NotFound
                    | ToolOutcome::Ambiguous
                    | ToolOutcome::ProbeError
                    | ToolOutcome::Blocked
                    | ToolOutcome::ConfirmationRequired
                    | ToolOutcome::Usage
            ),
            Self::Untrack => matches!(
                outcome,
                ToolOutcome::Untracked
                    | ToolOutcome::NotFound
                    | ToolOutcome::Blocked
                    | ToolOutcome::ConfirmationRequired
                    | ToolOutcome::RecoveryRequired
                    | ToolOutcome::Usage
            ),
            Self::Refresh => matches!(
                outcome,
                ToolOutcome::Committed
                    | ToolOutcome::NoOp
                    | ToolOutcome::DryRun
                    | ToolOutcome::Blocked
                    | ToolOutcome::CheckFailed
                    | ToolOutcome::RecoveryRequired
                    | ToolOutcome::Usage
            ),
            Self::Upgrade => matches!(
                outcome,
                ToolOutcome::Committed
                    | ToolOutcome::Current
                    | ToolOutcome::DryRun
                    | ToolOutcome::Blocked
                    | ToolOutcome::CheckFailed
                    | ToolOutcome::RecoveryRequired
                    | ToolOutcome::Usage
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputStream {
    Stdout,
    Stderr,
}

/// Select the human-readable stream for an outcome.
pub(crate) const fn output_stream(command: ToolCommand, outcome: ToolOutcome) -> OutputStream {
    match command {
        ToolCommand::Discover => match outcome {
            ToolOutcome::Selected | ToolOutcome::NotFound | ToolOutcome::Ambiguous => {
                OutputStream::Stdout
            }
            ToolOutcome::ProbeError => OutputStream::Stderr,
            _ => OutputStream::Stderr,
        },
        ToolCommand::Outdated => match outcome {
            ToolOutcome::CheckFailed | ToolOutcome::Usage => OutputStream::Stderr,
            _ => OutputStream::Stdout,
        },
        ToolCommand::Track => match outcome {
            ToolOutcome::Tracked => OutputStream::Stdout,
            _ => OutputStream::Stderr,
        },
        ToolCommand::Untrack => match outcome {
            ToolOutcome::Untracked => OutputStream::Stdout,
            _ => OutputStream::Stderr,
        },
        ToolCommand::Refresh | ToolCommand::Upgrade => match outcome {
            ToolOutcome::Committed
            | ToolOutcome::NoOp
            | ToolOutcome::DryRun
            | ToolOutcome::Current => OutputStream::Stdout,
            _ => OutputStream::Stderr,
        },
    }
}

pub(crate) fn json_usage(
    command: ToolCommand,
    requested: bool,
) -> Result<(), ccync_engine::ExitCode> {
    if requested && !command.supports_json() {
        eprintln!("{} does not support --json", command.as_str());
        Err(ccync_engine::ExitCode::Usage)
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolOutcomeFacts {
    pub(crate) command: ToolCommand,
    pub(crate) outcome: ToolOutcome,
    pub(crate) selection_state: Option<String>,
    pub(crate) update_state: Option<String>,
    pub(crate) update_freshness: Option<String>,
    pub(crate) next_action: Option<String>,
    pub(crate) affected_agents: Vec<String>,
    pub(crate) projection_changed: bool,
    pub(crate) reload_advice: Option<String>,
}

impl ToolOutcomeFacts {
    pub(crate) fn new(command: ToolCommand, outcome: ToolOutcome) -> Self {
        Self {
            command,
            outcome,
            selection_state: None,
            update_state: None,
            update_freshness: None,
            next_action: None,
            affected_agents: Vec::new(),
            projection_changed: false,
            reload_advice: None,
        }
    }

    pub(crate) fn with_reload(
        mut self,
        projection_changed: bool,
        affected_agents: &[String],
    ) -> Self {
        self.projection_changed = projection_changed;
        self.affected_agents = affected_agents.to_vec();
        self.reload_advice = reload_advice(self.outcome, projection_changed, affected_agents);
        self
    }
}

pub(crate) fn reload_advice(
    outcome: ToolOutcome,
    projection_changed: bool,
    agents: &[String],
) -> Option<String> {
    if !matches!(outcome, ToolOutcome::Committed | ToolOutcome::Untracked) || !projection_changed {
        return None;
    }
    Some(reload_line(agents))
}

pub(crate) fn outcome_json(facts: &ToolOutcomeFacts) -> serde_json::Value {
    serde_json::json!({
        "schemaVersion": 1,
        "command": facts.command.as_str(),
        "outcome": facts.outcome.as_str(),
        "selectionState": facts.selection_state,
        "updateState": facts.update_state,
        "updateFreshness": facts.update_freshness,
        "nextAction": facts.next_action,
        "affectedAgents": facts.affected_agents,
        "projectionChanged": facts.projection_changed,
        "reloadAdvice": facts.reload_advice,
    })
}

pub(crate) fn render_outcome_json(facts: &ToolOutcomeFacts) -> Result<String, serde_json::Error> {
    serde_json::to_string(&outcome_json(facts))
}

pub(crate) fn render_outcome_human(facts: &ToolOutcomeFacts) -> String {
    let mut lines = vec![format!("outcome: {}", facts.outcome.as_str())];
    if let Some(value) = facts.selection_state.as_deref() {
        lines.push(format!("selection state: {value}"));
    }
    if let Some(value) = facts.update_state.as_deref() {
        lines.push(format!("update state: {value}"));
    }
    if let Some(value) = facts.update_freshness.as_deref() {
        lines.push(format!("update freshness: {value}"));
    }
    if let Some(value) = facts.next_action.as_deref() {
        lines.push(format!("next action: {value}"));
    }
    if !facts.affected_agents.is_empty() {
        lines.push(format!(
            "affected agents: {}",
            facts.affected_agents.join(", ")
        ));
    }
    lines.push(format!("projection changed: {}", facts.projection_changed));
    if let Some(value) = facts.reload_advice.as_deref() {
        lines.push(format!("reload advice: {value}"));
    }
    lines.join("\n") + "\n"
}
pub(crate) fn capabilities_json(value: AdapterCapabilities) -> serde_json::Value {
    use ccync_engine::tool_manager::adapters::CapabilityState;
    fn state(value: CapabilityState) -> String {
        match value {
            CapabilityState::Supported => "supported",
            CapabilityState::Unsupported => "unsupported",
            CapabilityState::Gated => "gated",
        }
        .into()
    }
    serde_json::json!({
        "localEnumeration": state(value.local_enumeration),
        "executableMapping": state(value.executable_mapping),
        "installedVersionLookup": state(value.installed_version_lookup),
        "remoteDiscovery": state(value.remote_discovery),
        "packageUpdate": state(value.package_update),
    })
}

pub(crate) fn render_json(value: &serde_json::Value) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(value)
}

pub(crate) fn tool_report_json(
    id: &str,
    facets: &ToolFacets,
    capabilities: AdapterCapabilities,
    next_action: &str,
    reload_advice: &str,
) -> serde_json::Value {
    serde_json::json!({
        "schemaVersion": 1,
        "id": id,
        "facets": facets,
        "lastCheck": facets.last_check,
        "capabilities": capabilities_json(capabilities),
        "nextAction": next_action,
        "reloadAdvice": reload_advice,
    })
}

pub(crate) fn tool_report_human(
    id: &str,
    facets: &ToolFacets,
    capabilities: AdapterCapabilities,
    next_action: &str,
    reload_advice: &str,
) -> String {
    let facet_values = serde_json::to_value(facets).unwrap_or(serde_json::Value::Null);
    let capability_values = capabilities_json(capabilities);
    let mut facts = vec![("id", id.to_owned())];
    if let Some(object) = facet_values.as_object() {
        for (key, value) in object {
            let label = match key.as_str() {
                "evidenceLevel" => "evidence level",
                "selectionState" => "selection state",
                "managerAvailability" => "manager availability",
                "installState" => "install state",
                "packageState" => "package state",
                "updateState" => "update state",
                "artifactState" => "artifact state",
                "projectionState" => "projection state",
                "holdPolicy" => "hold policy",
                "managerPinState" => "manager pin state",
                "operationState" => "operation state",
                "lastCheck" => "last check",
                _ => continue,
            };
            facts.push((
                label,
                value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| "unknown".into()),
            ));
        }
    }
    if let Some(object) = capability_values.as_object() {
        for (key, value) in object {
            let label = match key.as_str() {
                "localEnumeration" => "capability local enumeration",
                "executableMapping" => "capability executable mapping",
                "installedVersionLookup" => "capability installed version lookup",
                "remoteDiscovery" => "capability remote discovery",
                "packageUpdate" => "capability package update",
                _ => continue,
            };
            facts.push((label, value.as_str().unwrap_or("unknown").to_owned()));
        }
    }
    facts.extend([
        ("next action", next_action.to_owned()),
        ("reload advice", reload_advice.to_owned()),
    ]);
    render_facts(facts)
}

pub(crate) fn dry_run_value() -> &'static str {
    "unknown-until-executed"
}

pub(crate) fn render_facts<'a>(facts: impl IntoIterator<Item = (&'a str, String)>) -> String {
    facts
        .into_iter()
        .map(|(label, value)| format!("{label}: {value}\n"))
        .collect()
}

pub(crate) fn reload_line(agents: &[String]) -> String {
    let affected = if agents.is_empty() {
        String::new()
    } else {
        format!("affected agents: {}\n", agents.join(", "))
    };
    format!("{affected}reload advised; loaded generation unknown")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccync_engine::tool_manager::model::*;

    fn facets() -> ToolFacets {
        ToolFacets {
            evidence_level: EvidenceLevel::Verified,
            selection_state: SelectionState::Unique,
            manager_availability: ManagerAvailability::Available,
            install_state: InstallState::Installed,
            package_state: PackageState::MatchesArtifactSource,
            update_state: UpdateState::Current,
            artifact_state: ArtifactState::Current,
            projection_state: ProjectionState::Current,
            hold_policy: HoldPolicy::Unheld,
            manager_pin_state: ManagerPinState::Unpinned,
            operation_state: OperationState::Committed,
            last_check: Some("now".into()),
        }
    }

    #[test]
    fn json_includes_every_facet_and_has_no_ansi() {
        let f = facets();
        let caps = AdapterCapabilities::for_adapter(
            AdapterKind::Cargo,
            ccync_engine::tool_manager::adapters::CapabilityState::Supported,
            ccync_engine::tool_manager::adapters::CapabilityState::Supported,
            ccync_engine::tool_manager::adapters::CapabilityState::Supported,
            ccync_engine::tool_manager::adapters::CapabilityState::Unsupported,
        );
        let report = tool_report_json("demo", &f, caps, "ccync outdated", "none");
        let json = render_json(&report).unwrap();
        for key in [
            "evidenceLevel",
            "selectionState",
            "managerAvailability",
            "installState",
            "packageState",
            "updateState",
            "artifactState",
            "projectionState",
            "holdPolicy",
            "managerPinState",
            "operationState",
            "lastCheck",
            "capabilities",
            "nextAction",
            "reloadAdvice",
        ] {
            assert!(json.contains(&format!("\"{key}\"")), "missing {key}");
        }
        assert!(!json.contains('\u{1b}'));
        let human = tool_report_human("demo", &f, caps, "ccync outdated", "none");
        for fact in [
            "evidence level:",
            "selection state:",
            "manager availability:",
            "install state:",
            "package state:",
            "update state:",
            "artifact state:",
            "projection state:",
            "hold policy:",
            "manager pin state:",
            "operation state:",
            "last check:",
            "capability local enumeration:",
            "capability executable mapping:",
            "capability installed version lookup:",
            "capability remote discovery:",
            "capability package update:",
            "next action:",
            "reload advice:",
        ] {
            assert!(human.contains(fact), "missing human fact {fact}");
        }
        assert!(!human.contains('\u{1b}'));
    }

    #[test]
    fn human_facts_are_single_complete_label_value_lines_at_narrow_width() {
        let output = render_facts([
            ("package state", "externally-changed".into()),
            ("next action", "ccync tool refresh demo".into()),
        ]);
        assert_eq!(
            output.lines().collect::<Vec<_>>(),
            [
                "package state: externally-changed",
                "next action: ccync tool refresh demo"
            ]
        );
        assert!(output.lines().all(|line| line.len() <= 40));
        assert!(!output.contains('\u{1b}'));
    }

    #[test]
    fn reload_advice_line_is_exact() {
        assert_eq!(
            reload_line(&["claude".into(), "codex".into()]),
            "affected agents: claude, codex\nreload advised; loaded generation unknown"
        );
        assert_eq!(
            reload_line(&[]),
            "reload advised; loaded generation unknown"
        );
        assert_eq!(dry_run_value(), "unknown-until-executed");
    }

    #[test]
    fn outcome_matrix() {
        let command_outcomes = [
            (
                ToolCommand::Discover,
                &[
                    ToolOutcome::Selected,
                    ToolOutcome::NotFound,
                    ToolOutcome::Ambiguous,
                    ToolOutcome::ProbeError,
                    ToolOutcome::Usage,
                ][..],
            ),
            (
                ToolCommand::Outdated,
                &[
                    ToolOutcome::Current,
                    ToolOutcome::Unsupported,
                    ToolOutcome::NotChecked,
                    ToolOutcome::UpdateAvailable,
                    ToolOutcome::CheckFailed,
                    ToolOutcome::ActionableDrift,
                    ToolOutcome::Usage,
                ][..],
            ),
            (
                ToolCommand::Track,
                &[
                    ToolOutcome::Tracked,
                    ToolOutcome::NotFound,
                    ToolOutcome::Ambiguous,
                    ToolOutcome::ProbeError,
                    ToolOutcome::Blocked,
                    ToolOutcome::ConfirmationRequired,
                    ToolOutcome::Usage,
                ][..],
            ),
            (
                ToolCommand::Untrack,
                &[
                    ToolOutcome::Untracked,
                    ToolOutcome::NotFound,
                    ToolOutcome::Blocked,
                    ToolOutcome::ConfirmationRequired,
                    ToolOutcome::RecoveryRequired,
                    ToolOutcome::Usage,
                ][..],
            ),
            (
                ToolCommand::Refresh,
                &[
                    ToolOutcome::Committed,
                    ToolOutcome::NoOp,
                    ToolOutcome::DryRun,
                    ToolOutcome::Blocked,
                    ToolOutcome::CheckFailed,
                    ToolOutcome::RecoveryRequired,
                    ToolOutcome::Usage,
                ][..],
            ),
            (
                ToolCommand::Upgrade,
                &[
                    ToolOutcome::Committed,
                    ToolOutcome::Current,
                    ToolOutcome::DryRun,
                    ToolOutcome::Blocked,
                    ToolOutcome::CheckFailed,
                    ToolOutcome::RecoveryRequired,
                    ToolOutcome::Usage,
                ][..],
            ),
        ];
        for (command, outcomes) in command_outcomes {
            for outcome in outcomes {
                assert!(command.allows(*outcome), "{command:?} rejects {outcome:?}");
                let expected_exit = match *outcome {
                    ToolOutcome::ProbeError | ToolOutcome::CheckFailed => {
                        ccync_engine::ExitCode::Error
                    }
                    ToolOutcome::NotFound
                    | ToolOutcome::Ambiguous
                    | ToolOutcome::UpdateAvailable
                    | ToolOutcome::ActionableDrift
                    | ToolOutcome::Blocked
                    | ToolOutcome::ConfirmationRequired
                    | ToolOutcome::RecoveryRequired => ccync_engine::ExitCode::UpdatesAvailable,
                    ToolOutcome::Usage => ccync_engine::ExitCode::Usage,
                    _ => ccync_engine::ExitCode::Success,
                };
                assert_eq!(outcome.exit_code(), expected_exit, "{outcome:?}");
                let expected_stream = match (command, *outcome) {
                    (ToolCommand::Discover, ToolOutcome::Selected)
                    | (ToolCommand::Discover, ToolOutcome::NotFound)
                    | (ToolCommand::Discover, ToolOutcome::Ambiguous)
                    | (ToolCommand::Outdated, ToolOutcome::Current)
                    | (ToolCommand::Outdated, ToolOutcome::Unsupported)
                    | (ToolCommand::Outdated, ToolOutcome::NotChecked)
                    | (ToolCommand::Outdated, ToolOutcome::UpdateAvailable)
                    | (ToolCommand::Outdated, ToolOutcome::ActionableDrift)
                    | (ToolCommand::Track, ToolOutcome::Tracked)
                    | (ToolCommand::Untrack, ToolOutcome::Untracked)
                    | (ToolCommand::Refresh, ToolOutcome::Committed)
                    | (ToolCommand::Refresh, ToolOutcome::NoOp)
                    | (ToolCommand::Refresh, ToolOutcome::DryRun)
                    | (ToolCommand::Upgrade, ToolOutcome::Committed)
                    | (ToolCommand::Upgrade, ToolOutcome::Current)
                    | (ToolCommand::Upgrade, ToolOutcome::DryRun) => OutputStream::Stdout,
                    _ => OutputStream::Stderr,
                };
                assert_eq!(
                    output_stream(command, *outcome),
                    expected_stream,
                    "{command:?} {outcome:?}"
                );
            }
        }

        for command in [
            ToolCommand::Discover,
            ToolCommand::Outdated,
            ToolCommand::Refresh,
            ToolCommand::Upgrade,
        ] {
            assert!(command.supports_json());
            assert_eq!(json_usage(command, true), Ok(()));
        }
        for command in [ToolCommand::Track, ToolCommand::Untrack] {
            assert!(!command.supports_json());
            assert_eq!(
                json_usage(command, true),
                Err(ccync_engine::ExitCode::Usage)
            );
        }
        let mut facts = ToolOutcomeFacts::new(ToolCommand::Refresh, ToolOutcome::Committed);
        facts.update_freshness = Some("stale".into());
        facts.next_action = Some("ccync outdated".into());
        facts = facts.with_reload(true, &["claude".into()]);
        let json = render_outcome_json(&facts).unwrap();
        let document: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(document["outcome"], "committed");
        assert_eq!(document["updateFreshness"], "stale");
        assert_eq!(document["affectedAgents"], serde_json::json!(["claude"]));
        assert_eq!(document["projectionChanged"], true);
        assert!(document["reloadAdvice"]
            .as_str()
            .unwrap()
            .contains("reload advised"));
        assert_eq!(json.matches('{').count(), 1);

        for outcome in [
            ToolOutcome::NoOp,
            ToolOutcome::DryRun,
            ToolOutcome::CheckFailed,
        ] {
            let facts = ToolOutcomeFacts::new(ToolCommand::Refresh, outcome).with_reload(true, &[]);
            let document = outcome_json(&facts);
            assert_eq!(document["reloadAdvice"], serde_json::Value::Null);
        }
        let facts = ToolOutcomeFacts::new(ToolCommand::Untrack, ToolOutcome::Untracked)
            .with_reload(true, &["codex".into()]);
        assert!(outcome_json(&facts)["reloadAdvice"].is_string());
        for outcome in [
            ToolOutcome::Tracked,
            ToolOutcome::Untracked,
            ToolOutcome::Committed,
        ] {
            let facts = ToolOutcomeFacts::new(ToolCommand::Track, outcome).with_reload(false, &[]);
            assert_eq!(
                outcome_json(&facts)["reloadAdvice"],
                serde_json::Value::Null
            );
        }
        let facts = ToolOutcomeFacts::new(ToolCommand::Outdated, ToolOutcome::Current);
        assert_eq!(
            outcome_json(&facts)["updateFreshness"],
            serde_json::Value::Null
        );
    }
}
