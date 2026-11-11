//! Index lifecycle and pending repairs.

use std::path::{Path, PathBuf};

use wi_history::durable;
use wi_index::Index;

use crate::error::{Result, StoreError};

const INDEX_PENDING: &str = "index-pending";
const INDEX_PENDING_LOCK: &str = "index-pending.lock";
/// Prefix of a pending-marker claim: `index-pending.claim-<pid>-<n>-<ns>`.
const INDEX_PENDING_CLAIM: &str = "index-pending.claim-";
/// Files one chunk of a time-sliced rebuild stores before the budget is
/// checked again (an unmeasured policy value).
const REBUILD_CHUNK: usize = 128;

use super::{CACHE_DIR_ENV, IndexState, Vault};

impl Vault {
    pub(crate) fn index_path(&self) -> Option<PathBuf> {
        if let Some(path) = &self.options.index_path {
            return Some(path.clone());
        }
        std::env::var_os(CACHE_DIR_ENV).map(|dir| {
            PathBuf::from(dir)
                .join(&self.vault_id)
                .join("index.sqlite3")
        })
    }

    /// Run `f` with the device index, opening it on first use. A
    /// connection whose database file was replaced is reopened first.
    pub(crate) fn with_index<T>(&self, f: impl FnOnce(&mut Index) -> Result<T>) -> Result<T> {
        self.check_index_identity();
        let mut slot = self.index.borrow_mut();
        if slot.is_none() {
            let path = match self.index_path() {
                Some(path) => path,
                None => Index::cache_path(&self.vault_id)?,
            };
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| StoreError::io(dir, e))?;
            }
            let index = Index::open_at(self.root(), &self.vault_id, path.clone())?;
            self.index_identity.set(file_identity(&path));
            *slot = Some(index);
        }
        match slot.as_mut() {
            Some(index) => f(index),
            None => Err(StoreError::Invalid("index unavailable".into())),
        }
    }

    /// Drop the open connection when its database file is gone or another
    /// file now has its path (another process rebuilt the index).
    fn check_index_identity(&self) {
        if self.index.borrow().is_none() {
            return;
        }
        let Some(opened) = self.index_identity.get() else {
            // Without an identity Windows cannot tell a rebuilt database
            // from the open one: reopen conservatively.
            if cfg!(windows) {
                self.close_index();
            }
            return;
        };
        let path = match self.index_path() {
            Some(path) => Some(path),
            None => Index::cache_path(&self.vault_id).ok(),
        };
        let current = path.as_deref().and_then(file_identity);
        if current != Some(opened) {
            self.close_index();
        }
    }

    /// A counter that grows each time this handle drops its index
    /// connection: the database file was replaced by another process
    /// (checked on every index use and by this call) or
    /// [`Vault::close_index`] ran. A caller that caches facts about the
    /// index (for example "a complete walk exists") records the epoch with
    /// them and re-derives them once [`Vault::index_reopened_since`] says
    /// the connection changed.
    pub fn index_epoch(&self) -> u64 {
        self.check_index_identity();
        self.index_epoch.get()
    }

    /// Whether the index connection was dropped or replaced after `mark`
    /// (a value [`Vault::index_epoch`] returned).
    pub fn index_reopened_since(&self, mark: u64) -> bool {
        self.index_epoch() != mark
    }

    /// Open and initialize the device index without scanning vault files.
    pub fn initialize_index(&self) -> Result<()> {
        self.with_index(|_| Ok(()))
    }

    /// Drop the open index connection (after the database was deleted).
    pub fn close_index(&self) {
        if self.index.borrow_mut().take().is_some() {
            self.index_epoch.set(self.index_epoch.get() + 1);
        }
        self.index_identity.set(None);
    }

    /// The device index database path, if known.
    pub fn index_database(&self) -> Result<Option<PathBuf>> {
        match self.index_path() {
            Some(path) => Ok(Some(path)),
            None => Ok(Some(Index::cache_path(&self.vault_id)?)),
        }
    }

    /// Update the index for `paths` after an acknowledged write.
    pub(crate) fn index_update(&self, paths: &[&str]) -> IndexState {
        let result = if crate::fault::fail_index() {
            Err(StoreError::Invalid("injected index failure".into()))
        } else {
            self.with_index(|index| Ok(index.update_paths(paths)?))
        };
        match result {
            Ok(_) => IndexState::Ok,
            Err(_) => {
                // Best effort: if even the marker fails, the next CLI query's
                // stat-walk still finds the change.
                let _ = self.mark_index_pending(paths);
                IndexState::Pending
            }
        }
    }

    fn pending_path(&self) -> PathBuf {
        self.history.wi_dir().join(INDEX_PENDING)
    }

    /// Held while the pending marker is appended to or settled, so a
    /// repair never drops a path appended after it read the marker.
    fn pending_lock(&self) -> Result<std::fs::File> {
        let file = self.history.wi_dir().join(INDEX_PENDING_LOCK);
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&file)
            .map_err(|e| StoreError::io(&file, e))?;
        handle.lock().map_err(|e| StoreError::io(&file, e))?;
        Ok(handle)
    }

    pub(crate) fn mark_index_pending(&self, paths: &[&str]) -> Result<()> {
        use std::io::Write;
        let _lock = self.pending_lock()?;
        let file = self.pending_path();
        let mut handle = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file)
            .map_err(|e| StoreError::io(&file, e))?;
        let text: String = paths.iter().map(|path| format!("{path}\n")).collect();
        handle
            .write_all(text.as_bytes())
            .map_err(|e| StoreError::io(&file, e))?;
        durable::barrier(&handle, self.history.durability())
            .map_err(|e| StoreError::io(&file, e))?;
        Ok(())
    }

    /// Paths whose index update is pending: the marker and every claim a
    /// repair has not settled yet.
    pub fn index_pending(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut files = vec![self.pending_path()];
        files.extend(self.pending_claims());
        for file in files {
            if let Ok(text) = std::fs::read_to_string(file) {
                for line in text.lines() {
                    if !out.iter().any(|p| p == line) {
                        out.push(line.to_owned());
                    }
                }
            }
        }
        out
    }

    /// Claim files of repairs that did not settle (sorted).
    fn pending_claims(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(self.history.wi_dir()) else {
            return Vec::new();
        };
        let mut out: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with(INDEX_PENDING_CLAIM))
            })
            .map(|e| e.path())
            .collect();
        out.sort();
        out
    }

    /// Take the pending marker under the marker lock by renaming it to a
    /// claim file of this repair; writers then append to a fresh marker.
    /// Claims left by an earlier repair that did not settle are taken
    /// over. Returns the claimed paths and claim files.
    fn claim_pending(&self) -> Result<PendingClaim> {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let _lock = self.pending_lock()?;
        let marker = self.pending_path();
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mine = self.history.wi_dir().join(format!(
            "{INDEX_PENDING_CLAIM}{}-{n}-{nanos}",
            std::process::id()
        ));
        match std::fs::rename(&marker, &mine) {
            Ok(()) => durable::barrier_dir(&self.history.wi_dir(), self.history.durability())?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(StoreError::io(marker, e)),
        }
        let mut claim = PendingClaim::default();
        for file in self.pending_claims() {
            match std::fs::read_to_string(&file) {
                Ok(text) => {
                    claim.paths.extend(text.lines().map(str::to_owned));
                    claim.files.push(file);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(StoreError::io(file, e)),
            }
        }
        Ok(claim)
    }

    /// Delete the claim files of a repair that finished. Paths marked
    /// after the claim are in the marker, which is never touched here.
    fn settle_pending(&self, claim: &PendingClaim) -> Result<()> {
        if claim.files.is_empty() {
            return Ok(());
        }
        let _lock = self.pending_lock()?;
        for file in &claim.files {
            match std::fs::remove_file(file) {
                Ok(()) => {}
                // Another repair took it over and settled it.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(StoreError::io(file, e)),
            }
        }
        Ok(())
    }

    pub(crate) fn repair_index(&self) -> Result<Vec<String>> {
        let claim = self.claim_pending()?;
        let mut pending = claim.paths.clone();
        if pending.is_empty() {
            self.settle_pending(&claim)?;
            return Ok(pending);
        }
        pending.sort();
        pending.dedup();
        let paths: Vec<&str> = pending.iter().map(String::as_str).collect();
        self.with_index(|index| Ok(index.update_paths(&paths)?))?;
        if let Some(hook) = &self.hooks.after_index_repair {
            hook(&self.pending_path());
        }
        self.settle_pending(&claim)?;
        Ok(pending)
    }

    /// The index's complete-walk watermark (opens the index).
    pub fn index_as_of(&self) -> Result<Option<String>> {
        self.with_index(|index| Ok(index.index_as_of()?))
    }

    /// Rebuild the device index from the files (`wi reindex`) and record
    /// work requests for tracked notes deleted outside the engine.
    pub fn reindex(&self) -> Result<wi_index::SyncReport> {
        Ok(self.reindex_with_missing()?.0)
    }

    /// `reindex`, also returning the paths recorded as missing. An index
    /// database that cannot be opened is discarded (it is disposable) and
    /// built again; the pending marker is cleared after a rebuild.
    pub fn reindex_with_missing(&self) -> Result<(wi_index::SyncReport, Vec<String>)> {
        // Paths marked during the rebuild stay pending.
        let claimed = self.claim_pending()?;
        let report = match self.with_index(|index| Ok(index.rebuild()?)) {
            Ok(report) => report,
            Err(first) => {
                self.close_index();
                if !self.discard_index_database() {
                    return Err(first);
                }
                self.with_index(|index| Ok(index.rebuild()?))?
            }
        };
        if let Some(hook) = &self.hooks.after_index_repair {
            hook(&self.pending_path());
        }
        self.settle_pending(&claimed)?;
        let missing = self.record_missing()?;
        Ok((report, missing))
    }

    /// One slice of a time-sliced rebuild (budgeted maintenance): rebuild
    /// chunks of paths until `over()` says stop, resuming the rebuild an
    /// earlier slice left (progress lives in the index, committed with each
    /// chunk). With `restart` a new rebuild begins first. An index that
    /// cannot be opened or rebuilt is discarded and the slice retried on a
    /// fresh database. When the slice finishes the rebuild, the claimed
    /// pending paths are re-read and settled and missing notes recorded, as
    /// [`Vault::reindex`] does; returns the final report then.
    pub(crate) fn reindex_slice(
        &self,
        restart: bool,
        over: &dyn Fn() -> bool,
    ) -> Result<Option<wi_index::SyncReport>> {
        let claimed = self.claim_pending()?;
        let slice = |restart: bool| {
            self.with_index(|index| {
                if restart {
                    index.begin_rebuild()?;
                }
                Ok(index.rebuild_slice(REBUILD_CHUNK, over)?)
            })
        };
        let done = match slice(restart) {
            Ok(done) => done,
            Err(first) => {
                self.close_index();
                if !self.discard_index_database() {
                    return Err(first);
                }
                slice(true)?
            }
        };
        // Unfinished: the claims stay for the next repair or slice.
        let Some(report) = done else {
            return Ok(None);
        };
        if !claimed.paths.is_empty() {
            let mut paths = claimed.paths.clone();
            paths.sort();
            paths.dedup();
            let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
            self.with_index(|index| Ok(index.update_paths(&paths)?))?;
        }
        if let Some(hook) = &self.hooks.after_index_repair {
            hook(&self.pending_path());
        }
        self.settle_pending(&claimed)?;
        self.record_missing()?;
        Ok(Some(report))
    }

    /// Whether a time-sliced rebuild of the device index is unfinished.
    pub fn index_rebuilding(&self) -> bool {
        matches!(
            self.with_index(|index| Ok(index.rebuild_progress()?)),
            Ok(Some(_))
        )
    }

    /// Remove an unusable index database (a file, its WAL files, or an
    /// empty directory in its place). Never removes anything else.
    fn discard_index_database(&self) -> bool {
        let Ok(Some(path)) = self.index_database() else {
            return false;
        };
        let removed = match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => std::fs::remove_file(&path).is_ok(),
            Ok(meta) if meta.is_dir() => std::fs::remove_dir(&path).is_ok(),
            _ => false,
        };
        for suffix in ["-wal", "-shm"] {
            let mut side = path.clone().into_os_string();
            side.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(side));
        }
        removed
    }

    /// Stat-walk catch-up: hash only files whose stat changed.
    pub fn catch_up(&self) -> Result<wi_index::SyncReport> {
        let root = self.root().to_path_buf();
        self.with_index(|index| Ok(index.sync(&root)?))
    }
}

/// Pending-index paths a repair took, with the claim files holding them.
#[derive(Default)]
struct PendingClaim {
    paths: Vec<String>,
    files: Vec<PathBuf>,
}

/// (device, inode) of `path`, when it exists.
#[cfg(unix)]
fn file_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// (volume serial, file id) of `path` from an open handle, when it exists
/// (D12). The 128-bit id is folded to 64 bits; on NTFS its high half is 0.
#[cfg(windows)]
fn file_identity(path: &Path) -> Option<(u64, u64)> {
    let stat = crate::contained::stamp_path(path, true)?;
    #[allow(clippy::cast_possible_truncation)]
    let id = (stat.file_id as u64) ^ ((stat.file_id >> 64) as u64);
    Some((stat.volume_serial, id))
}

/// No file identity is available on this platform yet.
#[cfg(not(any(unix, windows)))]
fn file_identity(_path: &Path) -> Option<(u64, u64)> {
    None
}
