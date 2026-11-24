//! The manifest cache against changes that keep the checkpoint and the last
//! cached frame: a load proves the file unchanged from its full state or
//! compares every cached byte, and lock-free reads never fill the cache.

use std::fs::{File, FileTimes, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

use crate::fault::{UNKNOWN_FILE_STATE, manifest_full_parses, manifest_prefix_compares};
use crate::manifest::{self, MAGIC, Manifest, ManifestSnapshot, Record, encode_frame};
use crate::manifest_cache::ManifestCache;
use crate::manifest_cache_tests::{assert_same, fresh, new_vault, record, save, strict};
use crate::{Blob, FsWorkingTree, HistoryError, IngestRecord, PublisherLock, Recovery, Vault};
use crate::{BlobId, hash_bytes};

/// Longer than a coarse kernel clock tick (about 15.6 ms on Windows), so an
/// outside rewrite gets a later change time than the state the cache kept;
/// within one tick the documented coarse-timestamp limit applies instead.
const TICK: Duration = Duration::from_millis(50);

fn fake_blob() -> BlobId {
    BlobId("0123456789012345678901234567890123456789".into())
}

fn ingest(seq: u64, blob: BlobId) -> Record {
    Record::Ingest {
        seq,
        time_ms: 0,
        rec: IngestRecord {
            actor: "cli".into(),
            path: format!("{seq}.md"),
            new_hash: hash_bytes(b"x"),
            new_blob: blob,
            batch: None,
        },
    }
}

fn intent(seq: u64, path: &str) -> Record {
    let mut rec = record_without_vault(path);
    rec.path = path.into();
    Record::Intent {
        seq,
        time_ms: 0,
        rec,
    }
}

fn record_without_vault(path: &str) -> crate::IntentRecord {
    crate::IntentRecord {
        op_id: None,
        request_digest: "d".into(),
        actor: "cli".into(),
        path: path.into(),
        moved_from: None,
        old_hash: None,
        new_hash: hash_bytes(b"x"),
        new_blob: fake_blob(),
        revert_of: None,
        batch: None,
        losses: Vec::new(),
    }
}

fn initial() -> Vec<u8> {
    let mut bytes = MAGIC.to_vec();
    bytes.extend(
        encode_frame(&Record::Checkpoint {
            manifest_id: "damage".into(),
            next_seq: 1,
            generation: 0,
            retired_upto: 0,
        })
        .unwrap(),
    );
    bytes
}

fn add(bytes: &mut Vec<u8>, record: &Record) {
    bytes.extend(encode_frame(record).unwrap());
}

/// The cached load equals a full parse of the same bytes, damage included.
fn assert_parse(cached: &ManifestSnapshot, bytes: &[u8]) {
    let full = manifest::parse(bytes);
    assert_eq!(cached.corrupt, full.corrupt);
    assert_same(cached, &full);
}

/// Overwrite `at` in place and append `tail`: same file, longer.
fn patch_and_grow(path: &Path, at: usize, patch: &[u8], tail: &[u8]) {
    let mut file = OpenOptions::new().write(true).open(path).unwrap();
    file.seek(SeekFrom::Start(at as u64)).unwrap();
    file.write_all(patch).unwrap();
    file.seek(SeekFrom::End(0)).unwrap();
    file.write_all(tail).unwrap();
}

#[test]
fn middle_damage_followed_by_growth_is_seen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("manifest.log");
    let cache = ManifestCache::default();
    let mut bytes = initial();
    add(&mut bytes, &ingest(1, fake_blob()));
    add(&mut bytes, &ingest(2, fake_blob()));
    std::fs::write(&path, &bytes).unwrap();
    let snap = cache.load(&path).unwrap().unwrap();
    let at = snap.frames[0].1.start + 20;
    bytes[at] ^= 0x20;
    let tail = encode_frame(&ingest(3, fake_blob())).unwrap();
    bytes.extend(&tail);
    patch_and_grow(&path, at, &[bytes[at]], &tail);
    let cached = cache.load(&path).unwrap().unwrap();
    assert!(cached.corrupt.is_some(), "damage under growth is reported");
    assert_parse(&cached, &bytes);
}

