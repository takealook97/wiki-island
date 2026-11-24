//! The parsed manifest kept between snapshots of one vault handle.
//!
//! Disk stays the only truth. The cache keeps the raw bytes of the prefix
//! it parsed and the full file state (identity, length, modification and
//! change time) recorded right after its last parse, fold or own append. A
//! load reuses the cache without reading the file only when the file state
//! equals that record. Otherwise it reads the file and compares the cached
//! prefix byte for byte: equal, only the bytes after it are folded; any
//! difference, a shorter file or a read error, the whole file is parsed. A
//! state is cached only when every byte read folded cleanly, so a damaged
//! manifest is never cached. Where the platform gives no file state, every
//! load compares. Lock-free reads never fill or fold the cache: on a state
//! mismatch they compare the cached prefix the same way and fold only the
//! bytes after it into a copy, or parse the whole file. A frame this
//! process appends enters the cache only after its barrier, under the
//! publisher lock. Writers that replace the file also invalidate.

use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use crate::error::{HistoryError, Result};
use crate::manifest::{Fold, ManifestSnapshot, parse_fold};

/// Everything the platform tells about a file that changes when its bytes
/// do: identity, length, modification time and change time at full
/// resolution. User space cannot set the change time back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileState {
    device: u64,
    file_id: u128,
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

#[cfg(unix)]
fn platform_state(_file: &File, meta: &Metadata) -> Option<FileState> {
    use std::os::unix::fs::MetadataExt;
    Some(FileState {
        device: meta.dev(),
        file_id: u128::from(meta.ino()),
        len: meta.len(),
        modified: (meta.mtime(), meta.mtime_nsec()),
        changed: (meta.ctime(), meta.ctime_nsec()),
    })
}

#[cfg(windows)]
fn platform_state(file: &File, _meta: &Metadata) -> Option<FileState> {
    let stat = wi_sys::fs::stamp(file).ok()?;
    Some(FileState {
        device: stat.volume_serial,
        file_id: stat.file_id,
        len: stat.size,
        modified: (stat.last_write, 0),
        changed: (stat.change_time, 0),
    })
}

#[cfg(not(any(unix, windows)))]
fn platform_state(_file: &File, _meta: &Metadata) -> Option<FileState> {
    None
}

/// The state of the open file, or `None` when it is unknown and every load
/// must compare bytes.
fn file_state(file: &File) -> Option<FileState> {
    if crate::fault::file_state_unknown() {
        return None;
    }
    let meta = file.metadata().ok()?;
    platform_state(file, &meta)
}

struct Cached {
    fold: Fold,
    /// The file bytes the fold covers, all of them valid frames.
    prefix: Vec<u8>,
    /// File state right after the fold, when known and consistent.
    state: Option<FileState>,
}

impl Cached {
    fn valid_len(&self) -> u64 {
        self.prefix.len() as u64
    }

    /// Whether the file is proved unchanged since the fold.
    fn unchanged(&self, now: Option<FileState>) -> bool {
        self.state
            .is_some_and(|s| Some(s) == now && s.len == self.valid_len())
    }
}

/// Per-handle cache of the parsed manifest.
#[derive(Default)]
pub(crate) struct ManifestCache(Mutex<Option<Cached>>);

impl std::fmt::Debug for ManifestCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ManifestCache")
    }
}

/// Open `path`; `None` when it does not exist.
fn open(path: &Path) -> Result<Option<File>> {
    match File::open(path) {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(HistoryError::io(path, e)),
    }
}

fn read_all(file: &mut File, path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.read_to_end(&mut bytes))
        .map_err(|e| HistoryError::io(path, e))?;
    Ok(bytes)
}

/// The state to record for `bytes` just read: known only when the file did
/// not change while it was read.
fn settled(before: Option<FileState>, file: &File, bytes: &[u8]) -> Option<FileState> {
    let after = file_state(file)?;
    (before == Some(after) && after.len == bytes.len() as u64).then_some(after)
}

