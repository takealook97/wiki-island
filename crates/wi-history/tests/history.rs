mod common;

use common::*;
use wi_history::{
    Durability, FlushBlock, Flusher, History, HistoryError, Manifest, Missing, PinRef, PinSource,
    PublisherLock, Vault, generation, hash_bytes, resolve_pin,
};

#[test]
fn one_commit_per_operation_in_manifest_order() {
    let (_dir, vault) = new_vault();
    let s1 = save(&vault, "notes/a.md", b"a1\n", "cli", Some("op-1"));
    let s2 = save(&vault, "b.md", b"b1\n", "mcp", Some("op-2"));
    let s3 = save(&vault, "notes/a.md", b"a2\n", "mcp", None);
    assert_eq!((s1, s2, s3), (1, 2, 3));

    let report = Flusher::flush(&vault).unwrap();
    assert_eq!(report.commits.len(), 3);
    assert_eq!(report.retired_upto, 3);
    assert_eq!(report.remaining, 0);

    let commits = commits(vault.root());
    assert_eq!(commits.len(), 3);
    assert_eq!(all_seqs(vault.root()), vec![1, 2, 3]);
    assert_eq!(
        trailer(&commits[0].1, "Wi-Operation"),
        vec!["op-1 digest-notes_a.md"]
    );
    assert_eq!(trailer(&commits[0].1, "Wi-Actor"), vec!["cli"]);
    assert_eq!(trailer(&commits[1].1, "Wi-Actor"), vec!["mcp"]);
    assert!(trailer(&commits[2].1, "Wi-Operation").is_empty());
    // Every acknowledged version is reachable in history.
    assert_eq!(show(vault.root(), &commits[0].0, "notes/a.md"), b"a1\n");
    assert_eq!(show(vault.root(), &commits[1].0, "b.md"), b"b1\n");
    assert_eq!(show(vault.root(), &commits[2].0, "notes/a.md"), b"a2\n");
    assert_eq!(show(vault.root(), &commits[2].0, "b.md"), b"b1\n");
    fsck(vault.root());
}

#[test]
fn bulk_import_batch_is_one_commit_and_waits_for_its_last_record() {
    let (_dir, vault) = new_vault();
    save(&vault, "before.md", b"x\n", "cli", None);
    ingest(&vault, "import/1.md", b"1\n", Some(("imp", false)));
    ingest(&vault, "import/2.md", b"2\n", Some(("imp", false)));

    let report = Flusher::flush(&vault).unwrap();
    assert_eq!(report.commits.len(), 1, "only the record before the batch");
    assert!(matches!(
        report.blocked,
        Some(wi_history::FlushBlock::IncompleteBatch { seq: 2, .. })
    ));
    assert_eq!(report.retired_upto, 1);

    ingest(&vault, "import/3.md", b"3\n", Some(("imp", true)));
    save(&vault, "after.md", b"y\n", "mcp", None);
    let report = Flusher::flush(&vault).unwrap();
    assert_eq!(report.commits.len(), 2);
    assert_eq!(report.commits[0].seqs, vec![2, 3, 4]);

    let commits = commits(vault.root());
    assert_eq!(commits.len(), 3);
    assert_eq!(trailer(&commits[1].1, "Wi-Batch"), vec!["imp"]);
    assert_eq!(seqs_of(&commits[1].1), vec![2, 3, 4]);
    for (path, bytes) in [
        ("import/1.md", b"1\n"),
        ("import/2.md", b"2\n"),
        ("import/3.md", b"3\n"),
    ] {
        assert_eq!(show(vault.root(), &commits[1].0, path), bytes);
    }
    assert_eq!(seqs_of(&commits[2].1), vec![5]);
}

#[test]
fn batch_with_an_aborted_intent_commits_the_rest_once() {
    let (_dir, vault) = new_vault();
    ingest(&vault, "imp/1.md", b"1", Some(("b", false)));
    // A crashed batch intent that recovery drops (file never written).
    let mut rec = intent(&vault, "imp/2.md", b"2", "import", None);
    rec.batch = Some(wi_history::BatchMark {
        id: "b".into(),
        last: false,
    });
    {
        let guard = PublisherLock::acquire(&vault).unwrap();
        Manifest::append_intent(&vault, &guard, rec).unwrap();
    }
    let report =
        wi_history::Recovery::replay(&vault, &wi_history::FsWorkingTree::new(vault.root()))
            .unwrap();
    assert_eq!(report.dropped, vec![2]);
    ingest(&vault, "imp/3.md", b"3", Some(("b", true)));
    let report = Flusher::flush(&vault).unwrap();
    assert_eq!(report.commits.len(), 1);
    assert_eq!(report.commits[0].seqs, vec![1, 3]);
    let commits = commits(vault.root());
    assert_eq!(
        trailer(&commits[0].1, "Wi-Seq")[0].split(' ').next(),
        Some("1,3")
    );
    assert_eq!(report.retired_upto, 3);
}

