//! OS advisory locks inside `.git/wi`. Locks are released when the guard is
//! dropped or the process dies. They are per open file description, so a
//! process must not acquire the same lock twice.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use crate::error::{HistoryError, Result};
use crate::vault::Vault;

const PUBLISH_LOCK: &str = "publish.lock";
const FLUSH_LOCK: &str = "flush.lock";

fn lock_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| HistoryError::io(path, e))?;
    file.lock().map_err(|e| HistoryError::io(path, e))?;
    Ok(file)
}

/// The single-publisher lock (`.git/wi/publish.lock`). Hold it only from the
/// budget reservation through the `done` record; never across model or
/// network calls.
pub struct PublisherLock;

/// Proof that the publisher lock is held. Manifest appends require it.
#[derive(Debug)]
pub struct PublisherGuard {
    _file: File,
    git_dir: PathBuf,
}

impl PublisherLock {
    /// Block until the publisher lock is held.
    pub fn acquire(vault: &Vault) -> Result<PublisherGuard> {
        let dir = vault.ensure_wi_dir()?;
        let file = lock_file(&dir.join(PUBLISH_LOCK))?;
        Ok(PublisherGuard {
            _file: file,
            git_dir: vault.git_dir().to_path_buf(),
        })
    }
}

impl PublisherGuard {
    pub(crate) fn check(&self, vault: &Vault) -> Result<()> {
        if self.git_dir == vault.git_dir() {
            Ok(())
        } else {
            Err(HistoryError::ForeignGuard)
        }
    }
}

/// Serializes flushes, replay and reverts (`.git/wi/flush.lock`). It is
/// separate from the publisher lock so saves can continue while a flush
/// writes commits.
#[derive(Debug)]
pub struct FlushLock {
    _file: File,
}

impl FlushLock {
    /// Block until the flush lock is held.
    pub fn acquire(vault: &Vault) -> Result<Self> {
        let dir = vault.ensure_wi_dir()?;
        Ok(Self {
            _file: lock_file(&dir.join(FLUSH_LOCK))?,
        })
    }
}
