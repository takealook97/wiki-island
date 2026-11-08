mod common;

use std::cell::RefCell;
use std::rc::Rc;

use common::{Fixture, assert_deletion_committed, git, manifest_text, write};
use serde_json::json;
use wi_history::hash_bytes;
use wi_store::{
    BodyEdit, EditRequest, Hooks, IndexState, Ingress, MoveRequest, Query, SearchOptions,
    StoreError, WorkKind, WriteRequest,
};

fn replace(
    path: &str,
    content: &str,
    expected: Option<String>,
    op_id: Option<&str>,
) -> WriteRequest {
    WriteRequest {
        path: path.into(),
        content: content.as_bytes().to_vec(),
        expected_checksum: expected,
        op_id: op_id.map(str::to_owned),
        ingress: Ingress::Cli,
    }
}

fn edit(path: &str, set: Vec<(&str, serde_json::Value)>, ingress: Ingress) -> EditRequest {
    EditRequest {
        path: path.into(),
        set: set.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
        remove: Vec::new(),
        body: None,
        expected_checksum: None,
        op_id: None,
        ingress,
    }
}

#[test]
fn stale_expected_checksum_is_refused_and_changes_nothing() {
    let (fx, vault) = Fixture::new();
    let first = write(&vault, "a.md", "one\n", Ingress::Cli);
    let before_manifest = manifest_text(&vault);
    let generation = vault.generation().unwrap();
    let stale = hash_bytes(b"something else");
    let err = vault
        .write(replace("a.md", "two\n", Some(stale), None))
        .unwrap_err();
    assert!(matches!(err, StoreError::Stale { .. }), "{err}");
    assert_eq!(
        std::fs::read(vault.root().join("a.md")).unwrap(),
        vault.read("a.md").unwrap().bytes
    );
    assert_eq!(vault.read("a.md").unwrap().checksum, first.checksum);
    assert_eq!(manifest_text(&vault), before_manifest);
    assert_eq!(vault.generation().unwrap(), generation);
    // An edit with a stale checksum is refused too.
    let mut request = edit("a.md", vec![("x", json!(1))], Ingress::Cli);
    request.expected_checksum = Some(hash_bytes(b"old"));
    assert!(matches!(vault.edit(request), Err(StoreError::Stale { .. })));
    assert_eq!(vault.read("a.md").unwrap().checksum, first.checksum);
    drop(fx);
}

#[test]
fn write_without_expected_checksum_on_an_existing_path_is_refused() {
    let (_fx, vault) = Fixture::new();
    let first = write(&vault, "notes/a.md", "one\n", Ingress::Mcp);
    let err = vault
        .write(replace("notes/a.md", "two\n", None, None))
        .unwrap_err();
    match err {
        StoreError::Exists { current, .. } => assert_eq!(current, first.checksum),
        other => panic!("{other}"),
    }
    // With the checksum it replaces.
    let second = vault
        .write(replace(
            "notes/a.md",
            "two\n",
            Some(first.checksum.clone()),
            None,
        ))
        .unwrap();
    assert_eq!(second.previous_checksum, Some(first.checksum));
    let text = std::fs::read_to_string(vault.root().join("notes/a.md")).unwrap();
    assert!(
        text.ends_with("---\ntwo\n") && text.contains("kind: agent"),
        "origin kept: {text}"
    );
}

