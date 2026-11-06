//! Regressions for path identity, provenance by ingress, degraded index,
//! lifecycle revert, first-result replay, read consistency and the op-id map.

mod common;

use std::rc::Rc;

use common::{Fixture, assert_deletion_committed, manifest_text, write};
use serde_json::json;
use wi_store::{
    EditRequest, Hooks, IndexState, Ingress, MoveRequest, StoreError, Vault, WriteRequest,
};

fn request(
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

fn text(vault: &Vault, rel: &str) -> String {
    std::fs::read_to_string(vault.root().join(rel)).unwrap()
}

fn set_config(fx: &Fixture, vault: &Vault, line: &str) -> Vault {
    let path = vault.root().join(".wiki-island/config.toml");
    let mut config = std::fs::read_to_string(&path).unwrap();
    config.push_str(line);
    std::fs::write(&path, config).unwrap();
    fx.open()
}

#[test]
fn filesystem_equivalent_protected_names_are_refused_before_any_change() {
    let (fx, vault) = Fixture::new();
    let vault = set_config(&fx, &vault, "export_paths = [\"exports\", \"caf\u{e9}\"]\n");
    vault.flush().unwrap();
    std::fs::create_dir_all(vault.root().join("exports")).unwrap();
    std::fs::create_dir_all(vault.root().join("caf\u{e9}")).unwrap();
    let before = manifest_text(&vault);
    let generation = vault.generation().unwrap();
    for path in [
        ".GIT/evil.md",
        ".Git/x.md",
        "notes/.gIT/x.md",
        ".Wiki-Island/internal.md",
        ".WIKI-ISLAND/config.toml",
        "Exports/leak.md",
        "EXPORTS/leak.md",
        "cafe\u{301}/leak.md",
        "CAFE\u{301}/leak.md",
        "Caf\u{c9}/leak.md",
    ] {
        let err = vault.write(request(path, "x\n", None, None)).unwrap_err();
        assert!(matches!(err, StoreError::Protected { .. }), "{path}: {err}");
        if path.to_lowercase().contains(".git") {
            assert!(vault.read(path).is_err(), "{path} readable");
        }
    }
    assert_eq!(manifest_text(&vault), before);
    assert_eq!(vault.generation().unwrap(), generation);
    assert!(!vault.root().join(".git/evil.md").exists());
    assert!(!vault.root().join(".wiki-island/internal.md").exists());
    assert!(!vault.root().join("exports/leak.md").exists());
    // The vault keeps working after the refusals.
    write(&vault, "ok.md", "fine\n", Ingress::Cli);
    let report = vault.flush().unwrap();
    assert!(report.blocked.is_none(), "{report:?}");
    assert!(fx.open().doctor().unwrap().healthy());
}

#[test]
fn absolute_path_through_an_in_vault_symlink_is_refused() {
    let (_fx, vault) = Fixture::new();
    std::fs::create_dir_all(vault.root().join("real")).unwrap();
    std::os::unix::fs::symlink(vault.root().join("real"), vault.root().join("alias")).unwrap();
    let absolute = vault.root().join("alias/b.md");
    for path in ["alias/a.md", absolute.to_str().unwrap()] {
        let err = vault.write(request(path, "x\n", None, None)).unwrap_err();
        assert!(matches!(err, StoreError::Protected { .. }), "{path}: {err}");
    }
    assert!(!vault.root().join("real/a.md").exists());
    assert!(!vault.root().join("real/b.md").exists());
    // An absolute path through a real directory still works, as does the
    // system alias of the temp root (`/var` → `/private/var` on macOS).
    let real = vault.root().join("real/c.md");
    let result = vault
        .write(request(real.to_str().unwrap(), "x\n", None, None))
        .unwrap();
    assert_eq!(result.path, "real/c.md");
}

fn engine_note(vault: &Vault, rel: &str, content: &str) -> String {
    vault
        .write(WriteRequest {
            ingress: Ingress::Engine,
            ..request(rel, content, None, None)
        })
        .unwrap()
        .checksum
}

const USER_RULE: &str = "---\ntype: memory\nsummary: owner rule\norigin:\n  kind: user\n  locator: file\nstatus: active\n---\nbody\n";

#[test]
fn mcp_cannot_detour_through_a_plain_type_to_change_a_claim() {
    let (_fx, vault) = Fixture::new();
    engine_note(&vault, "rule.md", USER_RULE);
    // Step 1 of the detour (demote to a plain type) is refused.
    let err = vault
        .edit(edit("rule.md", vec![("type", json!("note"))], Ingress::Mcp))
        .unwrap_err();
    assert!(
        matches!(&err, StoreError::ClaimEditRequiresCandidate { fields, .. } if fields.contains(&"type".to_owned())),
        "{err}"
    );
    // A replace that changes the type away is refused the same way.
    let current = vault.read("rule.md").unwrap().checksum;
    let err = vault
        .write(WriteRequest {
            ingress: Ingress::Mcp,
            ..request(
                "rule.md",
                "---\ntype: note\nsummary: agent text\n---\nbody\n",
                Some(current),
                None,
            )
        })
        .unwrap_err();
    assert!(
        matches!(err, StoreError::ClaimEditRequiresCandidate { .. }),
        "{err}"
    );
    assert_eq!(text(&vault, "rule.md"), USER_RULE);
}

#[test]
fn mcp_promotion_of_a_plain_note_never_inherits_its_origin() {
    let (_fx, vault) = Fixture::new();
    let created = write(
        &vault,
        "plain.md",
        "---\nsummary: owner text\n---\nbody\n",
        Ingress::Cli,
    );
    assert!(text(&vault, "plain.md").contains("kind: user"));
    // Promotion with changed claim content is refused for MCP.
    let err = vault
        .write(WriteRequest {
            ingress: Ingress::Mcp,
            ..request(
                "plain.md",
                "---\ntype: memory\nsummary: agent text\n---\nbody\n",
                Some(created.checksum.clone()),
                None,
            )
        })
        .unwrap_err();
    assert!(
        matches!(err, StoreError::ClaimEditRequiresCandidate { .. }),
        "{err}"
    );
    // Promotion without a claim change is a creation: origin is the agent's.
    vault
        .edit(edit(
            "plain.md",
            vec![("type", json!("memory"))],
            Ingress::Mcp,
        ))
        .unwrap();
    let now = text(&vault, "plain.md");
    assert!(
        now.contains("kind: agent") && !now.contains("kind: user"),
        "{now}"
    );
    assert!(now.contains("mcp:edit"), "{now}");
    // And then the claim is the agent's to keep but not to change in place.
    let err = vault
        .edit(edit(
            "plain.md",
            vec![("summary", json!("x"))],
            Ingress::Mcp,
        ))
        .unwrap_err();
    assert!(matches!(err, StoreError::ClaimEditRequiresCandidate { .. }));
}

#[test]
fn cli_claim_change_sets_user_origin_and_resets_candidate() {
    let (_fx, vault) = Fixture::new();
    engine_note(
        &vault,
        "m.md",
        "---\ntype: memory\nsummary: agent\norigin:\n  kind: agent\n  locator: mcp:write\nstatus: active\n---\nbody\n",
    );
    vault
        .edit(edit(
            "m.md",
            vec![("summary", json!("owner-authored"))],
            Ingress::Cli,
        ))
        .unwrap();
    let now = text(&vault, "m.md");
    assert!(
        now.contains("kind: user") && now.contains("cli:edit"),
        "{now}"
    );
    assert!(now.contains("status: candidate"), "{now}");
    assert!(now.contains("summary: owner-authored"), "{now}");
    // A full CLI replace of the claim does the same.
    engine_note(
        &vault,
        "w.md",
        "---\ntype: memory\nsummary: agent\norigin:\n  kind: agent\n  locator: mcp:write\nstatus: active\n---\n",
    );
    let current = vault.read("w.md").unwrap().checksum;
    vault
        .write(request(
            "w.md",
            "---\ntype: memory\nsummary: mine\n---\n",
            Some(current),
            None,
        ))
        .unwrap();
    let now = text(&vault, "w.md");
    assert!(
        now.contains("kind: user") && now.contains("status: candidate"),
        "{now}"
    );
    // A body-only CLI edit keeps provenance.
    let before = text(&vault, "m.md");
    vault
        .edit(EditRequest {
            body: Some(wi_store::BodyEdit::Append {
                content: "more".into(),
            }),
            ..edit("m.md", vec![], Ingress::Cli)
        })
        .unwrap();
    assert!(text(&vault, "m.md").starts_with(before.split("body").next().unwrap()));
}

#[test]
fn pending_index_repair_never_blocks_the_vault() {
    let (fx, vault) = Fixture::new();
    write(&vault, "a.md", "alpha words\n", Ingress::Cli);
    let db = fx.cache.path().join("index.sqlite3");
    vault.close_index();
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
    }
    std::fs::create_dir(&db).unwrap();
    let pending = write(&vault, "b.md", "beta words\n", Ingress::Cli);
    assert_eq!(pending.index, IndexState::Pending);
    assert!(!vault.index_pending().is_empty());
    drop(vault);

    let vault = fx.open();
    assert!(vault.open_report().index_degraded.is_some());
    assert!(!vault.index_pending().is_empty(), "marker kept");
    assert_eq!(vault.read("b.md").unwrap().checksum, pending.checksum);
    let next = write(&vault, "c.md", "gamma words\n", Ingress::Cli);
    assert_eq!(next.index, IndexState::Pending);
    let found = vault
        .search(
            &wi_store::Query::text("beta"),
            wi_store::SearchOptions::default(),
        )
        .unwrap();
    assert!(found.hits.iter().any(|h| h.rel_path.ends_with("b.md")));
    let report = vault.doctor().unwrap();
    assert!(
        report
            .checks
            .iter()
            .any(|c| c.name == "index" && c.status == "warn")
    );
    // `wi reindex` discards the unusable database and clears the marker.
    vault.reindex().unwrap();
    assert!(vault.index_pending().is_empty());
    drop(vault);
    let vault = fx.open();
    assert!(vault.open_report().index_degraded.is_none());
}

