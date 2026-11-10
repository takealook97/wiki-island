//! A rebuild run in slices ends with the same index as a one-shot rebuild,
//! never claims a complete walk before its last slice and resumes after the
//! process stops between slices.

use std::{fs, path::Path};
use tempfile::TempDir;
use wi_index::{Index, Query};

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn seed(root: &Path) {
    for i in 0..23 {
        let next = (i + 1) % 23;
        write(
            root,
            &format!("notes/n{i:02}.md"),
            &format!(
                "---\ntitle: Note {i}\n---\nalpha body {i} links to [[Note {next}]].\n- [fact] item {i} #tag{i}\n"
            ),
        );
    }
    write(root, "plain.md", "a plain beta note without frontmatter\n");
}

fn open(root: &Path, cache: &TempDir) -> Index {
    Index::open_at(root, "test", cache.path().join("index.sqlite3")).unwrap()
}

/// Table contents without row ids or stat stamps.
fn dump(cache: &TempDir) -> Vec<String> {
    let db = rusqlite::Connection::open(cache.path().join("index.sqlite3")).unwrap();
    let mut out = Vec::new();
    for sql in [
        "SELECT path||'|'||content_hash FROM files ORDER BY path",
        "SELECT path||'|'||permalink||'|'||title||'|'||note_type||'|'||content||'|'||metadata FROM notes ORDER BY path",
        "SELECT path||'|'||ifnull(category,'')||'|'||content||'|'||tags||'|'||line_start FROM observations ORDER BY path,line_start",
        "SELECT from_path||'|'||to_name||'|'||ifnull(to_path,'')||'|'||relation_type FROM relations ORDER BY from_path,to_name",
        "SELECT title||'|'||permalink FROM search_fts ORDER BY permalink",
        "SELECT path||'|'||content_hash FROM lifecycle_snapshots ORDER BY path",
    ] {
        let mut statement = db.prepare(sql).unwrap();
        let rows = statement.query_map([], |r| r.get::<_, String>(0)).unwrap();
        out.extend(rows.map(Result::unwrap));
    }
    out
}

fn hits(index: &Index, text: &str) -> (bool, Vec<String>) {
    let result = index.search(&Query::text(text)).unwrap();
    let mut paths: Vec<String> = result
        .hits
        .iter()
        .map(|h| h.rel_path.to_string_lossy().into_owned())
        .collect();
    paths.sort();
    (result.fallback, paths)
}

/// Slices of `chunk` files until done; returns how many slices ran.
fn run_slices(index: &mut Index, chunk: usize) -> usize {
    for n in 1..100 {
        if index.rebuild_slice(chunk, &|| true).unwrap().is_some() {
            return n;
        }
        assert_eq!(index.index_as_of().unwrap(), None, "no complete walk yet");
        assert!(index.rebuild_progress().unwrap().is_some());
    }
    panic!("rebuild never finished")
}

#[test]
fn sliced_rebuild_matches_a_one_shot_rebuild() {
    let vault = TempDir::new().unwrap();
    seed(vault.path());
    let (whole_cache, sliced_cache) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let mut whole = open(vault.path(), &whole_cache);
    whole.rebuild().unwrap();
    let mut sliced = open(vault.path(), &sliced_cache);
    let slices = run_slices(&mut sliced, 4);
    assert!(slices >= 6, "24 files in chunks of 4 take several slices");
    assert!(sliced.index_as_of().unwrap().is_some());
    assert_eq!(sliced.rebuild_progress().unwrap(), None);
    assert_eq!(dump(&sliced_cache), dump(&whole_cache));
    for text in ["alpha", "beta", "item", "Note 7"] {
        let (mode, paths) = hits(&sliced, text);
        assert_eq!((mode, paths), hits(&whole, text), "{text}");
        assert!(!mode, "indexed");
    }
}