#[test]
fn operation_id_replays_the_first_result_across_changes_restart_and_index_loss() {
    let (fx, vault) = Fixture::new();
    let request = replace("op.md", "first\n", None, Some("op-1"));
    let first = vault.write(request.clone()).unwrap();
    assert!(!first.replayed);
    // Same id and digest after the file changed: the first result, nothing written.
    std::fs::write(vault.root().join("op.md"), b"edited by hand\n").unwrap();
    let again = vault.write(request.clone()).unwrap();
    assert!(again.replayed);
    assert_eq!(again.checksum, first.checksum);
    assert_eq!(again.seq, first.seq);
    assert_eq!(
        std::fs::read(vault.root().join("op.md")).unwrap(),
        b"edited by hand\n"
    );
    // A different digest is refused.
    let other = vault.write(replace("op.md", "different\n", None, Some("op-1")));
    assert!(
        matches!(other, Err(StoreError::OperationMismatch { .. })),
        "{other:?}"
    );
    // After commit (record retired), restart and index deletion: from the trailer.
    let flushed = vault.flush().unwrap();
    assert!(!flushed.commits.is_empty());
    drop(vault);
    std::fs::remove_file(fx.cache.path().join("index.sqlite3")).unwrap();
    let reopened = fx.open();
    let replay = reopened.write(request).unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.checksum, first.checksum);
    assert_eq!(replay.seq, first.seq);
    assert!(git(reopened.root(), &["log", "--format=%B"]).contains("Wi-Operation: op-1 "));
    let other = reopened.write(replace("op.md", "different\n", None, Some("op-1")));
    assert!(matches!(other, Err(StoreError::OperationMismatch { .. })));
}

#[test]
fn conflict_markers_get_flag_and_work_item_and_the_engine_refuses_to_write() {
    let (_fx, vault) = Fixture::new();
    let first = write(&vault, "c.md", "base text\n", Ingress::Cli);
    vault.flush().unwrap();
    let conflicted = "<<<<<<< HEAD\nours conflictword\n=======\ntheirs\n>>>>>>> branch\n";
    std::fs::write(vault.root().join("c.md"), conflicted).unwrap();
    let report = vault.ingest_path("c.md").unwrap();
    assert!(report.conflict_markers);
    let work = vault.work_requests().unwrap();
    assert!(
        work.iter()
            .any(|w| w.kind == WorkKind::MergeConflict && w.path == "c.md")
    );
    let hits = vault
        .search(
            &Query::text("conflictword"),
            SearchOptions { catch_up: true },
        )
        .unwrap();
    assert_eq!(hits.hits.len(), 1);
    assert!(hits.hits[0].conflict_markers, "index flag");
    let current = hash_bytes(conflicted.as_bytes());
    let refused = vault.write(replace("c.md", "fixed\n", Some(current), None));
    assert!(
        matches!(refused, Err(StoreError::ConflictMarkers { .. })),
        "{refused:?}"
    );
    let refused = vault.edit(edit("c.md", vec![("x", json!(1))], Ingress::Cli));
    assert!(matches!(refused, Err(StoreError::ConflictMarkers { .. })));
    assert_eq!(
        std::fs::read_to_string(vault.root().join("c.md")).unwrap(),
        conflicted
    );
    let _ = first;
}

#[test]
fn editor_save_between_check_and_swap_is_kept_as_a_conflict_copy() {
    const {
        assert!(
            wi_store::EXCHANGE_SUPPORTED,
            "this platform has no atomic exchange"
        )
    };
    let (_fx, mut vault) = Fixture::new();
    let first = write(&vault, "race.md", "original\n", Ingress::Cli);
    vault.set_hooks(Hooks {
        before_swap: Some(Rc::new(|target| {
            std::fs::write(target, b"editor save\n").unwrap();
        })),
        ..Hooks::default()
    });
    let result = vault
        .write(replace("race.md", "engine\n", Some(first.checksum), None))
        .unwrap();
    let copy = result.conflict_copy.clone().expect("conflict copy");
    let text = std::fs::read_to_string(vault.root().join("race.md")).unwrap();
    assert!(text.ends_with("---\nengine\n"), "{text}");
    assert_eq!(
        std::fs::read(vault.root().join(&copy)).unwrap(),
        b"editor save\n"
    );
    assert!(copy.starts_with(".race.md.wi-conflict-"), "{copy}");
    let work = vault.work_requests().unwrap();
    assert!(
        work.iter()
            .any(|w| w.kind == WorkKind::ConflictCopy && w.path == "race.md")
    );
    // Create race: an editor creates the file after the absence check.
    let result = vault
        .write(replace("created.md", "engine create\n", None, None))
        .unwrap();
    let copy = result.conflict_copy.expect("create race conflict copy");
    assert_eq!(
        std::fs::read(vault.root().join(&copy)).unwrap(),
        b"editor save\n"
    );
}