impl ManifestCache {
    fn lock(&self) -> MutexGuard<'_, Option<Cached>> {
        // A panic while the lock was held may have left a half-updated
        // state: any caller, a lock-free read included, drops it and the
        // next load parses again.
        self.0.lock().unwrap_or_else(|poisoned| {
            let mut state = poisoned.into_inner();
            *state = None;
            state
        })
    }

    /// Length of the cached valid prefix, if a state is cached.
    #[cfg(test)]
    pub(crate) fn cached_len(&self) -> Option<u64> {
        self.lock().as_ref().map(Cached::valid_len)
    }

    /// Forget the cached state; the next load parses the whole file.
    pub(crate) fn invalidate(&self) {
        *self.lock() = None;
    }

    /// The snapshot of the manifest at `path`, or `None` when it does not
    /// exist or is empty, updating the cache. Callers hold the publisher
    /// lock. Corruption is reported in the snapshot.
    pub(crate) fn load(&self, path: &Path) -> Result<Option<ManifestSnapshot>> {
        let Some(mut file) = open(path)? else {
            self.invalidate();
            return Ok(None);
        };
        let before = file_state(&file);
        let mut state = self.lock();
        if let Some(cached) = state.as_ref()
            && cached.unchanged(before)
        {
            return Ok(Some(cached.fold.snapshot(cached.valid_len(), None)));
        }
        // Taken out so new frames fold in place; put back only when clean.
        let cached = state.take();
        let bytes = read_all(&mut file, path)?;
        if bytes.is_empty() {
            return Ok(None);
        }
        let recorded = settled(before, &file, &bytes);
        let (fold, snap) = match cached {
            Some(cached) if Self::extends(&cached, &bytes) => {
                let mut fold = cached.fold;
                let start = cached.prefix.len();
                let (end, corrupt) = fold.fold_frames(&bytes[start..], start);
                let snap = fold.snapshot(end as u64, corrupt);
                (fold, snap)
            }
            _ => parse_fold(&bytes),
        };
        if snap.corrupt.is_none() {
            *state = Some(Cached {
                fold,
                prefix: bytes,
                state: recorded,
            });
        }
        Ok(Some(snap))
    }

    /// Whether `bytes` start with the cached prefix, compared byte for byte.
    fn extends(cached: &Cached, bytes: &[u8]) -> bool {
        crate::fault::count_prefix_compare();
        bytes.starts_with(&cached.prefix)
    }

    /// Lock-free read that leaves the cache as it was: the cached snapshot
    /// when the file is proved unchanged; otherwise the file is read and,
    /// when it starts with the cached prefix, only the bytes after it are
    /// folded into a copy of the cached fold, else the whole file is parsed.
    /// The mutex is never held while the file is read or the tail folded.
    /// One exception to leaving the cache as it was: if a panic poisoned
    /// the mutex, this read, like any other, drops the cached state.
    pub(crate) fn peek(&self, path: &Path) -> Result<Option<ManifestSnapshot>> {
        let Some(mut file) = open(path)? else {
            return Ok(None);
        };
        let now = file_state(&file);
        if let Some(cached) = self.lock().as_ref()
            && cached.unchanged(now)
        {
            return Ok(Some(cached.fold.snapshot(cached.valid_len(), None)));
        }
        let bytes = read_all(&mut file, path)?;
        if bytes.is_empty() {
            return Ok(None);
        }
        let Some((mut fold, start)) = self.copy_if_extended(&bytes) else {
            return Ok(Some(parse_fold(&bytes).1));
        };
        let (end, corrupt) = fold.fold_frames(&bytes[start..], start);
        Ok(Some(fold.snapshot(end as u64, corrupt)))
    }

    /// A copy of the cached fold and the length of the prefix it covers,
    /// when `bytes` start with that prefix.
    fn copy_if_extended(&self, bytes: &[u8]) -> Option<(Fold, usize)> {
        let state = self.lock();
        let cached = state.as_ref()?;
        Self::extends(cached, bytes).then(|| (cached.fold.clone(), cached.prefix.len()))
    }

    /// Fold `frame`, which this process appended at offset `at` and whose
    /// barrier returned, while the publisher lock is still held. The cache
    /// takes the frame only when it held exactly the bytes before it and the
    /// file was proved unchanged up to the write; otherwise it keeps its
    /// state and the next load compares. It records the file state only
    /// when [`AppendStates::own`] proves nothing else wrote during the
    /// barrier.
    pub(crate) fn appended(&self, states: AppendStates, at: u64, frame: &[u8]) {
        let mut state = self.lock();
        let Some(cached) = state.as_mut() else {
            return;
        };
        if cached.valid_len() != at || !cached.unchanged(states.before) {
            return;
        }
        let start = cached.prefix.len();
        let (end, corrupt) = cached.fold.fold_frames(frame, start);
        if corrupt.is_some() || end != start + frame.len() {
            *state = None;
            return;
        }
        cached.prefix.extend_from_slice(frame);
        cached.state = states.own(cached.valid_len());
    }
}

/// File states an own append samples from its handle: just before the
/// write, right after it returned, and after the barrier.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AppendStates {
    pub(crate) before: Option<FileState>,
    pub(crate) written: Option<FileState>,
    pub(crate) after: Option<FileState>,
}

impl AppendStates {
    /// The state to record after the append, known only when the two
    /// samples after the write are equal, the file is the one written to
    /// and it is `len` bytes long. The barrier keeps the change time, so
    /// any other write during it is seen here.
    fn own(&self, len: u64) -> Option<FileState> {
        let (before, written) = (self.before?, self.written?);
        let same_file = written.device == before.device && written.file_id == before.file_id;
        (self.after == Some(written) && same_file && written.len == len).then_some(written)
    }
}

/// File state of an open manifest, for [`AppendStates`].
pub(crate) fn state_of(file: &File) -> Option<FileState> {
    file_state(file)
}