#[test]
fn a_partial_index_answers_from_the_literal_fallback() {
    let vault = TempDir::new().unwrap();
    seed(vault.path());
    let cache = TempDir::new().unwrap();
    let mut index = open(vault.path(), &cache);
    index.sync(vault.path()).unwrap();
    assert!(index.index_as_of().unwrap().is_some());
    index.begin_rebuild().unwrap();
    assert_eq!(index.index_as_of().unwrap(), None);
    assert_eq!(index.rebuild_slice(4, &|| true).unwrap().map(|_| ()), None);
    let (mode, paths) = hits(&index, "alpha");
    assert!(mode, "literal fallback");
    let literal = wi_index::literal_search(vault.path(), &Query::text("alpha")).unwrap();
    assert_eq!(
        paths.len(),
        literal.hits.len(),
        "the same answer as a file scan"
    );
    assert!(!paths.is_empty());
}

#[test]
fn changes_between_slices_and_a_restart_end_like_a_fresh_rebuild() {
    let vault = TempDir::new().unwrap();
    seed(vault.path());
    let cache = TempDir::new().unwrap();
    let mut index = open(vault.path(), &cache);
    for _ in 0..3 {
        assert!(index.rebuild_slice(4, &|| true).unwrap().is_none());
    }
    // The process stops between slices; the vault changes meanwhile, also
    // in paths the earlier slices already rebuilt.
    drop(index);
    write(
        vault.path(),
        "notes/n00.md",
        "---\ntitle: Note 0\n---\nchanged gamma words\n",
    );
    fs::remove_file(vault.path().join("notes/n01.md")).unwrap();
    write(vault.path(), "notes/a-new.md", "a new delta note\n");
    write(vault.path(), "zz-last.md", "the last epsilon note\n");
    let mut index = open(vault.path(), &cache);
    assert!(
        index.rebuild_progress().unwrap().is_some(),
        "progress survives"
    );
    let report = loop {
        if let Some(report) = index.rebuild_slice(4, &|| true).unwrap() {
            break report;
        }
    };
    assert_eq!(report.removed, 1, "{report:?}");
    let fresh_cache = TempDir::new().unwrap();
    open(vault.path(), &fresh_cache).rebuild().unwrap();
    assert_eq!(dump(&cache), dump(&fresh_cache));
    assert_eq!(hits(&index, "gamma").1, ["notes/n00.md"]);
    assert_eq!(hits(&index, "delta").1, ["notes/a-new.md"]);
    assert!(
        hits(&index, "body 1 links")
            .1
            .iter()
            .all(|p| p != "notes/n01.md")
    );
}

#[test]
fn a_full_sync_ends_an_unfinished_rebuild() {
    let vault = TempDir::new().unwrap();
    seed(vault.path());
    let cache = TempDir::new().unwrap();
    let mut index = open(vault.path(), &cache);
    assert!(index.rebuild_slice(4, &|| true).unwrap().is_none());
    index.sync(vault.path()).unwrap();
    assert!(index.index_as_of().unwrap().is_some());
    assert_eq!(index.rebuild_progress().unwrap(), None);
    // The next slice starts a new rebuild rather than resuming the old one.
    assert!(index.rebuild_slice(100, &|| false).unwrap().is_some());
}

/// Two notes linking to each other: `a.md` → B and `b.md` → A.
fn seed_pair(root: &Path) {
    write(root, "a.md", "---\ntitle: A\n---\n[[B]]\n");
    write(root, "b.md", "---\ntitle: B\n---\n[[A]]\n");
}

