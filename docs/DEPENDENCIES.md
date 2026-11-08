# Dependencies

Versions are manifest ranges; `Cargo.lock` records exact resolved versions. Capture, search, reads and MCP use local runtime code only. Only `wi sync`, `wi publish` and their background threads use the network, outside engine locks; crate downloads are build-time only. Each crate declares its own dependencies. A new dependency needs a written reason here: what it does, what it costs, what the alternative was. Release builds use thin LTO, one codegen unit and symbol stripping; panic behavior stays Rust's default because a workspace-wide abort would also affect libraries.

## wi-sys

Windows only; the crate is empty on other platforms. It holds all `unsafe` code of the workspace. `wi-store`, `wi-history`, `wi-docs`, `wi-cli` and `wi-notion` depend on it under `[target.'cfg(windows)'.dependencies]`.

| Crate | Range | Reason and alternative |
| --- | --- | --- |
| `windows-sys` | 0.61 (Windows only; features for NT file calls, file system, job objects, thread snapshot, credentials, console, security) | Raw bindings for calls std does not expose: `NtCreateFile` and `NtSetInformationFile` for handle-relative open and rename, POSIX delete and rename semantics, job objects, Credential Manager and console mode. Bindings only, with no runtime and no extra cost; it was already in `Cargo.lock` through other crates. The `windows` crate is heavier, and std has no handle-relative APIs. |
| `tempfile` (dev, Windows only) | 3 | Isolated directories for the Windows tests. |

## wi-note

| Crate | Range | Reason and alternative |
| --- | --- | --- |
| `yaml-rust2` | 0.10 | YAML values plus marked parser events for key spans, so edits patch only the touched key. No emitter is used. Chosen over `saphyr` for its small event API. |
| `serde`, `serde_json` | 1 (`float_roundtrip`) | Typed lifecycle projections, a finite typed value model, quoted-scalar encoding and CLI JSON. A custom value tree would complicate downstream APIs. |
| `thiserror` | 2 | Typed library errors. |
| `toml_edit` | 0.23 | Preserve unknown config keys, comments and formatting; plain TOML deserialization loses them. |
| `pulldown-cmark` | 0.13 | CommonMark inline ranges, nested lists and code exclusion; a hand-written fence scanner misses delimiter edge cases. |
| `regex` | 1 | The observation category and timestamp grammar; fixed patterns are initialized once. |
| `unicode-normalization` | 0.1 | NFC normalization; without it canonical-equivalent names would split. |
| `unidecode` | 0.3 | ASCII transliteration for permalinks, including Korean; stripping non-ASCII would lose addresses. Small overrides cover kana, Ukrainian and fractions; coverage is not exhaustive. |
| `mime_guess` | 2 | Recognize real extensions while keeping version-number periods (`Version 2.0.0`). |
| `proptest` (dev) | 1 | Arbitrary-body identity and nested set/get invariants. |

## wi-history

| Crate | Range | Reason and alternative |
| --- | --- | --- |
| `gix` | 0.90 (no default features; `sha1`, `index`) | Pure-Rust git: blob writes, tree editing, commits, compare-and-swap ref edits, revision walks and index writes, with no network or C toolchain. `git2` and shelling out to git were the alternatives. |
| `sha2` | 0.10 | `sha256:` content hashes and manifest frame checksums. |
| `rustix` | 1 (`fs`; Unix only) | fsync and `F_FULLFSYNC` barriers for the durability modes; std `sync_all` always issues `F_FULLFSYNC` on macOS. |
| `toml_edit` | 0.23 | Read the config key `durability` without depending on `wi-note`. |
| `serde`, `serde_json`, `thiserror` | 1, 1, 2 | JSON manifest records and typed errors. A binary codec would be harder to inspect and quarantine by hand. |
| `tempfile` (dev) | 3 | Isolated temporary vaults. |

## wi-index

| Crate | Range | Reason and alternative |
| --- | --- | --- |
| `rusqlite` | 0.37 (`bundled`) | SQLite FTS5 and JSON with a reproducible build; system SQLite features vary. |
| `rayon` | 1 | Parallel parsing of changed notes in bounded batches. |
| `ignore` | 0.4 | Nested gitignore patterns and a symlink-free walk; global and parent ignore rules are disabled. |
| `dirs` | 6 | Platform cache directory. |
| `chrono` | 0.4 (`clock`, no default features) | RFC 3339 UTC watermark. |
| `unicode-general-category` | 1 | Attach combining marks to script units; ranges alone split Thai and variation-selector sequences. |
| `memchr` | 2 | Fast literal substring scan in the fallback. |
| `sha2`, `serde`, `serde_json`, `thiserror`, `unicode-normalization` | as above | Hashes, result model, errors, NFKC script analysis. |
| `tempfile`, `criterion` (dev) | 3, 0.7 | Isolated fixtures; repeatable microbenchmarks. |

## wi-lifecycle

Depends on `wi-note` only and does no I/O. `chrono` 0.4 (`std`, `serde`; RFC 3339 and ISO 8601 duration arithmetic), `regex` 1 (capture-filter patterns), `sha2`, `serde`/`serde_json`, `thiserror`, and `proptest` (dev) for cap arithmetic properties.

## wi-store

