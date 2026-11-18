use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpAttemptInitiator {
    Agent,
    Ccync,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpAttemptTrigger {
    AgentRetry,
    DirectRequest,
    ConfigReload,
    UserOperation,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpRetryStopState {
    Terminated,
    OutOfBounds,
    StillRetrying,
    Truncated,
    ManualIntervention,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpEvidenceSource {
    NativeConfig,
    Plugin,
    PersonalList,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpEvidenceActor {
    Agent,
    Ccync,
    ThirdParty,
    Unknown,
}
#[derive(Debug, thiserror::Error)]
pub enum McpEvidenceError {
    #[error("invalid or unsanitized evidence")]
    InvalidEvidence,
    #[error("unsupported evidence version")]
    UnsupportedVersion,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpEvidenceAttempt {
    host: String,
    host_version: String,
    session: String,
    server: String,
    operation: String,
    attempt: String,
    events: Vec<u64>,
    initiator: McpAttemptInitiator,
    trigger: McpAttemptTrigger,
    sources: Vec<McpEvidenceSource>,
    readers: Vec<McpEvidenceActor>,
}
impl McpEvidenceAttempt {
    pub fn initiator(&self) -> McpAttemptInitiator {
        self.initiator
    }
    pub fn trigger(&self) -> McpAttemptTrigger {
        self.trigger
    }
    pub fn host(&self) -> &str {
        &self.host
    }
    pub fn host_version(&self) -> &str {
        &self.host_version
    }
    pub fn session(&self) -> &str {
        &self.session
    }
    pub fn server(&self) -> &str {
        &self.server
    }
    pub fn operation(&self) -> &str {
        &self.operation
    }
    pub fn attempt(&self) -> &str {
        &self.attempt
    }
    pub fn events(&self) -> &[u64] {
        &self.events
    }
    pub fn sources(&self) -> &[McpEvidenceSource] {
        &self.sources
    }
    pub fn readers(&self) -> &[McpEvidenceActor] {
        &self.readers
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpEvidenceReport {
    run: String,
    attempts: Vec<McpEvidenceAttempt>,
    gaps: Vec<String>,
    stop_state: McpRetryStopState,
}
impl McpEvidenceReport {
    pub fn run(&self) -> &str {
        &self.run
    }
    pub fn attempts(&self) -> &[McpEvidenceAttempt] {
        &self.attempts
    }
    pub fn gaps(&self) -> &[String] {
        &self.gaps
    }
    pub fn stop_state(&self) -> McpRetryStopState {
        self.stop_state
    }
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    version: u64,
    run: String,
    events: Vec<Event>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Event {
    seq: u64,
    time: String,
    host: String,
    host_version: String,
    session: String,
    server: String,
    operation: String,
    attempt: String,
    #[serde(deserialize_with = "required_nullable")]
    parent: Option<u64>,
    kind: Kind,
    source: McpEvidenceSource,
    actor: McpEvidenceActor,
    result: Outcome,
    #[serde(deserialize_with = "required_nullable")]
    digest: Option<String>,
}
fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Kind {
    Use,
    Dispatch,
    LaunchRequest,
    Failure,
    Write,
    Notify,
    Reload,
    Schedule,
    Terminate,
    HealthCall,
    ObserveStart,
    ObserveEnd,
    BoundExceeded,
    ManualStop,
}
use McpEvidenceActor as Actor;
use McpEvidenceSource as Source;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Outcome {
    Ok,
    Error,
    Unchanged,
    None,
}

fn alias(s: &str) -> bool {
    let mut bytes = s.bytes();
    s.len() <= 64
        && bytes
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && bytes.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c))
}
fn host_version(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".+-".contains(&c))
}
fn digest(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|hash| {
        hash.len() == 64
            && hash
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}
fn timestamp(s: &str) -> Option<i128> {
    let re = regex::Regex::new(r"^(\d{4})-(\d{2})-(\d{2})[Tt](\d{2}):(\d{2}):(\d{2})(?:\.(\d{1,9}))?([Zz]|[+-]\d{2}:\d{2})$").ok()?;
    let c = re.captures(s)?;
    let n = |i| c.get(i)?.as_str().parse::<i64>().ok();
    let (y, m, d, h, min, sec) = (n(1)?, n(2)?, n(3)?, n(4)?, n(5)?, n(6)?);
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1..=12).contains(&m)
        || d < 1
        || d > days[(m - 1) as usize]
        || h > 23
        || min > 59
        || sec > 59
    {
        return None;
    }
    let zone = c.get(8)?.as_str();
    let offset = if zone.eq_ignore_ascii_case("z") {
        0
    } else {
        let zh: i64 = zone[1..3].parse().ok()?;
        let zm: i64 = zone[4..6].parse().ok()?;
        if zh > 23 || zm > 59 {
            return None;
        }
        (zh * 60 + zm) * if zone.starts_with('-') { -1 } else { 1 }
    };
    let year = y - i64::from(m <= 2);
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = m + if m > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let fraction = c
        .get(7)
        .and_then(|v| {
            format!("{:<0width$}", v.as_str(), width = 9)
                .parse::<i128>()
                .ok()
        })
        .unwrap_or(0);
    Some(
        ((days * 86400 + h * 3600 + min * 60 + sec - offset * 60) as i128) * 1_000_000_000
            + fraction,
    )
}
fn same_scope(a: &Event, b: &Event) -> bool {
    a.host == b.host
        && a.host_version == b.host_version
        && a.session == b.session
        && a.server == b.server
        && a.operation == b.operation
}

/// Analyze a sanitized event file without reading or writing external state.
pub fn analyze_mcp_evidence(bytes: &[u8]) -> Result<McpEvidenceReport, McpEvidenceError> {
    let file: File =
        serde_json::from_slice(bytes).map_err(|_| McpEvidenceError::InvalidEvidence)?;
    if file.version != 1 {
        return Err(McpEvidenceError::UnsupportedVersion);
    }
    if !alias(&file.run) {
        return Err(McpEvidenceError::InvalidEvidence);
    }
    let mut starts = file.events.iter().filter(|e| e.kind == Kind::ObserveStart);
    let scope = starts.next();
    if starts.next().is_some()
        || file
            .events
            .iter()
            .filter(|e| e.kind == Kind::ObserveEnd)
            .count()
            > 1
    {
        return Err(McpEvidenceError::InvalidEvidence);
    }
    let mut previous_seq = None;
    let mut previous_time = None;
    let mut gaps = BTreeSet::new();
    let mut events = BTreeMap::new();
    for e in &file.events {
        if !matches!(
            e.host.as_str(),
            "claude" | "codex" | "copilot" | "opencode" | "agy"
        ) || !host_version(&e.host_version)
            || ![&e.session, &e.server, &e.operation, &e.attempt]
                .iter()
                .all(|s| alias(s))
            || e.digest.as_deref().is_some_and(|d| !digest(d))
        {
            return Err(McpEvidenceError::InvalidEvidence);
        }
        let t = timestamp(&e.time).ok_or(McpEvidenceError::InvalidEvidence)?;
        if previous_seq.is_some_and(|p| e.seq <= p) {
            return Err(McpEvidenceError::InvalidEvidence);
        }
        if previous_seq.is_some_and(|p| e.seq != p + 1) {
            gaps.insert("missing sequence events".to_string());
        }
        previous_seq = Some(e.seq);
        if scope.is_some_and(|start| same_scope(start, e)) {
            if previous_time.is_some_and(|p| t <= p) {
                gaps.insert("overlapping or uncalibrated event times".to_string());
            }
            previous_time = Some(t);
            events.insert(e.seq, e);
        } else {
            gaps.insert(format!("foreign or unscoped event {}", e.seq));
        }
    }
    let mut groups: BTreeMap<(&str, &str, &str, &str, &str, &str), Vec<&Event>> = BTreeMap::new();
    for e in events.values().copied() {
        if matches!(
            e.kind,
            Kind::Use | Kind::Dispatch | Kind::LaunchRequest | Kind::Failure
        ) {
            groups
                .entry((
                    &e.host,
                    &e.host_version,
                    &e.session,
                    &e.server,
                    &e.operation,
                    &e.attempt,
                ))
                .or_default()
                .push(e);
        }
    }
    let global_uncertainty = gaps.iter().any(|gap| {
        gap == "missing sequence events" || gap == "overlapping or uncalibrated event times"
    });
    let mut attempts = Vec::new();
    for ((host, version, session, server, operation, attempt), group) in groups {
        let mut bad_chain = false;
        let mut ancestors = Vec::new();
        for e in &group {
            let mut current = *e;
            while let Some(parent) = current.parent {
                match events.get(&parent) {
                    Some(p) if p.seq < current.seq && same_scope(p, current) => {
                        ancestors.push(*p);
                        current = p;
                    }
                    _ => {
                        gaps.insert("missing or cross-scope parent".into());
                        bad_chain = true;
                        break;
                    }
                }
            }
        }
        let requests: Vec<_> = group
            .iter()
            .filter(|e| matches!(e.kind, Kind::Dispatch | Kind::LaunchRequest))
            .collect();
        if requests.is_empty() {
            gaps.insert("missing attempt request".into());
        }
        if !group.iter().any(|e| e.kind == Kind::Failure) {
            gaps.insert("missing failure event".into());
        }
        let actors: BTreeSet<_> = requests
            .iter()
            .filter_map(|e| match e.actor {
                Actor::Agent => Some(0),
                Actor::Ccync => Some(1),
                _ => None,
            })
            .collect();
        let has_unknown_actor = requests
            .iter()
            .any(|e| matches!(e.actor, Actor::Unknown | Actor::ThirdParty));
        let initiator = if actors.len() == 1 && !has_unknown_actor {
            if actors.contains(&0) {
                McpAttemptInitiator::Agent
            } else {
                McpAttemptInitiator::Ccync
            }
        } else {
            McpAttemptInitiator::Unknown
        };
        let sources: BTreeSet<_> = group
            .iter()
            .chain(ancestors.iter())
            .map(|e| e.source)
            .filter(|s| *s != Source::Unknown)
            .collect();
        let readers: BTreeSet<_> = group
            .iter()
            .chain(ancestors.iter())
            .filter(|e| matches!(e.kind, Kind::Use | Kind::Dispatch | Kind::LaunchRequest))
            .map(|e| e.actor)
            .collect();
        let mut trigger = McpAttemptTrigger::Unknown;
        if !global_uncertainty
            && !bad_chain
            && !requests.is_empty()
            && !has_unknown_actor
            && sources.len() <= 1
        {
            if ancestors.iter().any(|e| {
                e.kind == Kind::Reload && e.actor == Actor::Ccync && e.result == Outcome::Ok
            }) {
                trigger = McpAttemptTrigger::ConfigReload;
            } else if initiator == McpAttemptInitiator::Ccync {
                trigger = McpAttemptTrigger::DirectRequest;
            } else if initiator == McpAttemptInitiator::Agent
                && ancestors
                    .iter()
                    .any(|e| e.kind == Kind::Failure && e.attempt != attempt)
            {
                trigger = McpAttemptTrigger::AgentRetry;
            } else if initiator == McpAttemptInitiator::Agent
                && ancestors.iter().any(|e| e.kind == Kind::Use)
            {
                trigger = McpAttemptTrigger::UserOperation;
            }
        }
        if trigger == McpAttemptTrigger::Unknown {
            gaps.insert("trigger not established".into());
        }
        attempts.push(McpEvidenceAttempt {
            host: host.into(),
            host_version: version.into(),
            session: session.into(),
            server: server.into(),
            operation: operation.into(),
            attempt: attempt.into(),
            events: group.iter().map(|e| e.seq).collect(),
            initiator,
            trigger,
            sources: sources.into_iter().collect(),
            readers: readers.into_iter().collect(),
        });
    }
    let complete = scope.is_some()
        && file
            .events
            .first()
            .is_some_and(|e| e.kind == Kind::ObserveStart)
        && file.events.last().is_some_and(|e| {
            e.kind == Kind::ObserveEnd
                && e.result == Outcome::Ok
                && scope.is_some_and(|start| same_scope(start, e))
        });
    let stop_state = if !complete {
        gaps.insert("observation interval incomplete".into());
        McpRetryStopState::Truncated
    } else {
        match file
            .events
            .iter()
            .rev()
            .skip(1)
            .find(|e| {
                scope.is_some_and(|start| same_scope(start, e))
                    && matches!(
                        e.kind,
                        Kind::LaunchRequest
                            | Kind::Failure
                            | Kind::Terminate
                            | Kind::BoundExceeded
                            | Kind::ManualStop
                    )
            })
            .map(|e| e.kind)
        {
            Some(Kind::ManualStop) => McpRetryStopState::ManualIntervention,
            Some(Kind::BoundExceeded) => McpRetryStopState::OutOfBounds,
            Some(Kind::Terminate) => McpRetryStopState::Terminated,
            Some(Kind::LaunchRequest | Kind::Failure) => McpRetryStopState::StillRetrying,
            _ => {
                gaps.insert("stop evidence not established".into());
                McpRetryStopState::Unknown
            }
        }
    };
    Ok(McpEvidenceReport {
        run: file.run,
        attempts,
        gaps: gaps.into_iter().collect(),
        stop_state,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn event(seq: u64, kind: &str, actor: &str, parent: Option<u64>, attempt: u64) -> Value {
        json!({"seq":seq,"time":format!("2026-01-01T00:00:{seq:02}Z"),"host":"codex","hostVersion":"1.0","session":"s1","server":"server1","operation":"op1","attempt":attempt.to_string(),"parent":parent,"kind":kind,"source":"native-config","actor":actor,"result":"ok","digest":null})
    }
    fn analyze(events: Vec<Value>) -> McpEvidenceReport {
        analyze_mcp_evidence(
            json!({"version":1,"run":"run1","events":events})
                .to_string()
                .as_bytes(),
        )
        .unwrap()
    }
    #[test]
    fn correlates_request_failure_and_retry_without_processes() {
        let r = analyze(vec![
            event(1, "observe-start", "unknown", None, 0),
            event(2, "use", "agent", None, 1),
            event(3, "dispatch", "agent", Some(2), 1),
            event(4, "launch-request", "agent", Some(3), 1),
            event(5, "failure", "agent", Some(4), 1),
            event(6, "dispatch", "agent", Some(5), 2),
            event(7, "failure", "agent", Some(6), 2),
            event(8, "bound-exceeded", "agent", Some(7), 2),
            event(9, "observe-end", "unknown", None, 0),
        ]);
        assert_eq!(r.attempts().len(), 2);
        assert_eq!(r.attempts()[0].trigger(), McpAttemptTrigger::UserOperation);
        assert_eq!(r.attempts()[1].trigger(), McpAttemptTrigger::AgentRetry);
        assert_eq!(r.stop_state(), McpRetryStopState::OutOfBounds);
    }
    #[test]
    fn direct_indirect_mixed_and_uncertain() {
        let direct = analyze(vec![
            event(1, "observe-start", "unknown", None, 0),
            event(2, "dispatch", "ccync", None, 1),
            event(3, "failure", "ccync", Some(2), 1),
            event(4, "terminate", "ccync", Some(3), 1),
            event(5, "observe-end", "unknown", None, 0),
        ]);
        assert_eq!(direct.stop_state(), McpRetryStopState::Terminated);
        assert_eq!(
            direct.attempts()[0].trigger(),
            McpAttemptTrigger::DirectRequest
        );
        let indirect = analyze(vec![
            event(1, "observe-start", "unknown", None, 0),
            event(2, "reload", "ccync", None, 0),
            event(3, "dispatch", "agent", Some(2), 1),
            event(4, "failure", "agent", Some(3), 1),
            event(5, "manual-stop", "unknown", Some(4), 1),
            event(6, "observe-end", "unknown", None, 0),
        ]);
        assert_eq!(indirect.stop_state(), McpRetryStopState::ManualIntervention);
        assert_eq!(
            indirect.attempts()[0].trigger(),
            McpAttemptTrigger::ConfigReload
        );
        let mixed = analyze(vec![
            event(1, "observe-start", "unknown", None, 0),
            event(2, "dispatch", "ccync", None, 1),
            event(3, "failure", "ccync", Some(2), 1),
            event(4, "dispatch", "agent", Some(3), 2),
            event(5, "failure", "agent", Some(4), 2),
            event(6, "observe-end", "unknown", None, 0),
        ]);
        assert_eq!(mixed.stop_state(), McpRetryStopState::StillRetrying);
        assert_eq!(mixed.attempts()[1].trigger(), McpAttemptTrigger::AgentRetry);
        let mut other = event(2, "use", "third-party", None, 1);
        other["source"] = json!("personal-list");
        let uncertain = analyze(vec![
            event(1, "observe-start", "unknown", None, 0),
            other,
            event(3, "dispatch", "agent", Some(2), 1),
            event(4, "failure", "agent", Some(3), 1),
        ]);
        assert_eq!(
            uncertain.attempts()[0].initiator(),
            McpAttemptInitiator::Agent
        );
        assert_eq!(
            uncertain.attempts()[0].trigger(),
            McpAttemptTrigger::Unknown
        );
        assert!(uncertain.attempts()[0]
            .readers()
            .contains(&McpEvidenceActor::ThirdParty));
        assert!(!uncertain.gaps().is_empty());
        assert_eq!(uncertain.stop_state(), McpRetryStopState::Truncated);
    }
    #[test]
    fn rejects_extra_fields_and_secrets() {
        let mut e = event(1, "dispatch", "agent", None, 1);
        e["command"] = json!("secret-password");
        let raw = json!({"version":1,"run":"run1","events":[e]}).to_string();
        assert!(analyze_mcp_evidence(raw.as_bytes()).is_err());
    }
    #[test]
    fn isolates_the_observed_operation() {
        for (field, value) in [
            ("host", "claude"),
            ("hostVersion", "2.0"),
            ("session", "s2"),
            ("server", "server2"),
            ("operation", "op2"),
        ] {
            let mut foreign = event(4, "manual-stop", "unknown", None, 2);
            foreign[field] = json!(value);
            let report = analyze(vec![
                event(1, "observe-start", "unknown", None, 0),
                event(2, "dispatch", "ccync", None, 1),
                event(3, "failure", "ccync", Some(2), 1),
                foreign,
                event(5, "observe-end", "unknown", None, 0),
            ]);
            assert_eq!(report.attempts().len(), 1, "{field}");
            assert_eq!(
                report.attempts()[0].trigger(),
                McpAttemptTrigger::DirectRequest
            );
            assert_eq!(report.stop_state(), McpRetryStopState::StillRetrying);
            assert!(report.gaps().iter().any(|gap| gap.contains("event 4")));
        }
        let report = analyze(vec![
            event(1, "dispatch", "agent", None, 1),
            event(2, "failure", "agent", Some(1), 1),
        ]);
        assert!(report.attempts().is_empty());
        assert_eq!(report.stop_state(), McpRetryStopState::Truncated);
        assert!(report.gaps().iter().any(|gap| gap.contains("event 1")));
        assert!(report.gaps().iter().any(|gap| gap.contains("event 2")));
        let mut end = event(2, "observe-end", "unknown", None, 0);
        end["session"] = json!("s2");
        assert_eq!(
            analyze(vec![event(1, "observe-start", "unknown", None, 0), end]).stop_state(),
            McpRetryStopState::Truncated
        );
    }
    #[test]
    fn rejects_duplicate_observation_boundaries() {
        for kind in ["observe-start", "observe-end"] {
            let raw = json!({"version":1,"run":"run1","events":[
                event(1, kind, "unknown", None, 0),
                event(2, kind, "unknown", None, 0)
            ]})
            .to_string();
            assert!(analyze_mcp_evidence(raw.as_bytes()).is_err());
        }
    }
    #[test]
    fn complete_intervals_use_last_relevant_event() {
        for (kind, state) in [
            ("bound-exceeded", McpRetryStopState::OutOfBounds),
            ("terminate", McpRetryStopState::Terminated),
            ("manual-stop", McpRetryStopState::ManualIntervention),
            ("failure", McpRetryStopState::StillRetrying),
            ("launch-request", McpRetryStopState::StillRetrying),
        ] {
            let report = analyze(vec![
                event(1, "observe-start", "unknown", None, 0),
                event(2, kind, "agent", None, 1),
                event(3, "observe-end", "unknown", None, 0),
            ]);
            assert_eq!(report.stop_state(), state);
        }
        let report = analyze(vec![
            event(1, "observe-start", "unknown", None, 0),
            event(2, "dispatch", "agent", None, 1),
            event(3, "observe-end", "unknown", None, 0),
        ]);
        assert_eq!(report.stop_state(), McpRetryStopState::Unknown);
        assert_eq!(report.attempts().len(), 1);
        assert!(report
            .gaps()
            .iter()
            .any(|gap| gap == "stop evidence not established"));
        let mut end = event(2, "observe-end", "unknown", None, 0);
        end["result"] = json!("error");
        assert_eq!(
            analyze(vec![event(1, "observe-start", "unknown", None, 0), end]).stop_state(),
            McpRetryStopState::Truncated
        );
    }
    #[test]
    fn counts_use_only_and_deduplicates_attempt_events() {
        let empty = analyze(vec![
            event(1, "observe-start", "unknown", None, 0),
            event(2, "observe-end", "unknown", None, 0),
        ]);
        assert!(empty.attempts().is_empty());
        assert_eq!(empty.stop_state(), McpRetryStopState::Unknown);
        assert!(empty
            .gaps()
            .iter()
            .any(|gap| gap == "stop evidence not established"));

        let report = analyze(vec![
            event(1, "observe-start", "unknown", None, 0),
            event(2, "use", "agent", None, 1),
            event(3, "use", "agent", None, 1),
            event(4, "use", "agent", None, 2),
            event(5, "dispatch", "agent", Some(4), 2),
            event(6, "observe-end", "unknown", None, 0),
        ]);
        assert_eq!(report.attempts().len(), 2);
        assert_eq!(report.attempts()[0].events(), &[2, 3]);
        assert_eq!(report.attempts()[1].events(), &[4, 5]);
        assert_eq!(report.stop_state(), McpRetryStopState::Unknown);
    }
    #[test]
    fn validates_shapes_before_analysis() {
        for (field, value) in [
            ("host", "host1"),
            ("hostVersion", "1_0"),
            ("session", "ghp_PlantedSecret123"),
            ("server", "sk live 1234"),
            ("operation", "-bad"),
            ("attempt", "UPPER"),
            ("digest", "abcd"),
        ] {
            let mut e = event(1, "dispatch", "agent", None, 1);
            e[field] = json!(value);
            let raw = json!({"version":1,"run":"run1","events":[e]}).to_string();
            assert!(analyze_mcp_evidence(raw.as_bytes()).is_err(), "{field}");
        }
        let mut e = event(1, "dispatch", "agent", None, 1);
        e["digest"] = json!(format!("sha256:{}", "a".repeat(64)));
        let raw = json!({"version":1,"run":"run1","events":[e]}).to_string();
        assert!(analyze_mcp_evidence(raw.as_bytes()).is_ok());
        let raw = json!({"version":1,"run":"Bad","events":[]}).to_string();
        assert!(analyze_mcp_evidence(raw.as_bytes()).is_err());
    }
}
