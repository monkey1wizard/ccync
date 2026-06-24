//! Runtime selection utilities.
//!
//! Ports `Get-DefaultPrimaryRuntime` and `Get-PipelinePhaseRole` from
//! `scripts/common/Common.{ps1,sh}`.

/// All recognized runtime keys in canonical preference order (D-13).
///
/// Antigravity is split into three distinct surfaces (`agy-cli`, `agy-ide`,
/// `agy-gui`) and Gemini CLI is its own product (`gemini-cli`). The key is the
/// canonical identifier; the on-disk directory differs per surface (see
/// `docs/naming.md` runtime-key table).
pub const VALID_RUNTIMES: &[&str] = &[
    "claude",
    "codex",
    "copilot",
    "gemini-cli",
    "opencode",
    "agy-cli",
    "agy-ide",
    "agy-gui",
];

/// Return the default primary runtime from a slice of selected runtime keys.
///
/// Prefers runtimes in this order: `copilot`, `agy-cli`, `agy-ide`, `agy-gui`,
/// `codex`, `claude`, `opencode`, `gemini-cli`.
/// Returns `None` when `selected` is empty or contains no recognized runtimes.
pub fn default_primary_runtime<'a>(selected: &[&'a str]) -> Option<&'a str> {
    const PREFERRED: &[&str] = &[
        "copilot",
        "agy-cli",
        "agy-ide",
        "agy-gui",
        "codex",
        "claude",
        "opencode",
        "gemini-cli",
    ];
    PREFERRED
        .iter()
        .copied()
        .find(|candidate| selected.contains(candidate))
}

/// Map a pipeline phase name to its role constant.
///
/// Ports `Get-PipelinePhaseRole` from Common.ps1.
/// Comparison is case-insensitive. Returns `None` for unrecognized phases.
pub fn pipeline_phase_role(phase: &str) -> Option<&'static str> {
    match phase.to_lowercase().as_str() {
        "implement" => Some("CODER"),
        "test" => Some("TESTER"),
        "audit" => Some("AUDITOR"),
        _ => None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── VALID_RUNTIMES ────────────────────────────────────────────────────────

    #[test]
    fn valid_runtimes_contains_all_eight() {
        assert_eq!(VALID_RUNTIMES.len(), 8);
        for key in &[
            "claude",
            "codex",
            "copilot",
            "gemini-cli",
            "opencode",
            "agy-cli",
            "agy-ide",
            "agy-gui",
        ] {
            assert!(
                VALID_RUNTIMES.contains(key),
                "VALID_RUNTIMES missing: {key}"
            );
        }
        // No bare gemini / antigravity keys (D-13).
        assert!(
            !VALID_RUNTIMES.contains(&"gemini"),
            "bare gemini must be gone"
        );
        assert!(
            !VALID_RUNTIMES.contains(&"antigravity"),
            "bare antigravity must be gone"
        );
    }

    // ── default_primary_runtime ───────────────────────────────────────────────

    #[test]
    fn prefers_copilot_when_present() {
        let selected = &["copilot", "claude", "codex"];
        assert_eq!(default_primary_runtime(selected), Some("copilot"));
    }

    #[test]
    fn falls_to_agy_cli_when_no_copilot() {
        let selected = &["agy-cli", "codex", "claude"];
        assert_eq!(default_primary_runtime(selected), Some("agy-cli"));
    }

    #[test]
    fn falls_to_codex_when_no_copilot_or_agy() {
        let selected = &["codex", "opencode"];
        assert_eq!(default_primary_runtime(selected), Some("codex"));
    }

    #[test]
    fn falls_to_claude_when_earlier_absent() {
        let selected = &["claude", "opencode", "gemini-cli"];
        assert_eq!(default_primary_runtime(selected), Some("claude"));
    }

    #[test]
    fn falls_to_opencode_when_only_opencode_and_gemini_cli() {
        let selected = &["opencode", "gemini-cli"];
        assert_eq!(default_primary_runtime(selected), Some("opencode"));
    }

    #[test]
    fn falls_to_gemini_cli_when_only_gemini_cli() {
        let selected = &["gemini-cli"];
        assert_eq!(default_primary_runtime(selected), Some("gemini-cli"));
    }

    #[test]
    fn returns_none_for_empty_selection() {
        let selected: &[&str] = &[];
        assert_eq!(default_primary_runtime(selected), None);
    }

    #[test]
    fn returns_none_for_unknown_runtimes_only() {
        let selected = &["xmachine", "unknown-tool"];
        assert_eq!(default_primary_runtime(selected), None);
    }

    // ── pipeline_phase_role ───────────────────────────────────────────────────

    #[test]
    fn implement_maps_to_coder() {
        assert_eq!(pipeline_phase_role("implement"), Some("CODER"));
    }

    #[test]
    fn implement_is_case_insensitive() {
        assert_eq!(pipeline_phase_role("IMPLEMENT"), Some("CODER"));
        assert_eq!(pipeline_phase_role("Implement"), Some("CODER"));
    }

    #[test]
    fn test_maps_to_tester() {
        assert_eq!(pipeline_phase_role("test"), Some("TESTER"));
    }

    #[test]
    fn audit_maps_to_auditor() {
        assert_eq!(pipeline_phase_role("audit"), Some("AUDITOR"));
    }

    #[test]
    fn unknown_phase_returns_none() {
        assert_eq!(pipeline_phase_role("deploy"), None);
        assert_eq!(pipeline_phase_role(""), None);
        assert_eq!(pipeline_phase_role("review"), None);
        assert_eq!(pipeline_phase_role("security"), None);
        assert_eq!(pipeline_phase_role("verify"), None);
    }
}
