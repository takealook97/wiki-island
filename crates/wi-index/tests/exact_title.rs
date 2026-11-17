//! A plain-text query equal to a note's delivered title ranks that note
//! first, even when many imported conversations repeat the title's words.

use std::{fs, path::Path};
use tempfile::TempDir;
use wi_index::{Index, Query};

const ADJECTIVES: [&str; 20] = [
    "amber", "brisk", "calm", "dusty", "eager", "faint", "gentle", "hollow", "ivory", "jolly",
    "keen", "lunar", "mellow", "noble", "opal", "placid", "quiet", "rustic", "silver", "tidy",
];
const NOUNS: [&str; 20] = [
    "garden", "harbor", "island", "jungle", "kitchen", "lantern", "meadow", "orchard", "pasture",
    "quarry", "river", "studio", "temple", "valley", "window", "yard", "zenith", "bridge",
    "canyon", "dune",
];
const CONVERSATIONS_PER_TITLE: usize = 25;

fn title(i: usize) -> String {
    format!("{} {}", ADJECTIVES[i], NOUNS[i])
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// Ordinary notes sort after every conversation path, so a score tie cannot
/// rescue them. `titled` puts the title in frontmatter; otherwise it is the
/// first heading and the file stem differs from it.
fn fill(root: &Path, titled: bool) {
    for i in 0..20 {
        let t = title(i);
        let body = if titled {
            format!("---\ntitle: {t}\n---\n\nA short ordinary note.\n")
        } else {
            format!("# {t}\n\nA short ordinary note.\n")
        };
        write(root, &format!("zz/note-{i:02}.md"), &body);
        for k in 0..CONVERSATIONS_PER_TITLE {
            let body = format!("# {t} conversation {k}\n\n{t} {t} {t} talk about the {t} again\n");
            write(root, &format!("AI/Claude/c{i:02}-{k:02}.md"), &body);
        }
    }
}

fn corpus(titled: bool) -> (TempDir, TempDir, Index) {
    let root = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let mut index = Index::open_at(root.path(), "exact", cache.path().join("i.db")).unwrap();
    fill(root.path(), titled);
    index.sync(root.path()).unwrap();
    (root, cache, index)
}

fn first_path(index: &Index, text: &str) -> String {
    let hits = index.search(&Query::text(text)).unwrap().hits;
    hits.first()
        .map(|h| h.rel_path.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[test]
fn search_exact_title_outranks_body_matches() {
    for titled in [false, true] {
        let (_root, _cache, index) = corpus(titled);
        for i in 0..20 {
            assert_eq!(
                first_path(&index, &title(i)),
                format!("zz/note-{i:02}.md"),
                "title {} (frontmatter: {titled})",
                title(i)
            );
        }
    }
}

#[test]
fn search_exact_title_ignores_case_and_surrounding_spaces() {
    for titled in [false, true] {
        let (_root, _cache, index) = corpus(titled);
        for i in 0..20 {
            let query = format!("  {}  ", title(i).to_uppercase());
            assert_eq!(first_path(&index, &query), format!("zz/note-{i:02}.md"));
        }
    }
}

#[test]
fn search_exact_title_uses_the_summary_of_a_memory_note() {
    let root = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let mut index = Index::open_at(root.path(), "exact", cache.path().join("i.db")).unwrap();
    fill(root.path(), false);
    let summary = "Tidy dune facts";
    write(
        root.path(),
        "zz/memory.md",
        &format!("---\ntype: memory\ntitle: Other name\nsummary: {summary}\n---\n\nShort claim.\n"),
    );
    for k in 0..CONVERSATIONS_PER_TITLE {
        let body = format!("# {summary} {k}\n\n{summary} {summary} {summary} again\n");
        write(root.path(), &format!("AI/Claude/m{k:02}.md"), &body);
    }
    index.sync(root.path()).unwrap();
    assert_eq!(first_path(&index, summary), "zz/memory.md");
}

#[test]
fn link_resolution_by_title_is_unchanged_for_a_heading_titled_note() {
    let root = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let mut index = Index::open_at(root.path(), "exact", cache.path().join("i.db")).unwrap();
    write(root.path(), "ordinary.md", "# Amber garden\n\nBody.\n");
    write(
        root.path(),
        "linker.md",
        "---\npermalink: linker\n---\n- relates_to [[Amber garden]]\n- relates_to [[ordinary]]\n",
    );
    index.sync(root.path()).unwrap();
    let edges = index.context("linker", 1).unwrap().edges;
    let target = |name: &str| {
        edges
            .iter()
            .find(|e| e.to_name == name)
            .unwrap()
            .to_path
            .clone()
    };
    assert_eq!(target("Amber garden"), None);
    assert_eq!(
        target("ordinary").as_deref(),
        Some(Path::new("ordinary.md"))
    );
}

#[test]
fn an_index_built_at_schema_7_is_rebuilt_on_open_and_ranks_correctly() {
    let (root, cache, index) = corpus(false);
    let db = cache.path().join("i.db");
    drop(index);
    let c = rusqlite::Connection::open(&db).unwrap();
    // What schema 7 stored: the file stem, not the heading.
    c.execute("UPDATE search_fts SET title=''", []).unwrap();
    c.pragma_update(None, "user_version", 7).unwrap();
    drop(c);
    let mut index = Index::open_at(root.path(), "exact", &db).unwrap();
    assert!(index.search(&Query::text(title(0))).unwrap().fallback);
    index.sync(root.path()).unwrap();
    for i in 0..20 {
        assert_eq!(first_path(&index, &title(i)), format!("zz/note-{i:02}.md"));
    }
}