#[test]
fn operations_from_a_one_shot_process_are_committed_by_a_later_flush() {
    let (_dir, vault) = new_vault();
    let root = vault.root().to_str().unwrap().to_owned();
    let out = testproc()
        .args(["append", &root, "cli", "3"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = testproc().args(["flush", &root]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(all_seqs(vault.root()), vec![1, 2, 3]);
    assert_eq!(generation(&vault).unwrap(), 3);
}

#[test]
fn generation_advances_per_publish_and_survives_retirement() {
    let (_dir, vault) = new_vault();
    assert_eq!(generation(&vault).unwrap(), 0);
    save(&vault, "a.md", b"1", "cli", None);
    assert_eq!(generation(&vault).unwrap(), 1);
    ingest(&vault, "b.md", b"2", None);
    assert_eq!(generation(&vault).unwrap(), 2);
    Flusher::flush(&vault).unwrap();
    assert_eq!(generation(&vault).unwrap(), 2);
    assert_eq!(save(&vault, "a.md", b"3", "cli", None), 3);
}

#[test]
fn history_lists_commits_and_revert_adds_a_commit() {
    let (_dir, vault) = new_vault();
    save(&vault, "a.md", b"v1\n", "cli", None);
    save(&vault, "other.md", b"o\n", "cli", None);
    save(&vault, "a.md", b"v2\n", "mcp", None);
    Flusher::flush(&vault).unwrap();
    let before = commits(vault.root());

    let log = History::log(&vault, "a.md").unwrap();
    assert_eq!(log.len(), 2, "other.md's commit is not listed");
    assert_eq!(log[0].commit, before[2].0, "newest first");
    assert_eq!(log[1].commit, before[0].0);
    assert!(
        log[0]
            .trailers
            .iter()
            .any(|(k, v)| k == "Wi-Actor" && v == "mcp")
    );

    let reverted = History::revert(&vault, "a.md", &log[1].commit, "cli").unwrap();
    let after = commits(vault.root());
    assert_eq!(after.len(), 4);
    assert_eq!(&after[..3], &before[..], "earlier commits intact");
    assert_eq!(after[3].0, reverted);
    assert_eq!(
        trailer(&after[3].1, "Wi-Revert"),
        vec![log[1].commit.as_str()]
    );
    assert_eq!(
        History::read_at(&vault, &reverted, "a.md").unwrap(),
        b"v1\n"
    );
    assert_eq!(History::log(&vault, "a.md").unwrap().len(), 3);
    fsck(vault.root());
}

#[test]
fn revert_flushes_pending_records_first() {
    let (_dir, vault) = new_vault();
    save(&vault, "a.md", b"v1\n", "cli", None);
    Flusher::flush(&vault).unwrap();
    let first = History::head(&vault).unwrap().unwrap();
    save(&vault, "a.md", b"v2\n", "cli", None);
    History::revert(&vault, "a.md", &first, "cli").unwrap();
    let commits = commits(vault.root());
    assert_eq!(commits.len(), 3);
    assert_eq!(seqs_of(&commits[1].1), vec![2]);
    assert_eq!(show(vault.root(), &commits[2].0, "a.md"), b"v1\n");
}

#[test]
fn revert_refuses_a_path_absent_from_the_commit() {
    let (_dir, vault) = new_vault();
    save(&vault, "a.md", b"v1\n", "cli", None);
    Flusher::flush(&vault).unwrap();
    let head = History::head(&vault).unwrap().unwrap();
    assert!(History::revert(&vault, "nope.md", &head, "cli").is_err());
    assert_eq!(commits(vault.root()).len(), 1);
}

#[test]
fn deleted_file_is_reported_missing_and_recoverable() {
    let (_dir, vault) = new_vault();
    save(&vault, "keep.md", b"keep\n", "cli", None);
    save(&vault, "gone.md", b"precious\n", "cli", None);
    save(&vault, "moved.md", b"m\n", "cli", None);
    Flusher::flush(&vault).unwrap();
    assert!(Missing::scan(&vault).unwrap().is_empty());

    std::fs::remove_file(vault.root().join("gone.md")).unwrap();
    // A pending archive move is not missing.
    let mut rec = intent(&vault, "archive/moved.md", b"m\n", "cli", None);
    rec.moved_from = Some("moved.md".into());
    rec.old_hash = None;
    save_record(&vault, rec, b"m\n");
    std::fs::remove_file(vault.root().join("moved.md")).unwrap();

    let missing = Missing::scan(&vault).unwrap();
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].path, "gone.md");
    assert_eq!(vault.read_blob(&missing[0].blob).unwrap(), b"precious\n");

    Flusher::flush(&vault).unwrap();
    let head = History::head(&vault).unwrap().unwrap();
    assert!(
        History::blob_at(&vault, &head, "moved.md")
            .unwrap()
            .is_none()
    );
    assert!(
        History::blob_at(&vault, &head, "archive/moved.md")
            .unwrap()
            .is_some()
    );
    let missing = Missing::scan(&vault).unwrap();
    assert_eq!(
        missing.iter().map(|m| m.path.as_str()).collect::<Vec<_>>(),
        vec!["gone.md"]
    );
}

#[test]
fn no_history_rewrite_every_earlier_head_stays_an_ancestor() {
    let (_dir, vault) = new_vault();
    let mut heads = Vec::new();
    for round in 0..3 {
        save(&vault, "a.md", format!("a{round}").as_bytes(), "cli", None);
        save(&vault, &format!("n{round}.md"), b"n", "mcp", None);
        Flusher::flush(&vault).unwrap();
        heads.push(History::head(&vault).unwrap().unwrap());
    }
    History::revert(&vault, "a.md", &heads[0], "cli").unwrap();
    heads.push(History::head(&vault).unwrap().unwrap());
    wi_history::Recovery::replay(&vault, &wi_history::FsWorkingTree::new(vault.root())).unwrap();
    Flusher::flush(&vault).unwrap();
    let last = History::head(&vault).unwrap().unwrap();
    for head in &heads {
        assert!(History::is_ancestor(&vault, head, &last).unwrap());
    }
    // The reflog shows only forward moves: each new value's parent chain holds the old.
    let reflog = git(vault.root(), &["reflog", "--format=%H"]);
    let entries: Vec<&str> = reflog.lines().collect();
    for pair in entries.windows(2) {
        assert!(History::is_ancestor(&vault, pair[1], pair[0]).unwrap());
    }
    fsck(vault.root());
}

fn external_commit(vault: &Vault) -> String {
    std::fs::write(vault.root().join("ext.md"), b"external").unwrap();
    git(vault.root(), &["add", "ext.md"]);
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
            "external",
        ],
    );
    History::head(vault).unwrap().unwrap()
}

