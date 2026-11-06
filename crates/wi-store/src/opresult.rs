//! First results of operations with an id (OPERATIONS `#idempotency`).
//!
//! A write with an operation id stores the stable fields of its first
//! result in `.git/wi/ops/<h[..2]>/<h[2..]>.json` (`h` = SHA-256 of the id)
//! after the swap and before `done`, under the publisher lock. The file is
//! per device, like the manifest, and survives manifest retirement and index
//! loss. A retry reads it in O(1). Generation, index state and `replayed`
//! are reported as they are at replay time.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use wi_history::Vault as HistoryVault;
use wi_history::{durable, hash_bytes};

use crate::error::{Result, StoreError};

const DIR: &str = "ops";

/// The stable fields of an operation's first result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FirstResult {
    pub op_id: String,
    pub request_digest: String,
    pub manifest_id: String,
    pub seq: u64,
    pub path: String,
    pub checksum: String,
    pub previous_checksum: Option<String>,
    pub conflict_copy: Option<String>,
    pub moved_from: Option<String>,
}

fn file(vault: &HistoryVault, op_id: &str) -> (PathBuf, PathBuf) {
    let hash = hash_bytes(op_id.as_bytes());
    let hex = hash.strip_prefix("sha256:").unwrap_or(&hash);
    let shard = vault.wi_dir().join(DIR).join(&hex[..2]);
    let path = shard.join(format!("{}.json", &hex[2..]));
    (shard, path)
}

/// Store a first result durably (file and directory barriers).
pub(crate) fn store(vault: &HistoryVault, first: &FirstResult) -> Result<()> {
    let (shard, path) = file(vault, &first.op_id);
    let mode = vault.durability();
    durable::create_dir_durable(&vault.wi_dir().join(DIR), mode)?;
    durable::create_dir_durable(&shard, mode)?;
    let bytes = serde_json::to_vec(first)
        .map_err(|e| StoreError::Invalid(format!("operation result: {e}")))?;
    durable::write_atomic(&path, &bytes, mode)?;
    Ok(())
}

/// The stored first result of `op_id`, if any. An unreadable file counts
/// as absent: the commit trailers still identify the operation.
pub(crate) fn load(vault: &HistoryVault, op_id: &str) -> Result<Option<FirstResult>> {
    let (_, path) = file(vault, op_id);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(StoreError::io(path, e)),
    };
    Ok(serde_json::from_slice::<FirstResult>(&bytes)
        .ok()
        .filter(|first| first.op_id == op_id))
}

/// Forget the first result of an operation that recovery dropped.
pub(crate) fn remove(vault: &HistoryVault, op_id: &str) -> Result<()> {
    let (shard, path) = file(vault, op_id);
    match std::fs::remove_file(&path) {
        Ok(()) => {
            durable::barrier_dir(&shard, vault.durability())?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(StoreError::io(path, e)),
    }
}
