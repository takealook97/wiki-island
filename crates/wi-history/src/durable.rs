//! Durability barriers. Every fsync in the engine goes through [`barrier`]
//! or [`barrier_dir`]; a workspace `clippy.toml` disallows direct
//! `File::sync_all` and `File::sync_data`.
//!
//! The ordering rules (blob before record, objects and ref before
//! retirement) are the same in both modes; only the strength of each barrier
//! differs. See `docs/OPERATIONS.md#durability-modes`.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;

use crate::error::{HistoryError, Result};

/// Strength of a durability barrier, from the vault config key `durability`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Durability {
    /// POSIX `fsync`. On Linux this reaches stable storage. On Apple
    /// platforms it orders writes and survives process and OS crashes but
    /// may not survive power loss (the drive cache is not flushed).
    #[default]
    Standard,
    /// `F_FULLFSYNC` on Apple platforms (drive cache flush), `fsync`
    /// elsewhere.
    Full,
}

impl Durability {
    /// Parse the config value: `"standard"` or `"full"`.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "standard" => Some(Self::Standard),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    /// The config value naming this mode.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Full => "full",
        }
    }
}

/// Make a file's written data and metadata durable at `mode` strength.
#[cfg(unix)]
pub fn barrier(file: &File, mode: Durability) -> std::io::Result<()> {
    match mode {
        Durability::Standard => rustix::fs::fsync(file)?,
        Durability::Full => full_sync(file)?,
    }
    Ok(())
}

/// Non-Unix durability is unsupported until directory barriers are validated.
#[cfg(not(unix))]
pub fn barrier(_file: &File, _mode: Durability) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "unsupported-platform: durability barriers require Unix",
    ))
}

#[cfg(target_vendor = "apple")]
fn full_sync(file: &File) -> std::io::Result<()> {
    match rustix::fs::fcntl_fullfsync(file) {
        // Some file systems (network and FUSE mounts) do not implement
        // F_FULLFSYNC; plain fsync is the strongest barrier they offer.
        Err(rustix::io::Errno::NOTSUP | rustix::io::Errno::INVAL) => Ok(rustix::fs::fsync(file)?),
        other => Ok(other?),
    }
}

#[cfg(all(unix, not(target_vendor = "apple")))]
fn full_sync(file: &File) -> std::io::Result<()> {
    Ok(rustix::fs::fsync(file)?)
}

/// Make the entries of directory `path` (files created, renamed or removed
/// in it) durable at `mode` strength.
pub fn barrier_dir(path: &Path, mode: Durability) -> Result<()> {
    trace::record(trace::Kind::Dir, path);
    let dir = File::open(path).map_err(|e| HistoryError::io(path, e))?;
    barrier(&dir, mode).map_err(|e| HistoryError::io(path, e))
}

/// Barrier an open file, naming `path` in errors and traces.
pub(crate) fn barrier_at(file: &File, path: &Path, mode: Durability) -> Result<()> {
    trace::record(trace::Kind::File, path);
    barrier(file, mode).map_err(|e| HistoryError::io(path, e))
}

/// Open and barrier the file at `path`.
pub(crate) fn barrier_path(path: &Path, mode: Durability) -> Result<()> {
    let file = File::open(path).map_err(|e| HistoryError::io(path, e))?;
    barrier_at(&file, path, mode)
}

/// Create a directory (and parents) and barrier the parent of each created level.
pub fn create_dir_durable(path: &Path, mode: Durability) -> Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        create_dir_durable(parent, mode)?;
    }
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(HistoryError::io(path, e)),
    }
    if let Some(parent) = path.parent() {
        barrier_dir(parent, mode)?;
    }
    Ok(())
}

/// Replace `path` with `bytes` atomically: temp file, barrier, rename, barrier dir.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: Durability) -> Result<()> {
    let dir = path.parent().ok_or_else(|| {
        HistoryError::io(path, std::io::Error::other("path has no parent directory"))
    })?;
    let tmp = path.with_extension("tmp");
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .map_err(|e| HistoryError::io(&tmp, e))?;
        file.write_all(bytes)
            .map_err(|e| HistoryError::io(&tmp, e))?;
        barrier_at(&file, &tmp, mode)?;
    }
    fs::rename(&tmp, path).map_err(|e| HistoryError::io(path, e))?;
    trace::record(trace::Kind::Rename, path);
    barrier_dir(dir, mode)
}

