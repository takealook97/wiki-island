//! Files the engine writes under `.wiki-island/` are never delivered as
//! notes, capture notes aside; only its input folders are indexed at all.

use std::fs;
use tempfile::TempDir;
use wi_index::{Index, Query, literal_search};

fn write(root: &std::path::Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn vault() -> TempDir {
    let root = TempDir::new().unwrap();
    write(root.path(), "note.md", "# Owner\n\nshared word\n");
    write(
        root.path(),
        ".wiki-island/exports/AGENTS.md",
        "# Rules\n\nshared word\n",
    );
    write(
        root.path(),
        ".wiki-island/imports/Notion/export.md",
        "---\ntitle: Notion import\n---\nshared word\n",
    );
    write(
        root.path(),
        ".wiki-island/captures/standup.md",
        "---\ntype: capture\n---\nshared word\n",
    );
    write(
        root.path(),
        ".wiki-island/work/item.md",
        "---\ntype: work\n---\nshared word\n",
    );
    root
}

fn indexed(cache: &TempDir) -> Vec<String> {
    let db = rusqlite::Connection::open(cache.path().join("index.db")).unwrap();
    db.prepare("SELECT path FROM files ORDER BY path")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn only_engine_input_is_indexed_and_nothing_engine_owned_is_delivered() {
    let root = vault();
    let cache = TempDir::new().unwrap();
    let mut index = Index::open_at(root.path(), "engine", cache.path().join("index.db")).unwrap();
    index.sync(root.path()).unwrap();
    assert_eq!(
        indexed(&cache),
        [
            ".wiki-island/captures/standup.md",
            ".wiki-island/work/item.md",
            "note.md"
        ]
    );

    // Capture notes are saved to be found again; other engine files never are.
    let delivered = |hits: Vec<wi_index::SearchHit>| -> Vec<String> {
        let mut paths: Vec<String> = hits
            .iter()
            .map(|h| h.rel_path.to_string_lossy().into_owned())
            .collect();
        paths.sort();
        paths
    };
    assert_eq!(
        delivered(index.search(&Query::text("shared")).unwrap().hits),
        [".wiki-island/captures/standup.md", "note.md"]
    );
    assert_eq!(
        delivered(
            literal_search(root.path(), &Query::text("shared"))
                .unwrap()
                .hits
        ),
        [".wiki-island/captures/standup.md", "note.md"]
    );
    let work = index.context(".wiki-island/work/item", 1).unwrap();
    assert!(work.hits.is_empty(), "{:?}", work.hits);
}

#[test]
fn an_engine_file_indexed_by_an_older_version_leaves_on_the_next_sync() {
    let root = vault();
    let cache = TempDir::new().unwrap();
    let mut index = Index::open_at(root.path(), "engine", cache.path().join("index.db")).unwrap();
    index.sync(root.path()).unwrap();
    // An older engine indexed its import record like a note.
    let db = rusqlite::Connection::open(cache.path().join("index.db")).unwrap();
    let rel = ".wiki-island/imports/Notion/export.md";
    db.execute(
        "INSERT INTO files(path,mtime_ns,size,content_hash) VALUES(?1,'0',1,'old')",
        [rel],
    )
    .unwrap();
    db.execute(
        "INSERT INTO notes(path,permalink,title,note_type,content,metadata) \
         VALUES(?1,'notion-import','Notion import','note','shared word','{}')",
        [rel],
    )
    .unwrap();
    db.execute(
        "INSERT INTO search_fts(rowid,title,content,script_ngrams,permalink) \
         VALUES(last_insert_rowid(),'Notion import','shared word','','notion-import')",
        [],
    )
    .unwrap();
    drop(db);
    let hits = index.search(&Query::text("Notion import")).unwrap().hits;
    assert!(hits.is_empty(), "{hits:?}");
    index.sync(root.path()).unwrap();
    assert!(!indexed(&cache).iter().any(|p| p == rel));
}