#[test]
fn flush_after_an_external_commit_builds_on_it() {
    let (_dir, vault) = new_vault();
    save(&vault, "a.md", b"1", "cli", None);
    Flusher::flush(&vault).unwrap();
    let external = external_commit(&vault);
    save(&vault, "a.md", b"2", "cli", None);
    Flusher::flush(&vault).unwrap();
    let head = History::head(&vault).unwrap().unwrap();
    assert!(History::is_ancestor(&vault, &external, &head).unwrap());
    // The git index followed the engine commit, so the manual commit kept a.md.
    assert_eq!(show(vault.root(), &external, "a.md"), b"1");
    assert_eq!(show(vault.root(), &head, "ext.md"), b"external");
}

#[test]
fn flush_refuses_when_the_branch_moved_and_forces_nothing() {
    let (_dir, vault) = new_vault();
    save(&vault, "a.md", b"1", "cli", None);
    Flusher::flush(&vault).unwrap();
    save(&vault, "a.md", b"2", "cli", None);

    let gate = tempfile::tempdir().unwrap();
    let root = vault.root().to_str().unwrap().to_owned();
    let child = testproc()
        .args(["flush", &root])
        .env(
            "WI_FAULT",
            format!("pause_before_advance:{}", gate.path().display()),
        )
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    while !gate.path().join("ready").exists() {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // Someone else (the git CLI) advances the branch meanwhile.
    let external = external_commit(&vault);
    std::fs::write(gate.path().join("go"), b"").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success(), "flush must fail on a moved branch");
    assert!(String::from_utf8_lossy(&out.stderr).contains("branch moved"));
    assert_eq!(
        History::head(&vault).unwrap().unwrap(),
        external,
        "nothing forced"
    );

    let snap = {
        let guard = PublisherLock::acquire(&vault).unwrap();
        Manifest::snapshot(&vault, &guard).unwrap()
    };
    assert_eq!(snap.entries.len(), 1, "record not retired");
    let report = Flusher::flush(&vault).unwrap();
    assert_eq!(report.commits.len(), 1);
    assert!(
        History::is_ancestor(&vault, &external, &History::head(&vault).unwrap().unwrap()).unwrap()
    );
}