#[test]
fn middle_damage_followed_by_growth_is_refused_and_quarantined() {
    let (_dir, vault) = new_vault();
    save(&vault, "a.md");
    save(&vault, "b.md");
    let snap = strict(&vault).unwrap();
    let path = Manifest::path(&vault);
    let at = snap.frames[0].1.start + 20;
    let flipped = std::fs::read(&path).unwrap()[at] ^ 0x20;
    let blob = Blob::write_durable(&vault, b"x").unwrap();
    let tail = encode_frame(&ingest(snap.next_seq, blob)).unwrap();
    patch_and_grow(&path, at, &[flipped], &tail);
    assert!(matches!(
        strict(&vault),
        Err(HistoryError::CorruptManifest { .. })
    ));
    let report = Recovery::replay(&vault, &FsWorkingTree::new(vault.root())).unwrap();
    assert_eq!(report.quarantined.len(), 1);
    assert_same(&strict(&vault).unwrap(), &fresh(&vault));
}

#[test]
fn a_same_length_rewrite_with_the_time_set_back_is_parsed_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("manifest.log");
    let cache = ManifestCache::default();
    let mut bytes = initial();
    add(&mut bytes, &intent(1, "a.md"));
    add(&mut bytes, &ingest(2, fake_blob()));
    std::fs::write(&path, &bytes).unwrap();
    let stamp = std::fs::metadata(&path).unwrap().modified().unwrap();
    let snap = cache.load(&path).unwrap().unwrap();
    let range = snap.frames[0].1.clone();
    let replacement = encode_frame(&intent(1, "b.md")).unwrap();
    assert_eq!(range.len(), replacement.len());
    bytes[range].copy_from_slice(&replacement);
    std::thread::sleep(TICK);
    // Truncate and regrow the same file, then restore its modification time.
    std::fs::write(&path, &bytes).unwrap();
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(stamp))
        .unwrap();
    let cached = cache.load(&path).unwrap().unwrap();
    assert_eq!(cached.entries[0].op.paths().next(), Some("b.md"));
    assert_parse(&cached, &bytes);
}

#[test]
fn a_stale_prefix_cannot_admit_an_overlapping_intent() {
    let (_dir, vault) = new_vault();
    let guard = PublisherLock::acquire(&vault).unwrap();
    Manifest::append_intent(&vault, &guard, record(&vault, "a.md", b"a")).unwrap();
    let blob = Blob::write_durable(&vault, b"c").unwrap();
    Manifest::append_ingest(
        &vault,
        &guard,
        IngestRecord {
            actor: "cli".into(),
            path: "c.md".into(),
            new_hash: hash_bytes(b"c"),
            new_blob: blob,
            batch: None,
        },
    )
    .unwrap();
    let snap = Manifest::snapshot(&vault, &guard).unwrap();
    let path = Manifest::path(&vault);
    let stamp = std::fs::metadata(&path).unwrap().modified().unwrap();
    std::thread::sleep(TICK);
    let mut bytes = std::fs::read(&path).unwrap();
    // Rename the open intent from `a.md` to `b.md`, same length, checksummed.
    let range = snap.frames[0].1.clone();
    let mut rec: Record = serde_json::from_slice(&bytes[range.start + 12..range.end]).unwrap();
    let Record::Intent {
        rec: ref mut inner, ..
    } = rec
    else {
        panic!("expected an intent frame");
    };
    inner.path = "b.md".into();
    let frame = encode_frame(&rec).unwrap();
    assert_eq!(frame.len(), range.len());
    bytes[range].copy_from_slice(&frame);
    std::fs::write(&path, &bytes).unwrap();
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(stamp))
        .unwrap();
    let refused = Manifest::append_intent(&vault, &guard, record(&vault, "b.md", b"b"));
    assert!(
        matches!(refused, Err(HistoryError::OpenIntent { seq: 1 })),
        "{refused:?}"
    );
    assert!(
        manifest::parse(&std::fs::read(&path).unwrap())
            .corrupt
            .is_none()
    );
}

