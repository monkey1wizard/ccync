//! `ccync` CLI subcommand handlers, split from main.rs by cohesion.
//! `run` (main.rs) dispatches to these `pub(crate)` handlers.

pub(crate) mod doctor;
pub(crate) mod init;
pub(crate) mod lifecycle;
pub(crate) mod plugin;
