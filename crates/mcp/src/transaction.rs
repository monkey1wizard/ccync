//! Write-ahead transaction journal for the MCP projection.
//!
//! Before the first host write, the projection persists `projection.txn.json`:
//! for every affected host/name pair, the fingerprint *token* of the live entry
//! **before** the write (`pre`) and the intended fingerprint **after** it
//! (`post`). Each token is a SHA-256 hex digest or the `absent` sentinel — never
//! the entry's resolved content (no commands, env, headers, or tokens ever reach
//! the journal). If a crash interrupts the sequential host writes, the next run
//! reads this journal and, by re-fingerprinting each live entry, classifies each
//! pair as landed (`== post`), not-landed (`== pre`), or ambiguous.
//!
//! Lives beside `managed.json`; written via the crate's atomic writer.

use crate::entry::EntryFingerprint;
use crate::host_render::Host;
use crate::io::write_atomic;
use crate::McpUpdateError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Current on-disk schema version.
const TRANSACTION_VERSION: u32 = 1;
/// File name, resolved beside `managed.json`.
const TRANSACTION_FILE: &str = "projection.txn.json";

/// One affected host/name pair's before/after fingerprint tokens. `pre`/`post`
/// are each a fingerprint hex digest or `"absent"` — never entry content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TxnEntry {
    pub host: String,
    pub name: String,
    pub pre: String,
    pub post: String,
}

/// A pending write-ahead transaction: an ID plus the complete set of affected
/// host/name pre/post pairs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProjectionTransaction {
    pub version: u32,
    pub transaction_id: String,
    pub entries: Vec<TxnEntry>,
}

impl ProjectionTransaction {
    /// Start a new, empty transaction with the given caller-supplied ID.
    pub(crate) fn new(transaction_id: impl Into<String>) -> Self {
        Self {
            version: TRANSACTION_VERSION,
            transaction_id: transaction_id.into(),
            entries: Vec::new(),
        }
    }

    /// Record one affected host/name pair by its pre/post fingerprints. Only the
    /// durable tokens are stored, never the entry content behind them.
    pub(crate) fn record(
        &mut self,
        host: Host,
        name: &str,
        pre: &EntryFingerprint,
        post: &EntryFingerprint,
    ) {
        self.entries.push(TxnEntry {
            host: host.as_str().to_string(),
            name: name.to_string(),
            pre: pre.as_token(),
            post: post.as_token(),
        });
    }

    /// Journal path beside a given `managed.json`.
    pub(crate) fn path_beside(managed_json: &Path) -> PathBuf {
        managed_json.with_file_name(TRANSACTION_FILE)
    }

    /// Load a pending transaction. A missing or empty file means "no pending
    /// transaction" (`Ok(None)`), never an error.
    pub(crate) fn load(path: &Path) -> Result<Option<Self>, McpUpdateError> {
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(path)?;
        if content.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(&content)?))
    }

    /// Persist atomically (pretty JSON; entry order preserved, which is the write
    /// order).
    pub(crate) fn save(&self, path: &Path) -> Result<(), McpUpdateError> {
        crate::ensure_parent(path)?;
        write_atomic(path, &serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Clear the journal (remove the file). Missing file is a no-op.
    pub(crate) fn remove(path: &Path) -> Result<(), McpUpdateError> {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::fingerprint_json;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn persisted_projection_transaction_round_trips_all_pairs() {
        let tmp = TempDir::new().unwrap();
        let path = ProjectionTransaction::path_beside(&tmp.path().join("managed.json"));

        let mut txn = ProjectionTransaction::new("txn-1");
        txn.record(
            Host::Claude,
            "srv-a",
            &EntryFingerprint::Absent,
            &fingerprint_json(&json!({"command":"bin"})),
        );
        txn.record(
            Host::Codex,
            "srv-b",
            &fingerprint_json(&json!({"command":"old"})),
            &EntryFingerprint::Absent,
        );
        txn.save(&path).unwrap();

        let first_bytes = std::fs::read_to_string(&path).unwrap();
        let loaded = ProjectionTransaction::load(&path).unwrap().unwrap();
        assert_eq!(loaded, txn);
        assert_eq!(loaded.entries.len(), 2, "every affected pair round-trips");

        loaded.save(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), first_bytes);
    }

    #[test]
    fn persisted_projection_transaction_missing_is_none() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("absent.json");
        assert!(ProjectionTransaction::load(&path).unwrap().is_none());
    }

    #[test]
    fn persisted_projection_transaction_journal_has_no_secret_bytes() {
        let tmp = TempDir::new().unwrap();
        let path = ProjectionTransaction::path_beside(&tmp.path().join("managed.json"));

        let secret = "super-secret-token-value-DO-NOT-PERSIST";
        let fp = fingerprint_json(&json!({"command":"bin","env":{"API_KEY": secret}}));
        let mut txn = ProjectionTransaction::new("txn-2");
        txn.record(Host::Claude, "srv", &EntryFingerprint::Absent, &fp);
        txn.save(&path).unwrap();

        let bytes = std::fs::read_to_string(&path).unwrap();
        assert!(
            !bytes.contains(secret),
            "journal must never carry entry secret values"
        );
        assert!(
            bytes.contains(&fp.as_token()),
            "journal carries only the fingerprint token"
        );
        assert!(
            bytes.contains("absent"),
            "absent sentinel is stored verbatim"
        );
    }

    #[test]
    fn persisted_projection_transaction_remove_clears() {
        let tmp = TempDir::new().unwrap();
        let path = ProjectionTransaction::path_beside(&tmp.path().join("managed.json"));
        ProjectionTransaction::new("txn-3").save(&path).unwrap();
        ProjectionTransaction::remove(&path).unwrap();
        assert!(ProjectionTransaction::load(&path).unwrap().is_none());
        // Removing an already-absent journal is a no-op.
        ProjectionTransaction::remove(&path).unwrap();
    }

    #[test]
    fn persisted_projection_transaction_path_is_sibling_of_managed() {
        let p = ProjectionTransaction::path_beside(Path::new("/x/build/mcp/managed.json"));
        assert_eq!(p, PathBuf::from("/x/build/mcp/projection.txn.json"));
    }
}
