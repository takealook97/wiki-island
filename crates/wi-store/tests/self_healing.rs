//! States that used to need a person: owner deletions and renames, a
//! crashed writer's temp file, work items whose cause is gone, a replaced
//! index and the skills change notice.

mod common;

use std::fs;

use common::{Fixture, assert_deletion_committed, git, write};
use wi_history::{Blob, IntentRecord, Manifest, PublisherLock, hash_bytes};
use wi_store::{Ingress, MaintainOptions, OpenOptions, Query, SearchOptions, Vault, WriteRequest};

fn hits(vault: &Vault, word: &str) -> Vec<String> {
    vault
        .search(&Query::text(word), SearchOptions::default())
        .unwrap()
        .hits
        .iter()
        .map(|h| h.rel_path.to_string_lossy().into_owned())
        .collect()
}

fn open_work(vault: &Vault, kind: &str) -> Vec<String> {
    vault
        .work_list()
        .unwrap()
        .into_iter()
        .filter(|w| w.work_kind.as_deref() == Some(kind) && w.state.as_deref() != Some("closed"))
        .map(|w| w.target.unwrap_or_default())
        .collect()
}

fn full() -> MaintainOptions {
    MaintainOptions {
        full: true,
        ..MaintainOptions::default()
    }
}

#[test]
fn an_owner_deletion_seen_by_the_watcher_is_committed_without_work() {
    let (_fx, vault) = Fixture::new();
    write(&vault, "memory/meeting.md", "notes\n", Ingress::Cli);
    vault.flush().unwrap();
    fs::remove_file(vault.root().join("memory/meeting.md")).unwrap();
    // What the watcher does for one event, then its flush.
    vault.ingest_path("memory/meeting.md").unwrap();
    vault.flush().unwrap();
    assert_deletion_committed(&vault, "memory/meeting.md");
    assert_eq!(git(vault.root(), &["status", "--porcelain"]), "");
    vault.maintain(&full()).unwrap();
    assert!(vault.work_list().unwrap().is_empty());
}

#[test]
fn an_owner_rename_is_committed_as_a_rename_without_work() {
    let (_fx, vault) = Fixture::new();
    write(
        &vault,
        "procedures/release.md",
        "bump, tag, push\n",
        Ingress::Cli,
    );
    vault.flush().unwrap();
    fs::rename(
        vault.root().join("procedures/release.md"),
        vault.root().join("procedures/Release-process.md"),
    )
    .unwrap();
    vault.ingest_path("procedures/Release-process.md").unwrap();
    vault.ingest_path("procedures/release.md").unwrap();
    vault.flush().unwrap();
    let log = git(vault.root(), &["log", "--format=%B", "-1"]);
    assert!(
        log.contains("Wi-Renamed: procedures/release.md -> procedures/Release-process.md"),
        "{log}"
    );
    assert_eq!(git(vault.root(), &["status", "--porcelain"]), "");
    assert!(vault.work_requests().unwrap().is_empty());
}

#[test]
fn deletions_no_watcher_saw_are_committed_by_a_full_maintenance_pass() {
    let (_fx, vault) = Fixture::new();
    write(&vault, "a.md", "a\n", Ingress::Cli);
    write(&vault, "b.md", "b\n", Ingress::Cli);
    vault.flush().unwrap();
    fs::remove_file(vault.root().join("a.md")).unwrap();
    fs::remove_file(vault.root().join("b.md")).unwrap();
    vault.maintain(&full()).unwrap();
    let log = git(vault.root(), &["log", "--format=%B"]);
    assert!(
        log.contains("Wi-Deleted: a.md") && log.contains("Wi-Deleted: b.md"),
        "{log}"
    );
    assert_eq!(git(vault.root(), &["status", "--porcelain"]), "");
    assert!(vault.work_requests().unwrap().is_empty());
}

fn open_intent(vault: &Vault, path: &str, new: &[u8]) {
    let h = vault.history_vault();
    let blob = Blob::write_durable(h, new).unwrap();
    let guard = PublisherLock::acquire(h).unwrap();
    Manifest::append_intent(
        h,
        &guard,
        IntentRecord {
            op_id: None,
            request_digest: hash_bytes(b"crash"),
            actor: "mcp".into(),
            path: path.into(),
            moved_from: None,
            old_hash: None,
            new_hash: hash_bytes(new),
            new_blob: blob,
            revert_of: None,
            batch: None,
            losses: vec![],
        },
    )
    .unwrap();
}