Reuses `wi-note`, `wi-history`, `wi-index`, `wi-lifecycle`, `serde`, `serde_json`, `sha2`, `thiserror`, `toml_edit`, `ignore`, `chrono`, `tempfile` and `criterion` (dev) for the reasons above.

| Crate | Range | Reason and alternative |
| --- | --- | --- |
| `rustix` | 1 (`fs`, `process`; Unix only) | Atomic exchange and no-replace renames (`renameat_with`) and descriptor-relative `openat`/`renameat`/`unlinkat` without `unsafe`; `process` checks whether a crashed writer's pid is gone. A raw `libc` call would need an `unsafe` module. |
| `notify-debouncer-mini` | 0.7 (re-exports `notify` 8) | File events (FSEvents, inotify) with debouncing; polling would cost a stat walk per interval. |
| `uuid` | 1 (`v4`) | The config `vault_id`. |
| `unicode-normalization` | 0.1 | NFC folding for path identity. |

## wi-docs

Reuses `wi-note`, `wi-store`, `wi-history`, `serde`, `serde_json`, `thiserror` and `chrono`. `rusqlite` is test-only.

| Crate | Version | Reason and alternative |
| --- | --- | --- |
| `lopdf` | =0.38.0, default features off | Native per-page PDF text, CMap decoding and geometry without a system executable. Not a layout or OCR engine. |
| `zip` | =4.6.1, deflate only | Read DOCX/PPTX members and Notion zips without unpacking to disk. |
| `quick-xml` | =0.38.4 | Streaming paragraph and slide text; a DOM retains more memory. |
| `csv` | =1.4.0 | Correct quoted fields and Tesseract TSV parsing. |
| `imagesize` | =0.14.0 | Image dimensions without decoding pixels. |
| `uuid` | 1 (v5, serde) | Deterministic document and run identities without a file-entity database. |
| `tempfile` | 3 | Private OCR input snapshots, so a concurrently changing source cannot affect OCR. |

Extractor dependencies are pinned because their behavior contributes to run identity. A dependency or normalization change must update the extractor version or pipeline identity before existing runs are reused. Tesseract and poppler are optional preinstalled executables, not dependencies; no model or network is involved.

## wi-notion

Reuses `wi-note` for canonical note parsing, `wi-history` for atomic durable map
replacement, `pulldown-cmark` for structured Markdown mapping, `serde` and
`serde_json` for blocks and device-local state, `sha2` for publication hashes,
`thiserror` for sanitized errors, and `tempfile` (dev) for isolated fixtures.

| Crate | Range | Reason and alternative |
| --- | --- | --- |
| `ureq` | 3 (defaults off; `rustls`, `json`) | Synchronous HTTPS requests for the optional publisher, with timeouts and redirects disabled. Adds HTTP parsing, root certificates and rustls TLS dependencies; no async runtime. An async HTTP client would add runtime coupling to a thread-based caller; a hand-written TLS client is not suitable. |
| `rustls` (transitive) | 0.23 | Certificate-verified TLS via ureq, without a platform TLS binding. The alternative is native TLS with platform-specific dependencies. |

## wi-export

Uses `wi-store` for all vault writes and eligibility, `wi-note`, `wi-lifecycle` and `wi-history` (host-config durability helpers), plus `serde`, `serde_json`, `thiserror`, `toml_edit` (preserve Codex config comments and unrelated keys), `percent-encoding` 2 (stable resource URIs for Unicode skill ids) and `tempfile` (private, collision-free replacement of host configs).

## wi-mcp

| Crate | Range | Reason and alternative |
| --- | --- | --- |
| `rmcp` | 3.8 (server, transport-io; defaults off) | The official Rust MCP SDK: stdio framing, initialization, dispatch. A custom JSON-RPC server would duplicate protocol and cancellation handling. |
| `tokio` | 1 (rt-multi-thread, sync, io-std, macros) | rmcp's asynchronous stdio. The git-backed vault stays on a dedicated OS thread. |
| `ignore`, `globset` | 0.4 | Symlink-free gitignore-aware listings and basename globs. |
| `pulldown-cmark` | 0.13 | Heading paths and byte ranges excluding code fences, including Setext headings. |
| `chrono` | 0.4 | Context `since` dates. |
| `serde_json`, `thiserror`, `tempfile` (dev) | | Schemas, bounded results, errors, fixtures. |

`rmcp` transitively enables tracing, schemars and futures. Logs are stderr-only.

## wi-cli

Workspace crates `wi-notion` (Notion view) and `wi-history` (durable publish state); `clap` 4 (derive, `env`; subcommands, help, `WI_VAULT`), `anyhow` 1 (error context at the binary boundary; libraries stay typed), `serde_json`, `tokio` for the stdio entry point, and `tempfile` (dev) for CLI and crash tests.

## wi-bench

A non-published, opt-in package. It reuses `wi-store`, `wi-index`, `wi-history`, `anyhow`, `clap`, `serde`, `serde_json`, `tempfile` and `criterion`. Two optional crates sit behind the `korean` feature and never enter the production binary:

| Crate | Pinned | Reason and cost |
| --- | --- | --- |
| `tantivy` | 0.25.0 | A separate BM25 experiment, so production FTS5 is compared with an alternative. |
| `lindera` | 0.43.3 (ko-dic) | Korean morphological segmentation; adds a dictionary download at build time and a large experimental binary. |

`Cargo.lock` records their transitive packages; their presence does not enable them in `wi`.
