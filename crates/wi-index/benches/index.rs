use criterion::{Criterion, criterion_group};
use std::{fs, time::Instant};
use tempfile::TempDir;
use wi_index::{Index, Query};

fn corpus(count: usize) -> TempDir {
    let dir = TempDir::new().unwrap();
    for i in 0..count {
        fs::write(
            dir.path().join(format!("note-{i:05}.md")),
            format!(
                "---\ntitle: Note {i}\ntype: note\n---\n{}\n",
                "결제와 환불 정책을 기록합니다. English payment and refund policy. ".repeat(16)
            ),
        )
        .unwrap();
    }
    dir
}
fn bench(c: &mut Criterion) {
    for count in [1_000, 10_000] {
        let vault = corpus(count);
        let mut index = Index::open_in_memory(vault.path(), "bench").unwrap();
        c.bench_function(&format!("rebuild-{count}"), |b| {
            b.iter(|| index.rebuild().unwrap())
        });
        c.bench_function(&format!("stat-sync-{count}"), |b| {
            b.iter(|| index.sync(vault.path()).unwrap())
        });
        let query = Query::text("환불 payment");
        c.bench_function(&format!("search-{count}"), |b| {
            b.iter(|| index.search(&query).unwrap())
        });
    }
}
// Set WI_INDEX_MEASURE=1 for a single disk-backed measurement (including corpus
// shape and percentiles); Criterion's normal mode remains available separately.
fn main() {
    if std::env::var_os("WI_INDEX_MEASURE").is_some() {
        for count in [1_000, 10_000] {
            let vault = corpus(count);
            let sizes: Vec<_> = fs::read_dir(vault.path())
                .unwrap()
                .map(|f| f.unwrap().metadata().unwrap().len())
                .collect();
            let cache = TempDir::new().unwrap();
            let mut index =
                Index::open_at(vault.path(), "bench", cache.path().join("index.sqlite3")).unwrap();
            let start = Instant::now();
            index.rebuild().unwrap();
            let rebuild = start.elapsed();
            let start = Instant::now();
            index.sync(vault.path()).unwrap();
            let sync = start.elapsed();
            let queries = ["환불", "결제", "payment", "환불 payment", "missing"];
            let mut times = Vec::new();
            for _ in 0..100 {
                for text in queries {
                    let start = Instant::now();
                    std::hint::black_box(index.search(&Query::text(text)).unwrap());
                    times.push(start.elapsed().as_secs_f64() * 1000.0);
                }
            }
            times.sort_by(f64::total_cmp);
            let size: u64 = fs::read_dir(cache.path())
                .unwrap()
                .map(|f| f.unwrap().metadata().unwrap().len())
                .sum();
            println!(
                "sqlite={}, notes={count}, bytes_per_note={}..={}, rebuild_ms={:.3}, stat_sync_ms={:.3}, search_n={}, p50_ms={:.3}, p95_ms={:.3}, cache_bytes={size}",
                rusqlite::version(),
                sizes.iter().min().unwrap(),
                sizes.iter().max().unwrap(),
                rebuild.as_secs_f64() * 1000.0,
                sync.as_secs_f64() * 1000.0,
                times.len(),
                times[times.len() / 2],
                times[times.len() * 95 / 100]
            );
        }
    } else {
        criterion_main_entry();
    }
}
criterion_group! { name = benches; config = Criterion::default().sample_size(10); targets = bench }
fn criterion_main_entry() {
    benches();
    Criterion::default().configure_from_args().final_summary();
}
