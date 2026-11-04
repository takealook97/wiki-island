//! Bulk-import batches interleaved with other records, including moves:
//! the final tree follows manifest order, every record is committed once,
//! and a crash between commit and retirement replays without blocking.

mod common;

use std::collections::BTreeMap;

use common::*;
use wi_history::{
    BatchMark, FlushReport, Flusher, History, Manifest, PublisherLock, Seq, Vault, blob_id_of,
};

fn batch_mark(batch: Option<(&str, bool)>) -> Option<BatchMark> {
    batch.map(|(id, last)| BatchMark {
        id: id.into(),
        last,
    })
}

fn write(vault: &Vault, path: &str, bytes: &[u8], batch: Option<(&str, bool)>) -> Seq {
    let mut rec = intent(vault, path, bytes, "cli", None);
    rec.batch = batch_mark(batch);
    save_record(vault, rec, bytes)
}

/// The engine's move in miniature: intent, destination written, source
/// removed, done.
fn mv(vault: &Vault, from: &str, to: &str, batch: Option<(&str, bool)>) -> Seq {
    let bytes = std::fs::read(vault.root().join(from)).unwrap();
    let mut rec = intent(vault, to, &bytes, "cli", None);
    rec.moved_from = Some(from.into());
    rec.old_hash = None;
    rec.batch = batch_mark(batch);
    let guard = PublisherLock::acquire(vault).unwrap();
    let seq = Manifest::append_intent(vault, &guard, rec).unwrap();
    write_file(vault, to, &bytes);
    std::fs::remove_file(vault.root().join(from)).unwrap();
    Manifest::append_done(vault, &guard, seq).unwrap();
    seq
}

/// Flush in a child process that aborts after publishing its commits and
/// before retiring any record.
fn crash_flush(vault: &Vault) {
    let root = vault.root().to_str().unwrap().to_owned();
    let status = testproc()
        .args(["flush", &root])
        .env("WI_FAULT", "after_commit_before_retire")
        .status()
        .unwrap();
    assert!(!status.success(), "the flush process was killed");
}

fn groups(report: &FlushReport) -> Vec<Vec<Seq>> {
    report.commits.iter().map(|c| c.seqs.clone()).collect()
}

