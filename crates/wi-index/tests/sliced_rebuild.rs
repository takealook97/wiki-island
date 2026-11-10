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
