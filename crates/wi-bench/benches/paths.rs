use criterion::{Criterion, criterion_group, criterion_main};
use std::time::Duration;
use wi_bench::{WORDS, prepare};
use wi_store::{Ingress, OpenOptions, Query, SearchOptions, Vault, WriteRequest};

fn paths(c: &mut Criterion) {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("vault");
    let cache = scratch.path().join("index.sqlite3");
    let (vault, _) = prepare(&root, &cache, 1000).unwrap();
    let mut id = 0;
    let mut checksum = None;
    c.bench_function("save-standard-1k", |b| {
        b.iter(|| {
            id += 1;
            let result = vault
                .write(WriteRequest {
                    path: "bench/save.md".into(),
                    content: format!(
                        "---\ntitle: Save\ntype: note\n---\nSynthetic save {id}. 결제 환불。\n"
                    )
                    .into_bytes(),
                    expected_checksum: checksum.clone(),
                    op_id: None,
                    ingress: Ingress::Mcp,
                })
                .unwrap();
            checksum = Some(result.checksum);
        })
    });
    vault.flush().unwrap();
    c.bench_function("search-current-1k", |b| {
        b.iter(|| {
            vault
                .search_current(
                    &Query::text(WORDS[0]),
                    &serde_json::Value::Null,
                    SearchOptions { catch_up: false },
                )
                .unwrap()
        })
    });
    c.bench_function("context-depth2-1k", |b| {
        b.iter(|| vault.context("notes/000/n00000", 2).unwrap())
    });
    c.bench_function("reindex-1k", |b| b.iter(|| vault.reindex().unwrap()));
    c.bench_function("ready-library-1k", |b| {
        b.iter(|| {
            let v = Vault::open_with(
                &root,
                OpenOptions {
                    index_path: Some(cache.clone()),
                },
            )
            .unwrap();
            v.index_as_of().unwrap()
        })
    });
}
criterion_group! { name=benches;config=Criterion::default().sample_size(10).measurement_time(Duration::from_secs(2));targets=paths }
criterion_main!(benches);
