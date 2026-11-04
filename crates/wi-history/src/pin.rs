//! Resolution of source pins `{ref, commit, pending, source_hash}` to bytes.

use serde::Serialize;

use crate::error::{HistoryError, Result};
use crate::history::History;
use crate::manifest::{Manifest, OpStatus, Seq};
use crate::vault::{Vault, hash_bytes};

/// The fields of a pin needed to find its bytes (`docs/FORMAT.md#pins`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinRef {
    pub path: String,
    pub source_hash: String,
    pub commit: Option<String>,
    pub pending: bool,
}

/// Where the pinned bytes were found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "via", rename_all = "snake_case")]
pub enum PinSource {
    /// A commit holds the bytes; a pending pin can be updated to this commit.
    Commit { commit: String },
    /// An unretired manifest record holds the bytes (not yet committed).
    Manifest { seq: Seq },
}

/// Resolved pin bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinResolution {
    pub bytes: Vec<u8>,
    pub source: PinSource,
    /// Engine-recorded time of the pinned bytes, unix milliseconds: the
    /// manifest record time, or the author time of the commit holding them
    /// (the flush time, so never earlier than the record).
    pub recorded_ms: i64,
}

fn from_manifest(vault: &Vault, pin: &PinRef) -> Result<Option<PinResolution>> {
    let snap = Manifest::snapshot_unlocked(vault)?;
    let hit = snap.entries.iter().rev().find(|e| {
        e.status == OpStatus::Done && e.op.path() == pin.path && e.op.new_hash() == pin.source_hash
    });
    let Some(entry) = hit else { return Ok(None) };
    let bytes = vault.read_blob(entry.op.new_blob())?;
    Ok(
        (hash_bytes(&bytes) == pin.source_hash).then_some(PinResolution {
            bytes,
            source: PinSource::Manifest { seq: entry.seq },
            recorded_ms: i64::try_from(entry.time_ms).unwrap_or(i64::MAX),
        }),
    )
}

fn from_history(vault: &Vault, pin: &PinRef) -> Result<Option<PinResolution>> {
    let mut seen = std::collections::BTreeSet::new();
    // Oldest commit first: the first commit that introduced these bytes.
    for entry in History::log(vault, &pin.path)?.into_iter().rev() {
        let Some(blob) = entry.blob else { continue };
        if !seen.insert(blob.clone()) {
            continue;
        }
        let bytes = vault.read_blob(&blob)?;
        if hash_bytes(&bytes) == pin.source_hash {
            return Ok(Some(PinResolution {
                bytes,
                source: PinSource::Commit {
                    commit: entry.commit,
                },
                recorded_ms: entry.author_time.saturating_mul(1000),
            }));
        }
    }
    Ok(None)
}

/// Resolve a pin. A named commit must hold bytes with `source_hash`. A
/// pending pin (`commit: null`) resolves through the manifest, then through
/// the first commit holding the same bytes once the flush has run.
pub fn resolve_pin(vault: &Vault, pin: &PinRef) -> Result<PinResolution> {
    if let Some(commit) = &pin.commit {
        let bytes = History::read_at(vault, commit, &pin.path)?;
        if hash_bytes(&bytes) != pin.source_hash {
            return Err(HistoryError::PinMismatch {
                path: pin.path.clone(),
                commit: commit.clone(),
                source_hash: pin.source_hash.clone(),
            });
        }
        return Ok(PinResolution {
            bytes,
            source: PinSource::Commit {
                commit: commit.clone(),
            },
            recorded_ms: History::commit_time(vault, commit)?.saturating_mul(1000),
        });
    }
    let found = if pin.pending {
        match from_manifest(vault, pin)? {
            Some(hit) => Some(hit),
            None => from_history(vault, pin)?,
        }
    } else {
        match from_history(vault, pin)? {
            Some(hit) => Some(hit),
            None => from_manifest(vault, pin)?,
        }
    };
    found.ok_or_else(|| HistoryError::PinUnresolved {
        path: pin.path.clone(),
        source_hash: pin.source_hash.clone(),
    })
}
