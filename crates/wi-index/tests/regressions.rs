use std::{collections::BTreeSet, fs, path::Path, time::Duration};
use tempfile::TempDir;
use wi_index::{Error, Index, Query, SearchMode, literal_search};

fn setup() -> (TempDir, TempDir, Index) {
    let root = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let index = Index::open_at(root.path(), "regressions", cache.path().join("index.db")).unwrap();
    (root, cache, index)
}

#[test]
fn deletes_use_child_indexes_and_rebuild_preserves_results() {
    let (root, cache, mut index) = setup();
    for i in 0..50 {
        let obs: String = (0..8)
            .map(|j| format!("- [fact] observation {j}\n"))
            .collect();
        fs::write(root.path().join(format!("{i}.md")), obs).unwrap();
    }
    index.sync(root.path()).unwrap();
    let before = index.search(&Query::text("observation")).unwrap().hits;
    index.rebuild().unwrap();
    let after = index.search(&Query::text("observation")).unwrap().hits;
    assert_eq!(
        before.iter().map(|h| &h.rel_path).collect::<Vec<_>>(),
        after.iter().map(|h| &h.rel_path).collect::<Vec<_>>()
    );
    let db = rusqlite::Connection::open(cache.path().join("index.db")).unwrap();
    let plans: Vec<String> = db
        .prepare("EXPLAIN QUERY PLAN DELETE FROM files WHERE path='0.md'")
        .unwrap()
        .query_map([], |r| r.get(3))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(
        plans.iter().any(|p| p.contains("observations_path")),
        "{plans:?}"
    );
    assert!(!plans.iter().any(|p| p.contains("SCAN observations")));
}

#[test]
fn bad_files_are_skipped_without_losing_previous_rows_or_valid_hits() {
    let (root, _cache, mut index) = setup();
    fs::write(root.path().join("ok.md"), "hello original").unwrap();
    fs::write(root.path().join("bad.md"), "previous hello").unwrap();
    index.sync(root.path()).unwrap();
    fs::write(root.path().join("bad.md"), [255, 254]).unwrap();
    fs::write(root.path().join("back\\slash.md"), "invalid path").unwrap();
    fs::write(
        root.path().join("malformed.md"),
        "---\na: [\n---\nhello plain text",
    )
    .unwrap();
    for report in [
        index.sync(root.path()).unwrap(),
        index.rebuild().unwrap(),
        index.update_paths(&["bad.md"]).unwrap(),
    ] {
        assert!(report.skipped.contains(&"bad.md".into()));
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.rel_path == Path::new("bad.md"))
        );
    }
    let indexed = index.search(&Query::text("hello")).unwrap();
    assert!(
        indexed
            .hits
            .iter()
            .any(|h| h.rel_path == Path::new("ok.md"))
    );
    assert!(
        indexed
            .hits
            .iter()
            .any(|h| h.rel_path == Path::new("bad.md"))
    );
    assert!(indexed.skipped.contains(&"bad.md".into()));
    assert_eq!(
        index.search(&Query::text("previous")).unwrap().hits.len(),
        1
    );
    let literal = literal_search(root.path(), &Query::text("hello")).unwrap();
    assert_eq!(literal.hits.len(), 2);
    assert!(literal.skipped.contains(&"bad.md".into()));
    assert!(literal.skipped.contains(&"back\\slash.md".into()));
    assert!(
        literal
            .diagnostics
            .iter()
            .any(|d| d.rel_path == Path::new("malformed.md"))
    );
}

#[test]
fn bad_config_is_typed_but_literal_fallback_keeps_working() {
    let (root, cache, mut index) = setup();
    fs::write(root.path().join("ok.md"), "hello").unwrap();
    fs::create_dir(root.path().join(".wiki-island")).unwrap();
    for config in [
        "<<<<<<< conflict",
        "ignore_dirs = 'bad'",
        "ignore_dirs = ['../x']",
        "max_note_bytes = -1",
    ] {
        fs::write(root.path().join(".wiki-island/config.toml"), config).unwrap();
        assert!(matches!(index.sync(root.path()), Err(Error::Config(_))));
        let result = wi_index::search_at_with_fallback(
            root.path(),
            "regressions",
            cache.path().join("missing.db"),
            &Query::text("hello"),
        )
        .unwrap();
        assert!(result.fallback);
        assert_eq!(result.hits.len(), 1);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.starts_with("config:"))
        );
    }
}