#[test]
fn pending_pin_resolves_through_manifest_then_commit() {
    let (_dir, vault) = new_vault();
    save(&vault, "src.md", b"old\n", "cli", None);
    Flusher::flush(&vault).unwrap();
    save(&vault, "src.md", b"source bytes\n", "cli", None);
    let pin = PinRef {
        path: "src.md".into(),
        source_hash: hash_bytes(b"source bytes\n"),
        commit: None,
        pending: true,
    };
    let hit = resolve_pin(&vault, &pin).unwrap();
    assert_eq!(hit.bytes, b"source bytes\n");
    assert_eq!(hit.source, PinSource::Manifest { seq: 2 });

    let report = Flusher::flush(&vault).unwrap();
    let hit = resolve_pin(&vault, &pin).unwrap();
    assert_eq!(
        hit.source,
        PinSource::Commit {
            commit: report.commits[0].commit.clone()
        }
    );
    assert_eq!(hit.bytes, b"source bytes\n");

    let pinned = PinRef {
        commit: Some(report.commits[0].commit.clone()),
        pending: false,
        ..pin.clone()
    };
    assert_eq!(
        resolve_pin(&vault, &pinned).unwrap().bytes,
        b"source bytes\n"
    );
    let first = commits(vault.root())[0].0.clone();
    let wrong = PinRef {
        commit: Some(first),
        ..pinned
    };
    assert!(matches!(
        resolve_pin(&vault, &wrong),
        Err(wi_history::HistoryError::PinMismatch { .. })
    ));
    let unknown = PinRef {
        source_hash: hash_bytes(b"never"),
        ..pin
    };
    assert!(resolve_pin(&vault, &unknown).is_err());
}

#[test]
fn records_refuse_missing_blobs_bad_paths_and_foreign_guards() {
    let (_dir, vault) = new_vault();
    let (_dir2, other) = new_vault();
    let guard = PublisherLock::acquire(&vault).unwrap();
    let mut rec = intent(&vault, "a.md", b"x", "cli", None);
    rec.new_blob = wi_history::BlobId("0".repeat(40));
    assert!(matches!(
        Manifest::append_intent(&vault, &guard, rec),
        Err(wi_history::HistoryError::MissingBlob(_))
    ));
    for bad in ["../x.md", "/abs.md", ".git/config", "a//b.md", ""] {
        let mut rec = intent(&vault, "a.md", b"x", "cli", None);
        rec.path = bad.into();
        assert!(
            Manifest::append_intent(&vault, &guard, rec).is_err(),
            "{bad}"
        );
    }
    let mut rec = intent(&vault, "a.md", b"x", "cli", None);
    rec.op_id = Some("has space".into());
    assert!(Manifest::append_intent(&vault, &guard, rec).is_err());
    let rec = intent(&other, "a.md", b"x", "cli", None);
    assert!(matches!(
        Manifest::append_intent(&other, &guard, rec),
        Err(wi_history::HistoryError::ForeignGuard)
    ));
    assert!(Manifest::append_done(&vault, &guard, 99).is_err());
    drop(guard);
    assert!(Vault::open(tempfile::tempdir().unwrap().path()).is_err());
}

