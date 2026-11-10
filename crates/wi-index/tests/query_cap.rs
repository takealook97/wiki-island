//! Text and title queries are capped at `MAX_QUERY_BYTES` and
//! `MAX_QUERY_TERMS`; an over-cap query is refused, never answered by the
//! literal scan fallback.
use std::fs;
use tempfile::TempDir;
use wi_index::{
    Error, Index, MAX_QUERY_BYTES, MAX_QUERY_TERMS, Query, SearchMode, literal_search,
    search_at_with_fallback,
};

fn synced() -> (TempDir, TempDir, Index) {
    let root = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    fs::write(root.path().join("a.md"), "alpha beta\n").unwrap();
    let mut index = Index::open_at(root.path(), "querycap", cache.path().join("index.db")).unwrap();
    index.sync(root.path()).unwrap();
    (root, cache, index)
}

/// One term of `bytes` bytes.
fn long_term(bytes: usize) -> String {
    "a".repeat(bytes)
}

/// `terms` short terms.
fn many_terms(terms: usize) -> String {
    vec!["alpha"; terms].join(" ")
}

/// Exactly `MAX_QUERY_TERMS` terms in exactly `MAX_QUERY_BYTES` bytes.
fn at_both_caps() -> String {
    let spaces = MAX_QUERY_TERMS - 1;
    let per = (MAX_QUERY_BYTES - spaces) / MAX_QUERY_TERMS;
    let mut terms = vec!["a".repeat(per); MAX_QUERY_TERMS];
    let extra = MAX_QUERY_BYTES - spaces - per * MAX_QUERY_TERMS;
    terms[0].push_str(&"a".repeat(extra));
    let text = terms.join(" ");
    assert_eq!(text.len(), MAX_QUERY_BYTES);
    assert_eq!(text.split_whitespace().count(), MAX_QUERY_TERMS);
    text
}

fn assert_too_long(result: wi_index::Result<wi_index::SearchResult>, bytes: usize, terms: usize) {
    match result {
        Err(Error::QueryTooLong { bytes: b, terms: t }) => assert_eq!((b, t), (bytes, terms)),
        Err(other) => panic!("{other}"),
        Ok(found) => panic!("over-cap query answered (fallback {})", found.fallback),
    }
}

#[test]
fn caps_are_the_documented_values() {
    assert_eq!(MAX_QUERY_BYTES, 2048);
    assert_eq!(MAX_QUERY_TERMS, 64);
}

#[test]
fn a_query_one_byte_or_one_term_over_the_cap_is_refused() {
    let (_root, _cache, index) = synced();
    for mode in [SearchMode::Text, SearchMode::Title] {
        let query = Query {
            text: long_term(MAX_QUERY_BYTES + 1),
            mode,
            ..Query::default()
        };
        assert_too_long(index.search(&query), MAX_QUERY_BYTES + 1, 1);
        let text = many_terms(MAX_QUERY_TERMS + 1);
        let bytes = text.len();
        let query = Query {
            text,
            mode,
            ..Query::default()
        };
        assert_too_long(index.search(&query), bytes, MAX_QUERY_TERMS + 1);
    }
}

#[test]
fn a_query_at_both_caps_is_accepted() {
    let (_root, _cache, index) = synced();
    for mode in [SearchMode::Text, SearchMode::Title] {
        let query = Query {
            text: at_both_caps(),
            mode,
            ..Query::default()
        };
        assert!(index.search(&query).is_ok());
    }
    let found = index
        .search(&Query::text(many_terms(MAX_QUERY_TERMS)))
        .unwrap();
    assert!(!found.fallback);
    assert_eq!(found.hits.len(), 1);
}

#[test]
fn an_over_cap_query_selects_neither_fallback() {
    let (root, cache, index) = synced();
    // An unclosed phrase is a syntax error the index answers with the
    // literal scan; over the cap the same text is refused instead.
    let unclosed = Query::text("\"alpha");
    assert!(index.search(&unclosed).unwrap().fallback);
    let text = format!("\"alpha {}", many_terms(MAX_QUERY_TERMS));
    let bytes = text.len();
    let over = Query::text(text);
    assert_too_long(index.search(&over), bytes, MAX_QUERY_TERMS + 1);
    // Neither the literal scan nor the fallback wrapper answers it, also
    // when no database exists.
    assert_too_long(
        literal_search(root.path(), &over),
        bytes,
        MAX_QUERY_TERMS + 1,
    );
    assert_too_long(
        search_at_with_fallback(
            root.path(),
            "querycap",
            cache.path().join("index.db"),
            &over,
        ),
        bytes,
        MAX_QUERY_TERMS + 1,
    );
    assert_too_long(
        search_at_with_fallback(
            root.path(),
            "querycap",
            cache.path().join("absent.db"),
            &over,
        ),
        bytes,
        MAX_QUERY_TERMS + 1,
    );
}

#[test]
fn exact_permalink_identities_carry_no_term_cap() {
    let (_root, _cache, index) = synced();
    let query = Query {
        text: many_terms(MAX_QUERY_TERMS + 1),
        mode: SearchMode::Permalink,
        ..Query::default()
    };
    assert!(index.search(&query).is_ok());
}

#[test]
fn a_text_or_title_query_above_the_older_bound_is_too_long_at_any_size() {
    let (_root, _cache, index) = synced();
    for bytes in [16_385, 100_000] {
        for mode in [SearchMode::Text, SearchMode::Title] {
            let query = Query {
                text: long_term(bytes),
                mode,
                ..Query::default()
            };
            assert_too_long(index.search(&query), bytes, 1);
        }
    }
}

#[test]
fn a_permalink_identity_above_the_older_bound_is_still_invalid() {
    let (_root, _cache, index) = synced();
    let query = Query {
        text: long_term(16_385),
        mode: SearchMode::Permalink,
        ..Query::default()
    };
    assert!(matches!(
        index.search(&query),
        Err(Error::InvalidQuery(message)) if message == "limit or text exceeds bounds"
    ));
}
