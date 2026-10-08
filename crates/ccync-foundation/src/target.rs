//! Projection target vocabulary — the seven independently addressable
//! projection destinations.
//!
//! `ProjectionTargetId` is the single typed identifier for a projection
//! target. `CANONICAL_PROJECTION_TARGETS` is the ordered table other
//! projection-domain code keys off of — order matters, it is the order
//! targets are written, reported, and displayed.

use std::fmt;
use std::str::FromStr;
use thiserror::Error;

/// A single independently addressable projection destination.
///
/// `PartialOrd`/`Ord` order by declaration order above and back a `BTreeMap`/
/// `BTreeSet` key (evidence carriers keyed by target).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProjectionTargetId {
    Claude,
    Codex,
    Copilot,
    Opencode,
    AgyCli,
    AgyIde,
    AgyGui,
}

impl ProjectionTargetId {
    /// Canonical string form, as written to config and shown in CLI output.
    pub fn as_str(self) -> &'static str {
        match self {
            ProjectionTargetId::Claude => "claude",
            ProjectionTargetId::Codex => "codex",
            ProjectionTargetId::Copilot => "copilot",
            ProjectionTargetId::Opencode => "opencode",
            ProjectionTargetId::AgyCli => "agy-cli",
            ProjectionTargetId::AgyIde => "agy-ide",
            ProjectionTargetId::AgyGui => "agy-gui",
        }
    }
}

impl fmt::Display for ProjectionTargetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when a string does not match any known projection target.
#[derive(Debug, Error)]
#[error("unknown projection target: {0}")]
pub struct ParseProjectionTargetError(String);

impl FromStr for ProjectionTargetId {
    type Err = ParseProjectionTargetError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "claude" => Ok(ProjectionTargetId::Claude),
            "codex" => Ok(ProjectionTargetId::Codex),
            "copilot" => Ok(ProjectionTargetId::Copilot),
            "opencode" => Ok(ProjectionTargetId::Opencode),
            "agy-cli" => Ok(ProjectionTargetId::AgyCli),
            "agy-ide" => Ok(ProjectionTargetId::AgyIde),
            "agy-gui" => Ok(ProjectionTargetId::AgyGui),
            other => Err(ParseProjectionTargetError(other.to_string())),
        }
    }
}

/// All seven projection targets, in canonical order.
pub const CANONICAL_PROJECTION_TARGETS: &[ProjectionTargetId] = &[
    ProjectionTargetId::Claude,
    ProjectionTargetId::Codex,
    ProjectionTargetId::Copilot,
    ProjectionTargetId::Opencode,
    ProjectionTargetId::AgyCli,
    ProjectionTargetId::AgyIde,
    ProjectionTargetId::AgyGui,
];

impl ProjectionTargetId {
    /// The single agent family this target belongs to.
    ///
    /// Derived by scanning `AgentId::expand` for every canonical agent —
    /// `expand` is the one authoritative mapping between agents and
    /// targets, so this reverse lookup cannot drift from it.
    pub fn agent_family(self) -> AgentId {
        CANONICAL_AGENTS
            .iter()
            .copied()
            .find(|agent| agent.expand().contains(&self))
            .expect("every projection target belongs to exactly one agent family")
    }
}

/// A single coding-agent choice a user can select for projection.
///
/// Four of the five choices are "identity families" that expand to exactly
/// one projection target of the matching name. `Agy` is the exception — it
/// expands to all three Antigravity face targets. See [`AgentId::expand`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentId {
    Claude,
    Codex,
    Copilot,
    Opencode,
    Agy,
}

impl AgentId {
    /// Canonical string form, as written to config and shown in CLI output.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentId::Claude => "claude",
            AgentId::Codex => "codex",
            AgentId::Copilot => "copilot",
            AgentId::Opencode => "opencode",
            AgentId::Agy => "agy",
        }
    }

    /// The projection target(s) this agent expands to.
    ///
    /// This is the single authoritative agent-to-target mapping —
    /// `ProjectionTargetId::agent_family` derives its reverse lookup from
    /// this table, so the two directions cannot diverge.
    pub fn expand(self) -> &'static [ProjectionTargetId] {
        match self {
            AgentId::Claude => &[ProjectionTargetId::Claude],
            AgentId::Codex => &[ProjectionTargetId::Codex],
            AgentId::Copilot => &[ProjectionTargetId::Copilot],
            AgentId::Opencode => &[ProjectionTargetId::Opencode],
            AgentId::Agy => &[
                ProjectionTargetId::AgyCli,
                ProjectionTargetId::AgyIde,
                ProjectionTargetId::AgyGui,
            ],
        }
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when a string does not match any known agent.
#[derive(Debug, Error)]
#[error("unknown agent: {0}")]
pub struct ParseAgentError(String);

impl FromStr for AgentId {
    type Err = ParseAgentError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "claude" => Ok(AgentId::Claude),
            "codex" => Ok(AgentId::Codex),
            "copilot" => Ok(AgentId::Copilot),
            "opencode" => Ok(AgentId::Opencode),
            "agy" => Ok(AgentId::Agy),
            other => Err(ParseAgentError(other.to_string())),
        }
    }
}

