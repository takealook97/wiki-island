//! Lock-free reads after the file state moved without new bytes: the
//! snapshot equals a full parse of the same bytes, the whole file is not
//! parsed again while the kept prefix still matches, and the kept state is
//! left as it was.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use crate::Vault;
use crate::fault::{manifest_full_parses, manifest_prefix_compares};
use crate::manifest::{self, Manifest, ManifestSnapshot};
use crate::manifest_cache_tests::{assert_same, new_vault, save, strict};

/// Set the permissions the file already has: no byte changes, but the
/// change time moves, so the kept file state no longer proves anything.
fn touch_metadata(path: &Path) {
    let permissions = std::fs::metadata(path).unwrap().permissions();
    std::fs::set_permissions(path, permissions).unwrap();
}

/// A vault whose handle keeps a parsed prefix, with more frames appended
/// after it by another handle and the file state moved.
fn kept_prefix_then_growth() -> (tempfile::TempDir, Vault) {
    let (dir, vault) = new_vault();
    for path in ["a.md", "b.md", "c.md"] {
        save(&vault, path);
    }
    strict(&vault).unwrap();
    let other = Vault::open(vault.root()).unwrap();
    for path in ["d.md", "e.md"] {
        save(&other, path);
    }
    touch_metadata(&Manifest::path(&vault));
    (dir, vault)
}

/// The lock-free read equals a full parse of the same bytes, damage
/// included, without a full parse and without touching the kept state.
fn assert_peek_folds_tail(vault: &Vault) -> ManifestSnapshot {
    let path = Manifest::path(vault);
    let kept = vault.manifest_cache().cached_len();
    assert!(kept.is_some(), "a prefix is kept");
    let parses = manifest_full_parses();
    let compares = manifest_prefix_compares();
    let read = Manifest::snapshot_unlocked(vault).unwrap();
    assert_eq!(manifest_full_parses(), parses, "no full parse");
    assert!(
        manifest_prefix_compares() > compares,
        "kept prefix compared"
    );
    assert_eq!(vault.manifest_cache().cached_len(), kept, "kept state");
    let full = manifest::parse(&std::fs::read(&path).unwrap());
    assert_eq!(read.corrupt, full.corrupt);
    assert_same(&read, &full);
    read
}

#[test]
fn a_metadata_change_alone_costs_an_unlocked_read_no_full_parse() {
    let (_dir, vault) = new_vault();
    for path in ["a.md", "b.md", "c.md"] {
        save(&vault, path);
    }
    strict(&vault).unwrap();
    touch_metadata(&Manifest::path(&vault));
    let read = assert_peek_folds_tail(&vault);
    assert_eq!(read.entries.len(), 3);
}

#[test]
fn an_unlocked_read_folds_only_the_appended_frames() {
    let (_dir, vault) = kept_prefix_then_growth();
    let read = assert_peek_folds_tail(&vault);
    assert!(read.corrupt.is_none());
    assert_eq!(read.entries.len(), 5);
}

#[test]
fn an_unlocked_read_reports_a_torn_tail_as_a_full_parse_does() {
    let (_dir, vault) = kept_prefix_then_growth();
    let path = Manifest::path(&vault);
    let bytes = std::fs::read(&path).unwrap();
    // Half of a copy of the last frame header and payload: a torn append.
    let torn = &bytes[bytes.len() - 40..bytes.len() - 10];
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(torn).unwrap();
    let read = assert_peek_folds_tail(&vault);
    assert!(read.corrupt.is_some(), "torn tail reported");
}

#[test]
fn an_unlocked_read_reports_a_damaged_last_frame_as_a_full_parse_does() {
    let (_dir, vault) = kept_prefix_then_growth();
    let path = Manifest::path(&vault);
    let mut bytes = std::fs::read(&path).unwrap();
    let last = bytes.len() - 3;
    bytes[last] ^= 0x20;
    // Same file, same length: only the bytes after the kept prefix change.
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .write_all(&bytes)
        .unwrap();
    let read = assert_peek_folds_tail(&vault);
    assert!(read.corrupt.is_some(), "damaged frame reported");
}
