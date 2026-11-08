//! Recovery and durability: no single bad path
//! blocks `Vault::open`, and editor bytes are never deleted or left hidden
//! without a work request.

mod common;

use common::Fixture;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::{fs, rc::Rc};
use wi_history::{Blob, IntentRecord, Manifest, PublisherLock, hash_bytes};
use wi_store::{
    Hooks, Ingress, MoveRequest, OpenOptions, Vault, WatchTuning, WorkKind, WriteRequest,
};

#[test]
fn atomic_editor_save_at_move_source_before_placement_makes_the_move_stale() {
    let (_fx, mut vault) = Fixture::new();
    let first = common::write(&vault, "a.md", "old", Ingress::Cli);
    let source = vault.root().join("a.md");
    let temporary = vault.root().join("editor-save.md");
    vault.set_hooks(Hooks {
        before_swap: Some(Rc::new(move |_| {
            fs::write(&temporary, b"atomic editor source save").unwrap();
            fs::rename(&temporary, &source).unwrap();
        })),
        ..Hooks::default()
    });
    let result = vault.move_note(MoveRequest {
        from: "a.md".into(),
        to: "b.md".into(),
        expected_checksum: Some(first.checksum),
        op_id: None,
        ingress: Ingress::Cli,
    });
    vault.set_hooks(Hooks::default());
    assert!(
        matches!(&result, Err(wi_store::StoreError::Stale { path, .. }) if path == "a.md"),
        "{result:?}"
    );
    assert_eq!(
        fs::read(vault.root().join("a.md")).unwrap(),
        b"atomic editor source save"
    );
    assert!(!vault.root().join("b.md").exists());
    let leftovers: Vec<_> = fs::read_dir(vault.root())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".wi-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn watcher_repair_and_cli_writes_keep_final_acknowledged_version() {
    use std::time::Duration;
    let (_fx, vault) = Fixture::new();
    let mut last = common::write(&vault, "a.md", "initial", Ingress::Cli);
    let handle = vault
        .watch_tuned(WatchTuning {
            repair_interval: Duration::from_millis(10),
            debounce: Some(Duration::from_millis(5)),
            ..WatchTuning::default()
        })
        .unwrap();
    for n in 0..25 {
        handle.inject_backend_error("force repair while CLI writes");
        last = vault
            .write(WriteRequest {
                path: "a.md".into(),
                content: format!("engine {n}\n").into_bytes(),
                expected_checksum: Some(last.checksum),
                op_id: Some(format!("adv-{n}")),
                ingress: Ingress::Cli,
            })
            .unwrap();
    }
    handle.stop();
    assert_eq!(
        hash_bytes(&fs::read(vault.root().join("a.md")).unwrap()),
        last.checksum
    );
    assert!(common::git(vault.root(), &["show", "HEAD:a.md"]).ends_with("engine 24\n"));
}

#[test]
fn late_editor_save_through_open_handle_must_survive_exchange() {
    use std::{
        cell::RefCell,
        io::{Seek, SeekFrom, Write},
    };
    let (fx, mut vault) = Fixture::new();
    let first = common::write(&vault, "a.md", "old", Ingress::Cli);
    let editor = Rc::new(RefCell::new(None::<fs::File>));
    let before = editor.clone();
    let after = editor.clone();
    vault.set_hooks(Hooks {
        before_swap: Some(Rc::new(move |path| {
            *before.borrow_mut() = Some(fs::OpenOptions::new().write(true).open(path).unwrap());
        })),
        after_swap: Some(Rc::new(move |_| {
            let mut held = after.borrow_mut();
            let file = held.as_mut().unwrap();
            file.seek(SeekFrom::Start(0)).unwrap();
            file.set_len(0).unwrap();
            file.write_all(b"late editor save acknowledged by OS")
                .unwrap();
            file.flush().unwrap();
            wi_history::durable::barrier(file, wi_history::durable::Durability::Standard).unwrap();
        })),
        ..Hooks::default()
    });
    let result = vault
        .write(WriteRequest {
            path: "a.md".into(),
            content: b"new".to_vec(),
            expected_checksum: Some(first.checksum),
            op_id: None,
            ingress: Ingress::Cli,
        })
        .unwrap();
    eprintln!("acknowledged engine result: {result:?}");
    drop(editor);
    drop(vault);
    let reopened = fx.open();
    let work = reopened.work_requests().unwrap();
    let entries: Vec<_> = fs::read_dir(reopened.root())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    eprintln!("after reopen: {entries:?}; work: {work:?}");
    assert!(
        result.conflict_copy.is_some(),
        "editor's successful save disappeared with displaced inode"
    );
}

