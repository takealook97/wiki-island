//! Placing a prepared temp file at its target (OPERATIONS write path step 7).
//!
//! Where the platform has an atomic exchange, the temp file and the target
//! swap names in one step and the displaced bytes stay in the temp path, so
//! an editor save that landed after the checksum check is never lost:
//! macOS `renameatx_np(RENAME_SWAP)` and Linux `renameat2(RENAME_EXCHANGE)`,
//! both through `rustix::fs::renameat_with` (no `unsafe` in this crate).
//! Creating a new note uses the no-replace flag (`RENAME_EXCL` /
//! `RENAME_NOREPLACE`); if an editor created the file meanwhile, the engine
//! swaps instead and keeps the editor's bytes the same way. A replacement
//! whose target was deleted after the check is stale; the engine never
//! recreates a file the owner deleted.
//!
//! When the file system refuses the flags (`EINVAL`, `ENOTSUP`, `ENOSYS`),
//! the engine re-hashes the target and then falls back to a plain rename.
//! That leaves a best-effort window between the re-hash and the rename in
//! which an editor save can be lost (exFAT, some network file systems).
//!
//! Every name is relative to a [`Dir`] opened without following symlinks.

use wi_history::durable::Durability;
use wi_history::hash_bytes;

use crate::contained::Dir;
use crate::error::{Result, StoreError};

/// How the new bytes reached the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Placed {
    /// Atomic swap; the temp name now holds the displaced bytes.
    Exchanged,
    /// New file created atomically without replacing anything.
    Created,
    /// Fallback rename (exchange unsupported here).
    Renamed,
}

/// Whether atomic exchange is compiled in for this platform.
pub const EXCHANGE_SUPPORTED: bool = cfg!(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple"
));

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn rename_flags(dir: &Dir, from: &str, to: &str, exchange: bool) -> std::io::Result<()> {
    use rustix::fs::{RenameFlags, renameat_with};
    let flags = if exchange {
        RenameFlags::EXCHANGE
    } else {
        RenameFlags::NOREPLACE
    };
    renameat_with(dir.file(), from, dir.file(), to, flags).map_err(std::io::Error::from)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn rename_flags(_dir: &Dir, _from: &str, _to: &str, _exchange: bool) -> std::io::Result<()> {
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}

#[cfg(unix)]
fn unsupported(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(code) if code == rustix::io::Errno::INVAL.raw_os_error()
            || code == rustix::io::Errno::NOTSUP.raw_os_error()
            || code == rustix::io::Errno::NOSYS.raw_os_error()
    ) || e.kind() == std::io::ErrorKind::Unsupported
}

#[cfg(not(unix))]
fn unsupported(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::Unsupported
}

/// Place temp file `tmp` at `name`, both in `dir`. `expected` is the hash
/// the precondition saw at `name` (`None`: absent); the fallback rename
/// re-checks it first.
pub(crate) fn place(dir: &Dir, tmp: &str, name: &str, expected: Option<&str>) -> Result<Placed> {
    let target = dir.path().join(name);
    let io = |e| StoreError::io(&target, e);
    let expect_exists = expected.is_some();
    match rename_flags(dir, tmp, name, expect_exists) {
        Ok(()) if expect_exists => Ok(Placed::Exchanged),
        Ok(()) => Ok(Placed::Created),
        Err(e) if unsupported(&e) => {
            let now = dir.read_regular(name)?.map(|b| hash_bytes(&b));
            if now.as_deref() != expected {
                return Err(StoreError::Stale {
                    path: target.display().to_string(),
                    expected: expected.unwrap_or("absent").to_owned(),
                    current: now,
                });
            }
            dir.rename(tmp, dir, name)?;
            Ok(Placed::Renamed)
        }
        // The owner deleted the file after the check: the replacement is
        // stale. Recreating it would undo the owner's deletion.
        Err(e) if expect_exists && e.kind() == std::io::ErrorKind::NotFound => {
            Err(StoreError::Stale {
                path: target.display().to_string(),
                expected: expected.unwrap_or_default().to_owned(),
                current: None,
            })
        }
        // Someone created the file after the check: swap and keep theirs.
        Err(e) if !expect_exists && e.kind() == std::io::ErrorKind::AlreadyExists => {
            rename_flags(dir, tmp, name, true).map_err(io)?;
            Ok(Placed::Exchanged)
        }
        Err(e) => Err(io(e)),
    }
}

/// Rename `from` to `to` in `dir` without replacing an existing `to`.
/// `Ok(false)` when `to` exists. Where the no-replace flag is refused, the
/// engine checks for `to` first (best effort, like [`place`]).
pub(crate) fn rename_no_replace(dir: &Dir, from: &str, to: &str) -> Result<bool> {
    match rename_flags(dir, from, to, false) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) if unsupported(&e) => {
            if dir.exists(to)? {
                return Ok(false);
            }
            dir.rename(from, dir, to)
        }
        Err(e) => Err(StoreError::io(dir.path().join(to), e)),
    }
}

/// A temp file that is removed on drop unless it was consumed.
pub(crate) struct TempFile {
    pub(crate) dir: Dir,
    pub(crate) name: String,
    armed: bool,
}

impl TempFile {
    /// Write `bytes` to a new temp file next to `target_name` and barrier
    /// it, with `permissions` when given (the bits of the replaced file).
    pub(crate) fn create(
        dir: &Dir,
        target_name: &str,
        bytes: &[u8],
        mode: Durability,
        permissions: Option<u32>,
    ) -> Result<Self> {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let name = format!(
            ".{target_name}{}{}-{n}-{nanos}",
            crate::paths::TMP_MARK,
            std::process::id()
        );
        dir.create_new(&name, bytes, mode, permissions)?;
        Ok(Self {
            dir: dir.try_clone()?,
            name,
            armed: true,
        })
    }

    /// The temp name was renamed away or is now owned elsewhere.
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.dir.remove(&self.name);
        }
    }
}
