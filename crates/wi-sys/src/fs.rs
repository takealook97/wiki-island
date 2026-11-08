//! Handle-relative file system access that never follows reparse points.
//!
//! A [`Dir`] holds an open directory handle. Every method takes ONE
//! component name and resolves it relative to that handle with
//! `NtCreateFile` (`OBJECT_ATTRIBUTES.RootDirectory`), so a concurrent
//! rename of an ancestor cannot redirect the operation, and every open uses
//! `FILE_OPEN_REPARSE_POINT` so symbolic links, junctions and other reparse
//! points are opened as themselves and then refused.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::mem::{MaybeUninit, offset_of, size_of};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{
    AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, HandleOrInvalid, OwnedHandle,
};
use std::path::{Path, PathBuf};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_INFORMATION_CLASS, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
    FILE_OPEN_REPARSE_POINT, FILE_RENAME_POSIX_SEMANTICS, FILE_RENAME_REPLACE_IF_EXISTS,
    FILE_SYNCHRONOUS_IO_NONALERT, FileRenameInformation, FileRenameInformationEx, NtCreateFile,
    NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_CANT_ACCESS_FILE, ERROR_DELETE_PENDING, ERROR_INVALID_FUNCTION,
    ERROR_INVALID_PARAMETER, ERROR_LOCK_VIOLATION, ERROR_MORE_DATA, ERROR_NO_MORE_FILES,
    ERROR_NOT_SUPPORTED, ERROR_SHARING_VIOLATION, ERROR_STOPPED_ON_SYMLINK, HANDLE,
    OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, STATUS_DELETE_PENDING, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, DELETE, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_ID_BOTH_DIR_INFO,
    FILE_ID_INFO, FILE_INFO_BY_HANDLE_CLASS, FILE_LIST_DIRECTORY, FILE_NAME_INFO,
    FILE_READ_ATTRIBUTES, FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_STANDARD_INFO, FILE_TRAVERSE, FileAttributeTagInfo, FileBasicInfo, FileDispositionInfo,
    FileDispositionInfoEx, FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo, FileIdInfo,
    FileNameInfo, FileNormalizedNameInfo, FileStandardInfo, FlushFileBuffers,
    GetFileInformationByHandle, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
    OPEN_EXISTING, SYNCHRONIZE, SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

/// Share mode of every handle this module opens: other processes may read,
/// write, rename and delete while the handle is open.
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
/// Access rights of a directory handle: enumerate, traverse, query.
const DIR_ACCESS: u32 = FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;

/// What a directory entry is, judged without following it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A regular file.
    File,
    /// A directory that is not a reparse point.
    Dir,
    /// A symbolic link, junction or any other reparse point.
    Reparse,
    /// Anything else, including entries whose name is not valid UTF-16.
    Other,
}

/// Metadata of an entry, read from a handle without following it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    /// Entry kind.
    pub kind: Kind,
    /// End-of-file position in bytes.
    pub size: u64,
    /// Last write time in 100 ns intervals since 1601-01-01 UTC.
    pub last_write: i64,
    /// Last metadata or data change time in 100 ns intervals since
    /// 1601-01-01 UTC.
    pub change_time: i64,
    /// Serial number of the volume holding the entry.
    pub volume_serial: u64,
    /// File identifier, unique on its volume.
    pub file_id: u128,
    /// Whether the read-only attribute is set.
    pub readonly: bool,
}

/// Volume serial number and file identifier: which object an entry is,
/// independent of its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    /// Serial number of the volume holding the entry.
    pub volume_serial: u64,
    /// File identifier, unique on its volume.
    pub file_id: u128,
}

impl Stat {
    /// The object identity of the entry this metadata describes.
    pub fn identity(&self) -> Identity {
        Identity {
            volume_serial: self.volume_serial,
            file_id: self.file_id,
        }
    }
}

/// Marker payload of the error returned when a name no longer refers to
/// the expected object; see [`is_identity_change`].
#[derive(Debug)]
struct IdentityChanged;

impl std::fmt::Display for IdentityChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the entry was replaced by a different file")
    }
}

impl std::error::Error for IdentityChanged {}

