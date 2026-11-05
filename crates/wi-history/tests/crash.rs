mod common;

use std::process::Command;

use common::*;
use wi_history::{
    Flusher, FsWorkingTree, History, HistoryError, Manifest, PublisherLock, Recovery, Vault,
    generation,
};

fn replay(vault: &Vault) -> wi_history::RecoveryReport {
    Recovery::replay(vault, &FsWorkingTree::new(vault.root())).unwrap()
}

fn manifest_path(vault: &Vault) -> std::path::PathBuf {
    vault.wi_dir().join("manifest.log")
}

/// Leave an intent without `done`, as a crash between steps 6 and 8 would.
fn crash_after_intent(vault: &Vault, path: &str, bytes: &[u8], write: Option<&[u8]>) -> u64 {
    let rec = intent(vault, path, bytes, "mcp", Some("op-crash"));
    let guard = PublisherLock::acquire(vault).unwrap();
    let seq = Manifest::append_intent(vault, &guard, rec).unwrap();
    if let Some(on_disk) = write {
        write_file(vault, path, on_disk);
    }
    seq
}

#[test]
fn replay_settles_intents_and_commits_every_ack_once_before_writes() {
    let (dir, vault) = new_vault();
    save(&vault, "a.md", b"a1", "cli", None);
    save(&vault, "b.md", b"b1", "cli", None);
    let open = crash_after_intent(&vault, "c.md", b"c-new", Some(b"c-new"));
    drop(vault);

    // Next start: writes to the open intent's path are refused until
    // replay (writes to other paths are not; see `held_intent_*`).
    let vault = Vault::open(dir.path()).unwrap();
    let guard = PublisherLock::acquire(&vault).unwrap();
    let rec = intent(&vault, "c.md", b"d", "cli", None);
    assert!(matches!(
        Manifest::append_intent(&vault, &guard, rec),
        Err(HistoryError::OpenIntent { seq }) if seq == open
    ));
    drop(guard);
    let flush = Flusher::flush(&vault);
    assert!(matches!(
        flush.unwrap().blocked,
        Some(wi_history::FlushBlock::OpenIntent { .. })
    ));

    let report = replay(&vault);
    assert_eq!(report.completed, vec![open]);
    assert_eq!(report.flush.retired_upto, 3);
    let commits = commits(vault.root());
    assert_eq!(all_seqs(vault.root()), vec![1, 2, 3]);
    assert_eq!(
        trailer(&commits[2].1, "Wi-Actor"),
        vec!["mcp"],
        "recorded actor, not ingress"
    );
    assert_eq!(show(vault.root(), &commits[2].0, "c.md"), b"c-new");
    assert_eq!(generation(&vault).unwrap(), 3);

    let again = replay(&vault);
    assert!(again.flush.commits.is_empty());
    assert_eq!(all_seqs(vault.root()), vec![1, 2, 3]);
    save(&vault, "d.md", b"d", "cli", None);
}

#[test]
fn intent_with_old_bytes_is_dropped() {
    let (_dir, vault) = new_vault();
    save(&vault, "a.md", b"old", "cli", None);
    let open = crash_after_intent(&vault, "a.md", b"new", None);
    let report = replay(&vault);
    assert_eq!(report.dropped, vec![open]);
    assert_eq!(all_seqs(vault.root()), vec![1]);
    assert_eq!(report.flush.remaining, 0);
    // A new file that never reached disk is dropped too.
    let open = crash_after_intent(&vault, "new.md", b"x", None);
    assert_eq!(replay(&vault).dropped, vec![open]);
}

#[test]
fn intent_with_other_bytes_is_a_conflict_until_acknowledged() {
    let (_dir, vault) = new_vault();
    save(&vault, "a.md", b"old", "cli", None);
    let open = crash_after_intent(&vault, "a.md", b"engine", Some(b"editor"));
    save_after_conflict_is_blocked(&vault, open);

    let report = replay(&vault);
    assert_eq!(report.conflicts.len(), 1);
    let conflict = &report.conflicts[0];
    assert_eq!(conflict.seq, open);
    assert_eq!(
        conflict.current_hash.as_deref(),
        Some(wi_history::hash_bytes(b"editor").as_str())
    );
    assert_eq!(
        vault.read_blob(&conflict.new_blob).unwrap(),
        b"engine",
        "both versions kept"
    );
    assert!(matches!(
        report.flush.blocked,
        Some(wi_history::FlushBlock::UnackedConflict { seq }) if seq == open
    ));

    // Later saves are accepted and wait behind the conflict.
    save(&vault, "b.md", b"b", "cli", None);
    assert_eq!(
        replay(&vault).conflicts.len(),
        1,
        "reported again until acknowledged"
    );
    let guard = PublisherLock::acquire(&vault).unwrap();
    Manifest::ack_conflict(&vault, &guard, open).unwrap();
    assert!(Manifest::ack_conflict(&vault, &guard, open).is_err());
    drop(guard);
    let report = Flusher::flush(&vault).unwrap();
    assert_eq!(report.retired_upto, 3);
    assert_eq!(all_seqs(vault.root()), vec![1, 3]);
}