/// A record interleaved with an incomplete batch neither splits nor
/// commits it; once the last record lands the batch is one commit.
#[test]
fn interleaved_record_does_not_commit_an_incomplete_batch() {
    let (_dir, vault) = new_vault();
    ingest(&vault, "import/1.md", b"1\n", Some(("batch-A", false)));
    ingest(&vault, "watched.md", b"w\n", None);
    let report = Flusher::flush(&vault).unwrap();
    assert!(report.commits.is_empty(), "{report:?}");
    assert!(matches!(
        report.blocked,
        Some(FlushBlock::IncompleteBatch { seq: 1, .. })
    ));
    assert_eq!(report.retired_upto, 0);
    assert_eq!(report.remaining, 2);

    ingest(&vault, "import/2.md", b"2\n", Some(("batch-A", true)));
    let report = Flusher::flush(&vault).unwrap();
    assert!(report.blocked.is_none());
    let seqs: Vec<_> = report.commits.iter().map(|c| c.seqs.clone()).collect();
    assert_eq!(seqs, vec![vec![2], vec![1, 3]]);
    assert_eq!(report.retired_upto, 3);
    assert_eq!(report.remaining, 0);
    let commits = commits(vault.root());
    assert_eq!(trailer(&commits[1].1, "Wi-Batch"), vec!["batch-A"]);
    assert_eq!(show(vault.root(), &commits[1].0, "import/1.md"), b"1\n");
    assert_eq!(show(vault.root(), &commits[1].0, "watched.md"), b"w\n");
    // A replay of the same manifest state commits nothing twice.
    assert!(Flusher::flush(&vault).unwrap().commits.is_empty());
    fsck(vault.root());
}

/// A batch member whose path a newer interleaved record wrote does not
/// put its older bytes back on top.
#[test]
fn batch_does_not_overwrite_a_newer_interleaved_write() {
    let (_dir, vault) = new_vault();
    ingest(&vault, "p.md", b"batch\n", Some(("b", false)));
    ingest(&vault, "p.md", b"watcher\n", None);
    ingest(&vault, "q.md", b"q\n", Some(("b", true)));
    let report = Flusher::flush(&vault).unwrap();
    assert_eq!(report.commits.len(), 2);
    let head = History::head(&vault).unwrap().unwrap();
    assert_eq!(show(vault.root(), &head, "p.md"), b"watcher\n");
    assert_eq!(show(vault.root(), &head, "q.md"), b"q\n");
    assert_eq!(report.remaining, 0);
}

/// revert refuses while an earlier operation is not committed, and
/// changes nothing.
#[test]
fn revert_refuses_while_records_are_pending() {
    let (_dir, vault) = new_vault();
    save(&vault, "a.md", b"v1\n", "cli", None);
    Flusher::flush(&vault).unwrap();
    let first = History::head(&vault).unwrap().unwrap();
    save(&vault, "a.md", b"v2\n", "cli", None);
    Flusher::flush(&vault).unwrap();
    let head = History::head(&vault).unwrap().unwrap();
    let rec = intent(&vault, "b.md", b"b\n", "mcp", None);
    let guard = PublisherLock::acquire(&vault).unwrap();
    let open = Manifest::append_intent(&vault, &guard, rec).unwrap();
    drop(guard);

    let err = History::revert(&vault, "a.md", &first, "cli").unwrap_err();
    assert!(
        matches!(
            &err,
            HistoryError::PendingFlush {
                remaining: 1,
                blocked: Some(FlushBlock::OpenIntent { seq })
            } if *seq == open
        ),
        "{err}"
    );
    assert_eq!(History::head(&vault).unwrap().unwrap(), head);
}

#[test]
fn durability_comes_from_the_vault_config_and_both_modes_commit() {
    for (text, mode) in [
        ("", Durability::Standard),
        ("durability = \"standard\"\n", Durability::Standard),
        (
            "vault_id = \"x\"\ndurability = \"full\"\n",
            Durability::Full,
        ),
    ] {
        let (dir, vault) = new_vault();
        write_file(&vault, ".wiki-island/config.toml", text.as_bytes());
        let vault = Vault::open(dir.path()).unwrap();
        assert_eq!(vault.durability(), mode);
        save(&vault, "a.md", b"a", "cli", None);
        assert_eq!(Flusher::flush(&vault).unwrap().remaining, 0);
    }
    let (dir, vault) = new_vault();
    write_file(
        &vault,
        ".wiki-island/config.toml",
        b"durability = \"fast\"\n",
    );
    assert!(matches!(
        Vault::open(dir.path()),
        Err(HistoryError::InvalidConfig { .. })
    ));
    let vault = Vault::init(tempfile::tempdir().unwrap().path())
        .unwrap()
        .with_durability(Durability::Full);
    assert_eq!(vault.durability(), Durability::Full);
}

