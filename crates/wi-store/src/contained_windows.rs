//! Handle-relative file access anchored at the vault root on Windows
//! (OPERATIONS `#path-identity-and-containment`).
//!
//! The same API as the Unix `contained.rs`, built on [`wi_sys::fs::Dir`]:
//! every directory below the root is opened one component at a time
//! relative to the checked parent handle, and every symbolic link,
//! junction or other reparse point is refused, so later reads, renames and
//! removals act on the directory that was checked. Components that are
//! short (8.3) aliases of a different long name are refused too, so
//! protected-path checks always see the real names. No I/O joins paths.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use wi_history::durable::{self, Durability};
use wi_sys::fs::{self as sys, Kind};

use crate::error::{Result, StoreError};

/// 1970-01-01 in FILETIME units (100 ns intervals since 1601-01-01).
const FILETIME_UNIX_EPOCH: i64 = 116_444_736_000_000_000;

/// An open directory inside the vault.
#[derive(Debug)]
pub(crate) struct Dir {
    dir: sys::Dir,
    path: PathBuf,
}

fn unsafe_component(rel: &str) -> StoreError {
    StoreError::Protected {
        path: rel.to_owned(),
        reason: "symlink or non-directory path component",
    }
}

fn alias_component(rel: &str) -> StoreError {
    StoreError::Protected {
        path: rel.to_owned(),
        reason: "short-name alias path component",
    }
}

/// Retry `op` while another handle is in the way (D10b): 5 tries, 20 ms
/// doubling. The last error is returned unchanged.
pub(crate) fn retry_shared<T>(mut op: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut delay = Duration::from_millis(20);
    for _ in 0..4 {
        match op() {
            Err(e) if sys::is_sharing_violation(&e) => {
                #[cfg(test)]
                retry_tests::after_failure();
                std::thread::sleep(delay);
                delay *= 2;
            }
            other => return other,
        }
    }
    op()
}

/// The error a retried mutation returns when its source name now refers to
/// a different file than the one first opened: the caller's view is stale.
pub(crate) fn map_io(path: &Path, e: std::io::Error) -> StoreError {
    if sys::is_identity_change(&e) {
        return StoreError::Stale {
            path: path.display().to_string(),
            expected: "the file first opened".to_owned(),
            current: Some("a different file".to_owned()),
        };
    }
    StoreError::io(path, e)
}

/// `last_write` (FILETIME) as nanoseconds since the Unix epoch, in the
/// index's stamp format (the same value `Metadata::modified` gives).
pub(crate) fn filetime_ns(last_write: i64) -> String {
    let nanos = (i128::from(last_write) - i128::from(FILETIME_UNIX_EPOCH)) * 100;
    if nanos < 0 {
        format!("-{}", nanos.unsigned_abs())
    } else {
        nanos.to_string()
    }
}

impl Dir {
    /// Open the vault root (already canonical).
    pub(crate) fn open_root(root: &Path) -> Result<Self> {
        let dir = sys::Dir::open_root(root).map_err(|e| StoreError::io(root, e))?;
        Ok(Self {
            dir,
            path: root.to_path_buf(),
        })
    }

    /// The directories of `rel` (all but its last component), opened from
    /// the root without following reparse points, and the last component.
    /// `None` when a directory is missing.
    pub(crate) fn open_parent(root: &Path, rel: &str) -> Result<Option<(Self, String)>> {
        let (dirs, name) = split(rel)?;
        let mut dir = Self::open_root(root)?;
        for part in dirs {
            match dir.child(part, rel)? {
                Some(next) => dir = next,
                None => return Ok(None),
            }
        }
        Ok(Some((dir, name.to_owned())))
    }