/// Whether `e` is the error [`Dir::unlink_if`] or [`Dir::rename_if`]
/// returns because the name now refers to a different object.
pub fn is_identity_change(e: &io::Error) -> bool {
    e.get_ref()
        .is_some_and(|inner| inner.is::<IdentityChanged>())
}

/// The error for which [`is_identity_change`] holds, for callers that
/// detect a changed binding themselves.
pub fn identity_changed() -> io::Error {
    io::Error::other(IdentityChanged)
}

/// Refuse unless the open `handle` is the object `expected` names.
fn expect_identity(handle: &OwnedHandle, expected: Option<Identity>) -> io::Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    if stamp_handle(handle.as_handle())?.identity() != expected {
        return Err(identity_changed());
    }
    Ok(())
}

/// Marker payload of the error returned when a component is a reparse
/// point; see [`is_reparse_refusal`].
#[derive(Debug)]
struct ReparseRefused;

impl std::fmt::Display for ReparseRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("refusing to use a symbolic link, junction or other reparse point")
    }
}

impl std::error::Error for ReparseRefused {}

fn reparse_refused() -> io::Error {
    io::Error::other(ReparseRefused)
}

/// Whether `e` is the error a [`Dir`] method returns because a component
/// is a symbolic link, junction or other reparse point.
pub fn is_reparse_refusal(e: &io::Error) -> bool {
    if e.get_ref()
        .is_some_and(|inner| inner.is::<ReparseRefused>())
    {
        return true;
    }
    matches!(
        e.raw_os_error().and_then(|c| u32::try_from(c).ok()),
        Some(ERROR_STOPPED_ON_SYMLINK | ERROR_CANT_ACCESS_FILE)
    )
}

/// Whether `e` means another handle is in the way and the operation may
/// succeed if retried shortly: a sharing or lock violation, a pending
/// delete, or access denied (Windows also reports an open replacement
/// target or a delete-pending entry that way). Callers retry with a bounded
/// backoff and treat persistent failure as an ordinary error.
pub fn is_sharing_violation(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error().and_then(|c| u32::try_from(c).ok()),
        Some(
            ERROR_SHARING_VIOLATION
                | ERROR_LOCK_VIOLATION
                | ERROR_DELETE_PENDING
                | ERROR_ACCESS_DENIED
        )
    )
}

/// An open directory that is known not to be a reparse point. All entry
/// operations are relative to this handle.
#[derive(Debug)]
pub struct Dir {
    handle: OwnedHandle,
}

impl Dir {
    /// Open the directory at `path`. Refuses when `path` itself is a
    /// reparse point or not a directory. Ancestors of `path` are resolved
    /// normally.
    pub fn open_root(path: &Path) -> io::Result<Dir> {
        // The extended form lifts the MAX_PATH limit without requiring an
        // OS policy or manifest opt-in.
        let wide = crate::wide_nul(&extended_path(path))?;
        // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives
        // the call; the security attributes and template handle are null,
        // which CreateFileW accepts.
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                DIR_ACCESS,
                SHARE_ALL,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        // SAFETY: CreateFileW returns either a new handle that we own or
        // INVALID_HANDLE_VALUE, which `HandleOrInvalid` is designed for.
        let handle = OwnedHandle::try_from(unsafe { HandleOrInvalid::from_raw_handle(raw) })
            .map_err(|_| io::Error::last_os_error())?;
        expect_dir(&handle)?;
        Ok(Dir { handle })
    }

    /// Open the existing subdirectory `name`; refuses reparse points and
    /// non-directories.
    pub fn open_dir(&self, name: &str) -> io::Result<Dir> {
        let handle = self.nt_open(name, DIR_ACCESS, FILE_OPEN, 0)?;
        expect_dir(&handle)?;
        Ok(Dir { handle })
    }

    /// Create the subdirectory `name` and open it. Fails with
    /// `ErrorKind::AlreadyExists` when any entry named `name` exists,
    /// including a reparse point, which is left untouched.
    pub fn create_dir(&self, name: &str) -> io::Result<Dir> {
        let handle = self
            .nt_open(name, DIR_ACCESS, FILE_CREATE, FILE_DIRECTORY_FILE)
            .map_err(|e| self.collision(name, e))?;
        Ok(Dir { handle })
    }