#[test]
fn lifecycle_revert_is_refused_and_missing_revert_uses_creation_checks() {
    let (_fx, vault) = Fixture::new();
    engine_note(&vault, "rule.md", USER_RULE);
    vault.flush().unwrap();
    let commit = vault.history("rule.md").unwrap()[0].commit.clone();
    for ingress in [Ingress::Cli, Ingress::Mcp] {
        let err = vault.revert("rule.md", &commit, ingress).unwrap_err();
        assert!(
            matches!(err, StoreError::LifecycleRevertRequiresFork { .. }),
            "{err}"
        );
        assert_eq!(err.code(), "lifecycle-revert-requires-fork");
    }
    // Missing lifecycle note: still refused, never recreated verbatim.
    std::fs::remove_file(vault.root().join("rule.md")).unwrap();
    let err = vault.revert("rule.md", &commit, Ingress::Mcp).unwrap_err();
    assert!(
        matches!(err, StoreError::LifecycleRevertRequiresFork { .. }),
        "{err}"
    );
    assert!(!vault.root().join("rule.md").exists());
    // Missing plain note: restored as a creation with this ingress's origin.
    write(&vault, "plain.md", "plain\n", Ingress::Cli);
    vault.flush().unwrap();
    let commit = vault.history("plain.md").unwrap()[0].commit.clone();
    std::fs::remove_file(vault.root().join("plain.md")).unwrap();
    vault.revert("plain.md", &commit, Ingress::Mcp).unwrap();
    let now = text(&vault, "plain.md");
    assert!(
        now.contains("kind: agent") && !now.contains("kind: user"),
        "{now}"
    );
}

