//! Reproducible release-only index measurements on disposable local corpora.
use std::{fs, path::Path, time::Instant};
use tempfile::TempDir;
use wi_index::{Index, Query};

fn percentile(values: &[f64], p: f64) -> f64 {
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    values[((values.len() - 1) as f64 * p).round() as usize]
}
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &to.join(entry.file_name()));
        } else if entry.file_type().unwrap().is_file() {
            fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
}
fn collect_paths(root: &Path, dir: &Path, paths: &mut Vec<String>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            collect_paths(root, &entry.path(), paths);
        } else if entry.path().extension().is_some_and(|ext| ext == "md") {
            paths.push(
                entry
                    .path()
                    .strip_prefix(root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string(),
            );
        }
    }
}
fn measure(vault: &Path, name: &str, count: usize) {
    let cache = TempDir::new().unwrap();
    let mut index = Index::open_at(vault, "bench", cache.path().join("index.db")).unwrap();
    let t = Instant::now();
    index.sync(vault).unwrap();
    let first = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let unchanged = index.sync(vault).unwrap();
    let stat_ms = t.elapsed().as_secs_f64() * 1000.;
    let t = Instant::now();
    index.rebuild().unwrap();
    let rebuild = t.elapsed().as_secs_f64();
    let mut paths = Vec::new();
    collect_paths(vault, vault, &mut paths);
    paths.sort();
    assert_eq!(paths.len(), count);
    let mut updates = Vec::new();
    for i in 0..60 {
        let rel = &paths[i * 13 % paths.len()];
        let path = vault.join(rel);
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str(&format!("\nedit {i}\n"));
        fs::write(path, text).unwrap();
        let t = Instant::now();
        index.update_paths(&[rel]).unwrap();
        updates.push(t.elapsed().as_secs_f64() * 1000.);
    }
    let queries = [
        "환불",
        "결제",
        "payment",
        "환불 payment",
        "missing",
        "database migration",
        "정책을",
        "決済",
        "refund policy",
        "ラーメン",
        "newcontent",
    ];
    let mut searches = Vec::new();
    for _ in 0..20 {
        for query in queries {
            let t = Instant::now();
            std::hint::black_box(index.search(&Query::text(query)).unwrap());
            searches.push(t.elapsed().as_secs_f64() * 1000.);
        }
    }
    println!(
        "corpus={name} n={count} sqlite={} first_s={first:.3} unchanged_ms={stat_ms:.3} unchanged_hashed={} populated_rebuild_s={rebuild:.3} update_n={} update_p50_ms={:.3} update_p95_ms={:.3} search_n={} search_p50_ms={:.3} search_p95_ms={:.3}",
        rusqlite::version(),
        unchanged.hashed,
        updates.len(),
        percentile(&updates, 0.5),
        percentile(&updates, 0.95),
        searches.len(),
        percentile(&searches, 0.5),
        percentile(&searches, 0.95)
    );
    assert!(rebuild < 10.0, "10k rebuild budget: {rebuild}s");
    assert!(percentile(&updates, 0.95) < 15.0, "targeted update budget");
}
#[test]
#[ignore = "release timing regression; optionally set WI_INDEX_CORPUS to an existing 10k-note corpus"]
fn index_performance() {
    if cfg!(debug_assertions) {
        panic!("run with --release");
    }
    if let Ok(source) = std::env::var("WI_INDEX_CORPUS") {
        let root = TempDir::new().unwrap();
        copy_tree(Path::new(&source), root.path());
        measure(root.path(), "external-10k-corpus", 10_000);
    }
    for (count, observations, relations) in [(5_000, 8, 0), (10_000, 8, 0), (10_000, 8, 5)] {
        let root = TempDir::new().unwrap();
        for i in 0..count {
            let obs: String = (0..observations)
                .map(|k| format!("- [fact] observation {k} of note {i} 환불 payment #t{k}\n"))
                .collect();
            let links: String = (1..=relations)
                .map(|j| format!("- links_to [[Note {}]]\n", (i + j * 13) % count))
                .collect();
            fs::write(
                root.path().join(format!("n{i}.md")),
                format!("---\ntitle: Note {i}\n---\n# Note {i}\n\n{obs}{links}"),
            )
            .unwrap();
        }
        measure(
            root.path(),
            &format!("index-{observations}obs-{relations}rels"),
            count,
        );
    }
}
