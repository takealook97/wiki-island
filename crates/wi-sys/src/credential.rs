//! Generic secrets in Windows Credential Manager.
//!
//! Secrets are stored as `CRED_TYPE_GENERIC` credentials persisted for the
//! local machine (this user on this computer, not roamed), with the secret
//! as a UTF-8 blob.

use std::io;

use windows_sys::Win32::Foundation::ERROR_NOT_FOUND;
use windows_sys::Win32::Security::Credentials::{
    CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree, CredReadW,
    CredWriteW,
};

/// Largest blob Credential Manager accepts (`CRED_MAX_CREDENTIAL_BLOB_SIZE`).
const MAX_BLOB: usize = 5 * 512;
/// Longest generic target name (`CRED_MAX_GENERIC_TARGET_NAME_LENGTH`).
const MAX_TARGET: usize = 32767;

/// Frees a credential returned by `CredReadW` on every path.
struct Freed(*mut CREDENTIALW);

impl Drop for Freed {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a successful CredReadW and is freed
        // exactly once.
        unsafe { CredFree(self.0.cast()) };
    }
}

fn target_wide(target: &str) -> io::Result<Vec<u16>> {
    if target.is_empty() || target.encode_utf16().count() > MAX_TARGET {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "credential target is empty or too long",
        ));
    }
    crate::wide_nul(target.as_ref())
}

/// The secret stored under `target`, or `None` when there is none.
pub fn read(target: &str) -> io::Result<Option<String>> {
    let wide = target_wide(target)?;
    let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated and `cred` is a writable output.
    if unsafe { CredReadW(wide.as_ptr(), CRED_TYPE_GENERIC, 0, &mut cred) } == 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(ERROR_NOT_FOUND as i32) {
            return Ok(None);
        }
        return Err(e);
    }
    let cred = Freed(cred);
    // SAFETY: CredReadW succeeded, so the pointer refers to a valid
    // CREDENTIALW until CredFree.
    let (size, blob) = unsafe {
        (
            (*cred.0).CredentialBlobSize as usize,
            (*cred.0).CredentialBlob,
        )
    };
    if size > MAX_BLOB || (size > 0 && blob.is_null()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "stored credential has an invalid size",
        ));
    }
    let bytes = if size == 0 {
        Vec::new()
    } else {
        // SAFETY: the blob is non-null and holds `size` bytes, valid until
        // CredFree.
        unsafe { std::slice::from_raw_parts(blob, size) }.to_vec()
    };
    drop(cred);
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "stored credential is not UTF-8"))
}

/// Store `secret` under `target`, replacing any existing secret.
pub fn write(target: &str, secret: &str) -> io::Result<()> {
    let mut wide = target_wide(target)?;
    if secret.len() > MAX_BLOB {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "secret is longer than Credential Manager allows",
        ));
    }
    let mut blob = secret.as_bytes().to_vec();
    let cred = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: wide.as_mut_ptr(),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: if blob.is_empty() {
            std::ptr::null_mut()
        } else {
            blob.as_mut_ptr()
        },
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        ..CREDENTIALW::default()
    };
    // SAFETY: `cred` points at `wide` (NUL-terminated) and `blob`, both of
    // which outlive the call; all other pointers are null.
    let ok = unsafe { CredWriteW(&cred, 0) };
    blob.fill(0);
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Delete the secret stored under `target`; `false` when there was none.
pub fn delete(target: &str) -> io::Result<bool> {
    let wide = target_wide(target)?;
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    if unsafe { CredDeleteW(wide.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(ERROR_NOT_FOUND as i32) {
            return Ok(false);
        }
        return Err(e);
    }
    Ok(true)
}