#[test]
fn replay_returns_the_first_result_after_retirement() {
    let (fx, mut vault) = Fixture::new();
    let first = write(&vault, "a.md", "one\n", Ingress::Cli);
    vault.flush().unwrap();
    vault.set_hooks(Hooks {
        before_swap: Some(Rc::new(|target| {
            std::fs::write(target, b"external late save\n").unwrap();
        })),
        ..Hooks::default()
    });
    let conflict = vault
        .write(request(
            "a.md",
            "two\n",
            Some(first.checksum.clone()),
            Some("conflict-result"),
        ))
        .unwrap();
    assert!(conflict.conflict_copy.is_some());
    assert_eq!(
        conflict.previous_checksum.as_deref(),
        Some(first.checksum.as_str())
    );
    vault.set_hooks(Hooks::default());
    let moved = vault
        .move_note(MoveRequest {
            from: "a.md".into(),
            to: "b.md".into(),
            expected_checksum: None,
            op_id: Some("move-result".into()),
            ingress: Ingress::Cli,
        })
        .unwrap();
    assert_eq!(moved.moved_from.as_deref(), Some("a.md"));
    vault.flush().unwrap();
    drop(vault);

    let same = |a: &wi_store::WriteResult, b: &wi_store::WriteResult| {
        assert_eq!(
            (
                &a.path,
                &a.checksum,
                &a.previous_checksum,
                a.seq,
                &a.conflict_copy,
                &a.moved_from
            ),
            (
                &b.path,
                &b.checksum,
                &b.previous_checksum,
                b.seq,
                &b.conflict_copy,
                &b.moved_from
            )
        );
        assert!(b.replayed);
    };
    let vault = fx.open();
    let replay = vault
        .write(request(
            "a.md",
            "two\n",
            Some(first.checksum.clone()),
            Some("conflict-result"),
        ))
        .unwrap();
    same(&conflict, &replay);
    let replay = vault
        .move_note(MoveRequest {
            from: "a.md".into(),
            to: "b.md".into(),
            expected_checksum: None,
            op_id: Some("move-result".into()),
            ingress: Ingress::Cli,
        })
        .unwrap();
    same(&moved, &replay);
    // Without the stored results (another device's clone), the commit
    // trailers and work requests still give path, checksum, conflict copy
    // and moved-from; the precondition hash is not in history.
    std::fs::remove_dir_all(vault.root().join(".git/wi/ops")).unwrap();
    let replay = vault
        .write(request(
            "a.md",
            "two\n",
            Some(first.checksum.clone()),
            Some("conflict-result"),
        ))
        .unwrap();
    assert_eq!(replay.conflict_copy, conflict.conflict_copy);
    assert_eq!(replay.checksum, conflict.checksum);
    assert_eq!(replay.previous_checksum, None);
    let err = vault
        .write(request("a.md", "other\n", None, Some("move-result")))
        .unwrap_err();
    assert!(matches!(err, StoreError::OperationMismatch { .. }));
}

