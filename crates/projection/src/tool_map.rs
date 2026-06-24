//! Invocation mode → Codex `sandbox_mode` mapping.
//!
//! ccync projects plugin agents generically (their Claude tool grants are copied
//! verbatim from the agent frontmatter — no abstract-token rewrite). The only
//! per-runtime permission mapping still needed is the Codex `sandbox_mode` string,
//! derived from the invocation mode.

/// Invocation mode for an agent projection.
///
/// Selects the Codex `sandbox_mode`: consult/read-only vs build/workspace-write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvocationMode {
    /// Planning/consult mode: Codex `sandbox_mode = "read-only"`.
    Consult,
    /// Implementation/build mode: Codex `sandbox_mode = "workspace-write"`.
    Build,
}

/// Map invocation mode to the Codex `sandbox_mode` value.
pub fn invocation_mode_to_codex_sandbox(mode: InvocationMode) -> &'static str {
    match mode {
        InvocationMode::Consult => "read-only",
        InvocationMode::Build => "workspace-write",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consult_mode_maps_to_read_only() {
        assert_eq!(
            invocation_mode_to_codex_sandbox(InvocationMode::Consult),
            "read-only"
        );
    }

    #[test]
    fn build_mode_maps_to_workspace_write() {
        assert_eq!(
            invocation_mode_to_codex_sandbox(InvocationMode::Build),
            "workspace-write"
        );
    }
}
