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

/// The sync service whose folder `path` appears to be in, if any.
pub fn cloud_service(path: &Path) -> Option<&'static str> {
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let text = format!("{}/", resolved.to_string_lossy().replace('\\', "/"));
    CLOUD_MARKERS
        .iter()
        .find(|(marker, _)| text.contains(marker))
        .map(|(_, service)| *service)
}

/// Return a warning when the git directory `git_dir` looks like it is inside
/// a cloud-synced folder.
///
/// A synced git directory can be corrupted by partial uploads and conflict
/// copies (`docs/DESIGN.md#storage-and-history`). Notes in a synced folder
/// are fine: folder mode keeps the git directory in the per-device data
/// directory. Detection is heuristic.
pub fn cloud_sync_warning(git_dir: &Path) -> Option<String> {
    cloud_service(git_dir).map(|service| {
        let resolved = git_dir.canonicalize().unwrap_or_else(|_| git_dir.to_path_buf());
        format!(
            "the history folder {} appears to be inside {service}; a synced git folder can be corrupted",
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
        assert_eq!(cloud_service(icloud), Some("iCloud Drive"));
        let drive = Path::new("/Users/x/Library/CloudStorage/GoogleDrive-a/My Drive/vault");
        assert!(cloud_sync_warning(drive).is_some());
        assert!(cloud_sync_warning(Path::new("/Users/x/Dropbox/vault")).is_some());
        assert!(cloud_sync_warning(Path::new("/Users/x/Projects/vault")).is_none());
    }
}