#[test]
fn read_during_a_live_intent_reports_generation_changed() {
    let (fx, mut writer) = Fixture::new();
    let first = write(&writer, "a.md", "old\n", Ingress::Cli);
    writer.flush().unwrap();
    let reader = Rc::new(fx.open());
    let seen = Rc::new(std::cell::RefCell::new(None));
    let (r, s) = (reader.clone(), seen.clone());
    writer.set_hooks(Hooks {
        after_swap: Some(Rc::new(move |_| {
            *s.borrow_mut() = Some(r.read("a.md").unwrap());
        })),
        ..Hooks::default()
    });
    let written = writer
        .write(request("a.md", "new\n", Some(first.checksum), None))
        .unwrap();
    let read = seen.borrow_mut().take().unwrap();
    assert_eq!(read.checksum, written.checksum);
    assert_eq!(
        read.diagnostics,
        vec![wi_store::GENERATION_CHANGED.to_owned()]
    );
    // After `done` the same read is consistent.
    let read = reader.read("a.md").unwrap();
    assert!(read.diagnostics.is_empty());
    assert_eq!(read.generation, writer.generation().unwrap());
}

#[test]
fn reindex_commits_owner_deletions() {
    let (_fx, vault) = Fixture::new();
    write(&vault, "gone.md", "bye\n", Ingress::Cli);
    vault.flush().unwrap();
    std::fs::remove_file(vault.root().join("gone.md")).unwrap();
    let (_, missing) = vault.reindex_with_missing().unwrap();
    assert_eq!(missing, vec!["gone.md".to_owned()]);
    assert_deletion_committed(&vault, "gone.md");
}

#[test]
fn op_id_map_is_incremental_and_falls_back_without_an_index() {
    let (fx, vault) = Fixture::new();
    for i in 0..5 {
        vault
            .write(request(
                &format!("n{i}.md"),
                "x\n",
                None,
                Some(&format!("op-{i}")),
            ))
            .unwrap();
        vault.flush().unwrap();
    }
    std::fs::remove_dir_all(vault.root().join(".git/wi/ops")).unwrap();
    // Replays come from commits through the device index map.
    let replay = vault
        .write(request("n3.md", "x\n", None, Some("op-3")))
        .unwrap();
    assert!(replay.replayed && replay.path == "n3.md");
    // A new id is not found and writes.
    let fresh = vault
        .write(request("n9.md", "x\n", None, Some("op-9")))
        .unwrap();
    assert!(!fresh.replayed);
    vault.flush().unwrap();
    // Without the index, the history walk answers the same.
    vault.close_index();
    std::fs::remove_file(fx.cache.path().join("index.sqlite3")).unwrap();
    std::fs::create_dir(fx.cache.path().join("index.sqlite3")).unwrap();
    let vault = fx.open();
    std::fs::remove_dir_all(vault.root().join(".git/wi/ops")).unwrap();
    let replay = vault
        .write(request("n1.md", "x\n", None, Some("op-1")))
        .unwrap();
    assert!(replay.replayed && replay.path == "n1.md");
    let err = vault
        .write(request("n1.md", "y\n", None, Some("op-9")))
        .unwrap_err();
    assert!(matches!(err, StoreError::OperationMismatch { .. }));
}