fn intent(vault: &Vault, path: &str, from: Option<&str>, old: Option<&[u8]>, new: &[u8]) -> u64 {
    let h = vault.history_vault();
    let blob = Blob::write_durable(h, new).unwrap();
    let guard = PublisherLock::acquire(h).unwrap();
    Manifest::append_intent(
        h,
        &guard,
        IntentRecord {
            op_id: None,
            request_digest: hash_bytes(b"adversarial crash"),
            actor: "cli".into(),
            path: path.into(),
            moved_from: from.map(str::to_owned),
            old_hash: old.map(hash_bytes),
            new_hash: hash_bytes(new),
            new_blob: blob,
            revert_of: None,
            batch: None,
            losses: vec![],
        },
    )
    .unwrap()
}

#[cfg(unix)]
#[test]
fn unreadable_target_must_not_prevent_open() {
    let (fx, vault) = Fixture::new();
    fs::write(vault.root().join("a.md"), b"old").unwrap();
    intent(&vault, "a.md", None, Some(b"old"), b"new");
    let path = vault.root().join("a.md");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    drop(vault);
    let reopened = Vault::open_with(fx.root_input(), fx.options());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        reopened.is_ok(),
        "opening must tolerate a recovery problem: {reopened:?}"
    );
}

#[cfg(unix)]
#[test]
fn unreadable_displaced_temp_must_not_prevent_open() {
    let (fx, vault) = Fixture::new();
    fs::write(vault.root().join("a.md"), b"new").unwrap();
    intent(&vault, "a.md", None, Some(b"old"), b"new");
    let temp = vault.root().join(".a.md.wi-tmp-2147483647-0");
    fs::write(&temp, b"editor bytes displaced by exchange").unwrap();
    fs::set_permissions(&temp, fs::Permissions::from_mode(0o000)).unwrap();
    drop(vault);
    let reopened = Vault::open_with(fx.root_input(), fx.options());
    fs::set_permissions(&temp, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        reopened.is_ok(),
        "opening must tolerate a recovery problem: {reopened:?}"
    );
}

#[test]
fn conflict_rename_failure_must_not_prevent_open() {
    let (fx, vault) = Fixture::new();
    fs::write(vault.root().join("a.md"), b"new").unwrap();
    let seq = intent(&vault, "a.md", None, Some(b"old"), b"new");
    let temp = vault.root().join(".a.md.wi-tmp-2147483647-0");
    fs::write(&temp, b"editor version").unwrap();
    fs::create_dir(vault.root().join(format!(".a.md.wi-conflict-{seq}"))).unwrap();
    drop(vault);
    let reopened = Vault::open_with(fx.root_input(), fx.options());
    assert!(
        reopened.is_ok(),
        "opening must tolerate a recovery problem: {reopened:?}"
    );
}

#[cfg(unix)]
#[test]
fn unreadable_displaced_editor_bytes_must_survive_failed_write() {
    let (fx, mut vault) = Fixture::new();
    let first = common::write(&vault, "a.md", "old", Ingress::Cli);
    vault.set_hooks(Hooks {
        before_swap: Some(Rc::new(|path| {
            fs::write(path, b"unique unsnapshotted editor bytes").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
        })),
        ..Hooks::default()
    });
    let result = vault.write(WriteRequest {
        path: "a.md".into(),
        content: b"new".to_vec(),
        expected_checksum: Some(first.checksum),
        op_id: None,
        ingress: Ingress::Cli,
    });
    eprintln!("write result: {result:?}");
    let names: Vec<_> = fs::read_dir(vault.root())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    eprintln!("root entries: {names:?}");
    let copies: Vec<_> = names
        .iter()
        .filter(|n| {
            n.to_string_lossy().contains(".wi-conflict-")
                || n.to_string_lossy().contains(".wi-tmp-")
        })
        .collect();
    assert!(
        !fs::read(vault.root().join("a.md"))
            .unwrap()
            .windows(b"unique unsnapshotted editor bytes".len())
            .any(|w| w == b"unique unsnapshotted editor bytes")
    );
    drop(vault);
    let reopened = fx.open();
    eprintln!(
        "work after failed write and reopen: {:?}",
        reopened.work_requests().unwrap()
    );
    assert!(
        !copies.is_empty(),
        "the only editor version was unlinked by TempFile::drop"
    );
}

