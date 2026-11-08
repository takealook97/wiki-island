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

/// The sync service whose folder `path` appears to be in, if any. `path` is
/// resolved physically first ([`resolve_path`]), so a new vault reached
/// through an alias of a synced folder, or through `..` after a symlink, is
/// still detected.
pub fn cloud_service(path: &Path) -> Option<&'static str> {
    let resolved = resolve_path(path);
    let text = format!("{}/", resolved.to_string_lossy().replace('\\', "/"));
    CLOUD_MARKERS
        .iter()
        .find(|(marker, _)| text.contains(marker))
        .map(|(_, service)| *service)
}

fn is_alias(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// Symlink hops followed by [`resolve_path`] before it gives up and treats
/// the rest of the path lexically (the usual `MAXSYMLINKS`).
const MAX_SYMLINK_HOPS: usize = 40;

/// `path` resolved physically, like `realpath`: components are walked left
/// to right against a resolved prefix. `.` is skipped; `..` takes the parent
/// of the resolved prefix (so `link/..` is the parent of the link target).
/// A name that is a symlink, dangling or not, is replaced by its target's
/// components (an absolute target restarts from its root; a relative one
/// continues from the resolved prefix); after [`MAX_SYMLINK_HOPS`] hops the
/// remainder is treated lexically. A plain name that exists is joined. Once
/// a name does not exist it and the rest form a missing tail in which `..`
/// pops lexically; popping the whole tail resumes physical resolution. A
/// relative `path` starts from the canonical working directory.
pub fn resolve_path(path: &Path) -> PathBuf {
    use std::collections::VecDeque;
    use std::ffi::OsString;
    use std::path::Component;

    enum Part {
        Root(OsString),
        Parent,
        Name(OsString),
    }
    fn parts(path: &Path) -> impl Iterator<Item = Part> + '_ {
        path.components().filter_map(|component| match component {
            Component::Prefix(_) | Component::RootDir => {
                Some(Part::Root(component.as_os_str().to_os_string()))
            }
            Component::CurDir => None,
            Component::ParentDir => Some(Part::Parent),
            Component::Normal(name) => Some(Part::Name(name.to_os_string())),
        })
    }

    let mut resolved = PathBuf::new();
    if !path.has_root() {
        match std::env::current_dir().and_then(|d| d.canonicalize()) {
            Ok(cwd) => resolved = cwd,
            Err(_) => return path.to_path_buf(),
        }
    }
    let mut tail: Vec<OsString> = Vec::new();
    let mut pending: VecDeque<Part> = parts(path).collect();
    let mut hops = 0;
    let mut lexical = false;
    while let Some(part) = pending.pop_front() {
        match part {
            Part::Root(root) => resolved.push(root),
            Part::Parent => {
                if tail.pop().is_none() {
                    resolved.pop();
                }
            }
            Part::Name(name) => {
                if lexical || !tail.is_empty() {
                    tail.push(name);
                    continue;
                }
                let joined = resolved.join(&name);
                match std::fs::symlink_metadata(&joined) {
                    Ok(meta) if is_alias(&meta) => {
                        let target = match std::fs::read_link(&joined) {
                            Ok(target) if hops < MAX_SYMLINK_HOPS => target,
                            _ => {
                                lexical = true;
                                tail.push(name);
                                continue;
                            }
                        };
                        hops += 1;
                        if target.has_root() {
                            resolved = PathBuf::new();
                        }
                        let spliced: Vec<Part> = parts(&target).collect();
                        for part in spliced.into_iter().rev() {
                            pending.push_front(part);
                        }
                    }
                    Ok(_) => resolved = joined,
                    Err(_) => tail.push(name),
                }
            }
        }
    }
    resolved.extend(tail);
    resolved
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

    #[cfg(windows)]
    fn junction(target: impl AsRef<Path>, link: impl AsRef<Path>) -> std::io::Result<()> {
        let output = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link.as_ref())
            .arg(target.as_ref())
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_roots_and_unc_paths_remain_absolute() {
        for root in [r"C:\", r"\\server\share\"] {
            assert_eq!(resolve_path(Path::new(root)), PathBuf::from(root));
            assert_eq!(
                resolve_path(&PathBuf::from(root).join("..")),
                PathBuf::from(root)
            );
        }
        assert_eq!(
            cloud_service(Path::new(r"C:\Users\fixture\Dropbox\vault")),
            Some("Dropbox")
        );
    }

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

    #[cfg(unix)]
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
            resolve_path(&path),
            tmp.join("Dropbox").canonicalize().unwrap().join("vault")
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[cfg(unix)]
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
            resolve_path(&path),
            tmp.join("Dropbox").canonicalize().unwrap().join("vault")
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn missing_parent_then_symlink_then_parent_still_detects_cloud() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("Dropbox/child")).unwrap();
        std::fs::create_dir_all(t.path().join("plain")).unwrap();
        std::os::unix::fs::symlink(t.path().join("Dropbox/child"), t.path().join("plain/hop"))
            .unwrap();
        let path = t.path().join("plain/missing/../hop/../vault");
        assert_eq!(cloud_service(&path), Some("Dropbox"), "{}", path.display());
        assert_eq!(
            resolve_path(&path),
            t.path()
                .join("Dropbox")
                .canonicalize()
                .unwrap()
                .join("vault")
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_missing_component_followed_by_dotdot_still_resolves_through_an_alias() {
        let tmp = std::env::temp_dir().join(format!("wi-cloud-dotdot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("Dropbox")).unwrap();
        junction(tmp.join("Dropbox"), tmp.join("alias-one")).unwrap();
        junction(tmp.join("alias-one"), tmp.join("alias-two")).unwrap();
        let path = tmp.join("alias-two/missing/./deeper/../../vault");
        assert_eq!(cloud_service(&path), Some("Dropbox"));
        assert_eq!(
            resolve_path(&path),
            tmp.join("Dropbox").canonicalize().unwrap().join("vault")
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn dotdot_back_to_an_existing_folder_resolves_its_symlinked_child() {
        let tmp = std::env::temp_dir().join(format!("wi-cloud-hop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("Dropbox")).unwrap();
        std::fs::create_dir_all(tmp.join("plain")).unwrap();
        junction(tmp.join("Dropbox"), tmp.join("plain/hop")).unwrap();
        let path = tmp.join("plain/missing/../hop/vault");
        assert_eq!(cloud_service(&path), Some("Dropbox"));
        assert_eq!(
            resolve_path(&path),
            tmp.join("Dropbox").canonicalize().unwrap().join("vault")
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn missing_parent_then_symlink_then_parent_still_detects_cloud() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("Dropbox/child")).unwrap();
        std::fs::create_dir_all(t.path().join("plain")).unwrap();
        junction(t.path().join("Dropbox/child"), t.path().join("plain/hop")).unwrap();
        let path = t.path().join("plain/missing/../hop/../vault");
        assert_eq!(cloud_service(&path), Some("Dropbox"), "{}", path.display());
        assert_eq!(
            resolve_path(&path),
            t.path()
                .join("Dropbox")
                .canonicalize()
                .unwrap()
                .join("vault")
        );
    }

    #[cfg(unix)]
    #[test]
    fn path_forms_resolve_physically() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().canonicalize().unwrap();
        std::fs::create_dir_all(base.join("Dropbox/child")).unwrap();
        std::fs::create_dir_all(base.join("plain/sub")).unwrap();
        std::os::unix::fs::symlink(base.join("Dropbox/child"), base.join("plain/hop")).unwrap();
        std::os::unix::fs::symlink(base.join("plain/hop"), base.join("chain")).unwrap();
        std::os::unix::fs::symlink(base.join("Dropbox/missing"), base.join("plain/dangling"))
            .unwrap();
        std::os::unix::fs::symlink("../Dropbox/gone", base.join("plain/relative")).unwrap();
        std::os::unix::fs::symlink("loop-b", base.join("loop-a")).unwrap();
        std::os::unix::fs::symlink("loop-a", base.join("loop-b")).unwrap();
        let dropbox = base.join("Dropbox");
        let cases: &[(&str, PathBuf)] = &[
            ("plain", base.join("plain")),
            ("plain/./sub/.", base.join("plain/sub")),
            ("plain/sub/..", base.join("plain")),
            ("plain/hop", dropbox.join("child")),
            ("plain/hop/..", dropbox.clone()),
            ("plain/hop/../vault", dropbox.join("vault")),
            ("chain/../vault", dropbox.join("vault")),
            ("plain/missing", base.join("plain/missing")),
            ("plain/missing/deeper/..", base.join("plain/missing")),
            ("plain/missing/../hop/vault", dropbox.join("child/vault")),
            ("plain/missing/../hop/../vault", dropbox.join("vault")),
            ("plain/a/b/../../hop/../x/./y", dropbox.join("x/y")),
            ("plain/sub/../hop/missing/../../v", dropbox.join("v")),
            ("missing/../../", base.parent().unwrap().to_path_buf()),
            ("plain/dangling", dropbox.join("missing")),
            ("plain/dangling/../vault", dropbox.join("vault")),
            ("plain/relative/../vault", dropbox.join("vault")),
            ("loop-a/x/../y", base.join("loop-a/y")),
        ];
        for (form, expected) in cases {
            assert_eq!(&resolve_path(&base.join(form)), expected, "{form}");
        }
        assert_eq!(resolve_path(Path::new("/")), PathBuf::from("/"));
        assert_eq!(resolve_path(Path::new("/..")), PathBuf::from("/"));
    }
}