    /// Open the existing regular file `name` for reading, and for writing
    /// when `write` (without truncation). Refuses reparse points and
    /// directories. The handle shares read, write and delete access.
    pub fn open_file(&self, name: &str, write: bool) -> io::Result<File> {
        let access = if write {
            FILE_GENERIC_READ | FILE_GENERIC_WRITE
        } else {
            FILE_GENERIC_READ
        };
        let handle = self.nt_open(name, access, FILE_OPEN, 0)?;
        expect_file(&handle)?;
        Ok(File::from(handle))
    }

    /// Create the regular file `name` for reading and writing. Fails with
    /// `ErrorKind::AlreadyExists` when any entry named `name` exists,
    /// including a reparse point, which is left untouched.
    pub fn create_file(&self, name: &str) -> io::Result<File> {
        let handle = self
            .nt_open(
                name,
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                FILE_CREATE,
                FILE_NON_DIRECTORY_FILE,
            )
            .map_err(|e| self.collision(name, e))?;
        Ok(File::from(handle))
    }

    /// Metadata of the entry `name` without following it; `None` when it
    /// does not exist. A reparse point is reported as [`Kind::Reparse`].
    pub fn stat(&self, name: &str) -> io::Result<Option<Stat>> {
        match self.nt_open(name, FILE_READ_ATTRIBUTES | SYNCHRONIZE, FILE_OPEN, 0) {
            Ok(handle) => stamp_handle(handle.as_handle()).map(Some),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Delete the non-directory entry `name` with POSIX semantics (the name
    /// disappears at once even if other handles stay open). Refuses
    /// reparse points and directories. The read-only attribute is ignored
    /// where the system supports that, as `unlink` does on Unix.
    pub fn unlink(&self, name: &str) -> io::Result<()> {
        self.unlink_checked(name, None)
    }

    /// [`Dir::unlink`], refused with an error for which
    /// [`is_identity_change`] holds when `name` no longer refers to the
    /// object `expected` (for example after a retry reopened the name).
    pub fn unlink_if(&self, name: &str, expected: Identity) -> io::Result<()> {
        self.unlink_checked(name, Some(expected))
    }

    fn unlink_checked(&self, name: &str, expected: Option<Identity>) -> io::Result<()> {
        let handle = self.nt_open(
            name,
            DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_OPEN,
            0,
        )?;
        expect_file(&handle)?;
        expect_identity(&handle, expected)?;
        delete_handle(&handle, true)
    }

    /// Delete the empty subdirectory `name` with POSIX semantics. Refuses
    /// reparse points and non-directories.
    pub fn remove_dir(&self, name: &str) -> io::Result<()> {
        let handle = self.nt_open(
            name,
            DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_OPEN,
            0,
        )?;
        expect_dir(&handle)?;
        delete_handle(&handle, false)
    }

    /// Rename the entry `from` in this directory to `to` in `to_dir`, with
    /// POSIX semantics. With `replace` an existing non-directory target is
    /// replaced atomically; without it an existing target yields
    /// `ErrorKind::AlreadyExists` and is left untouched. Refuses when
    /// `from` is a reparse point.
    pub fn rename(&self, from: &str, to_dir: &Dir, to: &str, replace: bool) -> io::Result<()> {
        self.rename_checked(from, to_dir, to, replace, None)
    }

    /// [`Dir::rename`], refused with an error for which
    /// [`is_identity_change`] holds when `from` no longer refers to the
    /// object `expected` (for example after a retry reopened the name).
    pub fn rename_if(
        &self,
        from: &str,
        to_dir: &Dir,
        to: &str,
        replace: bool,
        expected: Identity,
    ) -> io::Result<()> {
        self.rename_checked(from, to_dir, to, replace, Some(expected))
    }

    fn rename_checked(
        &self,
        from: &str,
        to_dir: &Dir,
        to: &str,
        replace: bool,
        expected: Option<Identity>,
    ) -> io::Result<()> {
        let target = component(to)?;
        let handle = self.nt_open(
            from,
            DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_OPEN,
            0,
        )?;
        if tag_info(handle.as_handle())?.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(reparse_refused());
        }
        expect_identity(&handle, expected)?;
        let mut flags = FILE_RENAME_POSIX_SEMANTICS;
        if replace {
            flags |= FILE_RENAME_REPLACE_IF_EXISTS;
        }
        let root = to_dir.handle.as_raw_handle() as HANDLE;
        match set_rename(&handle, FileRenameInformationEx, flags, root, &target) {
            Err(e) if is_unsupported_class(&e) => set_rename(
                &handle,
                FileRenameInformation,
                u32::from(replace),
                root,
                &target,
            ),
            other => other,
        }
    }

    /// Names and kinds of the entries in this directory, without `.` and
    /// `..`. A name that is not valid UTF-16 is returned lossily with
    /// [`Kind::Other`] so callers can skip or report it.
    pub fn list(&self) -> io::Result<Vec<(String, Kind)>> {
        // 64 KiB, 8-byte aligned.
        let mut buf = vec![0u64; 8192];
        let bytes = buf.len() * size_of::<u64>();
        let mut out = Vec::new();
        let mut class = FileIdBothDirectoryRestartInfo;
        loop {
            // SAFETY: `buf` is a writable, 8-byte aligned buffer of `bytes`
            // bytes; the handle is a valid synchronous directory handle.
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    self.raw(),
                    class,
                    buf.as_mut_ptr().cast(),
                    u32::try_from(bytes).unwrap_or(u32::MAX),
                )
            };
            if ok == 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                    return Ok(out);
                }
                return Err(e);
            }
            class = FileIdBothDirectoryInfo;
            parse_dir_entries(&buf, bytes, &mut out)?;
        }
    }

    /// The normalized final path of this directory as reported by the
    /// system (with a `\\?\` prefix). Diagnostic only.
    pub fn final_path(&self) -> io::Result<PathBuf> {
        let mut len = 512usize;
        loop {
            let mut buf = vec![0u16; len];
            // SAFETY: `buf` has room for `len` UTF-16 units.
            let got = unsafe {
                GetFinalPathNameByHandleW(
                    self.raw(),
                    buf.as_mut_ptr(),
                    u32::try_from(len).unwrap_or(u32::MAX),
                    0,
                )
            } as usize;
            if got == 0 {
                return Err(io::Error::last_os_error());
            }
            if got < len {
                buf.truncate(got);
                return Ok(PathBuf::from(OsString::from_wide(&buf)));
            }
            len = got + 1;
        }
    }

    /// A second handle to the same directory.
    pub fn try_clone(&self) -> io::Result<Dir> {
        Ok(Dir {
            handle: self.handle.try_clone()?,
        })
    }

    /// Directory barrier. Windows has no directory flush for namespace
    /// changes, so this returns `Ok(())` without doing anything.
    pub fn flush(&self) -> io::Result<()> {
        Ok(())
    }

    /// The on-disk name of the existing entry `name`, which differs from
    /// `name` when `name` is a short (8.3) alias or differs in case. The
    /// entry is not followed if it is a reparse point.
    pub fn long_name(&self, name: &str) -> io::Result<String> {
        let handle = self.nt_open(name, FILE_READ_ATTRIBUTES | SYNCHRONIZE, FILE_OPEN, 0)?;
        let full = match name_info(handle.as_handle(), FileNormalizedNameInfo) {
            Err(e) if is_unsupported_class(&e) => name_info(handle.as_handle(), FileNameInfo)?,
            other => other?,
        };
        let last = match full.iter().rposition(|&c| c == u16::from(b'\\')) {
            Some(i) => &full[i + 1..],
            None => &full[..],
        };
        String::from_utf16(last).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "entry name is not valid UTF-16")
        })
    }

    /// A create can fail with access denied instead of a name collision
    /// when the existing entry is of the other kind or a reparse point;
    /// report that as `AlreadyExists` so "never overwrite" is one error.
    fn collision(&self, name: &str, e: io::Error) -> io::Error {
        if e.kind() == io::ErrorKind::PermissionDenied && matches!(self.stat(name), Ok(Some(_))) {
            return io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("an entry named {name:?} already exists"),
            );
        }
        e
    }

    fn raw(&self) -> HANDLE {
        self.handle.as_raw_handle() as HANDLE
    }

    /// Open or create the single component `name` relative to this
    /// directory without following a reparse point.
    fn nt_open(
        &self,
        name: &str,
        access: u32,
        disposition: u32,
        options: u32,
    ) -> io::Result<OwnedHandle> {
        let wide = component(name)?;
        let byte_len = u16::try_from(wide.len() * 2)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name is too long"))?;
        let unicode = UNICODE_STRING {
            Length: byte_len,
            MaximumLength: byte_len,
            Buffer: wide.as_ptr().cast_mut(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: self.raw(),
            ObjectName: &unicode,
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: std::ptr::null(),
            SecurityQualityOfService: std::ptr::null(),
        };
        let mut status_block = IO_STATUS_BLOCK::default();
        let mut raw: HANDLE = std::ptr::null_mut();
        let attrs = if options & FILE_DIRECTORY_FILE != 0 {
            0
        } else {
            FILE_ATTRIBUTE_NORMAL
        };
        // SAFETY: every pointer refers to a live local value: `unicode`
        // borrows `wide` (not NUL-terminated, length given in bytes),
        // `attributes` borrows `unicode` and our directory handle, and
        // `raw`/`status_block` are writable outputs. No allocation size or
        // extended attributes are passed.
        let status = unsafe {
            NtCreateFile(
                &mut raw,
                access | SYNCHRONIZE,
                &attributes,
                &mut status_block,
                std::ptr::null(),
                attrs,
                SHARE_ALL,
                disposition,
                options | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
                std::ptr::null(),
                0,
            )
        };
        if status < 0 {
            return Err(nt_error(status));
        }
        // SAFETY: NtCreateFile succeeded, so `raw` is a new handle that
        // nothing else owns.
        Ok(unsafe { OwnedHandle::from_raw_handle(raw as _) })
    }
}

