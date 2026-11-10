# Performance targets and measurement

Performance is an owner priority and speed work is driven by measurement. Targets are policy. A number is evidence only when it names the reference machine, the corpus and the method. This file holds targets and how to measure; results are produced by `wi-bench` and are not recorded here.

## Targets

Reference machine: Apple Silicon Mac, release build.

| Area | Target (p95 unless noted) |
| --- | --- |
| Single-note save, tool call to searchable (file and index; git commit excluded) | ≤ 50 ms, standard and full durability |
| Search on 10,000 notes (single word and ranked multi-term, including Korean) | ≤ 50 ms |
| Current search on a fresh handle (generation-cold) | ≤ 50 ms |
| Context traversal on 10,000 notes | ≤ 100 ms |
| Full reindex of 10,000 notes | ≤ 10 s |
| Startup to ready, CLI and MCP server, existing index, no full-vault scan | ≤ 300 ms |
| Idle memory of the MCP server | ≤ 100 MB RSS |
| Release binary | ≤ 30 MB |
| One-shot command catch-up with nothing changed (no live watcher) | unmeasured target; measured by `cli_query_with_catch_up` |
| Idle maintenance cost | one generation check; a pass with nothing to do writes and commits nothing |

Corpus rule: the corpus is real or realistically generated, mixes Korean and English, and its size and note-length distribution are published with the result. Lifecycle constants (`stale_after_days`, caps, byte and hit limits) are unmeasured policy choices.

## Optimization rules

- **Incremental work.** Store mtime, size and content hash per file. Startup and sync compare stat data first and hash only on change. Unchanged files are never re-parsed.
- **Parallelism.** Reindex and import parse in a work-stealing pool. The index is written in batched transactions.
- **SQLite.** WAL mode, cached prepared statements, one writer and read connections; pragmas are tuned with evidence.
- **Git off the hot path.** A write returns after file, blob, manifest records and index. Commits are coalesced and flushed in batches; the queue flushes on exit. An edit is durable in the file before it has history ([OPERATIONS](OPERATIONS.md#git-history-and-sync)).
- **Consistency without a watcher.** "Ready" means usable with no vault scan. A one-shot CLI query then runs a stat-walk catch-up, hashing only files whose stat changed. With a live watcher lease the catch-up is skipped. Results carry `index_as_of`, and a current read re-hashes its returned hits (at most 8).
- **Index-resident snapshots.** Lifecycle note snapshots live in the index, so a cold current search does not re-parse every lifecycle note ([DESIGN](DESIGN.md#index-and-search)).
- **Bounded output.** Results stay within byte caps, and a read a caller aims at a file is bounded by `max_note_bytes` (8 MiB by default) and refused above it without loading the file ([PROTOCOL](PROTOCOL.md#read-limit-and-query-cap)). Text and title queries are capped at 2048 bytes and 64 terms, and the parser decides once per query whether it has several terms. Write-path, recovery, ingest, import, publish and maintenance reads still load whole files. The caps are unmeasured policy choices. `tools/list` stays small: the shared result envelope is not repeated on every tool.
- **Maintenance is time-sliced.** A pass runs on the watcher thread with a time budget and resumes on the next tick ([OPERATIONS](OPERATIONS.md#maintenance)). An index rebuild in that pass is sliced too, 128 files per chunk; the last slice's stat compare and link resolution are not divided. Slice durations are unmeasured.
- **Lazy heavy parts.** OCR, document parsers and the Tantivy experiment sit behind optional features or binaries and never load on the capture or search path.

## How to measure

`wi-bench` is opt-in and macOS only. Scratch paths must be new and outside the repository. Full commands are in [crates/wi-bench/README.md](../crates/wi-bench/README.md).

```sh
cargo build --release -p wi-bench -p wiki-island --bin wi-bench --bin wi
target/release/wi-bench run --root /tmp/wi-measure/vault --cache /tmp/wi-measure/index.sqlite3 \
  --notes 10000 --output /tmp/wi-measure/report.json
```

A report records the machine, toolchain, corpus fingerprint and distribution, sample counts and per-metric p50/p95/max against the targets above. `cargo bench -p wi-bench --bench paths` runs criterion diagnostics.

## Regression gate

`wi-bench check --baseline docs/bench/baseline.json --runs 3 --wi target/release/wi` takes three complete fresh 10k runs, uses the per-metric median, and fails when:

- a latency metric (unit `ms`) has a median p95 above both 120% of the baseline and the baseline plus 1.0 ms;
- a size or RSS metric exceeds 120% of the baseline;
- the schema, machine, corpus, metric set, sample count, unit or target differs (an intentional rebaseline after review is required).

`docs/bench/baseline.json` is the median of three runs of a reference revision; rebuild it with `wi-bench median`. The 20% threshold and the 1.0 ms floor are unmeasured policy choices. Investigate a failure instead of rerunning until green. Run the gate on a quiet machine: contention inflates tails.

## Korean retrieval

`wi-bench quality` (feature `korean`) evaluates retrieval on a public Korean reading-comprehension set, downloaded separately; only aggregates may be committed. It compares production FTS5 (all-terms first, then ranked OR) with an optional Tantivy and morphological-analyzer configuration using Recall@1/5/10 and MRR@10, and reports short derived probes separately because their relevance labels are incomplete. The configurations differ in more than the tokenizer (title weights, conjunction, ranking), so the result is not an isolated tokenizer comparison. FTS5 stays the production default; a change of default requires a new measurement and a new decision. Commands and data handling: [wi-bench](../crates/wi-bench/README.md#korean-quality-local-data-only).
