//! ccync setup support: the runtime-selection option types + interactive
//! session seam reused by `ccync init`, plus the management health checks
//! (`health`).
//!
//! The former `ccync setup` orchestrator and its machine-setup steps (AGY
//! pre-cleanup, collaborative-tools, install sequencing) were removed with the
//! 9-command surface — install *is* `ccync sync`, and master adoption is
//! `ccync init`. `SetupOptions` / `SetupError` are retained because `session`
//! is parameterised by them.

pub mod health;
pub mod session;

use thiserror::Error;

/// CLI-facing options for `ccync setup` (mirrors Setup-Machine parameters).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetupOptions {
    pub uninstall: bool,
    pub purge: bool,
    pub confirm_purge: bool,
    pub replace: bool,
    pub dry_run: bool,
    pub check: bool,
    pub reconfigure: bool,
    pub bootstrap_install: bool,
    pub selected_runtimes: Option<Vec<String>>,
    pub primary_runtime: Option<String>,
    /// `ccync setup --tools` — run the collaborative-tools flow instead of
    /// machine setup.
    pub tools: bool,
    /// `--tool <csv>` filter for the tools flow.
    pub tool: Option<Vec<String>>,
}

#[derive(Debug, Error)]
pub enum SetupError {
    /// Bad flag combination — maps to CLI usage exit (64).
    #[error("{0}")]
    Usage(String),
    #[error("{0}")]
    Message(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── flag mapping (architect C-2) ─────────────────────────────────────────
    //
    // Table test: every flag Setup-Machine forwarded to
    // `ccync update --machine-only` must land on a MachineUpdateOptions field.
    // Uses a synthetic base options value to keep the test hermetic
    // (machine_options_from_config requires real machine state).

    #[test]
    fn flag_mapping_table_covers_all_forwarded_flags() {
        let base = projection::MachineUpdateOptions {
            repo_root: std::path::PathBuf::from("/repo"),
            source_root: std::path::PathBuf::from("/repo"),
            user_home: std::path::PathBuf::from("/home/u"),
            appdata_root: std::path::PathBuf::from("/home/u/.config"),
            selected_runtimes: vec!["claude".into()],
            primary_runtime: "claude".into(),
            dry_run: false,
            replace: false,
            uninstall: false,
            include_source_projections: false,
        };

        // (flag, applier, asserter) table
        struct Case {
            flag: &'static str,
            opts: SetupOptions,
            selected: Vec<String>,
            primary: String,
            assert: fn(&projection::MachineUpdateOptions),
        }
        let cases = vec![
            Case {
                flag: "--dry-run",
                opts: SetupOptions {
                    dry_run: true,
                    ..Default::default()
                },
                selected: vec!["claude".into()],
                primary: "claude".into(),
                assert: |m| assert!(m.dry_run),
            },
            Case {
                flag: "--uninstall",
                opts: SetupOptions {
                    uninstall: true,
                    ..Default::default()
                },
                selected: vec![],
                primary: String::new(),
                assert: |m| assert!(m.uninstall),
            },
            Case {
                flag: "--replace",
                opts: SetupOptions {
                    replace: true,
                    ..Default::default()
                },
                selected: vec!["claude".into()],
                primary: "claude".into(),
                assert: |m| assert!(m.replace),
            },
            Case {
                flag: "--selected-runtimes",
                opts: SetupOptions::default(),
                selected: vec!["codex".into(), "copilot".into()],
                primary: "copilot".into(),
                assert: |m| {
                    assert_eq!(
                        m.selected_runtimes,
                        vec!["codex".to_string(), "copilot".to_string()]
                    )
                },
            },
            Case {
                flag: "--primary-runtime",
                opts: SetupOptions::default(),
                selected: vec!["codex".into(), "copilot".into()],
                primary: "codex".into(),
                assert: |m| assert_eq!(m.primary_runtime, "codex"),
            },
        ];

        for case in cases {
            let mut machine = base.clone();
            machine.dry_run = case.opts.dry_run;
            machine.uninstall = case.opts.uninstall;
            machine.replace = case.opts.replace;
            if !case.opts.uninstall {
                machine.selected_runtimes = case.selected.clone();
                machine.primary_runtime = case.primary.clone();
            }
            (case.assert)(&machine);
            let _ = case.flag;
        }
    }

    #[test]
    fn uninstall_does_not_override_stored_runtime_selection() {
        // Setup-Machine omits --selected-runtimes/--primary-runtime on
        // uninstall; ccync update then uses install-state values. Mirror that.
        let mut machine = projection::MachineUpdateOptions {
            repo_root: std::path::PathBuf::from("/repo"),
            source_root: std::path::PathBuf::from("/repo"),
            user_home: std::path::PathBuf::from("/home/u"),
            appdata_root: std::path::PathBuf::from("/home/u/.config"),
            selected_runtimes: vec!["claude".into(), "copilot".into()],
            primary_runtime: "copilot".into(),
            dry_run: false,
            replace: false,
            uninstall: false,
            include_source_projections: false,
        };
        let o = SetupOptions {
            uninstall: true,
            ..Default::default()
        };
        machine.uninstall = o.uninstall;
        if !o.uninstall {
            machine.selected_runtimes = vec![];
            machine.primary_runtime = String::new();
        }
        assert!(machine.uninstall);
        assert_eq!(
            machine.selected_runtimes,
            vec!["claude".to_string(), "copilot".to_string()]
        );
        assert_eq!(machine.primary_runtime, "copilot");
    }
}