impl AsHandle for Dir {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.handle.as_handle()
    }
}

/// Metadata of an open file, from the same handle (no path lookup).
pub fn stamp(file: &File) -> io::Result<Stat> {
    stamp_handle(file.as_handle())
}

/// Flush the file's data and metadata to the device (`FlushFileBuffers`).
/// The handle must have write access.
pub fn flush_file(file: &File) -> io::Result<()> {
    // SAFETY: the handle is valid for the lifetime of `file`.
    if unsafe { FlushFileBuffers(file.as_raw_handle() as HANDLE) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Best-effort flush of the file at `path`: opens it for writing without
/// truncation and flushes it. Returns `Ok(false)` without flushing when
/// the file is read-only (the read-only attribute denies write access) or
/// is a directory, `Ok(true)` after a flush.
pub fn flush_path(path: &Path) -> io::Result<bool> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() || meta.permissions().readonly() {
        return Ok(false);
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .share_mode(SHARE_ALL)
        .open(path);
    match file {
        Ok(file) => flush_file(&file).map(|()| true),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            let readonly = std::fs::metadata(path)
                .map(|m| m.permissions().readonly())
                .unwrap_or(false);
            if readonly { Ok(false) } else { Err(e) }
        }
        Err(e) => Err(e),
    }
}

/// The extended (`\\?\`) form of an absolute drive or UNC path, so that
/// CreateFileW accepts it beyond MAX_PATH. Paths that are already verbatim
/// or device paths, relative, or contain `..` are returned unchanged.
fn extended_path(path: &Path) -> OsString {
    use std::path::{Component, Prefix};
    let plain = || path.as_os_str().to_owned();
    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return plain();
    };
    let mut out = match prefix.kind() {
        Prefix::Disk(letter) => OsString::from(format!(r"\\?\{}:", char::from(letter))),
        Prefix::UNC(server, share) => {
            let mut out = OsString::from(r"\\?\UNC\");
            out.push(server);
            out.push(r"\");
            out.push(share);
            out
        }
        _ => return plain(),
    };
    if components.next() != Some(Component::RootDir) {
        return plain();
    }
    let mut named = false;
    for component in components {
        match component {
            Component::Normal(name) => {
                out.push(r"\");
                out.push(name);
                named = true;
            }
            Component::CurDir => {}
            _ => return plain(),
        }
    }
    if !named {
        // `\\?\C:` names the volume device; the root directory needs the
        // trailing separator.
        out.push(r"\");
    }
    out
}

/// Validate a single component and encode it as UTF-16 (no terminator).
fn component(name: &str) -> io::Result<Vec<u16>> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['\\', '/', ':', '\0']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a single path component: {name:?}"),
        ));
    }
    Ok(name.encode_utf16().collect())
}