/// Write a new file with `bytes` (failing if it exists), barrier it and its directory.
pub(crate) fn write_new_durable(path: &Path, bytes: &[u8], mode: Durability) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| HistoryError::io(path, e))?;
    file.write_all(bytes)
        .map_err(|e| HistoryError::io(path, e))?;
    barrier_at(&file, path, mode)?;
    if let Some(dir) = path.parent() {
        barrier_dir(dir, mode)?;
    }
    Ok(())
}

/// Barrier every file and directory below `root` (children before their
/// directory), then `root` itself. Used once, for freshly initialized git
/// metadata, which is small.
pub(crate) fn barrier_tree(root: &Path, mode: Durability) -> Result<()> {
    let entries = fs::read_dir(root).map_err(|e| HistoryError::io(root, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| HistoryError::io(root, e))?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|e| HistoryError::io(&path, e))?;
        if kind.is_dir() {
            barrier_tree(&path, mode)?;
        } else if kind.is_file() {
            barrier_path(&path, mode)?;
        }
    }
    barrier_dir(root, mode)
}

/// Test-only record of barrier calls, in order, per thread. Power loss
/// cannot be simulated, so ordering tests assert on this trace instead.
pub(crate) mod trace {
    use std::path::Path;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Kind {
        File,
        Dir,
        /// A rename publishing a file (not a barrier; marks ordering).
        Rename,
    }

    #[cfg(test)]
    thread_local! {
        static TRACE: std::cell::RefCell<Option<Vec<(Kind, std::path::PathBuf)>>> =
            const { std::cell::RefCell::new(None) };
    }

    #[cfg(test)]
    pub(crate) fn record(kind: Kind, path: &Path) {
        TRACE.with(|t| {
            if let Some(events) = t.borrow_mut().as_mut() {
                events.push((kind, path.to_path_buf()));
            }
        });
    }

    #[cfg(not(test))]
    #[inline]
    pub(crate) fn record(_kind: Kind, _path: &Path) {}

    /// Run `f` and return its result with the barriers it issued.
    #[cfg(test)]
    pub(crate) fn capture<T>(f: impl FnOnce() -> T) -> (T, Vec<(Kind, std::path::PathBuf)>) {
        TRACE.with(|t| *t.borrow_mut() = Some(Vec::new()));
        let out = f();
        let events = TRACE.with(|t| t.borrow_mut().take()).unwrap_or_default();
        (out, events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_parse_and_both_barriers_succeed() {
        assert_eq!(Durability::parse("standard"), Some(Durability::Standard));
        assert_eq!(Durability::parse("full"), Some(Durability::Full));
        assert_eq!(Durability::parse("Full"), None);
        assert_eq!(Durability::default(), Durability::Standard);
        let dir = tempfile::tempdir().unwrap();
        for mode in [Durability::Standard, Durability::Full] {
            let path = dir.path().join(mode.as_str());
            write_new_durable(&path, b"x", mode).unwrap();
            barrier_dir(dir.path(), mode).unwrap();
        }
    }

    /// No direct `sync_all`/`sync_data` outside this module.
    /// Backs up the clippy `disallowed-methods` lint for code it misses.
    #[test]
    fn no_direct_sync_calls_outside_the_barrier_helper() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![src];
        let mut offenders = Vec::new();
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs")
                    && !path.ends_with("durable.rs")
                {
                    let text = fs::read_to_string(&path).unwrap();
                    if text.contains(concat!("sync_", "all("))
                        || text.contains(concat!("sync_", "data("))
                    {
                        offenders.push(path);
                    }
                }
            }
        }
        assert!(offenders.is_empty(), "direct sync calls in {offenders:?}");
    }
}
