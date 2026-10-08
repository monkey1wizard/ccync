//! `ccync` CLI subcommand handlers, split from main.rs by cohesion.
//! `run` (main.rs) dispatches to these `pub(crate)` handlers.

pub(crate) mod args;
pub(crate) mod cleanup;
pub(crate) mod doctor;
pub(crate) mod outdated;
pub(crate) mod init;
pub(crate) mod lifecycle;
pub(crate) mod pin;
pub(crate) mod plugin;
pub(crate) mod refresh;
pub(crate) mod repair;
pub(crate) mod rollback;
pub(crate) mod roster;
pub(crate) mod search;
pub(crate) mod show;
pub(crate) mod tool;
pub(crate) mod tool_refresh;
pub(crate) mod tool_output;
pub(crate) mod tool_track;
pub(crate) mod update;
pub(crate) mod upgrade;

#[cfg(test)]
pub(crate) mod lifecycle_block_tests;

#[cfg(test)]
pub(crate) mod lifecycle_secret_skip_tests;

#[cfg(test)]
pub(crate) mod lifecycle_dry_run_tests;

#[cfg(test)]
pub(crate) mod init_disclosure_tests;

#[cfg(test)]
pub(crate) mod lifecycle_payload_tests;

#[cfg(test)]
pub(crate) mod lifecycle_strip_tests;

#[cfg(test)]
pub(crate) mod lifecycle_strip_refusal_tests;

#[cfg(test)]
pub(crate) mod lifecycle_restore_flag_tests;

#[cfg(test)]
pub(crate) mod lifecycle_drift_tests;

#[cfg(test)]
pub(crate) mod lifecycle_restore_secret_tests;

#[cfg(test)]
pub(crate) mod lifecycle_machine_switch_tests;

#[cfg(test)]
pub(crate) mod lifecycle_backup_tool_tests;

#[cfg(test)]
pub(crate) mod doctor_block_tests;