#[test]
fn crash_after_conflict_rename_must_create_work_request_on_open() {
    let (fx, vault) = Fixture::new();
    let seq = intent(&vault, "a.md", None, Some(b"old"), b"new");
    fs::write(vault.root().join("a.md"), b"new").unwrap();
    // State after settle_displaced renamed temp, before record_work.
    let copy = vault.root().join(format!(".a.md.wi-conflict-{seq}"));
    fs::write(&copy, b"unique unsnapshotted editor bytes").unwrap();
    drop(vault);
    let reopened = fx.open();
    assert!(copy.exists());
    let work = reopened.work_requests().unwrap();
    eprintln!("work after recovery: {work:?}");
    assert!(
        work.iter()
            .any(|w| w.kind == WorkKind::ConflictCopy && w.seq == Some(seq)),
        "editor bytes are in an excluded hidden conflict copy without a work request"
    );
}

#[test]
fn edited_move_destination_must_not_strand_editor_source_aside() {
    let (fx, vault) = Fixture::new();
    fs::write(vault.root().join("a.md"), b"old").unwrap();
    let seq = intent(&vault, "b.md", Some("a.md"), None, b"old");
    fs::write(vault.root().join("b.md"), b"old").unwrap();
    // Editor save after source precondition; retire_move_source renamed it aside.
    fs::write(vault.root().join("a.md"), b"unique editor source version").unwrap();
    let aside = vault.root().join(format!(".a.md.wi-moved-{seq}"));
    fs::rename(vault.root().join("a.md"), &aside).unwrap();
    // Writer crashes, editor saves the destination before reopen.
    fs::write(vault.root().join("b.md"), b"destination editor version").unwrap();
    drop(vault);
    let reopened = fx.open();
    eprintln!(
        "work after recovery: {:?}",
        reopened.work_requests().unwrap()
    );
    assert!(
        !aside.exists(),
        "the only source editor version is stranded in hidden aside"
    );
}

