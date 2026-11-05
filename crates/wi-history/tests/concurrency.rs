mod common;

use std::collections::BTreeMap;

use common::*;
use wi_history::{Flusher, FsWorkingTree, Manifest, OpStatus, PublisherLock, Recovery};

#[test]
fn two_processes_append_concurrently() {
    let (_dir, vault) = new_vault();
    let root = vault.root().to_str().unwrap().to_owned();
    const N: usize = 40;
    let children: Vec<_> = ["cli", "mcp"]
        .iter()
        .map(|actor| {
            testproc()
                .args(["append", &root, actor, &N.to_string()])
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut acked: Vec<u64> = Vec::new();
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        acked.extend(
            String::from_utf8(out.stdout)
                .unwrap()
                .lines()
                .map(|l| l.parse::<u64>().unwrap()),
        );
    }
    acked.sort_unstable();
    assert_eq!(
        acked,
        (1..=(2 * N) as u64).collect::<Vec<_>>(),
        "distinct seqs"
    );

    // A strict read validates every frame checksum.
    let snap = {
        let guard = PublisherLock::acquire(&vault).unwrap();
        Manifest::snapshot(&vault, &guard).unwrap()
    };
    assert_eq!(snap.entries.len(), 2 * N);
    for entry in &snap.entries {
        assert_eq!(entry.status, OpStatus::Done);
        assert_eq!(
            wi_history::hash_bytes(&vault.read_blob(entry.op.new_blob()).unwrap()),
            entry.op.new_hash()
        );
    }

    let report = Flusher::flush(&vault).unwrap();
    assert_eq!(report.commits.len(), 2 * N);
    let seqs = all_seqs(vault.root());
    assert_eq!(
        seqs,
        (1..=(2 * N) as u64).collect::<Vec<_>>(),
        "committed once, in order"
    );
    // Each actor's own saves stay in its order.
    let mut per_actor: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (_, msg) in commits(vault.root()) {
        let actor = trailer(&msg, "Wi-Actor")[0].to_owned();
        per_actor
            .entry(actor)
            .or_default()
            .push(trailer(&msg, "Wi-Operation")[0].to_owned());
    }
    for (actor, ops) in per_actor {
        let expected: Vec<String> = (0..N).map(|i| format!("{actor}-{i} d{i}")).collect();
        assert_eq!(ops, expected);
    }
    let again = Recovery::replay(&vault, &FsWorkingTree::new(vault.root())).unwrap();
    assert!(again.flush.commits.is_empty());
    fsck(vault.root());
}

#[test]
fn concurrent_flushes_commit_each_record_once() {
    let (_dir, vault) = new_vault();
    let root = vault.root().to_str().unwrap().to_owned();
    let writer = testproc()
        .args(["append", &root, "cli", "30"])
        .spawn()
        .unwrap();
    let flushers: Vec<_> = (0..3)
        .map(|_| {
            testproc()
                .args(["flush", &root])
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    assert!(writer.wait_with_output().unwrap().status.success());
    for f in flushers {
        assert!(f.wait_with_output().unwrap().status.success());
    }
    Flusher::flush(&vault).unwrap();
    assert_eq!(all_seqs(vault.root()), (1..=30).collect::<Vec<_>>());
}

#[test]
fn record_appended_during_flush_survives() {
    let (_dir, vault) = new_vault();
    save(&vault, "a.md", b"1", "cli", None);
    save(&vault, "b.md", b"2", "cli", None);
    let gate = tempfile::tempdir().unwrap();
    let root = vault.root().to_str().unwrap().to_owned();
    let child = testproc()
        .args(["flush", &root])
        .env(
            "WI_FAULT",
            format!("pause_after_snapshot:{}", gate.path().display()),
        )
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    while !gate.path().join("ready").exists() {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // The flush holds only the flush lock now; a save proceeds.
    let late = save(&vault, "c.md", b"3", "mcp", None);
    std::fs::write(gate.path().join("go"), b"").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["retired_upto"], 2);
    assert_eq!(report["commits"].as_array().unwrap().len(), 2);

    let snap = {
        let guard = PublisherLock::acquire(&vault).unwrap();
        Manifest::snapshot(&vault, &guard).unwrap()
    };
    assert_eq!(snap.retired_upto, 2);
    assert_eq!(snap.entries.len(), 1);
    assert_eq!(snap.entries[0].seq, late);
    assert_eq!(snap.entries[0].status, OpStatus::Done);
    Flusher::flush(&vault).unwrap();
    assert_eq!(all_seqs(vault.root()), vec![1, 2, 3]);
}
