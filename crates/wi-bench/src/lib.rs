//! Reproducible, opt-in measurement; no production defaults are changed.
#![forbid(unsafe_code)]

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Instant;
use wi_store::{Ingress, OpenOptions, Query, SearchOptions, Vault, WriteRequest};

pub const CORPUS_VERSION: &str = "mixed-v1";
pub const WORDS: [&str; 8] = [
    "payment", "refund", "search", "storage", "결제", "환불", "검색", "保存",
];
pub const SOURCE: &str = "# Source\n\nSynthetic benchmark evidence.\n";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Corpus {
    pub version: String,
    pub notes: usize,
    pub lifecycle_notes: usize,
    pub tree_sha256: String,
    pub bytes_min: usize,
    pub bytes_p50: usize,
    pub bytes_p95: usize,
    pub bytes_max: usize,
    pub bytes_total: usize,
}

/// Flat id ordering, 100 folders, varying lengths, three observations and two
/// resolved relation edges per note. Every tenth note has a longer body.
/// Exactly 30% are active, historical, source-pinned lifecycle claims.
pub fn note(i: usize, count: usize, source_hash: &str) -> String {
    let word = WORDS[i % WORDS.len()];
    let path = |id: usize| format!("notes/{:03}/n{id:05}", id % 100);
    let mut header = format!("---\ntitle: Synthetic {i}\npermalink: {}\n", path(i));
    if i % 10 < 3 {
        header.push_str(&format!(
            "type: memory\nkind: fact\nclaim_kind: historical\nstatus: active\nsummary: {word} claim {i}\nentities: [{{id: 'synthetic:{}'}}]\nscope: work\napplicability: {{from: null, until: null, conditions: [], supersedes: []}}\nobserved_at: 2026-01-01T00:00:00Z\norigin: {{kind: user, locator: file, captured_at: null}}\nsources: [{{ref: source.md, commit: null, pending: true, source_hash: '{source_hash}', locator: {{span: {{start: 0, end: {}}}}}, span_hash: '{source_hash}'}}]\n",
            i % 97, SOURCE.len()
        ));
    } else {
        header.push_str("type: note\n");
    }
    header.push_str("---\n\n");
    let repetitions = if i % 10 == 9 { 40 + i % 17 } else { 2 + i % 13 };
    header.push_str(
        &format!("{word} record {i}. 결제와 환불 정책. English knowledge. 日本語の記録。\n")
            .repeat(repetitions),
    );
    header.push_str(&format!(
        "\n## Observations\n- [fact] {word} observed synthetic fact {i}\n- [detail] multilingual observation\n- [source] local evidence\n\n## Relations\n- relates_to [[{}]]\n- supports [[{}]]\n",
        path((i + 1) % count), path((i + 7) % count)
    ));
    header
}

pub fn generate(root: &Path, count: usize) -> Result<Corpus> {
    ensure!(count >= 10, "corpus requires at least ten notes");
    ensure!(!root.exists(), "refusing to overwrite existing corpus");
    fs::create_dir_all(root)?;
    fs::write(root.join("source.md"), SOURCE)?;
    let hash = wi_history::hash_bytes(SOURCE.as_bytes());
    let mut sizes = vec![SOURCE.len()];
    let mut hashes = BTreeMap::from([("source.md".to_owned(), hash.clone())]);
    for i in 0..count - 1 {
        let folder = root.join(format!("notes/{:03}", i % 100));
        fs::create_dir_all(&folder)?;
        let text = note(i, count - 1, &hash);
        sizes.push(text.len());
        hashes.insert(
            format!("notes/{:03}/n{i:05}.md", i % 100),
            wi_history::hash_bytes(text.as_bytes()),
        );
        fs::write(folder.join(format!("n{i:05}.md")), text)?;
    }
    sizes.sort_unstable();
    let fingerprint: String = hashes
        .iter()
        .map(|(path, hash)| format!("{path}\0{hash}\n"))
        .collect();
    Ok(Corpus {
        tree_sha256: wi_history::hash_bytes(fingerprint.as_bytes()),
        version: CORPUS_VERSION.into(),
        notes: count,
        lifecycle_notes: (0..count - 1).filter(|i| i % 10 < 3).count(),
        bytes_min: sizes[0],
        bytes_p50: sizes[sizes.len() / 2],
        bytes_p95: sizes[sizes.len() * 95 / 100],
        bytes_max: sizes[sizes.len() - 1],
        bytes_total: sizes.iter().sum(),
    })
}

