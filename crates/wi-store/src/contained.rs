//! Descriptor-relative file access anchored at the vault root (OPERATIONS
//! `#path-identity-and-containment`).
//!
//! Every directory below the root is opened one component at a time with
//! `O_NOFOLLOW | O_DIRECTORY`, so a symlink (or a non-directory) anywhere in
//! the path is refused, and later reads, renames and removals act on the
//! directory that was checked, not on whatever a path names by then. Used by
//! the write path, reads, file ingress and crash recovery.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;
use wi_history::durable::{self, Durability};

use crate::error::{Result, StoreError};

/// An open directory inside the vault.
#[derive(Debug)]
pub(crate) struct Dir {
    file: File,
    path: PathBuf,
}

fn io_err(path: &Path, e: Errno) -> StoreError {
    StoreError::io(path, std::io::Error::from(e))
}

fn unsafe_component(rel: &str) -> StoreError {
    StoreError::Protected {
        path: rel.to_owned(),
        reason: "symlink or non-directory path component",
    }
}

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

impl Dir {
    /// Open the vault root (already canonical).
    pub(crate) fn open_root(root: &Path) -> Result<Self> {
        let fd = rustix::fs::open(root, DIR_FLAGS, Mode::empty()).map_err(|e| io_err(root, e))?;
        Ok(Self {
            file: File::from(fd),
            path: root.to_path_buf(),
        })
    }

    /// The directories of `rel` (all but its last component), opened from
    /// the root without following symlinks, and the last component. `None`
    /// when a directory is missing.
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