fn crashed_move(vault: &Vault, from: &str, to: &str, bytes: &[u8]) -> u64 {
    let history = vault.history_vault();
    let blob = Blob::write_durable(history, bytes).unwrap();
    let guard = PublisherLock::acquire(history).unwrap();
    let seq = Manifest::append_intent(
        history,
        &guard,
        IntentRecord {
            op_id: None,
            request_digest: hash_bytes(b"crash"),
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
#[cfg(unix)]
#[test]
fn probe_recovery_unreadable_target_must_open() {
    use std::os::unix::fs::PermissionsExt;
    let (fx, vault) = Fixture::new();
    fs::write(vault.root().join("a.png"), b"binary\xff").unwrap();
    crashed_move(&vault, "a.png", "b.png", b"binary\xff");
    let target = vault.root().join("b.png");
    drop(vault);
    fs::set_permissions(&target, fs::Permissions::from_mode(0o000)).unwrap();
    let opened = Vault::open_with(fx.root_input(), fx.options());
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        opened.is_ok(),
        "opening must tolerate a recovery problem: {opened:?}"
    );
}
#[test]
fn probe_recovery_temp_rename_failure_must_open() {
    let (fx, vault) = Fixture::new();
    fs::write(vault.root().join("a.png"), b"binary\xff").unwrap();
    let seq = crashed_move(&vault, "a.png", "b.png", b"binary\xff");
    fs::write(vault.root().join(".b.png.wi-tmp-123-0"), b"editor bytes").unwrap();
    fs::create_dir(vault.root().join(format!(".b.png.wi-conflict-{seq}"))).unwrap();
    drop(vault);
    let opened = Vault::open_with(fx.root_input(), fx.options());
    assert!(
        opened.is_ok(),
        "opening must tolerate a recovery problem: {opened:?}"
    );
}
#[test]
fn probe_child_index_fail_writer() {
    let Ok(root) = std::env::var("WI_PROBE_ROOT") else {
        return;
    };
    let index = std::env::var("WI_PROBE_INDEX").unwrap();
    let vault = Vault::open_with(
        root,
        OpenOptions {
            index_path: Some(index.into()),
        },
    )
    .unwrap();
    let result = vault
        .write(WriteRequest {
            path: "a.md".into(),
            content: b"fresh unique content\n".to_vec(),
            expected_checksum: Some(hash_bytes(&fs::read(vault.root().join("a.md")).unwrap())),
            op_id: None,
            ingress: Ingress::Cli,
        })
        .unwrap();
    assert!(matches!(result.index, wi_store::IndexState::Pending));
    assert_eq!(vault.index_pending(), vec!["a.md"]);
}
fn identical_late_append(reindex: bool) {
    let (fx, mut vault) = Fixture::new();
    common::write(&vault, "a.md", "old\n", Ingress::Cli);
    vault.flush().unwrap();
    let other = fx.open();
    let root = fx.root_input();
    let index = fx.options().index_path.unwrap();
    fs::write(vault.root().join(".git/wi/index-pending"), "a.md\n").unwrap();
    vault.set_hooks(Hooks {
        after_index_repair: Some(Rc::new(move |marker: &Path| {
            other.recover().unwrap();
            assert!(!marker.exists());
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "probe_child_index_fail_writer", "--nocapture"])
                .env("WI_PROBE_ROOT", &root)
                .env("WI_PROBE_INDEX", &index)
                .env("WI_FAULT", "index_update")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(fs::read_to_string(marker).unwrap(), "a.md\n");
        })),
        ..Hooks::default()
    });
    if reindex {
        vault.reindex_with_missing().unwrap();
    } else {
        vault.recover().unwrap();
    }
    let idx = wi_index::Index::open_at(
        vault.root(),
        vault.vault_id(),
        fx.options().index_path.unwrap(),
    )
    .unwrap();
    let hits = idx
        .search(&wi_index::Query {
            text: "fresh".into(),
            ..Default::default()
        })
        .unwrap();
    assert!(
        hits.hits.is_empty(),
        "probe expects the intentionally failed index update to remain stale"
    );
    assert!(
        fs::read_to_string(vault.root().join("a.md"))
            .unwrap()
            .contains("fresh")
    );
    eprintln!(
        "new file bytes contain fresh; index returns no hits; pending={:?}",
        vault.index_pending()
    );
    assert_eq!(
        vault.index_pending(),
        vec!["a.md"],
        "new failed update was lost by prefix ABA"
    );
}
#[test]
fn probe_ingress_refreshes_head_after_writer_flush() {
    use std::cell::Cell;
    let (fx, mut vault) = Fixture::new();
    common::write(&vault, "a.md", "old\n", Ingress::Cli);
    vault.flush().unwrap();
    fs::write(vault.root().join("a.md"), "editor\n").unwrap();
    let writer = fx.open();
    let once = Cell::new(false);
    vault.set_hooks(Hooks {
        before_ingest_lock: Some(Rc::new(move |_| {
            if !once.replace(true) {
                writer
                    .write(WriteRequest {
                        path: "a.md".into(),
                        content: b"new engine\n".to_vec(),
                        expected_checksum: Some(hash_bytes(b"editor\n")),
                        op_id: None,
                        ingress: Ingress::Cli,
                    })
                    .unwrap();
                writer.flush().unwrap();
            }
        })),
        ..Hooks::default()
    });
    assert_eq!(
        vault.ingest_path("a.md").unwrap().outcome,
        wi_store::IngestOutcome::Unchanged
    );
}

#[test]
fn probe_index_repair_must_keep_identical_late_append() {
    identical_late_append(false);
}
#[test]
fn probe_reindex_must_keep_identical_late_append() {
    identical_late_append(true);
}
#[cfg(unix)]
#[test]
fn probe_unreadable_move_source_is_work_request() {
    use std::os::unix::fs::PermissionsExt;
    let (fx, vault) = Fixture::new();
    let source = vault.root().join("a.png");
    fs::write(&source, b"binary\xff").unwrap();
    let seq = crashed_move(&vault, "a.png", "b.png", b"binary\xff");
    drop(vault);
    fs::set_permissions(&source, fs::Permissions::from_mode(0o000)).unwrap();
    let opened = Vault::open_with(fx.root_input(), fx.options());
    // The source is taken aside before it is read.
    let aside = fx.root_input().join(format!(".a.png.wi-moved-{seq}"));
    let held = if aside.exists() { &aside } else { &source };
    fs::set_permissions(held, fs::Permissions::from_mode(0o600)).unwrap();
    let v = opened.unwrap();
    assert!(
        v.work_requests()
            .unwrap()
            .iter()
            .any(|w| w.kind == wi_store::WorkKind::RecoveryProblem)
    );
    // The intent stayed open; once the path is readable a later open settles it.
    drop(v);
    let v = fx.open();
    assert!(
        !aside.exists() && !source.exists(),
        "unchanged source removed on retry"
    );
    assert_eq!(fs::read(v.root().join("b.png")).unwrap(), b"binary\xff");
    let problems = v
        .work_requests()
        .unwrap()
        .into_iter()
        .filter(|w| w.kind == WorkKind::RecoveryProblem)
        .count();
    assert_eq!(problems, 1, "one request per (seq, path)");
}

