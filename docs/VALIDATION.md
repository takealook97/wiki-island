# Validation

How the project is verified and what each automated case establishes. A pass proves only that case, not general correctness. Tests live under `crates/*/tests/` and in module tests; they are named after the behavior they check, so `cargo test --workspace -- <words>` finds a case.

## Running the gate

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

Ignored tests are measurement helpers that need `--ignored`. Tests use temporary vaults, a temporary `HOME` and a temporary cache (`WI_CACHE_DIR`); crash tests set `WI_FAULT` on a child `wi` process only; tests that need a fixed clock set `WI_NOW`. Nothing touches a real host config, real LaunchAgents or the network. Platform checks (Linux container, Windows type check) are in [CONTRIBUTING](../CONTRIBUTING.md#linux-container).

## Method

- **Off-switch baseline.** With `lifecycle: off`, behavior matches plain-note behavior on the compatibility scenarios. Every lifecycle change is judged against this baseline, and changed expectations are disclosed next to the old ones.
- A runner aborts before the test if the vault is not in its seed state.
- Counts are reported with denominators, never as a bare percentage.
- A case whose outcome depends on the platform names the platform it was observed on.

## Cases by area

Each row is a finite, checkable statement.

### Notes and parsing (`wi-note`)

- Frontmatter edits change only the touched key; unknown keys, comments, quoting, BOM, CRLF and a missing final newline survive byte for byte.
- Unrepresentable values and edits that would change a neighboring key are refused with a typed reason.
- Malformed frontmatter yields diagnostics, plain-Markdown content and no trusted metadata.
- Observations, relations and wikilinks parse as specified, excluding code fences.
- Permalinks are stable across a rename and get a numeric suffix on collision.
- A sidecar carrying the legacy parse-semantics key reads as before; new writes emit only `parse_semantics`.

### History (`wi-history`)

- A blob is durable before a manifest record names it; barrier order is asserted on an in-process trace.
- A killed process at each flush step ends in the old state or the completed operation, never a lost acknowledged write.
- Flush commits each recorded operation in order, retires only the committed prefix, and never forces a moved branch.
- A corrupt manifest tail is quarantined with everything after it and a work item is opened.
- Two processes cannot publish at once; a bulk batch commits as one commit and a move never leaves its source behind.
- Revert never rewrites history and refuses while records are pending.

### Index and search (`wi-index`)

- Deleting the index loses nothing; a schema-version change rebuilds it.
- Exact and prefix matches, title weighting, Korean two-syllable queries and mixed-script queries find the intended notes.
- A plain multi-term query returns all-terms hits first, then ranked OR hits; quoted phrases and explicit operators stay strict.
- A missing or corrupt index answers from a labelled literal scan; malformed query syntax selects the fallback.
- Per-file failures are reported as skipped paths and diagnostics while other files commit.
- `.obsidian/` and `.trash/` are never indexed; ignored directories are pruned without an error.
- Lifecycle snapshots in the index are used only while the file stamp, hash and format tag match.
- Concurrent first initialization of the cache is safe.

### Write path and watcher (`wi-store`)

- A write with a stale expected checksum is refused and changes nothing; `wi_write` without a checksum on an existing path is refused.
- An operation id replays its first result after a file change, a restart, a flush and index deletion; a different digest is refused.
- An editor save injected between the checksum check and the swap is kept as a conflict copy with a work item where atomic exchange is supported.
- A kill before the intent leaves the old file; after the intent and before the swap, the intent is dropped; after the swap and before `done`, the operation completes with its recorded actor.
- Protected names (case and Unicode variants), `.git`, paths outside the vault and symlinks are refused; recovery never touches files behind a changed directory.
- A file with conflict markers gets a work item and the engine does not write to it.
- A read during a publication retries once and then reports `generation-changed`; hits changed after indexing are re-read or withheld.
- An external editor change is indexed and committed after the debounce; the watcher ignores an engine write in flight.
- One failing path does not keep other editor notes out of history; an unreadable file is reported.
- A deleted engine-written note is recorded as `missing`.
- A partial watcher repair failure withdraws the lease and retries; a clean repair restores it.
- A recovery problem on one path holds only that intent: the vault opens, other paths write, `recovery-held` names the held path, and non-regular scratch entries are reported by name without being followed.
- A failed index repair never blocks the vault.

### Lifecycle (`wi-lifecycle`, wiring in `wi-store`, `wi-mcp`, `wi-cli`)

- Each gate passes and fails as specified; failures leave a candidate with per-gate guidance; passing adoption writes no gate record.
- User-origin notes are completed and adopted without owner action; agent-origin claims keep the source requirements.
- Rules and preferences adopt only with user origin and are never retired by agents or automatic actions.
- Freshness: windows, TTLs, drift, missing sources, future timestamps and unknown evidence times withhold or age claims as specified; durations parse strictly.
- Conflicts, supersedes validation and replacement order are crash-safe; a recovery pass completes an interrupted supersede.
- The per-entity change cap counts losses over a trailing 24 hours, survives index loss, defers overflow and resumes it without an agent; there is no vault-wide cap.
- MCP claim edits create candidates and never change the old note; reverting a lifecycle note creates a fork with `restores`.
- Capture filters secrets and one-off values; snapshots record `observed_by`, and a sidecar without it reads as `unknown`.
- With `lifecycle: off` every write is accepted and every action allowed.

### Maintenance

- A pass with nothing to do writes and commits nothing and is idempotent.
- A pass retries held intents, repairs a pending index, rebuilds only on detected drift, creates and refreshes work items, auto-closes items whose cause is gone, and opens procedure health items with stable keys.
- The built-in maintenance procedure is written once and refreshed only when the binary's text changes.
- The watcher runs a pass after its first repair, then only after a generation change, at most every 15 minutes, plus one full pass per day; a time-budgeted slice resumes on the next tick.

### MCP (`wi-mcp`)

- Every tool has positive and negative cases; there is no delete tool; tool results stay within the byte cap and hit limit.
- A qualifier-bearing claim is delivered whole or withheld whole; withheld entries carry only a reason and a count.
- Read modes `current` and `all` (with the old names as aliases) label eligibility as specified.
- `wi_work next` returns at most the requested items within the byte budget.
- Engine-owned fields cannot be written; origin, manifest actor and commit actor agree.
- Pagination, aliases, section edits, directory moves and restart replay behave as specified.
- A real child-process JSON-RPC session covers initialize, list, write, search, read, refusals and EOF; unknown tools and malformed arguments return `-32602`.
- Procedure resources list only eligible procedures; direct reads flag excluded and stale ones; duplicate ids are refused.
- The first query on a fresh device cache answers from the index after one catch-up.

### Import (`wi-docs`)

- PDF, DOCX, PPTX, CSV and images produce a sidecar and a run note; originals are unchanged; page maps cover every page and resolve pins to UTF-8 ranges.
- Re-import reuses permanent results and refuses to overwrite an edited sidecar; a partial run caused by a timeout or the budget is retried.
- A missing external tool is reported as absent, a probe timeout as unknown (retried next import); descendants of a killed tool are stopped.
- A directory import continues past unreadable entries and respects the vault's ignore rules.
- A Notion export (zip or folder) becomes notes with ids stripped from names, links rewritten and CSV databases converted.

### Export, hosts and service (`wi-export`, `wi-cli`)

- Rules fragments include only current user-origin rules, drop whole notes to fit the cap, list omissions, expire at `valid_until`, and refuse to overwrite a hand edit.
- Host registration previews a diff, applies only with `--yes`, preserves unrelated keys, comments, bytes and file modes, backs up the original and is idempotent; hosts without an existing config are skipped.
- Skills sync writes only owned directories, is incremental by checksum, removes what it owns and never touches other directories.
- `wi setup` is idempotent and prints its summary; `wi uninstall` removes everything it owns and no notes.
- The service plist is written atomically, rolled back on a bootstrap failure, and refused for a development binary (fake launchctl, temporary HOME).

### CLI (`wi-cli`)

- Every command is exercised; `--json` works on every command; usage errors exit 2 with a JSON error.
- A one-shot command ingests and commits unseen editor changes before it returns; with a live watcher lease it skips the scan.
- `wi doctor` reports unseen files, conflict copies, held paths, degraded index and large attachments with the next step.
- Plain-language errors never expose internals.

### Compatibility

- A plain folder of Markdown notes, including an Obsidian vault, works unchanged; `.obsidian/` is never modified.
- Existing vaults keep working across format changes; legacy keys are read and new writes use the current names.

## Host and client checks

Run by hand when a host version changes; not part of the automated gate.

- One real model-driven `wi_search` call has succeeded in each of Claude Code, Codex CLI and Antigravity CLI against a scratch vault under `/tmp`, passing the server per invocation or through a temporary registration that was removed afterwards. This does not show that writes, resources, skills or `wi setup` work inside a real host session, nor behavior on other versions or platforms.
- The official TypeScript MCP SDK client and the MCP Inspector CLI have listed the tools, called tools on a synthetic vault and received matching text and structured content within the byte cap.

## Platform

macOS arm64 (APFS) is the tested platform. A Linux arm64 container passes the gate; bare-metal Linux, ext4 and Windows runtime are untested (Windows only type-checks, and vault functions fail closed on non-Unix). Power-loss durability is not simulated.

## Documentation checks

Relative links in `README.md`, `AGENTS.md`, `CONTRIBUTING.md`, `docs/` and the crate READMEs resolve. The documents describe current behavior only: they contain no phase or review codes, dates of past work, commit hashes or names of other products.

## Remote sync and device work notes

- `wi-store/tests/sync.rs` uses a temporary bare remote and cloned vaults with separate data and index directories: fast-forward and publication-generation changes, disjoint divergence and local-first merge ancestry, same-note copies, both delete/edit directions, provenance round trips, disjoint device work paths, read-only foreign items, unavailable remotes and backoff, rejected push retries, and large-file exclusion with one work request, one health report after a day of failure, and startup of the separate background thread, and operation retries from both merge parents after index deletion, an empty local remote with a different default branch, and a busy git staging index that does not block sync or subsequent saves, and refusal of an unrelated vault identity without changing the remote, and refusal to upload an oversized blob that another git tool committed, and an oversized engine write that stays in the folder and out of local and remote history.
- `wi-cli/tests/sync.rs` uses separate temporary homes and local remotes. It aborts at large-file reporting/exclusion, flush, fetch, intent, file application, ref publication, generation publication, index update and push boundaries; reopening and retry retain both conflicting versions and converge. It also aborts namespaced work-note migration at ordinary move boundaries and checks byte identity, listing repair and idempotence. Status and setup work with an unreachable remote, and a normal CLI read after a large-file exclusion crash does not commit the excluded bytes. A subprocess lock probe checks that transport holds neither engine lock and disables interactive prompts; a short injected timeout checks process-group termination, an editor save after a crash is retained in a recovery copy, and an externally moved branch is retained without leaving a blocking intent.
- `wi-history/tests/history.rs` checks operation and loss trailers from both merge parents, without double-counting shared ancestors.
- `wi-mcp/tests/lifecycle.rs` verifies foreign-device work is listed read-only and excluded from `next`. CLI unit tests use an injected runner to check unavailable authentication, private repository creation/reuse and public repository refusal without contacting a host.
- `wi-store/tests/folder_mode.rs` checks, without a remote, that ingest, capture and `wi write` of bytes above 50 MiB reach no reachable git object, add one exclude rule and one work request each.
- `wi-store/tests/sync.rs` also checks that a tracked note grown past 50 MiB keeps its last committed version at the local and remote tip and on the peer with one work request; that a file above 50 MiB ingested before `wi sync setup` never enters history or blocks the first push; that a remote file name this device cannot create is skipped with one work request, never recorded as a deletion, while later notes still push; and that a peer's `wi doctor` lists a committed sync conflict copy as `ok` and its work item read-only.
- `wi-cli/tests/sync.rs` checks that an editor save made while a sync intent is pending (remote deletion or remote change, and after a crash between files) stays visible, leaves no hidden copy and is committed over the planned version on both devices.
- `wi-store/tests/folder_mode.rs` and the `wi-history` cloud unit test check that a missing component followed by `..` under a symlinked synced folder still chooses folder mode. `wi-export/tests/views.rs` checks that two open handles of one root under different data directories keep rules, target and backup state apart, with no process-wide registry.
- `wi-notion/tests/publish.rs` checks that `./` and `../` wikilinks resolve from the source note's folder; missing targets and paths above the root stay text.

## Notion view

- `wi-store/tests/view.rs` checks that the committed snapshot reads the branch tip only, skips non-notes, keeps permalinks unique and flags archived notes.
- `wi-cli` unit tests (`publish_cmd`) run against a loopback mock with an injected secret store and token source: page-link parsing, a token that never reaches the vault or data directory, committed-only publication, unchanged notes without requests, update and archive, persisted backoff with deferred background attempts, and a background thread that publishes once after the committed state settles and stops cleanly. `wi-cli/tests/publish.rs` checks that status and an unconfigured publish need no network or keychain and that bad parents and tokens are refused before the keychain is used.
- Real Notion, real keychain access and long-running service cadence are unmeasured.

- These are process-crash and local-transport checks. Real remote authentication, real hosting setup, power loss, and network performance are unmeasured.
