//! Windows platform layer for wiki-island.
//!
//! This is the only crate in the workspace that may contain `unsafe` code.
//! Containment, durability, process and credential guarantees on Windows
//! need Win32 and NT calls that the standard library does not expose
//! (handle-relative opens, POSIX-semantics rename and delete, job objects,
//! Credential Manager, console modes). Keeping every foreign call here lets
//! every other crate keep `#![forbid(unsafe_code)]`. Each `unsafe` block
//! states why it is sound, and every owned handle is an `OwnedHandle`.
//!
//! On other platforms the crate is empty.
#![cfg_attr(not(windows), forbid(unsafe_code))]
#![cfg_attr(
    windows,
    deny(unsafe_op_in_unsafe_fn, clippy::undocumented_unsafe_blocks)
)]

#[cfg(windows)]
pub mod console;
#[cfg(windows)]
pub mod credential;
#[cfg(windows)]
pub mod fs;
#[cfg(windows)]
pub mod process;

/// Encode `s` as a NUL-terminated UTF-16 string, refusing interior NULs.
#[cfg(windows)]
fn wide_nul(s: &std::ffi::OsStr) -> std::io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    let mut wide: Vec<u16> = s.encode_wide().collect();
    if wide.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "string contains a NUL character",
        ));
    }
    wide.push(0);
    Ok(wide)
}