fn head_paths(vault: &Vault) -> Vec<String> {
    let head = History::head(vault).unwrap().unwrap();
    git(vault.root(), &["ls-tree", "-r", "--name-only", &head])
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A batch member moved away by a newer interleaved move is legitimately
/// absent from the batch commit; replay after a crash must accept it.
#[test]
fn replay_accepts_a_batch_member_moved_by_an_interleaved_record() {
    let (_dir, vault) = new_vault();
    write(&vault, "p.md", b"p\n", Some(("A", false)));
    mv(&vault, "p.md", "archive/p.md", None);
    write(&vault, "q.md", b"q\n", Some(("A", true)));

    crash_flush(&vault);
    assert_eq!(all_seqs(vault.root()), vec![2, 1, 3]);
    let report = Flusher::flush(&vault).unwrap();
    assert_eq!(report.blocked, None, "{report:?}");
    assert!(report.commits.is_empty());
    assert_eq!(report.already_committed, vec![2, 1, 3]);
    assert_eq!(report.retired_upto, 3);
    assert_eq!(report.remaining, 0);
    assert_eq!(head_paths(&vault), vec!["archive/p.md", "q.md"]);
    fsck(vault.root());
}

/// The same for a member moved by a later member of its own batch.
#[test]
fn replay_accepts_a_batch_member_moved_within_its_batch() {
    let (_dir, vault) = new_vault();
    write(&vault, "p.md", b"p\n", Some(("A", false)));
    mv(&vault, "p.md", "r.md", Some(("A", true)));

    crash_flush(&vault);
    let report = Flusher::flush(&vault).unwrap();
    assert_eq!(report.blocked, None, "{report:?}");
    assert_eq!(report.already_committed, vec![1, 2]);
    assert_eq!(report.remaining, 0);
    assert_eq!(head_paths(&vault), vec!["r.md"]);
}

/// A newer write to a batch move's destination supersedes only the
/// destination upsert; the source deletion still lands.
#[test]
fn superseded_batch_move_still_removes_its_source() {
    let (_dir, vault) = new_vault();
    write(&vault, "src.md", b"old\n", None);
    Flusher::flush(&vault).unwrap();
    mv(&vault, "src.md", "dest.md", Some(("A", false)));
    write(&vault, "dest.md", b"new\n", None);
    write(&vault, "q.md", b"q\n", Some(("A", true)));

    let report = Flusher::flush(&vault).unwrap();
    assert_eq!(groups(&report), vec![vec![3], vec![2, 4]]);
    assert_eq!(report.remaining, 0);
    assert_eq!(head_paths(&vault), vec!["dest.md", "q.md"]);
    let head = History::head(&vault).unwrap().unwrap();
    assert_eq!(show(vault.root(), &head, "dest.md"), b"new\n");
}

/// A newer recreation of a batch move's source survives the batch.
#[test]
fn newer_recreation_of_a_batch_move_source_survives() {
    let (_dir, vault) = new_vault();
    write(&vault, "src.md", b"old\n", None);
    Flusher::flush(&vault).unwrap();
    mv(&vault, "src.md", "dest.md", Some(("A", false)));
    write(&vault, "src.md", b"again\n", None);
    write(&vault, "q.md", b"q\n", Some(("A", true)));

    Flusher::flush(&vault).unwrap();
    let head = History::head(&vault).unwrap().unwrap();
    assert_eq!(head_paths(&vault), vec!["dest.md", "q.md", "src.md"]);
    assert_eq!(show(vault.root(), &head, "src.md"), b"again\n");
    assert_eq!(show(vault.root(), &head, "dest.md"), b"old\n");
}

/// Deterministic xorshift64*, so failures reproduce from the printed seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const PATHS: [&str; 5] = ["a.md", "b.md", "c.md", "d/e.md", "d/f.md"];

/// Append a random interleaving of writes and moves, inside and outside up
/// to two concurrent batches, and return the reference tree: every record
/// applied in manifest order.
fn random_manifest(vault: &Vault, rng: &mut Rng, seed: u64) -> (BTreeMap<String, Vec<u8>>, Seq) {
    let mut model: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut open: Vec<String> = Vec::new();
    let mut batches = 0;
    let mut last = 0;
    let steps = 6 + rng.below(10);
    for step in 0..steps {
        let batch = match rng.below(4) {
            0 if open.len() < 2 => {
                batches += 1;
                open.push(format!("b{batches}"));
                open.last().cloned()
            }
            1 | 2 if !open.is_empty() => Some(open[rng.below(open.len())].clone()),
            _ => None,
        };
        // Sometimes this member closes its batch.
        let closes = batch.is_some() && rng.below(4) == 0;
        let mark = batch.as_deref().map(|id| (id, closes));
        let absent: Vec<&str> = PATHS
            .iter()
            .copied()
            .filter(|p| !model.contains_key(*p))
            .collect();
        if !model.is_empty() && !absent.is_empty() && rng.below(3) == 0 {
            let keys: Vec<String> = model.keys().cloned().collect();
            let from = keys[rng.below(keys.len())].clone();
            let to = absent[rng.below(absent.len())];
            last = mv(vault, &from, to, mark);
            let bytes = model.remove(&from).unwrap();
            model.insert(to.to_owned(), bytes);
        } else {
            let path = PATHS[rng.below(PATHS.len())];
            let bytes = format!("{seed}-{step}\n").into_bytes();
            last = write(vault, path, &bytes, mark);
            model.insert(path.to_owned(), bytes);
        }
        if closes {
            open.retain(|id| Some(id) != batch.as_ref());
        }
    }
    for (i, id) in open.iter().enumerate() {
        let path = PATHS[rng.below(PATHS.len())];
        let bytes = format!("{seed}-close-{i}\n").into_bytes();
        last = write(vault, path, &bytes, Some((id, true)));
        model.insert(path.to_owned(), bytes);
    }
    (model, last)
}

fn assert_head_matches(vault: &Vault, model: &BTreeMap<String, Vec<u8>>, seed: u64) {
    let head = History::head(vault).unwrap().unwrap();
    for path in PATHS {
        let at = History::blob_at(vault, &head, path).unwrap();
        let want = model.get(path).map(|b| blob_id_of(b).unwrap());
        assert_eq!(at, want, "seed {seed}: {path}");
    }
}

#[test]
fn random_interleavings_match_manifest_order_and_commit_each_record_once() {
    for seed in 1..=48u64 {
        let (_dir, vault) = new_vault();
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let (model, last) = random_manifest(&vault, &mut rng, seed);
        let crashed = seed % 2 == 0;
        if crashed {
            crash_flush(&vault);
        }
        let report = Flusher::flush(&vault).unwrap();
        assert_eq!(report.blocked, None, "seed {seed}: {report:?}");
        assert_eq!(report.remaining, 0, "seed {seed}");
        assert_eq!(report.retired_upto, last, "seed {seed}");
        if crashed {
            assert!(report.commits.is_empty(), "seed {seed}: {report:?}");
        }
        assert_head_matches(&vault, &model, seed);
        let mut seqs = all_seqs(vault.root());
        seqs.sort_unstable();
        assert_eq!(seqs, (1..=last).collect::<Vec<_>>(), "seed {seed}");
        assert!(Flusher::flush(&vault).unwrap().commits.is_empty());
    }
}
