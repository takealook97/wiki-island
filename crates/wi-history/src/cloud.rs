//! Best-effort warning for vaults inside cloud-synced folders.

use std::path::{Path, PathBuf};

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

/// The sync service whose folder `path` appears to be in, if any. A path
/// that does not exist yet is judged by its nearest existing ancestor with
/// symlinks resolved, so a new vault reached through an alias of a synced
/// folder is still detected.
pub fn cloud_service(path: &Path) -> Option<&'static str> {
    let resolved = resolve_existing(path);
    let text = format!("{}/", resolved.to_string_lossy().replace('\\', "/"));
    CLOUD_MARKERS
        .iter()
        .find(|(marker, _)| text.contains(marker))
        .map(|(_, service)| *service)
}

/// `path` with its nearest existing ancestor canonicalized and the missing
/// components applied lexically (`.` dropped, `..` removes the previous
/// component), so `alias/missing/../vault` resolves through `alias`;
/// `path` itself when no ancestor resolves. Every component after the
/// nearest existing ancestor is missing, so lexical `..` is exact there.
fn resolve_existing(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut missing = Vec::new();
    let mut current = path;
    loop {
        if let Ok(resolved) = current.canonicalize() {
            let mut out = resolved;
            let mut popped = false;
            for part in missing.iter().rev() {
                match part {
                    Component::CurDir => {}
                    Component::ParentDir => {
                        out.pop();
                        popped = true;
                    }
                    other => out.push(other.as_os_str()),
                }
            }
            // A `..` can climb back to an existing ancestor whose children
            // (perhaps symlinks) were never resolved; resolve the lexical
            // result once more. It holds no `..`, so this ends.
            return if popped { resolve_existing(&out) } else { out };
        }
        match (current.parent(), current.components().next_back()) {
            (Some(parent), Some(last)) => {
                missing.push(last);
                current = if parent.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    parent
                };
            }
            _ => return path.to_path_buf(),
        }
    }
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

    #[test]
    fn a_missing_component_followed_by_dotdot_still_resolves_through_an_alias() {
        let tmp = std::env::temp_dir().join(format!("wi-cloud-dotdot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("Dropbox")).unwrap();
        std::os::unix::fs::symlink(tmp.join("Dropbox"), tmp.join("alias-one")).unwrap();
        std::os::unix::fs::symlink(tmp.join("alias-one"), tmp.join("alias-two")).unwrap();
        let path = tmp.join("alias-two/missing/./deeper/../../vault");
        assert_eq!(cloud_service(&path), Some("Dropbox"));
        assert_eq!(
            resolve_existing(&path),
            tmp.join("Dropbox").canonicalize().unwrap().join("vault")
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn dotdot_back_to_an_existing_folder_resolves_its_symlinked_child() {
        let tmp = std::env::temp_dir().join(format!("wi-cloud-hop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("Dropbox")).unwrap();
        std::fs::create_dir_all(tmp.join("plain")).unwrap();
        std::os::unix::fs::symlink(tmp.join("Dropbox"), tmp.join("plain/hop")).unwrap();
        let path = tmp.join("plain/missing/../hop/vault");
        assert_eq!(cloud_service(&path), Some("Dropbox"));
        assert_eq!(
            resolve_existing(&path),
            tmp.join("Dropbox").canonicalize().unwrap().join("vault")
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
