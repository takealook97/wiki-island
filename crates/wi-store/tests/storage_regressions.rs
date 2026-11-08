//! Regression tests for core data-path defects: ignore rules on file
//! ingress, move-source crash recovery, permission bits of replacements,
//! ingress revalidation under the publisher lock, watcher startup cancellation
//! and the pending index marker.

mod common;

use std::cell::Cell;
use std::fs;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use common::{Fixture, git};
use wi_history::{Blob, IntentRecord, Manifest, PublisherLock, hash_bytes};
#[cfg(unix)]
use wi_store::MoveRequest;
use wi_store::{Hooks, IngestOutcome, Ingress, Vault, WatchTuning, WorkKind, WriteRequest};

fn skipped(vault: &Vault, rel: &str) -> bool {
    matches!(
        vault.ingest_path(rel).unwrap().outcome,
        IngestOutcome::Skipped { .. }
    )
}

fn manifest_paths(vault: &Vault) -> Vec<String> {
    let guard = PublisherLock::acquire(vault.history_vault()).unwrap();
    Manifest::snapshot(vault.history_vault(), &guard)
        .unwrap()
        .entries
        .iter()
        .map(|e| e.op.path().to_owned())
        .collect()
}

// Ignore rules.

#[test]
fn file_ingress_honours_nested_and_dot_ignore_files() {
    let (fx, vault) = Fixture::new();
    let root = vault.root().to_path_buf();
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::create_dir_all(root.join("drafts")).unwrap();
    fs::write(root.join("nested/.gitignore"), "private.md\n/anchored.md\n").unwrap();
    fs::write(root.join(".ignore"), "scratch.md\n").unwrap();
    let mut top = fs::read_to_string(root.join(".gitignore")).unwrap();
    top.push_str("root-private.md\ndrafts/\n");
    fs::write(root.join(".gitignore"), top).unwrap();
    // A deeper whitelist cannot re-include a file below an ignored directory.
    fs::write(root.join("drafts/.gitignore"), "!kept.md\n").unwrap();
    for rel in [
        "nested/private.md",
        "nested/anchored.md",
        "scratch.md",
        "root-private.md",
        "drafts/kept.md",
    ] {
        fs::write(root.join(rel), format!("ignored draft {rel}\n")).unwrap();
        assert!(skipped(&vault, rel), "{rel} must be skipped");
    }
    // Not ignored: an anchored rule applies only in its own directory.
    fs::write(root.join("anchored.md"), "kept\n").unwrap();
    assert!(!skipped(&vault, "anchored.md"));
    // A full scan agrees with event ingress.
    let report = vault.reconcile().unwrap();
    let ingested: Vec<&str> = report.ingested.iter().map(|r| r.path.as_str()).collect();
    for rel in [
        "nested/private.md",
        "nested/anchored.md",
        "scratch.md",
        "root-private.md",
        "drafts/kept.md",
    ] {
        assert!(!ingested.contains(&rel), "scan ingested {rel}");
    }
    vault.flush().unwrap();
    let tracked = git(&fx.root_input(), &["ls-tree", "-r", "--name-only", "HEAD"]);
    for rel in [
        "nested/private.md",
        "nested/anchored.md",
        "scratch.md",
        "root-private.md",
        "drafts/kept.md",
    ] {
        assert!(!tracked.lines().any(|l| l == rel), "{rel} was committed");
    }
}

// Move-source recovery.

/// Persist an open move intent `from` -> `to` with the target bytes, as a
/// writer that crashed after placing the target leaves it.
fn crashed_move(vault: &Vault, from: &str, to: &str, bytes: &[u8]) -> u64 {
    let history = vault.history_vault();
    let blob = Blob::write_durable(history, bytes).unwrap();
    let guard = PublisherLock::acquire(history).unwrap();
    let seq = Manifest::append_intent(
        history,
        &guard,
        IntentRecord {
            op_id: None,
            request_digest: hash_bytes(b"crashed move"),
            actor: "cli".into(),
            path: to.into(),
            moved_from: Some(from.into()),
            old_hash: None,
            new_hash: hash_bytes(bytes),
            new_blob: blob,
            revert_of: None,
            batch: None,
            losses: vec![],
        },
    )
    .unwrap();
    fs::write(vault.root().join(to), bytes).unwrap();
    seq
}

fn kinds(vault: &Vault, kind: WorkKind) -> usize {
    vault
        .work_requests()
        .unwrap()
        .into_iter()
        .filter(|r| r.kind == kind)
        .count()
}

