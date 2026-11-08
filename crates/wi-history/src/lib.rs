//! Git history and the durable manifest log for a wiki-island vault.
//!
//! The working-tree file is the current state of a note; git is coalesced
//! history. Every acknowledged engine save is first recorded in an
//! append-only, framed and checksummed manifest inside `.git/wi/`, with its
//! bytes already stored as a durable git blob. A flush turns manifest records
//! into one commit each, in sequence order, and then retires the committed
//! prefix. See `docs/OPERATIONS.md#publisher-and-manifest`.
#![forbid(unsafe_code)]

mod changes;
mod cloud;
pub mod durable;
mod error;
mod fault;
mod flush;
mod gitops;
mod history;
mod lock;
mod manifest;
mod missing;
mod pin;
mod recovery;
mod storage;
mod sync;
mod vault;

#[cfg(test)]
mod barrier_tests;

pub use changes::{PublicationMark, changed_paths_since, publication_mark};
pub use cloud::{cloud_service, cloud_sync_warning, resolve_path};
pub use durable::Durability;
pub use error::{HistoryError, Result};
pub use flush::{CommitSummary, FlushBlock, FlushReport, Flusher};
pub use history::{Deletion, History, HistoryEntry, OperationHit, OperationScan, OperationTrailer};
pub use lock::{FlushLock, PublisherGuard, PublisherLock};
pub use manifest::{
    BatchMark, IngestRecord, IntentRecord, Manifest, ManifestEntry, ManifestSnapshot, OpRecord,
    OpStatus, Seq,
};
pub use missing::{Missing, MissingEntry};
pub use pin::{PinRef, PinResolution, PinSource, resolve_pin};
pub use recovery::{
    ConflictEntry, FsWorkingTree, QuarantineEntry, Recovery, RecoveryReport, UnsettledIntent,
    WorkingTree,
};
pub use storage::{
    DATA_DIR_ENV, StorageMode, VAULT_LOCAL_DIR, data_root, device_id, external_git_dir,
    local_state_dir, read_vault_id, storage_mode, vault_state_dir,
};
pub use sync::{SyncRepo, SyncTree};
pub use vault::{Blob, BlobId, Vault, blob_id_of, generation, hash_bytes, publication_state};

/// Trailer naming the internal manifest sequence id: `Wi-Seq: <seq> <manifest-id>`.
pub const TRAILER_SEQ: &str = "Wi-Seq";
/// Trailer naming the caller's operation id: `Wi-Operation: <op-id> <request-digest>`.
pub const TRAILER_OPERATION: &str = "Wi-Operation";
/// Trailer naming the ingress actor: `Wi-Actor: <actor>`.
pub const TRAILER_ACTOR: &str = "Wi-Actor";
/// Trailer naming the commit a revert restores from: `Wi-Revert: <commit>`.
pub const TRAILER_REVERT: &str = "Wi-Revert";
/// Trailer naming the previous path of a move: `Wi-Moved-From: <path>`.
pub const TRAILER_MOVED_FROM: &str = "Wi-Moved-From";
/// Trailer naming a path deleted outside the engine: `Wi-Deleted: <path>`.
pub const TRAILER_DELETED: &str = "Wi-Deleted";
/// Trailer naming a rename made outside the engine: `Wi-Renamed: <from> -> <to>`.
pub const TRAILER_RENAMED: &str = "Wi-Renamed";
/// Trailer naming a bulk-import batch: `Wi-Batch: <batch-id>`.
pub const TRAILER_BATCH: &str = "Wi-Batch";
/// Trailer recording one change-cap loss event: `Wi-Loss: <json>`.
pub const TRAILER_LOSS: &str = "Wi-Loss";
