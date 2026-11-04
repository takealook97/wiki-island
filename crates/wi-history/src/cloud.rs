//! Best-effort warning for vaults inside cloud-synced folders.

use std::path::Path;

/// Path fragments of common sync clients. Matching is heuristic: a synced
/// folder can live anywhere, so absence of a warning proves nothing.
const CLOUD_MARKERS: &[(&str, &str)] = &[
    ("/Library/Mobile Documents/", "iCloud Drive"),
    ("/iCloud Drive/", "iCloud Drive"),
    ("/iCloudDrive/", "iCloud Drive"),
    (
        "/Library/CloudStorage/",
        "a macOS File Provider sync client",
    ),
    ("/Google Drive/", "Google Drive"),
    ("/GoogleDrive/", "Google Drive"),
    ("/My Drive/", "Google Drive"),
    ("/Dropbox/", "Dropbox"),
    ("/OneDrive", "OneDrive"),
    ("/Box/", "Box"),
    ("/pCloud Drive/", "pCloud"),
];

/// Return a warning when `path` looks like it is inside a cloud-synced folder.
///
/// A synced `.git` can be corrupted by partial uploads and conflict copies
/// (`docs/DESIGN.md#storage-and-history`). This cannot be detected reliably.
pub fn cloud_sync_warning(path: &Path) -> Option<String> {
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let text = format!("{}/", resolved.to_string_lossy().replace('\\', "/"));
    CLOUD_MARKERS
        .iter()
        .find(|(marker, _)| text.contains(marker))
        .map(|(_, service)| {
            format!(
                "{} appears to be inside {service}; keep the vault and its .git outside cloud-synced folders and sync through a git remote",
                resolved.display()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warns_for_known_sync_folders() {
        let icloud = Path::new("/Users/x/Library/Mobile Documents/com~apple~CloudDocs/vault");
        assert!(cloud_sync_warning(icloud).unwrap().contains("iCloud"));
        let drive = Path::new("/Users/x/Library/CloudStorage/GoogleDrive-a/My Drive/vault");
        assert!(cloud_sync_warning(drive).is_some());
        assert!(cloud_sync_warning(Path::new("/Users/x/Dropbox/vault")).is_some());
        assert!(cloud_sync_warning(Path::new("/Users/x/Projects/vault")).is_none());
    }
}