/// A save to the open intent's path (`a.md`) is refused before replay.
fn save_after_conflict_is_blocked(vault: &Vault, open: u64) {
    let guard = PublisherLock::acquire(vault).unwrap();
    let rec = intent(vault, "a.md", b"z", "cli", None);
    assert!(matches!(
        Manifest::append_intent(vault, &guard, rec),
        Err(HistoryError::OpenIntent { seq }) if seq == open
    ));
}

#[test]
fn crash_between_commit_and_retire_commits_nothing_twice() {
    let (_dir, vault) = new_vault();
    for i in 0..3 {
        save(
            &vault,
            &format!("n{i}.md"),
            format!("{i}").as_bytes(),
            "cli",
            None,
        );
    }
    let root = vault.root().to_str().unwrap().to_owned();
    let status = testproc()
        .args(["flush", &root])
        .env("WI_FAULT", "after_commit_before_retire")
        .status()
        .unwrap();
    assert!(!status.success(), "the flush process was killed");
    let head = History::head(&vault).unwrap().unwrap();
    assert_eq!(all_seqs(vault.root()), vec![1, 2, 3], "commits are durable");
    let snap = {
        let guard = PublisherLock::acquire(&vault).unwrap();
        Manifest::snapshot(&vault, &guard).unwrap()
    };
    assert_eq!(snap.retired_upto, 0);
    assert_eq!(snap.entries.len(), 3, "not retired");

    let report = replay(&vault);
    assert!(report.flush.commits.is_empty());
    assert_eq!(report.flush.already_committed, vec![1, 2, 3]);
    assert_eq!(report.flush.retired_upto, 3);
    assert_eq!(History::head(&vault).unwrap().unwrap(), head);
    assert_eq!(all_seqs(vault.root()), vec![1, 2, 3]);

    save(&vault, "n3.md", b"3", "cli", None);
    Flusher::flush(&vault).unwrap();
    assert_eq!(all_seqs(vault.root()), vec![1, 2, 3, 4]);
    fsck(vault.root());
}

fn frame_starts(bytes: &[u8]) -> Vec<usize> {
    let mut pos = b"wi-manifest 1\n".len();
    let mut out = Vec::new();
    while pos + 12 <= bytes.len() {
        out.push(pos);
        let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 12 + len;
    }
    out
}

#[test]
fn truncated_last_record_is_quarantined_and_nothing_is_dropped() {
    let (_dir, vault) = new_vault();
    for i in 0..3 {
        save(
            &vault,
            &format!("n{i}.md"),
            format!("{i}").as_bytes(),
            "cli",
            None,
        );
    }
    let path = manifest_path(&vault);
    let bytes = std::fs::read(&path).unwrap();
    let cut = bytes.len() - 5;
    let last = *frame_starts(&bytes).last().unwrap();
    std::fs::write(&path, &bytes[..cut]).unwrap();

    // Before replay, strict readers refuse and lock-free readers tolerate.
    let guard = PublisherLock::acquire(&vault).unwrap();
    assert!(matches!(
        Manifest::snapshot(&vault, &guard),
        Err(HistoryError::CorruptManifest { .. })
    ));
    drop(guard);
    assert_eq!(generation(&vault).unwrap(), 2);

    let report = replay(&vault);
    assert_eq!(report.quarantined.len(), 1);
    let q = &report.quarantined[0];
    assert_eq!(q.offset as usize, last);
    assert_eq!(std::fs::read(&q.file).unwrap(), &bytes[last..cut]);
    assert!(q.file.starts_with(vault.git_dir()));
    assert_eq!(report.quarantine_files, vec![q.file.clone()]);
    // The lost record was seq 3's `done`; its intent survived and the file
    // holds the new bytes, so replay completed it.
    assert_eq!(report.completed, vec![3]);
    assert_eq!(all_seqs(vault.root()), vec![1, 2, 3]);
}

