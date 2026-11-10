# Protocol

The local MCP stdio surface, implemented in `wi-mcp`. Exact executable input schemas are returned by `tools/list` and defined in [schema.rs](../crates/wi-mcp/src/schema.rs).

## Transport

`wi mcp [--vault PATH]` (also `wiki-island mcp`) serves MCP stdio using the official Rust SDK `rmcp`. `WI_VAULT` supplies the default vault; otherwise the current directory is used. Diagnostics go to stderr. `--json` adds no CLI output to the MCP stream. `wi setup` registers this command with every detected host ([INTEGRATIONS](INTEGRATIONS.md)).

The server advertises baseline [2025-11-25](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports). It supports initialize, `notifications/initialized`, `tools/list`, `tools/call`, `resources/list`, `resources/read` and EOF shutdown. Resource URI templates and subscriptions are absent. There is no HTTP transport, delete tool, embedding runtime or remote query. No tool uses the network; sync and Notion publishing are CLI and background-only ([OPERATIONS](OPERATIONS.md#git-history-and-sync)).

The server opens and replays the vault without a full scan, then accepts requests. A background thread registers the watcher, reconciles ingress, catches up the index and runs maintenance ([OPERATIONS](OPERATIONS.md#maintenance)). An existing index can omit new files until catch-up finishes. An index without a completed walk selects a labelled literal scan; `index_as_of: null` means no completed walk. The server notices when the index file is replaced and reopens it. Returned hits are always re-read and hash-checked.

## Tools and arguments

Fifteen tools, all prefixed `wi_`. All argument objects reject unknown keys, wrong types and invalid enum or range values. Integer fields accept integral floats such as `2.0`. Inputs above 1 MiB are refused. Strings are nonempty unless described as content or query. Paths are inside the selected vault; `/` means its root only for list and find. No project selector, output format or token budget is accepted. Each tool declares only its input schema; the shared result envelope is described [below](#results), not repeated per tool. Documented argument aliases (for example `file_path` for `identifier`) are accepted but not advertised; a conflict between a canonical argument and an alias is refused.

Read tools (`wi_read`, `wi_search`, `wi_context`, `wi_list`, `wi_find`) accept `read_mode: current|all` (default `current`; `historic` and `explore` are accepted aliases of `all`) and a `conditions` key/value object. Write tools (`wi_write`, `wi_edit`, `wi_move`, `wi_archive`, `wi_revert`, `wi_capture`, `wi_adopt`, `wi_work`) accept optional `expected_checksum` and `operation_id`.

| Tool | Required arguments | Optional arguments / defaults |
| --- | --- | --- |
| `wi_read` | `identifier` (path or unique permalink) | `section` (unique heading, heading path, or zero-based `Log[1]`); `start_line`, `end_line` (positive, inclusive); `include_frontmatter: true`; `include_content: true` |
| `wi_write` | `identifier` (Markdown path), `content` (may be empty) | Create-only without `expected_checksum` |
| `wi_edit` | `identifier`, `command` | `command: append\|prepend\|find_replace\|set_metadata\|replace_section\|insert_before_section\|insert_after_section`; `content`; `find_text`; `expected_replacements: 1` (1–1000000); `metadata` object; `remove` string[]; `section`; `replace_subsections: true` |
| `wi_move` | `destination`, exactly one of `identifier` (file) or `path` (directory) | A trailing `/` moves into a folder. File moves retain permalinks. Directory members publish separately |
| `wi_archive` | `identifier` (path) | Moves under `archive/` |
| `wi_search` | none | `query` (default empty); `mode: text\|title\|permalink` (text); `filters` object; `note_type`; `categories` string[] (any exact observation category); pagination |
| `wi_context` | `identifier` | `depth: 1` (0–16); `since` (RFC 3339, `YYYY-MM-DD`, or relative `7d`/`24h`/`2w`); `max_related: 7` (0–7); bidirectional traversal |
| `wi_list` | none | `path: "."`; `depth: 1` (1–10); `glob` (basename); `sort: title_asc\|title_desc\|updated_asc\|updated_desc`; pagination |
| `wi_find` | none | `path: "."`; `name` (basename glob); `filters` object; `fields` string[] (dot paths); `depth: 10`; `limit: 8` (1–8). Name and filters are exclusive; fields needs filters |
| `wi_history` | `identifier` (path) | `command: log` (default); pagination; read-only |
| `wi_revert` | `identifier` (path), `commit` (full id) | Lifecycle restoration creates a fork |
| `wi_capture` | `content` | `title`, `no_memory_reason`; `snapshot: {url,path,content}`; `candidates: [{path,content}]` |
| `wi_adopt` | `identifier` | Reruns the gates on a candidate |
| `wi_lint` | none | `identifier` (default: every memory note) |
| `wi_work` | `action: next\|list\|open\|close\|sweep` (`command` is accepted as an alias) | `identifier`, `target`, `reason`, `outcome`, `limits`, `reopen_trigger`, `reopen_detail`; for `next`: `max_items` (default 3) and `max_bytes` (default 4096) |

There is no delete tool. Procedures are served as MCP resources ([SKILLS](SKILLS.md)). `wi_work next` returns the highest-priority open work items, compact and within the byte budget, so a host agent can act on them cheaply. Listed items carry `actionable`; another device's work items are `false` and read-only here ([OPERATIONS](OPERATIONS.md#work-items)).

Section and line selectors are exclusive. Lines are one-based in the selected content; a qualifier-bearing lifecycle note may not be sliced, projected or stripped by a read. Heading selection uses Markdown parsing, excludes code fences and supports Setext headings. Replace-section keeps the heading and surrounding blank lines; `replace_subsections: false` keeps nested sections. Section edits pin the checksum observed while preparing the patch and persist the original semantic arguments for idempotent replay, including after restart.

`set_metadata` accepts set and remove operations, preserves untouched bytes and refuses `title` and `permalink` changes. `find_replace` edits body text only, with an exact occurrence count. An MCP edit of claim content creates a candidate ([KNOWLEDGE](KNOWLEDGE.md#adoption)); without the lifecycle provider the result is `claim-edit-requires-candidate`. A CLI edit of a superseded or rejected note returns `terminal-note` without publishing; fork with `restores`. File ingress records already-edited bytes and a work request and never resets terminal status. Unsupported or malformed frontmatter on MCP creation is refused when agent origin cannot be recorded.

Find uses the index's metadata predicates. The subtree is applied before the delivery limit (at most 10000 internal matches); `depth` applies to glob and list walking, not metadata search. A plain-note `fields` projection omits body text and retains the selected metadata; a lifecycle projection adds `selected_fields` while retaining all qualifiers. Find and list skip ignored files, `.obsidian/`, `.trash/`, engine state, scratch names and symlinks. Name-mode find and list return only `items` rows `{path,kind,size,title,permalink}`; folder size, title and permalink and absent titles are null. Oversized and binary files remain discoverable by name; binary body delivery is unsupported. Folders sort first; title sorts fall back to the path; `updated` sorts use file modification time. Context `since` also uses modification time for related files and always retains the root.

### Pagination

List, find (including folders), search, history and recent-history reads accept `limit: 1..8` (default 8) and either `offset: 0..4294967295` (default 0) or `page: 1..4294967295` (one-based); combining them is refused. They return `offset`, an observed `total`, `total_capped` (search and metadata find), a nullable `next_offset`, and `withheld: [{reason:"hit-limit",count}]` for the remaining rows. Reuse the same filter and limit with `next_offset`. These are live offset pages, not a frozen snapshot. Byte omissions report `does-not-fit` separately. Search and metadata find rank and limit candidates in the index: a page examines about four times `offset + limit` candidates, doubling (at most 8 rounds) only while an observation-category or folder filter or hash verification leaves it short, never more than 10000; `window_truncated: true` marks that the window reached 10000 with matches unexamined. Without a post-filter, `total` is a SQL count capped at 1000 and `total_capped: true` means more matches exist.

### Read limit and query cap

A read a caller aims at a file is bounded by the vault config key `max_note_bytes` ([FORMAT](FORMAT.md#vault-layout-and-config)), 8 MiB (8388608 bytes) by default, for every file type, Markdown or attachment. It covers `wi_read`, the identifier resolution behind `wi_context` and `wi_edit`, the delivery of `wi_search` hits and, on the command line, `wi read`. A file above the limit is refused with `too-large` and the fixed message `The file is larger than the read limit. Open it in its own app, or raise max_note_bytes in the settings file.` without being read into memory: the size comes from the open file handle, and a read stops after `limit + 1` bytes if the file grows meanwhile. A file exactly at the limit is read. The owner raises the limit with the same key the index uses; the new value applies when the vault is next opened.

- `wi_list` and `wi_find` never drop a file above the limit: its row carries the real `size`, `title: null` and the generated permalink, and its body is not loaded.
- A `wi_search` hit whose file is above the limit is withheld as `changed-after-index`, like an unreadable hit, and does not match a `categories` filter.
- `wi_edit` of a note above the limit is refused with `too-large`, because identity resolution shares the bounded read. Such notes are already skipped by the index. Raise `max_note_bytes` to edit one.
- `wi_history` returns no file content and is not refused for the size of the file: it only checks that the target can be read, without reading it (`Vault::require_readable`). A missing file returns an empty history; a symlink or directory is refused like a read.
- Not bounded by this limit: write-path checks, directory-move checksums, crash recovery, ingest, import, publish, maintenance and lifecycle-internal reads. They still read whole files. The limit is a policy choice and unmeasured.

A text or title query above 2048 bytes (the raw text) or 64 terms (whitespace-separated units) is refused with `invalid-request` and the fixed message `The search text is too long. Search for a few key words instead.` The caps apply to `mode: text` and `mode: title` only; permalink identities, `wi_find` glob patterns and exact context identities have no term cap. A query at both caps is accepted. An over-cap query never selects the literal scan fallback. The check runs first, so the same error is returned at any length, including above the 16384-byte bound; that older bound still applies to the other query modes. Both caps are unmeasured policy choices.

Existing vaults keep working: there is no format, config or index-schema change and no index rebuild, and the existing `max_note_bytes` key gains a second use. What changes for them: a direct read of a file above `max_note_bytes` is refused instead of delivered, and a text or title query above the caps is refused instead of searched.

### Directory moves

A directory move selects visible files recursively without following symlinks, pins each current checksum, and applies the same store and lifecycle rules as individual moves. It is a sequence of durable publications, not an atomic rename: a later refusal reports partial progress and completed members stay moved. Ignored files, symlinks and empty source directories remain. Per-file `expected_checksum` and `operation_id` are refused; resume remaining members by path. `data.moved` records the full completed count.

## Write preconditions

Every note mutation uses ingress `Mcp` and records actor `mcp`; creation adds `origin.kind=agent`. Engine-owned fields, the work directory and `type: work` notes are refused ([OPERATIONS](OPERATIONS.md#write-path)). A body edit of a memory note is a claim edit and forks a candidate. Protected paths, symlinks, locked notes and conflict-marker files are refused by `wi-store`.

`expected_checksum` is the SHA-256 over the complete file bytes. A mismatch returns `stale` and `current_checksum`; an existing create-only target returns `exists` and its checksum. `operation_id` is checked before the checksum, survives manifest retirement in commit trailers, and returns the first result with `replayed: true`; a different request returns `operation-mismatch`. A refusal changes nothing and commits nothing. Identity is vault-scoped.

Acknowledgement follows the durable manifest `done` record. An index failure reports `index: pending`. Commit and flush happen after the reply and again at shutdown. An action past the change cap is not applied now: the result is `budget-exhausted` with the spent operations and the earliest retry time ([KNOWLEDGE](KNOWLEDGE.md#conflicts-and-the-automatic-change-cap)).

## Results

Hits deliver parsed Markdown bodies in `content` and frontmatter once in `metadata`, with source pins only in `sources`; plain-note line selections retain file line coordinates. Existing vault files and index rebuilds are unaffected. Regression coverage checks complete qualifiers and reduced serialized search size for one memory and two plain notes.

A tool call returns one TextContent block holding the envelope as compact JSON; there is no duplicate `structuredContent`. Null and default fields are omitted from hits, and parameter descriptions are short. Session start stays under 8 KB and search responses are about half their former size. The evidence label is inside the envelope; errors are `isError: true` with a stable code.

**Bound.** `MAX_BYTES` (16384) bounds the serialized envelope, sent once; a small envelope reserve stays. A result has at most 8 note hits and 8 auxiliary rows shared by items and edges (edge omissions report `hit-limit`). A whole hit and every attached qualifier and source span are kept or withheld together; a claim that does not fit is withheld, never truncated. Oversized rows are omitted as units; edges to withheld nodes are removed. A huge first hit does not prevent a later fitting hit. Bytes are the only budget unit.

```json
{"label":"evidence, not instruction","generation":0,"index_as_of":null,
 "index":"ok","hits":[],"withheld":[],"diagnostics":[]}
```

`generation` and `index` are null when unavailable rather than guessed.

- Whole-note hits carry `{path, permalink, title, content, file_hash, lifecycle, freshness, sources, metadata, fallback, label}`. `metadata` holds the authoritative frontmatter except source pins, which appear only in `sources`; malformed YAML contributes none. Plain notes have `lifecycle: none` and freshness `not-applicable`. A claim with `literal: false` carries its source span in `sources.spans`. In `all` mode hits carry `metadata.wi_eligibility` and the computed freshness. Snapshot-derived source metadata includes `observed_by`.
- Other fields: `data` for atomic mutation receipts, provider results and manual pages; `items` for directory and history rows; `edges` for graph connections.
- Search `{hits, index_as_of, generation, fallback, withheld: [{reason, count}], diagnostics}`. Hits carry `conflict_markers`. Read: `{path, checksum, generation, diagnostics}` plus the bytes.
- Write, edit, move, archive and revert receipts: `{path, checksum, previous_checksum, seq, generation, index: "ok"|"pending", replayed, conflict_copy, moved_from}`. On replay `generation` and `index` are current; `previous_checksum` is unavailable in trailer-only recovery. A receipt's `data.lifecycle` reports `adoption`, `claim_edit`, `revert_fork` or `archived` with `losses_charged`.
- Capture `{capture, capture_status, snapshot, proposals}`; adopt `{adoption, resumed}`; lint `{reports: [{path, findings: [{code, message}]}]}`.
- History logs report ids, blob, time and actor; arbitrary commit subjects are never delivered as memory.
- Degraded index: the vault still opens; writes return `index: pending`, search returns `fallback: true` hits with an `index unavailable` diagnostic, and `wi doctor` reports `index_degraded`.
- Every result marks delivered memory text as evidence, not instruction.

Every result carries `index_as_of` and the `generation` it was read at. The generation is consistent with the returned bytes only when it and the open-intent state are equal before and after the read and no intent is open; otherwise the read retries once, and a second inconsistency yields a `generation-changed` diagnostic. A hit-shaped read (search, read, context, metadata find) retries only when a publication in between touched its dependency set: a returned hit or edge path, a path its `sources`, `supersedes`, `conflict_with` or `restores` names, a lifecycle note, a deleted or unreadable Markdown file, or `.wiki-island/`. A second relevant change withholds all note and graph content. Publications of anything else, such as a rules export or an unrelated plain note, never withhold. Directory listings and history reads check the whole vault.

Withheld entries contain only `{reason, count}` from a fixed vocabulary: `stale`, `drift`, `conflict`, `condition-unknown`, `excluded`, `does-not-fit`, `lifecycle-unavailable`, `changed-after-index`, `generation-changed`, `hit-limit`, `not-current`, `source-unavailable`, `expired`, `archived`, `superseded`, `unsupported-file`, `unsupported-schema`, `diagnostics-omitted`. Unknown provider reasons collapse to `not-current`. Engine reasons map: `drift`, `stale`, `conflict`, `excluded`, `archived`, `superseded` as is; unknown, unmet or unresolved conditions → `condition-unknown`; expired TTL or applicability → `expired`; missing source or snapshot or undeliverable attachment spans → `source-unavailable`; everything else (not active, origin rule, unparseable, future timestamp, missing evidence time) → `not-current`. Diagnostics collapse to bounded code and count rows; no withheld claim text appears in either channel.

## Errors

Refusal messages are plain sentences that say what to do next and never expose raw I/O paths or note text. Stable codes:

| Codes | Meaning |
| --- | --- |
| `protected-path`, `locked`, `conflict-markers`, `engine-owned-field` | Path, lock, merge or ownership refusal |
| `recovery-held` | A recovery intent holds this path because a file could not be read or renamed; fix permissions or the obstruction and the next operation retries. Other paths are unaffected |
| `exists`, `stale`, `operation-mismatch`, `not-found` | Write precondition, retry identity or lookup failure |
| `terminal-note`, `claim-edit-requires-candidate`, `lifecycle-revert-requires-fork` | Revise through a candidate or `restores` fork |
| `claim-edit-mixed`, `claim-edit-refused`, `revert-refused`, `rule-retirement`, `work-close-refused` | Lifecycle mutation policy refusal |
| `capture-dropped`, `capture-empty`, `budget-exhausted`, `lifecycle-unavailable` | Capture, change cap or provider refusal (`budget-exhausted` carries `retry_at`, `spent` and `work`) |
| `edit-refused`, `invalid-request`, `unsupported` | Unsupported format or edit, bad arguments or operation; `invalid-request` also covers a text or title query above the [query cap](#read-limit-and-query-cap) |
| `too-large` | A file above the [read limit](#read-limit-and-query-cap); the message is fixed and says to open the file in its own app or raise `max_note_bytes` |
| `future-timestamp`, `unparseable-timestamp` | A lifecycle timestamp is in the future or unreadable |
| `io`, `parse`, `history`, `index`, `config`, `watch`, `not-initialised` | Filesystem, encoding, history, cache, config, watcher or vault failure |
| `generation-unavailable`, `service-unavailable` | No trustworthy generation or unavailable service; observation fields are null |
| `error-details-omitted` | Oversized details omitted under the byte cap |

Unknown tool names and malformed `tools/call` parameters return JSON-RPC `-32602`. A missing section or occurrence returns `not-found`; an ambiguous section is `invalid-request`. CLI usage errors exit 2 and, with `--json`, print `{"error": {"code": "usage", ...}}`. Custom lifecycle providers may define additional codes and must document them. The server awaits capacity in a bounded 256-request queue; requests beyond it wait rather than receive a busy error.

## Lifecycle provider

The `provider` module of `wi-mcp` defines `Lifecycle: Send + Sync`; calls run serially on the vault's owning thread. `capture`, `adopt`, `lint` and `work` take the original JSON arguments and return typed results. `filter_current` receives full `DeliveryHit` values and query conditions and returns hits and content-free withheld counts; `label` attaches freshness and source evidence; write, edit, section-edit, move, archive and revert hooks let the lifecycle enforce candidate creation and change caps without a crate cycle. `serve_stdio` (and `wi mcp`) uses the wired `VaultLifecycle`; `serve_stdio_with` and `Server::open` select another. The `Unavailable` provider returns `lifecycle-unavailable` for the lifecycle tools, withholds lifecycle notes from current reads and passes ordinary store mutations through.

## Brief

`wi brief` and the `wiki-island://brief` resource provide a brief. Its `presented` field describes the density ladder: `full`, `core`, `summary`, or `title`.

## Procedure resources

`resources/list` exposes eligible, non-excluded procedures only. `resources/read` accepts `wiki-island://procedures/<percent-encoded-skill_id>`, compared after percent-decoding with a case-insensitive scheme and authority, so equivalent spellings (`deploy-x`, `deploy%2Dx`) name one resource. It returns `application/json` text with `evidence_not_instruction`, mode, eligibility, reason, status, freshness, excluded, exclusion_reason, stale, do_not_execute, sources (note, checksum, refs, spans), path and procedure body. Excluded, stale and candidate procedures stay directly readable as `all`, never current. Unknown ids return resource-not-found (`-32002`). Resolution is per requested identity: terminal predecessors are retained history, so the single current entry (else the single non-terminal entry) of that `skill_id` is returned; competing current entries, or only terminal history, are refused for that id alone, and discovery omits such an id without hiding others. Direct JSON text is capped at 1 MiB, separate from the tool result cap.
