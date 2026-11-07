# wi-bench

Opt-in measurement tool, not published and not a dependency of any production crate. It generates a deterministic multilingual corpus (`mixed-v1`: Korean, English and Japanese notes with observations, relations and lifecycle claims), times the real write, search, context, reindex, startup and MCP paths, and compares a run with a baseline. It runs on macOS (machine detection reads `sysctl` and `sw_vers`); scratch paths must be new and outside the repository. Targets and rules: [PERFORMANCE](../../docs/PERFORMANCE.md).

```sh
cargo build --release -p wi-bench -p wiki-island --bin wi-bench --bin wi
target/release/wi-bench run --root /tmp/wi-measure/vault --cache /tmp/wi-measure/index.sqlite3 \
  --notes 10000 --output /tmp/wi-measure/report.json     # 1k or 10k notes
cargo bench -p wi-bench --bench paths                     # criterion diagnostics
```

`run` writes a JSON report and a Markdown table with the machine, corpus fingerprint and length distribution, and per-metric p50, p95 and max. Process-level metrics (CLI and MCP startup, idle RSS, MCP tool round trips) need an exact initial corpus and a helper:

```sh
target/release/wi-bench prepare --root /tmp/wi-process/vault --cache /tmp/wi-process/cache --notes 10000
python3 crates/wi-bench/processes.py --wi target/release/wi \
  --root /tmp/wi-process/vault --cache /tmp/wi-process/cache --report /tmp/wi-measure/report.json
```

## Regression gate

```sh
# Three complete fresh 10k runs, compared with the checked-in baseline (median per metric).
target/release/wi-bench check --baseline docs/bench/baseline.json --runs 3 --wi target/release/wi
# Compare saved reports without rerunning.
target/release/wi-bench check --baseline docs/bench/baseline.json \
  --current /tmp/a.json --current /tmp/b.json --current /tmp/c.json
# Build a baseline from several runs.
target/release/wi-bench median --input /tmp/a.json --input /tmp/b.json --input /tmp/c.json \
  --output docs/bench/baseline.json
```

A latency metric regresses only when its median p95 exceeds both 120% of the baseline and the baseline plus 1.0 ms; size and RSS metrics regress above 120%. Schema, machine, corpus, metric set, sample count, unit and target must match, otherwise the gate fails and an intentional rebaseline is required. Investigate a failure instead of rerunning until green. A `PATH` without `~/.cargo/bin` makes `wi-bench` fail at machine detection.

## Korean quality (local data only)

```sh
curl -Lf https://korquad.github.io/dataset/KorQuAD_v1.0_dev.json -o /tmp/KorQuAD_v1.0_dev.json
cargo build --release -p wi-bench --features korean
target/release/wi-bench quality --dataset /tmp/KorQuAD_v1.0_dev.json --output /tmp/korean-quality.json
```

The KorQuAD 1.0 dev set is [CC BY-ND 2.0 KR](https://korquad.github.io/category/1.0_ENG.html). The data and transformed notes stay in temporary directories; commit only aggregate results, never passages, questions, answers or per-query extracts. The source file's SHA-256 is enforced before evaluation. The evaluator uses every dev question with its source passage as the single gold, and reports Recall@1/5/10 and MRR@10 for production FTS5, an optional Tantivy configuration with a Korean morphological analyzer, and a literal-scan baseline. Auxiliary one and two-syllable probes are derived, have incomplete relevance labels and are reported separately. This compares complete retrieval configurations, not isolated tokenizers.
