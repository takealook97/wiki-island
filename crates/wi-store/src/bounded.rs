//! Bounded reads of an already opened, checked regular file.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::error::{Result, StoreError};

/// Read `file` whole when it holds at most `max_bytes`. The size comes from
/// the open handle, so a larger file is refused without reading it, and the
/// read itself stops after `max_bytes + 1` bytes in case the file grew.
pub(crate) fn read_capped(file: File, max_bytes: u64, path: &Path) -> Result<Vec<u8>> {
    let too_large = |size| StoreError::TooLarge {
        path: path.display().to_string(),
        size,
        limit: max_bytes,
    };
    let size = file.metadata().map_err(|e| StoreError::io(path, e))?.len();
    if size > max_bytes {
        return Err(too_large(size));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| StoreError::io(path, e))?;
    let read = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if read > max_bytes {
        return Err(too_large(read));
    }
    Ok(bytes)
}

/// Name a `TooLarge` refusal by its vault-relative path.
pub(crate) fn name_rel(error: StoreError, rel: &str) -> StoreError {
    match error {
        StoreError::TooLarge { size, limit, .. } => StoreError::TooLarge {
            path: rel.to_owned(),
            size,
            limit,
        },
        other => other,
    }
}