#[test]
fn an_unlocked_read_during_a_held_publisher_lock_fills_no_cache() {
    let (_dir, vault) = new_vault();
    save(&vault, "a.md");
    let guard = PublisherLock::acquire(&vault).unwrap();
    let before = Manifest::snapshot(&vault, &guard).unwrap();
    assert_eq!(vault.manifest_cache().cached_len(), Some(before.valid_len));
    // Nothing changed: the lock-free read reuses the cache, read-only.
    let parses = manifest_full_parses();
    let compares = manifest_prefix_compares();
    let same = Manifest::snapshot_unlocked(&vault).unwrap();
    assert_eq!(manifest_full_parses(), parses);
    assert_eq!(manifest_prefix_compares(), compares);
    assert_same(&same, &before);
    // A complete frame written before any barrier, as a publisher does.
    let blob = Blob::write_durable(&vault, b"x").unwrap();
    let frame = encode_frame(&ingest(before.next_seq, blob)).unwrap();
    let mut file = OpenOptions::new()
        .append(true)
        .open(Manifest::path(&vault))
        .unwrap();
    file.write_all(&frame).unwrap();
    let read = Manifest::snapshot_unlocked(&vault).unwrap();
    assert_eq!(read.entries.len(), before.entries.len() + 1);
    assert_eq!(vault.manifest_cache().cached_len(), Some(before.valid_len));
    let root = vault.root().to_path_buf();
    let count = before.entries.len() + 1;
    std::thread::spawn(move || {
        let other = Vault::open(&root).unwrap();
        let read = Manifest::snapshot_unlocked(&other).unwrap();
        assert_eq!(read.entries.len(), count);
        assert_eq!(other.manifest_cache().cached_len(), None);
    })
    .join()
    .unwrap();
    drop(guard);
}

#[test]
fn a_replacement_with_unknown_identity_is_compared_byte_for_byte() {
    let (_dir, vault) = new_vault();
    for path in ["a.md", "b.md", "c.md"] {
        save(&vault, path);
    }
    UNKNOWN_FILE_STATE.with(|u| u.set(true));
    let snap = strict(&vault).unwrap();
    // Nothing changed, but nothing proves it: every load compares.
    let compares = manifest_prefix_compares();
    strict(&vault).unwrap();
    assert!(
        manifest_prefix_compares() > compares,
        "unknown state compared"
    );
    // The Windows shape: a new file with the same checkpoint and last frame,
    // damage between them, and one more frame.
    let path = Manifest::path(&vault);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[snap.frames[1].1.start + 20] ^= 0x20;
    let blob = Blob::write_durable(&vault, b"x").unwrap();
    bytes.extend(encode_frame(&ingest(snap.next_seq, blob)).unwrap());
    let staged = path.with_extension("staged");
    std::fs::write(&staged, &bytes).unwrap();
    std::fs::rename(&staged, &path).unwrap();
    let result = strict(&vault);
    UNKNOWN_FILE_STATE.with(|u| u.set(false));
    assert!(
        matches!(result, Err(HistoryError::CorruptManifest { .. })),
        "{result:?}"
    );
}

#[test]
fn own_appends_keep_the_no_read_path() {
    let (_dir, vault) = new_vault();
    save(&vault, "a.md");
    strict(&vault).unwrap();
    let parses = manifest_full_parses();
    let compares = manifest_prefix_compares();
    for i in 0..20 {
        save(&vault, &format!("n{i}.md"));
    }
    let snap = strict(&vault).unwrap();
    assert_eq!(manifest_full_parses(), parses, "no full parse");
    assert_eq!(manifest_prefix_compares(), compares, "no file read");
    assert_eq!(snap.entries.len(), 21);
    assert_same(&snap, &fresh(&vault));
}