/// Relations whose target is not resolved.
fn unresolved(cache: &TempDir) -> i64 {
    rusqlite::Connection::open(cache.path().join("index.sqlite3"))
        .unwrap()
        .query_row(
            "SELECT count(*) FROM relations WHERE to_path IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn a_sync_that_ends_an_unfinished_rebuild_resolves_its_links() {
    let vault = TempDir::new().unwrap();
    seed_pair(vault.path());
    let cache = TempDir::new().unwrap();
    let mut index = open(vault.path(), &cache);
    // Every file is rebuilt, but the slice stopped before the last step.
    assert!(index.rebuild_slice(128, &|| true).unwrap().is_none());
    assert_eq!(unresolved(&cache), 2, "chunks defer link resolution");
    let report = index.sync(vault.path()).unwrap();
    assert_eq!((report.parsed, report.removed), (0, 0));
    assert!(index.index_as_of().unwrap().is_some());
    assert_eq!(index.rebuild_progress().unwrap(), None);
    assert_eq!(
        unresolved(&cache),
        0,
        "the sync did the deferred resolution"
    );
    assert_eq!(index.context("a", 1).unwrap().hits.len(), 2);
}

#[test]
fn a_chunk_stops_when_another_walk_completed_the_index() {
    let vault = TempDir::new().unwrap();
    seed_pair(vault.path());
    let cache = TempDir::new().unwrap();
    let mut slicer = open(vault.path(), &cache);
    let other = std::cell::RefCell::new(open(vault.path(), &cache));
    let calls = std::cell::Cell::new(0);
    // After the first one-file chunk, another connection rebuilds the
    // whole index before the slicer's next chunk.
    let done = slicer
        .rebuild_slice(1, &|| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                other.borrow_mut().rebuild().unwrap();
                false
            } else {
                true
            }
        })
        .unwrap();
    assert!(done.is_none());
    assert!(slicer.index_as_of().unwrap().is_some());
    assert_eq!(
        slicer.rebuild_progress().unwrap(),
        None,
        "no cursor came back"
    );
    assert_eq!(unresolved(&cache), 0, "the completed index is intact");
    assert_eq!(slicer.context("a", 1).unwrap().hits.len(), 2);
}

#[test]
fn a_chunk_stops_when_another_slicer_restarted_the_rebuild() {
    let vault = TempDir::new().unwrap();
    seed_pair(vault.path());
    write(vault.path(), "c.md", "C");
    let cache = TempDir::new().unwrap();
    let mut first = open(vault.path(), &cache);
    let second = std::cell::RefCell::new(open(vault.path(), &cache));
    let calls = std::cell::Cell::new(0);
    let done = first
        .rebuild_slice(1, &|| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                second.borrow_mut().begin_rebuild().unwrap();
            }
            false
        })
        .unwrap();
    assert!(done.is_none(), "the old rebuild stopped");
    assert_eq!(calls.get(), 1, "without writing another chunk");
    assert_eq!(first.rebuild_progress().unwrap().as_deref(), Some(""));
    assert!(
        second
            .borrow_mut()
            .rebuild_slice(128, &|| false)
            .unwrap()
            .is_some()
    );
    assert_eq!(unresolved(&cache), 0);
}

#[test]
fn the_last_step_reparses_new_files_within_the_budget() {
    let vault = TempDir::new().unwrap();
    write(vault.path(), "z.md", "last note");
    let cache = TempDir::new().unwrap();
    let mut index = open(vault.path(), &cache);
    assert!(index.rebuild_slice(128, &|| true).unwrap().is_none());
    // Files added before the cursor are left to the last step.
    for i in 0..400 {
        write(vault.path(), &format!("a{i:04}.md"), "new note");
    }
    let calls = std::cell::Cell::new(0);
    let over = || {
        calls.set(calls.get() + 1);
        true
    };
    assert!(index.rebuild_slice(128, &over).unwrap().is_none());
    assert_eq!(calls.get(), 1, "one chunk, then the budget check");
    let mut slices = 1;
    while index.rebuild_slice(128, &over).unwrap().is_none() {
        slices += 1;
        assert_eq!(index.index_as_of().unwrap(), None);
        assert!(slices < 10, "the last step never finished");
    }
    assert!(slices >= 4, "400 files in chunks of 128: {slices}");
    let fresh = TempDir::new().unwrap();
    open(vault.path(), &fresh).rebuild().unwrap();
    assert_eq!(dump(&cache), dump(&fresh));
}

