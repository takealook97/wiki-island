//! Another writer that ignores the publisher lock changes the manifest
//! after an own append passed its barrier and before the cache records it.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

use crate::fault::{AFTER_MANIFEST_BARRIER, BarrierHook};
use crate::manifest::{self, Manifest, Record, encode_frame};
use crate::manifest_cache_tests::{fresh, new_vault, record, strict};
use crate::{Blob, HistoryError, IngestRecord, PublisherLock, Vault, hash_bytes};

/// Longer than a coarse kernel clock tick, so the other write gets a later
/// change time than the own append.
const TICK: Duration = Duration::from_millis(20);

fn arm(hook: impl FnOnce(&Path) + 'static) {
    let hook: BarrierHook = Box::new(hook);
    AFTER_MANIFEST_BARRIER.with(|h| *h.borrow_mut() = Some(hook));
}

/// Rewrite the first record after the checkpoint, an intent on `from`, in
/// place to the same intent on `to`; the file length stays.
fn retarget_first_intent(path: &Path, from: &str, to: &str) {
    let bytes = std::fs::read(path).unwrap();
    let snap = manifest::parse(&bytes);
    let range = snap.frames[0].1.clone();
    let mut record: Record = serde_json::from_slice(&bytes[range.start + 12..range.end]).unwrap();
    let Record::Intent { rec, .. } = &mut record else {
        panic!("intent expected");
    };
    assert_eq!(rec.path, from);
    rec.path = to.into();
    let frame = encode_frame(&record).unwrap();
    assert_eq!(frame.len(), range.len(), "same length");
    std::thread::sleep(TICK);
    let mut file = OpenOptions::new().write(true).open(path).unwrap();
    file.seek(SeekFrom::Start(range.start as u64)).unwrap();
    file.write_all(&frame).unwrap();
}

fn append_ingest(vault: &Vault, guard: &crate::PublisherGuard, path: &str) {
    let rec = IngestRecord {
        actor: "cli".into(),
        path: path.into(),
        new_hash: hash_bytes(b"c"),
        new_blob: Blob::write_durable(vault, b"c").unwrap(),
        batch: None,
    };
    Manifest::append_ingest(vault, guard, rec).unwrap();
}

#[test]
fn an_equal_length_rewrite_after_the_append_barrier_is_caught() {
    let (_dir, vault) = new_vault();
    let guard = PublisherLock::acquire(&vault).unwrap();
    Manifest::append_intent(&vault, &guard, record(&vault, "a.md", b"a")).unwrap();
    arm(|path| retarget_first_intent(path, "a.md", "b.md"));
    append_ingest(&vault, &guard, "c.md");
    assert!(
        AFTER_MANIFEST_BARRIER.with(|h| h.borrow().is_none()),
        "hook ran"
    );
    // The open intent now holds `b.md`, so a second one on it is refused.
    let next = Manifest::append_intent(&vault, &guard, record(&vault, "b.md", b"b"));
    assert!(
        matches!(next, Err(HistoryError::OpenIntent { seq: 1 })),
        "{next:?}"
    );
    let bytes = std::fs::read(Manifest::path(&vault)).unwrap();
    assert!(manifest::parse(&bytes).corrupt.is_none());
    drop(guard);
    assert_eq!(strict(&vault).unwrap().open, fresh(&vault).open);
}

#[test]
fn an_append_by_another_writer_after_the_append_barrier_is_folded() {
    let (_dir, vault) = new_vault();
    let guard = PublisherLock::acquire(&vault).unwrap();
    Manifest::append_intent(&vault, &guard, record(&vault, "a.md", b"a")).unwrap();
    let blob = Blob::write_durable(&vault, b"x").unwrap();
    arm(move |path| {
        let bytes = std::fs::read(path).unwrap();
        let seq = manifest::parse(&bytes).next_seq;
        let extra = Record::Ingest {
            seq,
            time_ms: 0,
            rec: IngestRecord {
                actor: "cli".into(),
                path: "x.md".into(),
                new_hash: hash_bytes(b"x"),
                new_blob: blob,
                batch: None,
            },
        };
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(&encode_frame(&extra).unwrap()).unwrap();
    });
    append_ingest(&vault, &guard, "c.md");
    let snap = Manifest::snapshot(&vault, &guard).unwrap();
    assert!(
        snap.entries.iter().any(|e| e.seq == 3),
        "other frame folded"
    );
    drop(guard);
    let full = fresh(&vault);
    assert_eq!(snap.valid_len, full.valid_len);
    assert_eq!(snap.next_seq, full.next_seq);
}
