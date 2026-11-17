//! The searchable title holds the delivered title and the link identity;
//! the exact ordering key folds case for every script; the literal fallback
//! matches the same titles as the complete index.

use std::{fs, path::Path};
use tempfile::TempDir;
use wi_index::{Index, Query, SearchMode};

const PADDING_NOTES: usize = 10;

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn open(root: &TempDir, cache: &TempDir) -> Index {
    Index::open_at(root.path(), "keys", cache.path().join("i.db")).unwrap()
}

fn query(text: &str, mode: SearchMode) -> Query {
    Query {
        mode,
        ..Query::text(text)
    }
}

fn paths(index: &Index, text: &str, mode: SearchMode) -> Vec<String> {
    index
        .search(&query(text, mode))
        .unwrap()
        .hits
        .into_iter()
        .map(|h| h.rel_path.to_string_lossy().into_owned())
        .collect()
}

fn first(index: &Index, text: &str, mode: SearchMode) -> String {
    paths(index, text, mode)
        .into_iter()
        .next()
        .unwrap_or_default()
}

/// `zz/exact.md` is titled `title` by its heading; `aa/*.md` notes, which
/// sort first on a score tie, repeat the title's words under a longer one.
fn case_corpus(title: &str) -> (TempDir, TempDir, Index) {
    let root = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    write(
        root.path(),
        "zz/exact.md",
        &format!("# {title}\n\n{}\n", "neutral padding ".repeat(200)),
    );
    for k in 0..PADDING_NOTES {
        write(
            root.path(),
            &format!("aa/other-{k}.md"),
            &format!(
                "# {title} discussion {k}\n\n{}\n",
                format!("{title} ").repeat(10)
            ),
        );
    }
    let mut index = open(&root, &cache);
    index.sync(root.path()).unwrap();
    (root, cache, index)
}

#[test]
fn exact_title_ignores_non_ascii_case() {
    let pairs = [
        ("Crème brûlée", "CRÈME BRÛLÉE"),
        ("Éclair", "éclair"),
        ("Ωμέγα λόγος", "ΩΜΈΓΑ ΛΌΓΟΣ"),
        ("Привет мир", "ПРИВЕТ МИР"),
    ];
    for (title, variant) in pairs {
        let (_root, _cache, index) = case_corpus(title);
        for text in [title, variant, &title.to_lowercase()] {
            for mode in [SearchMode::Text, SearchMode::Title] {
                assert_eq!(
                    first(&index, text, mode),
                    "zz/exact.md",
                    "title {title}, query {text}, mode {mode:?}"
                );
            }
        }
    }
}

#[test]
fn exact_title_for_a_korean_heading_is_unaffected() {
    let (_root, _cache, index) = case_corpus("가을 바다");
    for mode in [SearchMode::Text, SearchMode::Title] {
        assert_eq!(first(&index, "가을 바다", mode), "zz/exact.md");
        assert_eq!(first(&index, "  가을 바다 ", mode), "zz/exact.md");
    }
}

#[test]
fn a_query_equal_to_the_file_stem_of_a_heading_titled_note_ranks_it_first() {
    let root = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    write(
        root.path(),
        "zz/Harbor Log.md",
        &format!("# Weekly notes\n\n{}\n", "neutral padding ".repeat(200)),
    );
    for k in 0..PADDING_NOTES {
        write(
            root.path(),
            &format!("aa/other-{k}.md"),
            &format!("# Harbor log entry {k}\n\n{}\n", "harbor log ".repeat(10)),
        );
    }
    let mut index = open(&root, &cache);
    index.sync(root.path()).unwrap();
    for text in ["Harbor Log", "harbor log", "HARBOR LOG"] {
        for mode in [SearchMode::Text, SearchMode::Title] {
            assert_eq!(
                first(&index, text, mode),
                "zz/Harbor Log.md",
                "{text} {mode:?}"
            );
        }
    }
    // The heading still is a title: title mode finds it.
    assert_eq!(
        paths(&index, "Weekly notes", SearchMode::Title),
        ["zz/Harbor Log.md"]
    );
}