#[test]
fn checksum_failure_quarantines_the_rest_and_replays_earlier_records() {
    let (_dir, vault) = new_vault();
    for i in 0..3 {
        save(
            &vault,
            &format!("n{i}.md"),
            format!("{i}").as_bytes(),
            "cli",
            None,
        );
    }
    let path = manifest_path(&vault);
    let mut bytes = std::fs::read(&path).unwrap();
    let starts = frame_starts(&bytes);
    // starts: checkpoint, then intent/done pairs; corrupt seq 3's intent.
    let target = starts[5];
    bytes[target + 20] ^= 0x20;
    std::fs::write(&path, &bytes).unwrap();

    let report = replay(&vault);
    assert_eq!(report.quarantined.len(), 1);
    let q = &report.quarantined[0];
    assert!(q.reason.contains("checksum"), "{}", q.reason);
    assert_eq!(std::fs::read(&q.file).unwrap(), &bytes[target..]);
    assert_eq!(all_seqs(vault.root()), vec![1, 2]);
    assert!(report.completed.is_empty());
    // The manifest is valid again and accepts writes. Sequence ids the
    // quarantined tail could hold (3 at most here) are never reused: the
    // next id is above the tail's bound of one record per 13 bytes.
    let bound = ((bytes.len() - target) as u64).div_ceil(13);
    assert_eq!(save(&vault, "n9.md", b"9", "cli", None), 3 + bound);
    Flusher::flush(&vault).unwrap();
    assert_eq!(all_seqs(vault.root()), vec![1, 2, 3 + bound]);
}

#[test]
fn unreadable_header_is_quarantined_whole() {
    let (_dir, vault) = new_vault();
    save(&vault, "a.md", b"a", "cli", None);
    Flusher::flush(&vault).unwrap();
    std::fs::write(manifest_path(&vault), b"garbage").unwrap();
    let report = replay(&vault);
    assert_eq!(
        std::fs::read(&report.quarantined[0].file).unwrap(),
        b"garbage"
    );
    // A new manifest id: new sequence ids cannot collide with committed ones.
    let seq = save(&vault, "b.md", b"b", "cli", None);
    Flusher::flush(&vault).unwrap();
    assert_eq!(seq, 1);
    assert_eq!(commits(vault.root()).len(), 2);
    let _ = Command::new("true");
}

/// commits exist for seq 1 and 2 but the checkpoint was not advanced;
/// seq 2's intent is corrupt and quarantined. A later save must not reuse
/// seq 2, and must not be retired as "already committed" by the old commit.
#[test]
fn quarantine_after_unretired_commits_never_reuses_a_sequence_id() {
    let (_dir, vault) = new_vault();
    save(&vault, "a.md", b"a", "mcp", Some("op-a"));
    save(&vault, "b.md", b"b", "mcp", Some("op-b"));
    let root = vault.root().to_str().unwrap().to_owned();
    let status = testproc()
        .args(["flush", &root])
        .env("WI_FAULT", "after_commit_before_retire")
        .status()
        .unwrap();
    assert!(!status.success());
    assert_eq!(all_seqs(vault.root()), vec![1, 2]);

    let path = manifest_path(&vault);
    let mut bytes = std::fs::read(&path).unwrap();
    let starts = frame_starts(&bytes);
    // checkpoint, intent 1, done 1, intent 2, done 2: corrupt intent 2.
    bytes[starts[3] + 20] ^= 0x20;
    std::fs::write(&path, &bytes).unwrap();
    let report = replay(&vault);
    assert_eq!(report.quarantined.len(), 1);
    assert_eq!(report.flush.already_committed, vec![1]);

    let seq = save(&vault, "new.md", b"new", "mcp", Some("op-new"));
    assert!(seq > 2, "seq {seq} reuses a committed sequence id");
    let report = Flusher::flush(&vault).unwrap();
    assert_eq!(report.commits.len(), 1, "{report:?}");
    assert!(report.already_committed.is_empty());
    assert_eq!(report.remaining, 0);
    let head = History::head(&vault).unwrap().unwrap();
    assert_eq!(show(vault.root(), &head, "new.md"), b"new");
    assert_eq!(all_seqs(vault.root()), vec![1, 2, seq]);
    fsck(vault.root());
}