/// All five agent choices, in canonical order.
pub const CANONICAL_AGENTS: &[AgentId] = &[
    AgentId::Claude,
    AgentId::Codex,
    AgentId::Copilot,
    AgentId::Opencode,
    AgentId::Agy,
];

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── CANONICAL_PROJECTION_TARGETS ─────────────────────────────────────────

    #[test]
    fn canonical_table_emits_seven_strings_in_canonical_order() {
        let strings: Vec<&str> = CANONICAL_PROJECTION_TARGETS
            .iter()
            .map(|target| target.as_str())
            .collect();
        assert_eq!(
            strings,
            vec!["claude", "codex", "copilot", "opencode", "agy-cli", "agy-ide", "agy-gui",]
        );
    }

    #[test]
    fn canonical_table_has_exactly_seven_entries() {
        assert_eq!(CANONICAL_PROJECTION_TARGETS.len(), 7);
    }

    // ── Display / as_str ─────────────────────────────────────────────────────

    #[test]
    fn display_matches_as_str() {
        for target in CANONICAL_PROJECTION_TARGETS {
            assert_eq!(target.to_string(), target.as_str());
        }
    }

    // ── FromStr ───────────────────────────────────────────────────────────────

    #[test]
    fn parse_round_trips_every_canonical_string() {
        for target in CANONICAL_PROJECTION_TARGETS {
            let parsed: ProjectionTargetId = target.as_str().parse().unwrap();
            assert_eq!(parsed, *target);
        }
    }

    #[test]
    fn parse_rejects_unknown_string() {
        let result = "not-a-target".parse::<ProjectionTargetId>();
        assert!(result.is_err());
    }

    #[test]
    fn parse_rejects_bare_gemini_and_antigravity() {
        // No bare `gemini` or `antigravity` spellings are recognized —
        // only the fully qualified target strings. `gemini-cli` itself is
        // retired and no longer parses either.
        assert!("gemini".parse::<ProjectionTargetId>().is_err());
        assert!("antigravity".parse::<ProjectionTargetId>().is_err());
        assert!("gemini-cli".parse::<ProjectionTargetId>().is_err());
    }

    // ── CANONICAL_AGENTS ─────────────────────────────────────────────────────

    #[test]
    fn canonical_agents_table_emits_five_strings_in_canonical_order() {
        let strings: Vec<&str> = CANONICAL_AGENTS
            .iter()
            .map(|agent| agent.as_str())
            .collect();
        assert_eq!(
            strings,
            vec!["claude", "codex", "copilot", "opencode", "agy"]
        );
    }

    #[test]
    fn canonical_agents_table_has_exactly_five_entries() {
        assert_eq!(CANONICAL_AGENTS.len(), 5);
    }

    // ── AgentId Display / as_str ─────────────────────────────────────────────

    #[test]
    fn agent_display_matches_as_str() {
        for agent in CANONICAL_AGENTS {
            assert_eq!(agent.to_string(), agent.as_str());
        }
    }

    // ── AgentId FromStr ──────────────────────────────────────────────────────

    #[test]
    fn agent_parse_round_trips_every_canonical_string() {
        for agent in CANONICAL_AGENTS {
            let parsed: AgentId = agent.as_str().parse().unwrap();
            assert_eq!(parsed, *agent);
        }
    }

    #[test]
    fn agent_parse_rejects_unknown_string() {
        let result = "not-an-agent".parse::<AgentId>();
        assert!(result.is_err());
    }

    // ── AgentId::expand ──────────────────────────────────────────────────────

    #[test]
    fn agy_expands_to_exactly_three_agy_targets_in_order() {
        assert_eq!(
            AgentId::Agy.expand(),
            &[
                ProjectionTargetId::AgyCli,
                ProjectionTargetId::AgyIde,
                ProjectionTargetId::AgyGui,
            ]
        );
    }

    #[test]
    fn single_agents_expand_to_exactly_one_matching_identity_target() {
        let identity_pairs = [
            (AgentId::Claude, ProjectionTargetId::Claude),
            (AgentId::Codex, ProjectionTargetId::Codex),
            (AgentId::Copilot, ProjectionTargetId::Copilot),
            (AgentId::Opencode, ProjectionTargetId::Opencode),
        ];
        for (agent, target) in identity_pairs {
            assert_eq!(agent.expand(), &[target]);
        }
    }

    // ── ProjectionTargetId::agent_family ─────────────────────────────────────

    #[test]
    fn agent_family_maps_every_target_to_its_correct_agent() {
        let expected = [
            (ProjectionTargetId::Claude, AgentId::Claude),
            (ProjectionTargetId::Codex, AgentId::Codex),
            (ProjectionTargetId::Copilot, AgentId::Copilot),
            (ProjectionTargetId::Opencode, AgentId::Opencode),
            (ProjectionTargetId::AgyCli, AgentId::Agy),
            (ProjectionTargetId::AgyIde, AgentId::Agy),
            (ProjectionTargetId::AgyGui, AgentId::Agy),
        ];
        for (target, agent) in expected {
            assert_eq!(target.agent_family(), agent);
        }
    }

    // ── expand + agent_family round-trip probe ───────────────────────────────

    #[test]
    fn expand_and_agent_family_round_trip_for_every_agent_and_target() {
        // Forward then back: every target produced by an agent's `expand`
        // must report that same agent as its `agent_family`.
        for agent in CANONICAL_AGENTS {
            for target in agent.expand() {
                assert_eq!(target.agent_family(), *agent);
            }
        }
        // Back then forward: every canonical target's `agent_family` must
        // list that target among its own `expand` output.
        for target in CANONICAL_PROJECTION_TARGETS {
            let agent = target.agent_family();
            assert!(agent.expand().contains(target));
        }
    }
}