/// Op-id lookup cost for a new id with many commits: history walk vs the
/// index map. `cargo test -p wi-store --release --test hardening -- --ignored --nocapture`
#[test]
#[ignore]
fn measure_op_id_lookup() {
    let (_fx, vault) = Fixture::new();
    let commits = 2000;
    for i in 0..commits {
        vault
            .write(request(
                &format!("n/{i}.md"),
                "x\n",
                None,
                Some(&format!("seed-{i}")),
            ))
            .unwrap();
        if i % 200 == 199 {
            vault.flush().unwrap();
        }
    }
    vault.flush().unwrap();
    let walk = std::time::Instant::now();
    for i in 0..20 {
        assert!(
            wi_history::History::find_operation(vault.history_vault(), &format!("absent-{i}"))
                .unwrap()
                .is_none()
        );
    }
    let walk = walk.elapsed() / 20;
    // Warm the map once (first scan of all commits), then time new-id writes.
    vault
        .write(request("warm.md", "x\n", None, Some("warm")))
        .unwrap();
    vault.flush().unwrap();
    let mut times = Vec::new();
    for i in 0..50 {
        let start = std::time::Instant::now();
        vault
            .write(request(
                &format!("m/{i}.md"),
                "x\n",
                None,
                Some(&format!("new-{i}")),
            ))
            .unwrap();
        times.push(start.elapsed());
        vault.flush().unwrap();
    }
    times.sort();
    println!(
        "commits={commits} walk_per_absent_id={walk:?} write_with_new_op_id p50={:?} p95={:?}",
        times[times.len() / 2],
        times[times.len() * 95 / 100]
    );
}

#[test]
fn terminal_claim_edits_refuse_and_file_ingress_never_revives() {
    let (_fixture, vault) = Fixture::new();
    write(
        &vault,
        "plain.md",
        "---\ntype: note\nstatus: rejected\nsummary: old\n---\n",
        Ingress::Engine,
    );
    vault
        .edit(edit(
            "plain.md",
            vec![("summary", json!("new"))],
            Ingress::Cli,
        ))
        .unwrap();
    for status in ["superseded", "rejected"] {
        let path = format!("{status}.md");
        write(
            &vault,
            &path,
            &format!(
                "---\ntype: memory\nstatus: {status}\nsummary: original\norigin: {{kind: agent}}\n---\nbody\n"
            ),
            Ingress::Engine,
        );
        vault.flush().unwrap();
        let before = vault.read(&path).unwrap();
        let generation = vault.generation().unwrap();
        assert!(matches!(
            vault.lifecycle_edit(edit(
                &path,
                vec![("summary", json!("changed"))],
                Ingress::Cli
            )),
            Err(StoreError::TerminalNote { .. })
        ));
        assert!(matches!(
            vault.edit(edit(&path, vec![("type", json!("note"))], Ingress::Cli)),
            Err(StoreError::TerminalNote { .. })
        ));
        assert_eq!(vault.read(&path).unwrap().bytes, before.bytes);
        assert_eq!(vault.generation().unwrap(), generation);
        let original = String::from_utf8(before.bytes).unwrap();
        std::fs::write(
            vault.root().join(&path),
            original.replace("summary: original", "summary: external"),
        )
        .unwrap();
        let error = vault.ingest_path(&path).unwrap_err();
        assert_eq!(error.code(), "terminal-note");
        assert!(error.to_string().contains("restores"));
        let bytes = vault.read(&path).unwrap().bytes;
        assert!(
            String::from_utf8(bytes)
                .unwrap()
                .contains(&format!("status: {status}"))
        );
        vault.reconcile().unwrap();
        assert!(text(&vault, &path).contains(&format!("status: {status}")));
    }
}