/// The sequence reuse backstop: a commit naming this manifest's sequence id with a
/// different operation blocks the flush; the record is neither skipped nor
/// retired.
#[test]
fn commit_naming_a_seq_with_other_bytes_blocks_instead_of_retiring() {
    let (_dir, vault) = new_vault();
    let manifest_id = {
        let guard = PublisherLock::acquire(&vault).unwrap();
        Manifest::snapshot(&vault, &guard).unwrap().manifest_id
    };
    write_file(&vault, "other.md", b"other");
    git(vault.root(), &["add", "other.md"]);
    git(
        vault.root(),
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            &format!("forged\n\nWi-Seq: 1 {manifest_id}\nWi-Actor: cli"),
        ],
    );
    let seq = save(&vault, "a.md", b"a", "cli", None);
    assert_eq!(seq, 1);
    let report = Flusher::flush(&vault).unwrap();
    assert!(
        matches!(
            report.blocked,
            Some(wi_history::FlushBlock::CommittedMismatch { seq: 1, .. })
        ),
        "{report:?}"
    );
    assert!(report.commits.is_empty());
    assert!(report.already_committed.is_empty());
    assert_eq!(report.retired_upto, 0);
    assert_eq!(report.remaining, 1);
}

/// A working tree whose `held` path cannot be hashed (an unreadable file).
struct Unreadable<'a> {
    inner: FsWorkingTree,
    held: &'a str,
}

impl wi_history::WorkingTree for Unreadable<'_> {
    fn current_hash(&self, path: &str) -> std::io::Result<Option<String>> {
        if path == self.held {
            return Err(std::io::Error::other("permission denied"));
        }
        self.inner.current_hash(path)
    }
}

#[test]
fn held_intent_blocks_only_its_own_paths_and_commits_later() {
    let (_dir, vault) = new_vault();
    save(&vault, "a.md", b"a1", "cli", None);
    let open = crash_after_intent(&vault, "a.md", b"a2", Some(b"a2"));
    let tree = Unreadable {
        inner: FsWorkingTree::new(vault.root()),
        held: "a.md",
    };
    let report = Recovery::replay(&vault, &tree).unwrap();
    assert_eq!(report.unsettled.len(), 1);
    assert_eq!(report.unsettled[0].seq, open);

    // The held path refuses records; other paths take them.
    let guard = PublisherLock::acquire(&vault).unwrap();
    let rec = intent(&vault, "a.md", b"a3", "cli", None);
    assert!(matches!(
        Manifest::append_intent(&vault, &guard, rec),
        Err(HistoryError::OpenIntent { seq }) if seq == open
    ));
    let mut moved = intent(&vault, "c.md", b"a3", "cli", None);
    moved.moved_from = Some("a.md".into());
    assert!(matches!(
        Manifest::append_intent(&vault, &guard, moved),
        Err(HistoryError::OpenIntent { seq }) if seq == open
    ));
    drop(guard);
    let b = save(&vault, "b.md", b"b1", "cli", None);
    let i = ingest(&vault, "d.md", b"d1", None);

    // Records after the held intent commit; nothing at or after it retires.
    let flush = Flusher::flush(&vault).unwrap();
    assert!(matches!(
        flush.blocked,
        Some(wi_history::FlushBlock::OpenIntent { seq }) if seq == open
    ));
    assert_eq!(all_seqs(vault.root()), vec![1, b, i]);
    assert_eq!(flush.retired_upto, 1);
    let again = Flusher::flush(&vault).unwrap();
    assert!(again.commits.is_empty(), "committed once: {again:?}");
    assert_eq!(again.already_committed, vec![b, i]);

    // Once readable, replay settles it, commits it and retires everything.
    let report = replay(&vault);
    assert_eq!(report.completed, vec![open]);
    assert_eq!(report.flush.remaining, 0);
    assert_eq!(report.flush.retired_upto, i);
    assert_eq!(all_seqs(vault.root()), vec![1, b, i, open]);
    let commits = commits(vault.root());
    assert_eq!(show(vault.root(), &commits[3].0, "a.md"), b"a2");
    assert_eq!(show(vault.root(), &commits[3].0, "b.md"), b"b1");
    save(&vault, "a.md", b"a3", "cli", None);
    fsck(vault.root());
}
