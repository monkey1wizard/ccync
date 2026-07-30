//! ccync setup support: management health checks (`health`) aggregated into
//! `ccync doctor`.
//!
//! The former `ccync setup` orchestrator, its machine-setup steps, and the
//! interactive runtime-selection seam were removed with the 9-command
//! surface — install *is* `ccync sync`, and master adoption is `ccync init`
//! (which writes runtime selection into `config.json` directly).

pub mod health;
