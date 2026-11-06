//! Single-note save latency (tool call to
//! searchable; git commit excluded) and startup to ready on 1k/10k notes.
//!
//! `WI_STORE_MEASURE=1 cargo bench -p wi-store --bench store` prints p50/p95
//! per corpus; plain `cargo bench` runs the Criterion harness on 1k notes.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group};
use tempfile::TempDir;
use wi_store::{Ingress, OpenOptions, Vault, WriteRequest};

const SENTENCE: &str =
    "결제 후 7일 안에 환불합니다. Refunds are paid within seven days of payment. ";

struct Corpus {
    _dir: TempDir,
    cache: TempDir,
    root: std::path::PathBuf,
}

impl Corpus {
    fn options(&self) -> OpenOptions {
        OpenOptions {
            index_path: Some(self.cache.path().join("index.sqlite3")),
        }
    }
}

fn corpus(notes: usize) -> Corpus {
    let dir = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = dir.path().join("vault");
    let options = OpenOptions {
        index_path: Some(cache.path().join("index.sqlite3")),
    };
    let (vault, _) = Vault::init_with(&root, options.clone()).unwrap();
    let root = vault.root().to_path_buf();
    for i in 0..notes {
        let folder = root.join(format!("notes/{:03}", i % 100));
        std::fs::create_dir_all(&folder).unwrap();
        let body = SENTENCE.repeat(8 + i % 9);
        std::fs::write(
            folder.join(format!("n{i}.md")),
            format!("---\ntitle: note {i}\ntype: note\n---\n{body}\n"),
        )
        .unwrap();
    }
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "corpus"]);
    vault.reindex().unwrap();
    Corpus {
        _dir: dir,
        cache,
        root,
    }
}

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success());
}

fn percentile(samples: &mut [Duration], p: f64) -> f64 {
    samples.sort();
    let rank = ((p * samples.len() as f64).ceil() as usize).clamp(1, samples.len());
    samples[rank - 1].as_secs_f64() * 1000.0
}

fn save(vault: &Vault, i: usize) {
    vault
        .write(WriteRequest {
            path: format!("bench/save-{i}.md"),
            content: format!("---\ntitle: save {i}\n---\n{SENTENCE}\n").into_bytes(),
            expected_checksum: None,
            op_id: None,
            ingress: Ingress::Cli,
        })
        .unwrap();
}

fn measure(notes: usize, durability: &str) {
    let corpus = corpus(notes);
    let config = corpus.root.join(".wiki-island/config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{text}durability = \"{durability}\"\n")).unwrap();
    let vault = Vault::open_with(&corpus.root, corpus.options()).unwrap();
    for i in 0..10 {
        save(&vault, 1_000_000 + i);
    }
    let mut saves = Vec::new();
    for i in 0..200 {
        let start = Instant::now();
        save(&vault, i);
        saves.push(start.elapsed());
    }
    vault.flush().unwrap();
    drop(vault);
    let mut opens = Vec::new();
    for _ in 0..30 {
        let start = Instant::now();
        let vault = Vault::open_with(&corpus.root, corpus.options()).unwrap();
        vault.generation().unwrap();
        opens.push(start.elapsed());
    }
    let mut ready = Vec::new();
    for _ in 0..30 {
        let start = Instant::now();
        let vault = Vault::open_with(&corpus.root, corpus.options()).unwrap();
        vault.index_as_of().unwrap();
        ready.push(start.elapsed());
    }
    println!(
        "notes={notes} durability={durability} save_ms p50={:.3} p95={:.3} (n=200) open_ms p50={:.3} p95={:.3} open_with_index_ms p50={:.3} p95={:.3} (n=30)",
        percentile(&mut saves.clone(), 0.5),
        percentile(&mut saves, 0.95),
        percentile(&mut opens.clone(), 0.5),
        percentile(&mut opens, 0.95),
        percentile(&mut ready.clone(), 0.5),
        percentile(&mut ready, 0.95),
    );
}

fn criterion_benches(c: &mut Criterion) {
    let corpus = corpus(1_000);
    let vault = Vault::open_with(&corpus.root, corpus.options()).unwrap();
    let mut i = 0;
    c.bench_function("save_1k", |b| {
        b.iter(|| {
            i += 1;
            save(&vault, i);
        })
    });
    c.bench_function("open_1k", |b| {
        b.iter(|| Vault::open_with(&corpus.root, corpus.options()).unwrap())
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(20).measurement_time(Duration::from_secs(3));
    targets = criterion_benches
}

fn main() {
    if std::env::var_os("WI_STORE_MEASURE").is_some() {
        for durability in ["standard", "full"] {
            measure(1_000, durability);
            measure(10_000, durability);
        }
        return;
    }
    benches();
    Criterion::default().configure_from_args().final_summary();
}
