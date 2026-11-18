//! Crash-safe single-file writer shared by the MCP host config writers and the
//! projection ownership / write-ahead-transaction state files.
//!
//! Two separate guarantees, worth naming separately because the write-ahead
//! transaction depends on the second one and this module originally provided
//! only the first:
//!
//! - **Atomicity** — content goes to a sibling temp file and is renamed over the
//!   destination, so no reader observes a half-written config, and a crash mid-
//!   write cannot truncate the destination.
//! - **Durability** — the temp file is flushed to the storage device *before*
//!   the rename, so the rename cannot publish a directory entry pointing at
//!   bytes that exist only in the page cache.
//!
//! Durability is what makes the journal genuinely write-*ahead*.
//! `ProjectionTransaction::save` must reach the device before the host writes it
//! describes; otherwise a power loss can commit the `~/.claude.json` write while
//! `projection.txn.json` is still dirty, leaving a mutated live config with an
//! absent or zero-length journal and nothing to recover from. Atomicity alone
//! does not give this — it only orders what a reader can see, not what survives.
//!
//! The write itself is delegated to `ccync_foundation::platform::atomic_write_bytes`,
//! which implements the same sibling-temp-file / durable-sync / rename algorithm;
//! this module only adapts that shared writer's error type to [`McpUpdateError`].

use crate::McpUpdateError;
use ccync_foundation::platform::{atomic_write_bytes, AtomicWriteError};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

/// Sibling temp path `<dest>.tmp` in the same directory (so the rename stays on
/// one filesystem). Appends `.tmp` to the full file name rather than replacing
/// the extension, so `config.toml` → `config.toml.tmp`.
///
/// Test-only: the write path now delegates to
/// `ccync_foundation::platform::atomic_write_bytes`, which has its own copy of
/// this helper. This one survives solely so `io.rs`'s and `merge.rs`'s tests
/// can compute the same `<dest>.tmp` path the shared writer uses internally,
/// without exposing it as a real dependency of production code.
#[cfg(test)]
pub(crate) fn tmp_sibling(dest: &Path) -> PathBuf {
    let mut name = dest
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    dest.with_file_name(name)
}

/// Write `contents` to `dest` atomically and durably: write a sibling
/// `<dest>.tmp`, flush it to the storage device, then `rename` it over `dest`.
/// The rename is atomic on a single filesystem (on Windows it overwrites the
/// target). A failure during the temp write leaves `dest` untouched; a failure
/// during the temp write or the rename removes the temp file.
///
/// Delegates to `ccync_foundation::platform::atomic_write_bytes`, whose
/// `sync_all` before the rename is load-bearing, not belt-and-braces: without
/// it the rename can be ordered ahead of the data it publishes, so a power
/// loss can leave `dest` present but empty or stale. That is fatal for the
/// write-ahead transaction journal, whose whole contract is to be on disk
/// before the host writes it describes (see the module docs).
pub(crate) fn write_atomic(dest: &Path, contents: &str) -> std::result::Result<(), McpUpdateError> {
    atomic_write_bytes(dest, contents.as_bytes()).map_err(|e| {
        McpUpdateError::Io(match e {
            AtomicWriteError::TempWrite(e) => e,
            AtomicWriteError::PreRename(e) => e,
            AtomicWriteError::Rename(e) => e,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    #[test]
    fn tmp_sibling_appends_suffix_keeps_extension() {
        let p = Path::new("/some/dir/config.toml");
        assert_eq!(tmp_sibling(p), PathBuf::from("/some/dir/config.toml.tmp"));
    }

    #[test]
    fn atomic_write_round_trips_and_overwrites() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("config.toml");

        write_atomic(&dest, "hello = 1\n").unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "hello = 1\n");

        // Overwrite an existing target.
        write_atomic(&dest, "hello = 2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "hello = 2\n");

        // No leftover temp sibling.
        assert!(!tmp_sibling(&dest).exists(), "temp sibling must not linger");
    }

    #[test]
    fn atomic_write_leaves_target_untouched_when_temp_write_fails() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("config.toml");
        std::fs::write(&dest, "original\n").unwrap();

        // Force the temp write to fail: pre-create <dest>.tmp as a directory, so
        // opening that path for writing errors before any rename can run.
        let tmp_path = tmp_sibling(&dest);
        std::fs::create_dir(&tmp_path).unwrap();

        let result = write_atomic(&dest, "new content\n");
        assert!(
            result.is_err(),
            "temp write onto a directory path must fail"
        );

        // The live target must be exactly as it was — never truncated.
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "original\n");
    }

    #[test]
    fn atomic_write_replaces_a_stale_temp_sibling() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("projection.txn.json");

        // A leftover temp file from an interrupted run, longer than the content
        // about to be written: the writer must truncate it, never leave a tail
        // of the previous contents behind for the rename to publish.
        std::fs::write(tmp_sibling(&dest), "{\"stale\": \"leftover from a crash\"}").unwrap();

        write_atomic(&dest, "{}").unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "{}");
        assert!(!tmp_sibling(&dest).exists(), "temp sibling must not linger");
    }
}
