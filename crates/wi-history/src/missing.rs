//! Detection of notes deleted outside the engine.

use serde::Serialize;

use crate::error::{HistoryError, Result};
use crate::gitops;
use crate::manifest::Manifest;
use crate::vault::{BlobId, Vault};

/// A path present at the branch tip but absent from the working tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MissingEntry {
    pub path: String,
    /// Last committed bytes, recoverable with `Vault::read_blob`.
    pub blob: BlobId,
    /// Commit the scan compared against.
    pub commit: String,
}

/// Missing-file scan.
pub struct Missing;

impl Missing {
    /// Paths tracked at the tip whose file is gone. Paths that a pending
    /// manifest record moves away (`moved_from`) or rewrites are skipped,
    /// because their commit is still queued. The caller records each entry
    /// as `missing` with a work item.
    pub fn scan(vault: &Vault) -> Result<Vec<MissingEntry>> {
        let Some(tip) = gitops::branch_tip(vault)?.1 else {
            return Ok(Vec::new());
        };
        let pending = Manifest::snapshot_unlocked(vault)?;
        let queued = |path: &str| {
            pending
                .entries
                .iter()
                .any(|e| e.op.moved_from() == Some(path))
        };
        let mut out = Vec::new();
        for (path, blob) in gitops::tree_blobs(vault, tip)? {
            let on_disk = vault.root().join(&path);
            match std::fs::symlink_metadata(&on_disk) {
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                // A name this file system cannot hold (synced from another
                // device) is not a deletion.
                Err(e) if e.kind() == std::io::ErrorKind::InvalidFilename => continue,
                Err(e) => return Err(HistoryError::io(on_disk, e)),
            }
            if queued(&path) {
                continue;
            }
            out.push(MissingEntry {
                path,
                blob: BlobId(blob.to_string()),
                commit: tip.to_string(),
            });
        }
        Ok(out)
    }
}
