//! The parsed manifest kept between snapshots of one vault handle.
//!
//! Disk stays the only truth. Each load opens the file and checks the cached
//! state against it: same file (device and inode on Unix), not shorter than
//! the cached valid prefix, the same modification time when the length is
//! unchanged, and the same bytes in the checkpoint frame and the last cached
//! frame. When the file only grew, the new frames are folded into a copy of
//! the cached state; otherwise the whole file is parsed. A state is cached
//! only when every byte read folded cleanly, so a damaged manifest is never
//! cached, and a frame enters the cache only once it is read back from the
//! file. Writers that replace the file also invalidate. Damage written into
//! the middle of a file that keeps its identity and grows is not seen here;
//! retirement parses the whole file before it copies frames.

use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::SystemTime;

use crate::error::{HistoryError, Result};
use crate::manifest::{Fold, MAGIC, ManifestSnapshot, parse_fold};

/// File identity: device and inode on Unix; none elsewhere, where the byte
/// checks alone decide.
type Identity = Option<(u64, u64)>;

#[cfg(unix)]
fn identity(meta: &Metadata) -> Identity {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn identity(_meta: &Metadata) -> Identity {
    None
}

struct Cached {
    fold: Fold,
    valid_len: u64,
    identity: Identity,
    /// Modification time when `valid_len` was read, if known.
    modified: Option<SystemTime>,
    /// Magic line and checkpoint frame: they name the manifest id, the
    /// retired prefix and the sequence state, and every rewrite changes them.
    head: Vec<u8>,
    /// Offset and bytes of the last frame of the valid prefix.
    last: (u64, Vec<u8>),
}

/// Per-handle cache of the parsed manifest.
#[derive(Default)]
pub(crate) struct ManifestCache(Mutex<Option<Cached>>);

impl std::fmt::Debug for ManifestCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ManifestCache")
    }
}

fn read_at(file: &mut File, path: &Path, offset: u64, len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.read_exact(&mut buf))
        .map_err(|e| HistoryError::io(path, e))?;
    Ok(buf)
}

impl Cached {
    /// Cache the state of a clean fold whose valid prefix ends at
    /// `valid_len`. `tail` holds the file bytes from `tail_start` on and
    /// must cover the checkpoint frame when `head` is `None`.
    fn new(
        fold: Fold,
        valid_len: u64,
        (identity, modified): (Identity, Option<SystemTime>),
        head: Option<Vec<u8>>,
        tail: &[u8],
        tail_start: usize,
    ) -> Option<Self> {
        let head = head.or_else(|| {
            tail.get(..fold.head_end.checked_sub(tail_start)?)
                .map(<[u8]>::to_vec)
        })?;
        let last_range = fold
            .frames()
            .last()
            .map_or(MAGIC.len()..fold.head_end, |(_, r)| r.clone());
        let local =
            last_range.start.checked_sub(tail_start)?..last_range.end.checked_sub(tail_start)?;
        let last = (last_range.start as u64, tail.get(local)?.to_vec());
        Some(Self {
            fold,
            valid_len,
            identity,
            modified,
            head,
            last,
        })
    }

    /// Whether the open file still starts with the cached valid prefix.
    fn matches(&self, file: &mut File, path: &Path, meta: &Metadata) -> Result<bool> {
        let len = meta.len();
        if identity(meta) != self.identity || len < self.valid_len {
            return Ok(false);
        }
        // Same length but written since: rewritten in place.
        if len == self.valid_len && meta.modified().ok() != self.modified {
            return Ok(false);
        }
        if read_at(file, path, 0, self.head.len())? != self.head {
            return Ok(false);
        }
        Ok(read_at(file, path, self.last.0, self.last.1.len())? == self.last.1)
    }
}

impl ManifestCache {
    fn lock(&self) -> MutexGuard<'_, Option<Cached>> {
        // A panic while the lock was held may have left a half-updated
        // state: drop it and parse again.
        self.0.lock().unwrap_or_else(|poisoned| {
            let mut state = poisoned.into_inner();
            *state = None;
            state
        })
    }

    /// Length of the cached valid prefix, if a state is cached.
    #[cfg(test)]
    pub(crate) fn cached_len(&self) -> Option<u64> {
        self.lock().as_ref().map(|c| c.valid_len)
    }

    /// Forget the cached state; the next load parses the whole file.
    pub(crate) fn invalidate(&self) {
        *self.lock() = None;
    }

    /// The snapshot of the manifest at `path`, or `None` when it does not
    /// exist or is empty. Corruption is reported in the snapshot.
    pub(crate) fn load(&self, path: &Path) -> Result<Option<ManifestSnapshot>> {
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.invalidate();
                return Ok(None);
            }
            Err(e) => return Err(HistoryError::io(path, e)),
        };
        let meta = file.metadata().map_err(|e| HistoryError::io(path, e))?;
        let len = meta.len();
        let mut state = self.lock();
        if len == 0 {
            *state = None;
            return Ok(None);
        }
        let stamp = (identity(&meta), meta.modified().ok());
        // Taken out so new frames fold in place; put back only when clean.
        if let Some(cached) = state.take()
            && cached.matches(&mut file, path, &meta)?
        {
            if len == cached.valid_len {
                let snap = cached.fold.snapshot(len, None);
                *state = Some(cached);
                return Ok(Some(snap));
            }
            let start = usize::try_from(cached.valid_len).unwrap_or(usize::MAX);
            let tail_len = usize::try_from(len - cached.valid_len).unwrap_or(usize::MAX);
            let tail = read_at(&mut file, path, cached.valid_len, tail_len)?;
            let mut fold = cached.fold;
            let (end, corrupt) = fold.fold_frames(&tail, start);
            let snap = fold.snapshot(end as u64, corrupt);
            if snap.corrupt.is_none() {
                *state = Cached::new(fold, len, stamp, Some(cached.head), &tail, start);
            }
            return Ok(Some(snap));
        }
        let mut bytes = Vec::new();
        file.seek(SeekFrom::Start(0))
            .and_then(|_| file.read_to_end(&mut bytes))
            .map_err(|e| HistoryError::io(path, e))?;
        if bytes.is_empty() {
            *state = None;
            return Ok(None);
        }
        let (fold, snap) = parse_fold(&bytes);
        // Bytes appended after the metadata was read leave the time unknown.
        let stamp = if bytes.len() as u64 == len {
            stamp
        } else {
            (stamp.0, None)
        };
        *state = match snap.corrupt {
            None => Cached::new(fold, bytes.len() as u64, stamp, None, &bytes, 0),
            Some(_) => None,
        };
        Ok(Some(snap))
    }
}
