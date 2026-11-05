//! Ranked search with a small limit returns exactly the prefix of the
//! unbounded ranking, keeps strict (all-terms) hits first and counts
//! matches without materializing them.

use std::fs;
use tempfile::TempDir;
use wi_index::{Index, Query, literal_search};

const WORDS: [&str; 6] = ["payment", "refund", "invoice", "결제", "환불", "정책이"];

fn corpus() -> (TempDir, TempDir, Index) {
    let root = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let mut index = Index::open_at(root.path(), "bounded", cache.path().join("i.db")).unwrap();
    for i in 0..240usize {
        // Deterministic mixes: some notes hold every term, most hold one.
        let words: Vec<&str> = WORDS
            .iter()
            .enumerate()
            .filter(|(j, _)| (i * 7 + j * 3) % (j + 2) == 0)
            .map(|(_, w)| *w)
            .collect();
        let body = format!("# Note {i}\n\n{} filler {}\n", words.join(" "), i % 13);
        fs::write(root.path().join(format!("n{i:03}.md")), body).unwrap();
    }
    index.sync(root.path()).unwrap();
    (root, cache, index)
}

fn paths(index: &Index, text: &str, limit: usize) -> Vec<String> {
    let query = Query {
        limit,
        ..Query::text(text)
    };
    index
        .search(&query)
        .unwrap()
        .hits
        .into_iter()
        .map(|h| h.rel_path.to_string_lossy().into_owned())
        .collect()
}

const QUERIES: [&str; 7] = [
    "payment",
    "payment refund",
    "payment nonexistentword",
    "환불 정책이",
    "결제 환불",
    "환불 NOT 정책이",
    "refund invoice 결제",
];

#[test]
fn bounded_limits_return_the_prefix_of_the_unbounded_ranking() {
    let (_root, _cache, index) = corpus();
    for text in QUERIES {
        let full = paths(&index, text, 10_000);
        assert!(!full.is_empty(), "{text}");
        for limit in [1, 3, 8, 32, 100] {
            let bounded = paths(&index, text, limit);
            let expected: Vec<_> = full.iter().take(limit).cloned().collect();
            assert_eq!(bounded, expected, "{text} limit {limit}");
        }
    }
}

#[test]
fn strict_matches_rank_ahead_of_partial_matches() {
    let (root, _cache, index) = corpus();
    let hits = paths(&index, "payment refund", 10_000);
    let strict: Vec<bool> = hits
        .iter()
        .map(|p| {
            let body = fs::read_to_string(root.path().join(p)).unwrap();
            body.contains("payment") && body.contains("refund")
        })
        .collect();
    let first_partial = strict.iter().position(|s| !s).unwrap();
    assert!(first_partial > 0, "some notes hold both terms");
    assert!(strict[first_partial..].iter().all(|s| !s), "{hits:?}");
}

#[test]
fn counts_are_exact_below_the_limit_and_flag_more_above_it() {
    let (root, _cache, index) = corpus();
    for text in QUERIES {
        let all = paths(&index, text, 10_000).len();
        let counted = |count_limit| {
            index
                .search(&Query {
                    count_limit,
                    ..Query::text(text)
                })
                .unwrap()
                .matches
        };
        assert_eq!(counted(0), None);
        assert_eq!(counted(1_000), Some(all), "{text}");
        if all > 5 {
            assert_eq!(counted(5), Some(6), "{text}: counted up to limit + 1");
        }
    }
    let fallback = literal_search(
        root.path(),
        &Query {
            count_limit: 1_000,
            ..Query::text("payment")
        },
    )
    .unwrap();
    assert_eq!(fallback.hits.len(), 8);
    assert!(fallback.matches.is_some_and(|m| m > 8));
}