fn nt_error(status: i32) -> io::Error {
    if status == STATUS_DELETE_PENDING {
        return io::Error::from_raw_os_error(ERROR_DELETE_PENDING as i32);
    }
    // SAFETY: RtlNtStatusToDosError only maps a status value.
    let code = unsafe { RtlNtStatusToDosError(status) };
    io::Error::from_raw_os_error(code as i32)
}

fn is_unsupported_class(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error().and_then(|c| u32::try_from(c).ok()),
        Some(ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED | ERROR_INVALID_FUNCTION)
    )
}

/// Query a fixed-size information class into `T`.
///
/// # Safety
/// `class` must be an information class whose output is exactly a `T`
/// (a plain C struct for which any bit pattern is valid).
unsafe fn query<T>(handle: BorrowedHandle<'_>, class: FILE_INFO_BY_HANDLE_CLASS) -> io::Result<T> {
    let mut out = MaybeUninit::<T>::zeroed();
    // SAFETY: `out` is writable for `size_of::<T>()` bytes and the handle
    // is valid while borrowed.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle.as_raw_handle() as HANDLE,
            class,
            out.as_mut_ptr().cast(),
            size_of::<T>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call succeeded and filled the struct; per the caller's
    // contract every bit pattern is a valid `T`.
    Ok(unsafe { out.assume_init() })
}

