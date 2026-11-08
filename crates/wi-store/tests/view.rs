//! Committed snapshots for read-only views read the branch tip only.
use wi_store::{OpenOptions, Placement, StorageMode, Vault};

#[test]
fn committed_notes_use_the_tip_with_unique_permalinks_and_archive_flags() {
    let temp = tempfile::tempdir().unwrap();
    let (vault, _) = Vault::init_placed(
        temp.path().join("notes"),
        OpenOptions {
            index_path: Some(temp.path().join("index.sqlite3")),
        },
        Placement {
            storage: Some(StorageMode::Git),
            data_root: Some(temp.path().join("data")),
        },
    )
    .unwrap();
    let root = vault.root().to_path_buf();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    for (path, text) in [
        ("a.md", "---\npermalink: shared\n---\n# Alpha\n"),
        ("b.md", "---\npermalink: shared\n---\n# Beta\n"),
        (
            "sub/a.md",
            "---\ntitle: Nested\nstatus: archived\n---\nbody\n",
        ),
        ("image.png", "not a note"),
    ] {
        std::fs::write(root.join(path), text).unwrap();
    }
    vault.reconcile().unwrap();
    vault.flush().unwrap();
    // Working-tree edits after the commit are not part of the snapshot.
    std::fs::write(root.join("a.md"), "uncommitted\n").unwrap();
    std::fs::write(root.join("draft.md"), "# Draft\n").unwrap();
    let notes = vault.committed_notes().unwrap();
    let paths: Vec<_> = notes.iter().map(|n| n.rel_path.as_str()).collect();
    assert_eq!(paths, ["a.md", "b.md", "sub/a.md"]);
    assert!(notes[0].bytes.starts_with(b"---\npermalink: shared"));
    assert_eq!(notes[0].permalink, "shared");
    assert_ne!(notes[1].permalink, "shared");
    assert_eq!(notes[0].title, "Alpha");
    assert_eq!(notes[2].title, "Nested");
    assert!(notes[2].archived && !notes[0].archived);
    let mut permalinks: Vec<_> = notes.iter().map(|n| &n.permalink).collect();
    permalinks.dedup();
    assert_eq!(permalinks.len(), 3);
    assert!(
        vault
            .device_state_dir()
            .unwrap()
            .starts_with(temp.path().join("data"))
    );
}