    /// `open_parent`, creating missing directories durably.
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
            match rustix::fs::mkdirat(&dir.file, part, Mode::from_raw_mode(0o777)) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(e) => return Err(io_err(&dir.path.join(part), e)),
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
        match rustix::fs::openat(&self.file, name, DIR_FLAGS, Mode::empty()) {
            Ok(fd) => Ok(Some(Self {
                file: File::from(fd),
                path: self.path.join(name),
            })),
            Err(Errno::NOENT) => Ok(None),
            Err(Errno::LOOP | Errno::NOTDIR) => Err(unsafe_component(rel)),
            Err(e) => Err(io_err(&self.path.join(name), e)),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn file(&self) -> &File {
        &self.file
    }

    pub(crate) fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            file: self
                .file
                .try_clone()
                .map_err(|e| StoreError::io(&self.path, e))?,
            path: self.path.clone(),
        })
    }

    /// Open a regular file for reading without following a symlink;
    /// `None` if absent.
    pub(crate) fn open_regular(&self, name: &str) -> Result<Option<File>> {
        let path = self.path.join(name);
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let fd = match rustix::fs::openat(&self.file, name, flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(Errno::LOOP) => {
                return Err(StoreError::Protected {
                    path: path.display().to_string(),
                    reason: "symlink",
                });
            }
            Err(e) => return Err(io_err(&path, e)),
        };
        let stat = rustix::fs::fstat(&fd).map_err(|e| io_err(&path, e))?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(StoreError::Invalid(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        Ok(Some(File::from(fd)))
    }

    /// Read a regular file without following a symlink; `None` if absent.
    pub(crate) fn read_regular(&self, name: &str) -> Result<Option<Vec<u8>>> {
        let path = self.path.join(name);
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let fd = match rustix::fs::openat(&self.file, name, flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(Errno::LOOP) => {
                return Err(StoreError::Protected {
                    path: path.display().to_string(),
                    reason: "symlink",
                });
            }
            Err(e) => return Err(io_err(&path, e)),
        };
        let stat = rustix::fs::fstat(&fd).map_err(|e| io_err(&path, e))?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(StoreError::Invalid(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        let mut bytes = Vec::new();
        File::from(fd)
            .read_to_end(&mut bytes)
            .map_err(|e| StoreError::io(&path, e))?;
        Ok(Some(bytes))
    }

    /// `(mtime_ns, size)` of a regular file without following a symlink, in
    /// the index's stamp format; `None` if absent or not a regular file.
    pub(crate) fn stat_regular(&self, name: &str) -> Result<Option<(String, u64)>> {
        let stat = match rustix::fs::statat(&self.file, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return Ok(None),
            Err(e) => return Err(io_err(&self.path.join(name), e)),
        };
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Ok(None);
        }
        let nanos = i128::from(stat.st_mtime) * 1_000_000_000 + i128::from(stat.st_mtime_nsec);
        let mtime_ns = if nanos < 0 {
            format!("-{}", nanos.unsigned_abs())
        } else {
            nanos.to_string()
        };
        Ok(Some((
            mtime_ns,
            u64::try_from(stat.st_size).unwrap_or(u64::MAX),
        )))
    }

    /// Permission bits of the regular file `name` without following a
    /// symlink; `None` if absent or not a regular file.
    pub(crate) fn permissions(&self, name: &str) -> Result<Option<u32>> {
        let stat = match rustix::fs::statat(&self.file, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return Ok(None),
            Err(e) => return Err(io_err(&self.path.join(name), e)),
        };
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Ok(None);
        }
        // st_mode is u16 on Apple platforms and u32 on Linux.
        #[allow(clippy::useless_conversion)]
        let mode = u32::from(stat.st_mode);
        Ok(Some(mode & 0o777))
    }

    /// Create `name` with `bytes` (failing if it exists) and barrier it.
    /// `permissions` replaces the default `0666 & umask`, so a replacement
    /// keeps the bits of the file it replaces.
    pub(crate) fn create_new(
        &self,
        name: &str,
        bytes: &[u8],
        mode: Durability,
        permissions: Option<u32>,
    ) -> Result<()> {
        let path = self.path.join(name);
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = rustix::fs::openat(&self.file, name, flags, Mode::from_raw_mode(0o666))
            .map_err(|e| io_err(&path, e))?;
        if let Some(bits) = permissions {
            rustix::fs::fchmod(&fd, Mode::from_raw_mode(bits as _))
                .map_err(|e| io_err(&path, e))?;
        }
        let mut file = File::from(fd);
        file.write_all(bytes)
            .map_err(|e| StoreError::io(&path, e))?;
        durable::barrier(&file, mode).map_err(|e| StoreError::io(&path, e))
    }

    /// Whether an entry of any kind named `name` exists (no symlink follow).
    pub(crate) fn exists(&self, name: &str) -> Result<bool> {
        match rustix::fs::statat(&self.file, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => Ok(true),
            Err(Errno::NOENT) => Ok(false),
            Err(e) => Err(io_err(&self.path.join(name), e)),
        }
    }

    pub(crate) fn remove(&self, name: &str) -> Result<()> {
        rustix::fs::unlinkat(&self.file, name, AtFlags::empty())
            .map_err(|e| io_err(&self.path.join(name), e))
    }

    /// Rename `from` in this directory to `to` in `dest`. `Ok(false)` when
    /// `from` does not exist.
    pub(crate) fn rename(&self, from: &str, dest: &Dir, to: &str) -> Result<bool> {
        match rustix::fs::renameat(&self.file, from, &dest.file, to) {
            Ok(()) => Ok(true),
            Err(Errno::NOENT) => Ok(false),
            Err(e) => Err(io_err(&dest.path.join(to), e)),
        }
    }

    /// Names of the entries in this directory.
    pub(crate) fn names(&self) -> Result<Vec<String>> {
        let fd = self
            .file
            .try_clone()
            .map_err(|e| StoreError::io(&self.path, e))?;
        let dir = rustix::fs::Dir::read_from(&fd).map_err(|e| io_err(&self.path, e))?;
        let mut out = Vec::new();
        for entry in dir {
            let entry = entry.map_err(|e| io_err(&self.path, e))?;
            if let Ok(name) = entry.file_name().to_str()
                && name != "."
                && name != ".."
            {
                out.push(name.to_owned());
            }
        }
        Ok(out)
    }

    pub(crate) fn barrier(&self, mode: Durability) -> Result<()> {
        durable::barrier(&self.file, mode).map_err(|e| StoreError::io(&self.path, e))
    }
}

pub(crate) fn split(rel: &str) -> Result<(Vec<&str>, &str)> {
    let mut parts: Vec<&str> = rel.split('/').collect();
    let name = parts.pop().unwrap_or_default();
    if name.is_empty()
        || parts
            .iter()
            .chain(std::iter::once(&name))
            .any(|p| p.is_empty() || *p == "." || *p == "..")
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