fn assert_many_matches_log(vault: &Vault, paths: &[&str]) {
    for limit in [None, Some(0), Some(1), Some(2), Some(100)] {
        let many = History::log_many(vault, paths, limit).unwrap();
        assert_eq!(many.len(), paths.len());
        for ((path, actual), requested) in many.into_iter().zip(paths) {
            assert_eq!(&path, requested);
            let mut expected = History::log(vault, requested).unwrap();
            expected.truncate(limit.unwrap_or(usize::MAX));
            assert_eq!(actual, expected, "path={path}, limit={limit:?}");
        }
    }
}

fn commit_external_changes(vault: &Vault, message: &str) {
    git(vault.root(), &["add", "-A"]);
    git(
        vault.root(),
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@localhost",
            "commit",
            "-qm",
            message,
        ],
    );
}

#[test]
fn history_many_matches_single_paths_across_renames_deletes_and_kinds() {
    let (_dir, vault) = new_vault();
    let paths = [
        "notes/a.md",
        "notes/b.md",
        "archive/a.md",
        "gone.md",
        "nested/child.md",
        "nested",
        "absent.md",
        "notes/a.md",
    ];
    assert_many_matches_log(&vault, &paths);
    assert!(History::log_many(&vault, &[], None).unwrap().is_empty());
    assert!(History::log_many(&vault, &["../escape"], Some(0)).is_err());
    for (path, bytes) in [
        ("notes/a.md", b"a".as_slice()),
        ("notes/b.md", b"b"),
        ("gone.md", b"gone"),
        ("nested/child.md", b"child"),
    ] {
        save(&vault, path, bytes, "cli", None);
    }
    save(&vault, "notes/a.md", b"new a", "mcp", None);
    Flusher::flush(&vault).unwrap();
    std::fs::create_dir(vault.root().join("archive")).unwrap();
    git(vault.root(), &["mv", "notes/a.md", "archive/a.md"]);
    git(vault.root(), &["rm", "gone.md", "nested/child.md"]);
    std::fs::write(vault.root().join("nested"), b"now a blob").unwrap();
    commit_external_changes(&vault, "move, delete and replace tree with blob");
    assert_many_matches_log(&vault, &paths);
    git(vault.root(), &["rm", "nested"]);
    std::fs::create_dir(vault.root().join("nested")).unwrap();
    std::fs::write(vault.root().join("nested/child.md"), b"restored child").unwrap();
    commit_external_changes(&vault, "replace blob with tree");
    assert_many_matches_log(&vault, &paths);
    let gone = History::log_many(&vault, &["gone.md"], None).unwrap();
    assert_eq!(gone[0].1.len(), 2);
    assert!(gone[0].1[0].blob.is_none());
}

#[test]
fn history_many_matches_every_parent_merge_semantics_and_walk_order() {
    let (_dir, vault) = new_vault();
    save(&vault, "a.md", b"base a", "cli", None);
    save(&vault, "b.md", b"base b", "cli", None);
    Flusher::flush(&vault).unwrap();
    let base = git(vault.root(), &["rev-parse", "HEAD"]).trim().to_owned();
    std::fs::write(vault.root().join("a.md"), b"left").unwrap();
    commit_external_changes(&vault, "left branch");
    let left = git(vault.root(), &["rev-parse", "HEAD"]).trim().to_owned();
    git(vault.root(), &["checkout", "-qb", "side", &base]);
    std::fs::write(vault.root().join("a.md"), b"right").unwrap();
    std::fs::write(vault.root().join("b.md"), b"right b").unwrap();
    commit_external_changes(&vault, "right branch");
    let right = git(vault.root(), &["rev-parse", "HEAD"]).trim().to_owned();
    std::fs::write(vault.root().join("a.md"), b"resolved").unwrap();
    git(vault.root(), &["add", "a.md"]);
    let tree = git(vault.root(), &["write-tree"]).trim().to_owned();
    let merge = git(
        vault.root(),
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@localhost",
            "commit-tree",
            &tree,
            "-p",
            &left,
            "-p",
            &right,
            "-m",
            "merge resolution",
        ],
    )
    .trim()
    .to_owned();
    git(vault.root(), &["update-ref", "HEAD", &merge]);
    assert_many_matches_log(&vault, &["a.md", "b.md", "missing.md"]);
    assert_eq!(History::log(&vault, "a.md").unwrap()[0].commit, merge);
    assert!(
        History::log(&vault, "b.md")
            .unwrap()
            .iter()
            .all(|e| e.commit != merge),
        "a blob identical to either parent is not a merge change"
    );
}