#[test]
fn no_conflict_copy_when_nothing_raced() {
    let (_fx, vault) = Fixture::new();
    let first = write(&vault, "calm.md", "a\n", Ingress::Cli);
    let second = vault
        .write(replace("calm.md", "b\n", Some(first.checksum), None))
        .unwrap();
    assert!(second.conflict_copy.is_none());
    let leftovers: Vec<_> = std::fs::read_dir(vault.root())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".wi-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn protected_paths_are_refused_and_change_nothing() {
    let (_fx, vault) = Fixture::new();
    let config = std::fs::read(vault.root().join(".wiki-island/config.toml")).unwrap();
    std::fs::write(
        vault.root().join(".wiki-island/config.toml"),
        [config.as_slice(), b"export_paths = [\"exports\"]\n"].concat(),
    )
    .unwrap();
    let vault = Fixture::open_existing(&vault);
    let outside = vault.root().parent().unwrap().join("outside.md");
    std::fs::create_dir_all(vault.root().join("real")).unwrap();
    std::os::unix::fs::symlink(vault.root().join("real"), vault.root().join("link")).unwrap();
    std::fs::write(vault.root().join("real/target.md"), b"t\n").unwrap();
    std::os::unix::fs::symlink(
        vault.root().join("real/target.md"),
        vault.root().join("file-link.md"),
    )
    .unwrap();
    let manifest = manifest_text(&vault);
    for path in [
        ".wiki-island/config.toml".to_owned(),
        ".wiki-island/other.md".to_owned(),
        "exports/rules.md".to_owned(),
        ".git/x.md".to_owned(),
        "sub/.git/x.md".to_owned(),
        "../outside.md".to_owned(),
        outside.display().to_string(),
        "link/new.md".to_owned(),
        "file-link.md".to_owned(),
        ".a.md.wi-conflict-3".to_owned(),
    ] {
        let err = vault.write(replace(&path, "x\n", None, None)).unwrap_err();
        assert!(matches!(err, StoreError::Protected { .. }), "{path}: {err}");
    }
    assert!(!outside.exists());
    assert!(!vault.root().join("real/new.md").exists());
    assert_eq!(
        std::fs::read(vault.root().join("real/target.md")).unwrap(),
        b"t\n"
    );
    assert_eq!(manifest_text(&vault), manifest);
    // Edits, moves and archives use the same checks.
    write(&vault, "ok.md", "ok\n", Ingress::Cli);
    let moved = vault.move_note(MoveRequest {
        from: "ok.md".into(),
        to: ".git/ok.md".into(),
        expected_checksum: None,
        op_id: None,
        ingress: Ingress::Cli,
    });
    assert!(matches!(moved, Err(StoreError::Protected { .. })));
}

#[test]
fn hit_changed_after_indexing_is_withheld_and_results_carry_index_as_of() {
    let (_fx, vault) = Fixture::new();
    write(
        &vault,
        "deep/nested/n.md",
        "offline keyword\n",
        Ingress::Cli,
    );
    let first = vault
        .search(&Query::text("keyword"), SearchOptions { catch_up: true })
        .unwrap();
    assert_eq!(first.hits.len(), 1);
    assert!(first.index_as_of.is_some());
    // Changed after indexing, no catch-up: re-hashed and withheld.
    std::fs::write(
        vault.root().join("deep/nested/n.md"),
        b"offline keyword changed\n",
    )
    .unwrap();
    let second = vault
        .search(&Query::text("keyword"), SearchOptions { catch_up: false })
        .unwrap();
    assert!(second.hits.is_empty());
    assert_eq!(second.withheld[0].reason, "changed-after-index");
    // The withheld path was re-indexed, so the next read sees it again.
    let third = vault
        .search(&Query::text("changed"), SearchOptions { catch_up: false })
        .unwrap();
    assert_eq!(third.hits.len(), 1);
}