#[test]
fn recovery_retires_a_binary_attachment_move_source() {
    let (fx, vault) = Fixture::new();
    let root = vault.root().to_path_buf();
    let bytes = b"\x89PNG\r\n\x1a\n\xff\xfe";
    fs::write(root.join("a.png"), bytes).unwrap();
    vault.ingest_path("a.png").unwrap();
    vault.flush().unwrap();
    crashed_move(&vault, "a.png", "b.png", bytes);
    drop(vault);
    let vault = fx.open();
    assert!(!root.join("a.png").exists());
    assert_eq!(fs::read(root.join("b.png")).unwrap(), bytes);
    assert_eq!(kinds(&vault, WorkKind::RecoveryProblem), 0);
    assert_eq!(kinds(&vault, WorkKind::ConflictCopy), 0);
}

#[test]
fn recovery_keeps_a_utf8_attachment_move_byte_for_byte() {
    let (fx, vault) = Fixture::new();
    let root = vault.root().to_path_buf();
    let bytes = b"plain text attachment, no frontmatter\n";
    fs::write(root.join("a.txt"), bytes).unwrap();
    vault.ingest_path("a.txt").unwrap();
    vault.flush().unwrap();
    let seq = crashed_move(&vault, "a.txt", "b.txt", bytes);
    drop(vault);
    let vault = fx.open();
    assert!(!root.join("a.txt").exists());
    assert!(!root.join(format!(".a.txt.wi-conflict-{seq}")).exists());
    assert_eq!(kinds(&vault, WorkKind::ConflictCopy), 0);
}

#[test]
fn recovery_settles_a_move_source_taken_aside() {
    let (fx, vault) = Fixture::new();
    let root = vault.root().to_path_buf();
    let old = b"---\npermalink: a\n---\nold draft\n";
    fs::write(root.join("a.md"), old).unwrap();
    vault.ingest_path("a.md").unwrap();
    vault.flush().unwrap();
    let seq = crashed_move(&vault, "a.md", "b.md", old);
    // An editor saved after the source check; the crash came right after
    // the source was taken aside.
    let aside = root.join(format!(".a.md.wi-moved-{seq}"));
    fs::write(root.join("a.md"), "new editor draft\n").unwrap();
    fs::rename(root.join("a.md"), &aside).unwrap();
    drop(vault);
    let vault = fx.open();
    let copy = root.join(format!(".a.md.wi-conflict-{seq}"));
    assert!(!aside.exists());
    assert_eq!(fs::read(&copy).unwrap(), b"new editor draft\n");
    assert_eq!(kinds(&vault, WorkKind::ConflictCopy), 1);
}

#[test]
fn recovery_removes_an_unchanged_move_source_taken_aside() {
    let (fx, vault) = Fixture::new();
    let root = vault.root().to_path_buf();
    let old = b"---\npermalink: a\n---\nold draft\n";
    fs::write(root.join("a.md"), old).unwrap();
    vault.ingest_path("a.md").unwrap();
    vault.flush().unwrap();
    let seq = crashed_move(&vault, "a.md", "b.md", old);
    let aside = root.join(format!(".a.md.wi-moved-{seq}"));
    fs::rename(root.join("a.md"), &aside).unwrap();
    // A new file at the source name after the aside rename is the owner's.
    fs::write(root.join("a.md"), "a new note\n").unwrap();
    drop(vault);
    let vault = fx.open();
    assert!(!aside.exists());
    assert!(!root.join(format!(".a.md.wi-conflict-{seq}")).exists());
    assert_eq!(fs::read(root.join("a.md")).unwrap(), b"a new note\n");
    assert_eq!(kinds(&vault, WorkKind::ConflictCopy), 0);
}

// Permission preservation.

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[cfg(unix)]
#[test]
fn replacements_keep_permission_bits() {
    use std::os::unix::fs::PermissionsExt;
    let (_fx, vault) = Fixture::new();
    let root = vault.root().to_path_buf();
    let first = common::write(&vault, "a.md", "old\n", Ingress::Cli);
    fs::set_permissions(root.join("a.md"), fs::Permissions::from_mode(0o640)).unwrap();
    vault
        .write(WriteRequest {
            path: "a.md".into(),
            content: b"new\n".to_vec(),
            expected_checksum: Some(first.checksum),
            op_id: None,
            ingress: Ingress::Cli,
        })
        .unwrap();
    assert_eq!(mode(&root.join("a.md")), 0o640);
    fs::set_permissions(root.join("a.md"), fs::Permissions::from_mode(0o604)).unwrap();
    vault
        .move_note(MoveRequest {
            from: "a.md".into(),
            to: "b.md".into(),
            expected_checksum: None,
            op_id: None,
            ingress: Ingress::Cli,
        })
        .unwrap();
    assert_eq!(mode(&root.join("b.md")), 0o604);
}

// Ingress revalidation.