#[cfg(unix)]
#[test]
fn unreadable_note_does_not_abort_search_or_rebuild() {
    use std::os::unix::fs::PermissionsExt;
    let (root, _cache, mut index) = setup();
    fs::write(root.path().join("ok.md"), "hello").unwrap();
    let bad = root.path().join("bad.md");
    fs::write(&bad, "private").unwrap();
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o0)).unwrap();
    let report = index.rebuild().unwrap();
    if fs::read(&bad).is_err() {
        assert_eq!(report.skipped, [Path::new("bad.md")]);
    }
    assert_eq!(
        literal_search(root.path(), &Query::text("hello"))
            .unwrap()
            .hits
            .len(),
        1
    );
    fs::set_permissions(bad, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn live_writer_never_aborts_completed_sync_work() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let (root, _cache, mut index) = setup();
    fs::write(root.path().join("ok.md"), "hello").unwrap();
    let path = root.path().join("changing.md");
    fs::write(&path, "initial").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let writer = std::thread::spawn(move || {
        while !flag.load(Ordering::Relaxed) {
            fs::write(&path, "changing text ".repeat(4_000)).unwrap();
            std::thread::sleep(Duration::from_micros(50));
        }
    });
    for _ in 0..12 {
        let report = index.sync(root.path()).unwrap();
        assert!(report.index_as_of.is_some());
        assert_eq!(index.search(&Query::text("hello")).unwrap().hits.len(), 1);
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    index.sync(root.path()).unwrap();
}

#[test]
fn slash_words_and_foldered_permalink_names_are_searchable() {
    let (root, _cache, mut index) = setup();
    fs::write(
        root.path().join("a.md"),
        "---\npermalink: custom/deep-note\n---\ninput/output TCP/IP client/server 읽기/쓰기",
    )
    .unwrap();
    index.sync(root.path()).unwrap();
    for text in ["input", "output", "IP", "server", "쓰기", "input/output"] {
        assert_eq!(
            index.search(&Query::text(text)).unwrap().hits.len(),
            1,
            "{text}"
        );
    }
    for text in ["deep", "deep-note", "custom/deep-note", "custom/deep*"] {
        let q = Query {
            mode: SearchMode::Permalink,
            ..Query::text(text)
        };
        assert_eq!(index.search(&q).unwrap().hits.len(), 1, "{text}");
    }
}

#[test]
fn incremental_resolution_matches_rebuild_for_old_new_ambiguous_and_relative_names() {
    let (root, _cache, mut index) = setup();
    fs::create_dir(root.path().join("folder")).unwrap();
    fs::write(
        root.path().join("folder/a.md"),
        "[[Old]] [[New]] [[shared]] [[new-id]] [relative](../target.md)",
    )
    .unwrap();
    fs::write(
        root.path().join("target.md"),
        "---\ntitle: Old\npermalink: shared\n---\nbody",
    )
    .unwrap();
    index.sync(root.path()).unwrap();
    let edges = |index: &Index| {
        index
            .context("folder/a", 1)
            .unwrap()
            .edges
            .into_iter()
            .map(|e| (e.to_name, e.to_path))
            .collect::<BTreeSet<_>>()
    };
    for text in [
        Some("---\ntitle: New\npermalink: new-id\n---\nbody"),
        None,
        Some("---\ntitle: Old\npermalink: shared\n---\nrestored"),
    ] {
        if let Some(text) = text {
            fs::write(root.path().join("target.md"), text).unwrap();
            index.update_paths(&["target.md"]).unwrap();
        } else {
            fs::remove_file(root.path().join("target.md")).unwrap();
            index.remove_paths(&["target.md"]).unwrap();
        }
        let before = edges(&index);
        index.rebuild().unwrap();
        assert_eq!(before, edges(&index));
    }
    fs::write(
        root.path().join("duplicate.md"),
        "---\ntitle: Old\npermalink: shared\n---\nbody",
    )
    .unwrap();
    index.update_paths(&["duplicate.md"]).unwrap();
    let before = edges(&index);
    assert!(before.contains(&("Old".into(), None)));
    index.rebuild().unwrap();
    assert_eq!(before, edges(&index));
}

#[test]
fn user_syntax_never_causes_fts_parse_errors() {
    let (root, _cache, mut index) = setup();
    fs::write(
        root.path().join("a.md"),
        "literal AND OR NOT NEAR payment 5\" screen",
    )
    .unwrap();
    index.sync(root.path()).unwrap();
    for text in [
        "\"",
        "*",
        "-",
        ":",
        "NEAR",
        "(",
        ")",
        "payment AND",
        "AND",
        "NOT payment",
        "5\" screen",
        "a OR",
        "OR AND",
    ] {
        for mode in [SearchMode::Text, SearchMode::Title, SearchMode::Permalink] {
            index
                .search(&Query {
                    mode,
                    ..Query::text(text)
                })
                .unwrap();
        }
    }
}

#[test]
fn reverify_detects_same_stat_edits_and_reuses_unchanged_hashes() {
    let (root, _cache, mut index) = setup();
    let path = root.path().join("a.md");
    fs::write(&path, "aaaa bbbb").unwrap();
    index.sync(root.path()).unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    fs::write(&path, "cccc dddd").unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    assert_eq!(index.sync(root.path()).unwrap().hashed, 0);
    assert_eq!(index.reverify().unwrap().parsed, 1);
    assert_eq!(index.search(&Query::text("cccc")).unwrap().hits.len(), 1);
    let report = index.reverify().unwrap();
    assert_eq!(report.hashed, 1);
    assert_eq!(report.parsed, 0);
}

#[test]
fn context_caps_keep_bfs_nodes_monotonic_and_edges_induced_and_sorted() {
    let (root, _cache, mut index) = setup();
    for i in 0..300 {
        let links: String = (1..16)
            .map(|j| format!("[[n{}]] ", (i + j) % 300))
            .collect();
        fs::write(root.path().join(format!("n{i}.md")), links).unwrap();
    }
    index.sync(root.path()).unwrap();
    let mut previous = BTreeSet::new();
    let mut previous_edges = BTreeSet::new();
    for depth in 0..6 {
        let result = index.context("n0", depth).unwrap();
        let nodes: BTreeSet<_> = result.hits.iter().map(|h| h.rel_path.clone()).collect();
        assert!(previous.is_subset(&nodes));
        assert!(nodes.len() <= 256);
        assert!(result.edges.len() <= 2048);
        assert!(result.edges.iter().all(|e| nodes.contains(&e.from_path)
            && e.to_path.as_ref().is_none_or(|p| nodes.contains(p))));
        let keys: Vec<_> = result
            .edges
            .iter()
            .map(|e| (&e.from_path, &e.to_name, &e.to_path, &e.relation_type))
            .collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]));
        let edges: BTreeSet<_> = result
            .edges
            .into_iter()
            .map(|e| (e.from_path, e.to_name, e.to_path, e.relation_type))
            .collect();
        assert!(previous_edges.is_subset(&edges));
        previous_edges = edges;
        previous = nodes;
    }
}