#[test]
fn generation_change_during_a_read_retries_once_then_reports() {
    let (fx, mut vault) = Fixture::new();
    write(&vault, "a.md", "alpha one\n", Ingress::Cli);
    write(&vault, "b.md", "alpha two\n", Ingress::Cli);
    vault.catch_up().unwrap();
    let writer = Rc::new(fx.open());
    let changes = Rc::new(RefCell::new(Vec::<u32>::new()));
    let (w, c) = (writer.clone(), changes.clone());
    // Attempt 0: A is superseded (its text no longer matches) during the read.
    vault.set_hooks(Hooks {
        during_read: Some(Rc::new(move |attempt| {
            if attempt == 0 {
                let vault = &w;
                let current = vault.read("a.md").unwrap().checksum;
                vault
                    .write(replace("a.md", "omega one\n", Some(current), None))
                    .unwrap();
                c.borrow_mut().push(attempt);
            }
        })),
        ..Hooks::default()
    });
    let result = vault
        .search(&Query::text("alpha"), SearchOptions::default())
        .unwrap();
    assert_eq!(changes.borrow().as_slice(), &[0]);
    let paths: Vec<_> = result
        .hits
        .iter()
        .map(|h| h.rel_path.to_string_lossy().into_owned())
        .collect();
    assert_eq!(paths, vec!["b.md"], "A is withheld after the retry");
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    assert_eq!(result.generation, vault.generation().unwrap());
    // A change on both attempts yields the diagnostic.
    let w = writer.clone();
    vault.set_hooks(Hooks {
        during_read: Some(Rc::new(move |attempt| {
            let vault = &w;
            let current = vault.read("b.md").unwrap().checksum;
            vault
                .write(replace(
                    "b.md",
                    &format!("alpha two {attempt}\n"),
                    Some(current),
                    None,
                ))
                .unwrap();
        })),
        ..Hooks::default()
    });
    let result = vault
        .search(&Query::text("alpha"), SearchOptions::default())
        .unwrap();
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d == wi_store::GENERATION_CHANGED)
    );
    // Plain reads carry the generation too.
    let read = vault.read("a.md").unwrap();
    assert_eq!(read.generation, vault.generation().unwrap());
}

#[test]
fn locked_notes_refuse_engine_edits_moves_and_replaces() {
    let (_fx, vault) = Fixture::new();
    std::fs::write(
        vault.root().join("locked.md"),
        b"---\nlocked: true\n---\nbody\n",
    )
    .unwrap();
    let current = hash_bytes(b"---\nlocked: true\n---\nbody\n");
    assert!(matches!(
        vault.edit(edit("locked.md", vec![("x", json!(1))], Ingress::Cli)),
        Err(StoreError::Locked { .. })
    ));
    assert!(matches!(
        vault.write(replace("locked.md", "x\n", Some(current), None)),
        Err(StoreError::Locked { .. })
    ));
    assert!(matches!(
        vault.archive("locked.md", None, None, Ingress::Cli),
        Err(StoreError::Locked { .. })
    ));
}