fn problems_for(vault: &Vault, path: &str, seq: u64) -> usize {
    vault
        .work_requests()
        .unwrap()
        .iter()
        .filter(|w| w.kind == WorkKind::RecoveryProblem && w.path == path && w.seq == Some(seq))
        .count()
}

#[test]
fn directory_displaced_temp_holds_intent_until_removed() {
    let (fx, vault) = Fixture::new();
    fs::write(vault.root().join("a.md"), b"new").unwrap();
    let seq = intent(&vault, "a.md", None, Some(b"old"), b"new");
    let name = ".a.md.wi-tmp-2147483647-0";
    let temp = vault.root().join(name);
    fs::create_dir(&temp).unwrap();
    drop(vault);

    for _ in 0..2 {
        let opened = fx.open();
        assert_eq!(problems_for(&opened, name, seq), 1);
        assert_eq!(
            opened
                .work_requests()
                .unwrap()
                .iter()
                .filter(|w| w.kind == WorkKind::RecoveryProblem)
                .count(),
            1
        );
        let guard = PublisherLock::acquire(opened.history_vault()).unwrap();
        let snap = Manifest::snapshot(opened.history_vault(), &guard).unwrap();
        assert!(snap.open_intents().any(|entry| entry.seq == seq));
        assert!(opened.open_report().recovery.completed.is_empty());
        assert!(temp.is_dir(), "recovery leaves the directory untouched");
    }

    fs::remove_dir(&temp).unwrap();
    let reopened = fx.open();
    assert_eq!(reopened.open_report().recovery.completed, vec![seq]);
    assert_eq!(problems_for(&reopened, name, seq), 1);
    let guard = PublisherLock::acquire(reopened.history_vault()).unwrap();
    let snap = Manifest::snapshot(reopened.history_vault(), &guard).unwrap();
    assert!(snap.open_intents().next().is_none());
    assert_eq!(fs::read(reopened.root().join("a.md")).unwrap(), b"new");
}

#[test]
fn doctor_reports_scratch_directories() {
    let (_fx, vault) = Fixture::new();
    for (name, check_name) in [
        (".a.md.wi-tmp-2147483647-0", "temp-files"),
        (".a.md.wi-conflict-99", "conflict-copies"),
        (".a.md.wi-moved-99", "moved-sources"),
    ] {
        fs::create_dir(vault.root().join(name)).unwrap();
        let report = vault.doctor().unwrap();
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == check_name && c.detail.contains(name)),
            "scratch directory {name} must be reported"
        );
    }
}

#[cfg(unix)]
#[test]
fn doctor_reports_scratch_symlinks_without_following_them() {
    let (_fx, vault) = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    let nested = ".outside.md.wi-tmp-2147483647-1";
    fs::write(outside.path().join(nested), b"outside bytes").unwrap();
    for (name, check_name) in [
        (".a.md.wi-tmp-2147483647-0", "temp-files"),
        (".a.md.wi-conflict-99", "conflict-copies"),
        (".a.md.wi-moved-99", "moved-sources"),
    ] {
        let link = vault.root().join(name);
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let report = vault.doctor().unwrap();
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == check_name && c.detail.contains(name)),
            "scratch symlink {name} must be reported"
        );
        assert!(report.checks.iter().all(|c| !c.detail.contains(nested)));
        assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
        assert_eq!(
            fs::read(outside.path().join(nested)).unwrap(),
            b"outside bytes"
        );
    }
}

