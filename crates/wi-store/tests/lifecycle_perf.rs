//! Current-mode search latency with lifecycle eligibility. Run in release:
//! `cargo test -p wi-store --release --test lifecycle_perf -- --ignored --nocapture`.

mod common;

use std::time::Instant;

use common::Fixture;
use serde_json::Value;
use wi_store::{Query, SearchOptions};

const NOTES: usize = 10_000;
const QUERIES: usize = 200;

#[test]
#[ignore = "measurement; run in release"]
fn measure_current_search_on_10k_notes_with_30_percent_lifecycle() {
    let (_f, vault) = Fixture::new();
    let source = b"# Runbook\n\nport 443\n";
    std::fs::create_dir_all(vault.root().join("notes")).unwrap();
    std::fs::create_dir_all(vault.root().join("memory")).unwrap();
    std::fs::write(vault.root().join("notes/runbook.md"), source).unwrap();
    let hash = wi_lifecycle::sha256(source);
    let words = [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
    ];
    for i in 0..NOTES {
        let word = words[i % words.len()];
        if i % 10 < 3 {
            let text = format!(
                "---\ntype: memory\nkind: fact\nclaim_kind: historical\nstatus: active\nsummary: \"{word} claim {i}\"\nentities: [{{\"id\": \"e:{}\", \"label\": null, \"source\": null}}]\nscope: work\napplicability: {{\"from\": null, \"until\": null, \"conditions\": [], \"supersedes\": []}}\nobserved_at: 2026-01-01T00:00:00Z\norigin: {{\"kind\": \"user\", \"locator\": \"file\", \"captured_at\": null}}\nsources: [{{\"ref\": \"notes/runbook.md\", \"commit\": null, \"pending\": true, \"source_hash\": \"{hash}\"}}]\n---\n{word} claim {i} body text.\n",
                i % 97
            );
            std::fs::write(vault.root().join(format!("memory/m{i}.md")), text).unwrap();
        } else {
            let text = format!("---\ntitle: note {i}\n---\n{word} plain note {i} body.\n");
            std::fs::write(vault.root().join(format!("notes/n{i}.md")), text).unwrap();
        }
    }
    vault.reindex().unwrap();
    let mut times = Vec::with_capacity(QUERIES);
    let mut cold = 0.0;
    for q in 0..QUERIES + 20 {
        let query = Query::text(words[q % words.len()]);
        let start = Instant::now();
        let found = vault
            .search_current(&query, &Value::Null, SearchOptions::default())
            .unwrap();
        let elapsed = start.elapsed();
        assert!(!found.response.hits.is_empty());
        if q == 0 {
            cold = elapsed.as_secs_f64() * 1000.0;
        }
        if q >= 20 {
            times.push(elapsed.as_secs_f64() * 1000.0);
        }
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| times[((times.len() as f64 - 1.0) * q).round() as usize];
    println!(
        "notes={NOTES} lifecycle=30% queries={QUERIES} current_search cold={cold:.2}ms p50={:.2}ms p95={:.2}ms max={:.2}ms",
        p(0.5),
        p(0.95),
        times[times.len() - 1]
    );
}