    /// `open_parent`, creating missing directories.
    pub(crate) fn create_parent(
        root: &Path,
        rel: &str,
        mode: Durability,
    ) -> Result<(Self, String)> {
        let (dirs, name) = split(rel)?;
        let mut dir = Self::open_root(root)?;
        for part in dirs {
            if let Some(next) = dir.child(part, rel)? {
                dir = next;
                continue;
            }
            match dir.dir.create_dir(part) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) if sys::is_reparse_refusal(&e) => return Err(unsafe_component(rel)),
                Err(e) => return Err(StoreError::io(dir.path.join(part), e)),
            }
            dir.barrier(mode)?;
            dir = dir.child(part, rel)?.ok_or_else(|| {
                StoreError::io(
                    dir.path.join(part),
                    std::io::Error::from(std::io::ErrorKind::NotFound),
                )
            })?;
        }
        Ok((dir, name.to_owned()))
    }

    fn child(&self, name: &str, rel: &str) -> Result<Option<Self>> {
        match self.dir.open_dir(name) {
            Ok(dir) => {
                self.check_alias(name, rel)?;
                Ok(Some(Self {
                    dir,
                    path: self.path.join(name),
                }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e)
                if sys::is_reparse_refusal(&e) || e.kind() == std::io::ErrorKind::NotADirectory =>
            {
                Err(unsafe_component(rel))
            }
            Err(e) => Err(StoreError::io(self.path.join(name), e)),
        }
    }

    /// Refuse `name` when it names an existing entry whose on-disk name is
    /// different, i.e. `name` is a short (8.3) alias (D7b). Check every
    /// existing name, including explicitly assigned aliases without `~`.
    fn check_alias(&self, name: &str, rel: &str) -> Result<()> {
        match self.dir.long_name(name) {
            Ok(long) if long.to_lowercase() == name.to_lowercase() => Ok(()),
            Ok(_) => Err(alias_component(rel)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) if sys::is_reparse_refusal(&e) => Err(unsafe_component(rel)),
            Err(e) => Err(StoreError::io(self.path.join(name), e)),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The checked directory handle.
    pub(crate) fn sys(&self) -> &sys::Dir {
        &self.dir
    }

    pub(crate) fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            dir: self
                .dir
                .try_clone()
                .map_err(|e| StoreError::io(&self.path, e))?,
            path: self.path.clone(),
        })
    }

    fn open(&self, name: &str, write: bool) -> Result<Option<File>> {
        let path = self.path.join(name);
        self.check_alias(name, &path.display().to_string())?;
        match self.dir.open_file(name, write) {
            Ok(file) => Ok(Some(file)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) if sys::is_reparse_refusal(&e) => Err(StoreError::Protected {
                path: path.display().to_string(),
                reason: "symlink",
            }),
            Err(e) if e.kind() == std::io::ErrorKind::IsADirectory => Err(StoreError::Invalid(
                format!("{} is not a regular file", path.display()),
            )),
            Err(e) => Err(StoreError::io(&path, e)),
        }
    }

    /// Open a regular file for reading without following a reparse point;
    /// `None` if absent.
    pub(crate) fn open_regular(&self, name: &str) -> Result<Option<File>> {
        self.open(name, false)
    }

    /// Read a regular file without following a reparse point; `None` if
    /// absent.
    pub(crate) fn read_regular(&self, name: &str) -> Result<Option<Vec<u8>>> {
        let Some(mut file) = self.open(name, false)? else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|e| StoreError::io(self.path.join(name), e))?;
        Ok(Some(bytes))
    }

    /// `read_regular` that refuses a file above `max_bytes` with
    /// [`StoreError::TooLarge`], reading at most `max_bytes + 1` bytes.
    pub(crate) fn read_regular_bounded(
        &self,
        name: &str,
        max_bytes: u64,
    ) -> Result<Option<Vec<u8>>> {
        let Some(file) = self.open(name, false)? else {
            return Ok(None);
        };
        crate::bounded::read_capped(file, max_bytes, &self.path.join(name)).map(Some)
    }

    fn stat_file(&self, name: &str) -> Result<Option<sys::Stat>> {
        match self.dir.stat(name) {
            Ok(Some(stat)) if stat.kind == Kind::File => Ok(Some(stat)),
            Ok(_) => Ok(None),
            Err(e) => Err(StoreError::io(self.path.join(name), e)),
        }
    }

    /// `(mtime_ns, size)` of a regular file without following a reparse
    /// point, in the index's stamp format; `None` if absent or not a
    /// regular file.
    pub(crate) fn stat_regular(&self, name: &str) -> Result<Option<(String, u64)>> {
        Ok(self
            .stat_file(name)?
            .map(|stat| (filetime_ns(stat.last_write), stat.size)))
    }

    /// Permission bits of the regular file `name`; `None` if absent or not
    /// a regular file. Windows has no mode bits: a read-only file reports
    /// `0o444`, any other `0o666`.
    pub(crate) fn permissions(&self, name: &str) -> Result<Option<u32>> {
        Ok(self
            .stat_file(name)?
            .map(|stat| if stat.readonly { 0o444 } else { 0o666 }))
    }

    /// Create `name` with `bytes` (failing if it exists) and barrier it.
    /// `permissions` is not applied on Windows: the read-only attribute
    /// would block the engine's own later replacement.
    pub(crate) fn create_new(
        &self,
        name: &str,
        bytes: &[u8],
        mode: Durability,
        _permissions: Option<u32>,
    ) -> Result<()> {
        let path = self.path.join(name);
        let mut file = self.dir.create_file(name).map_err(|e| {
            if sys::is_reparse_refusal(&e) {
                return StoreError::Protected {
                    path: path.display().to_string(),
                    reason: "symlink",
                };
            }
            StoreError::io(&path, e)
        })?;
        file.write_all(bytes)
            .map_err(|e| StoreError::io(&path, e))?;
        durable::barrier(&file, mode).map_err(|e| StoreError::io(&path, e))
    }

    /// Whether an entry of any kind named `name` exists (no reparse
    /// follow).
    pub(crate) fn exists(&self, name: &str) -> Result<bool> {
        self.dir
            .stat(name)
            .map(|s| s.is_some())
            .map_err(|e| StoreError::io(self.path.join(name), e))
    }

    pub(crate) fn remove(&self, name: &str) -> Result<()> {
        let path = self.path.join(name);
        self.check_alias(name, &path.display().to_string())?;
        self.retry_same(name, |id| self.dir.unlink_if(name, id))
            .map_err(|e| map_io(&path, e))
    }

    /// Run the destructive `op` on the source `name` with the sharing
    /// retry (D10b), bound to the object `name` refers to now: its identity
    /// is captured from a metadata-only handle before the first attempt
    /// that can read it, and `op` must refuse a different object, so a retry never reopens a
    /// file swapped in under the same name. A retry also re-checks that
    /// `name` is still the entry's long name.
    pub(crate) fn retry_same<T>(
        &self,
        name: &str,
        mut op: impl FnMut(sys::Identity) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let mut id = None;
        let mut first = true;
        retry_shared(|| {
            if !std::mem::take(&mut first) {
                self.recheck_alias(name)?;
            }
            let id = match id {
                Some(id) => id,
                // Captured once, on the first attempt that can read it.
                None => *id.insert(self.identity(name)?),
            };
            op(id)
        })
    }

    /// The identity of the entry `name` without following it.
    pub(crate) fn identity(&self, name: &str) -> std::io::Result<sys::Identity> {
        self.dir
            .stat(name)?
            .map(|stat| stat.identity())
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
    }

    /// [`Dir::check_alias`] for a retry: a name that became a short alias
    /// no longer refers to the entry that was checked.
    fn recheck_alias(&self, name: &str) -> std::io::Result<()> {
        match self.dir.long_name(name) {
            Ok(long) if long.to_lowercase() == name.to_lowercase() => Ok(()),
            Ok(_) => Err(sys::identity_changed()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Rename `from` in this directory to `to` in `dest`, replacing an
    /// existing `to`. `Ok(false)` when `from` does not exist.
    pub(crate) fn rename(&self, from: &str, dest: &Dir, to: &str) -> Result<bool> {
        self.rename_with(from, dest, to, true)
    }

    /// Rename `from` to `to` in `dest`; without `replace` an existing `to`
    /// yields `ErrorKind::AlreadyExists` and is left untouched.
    pub(crate) fn rename_with(
        &self,
        from: &str,
        dest: &Dir,
        to: &str,
        replace: bool,
    ) -> Result<bool> {
        let target = dest.path.join(to);
        self.check_alias(from, &self.path.join(from).display().to_string())?;
        dest.check_alias(to, &target.display().to_string())?;
        match self.rename_raw(from, dest, to, replace) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) if sys::is_reparse_refusal(&e) => Err(StoreError::Protected {
                path: self.path.join(from).display().to_string(),
                reason: "symlink",
            }),
            Err(e) => Err(map_io(&target, e)),
        }
    }

    /// The handle rename with the sharing-violation retry and no mapping.
    pub(crate) fn rename_raw(
        &self,
        from: &str,
        dest: &Dir,
        to: &str,
        replace: bool,
    ) -> std::io::Result<()> {
        self.retry_same(from, |id| {
            self.dir.rename_if(from, &dest.dir, to, replace, id)
        })
    }

    /// Names of the entries in this directory. Names that are not valid
    /// Unicode are skipped, as on Unix.
    pub(crate) fn names(&self) -> Result<Vec<String>> {
        let entries = self.dir.list().map_err(|e| StoreError::io(&self.path, e))?;
        Ok(entries
            .into_iter()
            .filter(|(name, kind)| !(*kind == Kind::Other && name.contains('\u{FFFD}')))
            .map(|(name, _)| name)
            .collect())
    }

    /// Directory barrier: Windows has no directory flush, so this only
    /// keeps the call order of the Unix code (D11).
    pub(crate) fn barrier(&self, _mode: Durability) -> Result<()> {
        self.dir.flush().map_err(|e| StoreError::io(&self.path, e))
    }
}