#[test]
fn note_size_bound_is_configurable_and_skips_without_reading_whole_file() {
    let (root, _cache, mut index) = setup();
    fs::write(root.path().join("ok.md"), "hello").unwrap();
    let big = root.path().join("big.md");
    fs::File::create(&big)
        .unwrap()
        .set_len(100_000_000)
        .unwrap();
    let report = index.rebuild().unwrap();
    assert_eq!(report.skipped, [Path::new("big.md")]);
    assert!(report.diagnostics[0].message.contains("size bound"));
    assert_eq!(
        literal_search(root.path(), &Query::text("hello"))
            .unwrap()
            .hits
            .len(),
        1
    );
    fs::create_dir(root.path().join(".wiki-island")).unwrap();
    fs::write(
        root.path().join(".wiki-island/config.toml"),
        "max_note_bytes = 4",
    )
    .unwrap();
    assert!(index.rebuild().unwrap().skipped.contains(&"ok.md".into()));
}

#[cfg(target_os = "linux")]
#[test]
fn non_utf8_filename_has_serializable_skip_diagnostic() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let (root, _cache, mut index) = setup();
    fs::write(root.path().join("ok.md"), "hello").unwrap();
    let name = OsString::from_vec(b"bad\xff.md".to_vec());
    fs::write(root.path().join(name), "bad path").unwrap();
    let report = index.sync(root.path()).unwrap();
    assert_eq!(report.skipped.len(), 1);
    serde_json::to_string(&report).unwrap();
    assert_eq!(index.search(&Query::text("hello")).unwrap().hits.len(), 1);
    assert_eq!(
        literal_search(root.path(), &Query::text("hello"))
            .unwrap()
            .skipped
            .len(),
        1
    );
}