pub fn command_text(program: &str, args: &[&str]) -> Result<String> {
    #[cfg(windows)]
    if matches!(program, "sysctl" | "uname" | "sw_vers" | "python3") {
        return Ok("unknown".into());
    }
    let output = Command::new(program).args(args).output()?;
    ensure!(
        output.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Machine {
    pub model: String,
    pub arch: String,
    pub memory_bytes: String,
    pub os: String,
    pub rust: String,
}
impl Machine {
    pub fn detect() -> Result<Self> {
        Ok(Self {
            model: command_text("sysctl", &["-n", "machdep.cpu.brand_string"])?,
            arch: command_text("uname", &["-m"])?,
            memory_bytes: command_text("sysctl", &["-n", "hw.memsize"])?,
            os: command_text("sw_vers", &["-productVersion"])?,
            rust: command_text("rustc", &["--version"])?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metric {
    pub unit: String,
    pub n: usize,
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
    pub target: Option<f64>,
    pub pass: Option<bool>,
}
impl Metric {
    pub fn samples(mut values: Vec<f64>, unit: &str, target: Option<f64>) -> Result<Self> {
        ensure!(
            !values.is_empty() && values.iter().all(|v| v.is_finite() && *v >= 0.0),
            "invalid samples"
        );
        values.sort_by(f64::total_cmp);
        let percentile = |p: f64| values[((values.len() as f64 * p).ceil() as usize).max(1) - 1];
        let p95 = percentile(0.95);
        Ok(Self {
            unit: unit.into(),
            n: values.len(),
            p50: percentile(0.5),
            p95,
            max: values[values.len() - 1],
            target,
            pass: target.map(|t| p95 <= t),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub schema: u32,
    pub date: String,
    pub commit: String,
    pub machine: Machine,
    pub corpus: Corpus,
    pub metrics: BTreeMap<String, Metric>,
    pub method: String,
}
impl Report {
    pub fn markdown(&self) -> String {
        let mut out = format!(
            "Machine: {}, {}, {} bytes RAM, {} {}, {}. Corpus: {} / {} notes; {} lifecycle; bytes min/median/p95/max: {}/{}/{}/{}.\n\n| Metric | n | p50 | p95 | Unit | Target | Pass |\n| --- | ---: | ---: | ---: | --- | ---: | --- |\n",
            self.machine.model,
            self.machine.arch,
            self.machine.memory_bytes,
            if cfg!(windows) { "Windows" } else { "macOS" },
            self.machine.os,
            self.machine.rust,
            self.corpus.version,
            self.corpus.notes,
            self.corpus.lifecycle_notes,
            self.corpus.bytes_min,
            self.corpus.bytes_p50,
            self.corpus.bytes_p95,
            self.corpus.bytes_max
        );
        for (name, m) in &self.metrics {
            out.push_str(&format!(
                "| {name} | {} | {:.3} | {:.3} | {} | {} | {} |\n",
                m.n,
                m.p50,
                m.p95,
                m.unit,
                m.target
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "—".into()),
                m.pass
                    .map(|p| if p { "PASS" } else { "FAIL" })
                    .unwrap_or("—")
            ));
        }
        out
    }
}

pub fn timed<T>(f: impl FnOnce() -> Result<T>) -> Result<(T, f64)> {
    let start = Instant::now();
    let value = f()?;
    Ok((value, start.elapsed().as_secs_f64() * 1000.0))
}

pub fn save(vault: &Vault, i: usize) -> Result<()> {
    let result = vault.write(WriteRequest {
        path: format!("bench/save-{i}.md"),
        content: format!("---\ntitle: Save {i}\ntype: note\n---\nSynthetic searchable save {i}. 결제 환불 日本語。\n").into_bytes(),
        expected_checksum: None, op_id: None, ingress: Ingress::Mcp,
    })?;
    ensure!(
        result.index == wi_store::IndexState::Ok,
        "write was not searchable"
    );
    Ok(())
}

pub fn prepare(root: &Path, cache: &Path, count: usize) -> Result<(Vault, Corpus)> {
    let corpus = generate(root, count)?;
    let options = OpenOptions {
        index_path: Some(cache.to_owned()),
    };
    let (vault, _) = Vault::init_with(root, options)?;
    let git = |args: &[&str]| -> Result<()> {
        let result = Command::new("git")
            .args(["-c", "core.autocrlf=false"])
            .arg("-C")
            .arg(root)
            .args(args)
            .output()?;
        ensure!(
            result.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        Ok(())
    };
    git(&["add", "-A"])?;
    git(&[
        "-c",
        "user.name=Benchmark",
        "-c",
        "user.email=bench@example.invalid",
        "commit",
        "-qm",
        "Synthetic corpus",
    ])?;
    vault.reindex()?;
    Ok((vault, corpus))
}

pub fn measure(root: &Path, cache: &Path, count: usize, samples: usize) -> Result<Report> {
    ensure!(samples >= 20, "at least 20 samples required");
    let (mut vault, corpus) = prepare(root, cache, count)?;
    let options = OpenOptions {
        index_path: Some(cache.to_owned()),
    };
    let mut metrics = BTreeMap::new();
    let mut insert = |name: &str, values: Vec<f64>, target: Option<f64>| -> Result<()> {
        metrics.insert(name.into(), Metric::samples(values, "ms", target)?);
        Ok(())
    };
    let mut reindexes = Vec::new();
    for _ in 0..3 {
        reindexes.push(timed(|| Ok(vault.reindex()?))?.1);
    }
    insert("reindex", reindexes, Some(10_000.0))?;
    let mut syncs = Vec::new();
    for _ in 0..20 {
        syncs.push(timed(|| Ok(vault.catch_up()?))?.1);
    }
    insert("stat_catch_up", syncs, None)?;
    // Search before writes: generation-cold and stable-generation are separate.
    let search = |vault: &Vault, i: usize| -> Result<()> {
        let response = vault.search_current(
            &Query::text(WORDS[i % WORDS.len()]),
            &Value::Null,
            SearchOptions { catch_up: false },
        )?;
        ensure!(
            !response.response.hits.is_empty(),
            "search returned no hits"
        );
        ensure!(
            response.response.diagnostics.is_empty(),
            "search diagnostics: {:?}",
            response.response.diagnostics
        );
        Ok(())
    };
    let mut cold = Vec::new();
    for _ in 0..20 {
        let v = Vault::open_with(root, options.clone())?;
        v.index_as_of()?;
        cold.push(timed(|| search(&v, 0))?.1);
    }
    insert("search_current_generation_cold", cold, Some(50.0))?;
    for i in 0..20 {
        search(&vault, i)?;
    }
    let mut searches = Vec::new();
    let mut contexts = Vec::new();
    for i in 0..samples {
        searches.push(timed(|| search(&vault, i))?.1);
        contexts.push(
            timed(|| {
                let id = (i * 47) % (count - 1);
                let found = vault.context(&format!("notes/{:03}/n{id:05}", id % 100), 2)?;
                ensure!(!found.edges.is_empty(), "context has no edges");
                Ok(())
            })?
            .1,
        );
    }
    insert("search_current_warm", searches, Some(50.0))?;
    insert("context_depth_2", contexts, Some(100.0))?;
    for mode in ["standard", "full"] {
        vault.flush()?;
        let config = root.join(".wiki-island/config.toml");
        let text = fs::read_to_string(&config)?;
        let text = text
            .lines()
            .filter(|l| !l.starts_with("durability ="))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(config, format!("{text}\ndurability = \"{mode}\"\n"))?;
        vault = Vault::open_with(root, options.clone())?;
        vault.index_as_of()?;
        for i in 0..10 {
            save(
                &vault,
                (if mode == "full" { 1_000_000 } else { 2_000_000 }) + i,
            )?;
        }
        let mut saves = Vec::new();
        for i in 0..samples {
            saves.push(
                timed(|| {
                    save(
                        &vault,
                        (if mode == "full" { 3_000_000 } else { 4_000_000 }) + i,
                    )
                })?
                .1,
            );
        }
        insert(&format!("save_{mode}"), saves, Some(50.0))?;
        let (report, elapsed) = timed(|| Ok(vault.flush()?))?;
        ensure!(
            report.commits.len() == samples + 10,
            "unexpected flush count"
        );
        insert(
            &format!("flush_per_commit_{mode}"),
            vec![elapsed / report.commits.len() as f64],
            None,
        )?;
        // First read after acknowledged write, with snapshot invalidation.
        search(&vault, 0)?;
        let mut post_write = Vec::new();
        for i in 0..20 {
            save(
                &vault,
                (if mode == "full" { 5_000_000 } else { 6_000_000 }) + i,
            )?;
            post_write.push(timed(|| search(&vault, 0))?.1);
        }
        insert(
            &format!("search_after_write_{mode}"),
            post_write,
            Some(50.0),
        )?;
        vault.flush()?;
    }
    let mut ready = Vec::new();
    for _ in 0..30 {
        ready.push(
            timed(|| {
                let v = Vault::open_with(root, options.clone())?;
                v.index_as_of()?;
                Ok(())
            })?
            .1,
        );
    }
    insert("startup_library", ready, Some(300.0))?;
    // Restore standard for process measurements and subsequent reproduction.
    let config = root.join(".wiki-island/config.toml");
    let text =
        fs::read_to_string(&config)?.replace("durability = \"full\"", "durability = \"standard\"");
    fs::write(config, text)?;
    Ok(Report {
        schema: 1, date: command_text("date", &["-u", "+%F"] )?, commit: command_text("git", &["rev-parse", "HEAD"] )?,
        machine: Machine::detect()?,corpus,metrics,
        method: "release; deterministic mixed-v1; OS cache warm; nearest-rank percentiles; corpus generation/build excluded; 20 search warmups; 10 save warmups per mode; saves exclude flush; context library depth=2; generation-cold search and post-write read reported separately; library ready=open+index without stat walk; process metrics from processes.py".into(),
    })
}

/// Absolute latency noise floor in milliseconds; an unmeasured policy choice.
const ABSOLUTE_FLOOR_MS: f64 = 1.0;

/// Strict comparable-schema gate. Missing/extra metrics and changed machine,
/// corpus or units are errors rather than silent omissions. Compare p95.
pub fn check(baseline: &Report, current: &Report) -> Result<()> {
    ensure!(baseline.schema == current.schema, "schema mismatch");
    ensure!(
        serde_json::to_value(&baseline.machine)? == serde_json::to_value(&current.machine)?,
        "machine mismatch"
    );
    ensure!(baseline.corpus == current.corpus, "corpus mismatch");
    ensure!(
        baseline.metrics.keys().eq(current.metrics.keys()),
        "metric set mismatch"
    );
    let mut failures = Vec::new();
    for (name, old) in &baseline.metrics {
        let new = &current.metrics[name];
        ensure!(
            old.unit == new.unit && old.n == new.n && old.target == new.target,
            "incomparable metric {name}"
        );
        ensure!(
            old.p95.is_finite() && new.p95.is_finite() && old.p95 >= 0.0 && new.p95 >= 0.0,
            "invalid metric {name}"
        );
        if new.p95 > old.p95 * 1.2 && (old.unit != "ms" || new.p95 > old.p95 + ABSOLUTE_FLOOR_MS) {
            failures.push(format!(
                "{name}: {:.3} > {:.3} (baseline {:.3} +20%)",
                new.p95,
                old.p95 * 1.2,
                old.p95
            ));
        }
    }
    if !failures.is_empty() {
        bail!("{}", failures.join("\n"));
    }
    Ok(())
}

/// Combine k comparable runs into one report whose p50/p95/max are the
/// per-metric medians (upper median for even k). Gating on the median of
/// p95s absorbs a single run's scheduling tail without widening the 20%
/// threshold; a real regression shifts most runs and still fails.
pub fn median_report(reports: &[Report]) -> Result<Report> {
    let first = reports
        .first()
        .ok_or_else(|| anyhow::anyhow!("median needs at least one report"))?;
    for other in &reports[1..] {
        ensure!(other.schema == first.schema, "schema mismatch between runs");
        ensure!(
            serde_json::to_value(&other.machine)? == serde_json::to_value(&first.machine)?,
            "machine mismatch between runs"
        );
        ensure!(other.corpus == first.corpus, "corpus mismatch between runs");
        ensure!(
            other.metrics.keys().eq(first.metrics.keys()),
            "metric set mismatch between runs"
        );
    }
    let median = |mut values: Vec<f64>| {
        values.sort_by(f64::total_cmp);
        values[values.len() / 2]
    };
    let mut metrics = BTreeMap::new();
    for (name, metric) in &first.metrics {
        let runs: Vec<&Metric> = reports.iter().map(|r| &r.metrics[name]).collect();
        ensure!(
            runs.iter()
                .all(|m| m.unit == metric.unit && m.n == metric.n && m.target == metric.target),
            "incomparable metric {name} between runs"
        );
        let p95 = median(runs.iter().map(|m| m.p95).collect());
        metrics.insert(
            name.clone(),
            Metric {
                unit: metric.unit.clone(),
                n: metric.n,
                p50: median(runs.iter().map(|m| m.p50).collect()),
                p95,
                max: median(runs.iter().map(|m| m.max).collect()),
                target: metric.target,
                pass: metric.target.map(|t| p95 <= t),
            },
        );
    }
    Ok(Report {
        metrics,
        method: format!(
            "{} Median of {} runs: per-metric median p50/p95/max.",
            first.method,
            reports.len()
        ),
        ..first.clone()
    })
}

pub fn write_report(report: &Report, output: &Path) -> Result<()> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(output, serde_json::to_string_pretty(report)?)?;
    fs::write(output.with_extension("md"), report.markdown())?;
    Ok(())
}

pub fn read_report(path: &Path) -> Result<Report> {
    serde_json::from_slice(&fs::read(path)?)
        .with_context(|| format!("read report {}", path.display()))
}

/// A separate ready vault keeps process startup/RSS on the exact initial
/// corpus instead of the extra notes created by library save samples.
pub fn prepare_process_corpus(root: &Path, cache: &Path, count: usize) -> Result<Corpus> {
    let staging = cache.join("staging.sqlite3");
    ensure!(!staging.exists(), "refusing to reuse staging cache");
    let (vault, corpus) = prepare(root, &staging, count)?;
    let target = cache.join(vault.vault_id()).join("index.sqlite3");
    drop(vault);
    fs::create_dir_all(
        target
            .parent()
            .ok_or_else(|| anyhow::anyhow!("cache parent"))?,
    )?;
    fs::rename(staging, target)?;
    Ok(corpus)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn baseline() -> Report {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/bench/baseline.json"
        );
        read_report(Path::new(path)).unwrap()
    }
    fn scaled(base: &Report, metric: &str, factor: f64) -> Report {
        let mut report = base.clone();
        let m = report.metrics.get_mut(metric).unwrap();
        m.p95 *= factor;
        report
    }
    #[test]
    fn median_gate_absorbs_one_tail_and_fails_a_shifted_regression() {
        let base = baseline();
        // One run with a 3x scheduling tail: median is unaffected.
        let runs = [
            base.clone(),
            scaled(&base, "save_standard", 3.0),
            base.clone(),
        ];
        check(&base, &median_report(&runs).unwrap()).unwrap();
        // A large shift in two of three runs exceeds both thresholds.
        let shifted = scaled(&base, "save_standard", 3.0);
        let runs = [shifted.clone(), base.clone(), shifted];
        let error = check(&base, &median_report(&runs).unwrap()).unwrap_err();
        assert!(error.to_string().contains("save_standard"), "{error}");
        // Runs on different corpora are never combined.
        let mut other = base.clone();
        other.corpus.notes += 1;
        assert!(median_report(&[base, other]).is_err());
    }
    #[test]
    fn process_corpus_has_a_ready_per_device_cache_without_save_growth() {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path().join("vault");
        let cache = scratch.path().join("cache");
        let corpus = prepare_process_corpus(&root, &cache, 100).unwrap();
        assert_eq!(corpus.notes, 100);
        let metadata = Vault::open_with(
            &root,
            OpenOptions {
                index_path: Some(cache.join("unused.sqlite3")),
            },
        )
        .unwrap();
        let path = cache.join(metadata.vault_id()).join("index.sqlite3");
        drop(metadata);
        assert!(path.is_file());
        assert!(!cache.join("staging.sqlite3").exists());
        assert!(!cache.join("unused.sqlite3").exists());
        let vault = Vault::open_with(
            &root,
            OpenOptions {
                index_path: Some(path),
            },
        )
        .unwrap();
        assert!(vault.index_as_of().unwrap().is_some());
        assert!(
            !vault
                .context("notes/000/n00000", 2)
                .unwrap()
                .edges
                .is_empty()
        );
        assert!(!root.join("bench").exists());
    }
    #[test]
    fn generated_memory_is_deliverable_with_attached_evidence() {
        let scratch = tempfile::tempdir().unwrap();
        let (vault, _) = prepare(
            &scratch.path().join("vault"),
            &scratch.path().join("index.sqlite3"),
            100,
        )
        .unwrap();
        let path = "notes/000/n00000.md";
        let bytes = fs::read(vault.root().join(path)).unwrap();
        let labels = vault
            .classify(&[(path.into(), bytes)], &Value::Null)
            .unwrap();
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].eligibility, "current", "{:?}", labels[0]);
        assert!(!labels[0].spans.is_empty());
    }
    #[test]
    fn regression_gate_rejects_regression_missing_metric_and_mismatched_corpus() {
        let mut a = Report {
            schema: 1,
            date: "test".into(),
            commit: "test".into(),
            machine: Machine {
                model: "test".into(),
                arch: "test".into(),
                memory_bytes: "1".into(),
                os: "test".into(),
                rust: "test".into(),
            },
            corpus: Corpus {
                version: "test".into(),
                notes: 10,
                lifecycle_notes: 3,
                tree_sha256: "test".into(),
                bytes_min: 1,
                bytes_p50: 1,
                bytes_p95: 1,
                bytes_max: 1,
                bytes_total: 10,
            },
            metrics: BTreeMap::from([(
                "x".into(),
                Metric::samples(vec![10.0], "ms", None).unwrap(),
            )]),
            method: "test".into(),
        };
        assert!(check(&a, &a).is_ok());
        let mut b = a.clone();
        a.metrics.get_mut("x").unwrap().p95 = 1.044;
        b.metrics.get_mut("x").unwrap().p95 = 1.421;
        assert!(check(&a, &b).is_ok());
        a.metrics.get_mut("x").unwrap().p95 = 10.0;
        b.metrics.get_mut("x").unwrap().p95 = 12.5;
        assert!(check(&a, &b).is_err());
        b.metrics.get_mut("x").unwrap().p95 = 11.5;
        assert!(check(&a, &b).is_ok());
        b.metrics.get_mut("x").unwrap().p95 = 12.0;
        assert!(check(&a, &b).is_ok());
        b.metrics.get_mut("x").unwrap().p95 = 12.01;
        assert!(check(&a, &b).is_err());
        b = a.clone();
        b.metrics.clear();
        assert!(check(&a, &b).is_err());
        b = a.clone();
        b.corpus.notes = 11;
        assert!(check(&a, &b).is_err());
        a.metrics.get_mut("x").unwrap().p95 = f64::NAN;
        assert!(check(&a, &a).is_err());
    }
    #[test]
    fn corpus_is_deterministic_and_has_expected_lifecycle_distribution() {
        let dir = tempfile::tempdir().unwrap();
        let a = generate(&dir.path().join("a"), 1000).unwrap();
        let b = generate(&dir.path().join("b"), 1000).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.lifecycle_notes, 300);
        assert!(a.bytes_max > a.bytes_min * 10);
        assert!(generate(&dir.path().join("a"), 1000).is_err());
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    #[test]
    fn unavailable_machine_probes_are_explicitly_unknown() {
        let machine = super::Machine::detect().unwrap();
        assert_eq!(machine.model, "unknown");
        assert_eq!(machine.arch, "unknown");
        assert_eq!(machine.memory_bytes, "unknown");
        assert_eq!(machine.os, "unknown");
        assert!(machine.rust.starts_with("rustc "));
    }
}