/// Metadata of the entry at the absolute `path` from an open handle
/// (D12), or `None` when it cannot be opened. With `follow` a final
/// reparse point is resolved; without it, it is reported as
/// [`Kind::Reparse`]. Used for files outside the vault (the index) and for
/// stat stamps of files the walk already found.
pub(crate) fn stamp_path(path: &Path, follow: bool) -> Option<sys::Stat> {
    use std::os::windows::fs::OpenOptionsExt;
    /// `FILE_READ_ATTRIBUTES`: enough to query, no data access.
    const READ_ATTRIBUTES: u32 = 0x80;
    /// Share read, write and delete, so no other process is blocked.
    const SHARE_ALL: u32 = 0x7;
    /// `FILE_FLAG_BACKUP_SEMANTICS`: also open directories.
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    /// `FILE_FLAG_OPEN_REPARSE_POINT`: open a reparse point itself.
    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let flags = if follow {
        BACKUP_SEMANTICS
    } else {
        BACKUP_SEMANTICS | OPEN_REPARSE_POINT
    };
    let file = std::fs::OpenOptions::new()
        .access_mode(READ_ATTRIBUTES)
        .share_mode(SHARE_ALL)
        .custom_flags(flags)
        .open(path)
        .ok()?;
    sys::stamp(&file).ok()
}