fn tag_info(handle: BorrowedHandle<'_>) -> io::Result<FILE_ATTRIBUTE_TAG_INFO> {
    // SAFETY: FileAttributeTagInfo returns a FILE_ATTRIBUTE_TAG_INFO.
    unsafe { query(handle, FileAttributeTagInfo) }
}

fn expect_dir(handle: &OwnedHandle) -> io::Result<()> {
    let attrs = tag_info(handle.as_handle())?.FileAttributes;
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(reparse_refused());
    }
    if attrs & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "not a directory",
        ));
    }
    Ok(())
}

fn expect_file(handle: &OwnedHandle) -> io::Result<()> {
    let attrs = tag_info(handle.as_handle())?.FileAttributes;
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(reparse_refused());
    }
    if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Err(io::Error::new(
            io::ErrorKind::IsADirectory,
            "is a directory",
        ));
    }
    Ok(())
}

fn stamp_handle(handle: BorrowedHandle<'_>) -> io::Result<Stat> {
    // SAFETY: FileBasicInfo returns a FILE_BASIC_INFO.
    let basic: FILE_BASIC_INFO = unsafe { query(handle, FileBasicInfo) }?;
    // SAFETY: FileStandardInfo returns a FILE_STANDARD_INFO.
    let standard: FILE_STANDARD_INFO = unsafe { query(handle, FileStandardInfo) }?;
    // SAFETY: FileIdInfo returns a FILE_ID_INFO.
    let (volume_serial, file_id) = match unsafe { query::<FILE_ID_INFO>(handle, FileIdInfo) } {
        Ok(id) => (
            id.VolumeSerialNumber,
            u128::from_le_bytes(id.FileId.Identifier),
        ),
        Err(e) if is_unsupported_class(&e) => legacy_id(handle)?,
        Err(e) => return Err(e),
    };
    let attrs = basic.FileAttributes;
    let kind = if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        Kind::Reparse
    } else if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 || standard.Directory {
        Kind::Dir
    } else {
        Kind::File
    };
    Ok(Stat {
        kind,
        size: u64::try_from(standard.EndOfFile).unwrap_or(0),
        last_write: basic.LastWriteTime,
        change_time: basic.ChangeTime,
        volume_serial,
        file_id,
        readonly: attrs & FILE_ATTRIBUTE_READONLY != 0,
    })
}