#[test]
fn maintenance_removes_a_dead_writers_temp_but_keeps_one_an_open_intent_names() {
    let (_fx, vault) = Fixture::new();
    // pid 2147483647 is never a running process.
    let dead = vault
        .root()
        .join("agent/.k27.md.wi-tmp-2147483647-27-400071000");
    fs::create_dir_all(dead.parent().unwrap()).unwrap();
    fs::write(&dead, b"half a write").unwrap();
    // A live writer's temp stays too.
    let live = vault
        .root()
        .join(format!(".live.md.wi-tmp-{}-1", std::process::id()));
    fs::write(&live, b"in flight").unwrap();
    vault.maintain(&MaintainOptions::default()).unwrap();
    assert!(!dead.exists(), "a dead writer's temp is removed");
    assert!(live.exists(), "a running writer's temp is kept");

    open_intent(&vault, "held.md", b"new bytes");
    let held = vault.root().join(".held.md.wi-tmp-2147483647-3");
    fs::write(&held, b"new bytes").unwrap();
    vault.maintain(&MaintainOptions::default()).unwrap();
    assert!(held.exists(), "an open intent's temp is kept for recovery");
}

#[test]
fn maintenance_closes_a_gate_item_once_its_note_is_active() {
    let (_fx, vault) = Fixture::new();
    let wrong = "---\ntype: memory\nsources: [{ref: notes/x.md, source_hash: \"sha256:00\"}]\n---\nThe port is 443.\n";
    let first = vault
        .lifecycle_write(WriteRequest {
            path: "memory/port.md".into(),
            content: wrong.as_bytes().to_vec(),
            expected_checksum: None,
            op_id: None,
            ingress: Ingress::Cli,
        })
        .unwrap();
    assert_eq!(open_work(&vault, "gate"), ["memory/port.md"]);
    // The owner fixes the note: it adopts.
    let current = fs::read(vault.root().join("memory/port.md")).unwrap();
    let fixed = String::from_utf8(current).unwrap().replace(
        "sources: [{ref: notes/x.md, source_hash: \"sha256:00\"}]\n",
        "",
    );
    let second = vault
        .lifecycle_write(WriteRequest {
            path: "memory/port.md".into(),
            content: fixed.into_bytes(),
            expected_checksum: Some(first_checksum(&vault, "memory/port.md")),
            op_id: None,
            ingress: Ingress::Cli,
        })
        .unwrap();
    let _ = first;
    assert_eq!(
        second.lifecycle.unwrap()["adoption"]["outcome"],
        "active",
        "the fixed note adopts"
    );
    let report = vault.maintain(&MaintainOptions::default()).unwrap();
    assert_eq!(report.work_closed, 1, "{report:?}");
    assert!(open_work(&vault, "gate").is_empty());
    // A second pass changes nothing.
    assert_eq!(
        vault
            .maintain(&MaintainOptions::default())
            .unwrap()
            .work_closed,
        0
    );
}

fn first_checksum(vault: &Vault, path: &str) -> String {
    hash_bytes(&fs::read(vault.root().join(path)).unwrap())
}

#[test]
fn maintenance_closes_a_gate_item_whose_note_was_deleted() {
    let (_fx, vault) = Fixture::new();
    vault
        .lifecycle_write(WriteRequest {
            path: "memory/ghost.md".into(),
            content: b"---\ntype: memory\nsources: [notes/absent.md]\n---\nGhost.\n".to_vec(),
            expected_checksum: None,
            op_id: None,
            ingress: Ingress::Mcp,
        })
        .unwrap();
    assert_eq!(open_work(&vault, "gate"), ["memory/ghost.md"]);
    fs::remove_file(vault.root().join("memory/ghost.md")).unwrap();
    vault.maintain(&full()).unwrap();
    assert!(open_work(&vault, "gate").is_empty());
    assert_eq!(git(vault.root(), &["status", "--porcelain"]), "");
}