#[test]
fn engine_owned_fields_are_refused_through_tools_and_origin_follows_ingress() {
    let (_fx, vault) = Fixture::new();
    let created = write(
        &vault,
        "m.md",
        "---\ntype: memory\nsummary: s\n---\nbody\n",
        Ingress::Mcp,
    );
    let text = std::fs::read_to_string(vault.root().join("m.md")).unwrap();
    assert!(text.contains("kind: agent"), "{text}");
    for field in [
        "status",
        "adoption",
        "check",
        "conflict_with",
        "origin",
        "excluded",
        "restores",
    ] {
        for ingress in [Ingress::Cli, Ingress::Mcp] {
            let err = vault
                .edit(edit("m.md", vec![(field, json!("x"))], ingress))
                .unwrap_err();
            assert!(
                matches!(err, StoreError::EngineOwnedField { .. }),
                "{field}: {err}"
            );
        }
    }
    // Creation may not set status, but may set restores.
    let err = vault
        .write(replace("n.md", "---\nstatus: active\n---\n", None, None))
        .unwrap_err();
    assert!(matches!(err, StoreError::EngineOwnedField { .. }));
    write(
        &vault,
        "r.md",
        "---\nrestores: {ref: m.md, commit: null}\n---\n",
        Ingress::Cli,
    );
    // A replace that changes origin is refused.
    let replaced = text.replace("kind: agent", "kind: user");
    let err = vault
        .write(replace(
            "m.md",
            &replaced,
            Some(created.checksum.clone()),
            None,
        ))
        .unwrap_err();
    assert!(matches!(err, StoreError::EngineOwnedField { .. }), "{err}");
    // MCP may not change claim content of a lifecycle note in place.
    let err = vault
        .edit(edit(
            "m.md",
            vec![("summary", json!("other"))],
            Ingress::Mcp,
        ))
        .unwrap_err();
    assert!(
        matches!(err, StoreError::ClaimEditRequiresCandidate { .. }),
        "{err}"
    );
    // The CLI (owner) may.
    vault
        .edit(edit(
            "m.md",
            vec![("summary", json!("other"))],
            Ingress::Cli,
        ))
        .unwrap();
    // The engine may set engine-owned fields.
    vault
        .edit(edit(
            "m.md",
            vec![("status", json!("candidate"))],
            Ingress::Engine,
        ))
        .unwrap();
}

#[test]
fn file_ingress_of_a_claim_change_keeps_written_origin_and_resets_candidate() {
    let (_fx, vault) = Fixture::new();
    write(
        &vault,
        "claim.md",
        // A pin to a missing file fails the sources gate, which user
        // completion cannot fill, so the reset note stays a candidate.
        "---\ntype: memory\nsummary: one\nsources: [{ref: missing.md, commit: null, pending: true, source_hash: abc}]\n---\n",
        Ingress::Mcp,
    );
    vault
        .edit(edit(
            "claim.md",
            vec![("status", json!("active"))],
            Ingress::Engine,
        ))
        .unwrap();
    let text = std::fs::read_to_string(vault.root().join("claim.md")).unwrap();
    std::fs::write(
        vault.root().join("claim.md"),
        text.replace("summary: one", "summary: two"),
    )
    .unwrap();
    let report = vault.ingest_path("claim.md").unwrap();
    assert!(matches!(
        report.outcome,
        wi_store::IngestOutcome::Ingested { .. }
    ));
    let update = report.lifecycle_update.expect("engine bookkeeping write");
    assert!(!update.replayed);
    let text = std::fs::read_to_string(vault.root().join("claim.md")).unwrap();
    assert!(text.contains("status: candidate"), "{text}");
    // Provenance comes from the note: the agent label stays (S3).
    assert!(
        text.contains("kind: agent") && !text.contains("kind: user"),
        "{text}"
    );
    assert!(text.contains("summary: two"), "{text}");
    assert!(
        vault
            .work_requests()
            .unwrap()
            .iter()
            .any(|w| w.kind == WorkKind::Gate)
    );
    // The engine's own write is correlated, not ingested again.
    let again = vault.ingest_path("claim.md").unwrap();
    assert_eq!(again.outcome, wi_store::IngestOutcome::Unchanged);
}

