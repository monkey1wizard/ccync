//! base: CCYNC foundation crate.
//!
//! Owns configuration truth, install mode resolution, path location
//! resolution, shared JSON/env utilities, runtime selection helpers, and
//! shared serde data-model types (MCP manifest types).
//! This crate depends on no other CCYNC crate — it is the dependency-law
//! foundation: every other CCYNC crate may depend on `base`, never the reverse.

pub mod config;
pub mod env_config;
pub mod health;
pub mod json_util;
pub mod mcp;
pub mod paths;
pub mod pins;
pub mod platform;
pub mod render;
pub mod secret;
pub mod target;

/// Shared test-only helpers used across this crate's `#[cfg(test)]` modules.
///
/// Tests that mutate process-wide env vars (`APPDATA`/`XDG_CONFIG_HOME`/
/// `USERPROFILE`/`HOME`) must serialize against a single shared lock — not
/// one private lock per module — or two tests in different modules can race
/// on the same env var concurrently under `cargo test`'s default
/// multithreading.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Poison-tolerant: a panicking test must not cascade-fail every other
    /// env test by poisoning the lock.
    pub(crate) fn env_guard() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