#[test]
fn ingress_revalidates_under_the_publisher_lock() {
    let (fx, mut vault) = Fixture::new();
    let root = vault.root().to_path_buf();
    common::write(&vault, "a.md", "engine version\n", Ingress::Cli);
    vault.flush().unwrap();
    fs::write(root.join("a.md"), "editor version\n").unwrap();
    // Another writer publishes while ingress waits for the lock.
    let writer = fx.open();
    let once = Cell::new(false);
    vault.set_hooks(Hooks {
        before_ingest_lock: Some(Rc::new(move |_| {
            if !once.replace(true) {
                writer
                    .write(WriteRequest {
                        path: "a.md".into(),
                        content: b"newer engine version\n".to_vec(),
                        expected_checksum: Some(hash_bytes(b"editor version\n")),
                        op_id: None,
                        ingress: Ingress::Cli,
                    })
                    .unwrap();
            }
        })),
        ..Hooks::default()
    });
    let report = vault.ingest_path("a.md").unwrap();
    assert_eq!(report.outcome, IngestOutcome::Unchanged);
    vault.flush().unwrap();
    assert_eq!(
        git(&fx.root_input(), &["show", "HEAD:a.md"]),
        fs::read_to_string(root.join("a.md")).unwrap()
    );
}

// Watcher startup.

#[test]
fn a_timed_out_watcher_start_never_takes_the_lease() {
    let (_fx, vault) = Fixture::new();
    let root = vault.root().to_path_buf();
    let lease = root.join(".git/wi/watcher-lease");
    let guard = PublisherLock::acquire(vault.history_vault()).unwrap();
    let tuning = WatchTuning {
        startup_timeout: Duration::from_millis(300),
        ..WatchTuning::default()
    };
    assert!(vault.watch_tuned(tuning).is_err());
    drop(guard);
    // The cancelled thread may now open the vault; it must not start.
    let until = Instant::now() + Duration::from_secs(3);
    let mut wrote = false;
    while Instant::now() < until {
        assert!(!lease.exists(), "a cancelled watcher took the lease");
        if !wrote && Instant::now() + Duration::from_secs(2) > until {
            fs::write(root.join("late.md"), "late\n").unwrap();
            wrote = true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!manifest_paths(&vault).iter().any(|p| p == "late.md"));
}

// Pending index repair.

#[test]
fn index_repair_keeps_paths_marked_after_it_read_the_marker() {
    let (_fx, mut vault) = Fixture::new();
    let marker = vault.root().join(".git/wi/index-pending");
    common::write(&vault, "a.md", "a\n", Ingress::Cli);
    fs::write(&marker, "a.md\n").unwrap();
    let calls = Cell::new(0);
    vault.set_hooks(Hooks {
        after_index_repair: Some(Rc::new(move |marker: &Path| {
            use std::io::Write;
            calls.set(calls.get() + 1);
            let mut f = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(marker)
                .unwrap();
            writeln!(f, "late-{}.md", calls.get()).unwrap();
        })),
        ..Hooks::default()
    });
    let report = vault.recover().unwrap();
    assert_eq!(report.index_repaired, vec!["a.md".to_owned()]);
    assert_eq!(vault.index_pending(), vec!["late-1.md".to_owned()]);
    // A rebuild keeps a path marked while it ran the same way.
    vault.reindex_with_missing().unwrap();
    assert_eq!(vault.index_pending(), vec!["late-2.md".to_owned()]);
}

/// The owner deletes a note after an agent's checksum check but before the
/// swap: the replacement is stale and the deletion stands.
#[test]
fn deletion_after_the_checksum_check_is_never_undone_by_a_replacement() {
    let (_f, mut vault) = Fixture::new();
    common::write(&vault, "same.md", "owner-original\n", Ingress::Cli);
    vault.flush().unwrap();
    let expected = hash_bytes(&fs::read(vault.root().join("same.md")).unwrap());
    vault.set_hooks(Hooks {
        before_swap: Some(Rc::new(|target: &Path| {
            fs::remove_file(target).unwrap();
        })),
        ..Hooks::default()
    });
    let result = vault.write(WriteRequest {
        path: "same.md".into(),
        content: b"agent-replacement\n".to_vec(),
        expected_checksum: Some(expected),
        op_id: None,
        ingress: Ingress::Mcp,
    });
    vault.set_hooks(Hooks::default());
    let err = result.expect_err("a replacement must not recreate a deleted note");
    assert_eq!(err.code(), "stale", "{err:?}");
    assert!(
        matches!(&err, wi_store::StoreError::Stale { path, .. } if path == "same.md"),
        "the error names the vault-relative path: {err:?}"
    );
    assert!(!vault.root().join("same.md").exists());
    // No temp file is left behind.
    let leftovers: Vec<_> = fs::read_dir(vault.root())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".wi-tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    vault.ingest_path("same.md").unwrap();
    vault.flush().unwrap();
    assert_eq!(
        git(vault.root(), &["status", "--porcelain", "--", "same.md"]),
        ""
    );
    assert!(git(vault.root(), &["log", "--format=%B"]).contains("Wi-Deleted: same.md"));
}