#[test]
fn missing_or_ambiguous_context_root_is_diagnosed() {
    let (root, _cache, mut index) = setup();
    assert_eq!(
        index.context("absent", 1).unwrap().diagnostics[0].message,
        "context-root-missing"
    );
    for path in ["a.md", "b.md"] {
        fs::write(root.path().join(path), "---\npermalink: same\n---\nbody").unwrap();
    }
    index.sync(root.path()).unwrap();
    let result = index.context("same", 1).unwrap();
    assert!(result.hits.is_empty());
    assert_eq!(result.diagnostics[0].message, "context-root-ambiguous");
}

/// NUL in query text selects labelled literal fallback in every
/// mode instead of a hard SQLite error.
#[test]
fn nul_in_query_text_falls_back_instead_of_failing() {
    let (root, _cache, mut index) = setup();
    fs::write(root.path().join("a.md"), "payment terms").unwrap();
    index.sync(root.path()).unwrap();
    for text in ["a\0b", "\0", "pay\0"] {
        for mode in [SearchMode::Text, SearchMode::Title, SearchMode::Permalink] {
            let query = Query {
                mode,
                ..Query::text(text)
            };
            let result = index.search(&query).unwrap();
            assert!(result.fallback, "{text:?} {mode:?}");
        }
    }
}

/// A targeted update replaces the stored skip state of the
/// paths it touched, and a result flags hits whose file is currently skipped.
#[test]
fn targeted_updates_refresh_skipped_paths_and_flag_skipped_hits() {
    let (root, _cache, mut index) = setup();
    fs::write(root.path().join("x.md"), "hello before").unwrap();
    fs::write(root.path().join("y.md"), "hello other").unwrap();
    index.sync(root.path()).unwrap();

    // Broken by a full walk: the stale row is returned but flagged.
    fs::write(root.path().join("x.md"), [255, 254]).unwrap();
    fs::write(root.path().join("y.md"), [255, 254]).unwrap();
    index.sync(root.path()).unwrap();
    let result = index.search(&Query::text("hello")).unwrap();
    assert_eq!(result.hits.len(), 2);
    assert!(result.hits.iter().all(|h| result.is_skipped(h)));

    // Repaired by a targeted update: no longer skipped; y.md still is.
    fs::write(root.path().join("x.md"), "hello after").unwrap();
    index.update_paths(&["x.md"]).unwrap();
    let result = index.search(&Query::text("hello")).unwrap();
    assert_eq!(result.skipped, vec![Path::new("y.md").to_path_buf()]);
    assert!(
        result
            .diagnostics
            .iter()
            .all(|d| d.rel_path != Path::new("x.md"))
    );
    let x = result
        .hits
        .iter()
        .find(|h| h.rel_path == Path::new("x.md"))
        .unwrap();
    assert!(!result.is_skipped(x));

    // Broken by a targeted update: skipped before the next full walk.
    fs::write(root.path().join("x.md"), [255, 254]).unwrap();
    index.update_paths(&["x.md"]).unwrap();
    let result = index.search(&Query::text("hello")).unwrap();
    assert!(result.skipped.contains(&"x.md".into()));

    // Removed: its skip entry goes with it.
    fs::remove_file(root.path().join("y.md")).unwrap();
    index.remove_paths(&["y.md"]).unwrap();
    let result = index.search(&Query::text("hello")).unwrap();
    assert_eq!(result.skipped, vec![Path::new("x.md").to_path_buf()]);
}