#[cfg(unix)]
#[test]
fn held_intent_has_one_problem_and_settles_when_readable() {
    let (fx, vault) = Fixture::new();
    fs::write(vault.root().join("a.md"), b"old").unwrap();
    let seq = intent(&vault, "a.md", None, Some(b"old"), b"new");
    let path = vault.root().join("a.md");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    drop(vault);
    let first = fx.open();
    drop(first);
    let second = fx.open();
    assert_eq!(
        problems_for(&second, "a.md", seq),
        1,
        "idempotent across opens"
    );
    assert!(
        second
            .doctor()
            .unwrap()
            .checks
            .iter()
            .any(|c| c.name == "manifest" && c.detail.contains("interrupted write")),
        "intent held open"
    );
    drop(second);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let third = fx.open();
    assert_eq!(
        third.open_report().recovery.dropped,
        vec![seq],
        "retried and settled"
    );
    assert_eq!(problems_for(&third, "a.md", seq), 1);
    third
        .write(WriteRequest {
            path: "a.md".into(),
            content: b"after".to_vec(),
            expected_checksum: Some(hash_bytes(b"old")),
            op_id: None,
            ingress: Ingress::Cli,
        })
        .expect("writes accepted again once the intent settled");
}

#[cfg(unix)]
#[test]
fn unreadable_displaced_temp_is_kept_and_settled_later() {
    let (fx, vault) = Fixture::new();
    fs::write(vault.root().join("a.md"), b"new").unwrap();
    let seq = intent(&vault, "a.md", None, Some(b"old"), b"new");
    let temp = vault.root().join(".a.md.wi-tmp-2147483647-0");
    fs::write(&temp, b"editor bytes displaced by exchange").unwrap();
    fs::set_permissions(&temp, fs::Permissions::from_mode(0o000)).unwrap();
    drop(vault);
    let opened = fx.open();
    assert_eq!(problems_for(&opened, ".a.md.wi-tmp-2147483647-0", seq), 1);
    // A scan never removes a temp that recovery kept.
    opened.reconcile().unwrap();
    assert!(
        temp.exists(),
        "reconcile must keep the unreadable displaced temp"
    );
    drop(opened);
    fs::set_permissions(&temp, fs::Permissions::from_mode(0o600)).unwrap();
    let reopened = fx.open();
    assert!(!temp.exists());
    let copy = reopened.root().join(format!(".a.md.wi-conflict-{seq}"));
    assert_eq!(
        fs::read(copy).unwrap(),
        b"editor bytes displaced by exchange"
    );
    assert!(
        reopened
            .work_requests()
            .unwrap()
            .iter()
            .any(|w| w.kind == WorkKind::ConflictCopy && w.seq == Some(seq))
    );
    assert_eq!(reopened.open_report().recovery.completed, vec![seq]);
}

