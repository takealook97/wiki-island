# Design

"Must" states the contract the code follows. A command or tool named in these docs but absent from `wi --help` or the MCP tool list is not implemented. Lifecycle constants (`stale_after_days`, caps, byte and hit limits) are unmeasured policy choices; performance targets are in [PERFORMANCE](PERFORMANCE.md).

## Product

- wiki-island is a personal knowledge and memory engine for one owner, used across that owner's devices and AI tools.
- It ships as one Rust program installed under two binary names, `wi` and `wiki-island`, with a CLI and an MCP stdio server ([PROTOCOL](PROTOCOL.md)).
- There is no UI, dashboard, web app or mobile app. Notes are plain files, so Obsidian or any editor is optional and works as is ([INTEGRATIONS](INTEGRATIONS.md#obsidian-and-other-editors)).
- Private, personal use. Licensed under AGPL-3.0-only.
- On top of notes and index the engine adds the lifecycle in [KNOWLEDGE](KNOWLEDGE.md): status, freshness, applicability, source pins, adoption gates and work items, with no human approval step.
- The engine never calls a model. Network policy: capture, search, reads, maintenance and every MCP tool never use the network; only the background sync and publish threads, the explicit `wi sync` and `wi publish` commands, and the explicit `wi setup --github`, which asks the hosting service for the account name and finds or creates the private repository, do, and never while an engine lock is held. `wi sync setup <url>` only stores the remote address locally. It maintains itself ([KNOWLEDGE](KNOWLEDGE.md#self-maintenance)) and a non-developer installs it with one command ([INTEGRATIONS](INTEGRATIONS.md#setup-and-uninstall)).

## Scope

In scope:

- note format and parsing: observations, relations, wikilinks ([FORMAT](FORMAT.md#note-syntax))
- permalinks and identity
- search: text, title, permalink, metadata filters
- context traversal, list and find
- document import with sidecars and page maps ([FORMAT](FORMAT.md#document-sidecars)) and Notion export import
- procedures exported as host skills and served over MCP ([SKILLS](SKILLS.md))
- optional git remote synchronization in git mode ([OPERATIONS](OPERATIONS.md#git-history-and-sync))
- an optional one-way, read-only Notion view of committed notes ([INTEGRATIONS](INTEGRATIONS.md#notion-view))

Three ways to use it: an Obsidian or file-synced folder as the canonical store (folder mode); a git working tree synced to a private remote, with Obsidian as a view (git mode); Notion as a read-only view on top of either.

Out of scope: cloud or hosted adapters beyond git remote transport and the Notion view, an HTTP API, and embeddings. Embeddings may return later as an opt-in feature, never on the capture or search path.

Structure:

- Writes are file-first: the file is written before any database state ([OPERATIONS](OPERATIONS.md#write-path)).
- Tool calls go straight to services, with no HTTP hop.
- One SQLite index per device, with no migrations: the index is rebuilt instead.
- Every MCP tool uses the `wi_` prefix. Note files are plain Markdown with frontmatter; lifecycle fields are extra keys, and unknown keys are preserved.

## Storage and history

- The vault is one directory of Markdown notes and attachments; they are the canonical content. History is a git repository in one of two storage modes:
  - **git mode**: history in `<vault>/.git` (a vault outside file-synced folders).
  - **folder mode**: history in an external git directory `<data_dir>/wiki-island/vaults/<vault_id>/git` whose `core.worktree` names the vault, so the folder holds only notes and `.wiki-island/`. Used for a vault inside a file-synced folder (iCloud Drive, Obsidian Sync, Dropbox and similar). History and per-device state are then private to each device.
- On open, `<vault>/.git` means git mode; otherwise an external git directory registered for the vault's `vault_id` means folder mode. `wi init` and `wi setup` choose folder mode inside a known file-synced folder and git mode elsewhere. A folder-mode vault moved on the same device is re-bound under a lock and reported by `wi doctor`; a second copy with the same `vault_id` is refused, never silently taken over.
- The current state is the durable working-tree file, identified by its content hash. Git holds coalesced history: engine writes are flushed in batches, one commit per acknowledged operation; the watcher commits external editor changes after a debounce; `wi commit` commits by hand. A revert is a new commit ([OPERATIONS](OPERATIONS.md#git-history-and-sync)).
- Until its commit, every acknowledged engine save is kept as a git blob named in a fsynced pending manifest. Intermediate external-editor saves the engine never saw are not preserved.
- Local git history is embedded (pure Rust) and needs no git binary, daemon or network. Optional remote transport uses the system git.
- Git-mode remote sync flushes, fetches, applies fast-forwards or a per-path three-way merge, and pushes without force. Conflicts keep the local path and preserve remote bytes in a committed conflict copy with a work request; no conflict markers are written. Folder mode leaves transport to the file-sync service.
- **Never put a git directory inside an iCloud Drive or Google Drive sync folder.** Those services sync files one by one and can deliver git's objects and refs partly or out of order; the likely result is a corrupt repository. Folder mode keeps history out of the synced folder; `wi doctor` warns when the history folder itself looks synced (detection is heuristic).
- In folder mode only the primary device (`primary_device` in `.wiki-island/config.toml`, a random per-device id kept in the data directory) runs lifecycle-mutating maintenance; the first setup records it and `wi setup --primary` takes over. Other devices still index, search, serve MCP, capture, ingest owner edits and keep their own history.
- Work notes live under `.wiki-island/work/<device_id>/`; listings mark other devices' items read-only. Flat engine work notes move through the crash-safe write path on open.
- No path (capture, `wi write`, import, ingest) adds a file larger than 50 MB to history: it stays in the folder, gets a managed exclude rule and one work request ([OPERATIONS](OPERATIONS.md#git-history-and-sync)).
- Originals stay in the repository by default; `wi doctor` advises considering Git LFS above an unmeasured 20 MiB threshold. No automatic LFS setup ([OPERATIONS](OPERATIONS.md#attachments)).

## Write path

Every write resolves its target, checks the operation id and the caller's expected checksum, writes and fsyncs a temp file in the same directory, makes the new bytes a durable git blob, appends an `intent` record to the pending manifest, swaps the file into place, appends a `done` record (the acknowledgment), updates the index and enqueues a commit. Frontmatter edits change only the touched keys. There is no delete: notes are archived, and external deletions are recorded as `missing`. Details: [OPERATIONS](OPERATIONS.md#write-path). The index update happens after the lock is released, so a commit or index failure never undoes an acknowledged file.

## Index and search

- The index is SQLite FTS5, one per device, in the platform cache directory outside the vault. It is never synced.
- Tokenizer: `unicode61` plus a script-bigram column for scripts without spaces, so short Korean queries such as 결제 and 환불 match. This is a mechanism, not a claim about retrieval quality; see [PERFORMANCE](PERFORMANCE.md#korean-retrieval).
- Query policy: a plain multi-term text query first runs the strict all-terms expression, then fills the remaining result slots with a ranked OR over every word term and script n-gram, ordered by the same bm25 column weights. All-terms hits always rank first; no stopword list is used. Single-term queries, quoted phrases and explicit `AND`/`OR`/`NOT` stay strict.
- Deleting the index loses nothing; it rebuilds itself when its schema version changes or it is damaged, and `wi reindex` rebuilds it on demand. Maintenance on a time budget rebuilds it in resumable chunks and records the complete walk only after the last one ([OPERATIONS](OPERATIONS.md#index-rebuild-and-fallback)).
- The index also holds, for every lifecycle note (`type` memory, capture or work), the parsed snapshot of the indexed bytes, written in the same transaction as the note's row. A read uses it only when the file's `(mtime_ns, size)` still equals the indexed stamp and the hash and format tag match; otherwise it re-reads the file. The hit itself is always re-read and judged from its current bytes. The row is a cache of a pure function of the file bytes.
- A literal scan fallback answers when the index is missing or corrupt. Its hits are labelled `fallback`.
- Search sits behind an internal interface, so the text engine can change. FTS5 stays the default; an optional Tantivy path exists only as a measurement experiment ([PERFORMANCE](PERFORMANCE.md#korean-retrieval)).
- Capture and keyword search need no model, network or daemon. The watcher runs inside the MCP server, `wi watch` or the optional service. Without a watcher, a CLI query first runs a stat-walk catch-up ([OPERATIONS](OPERATIONS.md#watcher)).

### Index library

`Index::open` puts its SQLite file at the platform cache directory plus `wiki-island/<vault_id>/index.sqlite3`. Tables: `files` (mtime, size, SHA-256), `notes` (body and metadata JSON), `observations`, `relations`, `index_meta`, and FTS5 title/body/script-gram/permalink columns. Prefixes are 1–4 characters; title bm25 weight is 5, other columns 1. Full sync parses in parallel in batches capped at 128 files / 32 MiB; unchanged stat pairs are neither hashed nor parsed. WAL and `synchronous=NORMAL` apply only to the disposable cache. A schema-version change resets the index.

Per-file failures (unsupported names, invalid UTF-8, unreadable or vanished files, oversized notes) yield `skipped` paths and diagnostics while completed files commit. Old rows for skipped paths remain, explicitly stale; `SearchResult::is_skipped` flags such hits. Malformed frontmatter is searchable plain text without authoritative metadata. `max_note_bytes` is a vault config key with an unmeasured 8 MiB default; it also bounds a read a caller aims at a file ([PROTOCOL](PROTOCOL.md#read-limit-and-query-cap)). A text or title query is capped at `MAX_QUERY_BYTES` (2048 bytes) and `MAX_QUERY_TERMS` (64 whitespace-separated terms), both unmeasured policy choices; an over-cap query is a distinct error (`QueryTooLong`), never the literal fallback. Query punctuation is escaped; malformed quotes or operators select the labelled literal fallback. Context completes a deterministic bounded BFS before applying its edge limit. Same-size edits with preserved mtime escape the ordinary stat walk and are found by targeted updates, a rebuild or `reverify()`. The API and query grammar are documented in the crate (`cargo doc -p wi-index`); see [wi-index](../crates/wi-index/README.md).

## Constraints that stay

Private by default; English docs, code, comments and commits; Markdown and attachments are canonical; the index is disposable; capture and keyword search need no daemon, network or model; no automatic deletion, and age is not falsity; status, freshness and effective time stay separate; no custom UI; folder names are not access control; single owner. Rules and preferences follow the origin rule, not an approval step ([AUTHORITY](AUTHORITY.md)). Full list: [AGENTS](../AGENTS.md#philosophy).

## Portability boundary

Unix and Windows (x86_64, NTFS) are supported. Other targets fail with `unsupported-platform` before any filesystem write, so containment and durability guarantees are never weakened.

### Windows

Windows has no `openat`, so `wi-store` has a second containment backend on top of `wi_sys::fs::Dir`, a directory handle that opens, creates, renames and deletes one component at a time relative to itself, refusing reparse points. The write path, recovery and ingress use the same API on both platforms. Windows adds name rules (reserved device names, `:`, trailing dot or space, 8.3 aliases), `\` input handling, a placement that re-hashes then replaces instead of exchanging, file barriers by `FlushFileBuffers` and no directory barrier, child processes in kill-on-close Job Objects, per-user data in `%LOCALAPPDATA%` and the Notion token in Credential Manager. The background service is a per-user Task Scheduler logon task. The mechanisms and every behavior weaker than on Unix are in [OPERATIONS](OPERATIONS.md#windows).

## Crate map

Twelve crates. A crate depends only on crates listed before it:

0. `wi-sys` (Windows system calls: handle-relative file operations, job objects, Credential Manager, console echo) depends on no workspace crate and is empty on other platforms. It is the only crate with `unsafe` code; every other crate forbids it. Crates use it only on Windows.
1. `wi-note` (note model, pure) depends on no workspace crate; `wi-history` (git history, manifest, flush, replay, durability barriers) depends on no workspace crate other than `wi-sys` on Windows.
2. `wi-lifecycle` (gates, eligibility, caps; pure) depends on `wi-note`.
3. `wi-index` (SQLite index, literal fallback) depends on `wi-note` and `wi-lifecycle`.
4. `wi-store` (write path, watcher, recovery, maintenance, lifecycle wiring) depends on `wi-note`, `wi-history`, `wi-index` and `wi-lifecycle`, and on `wi-sys` on Windows.
5. `wi-export` (rules export, host table and registration, skills sync, service) depends on `wi-store`, `wi-note`, `wi-lifecycle` and `wi-history`; `wi-docs` (document and Notion import) depends on `wi-note`, `wi-store`, `wi-history` and `wi-index`, and on `wi-sys` on Windows.
6. `wi-mcp` (MCP stdio server and its lifecycle `provider` module) depends on `wi-export`, `wi-store` and `wi-note`; `wi-lifecycle` is a dev-dependency used by its tests.
7. `wi-notion` (one-way Notion publisher; takes notes from the caller and never depends on `wi-store`) depends on `wi-note` and `wi-history`, and on `wi-sys` on Windows.
8. `wi-cli` (the `wi` and `wiki-island` binaries) depends on `wi-export`, `wi-docs`, `wi-note`, `wi-store`, `wi-mcp`, `wi-lifecycle`, `wi-history` and `wi-notion`, and on `wi-index` and `wi-sys` on Windows only.
9. `wi-bench` (opt-in measurement, not published) depends on `wi-store`, `wi-index` and `wi-history`.