/// (volume serial, file index) from the older query, for file systems
/// without `FileIdInfo`.
fn legacy_id(handle: BorrowedHandle<'_>) -> io::Result<(u64, u128)> {
    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: `info` is writable and the handle is valid while borrowed.
    if unsafe { GetFileInformationByHandle(handle.as_raw_handle() as HANDLE, info.as_mut_ptr()) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call succeeded and filled the plain C struct.
    let info = unsafe { info.assume_init() };
    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    Ok((u64::from(info.dwVolumeSerialNumber), u128::from(index)))
}

fn set_info<T>(
    handle: &OwnedHandle,
    class: FILE_INFO_BY_HANDLE_CLASS,
    value: &T,
) -> io::Result<()> {
    // SAFETY: `value` is readable for `size_of::<T>()` bytes and callers
    // pass the struct that `class` expects.
    let ok = unsafe {
        SetFileInformationByHandle(
            handle.as_raw_handle() as HANDLE,
            class,
            (value as *const T).cast(),
            size_of::<T>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Mark the open entry for deletion, preferring POSIX semantics.
fn delete_handle(handle: &OwnedHandle, ignore_readonly: bool) -> io::Result<()> {
    let posix = FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS;
    if ignore_readonly {
        let flags = posix | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE;
        match set_info(
            handle,
            FileDispositionInfoEx,
            &FILE_DISPOSITION_INFO_EX { Flags: flags },
        ) {
            Err(e) if is_unsupported_class(&e) => {}
            other => return other,
        }
    }
    match set_info(
        handle,
        FileDispositionInfoEx,
        &FILE_DISPOSITION_INFO_EX { Flags: posix },
    ) {
        Err(e) if is_unsupported_class(&e) => set_info(
            handle,
            FileDispositionInfo,
            &FILE_DISPOSITION_INFO { DeleteFile: true },
        ),
        other => other,
    }
}

/// Issue a rename with a `FILE_RENAME_INFO` whose first field is `flags`
/// (for `FileRenameInformationEx`) or the `ReplaceIfExists` boolean (for
/// `FileRenameInformation`, passed as 0 or 1). Calls `NtSetInformationFile`
/// directly: the Win32 wrapper rejects a relative target name combined
/// with `RootDirectory`.
fn set_rename(
    handle: &OwnedHandle,
    class: FILE_INFORMATION_CLASS,
    flags: u32,
    root: HANDLE,
    name: &[u16],
) -> io::Result<()> {
    let name_offset = offset_of!(FILE_RENAME_INFO, FileName);
    let name_bytes = std::mem::size_of_val(name);
    // Header, name and a terminating NUL, rounded up to whole u64 words so
    // the buffer is aligned for FILE_RENAME_INFO.
    let total = name_offset + name_bytes + size_of::<u16>();
    let total = total.max(size_of::<FILE_RENAME_INFO>());
    let mut buf = vec![0u64; total.div_ceil(size_of::<u64>())];
    let info = buf.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    let name_len = u32::try_from(name_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name is too long"))?;
    // SAFETY: `buf` is zeroed, 8-byte aligned and at least
    // `size_of::<FILE_RENAME_INFO>()` bytes, so writing the header fields
    // through `info` is in bounds. The name is copied to `name_offset`,
    // and `name_offset + name_bytes + 2 <= total <= buf` bytes.
    unsafe {
        if class == FileRenameInformation {
            (*info).Anonymous.ReplaceIfExists = flags != 0;
        } else {
            (*info).Anonymous.Flags = flags;
        }
        (*info).RootDirectory = root;
        (*info).FileNameLength = name_len;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            buf.as_mut_ptr().cast::<u8>().add(name_offset).cast::<u16>(),
            name.len(),
        );
    }
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: the buffer holds a complete FILE_RENAME_INFORMATION of
    // `total` bytes and outlives the call; `root` is a valid directory
    // handle borrowed from the caller; `status_block` is writable.
    let status = unsafe {
        NtSetInformationFile(
            handle.as_raw_handle() as HANDLE,
            &mut status_block,
            buf.as_ptr().cast(),
            u32::try_from(total).unwrap_or(u32::MAX),
            class,
        )
    };
    if status < 0 {
        return Err(nt_error(status));
    }
    Ok(())
}

/// Read the name information class `class` (a `FILE_NAME_INFO`) of an
/// open handle as UTF-16 units.
fn name_info(handle: BorrowedHandle<'_>, class: FILE_INFO_BY_HANDLE_CLASS) -> io::Result<Vec<u16>> {
    let header = offset_of!(FILE_NAME_INFO, FileName);
    let mut words = 128usize;
    loop {
        let mut buf = vec![0u64; words];
        let bytes = words * size_of::<u64>();
        // SAFETY: `buf` is writable, 8-byte aligned and `bytes` long.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle.as_raw_handle() as HANDLE,
                class,
                buf.as_mut_ptr().cast(),
                u32::try_from(bytes).unwrap_or(u32::MAX),
            )
        };
        if ok == 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(ERROR_MORE_DATA as i32) && words < 16384 {
                words *= 4;
                continue;
            }
            return Err(e);
        }
        let base = buf.as_ptr().cast::<u8>();
        // SAFETY: the header lies within `buf` (bytes >= 1024 > header).
        let len = unsafe {
            std::ptr::read_unaligned(
                base.add(offset_of!(FILE_NAME_INFO, FileNameLength))
                    .cast::<u32>(),
            )
        } as usize;
        if header + len > bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file name information overflows its buffer",
            ));
        }
        let mut name = vec![0u16; len / 2];
        // SAFETY: `header + len <= bytes`, so the copied range is inside
        // `buf`; byte-wise copy has no alignment requirement.
        unsafe {
            std::ptr::copy_nonoverlapping(
                base.add(header),
                name.as_mut_ptr().cast::<u8>(),
                name.len() * 2,
            );
        }
        return Ok(name);
    }
}

