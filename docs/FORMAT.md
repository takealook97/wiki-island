# Format

This defines the files. Lifecycle behavior is in [KNOWLEDGE](KNOWLEDGE.md); the write path is in [OPERATIONS](OPERATIONS.md).

## Vault layout and config

A vault is one directory of notes with git history, either in `<vault>/.git` (git mode) or in an external git directory in the per-device data directory (folder mode, for a vault in a file-synced folder, judged by the nearest existing ancestor of a new vault path with symlinks resolved and the missing components applied lexically, so `alias/missing/../vault` is judged through `alias`; when a `..` climbs back to an existing folder the lexical result is resolved once more, so `plain/missing/../hop/vault` with `hop` a symlink is judged through the symlink target; [DESIGN](DESIGN.md#storage-and-history)). Any folder of Markdown notes is a valid vault.

```text
<vault>/
  <any path>/<note>.md      notes; any folder layout
  <file>.pdf, <file>.pdf.md original and its import sidecar
  document-ingestion-runs/  import run notes
  archive/                  archived notes, original relative path kept
  .wiki-island/exports/     generated rules fragments (hidden, still synced; no wall-clock field)
  .wiki-island/
    config.toml             vault config, tracked (`vault_id`, `primary_device`, ...)
    work/<device_id>/       work items (engine-owned; only the named device acts on them)
    captures/               capture notes
    procedures/             built-in maintenance procedure (engine-owned)
    local/                  per-device state, gitignored (git mode only)
  .obsidian/, .trash/       editor folders: ignored, never read or modified

<data_dir>/wiki-island/     per device ($WI_DATA_DIR; macOS ~/Library/Application Support/wiki-island)
  device-id                 this device's random id
  vaults/<vault_id>/git/    folder-mode history (core.worktree = the vault); ignore rules in info/exclude
  vaults/<vault_id>/local/  folder-mode per-device state (what git mode keeps in .wiki-island/local)
```

Existing flat engine work notes migrate into the opening device's work folder through durable move intents. Their bytes and work identifiers stay unchanged. Other devices' folders remain visible and read-only; an index rebuild recovers them all from Markdown. The new layout does not change frontmatter or the index schema.

Remote configuration, retry state and the replayable sync intent are per-device JSON files in `<git dir>/wi/` (`sync-config.json`, `sync-state.json`, `sync-intent.json`); none is synchronized. A sync intent names the expected and target commit, a reserved publication sequence, old/new blobs for each changed path and conflict-copy requests. Completed sync uses the existing manifest checkpoint format to advance the generation. Older notes and checkpoints remain readable. A local `git-index-pending` flag requests a retry of a disposable staging-index refresh; ordinary flush/recovery clears it after refreshing from the current branch.

In folder mode the engine writes no `.gitignore` into the vault; its ignore lines go to `<git dir>/info/exclude`, which ingest, the watcher and the index walk honour after `.ignore` and `.gitignore` files. Git mode also honours `info/exclude`, including managed large-file exclusions.

The SQLite index is not in this tree: it lives in the platform cache, one per device. Folder names carry no permission. Any folder of Markdown notes, including an Obsidian vault, is a valid vault; its `.obsidian/` and `.trash/` folders are treated as engine-internal by ingest, the index walk and `wi doctor`.

Vault config is `.wiki-island/config.toml`. Its keys:

| Key | Values | Default |
| --- | --- | --- |
| `lifecycle` | `on` \| `off` | `on`. `off` gives plain-note behavior, the validation baseline ([KNOWLEDGE](KNOWLEDGE.md#lint-and-the-off-switch)) |
| `stale_after_days` | integer days | `30`, unmeasured policy choice |
| `durability` | `standard` \| `full` | `standard`: `fsync` barriers; `full` adds `F_FULLFSYNC` on Apple platforms ([OPERATIONS](OPERATIONS.md#durability-modes)) |
| `export_paths` | list of vault-relative paths | no additional paths; ordinary tools refuse to write under them |
| `export_codex_path`, `export_claude_path`, `export_gemini_path` | vault-relative generated fragment | `.wiki-island/exports/AGENTS.md`, `.wiki-island/exports/CLAUDE.md`, `exports/GEMINI.md` |
| `watch_debounce_ms` | positive integer | `200`, unmeasured policy choice |
| `max_note_bytes` | positive integer | 8 MiB, unmeasured policy choice. Bounds what the index parses and what a read aimed at a file delivers, for every file type ([PROTOCOL](PROTOCOL.md#read-limit-and-query-cap)) |

The config also records which hosts have skills sync enabled ([INTEGRATIONS](INTEGRATIONS.md#skills-export)). `vault_id` is a UUIDv4 written by `wi init`. Unknown TOML keys and comments are preserved. `.wiki-island/local/` is per-device and gitignored. The publisher lock and manifest live in `.git/wi/`. The other defaults (the change-cap budget, 16384 bytes, 8 hits) are in [KNOWLEDGE](KNOWLEDGE.md); all are unmeasured.

## Note syntax

The note grammar:

- Frontmatter: YAML between `---` lines. `title` defaults to the file name stem and `type` to `note`. Unknown keys are kept as metadata. YAML aliases and merge keys are unsupported.
- Observation: `- [category] content #tag (context)`.
- Relation: `- relation_type [[Target]] (context)`.
- A wikilink `[[Target]]` in prose creates a `links_to` edge. An unresolved target keeps its authored text.
- Headings are optional.
- Permalink: an explicit frontmatter `permalink` wins. Otherwise the existing permalink is kept, or one is built from the normalized file path, with a numeric suffix on collision. A move keeps the permalink by default.

Permalinks are addresses. A rename, an equal title or duplicate content never proves identity.

## Lifecycle frontmatter

Extra frontmatter keys, treated as ordinary metadata by other readers.

```yaml
type: memory                # memory | capture | work
kind: fact                  # fact|decision|rule|procedure|preference|event|null
claim_kind: current-state   # current-state|historical|intent|null
status: candidate           # see status sets below
summary: null               # one declarative statement; null while unresolved
tags: null                  # null = unresolved, [] = checked, none apply
entities: null              # [{id, label, source}]
scope: null                 # filter string only, never authorization
slot: null                  # optional conflict key, for example "port"
value: null                 # the claim's value within its slot
observed_at: 2026-11-05T09:00:00Z   # required when claim_kind=current-state
origin: {kind: agent, locator: "mcp:wi_capture", captured_at: 2026-11-05T09:00:00Z}
sources:                    # pins; the whole source is the dependency
  - ref: notes/pricing.md
    commit: 3f2a…           # git hash when known; null with pending: true
    pending: false
    source_hash: "sha256:…"   # always present
    span_hash: "sha256:…"
    locator: {span: {start: 120, end: 188}}
applicability: {from: null, until: null, conditions: [], supersedes: []}
check: {checked_at: null, result: null, integrity_at: null, volatile: false, ttl: null}   # ttl: ISO 8601 duration
adoption: {at: null, by: null, literal: null}   # gates: written only when a gate fails
conflict_with: []
restores: null              # {ref, commit} of an earlier revision this note brings back
excluded: false             # procedures: do not execute
exclusion_reason: null
skill_id: null              # stable id for kind=procedure
```

- **Plain notes.** A note whose `type` is not `memory`, `capture` or `work` is a plain note, labelled `lifecycle: none` and never trusted memory. A `status` key on it is ignored for trust.
- For agent-origin notes the engine may lower a claimed `observed_at` to the evidence time of the pinned sources ([KNOWLEDGE](KNOWLEDGE.md#freshness)); a user-origin `observed_at` must not be in the future.
- `origin` says who authored the current claim content (`summary`, `sources`, `applicability`, `kind`, `claim_kind`, `observed_at`, `entities`, `scope`, `slot`, `value` and, for `type: memory`, the Markdown body). Writes set it: CLI → `user`, MCP call → `agent`, import → `file` or `url` ([AUTHORITY](AUTHORITY.md)). File ingress reads it from the note itself: a note without `origin` and without `noted_by: agent` (new, or one whose agent label the owner removed) gets `user`; a written `origin` stays as written. A file-ingress change to claim content resets the note to `candidate`; MCP never changes claim content in place ([KNOWLEDGE](KNOWLEDGE.md#adoption)).
- Engine-owned, never writable through `wi_edit` or `wi_write`: `status`, `adoption`, `check`, `conflict_with`, `origin`, `excluded`, the work and queue keys `work`, `deferred`, `work_id` and `work_kind`, and `restores` except at creation. MCP also may not remove or change the agent label `noted_by`; a rewrite that omits it keeps it. The engine also refuses every engine write to a note with `locked: true`.
- `conditions` use the grammar `[{key, op: eq|in, values: [..]}]`, a conjunction; `eq` has one value, `in` a non-empty sorted set. `from` is inclusive and `until` exclusive; `null` bounds are unbounded.
- `slot` and `value` drive model-free conflict detection; without `slot` there is none ([KNOWLEDGE](KNOWLEDGE.md#adoption)). `adoption.literal` is set by the literal gate. `adoption.gates` holds one `fail: <reason>` per failed gate and is written only on failure; a passing adoption omits it. Keys the engine filled for a user-origin note are listed in `adoption.completed`, and defaulted keys are omitted.
- `check` records the last source check (`result: unchanged|changed|missing|unavailable`) and, separately, `integrity_at`: when the stored bytes were last re-hashed unchanged. Integrity is not an observation. A URL snapshot's observations are capture events recorded in the snapshot's sidecar `<snapshot>.wi-observed.json`, one `{at, hash, observed_by}` per host fetch. `observed_by` is the ingress origin kind (`agent` for MCP, `user` for the CLI and watcher); a sidecar without it reads as `unknown`. Freshness is computed at read time ([KNOWLEDGE](KNOWLEDGE.md#freshness)). Future timestamps are refused.
- `adoption.by` is an informational agent or process id, not an attestation.
- `applicability.supersedes` is a relation. It does not change status by itself; the adoption step does, and only for valid targets ([KNOWLEDGE](KNOWLEDGE.md#conflicts-and-the-automatic-change-cap)).

### Pins

`{ref, commit, pending, source_hash, span_hash, locator}`. `ref` is a vault-relative path. `source_hash` is the SHA-256 of the whole source and is always present; `span_hash` covers the cited part. `commit` is the git commit holding those bytes, when known. A pin to bytes not yet committed has `commit: null` and `pending: true`; a later read resolves it through the pending manifest or a later commit with the same blob. An external source is a snapshot file captured into the vault; the pin points to the snapshot, and `locator` also keeps the URL. One locator kind:

| Locator | Shape | Unit |
| --- | --- | --- |
| span | `{span: {start, end}}` | UTF-8 byte offsets |
| page | `{page: {n, label}}` | physical page number plus printed label |
| region | `{region: {page, bbox}}` | `[left, top, right, bottom]` from top-left; PDF points or image pixels ([sidecars](#document-sidecars)) |

Native page maps count Unicode code points; pin spans count UTF-8 bytes.

### Status sets

| Note type | Statuses | Terminal |
| --- | --- | --- |
| memory | `candidate`, `active`, `rejected`, `superseded`, `archived` | `rejected`, `superseded` |
| capture (`capture_status`) | `pending`, `resolved` | none |
| work (`work.state`) | `ready`, `investigating`, `checkpointed`, `closed` | none; a reopen trigger restarts it |

- No delete transition exists for any type.
- A terminal note is never revived. A revert creates a new note that names the old one in `restores`, and it passes the gates again.
- `archived` returns to `active` only through a new revision that passes the gates.
- Capture notes add `outputs: [permalink]` and `no_memory_reason`. Work notes carry `work: {target, state, next_action, next_trigger, outcome, reason, limits, reopen}` ([OPERATIONS](OPERATIONS.md#work-items)).

## Null and empty

`null` means unresolved: nobody has looked. `[]` means checked, and none apply. This holds for `tags`, `entities`, `conditions` and every list key. A candidate's nulls never default to values that would let it pass the gates.

## Byte-preserving frontmatter edits

A frontmatter parse error names the line and says how to fix it (for example: quote a value that contains `: `). A create is never reported as an "edit refused".

An engine edit changes only the keys it touches. Key order, quoting, comments, unknown keys and the body stay byte for byte. For example `custom_key: "Keep: exactly"` and `unknown_list: [z, a]` stay as written and no `permalink` key is added. If a touched key cannot be edited without changing other bytes, the edit is refused with a reason.

The parser reads YAML values and patches only the touched top-level key span; it never serializes a whole document. A replacement covers the key line and its value block, including any inline comment on that key. Adjacent standalone comments are kept. Removal is refused with `WouldChangeOtherKey` if retaining adjacent whitespace or comments would change a neighbouring value (for example a keep-chomped block scalar). New keys append before the closing fence. Missing frontmatter gets a minimal leading block; edits return a new note and perform no filesystem writes. Unedited notes preserve BOM, CRLF, null bytes and missing final newline.

Editing supports block-style top-level mappings. Flow-style root mappings, anchors/aliases, multiple YAML documents, explicit document terminators, indentation tabs, complex or non-string keys and non-finite numbers are read-only with a typed reason. Unterminated fences, invalid YAML, duplicate top-level keys and non-mappings produce diagnostics, retain the original bytes and expose the content as plain Markdown where safe. Invalid UTF-8 is a typed parse error. No malformed metadata supplies trusted lifecycle state.

Written values use plain scalars when conservatively safe, otherwise YAML-compatible JSON double quotes; lists and objects use flow collections. The plain whitelist is safe under both YAML 1.1 and 1.2: a leading letter, followed by letters, digits, spaces or `._/-`, excluding boolean and null words in any case. Date/time, numeric-looking, merge/value and tab-containing strings are quoted. Unicode line separators are escaped. A post-edit parse verifies the requested value; an unrepresentable value is refused.

## Document sidecars

Import of PDF, DOCX, PPTX, CSV or an image keeps the original file unchanged and writes:

- a sidecar `<file>.md`, for example `report.pdf.md`, of type `extracted_text`
- a run note under `document-ingestion-runs/`

Together they record the source SHA-256, the extractor name and version, the options hash and the page map. Extracted text is searchable, but no observations or relations are parsed from it (`parse_semantics: false`). Sidecars written by imports before the key was renamed carry the legacy key `bm_parse_semantics`; readers accept both and writers emit only `parse_semantics`. The `derived_from` provenance edge from the sidecar to its source comes from the validated source record. Extraction is local and model-free; OCR is optional ([OPERATIONS](OPERATIONS.md#document-import)).

Sidecars carry `schema_version: "1"`, `schema: schema/document-extraction`, `type: extracted_text` and `parse_semantics: false`. Source records contain `kind: file`, MIME type, a UUID, the vault-relative path, a `sha256:` checksum and the byte size. The extraction summary has engine and version, profile, options hash, status, UTC timestamp, duration, page count, extracted-page count and outstanding OCR page diagnostics. Fallback extractors actually used are recorded as sidecar tags: `extractor/<engine>` per distinct fallback (for example `extractor/pdftotext`) and `ocr` when any page was OCRed; per-page engines stay in the run note's `page_extractors`.

The `derived_from` index edge keeps the full binary filename as its target and is pinned to the original file (`relations.target_file = 1`): it is never resolved through note identities, so it cannot loop back to the sidecar whose permalink matches the binary name, and context lists it as an edge without traversing into it.

Run notes have `schema: schema/document-ingestion-run` and `type: document_ingestion_run`. Their `extraction.page_map` has `unit: unicode_code_point`, the raw-body SHA-256, the Unicode character length and ordered, consecutive half-open `{page, start, end}` ranges covering every page, including empty ones. The body is normalized (LF, no leading blank lines, one trailing newline for nonempty text). Extraction `status` is distinct from ingestion `stage: raw`.

The run body's fenced JSON contains `utf8_page_map` in the same shape with `unit: utf8_byte`, `geometry`, OCR `words`, per-page `page_extractors`, `pages_not_indexed`, `diagnostics` and an exact `sidecar` snapshot, which preserves the accepted revision and allows a retry after a stopped run-only publication. Both maps bind to the same raw-body SHA-256. `output.raw.checksum` covers the entire accepted sidecar bytes. Native maps are never applied to an enriched or edited body. A stale hash, nonconsecutive range or UTF-8 boundary inside a character is refused.

Page numbers are one-based physical PDF pages, presentation-order PPTX slides, or one logical DOCX/CSV/image page. Printed labels are caller-supplied display labels. `pins::resolve_page` returns the UTF-8 byte range; `resolve_region` validates finite, positive-area bounds against recorded geometry. PDF boxes are in PDF points (1/72 inch) from the unrotated MediaBox top-left, ignoring display rotation and CropBox; pages with a non-default UserUnit have no region geometry. Image boxes use first-frame pixels from the top-left. DOCX, PPTX and CSV claim no region geometry. A resolved region addresses the original page and box, not a substring of native text. `wi_docs::resolve_pin` verifies the current original's SHA-256 before resolving.

Identity: the source UUID is UUIDv5 of `file:<normalized relative path>` under the vault UUID. Document and run UUIDs use a fixed namespace; run identity includes source UUID and hash, pipeline, extractor and version, profile, options hash and a null prompt version. The path rule is portable to Windows and rejects reserved filenames. Source identity across a move is not inferred.

Size bound: each complete serialized sidecar and run note must fit `max_note_bytes`. Import keeps a prefix of whole pages and records omitted one-based page numbers in `pages_not_indexed`, with status `partial` and a diagnostic. Maps still cover every page; omitted pages have empty ranges and `resolve_pin` refuses them. Metadata that cannot fit causes refusal before publication. This limits searchable output, not the canonical source.

Projections: DOCX table rows render one per line with cells joined by ` | `; headers and footers are appended under marked sections. PPTX speaker notes share their slide's page under `## Speaker notes`. CSV uses a Markdown table for up to 50 columns, otherwise escaped TSV.

Document and run notes get no `origin` key; the manifest's `import` actor records their origin. An existing sidecar is replaced only when its exact bytes match its previous run's accepted revision and the expected checksum still holds; otherwise import refuses without rewriting. Malformed supported inputs produce a failed run without a sidecar. Extractor limits: [wi-docs](../crates/wi-docs/README.md).

### Notion exports

`wi import` recognizes a Notion export (a `.zip` or folder whose names carry 32-hex Notion ids) and converts it offline into ordinary notes under `Notion/<export name>/`: Notion ids are stripped from names, links are rewritten, CSV databases become notes with their columns as top-level properties (prefixed only when a column collides with an engine key), pages get minimal frontmatter with no absolute paths, and attachments (images, files) are copied beside the converted pages. A hidden record at `.wiki-island/imports/Notion/<export name>.md` keeps the mapping and checksums (a `_Import.md` an older version left beside the pages moves there once), so a repeated import changes only what changed, and a converted note or copied attachment edited in the vault is never overwritten. Nothing is fetched and no live sync exists. An outer zip that contains `*-Part-N.zip` members is recognized too. A folder export is passed as one argument (`wi import "Notion Export"`), not expanded into its files.

## Generated rules fragments

Generated paths are always protected in addition to `export_paths`. Each fragment begins with `<!-- wiki-island generated sha256:<digest> -->` followed by a newline; the digest covers all remaining bytes. Files without a valid marker are never overwritten. Config is loaded at vault open. Rebuilding the index leaves notes, fragments and work items intact. See [INTEGRATIONS](INTEGRATIONS.md#rules-export) for rendering, caps and stale-file limits.