#[test]
fn move_keeps_the_permalink_and_archive_moves_under_archive() {
    let (_fx, vault) = Fixture::new();
    write(
        &vault,
        "notes/Payment Policy.md",
        "---\ntitle: Payment\n---\nbody\n",
        Ingress::Cli,
    );
    let moved = vault
        .move_note(MoveRequest {
            from: "notes/Payment Policy.md".into(),
            to: "policies/payment.md".into(),
            expected_checksum: None,
            op_id: None,
            ingress: Ingress::Cli,
        })
        .unwrap();
    assert_eq!(moved.moved_from.as_deref(), Some("notes/Payment Policy.md"));
    assert!(!vault.root().join("notes/Payment Policy.md").exists());
    let text = std::fs::read_to_string(vault.root().join("policies/payment.md")).unwrap();
    assert!(text.contains("permalink: notes/payment-policy"), "{text}");
    let archived = vault
        .archive("policies/payment.md", None, None, Ingress::Cli)
        .unwrap();
    assert_eq!(archived.path, "archive/policies/payment.md");
    vault.flush().unwrap();
    let log = git(vault.root(), &["log", "--format=%B"]);
    assert!(log.contains("Wi-Moved-From: policies/payment.md"), "{log}");
    let tree = git(vault.root(), &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(tree.contains("archive/policies/payment.md"));
    assert!(!tree.contains("notes/Payment Policy.md"));
}

#[test]
fn body_edits_preserve_frontmatter_bytes() {
    let (_fx, vault) = Fixture::new();
    let original = "---\ncustom_key: \"Keep: exactly\"\nunknown_list: [z, a]\n---\nhello world\n";
    write(&vault, "e.md", original, Ingress::Engine);
    let mut request = edit("e.md", vec![], Ingress::Cli);
    request.body = Some(BodyEdit::FindReplace {
        find: "world".into(),
        replace: "there".into(),
        expected_replacements: 1,
    });
    vault.edit(request).unwrap();
    let mut request = edit("e.md", vec![], Ingress::Cli);
    request.body = Some(BodyEdit::Append {
        content: "tail".into(),
    });
    vault.edit(request).unwrap();
    assert_eq!(
        std::fs::read_to_string(vault.root().join("e.md")).unwrap(),
        "---\ncustom_key: \"Keep: exactly\"\nunknown_list: [z, a]\n---\nhello there\ntail\n"
    );
}

#[test]
fn end_to_end_write_search_commit_history_revert() {
    let (_fx, vault) = Fixture::new();
    let v1 = write(&vault, "doc.md", "결제 version one\n", Ingress::Cli);
    let found = vault
        .search(&Query::text("결제"), SearchOptions { catch_up: true })
        .unwrap();
    assert_eq!(found.hits.len(), 1);
    assert!(found.generation >= v1.seq);
    vault.flush().unwrap();
    let v2 = vault
        .write(replace(
            "doc.md",
            "결제 version two\n",
            Some(v1.checksum.clone()),
            None,
        ))
        .unwrap();
    assert_eq!(v2.index, IndexState::Ok);
    vault.flush().unwrap();
    let history = vault.history("doc.md").unwrap();
    assert_eq!(history.len(), 2);
    let oldest = history.last().unwrap().commit.clone();
    let reverted = vault.revert("doc.md", &oldest, Ingress::Cli).unwrap();
    assert_eq!(reverted.checksum, v1.checksum);
    vault.flush().unwrap();
    let log = git(vault.root(), &["log", "--format=%B", "-1"]);
    assert!(log.contains(&format!("Wi-Revert: {oldest}")), "{log}");
    assert_eq!(vault.history("doc.md").unwrap().len(), 3);
    git(vault.root(), &["fsck", "--strict", "--no-dangling"]);
}

#[test]
fn reconcile_ingests_offline_edits_and_commits_deleted_files() {
    let (_fx, vault) = Fixture::new();
    write(&vault, "keep.md", "k\n", Ingress::Cli);
    write(&vault, "gone.md", "g\n", Ingress::Cli);
    vault.flush().unwrap();
    std::fs::write(vault.root().join("keep.md"), b"k2\n").unwrap();
    std::fs::write(vault.root().join("new.md"), b"n\n").unwrap();
    std::fs::remove_file(vault.root().join("gone.md")).unwrap();
    let report = vault.reconcile().unwrap();
    let ingested: Vec<_> = report.ingested.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(ingested, vec!["keep.md", "new.md"]);
    assert_eq!(report.missing, vec!["gone.md"]);
    assert_deletion_committed(&vault, "gone.md");
    vault.flush().unwrap();
    let log = git(vault.root(), &["log", "--format=%B", "-3"]);
    assert!(log.contains("Wi-Actor: watcher"), "{log}");
    // Nothing left to ingest.
    assert!(vault.reconcile().unwrap().ingested.is_empty());
}

impl Fixture {
    fn open_existing(vault: &wi_store::Vault) -> wi_store::Vault {
        wi_store::Vault::open_with(vault.root(), wi_store::OpenOptions::default()).unwrap()
    }
}