pub(crate) fn split(rel: &str) -> Result<(Vec<&str>, &str)> {
    let mut parts: Vec<&str> = rel.split('/').collect();
    let name = parts.pop().unwrap_or_default();
    if name.is_empty()
        || parts
            .iter()
            .chain(std::iter::once(&name))
            .any(|p| p.is_empty() || *p == "." || *p == ".." || p.contains(['\\', ':']))
    {
        return Err(StoreError::Protected {
            path: rel.to_owned(),
            reason: "not a normalized vault path",
        });
    }
    Ok((parts, name))
}

/// Read the regular file at the vault-relative `rel` through checked
/// directories; `None` when it or a directory is absent.
pub(crate) fn read_rel(root: &Path, rel: &str) -> Result<Option<Vec<u8>>> {
    match Dir::open_parent(root, rel)? {
        Some((dir, name)) => dir.read_regular(&name),
        None => Ok(None),
    }
}

/// Whether the regular file at the vault-relative `rel` can be opened through
/// the checked directories, reading no bytes; `false` when it or a directory
/// is absent. Links and non-regular files give the errors `read_rel` gives.
pub(crate) fn open_rel_regular(root: &Path, rel: &str) -> Result<bool> {
    match Dir::open_parent(root, rel)? {
        Some((dir, name)) => Ok(dir.open_regular(&name)?.is_some()),
        None => Ok(false),
    }
}