/// Append the entries of one `FILE_ID_BOTH_DIR_INFO` chain in `buf`.
fn parse_dir_entries(buf: &[u64], bytes: usize, out: &mut Vec<(String, Kind)>) -> io::Result<()> {
    let base = buf.as_ptr().cast::<u8>();
    let name_at = offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
    let corrupt = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "directory listing overflows its buffer",
        )
    };
    let mut at = 0usize;
    loop {
        if at + name_at > bytes {
            return Err(corrupt());
        }
        // SAFETY: `at + name_at <= bytes`, so the fixed header fields read
        // below lie inside `buf`; unaligned reads need no alignment.
        let (next, attrs, len) = unsafe {
            let entry = base.add(at);
            (
                std::ptr::read_unaligned(
                    entry
                        .add(offset_of!(FILE_ID_BOTH_DIR_INFO, NextEntryOffset))
                        .cast::<u32>(),
                ) as usize,
                std::ptr::read_unaligned(
                    entry
                        .add(offset_of!(FILE_ID_BOTH_DIR_INFO, FileAttributes))
                        .cast::<u32>(),
                ),
                std::ptr::read_unaligned(
                    entry
                        .add(offset_of!(FILE_ID_BOTH_DIR_INFO, FileNameLength))
                        .cast::<u32>(),
                ) as usize,
            )
        };
        if at + name_at + len > bytes {
            return Err(corrupt());
        }
        let mut name = vec![0u16; len / 2];
        // SAFETY: `at + name_at + len <= bytes`; byte-wise copy into a
        // buffer of `len / 2` u16 units.
        unsafe {
            std::ptr::copy_nonoverlapping(
                base.add(at + name_at),
                name.as_mut_ptr().cast::<u8>(),
                name.len() * 2,
            );
        }
        let dot = [u16::from(b'.')];
        let dotdot = [u16::from(b'.'), u16::from(b'.')];
        if name != dot && name != dotdot {
            let kind = if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                Kind::Reparse
            } else if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
                Kind::Dir
            } else {
                Kind::File
            };
            match String::from_utf16(&name) {
                Ok(s) => out.push((s, kind)),
                Err(_) => out.push((String::from_utf16_lossy(&name), Kind::Other)),
            }
        }
        if next == 0 {
            return Ok(());
        }
        at += next;
    }
}

#[cfg(test)]
mod tests {
    use super::extended_path;
    use std::ffi::OsString;
    use std::path::Path;

    #[test]
    fn extended_path_prefixes_absolute_drive_and_unc_paths() {
        let cases = [
            (r"C:\vault\notes", r"\\?\C:\vault\notes"),
            (r"C:/vault/notes", r"\\?\C:\vault\notes"),
            (r"C:\", r"\\?\C:\"),
            (r"\\server\share\vault", r"\\?\UNC\server\share\vault"),
            (r"\\?\C:\vault", r"\\?\C:\vault"),
            (r"C:vault", r"C:vault"),
            (r"vault\notes", r"vault\notes"),
            (r"C:\vault\..\other", r"C:\vault\..\other"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                extended_path(Path::new(input)),
                OsString::from(expected),
                "{input}"
            );
        }
    }
}