#[test]
fn owner_deletions_are_one_commit_and_skip_paths_that_came_back() {
    let (_dir, vault) = new_vault();
    save(&vault, "a.md", b"a\n", "cli", None);
    save(&vault, "b.md", b"b\n", "cli", None);
    save(&vault, "kept.md", b"k\n", "cli", None);
    Flusher::flush(&vault).unwrap();
    std::fs::remove_file(vault.root().join("a.md")).unwrap();
    std::fs::rename(vault.root().join("b.md"), vault.root().join("c.md")).unwrap();
    // `c.md` arrives as an unflushed ingest: committed before the deletion.
    ingest(&vault, "c.md", b"b\n", None);
    let deletions = [
        wi_history::Deletion {
            path: "a.md".into(),
            renamed_to: None,
        },
        wi_history::Deletion {
            path: "b.md".into(),
            renamed_to: Some("c.md".into()),
        },
        wi_history::Deletion {
            path: "kept.md".into(),
            renamed_to: None,
        },
    ];
    let (_, removed) = History::record_deletions(&vault, &deletions, "watcher")
        .unwrap()
        .unwrap();
    assert_eq!(removed, ["a.md", "b.md"], "kept.md is still on disk");
    let commits = commits(vault.root());
    let (_, message) = commits.last().unwrap();
    assert_eq!(trailer(message, "Wi-Actor"), vec!["watcher"]);
    assert_eq!(trailer(message, "Wi-Deleted"), vec!["a.md"]);
    assert_eq!(trailer(message, "Wi-Renamed"), vec!["b.md -> c.md"]);
    assert!(Missing::scan(&vault).unwrap().is_empty());
    assert_eq!(
        Manifest::snapshot_unlocked(&vault).unwrap().entries.len(),
        0
    );
    // Nothing left: a second call commits nothing.
    assert!(
        History::record_deletions(&vault, &deletions, "watcher")
            .unwrap()
            .is_none()
    );
    fsck(vault.root());
}

#[test]
fn merged_history_keeps_operations_and_losses_from_every_parent_once() {
    let (_dir, vault) = new_vault();
    let mut base = intent(&vault, "base.md", b"base\n", "engine", Some("base"));
    base.losses.push(serde_json::json!({"event":"base"}));
    save_record(&vault, base, b"base\n");
    Flusher::flush(&vault).unwrap();
    let root = History::head(&vault).unwrap().unwrap();
    let branch = git(vault.root(), &["symbolic-ref", "--short", "HEAD"])
        .trim()
        .to_owned();
    git(vault.root(), &["branch", "other", &root]);
    let mut first = intent(&vault, "first.md", b"first\n", "engine", Some("first"));
    first.losses.push(serde_json::json!({"event":"first"}));
    save_record(&vault, first, b"first\n");
    Flusher::flush(&vault).unwrap();
    git(vault.root(), &["checkout", "other"]);
    let mut second = intent(&vault, "second.md", b"second\n", "engine", Some("second"));
    second.losses.push(serde_json::json!({"event":"second"}));
    save_record(&vault, second, b"second\n");
    Flusher::flush(&vault).unwrap();
    git(vault.root(), &["checkout", &branch]);
    #[cfg(windows)]
    let hooks = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    let hooks_arg = format!("core.hooksPath={}", hooks.path().display());
    #[cfg(not(windows))]
    let hooks_arg = String::from("core.hooksPath=/dev/null");
    git(
        vault.root(),
        &[
            "-c",
            hooks_arg.as_str(),
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@localhost",
            "-c",
            "commit.gpgSign=false",
            "merge",
            "--no-ff",
            "other",
            "-m",
            "merge notes",
        ],
    );
    for op in ["base", "first", "second"] {
        assert!(History::find_operation(&vault, op).unwrap().is_some());
    }
    let scan = History::operations_since(&vault, Some(&root)).unwrap();
    assert!(
        !scan.reached_stop,
        "a new merge requests a complete disposable-map rebuild"
    );
    for op in ["base", "first", "second"] {
        assert_eq!(scan.operations.iter().filter(|t| t.op_id == op).count(), 1);
    }
    let mut losses = History::losses_since(&vault, 0).unwrap();
    losses.sort();
    assert_eq!(
        losses,
        ["base", "first", "second"].map(|event| serde_json::json!({"event":event}).to_string())
    );
}
