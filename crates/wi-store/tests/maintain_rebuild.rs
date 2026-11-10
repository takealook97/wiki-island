//! Budgeted maintenance rebuilds an unusable index in slices across passes
//! and process restarts, never serving the partial index as complete.

mod common;

use std::time::Duration;

use common::Fixture;
use wi_store::{MaintainOptions, Query, Vault};

/// More notes than one rebuild chunk holds, so a tiny budget needs
/// several passes.
const NOTES: usize = 300;

fn seed(vault: &Vault) {
    for i in 0..NOTES {
        let path = vault.root().join(format!("notes/n{i:04}.md"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let next = (i + 1) % NOTES;
        std::fs::write(
            path,
            format!("---\ntitle: Note {i}\n---\nalpha body {i} links to [[Note {next}]].\n"),
        )
        .unwrap();
    }
    vault.auto_ingest().unwrap();
    vault.flush().unwrap();
}

fn tiny() -> MaintainOptions {
    MaintainOptions {
        full: false,
        time_budget: Some(Duration::from_nanos(1)),
    }
}

fn answer(vault: &Vault, text: &str) -> (bool, Vec<String>) {
    let query = Query {
        text: text.into(),
        limit: 1000,
        ..Default::default()
    };
    let response = vault.search(&query, Default::default()).unwrap();
    let mut paths: Vec<String> = response
        .hits
        .iter()
        .map(|h| h.rel_path.to_string_lossy().replace('\\', "/"))
        .collect();
    paths.sort();
    (response.fallback, paths)
}

fn index_check(vault: &Vault) -> String {
    let report = vault.doctor().unwrap();
    let check = report.checks.iter().find(|c| c.name == "index").unwrap();
    check.detail.clone()
}

#[test]
fn a_lost_index_is_rebuilt_in_slices_that_resume_after_a_restart() {
    let (f, vault) = Fixture::new();
    seed(&vault);
    vault.reindex().unwrap();
    let queries = ["alpha", "body 17", "Note 250"];
    let expected: Vec<_> = queries.iter().map(|q| answer(&vault, q)).collect();
    assert!(
        expected
            .iter()
            .all(|(fallback, hits)| !fallback && !hits.is_empty())
    );
    drop(vault);
    std::fs::remove_file(f.cache.path().join("index.sqlite3")).unwrap();

    let mut vault = f.open();
    let mut passes = 0;
    let mut rebuilt = false;
    while !rebuilt {
        passes += 1;
        assert!(passes < 50, "the rebuild never finished");
        let report = vault.maintain(&tiny()).unwrap();
        rebuilt = report.index_rebuilt;
        if rebuilt {
            break;
        }
        assert!(!report.complete, "{report:?}");
        assert_eq!(
            vault.index_as_of().unwrap(),
            None,
            "a partial index is never complete"
        );
        // The first pass may stop before the index stage.
        if !vault.index_rebuilding() {
            assert_eq!(passes, 1);
            continue;
        }
        assert!(index_check(&vault).contains("rebuild in progress"));
        let (fallback, hits) = answer(&vault, "alpha");
        assert!(fallback, "search answers from the files meanwhile");
        assert_eq!(hits, expected[0].1);
        if passes == 2 {
            // The process stops between passes and starts again.
            drop(vault);
            vault = f.open();
            assert!(vault.index_rebuilding(), "progress survives a restart");
        }
    }
    assert!(
        passes >= 3,
        "{NOTES} notes take several slices, took {passes}"
    );
    assert!(vault.index_as_of().unwrap().is_some());
    assert!(!vault.index_rebuilding());
    assert!(index_check(&vault).contains("complete walk"));
    for (query, want) in queries.iter().zip(&expected) {
        assert_eq!(&answer(&vault, query), want, "{query}");
    }
    for _ in 0..50 {
        if vault.maintain(&tiny()).unwrap().complete {
            return;
        }
    }
    panic!("the pass after the rebuild never completed");
}

#[test]
fn an_unbudgeted_pass_still_rebuilds_in_one_step() {
    let (f, vault) = Fixture::new();
    seed(&vault);
    drop(vault);
    std::fs::remove_file(f.cache.path().join("index.sqlite3")).unwrap();
    let vault = f.open();
    let report = vault.maintain(&MaintainOptions::default()).unwrap();
    assert!(report.index_rebuilt && report.complete, "{report:?}");
    assert!(vault.index_as_of().unwrap().is_some());
}

#[test]
fn an_unbudgeted_pass_finishes_an_unfinished_rebuild() {
    let (f, vault) = Fixture::new();
    seed(&vault);
    drop(vault);
    std::fs::remove_file(f.cache.path().join("index.sqlite3")).unwrap();
    let vault = f.open();
    for _ in 0..3 {
        assert!(!vault.maintain(&tiny()).unwrap().complete);
        if vault.index_rebuilding() {
            break;
        }
    }
    assert!(vault.index_rebuilding());
    let report = vault.maintain(&MaintainOptions::default()).unwrap();
    assert!(report.index_rebuilt && report.complete, "{report:?}");
    assert!(!vault.index_rebuilding());
    assert!(!answer(&vault, "alpha").0);
}

#[test]
fn a_database_deleted_mid_rebuild_restarts_on_the_same_handle() {
    let (f, vault) = Fixture::new();
    seed(&vault);
    drop(vault);
    std::fs::remove_file(f.cache.path().join("index.sqlite3")).unwrap();
    let vault = f.open();
    for _ in 0..5 {
        vault.maintain(&tiny()).unwrap();
        if vault.index_rebuilding() {
            break;
        }
    }
    assert!(vault.index_rebuilding());
    // Windows cannot unlink an open SQLite database: close it first.
    vault.close_index();
    std::fs::remove_file(f.cache.path().join("index.sqlite3")).unwrap();
    assert!(vault.index_as_of().unwrap().is_none());
    let mut rebuilt = false;
    for _ in 0..30 {
        let report = vault.maintain(&tiny()).unwrap();
        rebuilt |= report.index_rebuilt;
        if report.complete {
            break;
        }
    }
    assert!(rebuilt);
    let (fallback, paths) = answer(&vault, "alpha");
    assert!(!fallback);
    assert_eq!(paths.len(), NOTES);
}

#[test]
fn steady_edits_to_one_file_do_not_starve_budgeted_maintenance() {
    let (f, vault) = Fixture::new();
    std::fs::write(vault.root().join("z.md"), "last").unwrap();
    vault.reindex().unwrap();
    drop(vault);
    std::fs::remove_file(f.cache.path().join("index.sqlite3")).unwrap();
    let vault = f.open();
    let db = rusqlite::Connection::open(f.cache.path().join("index.sqlite3")).unwrap();
    let cursor = || -> Option<String> {
        db.query_row(
            "SELECT value FROM index_meta WHERE key='rebuild_after'",
            [],
            |r| r.get(0),
        )
        .ok()
    };
    for _ in 0..10 {
        assert!(!vault.maintain(&tiny()).unwrap().index_rebuilt);
        if cursor().as_deref() == Some("z.md") {
            break;
        }
    }
    assert_eq!(cursor().as_deref(), Some("z.md"));
    std::fs::write(vault.root().join("a.md"), "new").unwrap();
    assert!(!vault.maintain(&tiny()).unwrap().index_rebuilt);
    // a.md changes before every pass and never stops changing.
    let mut rebuilt = false;
    for n in 0..30 {
        std::fs::write(vault.root().join("a.md"), "x".repeat(10 + n)).unwrap();
        let report = vault.maintain(&tiny()).unwrap();
        rebuilt |= report.index_rebuilt;
        if rebuilt {
            assert!(vault.index_as_of().unwrap().is_some());
        }
        if report.complete {
            assert!(rebuilt && n < 30);
            assert!(!answer(&vault, "last").0, "search leaves the fallback");
            return;
        }
    }
    panic!("budgeted maintenance never completed under steady edits (rebuilt: {rebuilt})");
}