#[test]
#[cfg(unix)]
fn a_running_handle_finds_notes_after_another_process_replaced_the_index() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    let index = tmp.path().join("cache/index.sqlite3");
    let options = OpenOptions {
        index_path: Some(index.clone()),
    };
    let (running, _) = Vault::init_with(&root, options.clone()).unwrap();
    write(&running, "before.md", "originalneedle\n", Ingress::Cli);
    running.flush().unwrap();
    assert_eq!(hits(&running, "originalneedle"), ["before.md"]);
    let mark = running.index_epoch();
    assert!(!running.index_reopened_since(mark));

    // Another process deletes the cache and rebuilds it, then writes.
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", index.display()));
    }
    let other = Vault::open_with(&root, options).unwrap();
    other.reindex().unwrap();
    write(&other, "external.md", "externalnewneedle\n", Ingress::Cli);
    other.flush().unwrap();
    drop(other);

    assert!(running.index_reopened_since(mark));
    assert_eq!(hits(&running, "externalnewneedle"), ["external.md"]);
    assert_eq!(hits(&running, "originalneedle"), ["before.md"]);
}

#[test]
#[cfg(windows)]
fn an_open_index_blocks_deletion_and_reopens_after_a_rebuild() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    let index = tmp.path().join("cache/index.sqlite3");
    let options = OpenOptions {
        index_path: Some(index.clone()),
    };
    let (running, _) = Vault::init_with(&root, options.clone()).unwrap();
    write(&running, "before.md", "originalneedle\n", Ingress::Cli);
    running.flush().unwrap();
    assert_eq!(hits(&running, "originalneedle"), ["before.md"]);
    let mark = running.index_epoch();

    // SQLite's Windows VFS does not share deletion of its open database.
    let error = fs::remove_file(&index).unwrap_err();
    assert!(matches!(error.raw_os_error(), Some(5 | 32 | 33)), "{error}");
    assert!(!running.index_reopened_since(mark));
    assert_eq!(hits(&running, "originalneedle"), ["before.md"]);

    running.close_index();
    fs::remove_file(&index).unwrap();
    for suffix in ["-wal", "-shm"] {
        let path = format!("{}{suffix}", index.display());
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("{error}"),
        }
    }
    let other = Vault::open_with(&root, options).unwrap();
    other.reindex().unwrap();
    write(&other, "external.md", "externalnewneedle\n", Ingress::Cli);
    other.flush().unwrap();
    drop(other);

    assert!(running.index_reopened_since(mark));
    assert_eq!(hits(&running, "externalnewneedle"), ["external.md"]);
    assert_eq!(hits(&running, "originalneedle"), ["before.md"]);
}

fn procedure(steps: &str) -> String {
    format!(
        "---\ntype: memory\nkind: procedure\nskill_id: release\nsummary: Cut a release\n---\n{steps}\n"
    )
}

#[test]
fn the_procedures_digest_follows_procedures_and_maintenance_never_consumes_it() {
    let (_fx, vault) = Fixture::new();
    vault
        .lifecycle_write(WriteRequest {
            path: "procedures/release.md".into(),
            content: procedure("original-steps").into_bytes(),
            expected_checksum: None,
            op_id: None,
            ingress: Ingress::Cli,
        })
        .unwrap();
    let synced = vault.procedures_digest().unwrap();
    assert_eq!(vault.procedures_digest().unwrap(), synced, "stable");
    let checksum = first_checksum(&vault, "procedures/release.md");
    let current = String::from_utf8(fs::read(vault.root().join("procedures/release.md")).unwrap())
        .unwrap()
        .replace("original-steps", "latest-steps");
    vault
        .lifecycle_write(WriteRequest {
            path: "procedures/release.md".into(),
            content: current.into_bytes(),
            expected_checksum: Some(checksum),
            op_id: None,
            ingress: Ingress::Cli,
        })
        .unwrap();
    // One process's maintenance pass sees the change first ...
    let report = vault.maintain(&MaintainOptions::default()).unwrap();
    assert!(report.procedures_changed);
    // ... and any other process still sees a digest that differs from
    // what it last synced.
    let other = vault.procedures_digest().unwrap();
    assert_ne!(other, synced);
    vault.maintain(&MaintainOptions::default()).unwrap();
    assert_eq!(vault.procedures_digest().unwrap(), other);
}
