//! `ccync` CLI subcommand handlers, split from main.rs by cohesion.
//! `run` (main.rs) dispatches to these `pub(crate)` handlers.

pub(crate) mod args;
pub(crate) mod cleanup;
pub(crate) mod doctor;
pub(crate) mod init;
pub(crate) mod lifecycle;
pub(crate) mod pin;
pub(crate) mod plugin;
pub(crate) mod refresh;
pub(crate) mod repair;
pub(crate) mod rollback;
pub(crate) mod search;
pub(crate) mod show;
pub(crate) mod update;
pub(crate) mod upgrade;
