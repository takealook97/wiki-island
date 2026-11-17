//! The manifest cache against its file: every snapshot through a cached
//! handle equals a full parse by a fresh handle, whatever happened to the
//! file between two loads.

use std::time::Duration;

use crate::fault::{FAIL_MANIFEST_BARRIER, manifest_full_parses};
use crate::manifest::{Manifest, ManifestSnapshot};
use crate::{Blob, Flusher, FsWorkingTree, HistoryError, IntentRecord, PublisherLock, Recovery};
use crate::{Seq, Vault, hash_bytes};

/// Long enough for a coarse file-time clock to tick between two writes.
const CLOCK_TICK: Duration = Duration::from_millis(50);

fn new_vault() -> (tempfile::TempDir, Vault) {
    let dir = tempfile::tempdir().unwrap();
    let vault = Vault::init(dir.path()).unwrap();
    (dir, vault)
}

fn record(vault: &Vault, path: &str, bytes: &[u8]) -> IntentRecord {
    IntentRecord {
        op_id: None,
        request_digest: "d".into(),
        actor: "cli".into(),
        path: path.into(),
        moved_from: None,
        old_hash: None,
        new_hash: hash_bytes(bytes),
        new_blob: Blob::write_durable(vault, bytes).unwrap(),
        revert_of: None,
        batch: None,
        losses: Vec::new(),
    }
}

fn save(vault: &Vault, path: &str) -> Seq {
    let bytes = format!("# {path}\n");
    let guard = PublisherLock::acquire(vault).unwrap();
    let seq =
        Manifest::append_intent(vault, &guard, record(vault, path, bytes.as_bytes())).unwrap();
    std::fs::write(vault.root().join(path), &bytes).unwrap();
    Manifest::append_done(vault, &guard, seq).unwrap();
    seq
}

fn strict(vault: &Vault) -> crate::Result<ManifestSnapshot> {
    let guard = PublisherLock::acquire(vault).unwrap();
    Manifest::snapshot(vault, &guard)
}

/// The parsed state a fresh handle (an empty cache) reads.
fn fresh(vault: &Vault) -> ManifestSnapshot {
    strict(&Vault::open(vault.root()).unwrap()).unwrap()
}

fn assert_same(cached: &ManifestSnapshot, full: &ManifestSnapshot) {
    assert_eq!(cached.manifest_id, full.manifest_id);
    assert_eq!(cached.retired_upto, full.retired_upto);
    assert_eq!(cached.next_seq, full.next_seq);
    assert_eq!(cached.generation, full.generation);
    assert_eq!(cached.valid_len, full.valid_len);
    assert_eq!(cached.open, full.open);
    assert_eq!(*cached.frames, *full.frames);
    let entries =
        |s: &ManifestSnapshot| s.entries.iter().map(|e| (**e).clone()).collect::<Vec<_>>();
    assert_eq!(entries(cached), entries(full));
}

#[test]
fn records_another_handle_appended_are_folded_without_a_full_parse() {
    let (_dir, vault) = new_vault();
    let other = Vault::open(vault.root()).unwrap();
    save(&vault, "a.md");
    strict(&vault).unwrap();
    save(&other, "b.md");
    save(&other, "c.md");
    let before = manifest_full_parses();
    let snap = strict(&vault).unwrap();
    assert_eq!(
        manifest_full_parses(),
        before,
        "grown file folded, not reparsed"
    );
    assert_eq!(snap.entries.len(), 3);
    assert_same(&snap, &fresh(&vault));
    // This handle's next write starts from the folded state.
    assert_eq!(save(&vault, "d.md"), 4);
    assert_same(&strict(&vault).unwrap(), &fresh(&vault));
}

#[test]
fn a_replaced_manifest_is_parsed_again() {
    let (_dir, vault) = new_vault();
    let other = Vault::open(vault.root()).unwrap();
    for path in ["a.md", "b.md", "c.md"] {
        save(&vault, path);
    }
    strict(&vault).unwrap();
    // Another handle retires the prefix: the file is atomically replaced.
    Flusher::flush(&other).unwrap();
    save(&other, "d.md");
    let before = manifest_full_parses();
    let snap = strict(&vault).unwrap();
    assert!(manifest_full_parses() > before, "replaced file reparsed");
    assert_eq!(snap.retired_upto, 3);
    assert_same(&snap, &fresh(&vault));
}

#[test]
fn a_same_length_rewrite_in_place_is_parsed_again() {
    let (_dir, vault) = new_vault();
    for path in ["a.md", "b.md", "c.md"] {
        save(&vault, path);
    }
    let snap = strict(&vault).unwrap();
    std::thread::sleep(CLOCK_TICK);
    // Damage a middle frame; keep the file, its length, its checkpoint and
    // its last frame.
    let path = Manifest::path(&vault);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[snap.frames[2].1.start + 20] ^= 0x20;
    std::fs::write(&path, &bytes).unwrap();
    assert!(matches!(
        strict(&vault),
        Err(HistoryError::CorruptManifest { .. })
    ));
}

#[test]
fn a_damaged_tail_is_never_cached_and_is_quarantined() {
    let (_dir, vault) = new_vault();
    save(&vault, "a.md");
    save(&vault, "b.md");
    let clean = strict(&vault).unwrap();
    let path = Manifest::path(&vault);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    std::io::Write::write_all(&mut file, &[7, 0, 0, 0, 1, 2]).unwrap();
    drop(file);
    for _ in 0..2 {
        assert!(matches!(
            strict(&vault),
            Err(HistoryError::CorruptManifest { .. })
        ));
        assert_eq!(vault.manifest_cache().cached_len(), None);
    }
    // Lock-free readers still see the valid prefix.
    assert_eq!(
        Manifest::snapshot_unlocked(&vault).unwrap().generation,
        clean.generation
    );
    let report = Recovery::replay(&vault, &FsWorkingTree::new(vault.root())).unwrap();
    assert_eq!(report.quarantined.len(), 1);
    let snap = strict(&vault).unwrap();
    assert_same(&snap, &fresh(&vault));
    assert_eq!(save(&vault, "c.md"), snap.next_seq);
}

#[test]
fn a_failed_append_barrier_leaves_the_cache_unchanged() {
    let (_dir, vault) = new_vault();
    save(&vault, "a.md");
    let snap = strict(&vault).unwrap();
    let cached = vault.manifest_cache().cached_len();
    assert_eq!(cached, Some(snap.valid_len));
    let guard = PublisherLock::acquire(&vault).unwrap();
    FAIL_MANIFEST_BARRIER.with(|f| f.set(true));
    let failed = Manifest::append_intent(&vault, &guard, record(&vault, "b.md", b"b"));
    FAIL_MANIFEST_BARRIER.with(|f| f.set(false));
    assert!(
        matches!(failed, Err(HistoryError::Injected(_))),
        "{failed:?}"
    );
    assert_eq!(vault.manifest_cache().cached_len(), cached);
    drop(guard);
    // The next load reads what the file holds, as a fresh handle does.
    assert_same(&strict(&vault).unwrap(), &fresh(&vault));
}