#[test]
fn a_failed_chunk_rolls_back_its_rows_and_progress() {
    let vault = TempDir::new().unwrap();
    seed_pair(vault.path());
    let cache = TempDir::new().unwrap();
    let mut index = open(vault.path(), &cache);
    index.begin_rebuild().unwrap();
    let db = rusqlite::Connection::open(cache.path().join("index.sqlite3")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_b BEFORE INSERT ON files WHEN NEW.path='b.md' \
         BEGIN SELECT RAISE(ABORT,'injected interruption'); END;",
    )
    .unwrap();
    assert!(index.rebuild_slice(128, &|| true).is_err());
    assert_eq!(index.rebuild_progress().unwrap().as_deref(), Some(""));
    assert_eq!(index.index_as_of().unwrap(), None);
    let rows: i64 = db
        .query_row("SELECT count(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0);
    db.execute_batch("DROP TRIGGER fail_b").unwrap();
    drop(index);
    let mut index = open(vault.path(), &cache);
    assert!(index.rebuild_slice(128, &|| false).unwrap().is_some());
    assert_eq!(unresolved(&cache), 0);
}

#[test]
fn a_schema_change_between_slices_rebuilds_everything() {
    let vault = TempDir::new().unwrap();
    seed_pair(vault.path());
    let cache = TempDir::new().unwrap();
    let mut index = open(vault.path(), &cache);
    assert!(index.rebuild_slice(1, &|| true).unwrap().is_none());
    drop(index);
    let db = rusqlite::Connection::open(cache.path().join("index.sqlite3")).unwrap();
    db.pragma_update(None, "user_version", 0).unwrap();
    drop(db);
    let mut index = open(vault.path(), &cache);
    assert_eq!(index.rebuild_progress().unwrap(), None);
    assert_eq!(index.index_as_of().unwrap(), None);
    assert!(index.rebuild_slice(128, &|| false).unwrap().is_some());
    assert_eq!(unresolved(&cache), 0);
    assert_eq!(index.search(&Query::text("B")).unwrap().hits.len(), 2);
}

#[test]
fn watcher_changes_on_both_sides_of_the_cursor_survive() {
    let vault = TempDir::new().unwrap();
    seed_pair(vault.path());
    write(vault.path(), "c.md", "C");
    write(vault.path(), "d.md", "D");
    let cache = TempDir::new().unwrap();
    let mut index = open(vault.path(), &cache);
    assert!(index.rebuild_slice(2, &|| true).unwrap().is_none());
    let mut watcher = open(vault.path(), &cache);
    let root = vault.path();
    fs::rename(root.join("a.md"), root.join("aa.md")).unwrap();
    fs::remove_file(root.join("b.md")).unwrap();
    fs::rename(root.join("c.md"), root.join("cc.md")).unwrap();
    fs::remove_file(root.join("d.md")).unwrap();
    watcher
        .update_paths(&["a.md", "aa.md", "b.md", "c.md", "cc.md", "d.md"])
        .unwrap();
    assert!(index.rebuild_slice(128, &|| false).unwrap().is_some());
    let db = rusqlite::Connection::open(cache.path().join("index.sqlite3")).unwrap();
    let paths: Vec<String> = db
        .prepare("SELECT path FROM files ORDER BY path")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(paths, ["aa.md", "cc.md"]);
}

#[test]
fn edits_behind_the_last_step_cursor_are_read_again() {
    let vault = TempDir::new().unwrap();
    write(vault.path(), "z.md", "last note");
    let cache = TempDir::new().unwrap();
    let mut index = open(vault.path(), &cache);
    assert!(index.rebuild_slice(1, &|| true).unwrap().is_none());
    for name in ["a0.md", "a1.md", "a2.md"] {
        write(vault.path(), name, "new note");
    }
    // The last step reparses a0.md, then the file changes again.
    assert!(index.rebuild_slice(1, &|| true).unwrap().is_none());
    write(vault.path(), "a0.md", "edited after the last step read it");
    while index.rebuild_slice(1, &|| true).unwrap().is_none() {}
    let fresh = TempDir::new().unwrap();
    open(vault.path(), &fresh).rebuild().unwrap();
    assert_eq!(dump(&cache), dump(&fresh));
}

#[test]
fn a_cursor_left_next_to_a_recorded_walk_restarts_the_rebuild() {
    let vault = TempDir::new().unwrap();
    seed_pair(vault.path());
    let cache = TempDir::new().unwrap();
    let mut index = open(vault.path(), &cache);
    index.sync(vault.path()).unwrap();
    let db = rusqlite::Connection::open(cache.path().join("index.sqlite3")).unwrap();
    db.execute(
        "INSERT INTO index_meta(key,value) VALUES('rebuild_after','a.md')",
        [],
    )
    .unwrap();
    assert_eq!(index.rebuild_progress().unwrap().as_deref(), Some("a.md"));
    assert!(index.rebuild_slice(128, &|| false).unwrap().is_some());
    assert_eq!(index.rebuild_progress().unwrap(), None);
    assert_eq!(unresolved(&cache), 0);
}