#[cfg(unix)]
#[test]
fn unreadable_displaced_bytes_get_conflict_copy_request() {
    let (_fx, mut vault) = Fixture::new();
    let first = common::write(&vault, "a.md", "old", Ingress::Cli);
    vault.set_hooks(Hooks {
        before_swap: Some(Rc::new(|path| {
            fs::write(path, b"unique editor bytes").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
        })),
        ..Hooks::default()
    });
    let result = vault
        .write(WriteRequest {
            path: "a.md".into(),
            content: b"new".to_vec(),
            expected_checksum: Some(first.checksum),
            op_id: None,
            ingress: Ingress::Cli,
        })
        .unwrap();
    let copy = result.conflict_copy.clone().expect("kept as conflict copy");
    let abs = vault.root().join(&copy);
    fs::set_permissions(&abs, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(fs::read(abs).unwrap(), b"unique editor bytes");
    assert!(
        vault
            .work_requests()
            .unwrap()
            .iter()
            .any(|w| w.kind == WorkKind::ConflictCopy
                && w.seq == Some(result.seq)
                && w.detail.contains("could not be read"))
    );
}

#[test]
fn doctor_reports_conflict_copy_without_request() {
    let (_fx, vault) = Fixture::new();
    common::write(&vault, "a.md", "x", Ingress::Cli);
    fs::write(vault.root().join(".a.md.wi-conflict-99"), b"orphan").unwrap();
    let report = vault.doctor().unwrap();
    let check = report
        .checks
        .iter()
        .find(|c| c.name == "conflict-copies")
        .expect("reported");
    assert!(check.detail.contains(".a.md.wi-conflict-99"));
}

#[test]
fn announced_copy_is_not_reported_by_doctor() {
    let (fx, vault) = Fixture::new();
    let seq = intent(&vault, "a.md", None, Some(b"old"), b"new");
    fs::write(vault.root().join("a.md"), b"new").unwrap();
    fs::write(
        vault.root().join(format!(".a.md.wi-conflict-{seq}")),
        b"editor",
    )
    .unwrap();
    drop(vault);
    let reopened = fx.open();
    assert!(
        !reopened
            .doctor()
            .unwrap()
            .checks
            .iter()
            .any(|c| c.name == "conflict-copies")
    );
    drop(reopened);
    let again = fx.open();
    let n = again
        .work_requests()
        .unwrap()
        .iter()
        .filter(|w| w.kind == WorkKind::ConflictCopy && w.seq == Some(seq))
        .count();
    assert_eq!(n, 1, "announced once");
}

#[test]
fn undone_move_puts_the_source_back() {
    let (fx, vault) = Fixture::new();
    fs::write(vault.root().join("a.md"), b"old").unwrap();
    let seq = intent(&vault, "b.md", Some("a.md"), None, b"old");
    // Writer claimed the source, then crashed before placing the target.
    let aside = vault.root().join(format!(".a.md.wi-moved-{seq}"));
    fs::rename(vault.root().join("a.md"), &aside).unwrap();
    drop(vault);
    let reopened = fx.open();
    assert!(!aside.exists());
    assert_eq!(fs::read(reopened.root().join("a.md")).unwrap(), b"old");
    assert!(
        !reopened
            .root()
            .join(format!(".a.md.wi-conflict-{seq}"))
            .exists()
    );
}

#[test]
fn undone_move_keeps_aside_as_conflict_copy_when_the_name_is_taken() {
    let (fx, vault) = Fixture::new();
    fs::write(vault.root().join("a.md"), b"old").unwrap();
    let seq = intent(&vault, "b.md", Some("a.md"), None, b"old");
    let aside = vault.root().join(format!(".a.md.wi-moved-{seq}"));
    fs::rename(vault.root().join("a.md"), &aside).unwrap();
    // An editor created a new file under the source name meanwhile.
    fs::write(vault.root().join("a.md"), b"new owner note").unwrap();
    drop(vault);
    let reopened = fx.open();
    assert!(!aside.exists());
    assert_eq!(
        fs::read(reopened.root().join("a.md")).unwrap(),
        b"new owner note"
    );
    let copy = reopened.root().join(format!(".a.md.wi-conflict-{seq}"));
    assert_eq!(
        fs::read(copy).unwrap(),
        b"old",
        "the only copy of the note is kept"
    );
    assert!(
        reopened
            .work_requests()
            .unwrap()
            .iter()
            .any(|w| w.kind == WorkKind::ConflictCopy && w.path == "a.md" && w.seq == Some(seq))
    );
}

#[test]
fn claim_left_by_crashed_repair_is_taken_over() {
    let (fx, vault) = Fixture::new();
    common::write(&vault, "a.md", "old\n", Ingress::Cli);
    let wi = vault.root().join(".git/wi");
    fs::write(wi.join("index-pending.claim-1-0-0"), "a.md\n").unwrap();
    fs::write(wi.join("index-pending"), "a.md\n").unwrap();
    assert_eq!(vault.index_pending(), vec!["a.md"]);
    drop(vault);
    let reopened = fx.open();
    assert_eq!(reopened.open_report().index_repaired, vec!["a.md"]);
    assert!(reopened.index_pending().is_empty());
    assert!(!wi.join("index-pending.claim-1-0-0").exists());
}

#[test]
fn pending_marker_written_during_repair_survives() {
    let (_fx, mut vault) = Fixture::new();
    common::write(&vault, "a.md", "a\n", Ingress::Cli);
    let marker = vault.root().join(".git/wi/index-pending");
    fs::write(&marker, "a.md\n").unwrap();
    vault.set_hooks(Hooks {
        after_index_repair: Some(Rc::new(|marker: &Path| {
            // A writer marks the same path again after the claim.
            fs::write(marker, "a.md\n").unwrap();
        })),
        ..Hooks::default()
    });
    vault.recover().unwrap();
    assert_eq!(
        vault.index_pending(),
        vec!["a.md"],
        "late identical mark kept"
    );
}