/// The notes behind every query a heading or memory summary title took
/// away at schema 8: words of a file name, an old frontmatter title.
fn recall_corpus() -> (TempDir, TempDir, Index) {
    let root = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    write(root.path(), "zz/old한글이름.md", "# 새 제목\n\n본문.\n");
    write(root.path(), "zz/中文旧名.md", "# 新标题\n\n正文。\n");
    write(root.path(), "zz/日本旧名前.md", "# 新しい題\n\n本文。\n");
    write(
        root.path(),
        "zz/memory.md",
        "---\ntype: memory\ntitle: Secret旧中文\nsummary: Current claim summary\n---\n\nShort claim.\n",
    );
    write(
        root.path(),
        "zz/stem-collision.md",
        "# Unrelated heading\n\nBody.\n",
    );
    write(root.path(), "aa/owner.md", "# stem-collision\n\nBody.\n");
    write(
        root.path(),
        "aa/comp-1-0.md",
        "# Compiler notes\n\nPlain body.\n",
    );
    let mut index = open(&root, &cache);
    index.sync(root.path()).unwrap();
    (root, cache, index)
}

#[test]
fn file_name_and_old_frontmatter_title_queries_keep_their_recall() {
    let (_root, _cache, index) = recall_corpus();
    // Text mode also matches part of a word through script n-grams; title
    // mode matches the whole name, as it always did.
    let text_only = [SearchMode::Text].as_slice();
    let both = [SearchMode::Text, SearchMode::Title].as_slice();
    let cases = [
        ("old한글이름", "zz/old한글이름.md", both),
        ("한글", "zz/old한글이름.md", text_only),
        ("中文旧名", "zz/中文旧名.md", both),
        ("旧名", "zz/中文旧名.md", text_only),
        ("旧名", "zz/日本旧名前.md", text_only),
        ("日本旧名前", "zz/日本旧名前.md", both),
        ("Secret旧中文", "zz/memory.md", both),
        ("旧中文", "zz/memory.md", text_only),
        ("中文", "zz/memory.md", text_only),
        ("中文", "zz/中文旧名.md", text_only),
        ("stem-collision", "zz/stem-collision.md", both),
        ("stem-collision", "aa/owner.md", both),
        ("comp", "aa/comp-1-0.md", both),
    ];
    for (text, path, modes) in cases {
        for &mode in modes {
            assert!(
                paths(&index, text, mode).iter().any(|p| p == path),
                "{text} {mode:?} lost {path}"
            );
        }
    }
    // Both titles are exact: the heading owner and the stem owner lead.
    let mut lead = paths(&index, "stem-collision", SearchMode::Text);
    lead.truncate(2);
    lead.sort();
    assert_eq!(lead, ["aa/owner.md", "zz/stem-collision.md"]);
}

fn fallback_corpus() -> (TempDir, TempDir, Index) {
    let root = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    write(root.path(), "h1.md", "# Delivered heading\nbodymarker\n");
    write(
        root.path(),
        "memory.md",
        "---\ntype: memory\nsummary: Memory summary\n---\n\nbodymarker\n",
    );
    let mut index = open(&root, &cache);
    index.sync(root.path()).unwrap();
    (root, cache, index)
}

fn fallback_answers(index: &Index) -> Vec<Vec<String>> {
    [
        ("Delivered heading", SearchMode::Title),
        ("Delivered heading", SearchMode::Text),
        ("Memory summary", SearchMode::Title),
        ("Memory summary", SearchMode::Text),
        ("h1", SearchMode::Title),
    ]
    .into_iter()
    .map(|(text, mode)| paths(index, text, mode))
    .collect()
}

#[test]
fn the_literal_fallback_matches_the_titles_the_complete_index_matches() {
    let (_root, _cache, mut index) = fallback_corpus();
    let complete = fallback_answers(&index);
    assert_eq!(
        complete,
        [
            vec!["h1.md"],
            vec!["h1.md"],
            vec!["memory.md"],
            vec!["memory.md"],
            vec!["h1.md"]
        ]
    );
    index.begin_rebuild().unwrap();
    assert!(index.search(&Query::text("bodymarker")).unwrap().fallback);
    assert_eq!(fallback_answers(&index), complete);
}