/// `read_rel` that refuses a file above `max_bytes` with
/// [`StoreError::TooLarge`] naming `rel`, without reading it whole.
pub(crate) fn read_rel_bounded(root: &Path, rel: &str, max_bytes: u64) -> Result<Option<Vec<u8>>> {
    match Dir::open_parent(root, rel)? {
        Some((dir, name)) => dir
            .read_regular_bounded(&name, max_bytes)
            .map_err(|e| crate::bounded::name_rel(e, rel)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use std::cell::RefCell;
    use std::os::windows::fs::OpenOptionsExt;

    type RetryHook = Box<dyn FnOnce()>;
    thread_local! {
        static AFTER_FAILURE: RefCell<Option<RetryHook>> = const { RefCell::new(None) };
    }

    pub(super) fn after_failure() {
        let hook = AFTER_FAILURE.with(|slot| slot.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
    }

    fn replace_source_after_first_failure(root: &Path) {
        // Refuse the first DELETE open while allowing metadata reads.
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1 | 0x2)
            .open(root.join("busy.tmp"))
            .unwrap();
        let root = root.to_path_buf();
        AFTER_FAILURE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                drop(held);
                std::fs::rename(root.join("busy.tmp"), root.join("original.tmp")).unwrap();
                std::fs::write(root.join("busy.tmp"), b"unrecorded editor bytes").unwrap();
            }));
        });
    }

    #[test]
    fn remove_does_not_retry_on_a_different_file() {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path();
        std::fs::write(root.join("busy.tmp"), b"known staging bytes").unwrap();
        let dir = Dir::open_root(root).unwrap();
        replace_source_after_first_failure(root);
        let result = dir.remove("busy.tmp");
        assert!(
            matches!(result, Err(StoreError::Stale { .. })),
            "a different file must not be deleted: {result:?}"
        );
        assert_eq!(
            std::fs::read(root.join("busy.tmp")).unwrap(),
            b"unrecorded editor bytes"
        );
        assert_eq!(
            std::fs::read(root.join("original.tmp")).unwrap(),
            b"known staging bytes"
        );
    }

    #[test]
    fn rename_does_not_retry_on_a_different_file() {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path();
        std::fs::write(root.join("busy.tmp"), b"known staging bytes").unwrap();
        let dir = Dir::open_root(root).unwrap();
        replace_source_after_first_failure(root);
        let result = dir.rename_raw("busy.tmp", &dir, "published.md", false);
        assert!(
            result.as_ref().is_err_and(sys::is_identity_change),
            "a different source must not be published: {result:?}"
        );
        assert_eq!(
            std::fs::read(root.join("busy.tmp")).unwrap(),
            b"unrecorded editor bytes"
        );
        assert!(!root.join("published.md").exists());
    }

    #[test]
    fn retried_remove_still_deletes_the_same_file() {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path();
        std::fs::write(root.join("busy.tmp"), b"known staging bytes").unwrap();
        let dir = Dir::open_root(root).unwrap();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1 | 0x2)
            .open(root.join("busy.tmp"))
            .unwrap();
        AFTER_FAILURE.with(|slot| *slot.borrow_mut() = Some(Box::new(move || drop(held))));
        dir.remove("busy.tmp").unwrap();
        assert!(!root.join("busy.tmp").exists());
    }
}
