//! Acceptance probes for user-scope instruction file disclosure in init
//! and verbs running the projection core.
//!
//! `run_unified_projection` has four production call sites:
//! 1. `lifecycle::cmd_sync` (and `refresh`)
//! 2. `init::cmd_init`
//! 3. `plugin::cmd_add`
//! 4. `plugin::cmd_remove`
//!
//! `init` passes `skip_first_run_gate = true`, so the first-run visibility gate
//! never fires there. `init` must therefore name user-scope instruction files
//! (`AGENTS.md` / `CLAUDE.md`) in its own output before projecting.
//! The add and remove paths pass `skip_first_run_gate = false`, so they already
//! trigger the gate.

#[test]
fn init_discloses_user_scope_instruction_files_before_projection() {
    let init_source = include_str!("init.rs");
    let cmd_init_fn = init_source
        .split_once("pub(crate) fn cmd_init(")
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.split_once("super::lifecycle::run_unified_projection("))
        .map(|(pre, _)| pre)
        .expect("init must declare cmd_init calling run_unified_projection");

    assert!(
        cmd_init_fn.contains("user_scope_instruction_path")
            || cmd_init_fn.contains("AGENTS.md"),
        "init does not name the user-scope instruction files (AGENTS.md) it is about to write before projecting"
    );
}

#[test]
fn init_call_site_skips_first_run_gate_necessitating_own_disclosure() {
    let init_source = include_str!("init.rs");
    assert!(
        init_source.contains("super::lifecycle::run_unified_projection(false, true, true, None)"),
        "init must pass skip_first_run_gate = true, necessitating its own disclosure"
    );
}

#[test]
fn add_and_remove_call_sites_do_not_skip_first_run_gate() {
    let plugin_source = include_str!("plugin.rs");

    // cmd_add calls run_unified_projection with skip_first_run_gate = false
    let (_, add_body) = plugin_source
        .split_once("pub(crate) fn cmd_add(")
        .expect("plugin.rs must define cmd_add");
    let (_, add_call) = add_body
        .split_once("super::lifecycle::run_unified_projection(")
        .expect("cmd_add must call run_unified_projection");
    assert!(
        add_call.contains("ADD_GATE_ABORT_CONTEXT"),
        "cmd_add must pass ADD_GATE_ABORT_CONTEXT to run_unified_projection"
    );

    // cmd_remove calls run_unified_projection with skip_first_run_gate = false
    let (_, remove_body) = plugin_source
        .split_once("pub(crate) fn cmd_remove(")
        .expect("plugin.rs must define cmd_remove");
    let (_, remove_call) = remove_body
        .split_once("super::lifecycle::run_unified_projection(")
        .expect("cmd_remove must call run_unified_projection");
    assert!(
        remove_call.contains("REMOVE_GATE_ABORT_CONTEXT"),
        "cmd_remove must pass REMOVE_GATE_ABORT_CONTEXT to run_unified_projection"
    );
}
