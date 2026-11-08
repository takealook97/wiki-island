//! A held recovery intent blocks writes to its own paths only: writes to
//! other paths are published, flushed, committed and searchable, and the
//! held path settles by itself once readable again.
#![cfg(unix)]

mod common;

use common::Fixture;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use wi_history::{Blob, IntentRecord, Manifest, PublisherLock, hash_bytes};
use wi_index::Query;
use wi_store::{Ingress, MoveRequest, SearchOptions, StoreError, Vault, WorkKind, WriteRequest};

fn crashed_intent(vault: &Vault, path: &str, old: &str, new: &[u8]) -> u64 {
    let h = vault.history_vault();
    let blob = Blob::write_durable(h, new).unwrap();
    let guard = PublisherLock::acquire(h).unwrap();
    Manifest::append_intent(
        h,
        &guard,
        IntentRecord {
            op_id: None,
            request_digest: hash_bytes(b"crash"),
            actor: "cli".into(),
            path: path.into(),
            moved_from: None,
            old_hash: Some(old.to_owned()),
            new_hash: hash_bytes(new),
            new_blob: blob,
            revert_of: None,
            batch: None,
            losses: vec![],
        },
    )
    .unwrap()
}

fn write(vault: &Vault, path: &str, content: &str) -> wi_store::Result<wi_store::WriteResult> {
    vault.write(WriteRequest {
        path: path.into(),
        content: content.as_bytes().to_vec(),
        expected_checksum: None,
        op_id: None,
        ingress: Ingress::Cli,
    })
}

fn hits(vault: &Vault, word: &str) -> Vec<String> {
    vault
        .search(&Query::text(word), SearchOptions::default())
        .unwrap()
        .hits
        .iter()
        .map(|h| h.rel_path.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn held_intent_blocks_only_its_paths_and_settles_on_next_open() {
    let (fx, vault) = Fixture::new();
    let old = common::write(&vault, "a.md", "old alphaword", Ingress::Cli).checksum;
    vault.flush().unwrap();
    let seq = crashed_intent(&vault, "a.md", &old, b"new alphaword");
    let a = vault.root().join("a.md");
    fs::set_permissions(&a, fs::Permissions::from_mode(0o000)).unwrap();
    drop(vault);

    let vault = fx.open();
    assert_eq!(vault.open_report().recovery.unsettled.len(), 1);
    assert!(
        vault
            .work_requests()
            .unwrap()
            .iter()
            .any(|w| w.kind == WorkKind::RecoveryProblem && w.seq == Some(seq)),
        "the held intent has its recovery-problem request"
    );

    // An unrelated write is published, flushed, committed and searchable.
    let b = write(&vault, "b.md", "bravoword").expect("unrelated write proceeds");
    let flush = vault.flush().unwrap();
    assert!(
        flush.commits.iter().any(|c| c.seqs.contains(&b.seq)),
        "{flush:?}"
    );
    let log = common::git(vault.root(), &["log", "--format=%s", "--", "b.md"]);
    assert!(log.contains("b.md"), "{log}");
    assert_eq!(hits(&vault, "bravoword"), vec!["b.md".to_owned()]);

    // A write or move touching the held path fails fast with a stable code.
    let err = write(&vault, "a.md", "again").unwrap_err();
    assert_eq!(err.code(), "recovery-held", "{err}");
    assert!(
        matches!(&err, StoreError::RecoveryHeld { path, seq: s } if path == "a.md" && *s == seq)
    );
    let err = vault
        .move_note(MoveRequest {
            from: "b.md".into(),
            to: "a.md".into(),
            expected_checksum: Some(b.checksum.clone()),
            op_id: None,
            ingress: Ingress::Cli,
        })
        .unwrap_err();
    assert_eq!(err.code(), "recovery-held", "{err}");
    // Watcher-style ingress skips the held path instead of failing.
    vault.reconcile().unwrap();
    drop(vault);

    // Restored permissions: the next open settles a.md automatically.
    fs::set_permissions(&a, fs::Permissions::from_mode(0o600)).unwrap();
    let vault = fx.open();
    assert!(vault.open_report().recovery.unsettled.is_empty());
    assert_eq!(vault.open_report().recovery.dropped, vec![seq]);
    assert_eq!(vault.open_report().recovery.flush.remaining, 0);
    vault
        .write(WriteRequest {
            path: "a.md".into(),
            content: b"after charlieword".to_vec(),
            expected_checksum: Some(old),
            op_id: None,
            ingress: Ingress::Cli,
        })
        .expect("a.md writable again");
    vault.flush().unwrap();
    assert_eq!(hits(&vault, "charlieword"), vec!["a.md".to_owned()]);
}

#[test]
fn held_intent_heals_in_a_live_handle_on_the_next_write_to_its_path() {
    let (fx, vault) = Fixture::new();
    let old = common::write(&vault, "a.md", "old", Ingress::Cli).checksum;
    let seq = crashed_intent(&vault, "a.md", &old, b"new");
    let a = vault.root().join("a.md");
    fs::set_permissions(&a, fs::Permissions::from_mode(0o000)).unwrap();
    drop(vault);
    let vault = fx.open();
    let err = write(&vault, "a.md", "x").unwrap_err();
    assert!(matches!(err, StoreError::RecoveryHeld { seq: s, .. } if s == seq));
    fs::set_permissions(&a, fs::Permissions::from_mode(0o600)).unwrap();
    // The write retries recovery once before refusing, so it now succeeds
    // in the same handle (no reopen needed).
    vault
        .write(WriteRequest {
            path: "a.md".into(),
            content: b"healed".to_vec(),
            expected_checksum: Some(old),
            op_id: None,
            ingress: Ingress::Cli,
        })
        .expect("settled by the write's recovery retry");
}
