# Validation

How the project is verified and what each automated case establishes. A pass proves only that case, not general correctness. Tests live under `crates/*/tests/` and in module tests; they are named after the behavior they check, so `cargo test --workspace -- <words>` finds a case.

## Running the gate

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

Ignored tests are measurement helpers that need `--ignored`. Tests use temporary vaults, a temporary `HOME` and a temporary cache (`WI_CACHE_DIR`); crash tests set `WI_FAULT` on a child `wi` process only; tests that need a fixed clock set `WI_NOW`. Nothing touches a real host config, real LaunchAgents or the network. Platform checks (Linux container, Windows cross check) are in [CONTRIBUTING](../CONTRIBUTING.md#linux-container). The CI workflow runs the format check on Ubuntu and clippy plus the tests on Ubuntu, Windows and macOS. Tests that isolate `HOME` also isolate `USERPROFILE`, `APPDATA` and `LOCALAPPDATA`.

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
- Vault handles of one process on the same index share the parsed lifecycle notes; a change indexed through one handle reaches the others' next listing, a handle on another index or a files-only handle keeps its own, a read snapshot keeps the notes it was built from until its next publication, and a lifecycle snapshot with nothing replaced or left out shares the parsed notes and equals a note-by-note copy (`crates/wi-store/src/lifecycle/snapshot.rs` tests).
- Concurrent first initialization of the cache is safe.
- A rebuild run in slices of 4 files ends with the same `files`, `notes`, `observations`, `relations`, search and lifecycle rows and the same search answers as a one-shot rebuild; until its last slice `index_as_of` is empty, progress is reported and search answers from the literal fallback; after the index is closed between slices and notes are changed, deleted and added (also in paths already rebuilt), the resumed rebuild ends equal to a fresh one-shot rebuild; a full sync ends an unfinished rebuild and resolves the links its chunks deferred; a chunk writes nothing once another connection completed the index or restarted the rebuild between chunks; 400 files added before the cursor are reparsed by the last step in chunks of 128 with a budget check after each; a file edited after the last step read it is read again; a file edited before every slice no longer holds the last step back, which finishes within three slices with search off the fallback and rows equal to a fresh rebuild; three files changed behind the cursor with chunks of 2 are read one chunk per slice with one budget check, and the rest, with one file changed again, is read and recorded in the next slice; a chunk that fails rolls back its rows and progress; a schema change between slices rebuilds everything; watcher renames and deletions on both sides of the cursor survive; a cursor left next to a recorded walk restarts the rebuild (`crates/wi-index/tests/sliced_rebuild.rs`).
- A text or title query of 2049 bytes or 65 terms is refused with a distinct error (`QueryTooLong`) and a query at both caps (2048 bytes, 64 terms) is accepted; the over-cap error is returned by `Index::search`, the literal search and the search-with-fallback entry, with and without a database, and never selects the literal fallback; permalink identities carry no term cap. This proves the cap values and where they apply, not retrieval quality or timing (`crates/wi-index/tests/query_cap.rs`).

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
- A read bounded by `max_note_bytes` returns `TooLarge` (path, size, limit) for an attachment and a Markdown file one byte over the limit, reads a file exactly at the limit, still reads the same file through the unbounded `Vault::read`, admits it after the limit is raised, and leaves `NotFound` and the default limit unchanged (`crates/wi-store/tests/bounded_read.rs`).
- The store refuses an over-cap text query as `invalid-request` without switching to the literal scan, and other index errors keep the code `index` (`crates/wi-store/tests/query_cap.rs`).

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
- With a 1 ns budget a deleted index of 300 notes is rebuilt over several passes, across a vault reopen between passes; meanwhile `index_as_of` stays empty, `wi doctor` reports `rebuild in progress` and search answers from the labelled literal scan with the same paths; afterwards the indexed answers equal those of `reindex` and the pass completes. A pass without a budget still rebuilds in one step and finishes an unfinished sliced rebuild; a database deleted mid-rebuild restarts on the same handle; with one file changed before every pass, budgeted maintenance still rebuilds and completes (`crates/wi-store/tests/maintain_rebuild.rs`). While a rebuild is unfinished, `current_rules` still lists a rule outside the rebuilt prefix (`crates/wi-store/tests/lifecycle.rs`). Slice timing on large vaults is unmeasured.

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
- `wi_read` of a file above `max_note_bytes` returns `too-large` with the fixed message; `wi_list` and `wi_find` still return the oversized attachment and Markdown file with their real sizes and no body; a directory move whose folder holds an oversized attachment succeeds (`oversized_files_are_refused_by_read_but_still_listed_and_moved` in `crates/wi-mcp/tests/tools.rs`).
- `wi_search` with a 2049-byte or a 65-term query returns `invalid-request` with the fixed message, and a 64-term query is accepted (`an_over_cap_search_is_an_invalid_request_with_a_fixed_message` in `crates/wi-mcp/tests/tools.rs`).
- `wi_history` of a file above `max_note_bytes` returns the same total and items as a small file and does not read it; a missing file still returns an empty history; `Vault::require_readable` succeeds on an oversized file without reading it, reports a missing file as not found and refuses a symlink and a directory (`require_readable_*` in `crates/wi-store/tests/bounded_read.rs`; `history_of_a_file_above_the_read_limit_behaves_like_a_small_file` in `crates/wi-mcp/tests/tools.rs`).
- A text or title query of 2049 bytes or more, or 65 terms or more, is refused with the fixed message (`an_over_cap_search_is_an_invalid_request_with_a_fixed_message` in `crates/wi-mcp/tests/tools.rs`).
- A broken settings file makes `wi mcp install` report `install-config` with the file name only and no absolute path, content or newline (`a_broken_settings_file_error_names_the_file_and_hides_absolute_paths` in `crates/wi-cli/tests/plain_errors.rs`).

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
- The systemd unit text is asserted exactly for paths with spaces, quotes, backslashes, `$` and `%`, starting with the ownership marker; a literal `$` in the program path and the refusal of program paths with `"` or `\` are covered. First install, unchanged no-op, restart of a changed active unit, rollback after an enable-then-start failure, rollback of a replacement to the prior file, enablement and running state, uninstall of a not-loaded unit, configured versus running status, a missing user manager, refusal to replace or uninstall a same-name unit without the marker, a failed or unknown `is-enabled`/`is-active` answer that aborts before any change, refusal of a running unit that has no unit file wiki-island knows, removal of the owned `default.target.wants` link when the unit file is already gone (and only that link), uninstall through the recorded unit path after a config home change, recording the path before any change, a second setup under another config home retiring the first unit, a failed setup there keeping the earlier record, a damaged record moved aside, concurrent applies at two config homes leaving no unrecorded unit, and uninstall with a damaged record are covered with a fake `systemctl` (`crates/wi-export/src/service/systemd_tests.rs`; these run on every Unix host).
- On Linux, `wi setup`, `wi doctor`, `wi service` and `wi uninstall` run against a fake `systemctl` with and without a user manager, with a manager that disappears after setup's first check (setup finishes with the skip), with `XDG_CONFIG_HOME` changed between setup and uninstall, and with a foreign same-name unit that setup and uninstall leave untouched (`crates/wi-cli/tests/linux_service.rs`, `crates/wi-cli/tests/setup_roundtrip.rs`). Tests that start a freshly copied binary retry Linux `ExecutableFileBusy` (`crates/wi-cli/tests/support/mod.rs`).
- The `linux-systemd` CI job on `ubuntu-24.04` starts a real user manager with lingering, runs `wi setup`, checks that the unit is enabled, active and not restarting, checks `wi doctor`, then runs `wi uninstall` and checks that the unit file is gone and the unit is neither enabled nor active (`.github/workflows/ci.yml`).
- The Task Scheduler definition is asserted exactly for a vault path with spaces, `&`, `<`, `>`, `"`, non-ASCII text and a trailing backslash, and a program path with a space. Command-line quoting, UTF-16 decoding, the `\\?\` prefix, the revision, first install, unchanged no-op, a stopped task started without re-registering, replacement and restart of a changed running task, rollback after a failed registration or start (new task deleted, or previous definition and running state restored), refusal to replace or remove a task without the marker, idempotent uninstall, configured versus running status (including the numeric last result in another display language) and Task Scheduler availability are covered with a fake `schtasks` (`crates/wi-export/src/service/windows_tests.rs`; these run on every host). A second fake models IgnoreNew (a start while running keeps the old instance) and a delete that leaves the running instance alive. With it: a failed list, definition or running query changes nothing on apply, is an error on uninstall and status, and is never read as absent or stopped; a failed `/End` or a watcher that keeps running stops the update before registering and keeps the old watcher; a changed definition replaces the running watcher; a failed registration (twice) restarts the watcher it ended; uninstall ends a running watcher before deleting and keeps the task when `/End` fails; a task without the `<Source>` marker or for another vault is refused; status reports a foreign task as neither configured nor running; a program or vault path with `%` is refused at plan time.
- On Windows, `wi setup`, `wi doctor`, `wi service` and `wi uninstall` run against a fake `schtasks` batch file with and without Task Scheduler, including a stopped task that doctor reports and setup restarts, a foreign task with the vault's name that doctor and status report and that install and uninstall leave in place, and a failed task list that uninstall reports instead of "nothing to remove"; without Task Scheduler `wi uninstall` changes nothing and says to try again, and succeeds once it answers (`crates/wi-cli/tests/windows_setup.rs`); planning against the real Task Scheduler reads the current user's SID (`crates/wi-export/tests/service.rs`).
- The `windows-standard-user` CI job on `windows-latest` creates a local user outside Administrators and, as that user (`Start-Process -Credential`), checks that `whoami /groups` has no `S-1-5-32-544`, then registers, starts, queries, ends and deletes a logon task with `schtasks`, failing on any nonzero `/Create`, `/Run`, `/End` or `/Delete` exit code, an unreadable task status, or a task action process left after `/End` and `/Delete`, and runs `wi setup` (task registered, its exported definition carries `<Source>wiki-island</Source>`), `wi doctor` (registered but not running) and `wi uninstall` (task gone). A credential logon has no desktop session, so an interactive-token task does not run there. A second step, as the runner user, runs the same roundtrip and checks that the task runs, a `wi` process exists, `wi doctor` reports it running, and after `wi uninstall` the task and process are gone (`.github/workflows/ci.yml`, `.github/scripts/windows-standard-user.ps1`).

### CLI (`wi-cli`)

- Every command is exercised; `--json` works on every command; usage errors exit 2 with a JSON error.
- A one-shot command ingests and commits unseen editor changes before it returns; with a live watcher lease it skips the scan.
- `wi doctor` reports unseen files, conflict copies, held paths, degraded index and large attachments with the next step.
- Plain-language errors never expose internals. Unit tests in `crates/wi-cli/src/errors.rs` render every `StoreError` variant (an exhaustive helper makes a new variant fail until it is added), including `Io` for each error kind inside and outside the notes folder and variants that hold an absolute path or jargon, and assert that the output holds no `os error`, no absolute path, no chain separator and none of the engine jargon words; a typed installer or service error keeps its message and anything else gets the generic sentence. `crates/wi-cli/src/error_text.rs` unit tests cover path cleaning (a vault-relative path prints with `/`, also when the message spells it with the platform separator) and jargon detection. `crates/wi-cli/tests/plain_errors.rs` runs the binary: `wi read` of an oversized file prints the plain sentence with no absolute path or content and reports `too-large` with `--json`; `wi search` over the cap reports `invalid-request` and the fixed sentence; `WI_DEBUG=1` adds one `detail:` line and `--json` gains no field. Limit: the presenter scans free text token by token, so an absolute path outside the notes folder that contains spaces can leave its later segments visible.
- Every relative vault path the CLI prints (search hits, import failures, paths inside error sentences) goes through `display_rel` in `crates/wi-cli/src/display_path.rs`, which joins components with `/` on every platform. Unit tests there cover a component-built path on all platforms and, on Windows only, a `a\b\c.md` input that must print `a/b/c.md`. Absolute paths shown to the owner keep native separators. Limit: the Windows case runs on GitHub Actions, not locally.

### Installer and release

- `scripts/tests/install_test.sh` runs `scripts/install.sh` against a local HTTP server with a fake release and a temporary `HOME`: a verified install, a second run adding the `PATH` line once, a checksum mismatch, a checksum list without the asset, with CRLF lines, with an extra field, with two matching lines, and with upper-case digests or the `*` marker; an archive over the size limit (lowered for the test through `WI_INSTALL_MAX_ASSET_BYTES`, with and without a `Content-Length` header) one exactly at it, a download that writes more than the limit and then fails with a curl error other than 63 (a fake `curl`; refused, no source build), an override of 0 (ignored, the default limit stays), and a checksum list over 64 KiB (each oversized download stops with nothing installed and no source build); a missing asset, no release, a redirect to `/releases`, an unreachable server and a forced source build (each falls back to the source build or stops as designed); a program that does not run; an archive with a symlink, a `..` entry or an extra entry; a folder where a program belongs; a failing second rename, a failing restore and `TERM` at each replacement step, which leave the older pair or report an incomplete install; and a verified install over an older pair. The source build is replaced by a stub (`WI_INSTALL_SOURCE_CMD`). It runs on Linux and macOS (`sh scripts/tests/install_test.sh`).
- Both installers cap downloads at 256 MiB for the release archive and 64 KiB for `SHA256SUMS` (`scripts/install.sh`, `scripts/install.ps1`). The caps are unmeasured policy values chosen well above the current archives; `install.sh` relies on `curl --max-filesize` plus a size check after the download (also when `curl` exits with another error after writing more than the limit), `install.ps1` refuses a larger stated `Content-Length` and counts bytes while they arrive. `scripts/tests/release_server.py` omits `Content-Length` while `NO_LENGTH` exists in its root.
- The `install` workflow runs `shellcheck` on `scripts/install.sh` and `scripts/tests/*.sh` and the suite above on `ubuntu-latest` and `macos-latest` (`.github/workflows/install.yml`).
- The `release` workflow builds `wi` and `wiki-island` for five targets, runs `--version` on both, logs each Linux program's highest `GLIBC_` symbol version, packages them, requires exactly five archives, writes `SHA256SUMS` and publishes only for a pushed `v*` tag (`.github/workflows/release.yml`). An end-to-end install of a published release through `scripts/install.sh` on real hardware is unmeasured.
- `scripts/tests/install_test.ps1` runs `scripts/install.ps1` in a new process of the same PowerShell edition against `scripts/tests/release_server.py` with a temporary `LOCALAPPDATA`, `USERPROFILE` and `TEMP`; the fake programs are console executables compiled in the test with the .NET Framework `csc.exe`, and the user `Path` text and value kind are saved and restored in a `finally` block. It covers: a verified install; a second run keeping one `Path` entry; a `REG_EXPAND_SZ` `Path` containing `%USERPROFILE%` and a `REG_SZ` `Path` keeping their kind and text; a missing `Path` created as `REG_EXPAND_SZ`; a checksum mismatch over an older pair and on a fresh machine; a checksum list without the asset, with two matching lines, with an extra field, with CRLF lines, and with an upper-case digest and `*` marker; an archive over the size limit (lowered for the test through `WI_INSTALL_MAX_ASSET_BYTES`), also served without a `Content-Length` header so the received bytes are counted, and a checksum list over 64 KiB (nothing installed); a missing asset, a missing list, no release (404 and a redirect to `/releases`) and an unreachable server; an archive with a `..` entry, a drive-qualified entry, an extra entry, a duplicate entry, a missing entry, a directory entry and a symbolic-link entry; a folder where a program belongs; a program that does not run; an install over an older pair, also while the older `wi.exe` is running, with the renamed copy removed on the next run; a failing placement that restores the older pair and a failing restore that reports an incomplete install with both names still runnable; an unsupported architecture; a failing `wi setup`; `WI_INSTALL_DIR`; an install folder, parent folder or grandparent folder that is a junction (refused, target untouched; the check walks every existing folder up to the drive root); an install folder name with `;` or `%` (installed, `Path` unchanged); and a run piped into `Invoke-Expression`, where a failure is a catchable error and the calling PowerShell keeps running.
- The `install-script-windows` job of the `install` workflow runs PSScriptAnalyzer (warnings and errors, only `PSAvoidUsingWriteHost` excluded) on both scripts, runs the test above under Windows PowerShell 5.1 and PowerShell 7 on `windows-latest`, and installs the latest published release with the checked-out `install.ps1` piped into `Invoke-Expression` (`WI_INSTALL_SKIP_SETUP=1`), then runs `--version` on both programs (`.github/workflows/install.yml`).
- Limits of the Windows installer: it is proven only by that CI job. Parsing and the analyzer were also checked with PowerShell 7 on macOS, and the link, ancestor-walk, download-limit and archive-attribute checks were probed there with symbolic links, the release server and in-memory archives; the install, registry and junction cases have not run outside CI. Untested: ARM64 Windows, Windows PowerShell on a .NET Framework older than 4.7.2 (the archive attribute check then ends in the generic failure sentence, so nothing is installed), corporate proxies, antivirus programs that lock files during the rename, and execution-policy settings other than the runner's. Whether the default program folder on `windows-latest` has a link above it is proven only by the last step of that job, which installs into it. The last step depends on GitHub and the network; an end-to-end install of a published release on real Windows hardware is unmeasured.

### Compatibility

- A plain folder of Markdown notes, including an Obsidian vault, works unchanged; `.obsidian/` is never modified.
- Existing vaults keep working across format changes; legacy keys are read and new writes use the current names.

## Host and client checks

Run by hand when a host version changes; not part of the automated gate.

- One real model-driven `wi_search` call has succeeded in each of Claude Code, Codex CLI and Antigravity CLI against a scratch vault under `/tmp`, passing the server per invocation or through a temporary registration that was removed afterwards. This does not show that writes, resources, skills or `wi setup` work inside a real host session, nor behavior on other versions or platforms.
- The official TypeScript MCP SDK client and the MCP Inspector CLI have listed the tools, called tools on a synthetic vault and received matching text and structured content within the byte cap.

## Platform

macOS arm64 (APFS) is the tested platform. A Linux arm64 container passes the gate, the CI matrix runs it on GitHub Actions Ubuntu, and the `linux-systemd` job exercises a real systemd user manager; bare-metal Linux and ext4 are untested.

Windows x86_64 (MSVC, NTFS) runs the clippy gate and `cargo test --workspace` on GitHub Actions `windows-latest`; locally, `cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings` and `cargo build --tests --workspace --target x86_64-pc-windows-gnu` check that everything compiles. Windows-specific cases, all runtime-tested on the runner:

- `wi-sys/tests/windows.rs`: a junction and a file or directory symlink are refused by every handle method; create never overwrites; rename with and without replace; a sharing violation on unlink and replace of a file opened without delete sharing; a path over 300 characters through handle-relative creation; 8.3 alias names; liveness of a running and an exited process; a job kills a grandchild started immediately, on terminate and on drop; a Credential Manager round trip with a unique target; console echo control refused on piped stdin.
- `wi-store/tests/windows.rs`: a bounded read refuses a file above the limit and still refuses a junction (`a_bounded_read_refuses_a_file_above_the_limit_and_still_refuses_a_junction`); a junction or symlink inside the vault is refused for read, stat, write and move; reserved and stream names are refused and create nothing; `\` input, verbatim input and a differently cased root are accepted and refused where specified; create never overwrites; a replacement is stale when the target changed or was deleted; every existing component is checked for an 8.3 alias (runs only when the volume generates short names).
- `wi-store/tests/windows_transport.rs`: an HTTPS authentication challenge fails before the 60 s timeout without prompting, and a short timeout kills a hook grandchild. `wi-store/tests/sync.rs` has Windows counterparts for a rejected push and for reserved remote names, including no false deletion by automatic ingress.
- `wi-docs`: an extractor job is killed on timeout, on an output cap and when its leader exits, with a grandchild. Native separators in import paths become portable paths and a junction in an imported folder is pruned.
- `wi-index`, `wi-history`: native nested paths share the slash keys of the index; junctions and symlinks are not indexed; the data directory comes from `LOCALAPPDATA` alone; the synced-folder check resolves junction chains.
- `wi-export`: each host's Windows config path, with `CODEX_HOME` kept; the Task Scheduler service (see [Export, hosts and service](#export-hosts-and-service-wi-export-wi-cli)).

Not measured on Windows: power-loss durability (namespace operations are not flushed, see [OPERATIONS](OPERATIONS.md#windows)), ARM64 Windows, ReFS, FAT and network shares, UNC vaults, a real interactive console echo restore, a failing job termination, and a real Notion publish through Credential Manager. Power-loss durability is not simulated on any platform.

## Dependency checks

Both run in the `policy` job on `ubuntu-latest` (`.github/workflows/ci.yml`; [DEPENDENCIES](DEPENDENCIES.md#license-policy-and-network-client-check)).

- `cargo deny check licenses` (cargo-deny 0.20.2, `deny.toml`) passes when every crate in `Cargo.lock`, for all features and all targets, has a license expression that an allow-list entry satisfies. It exited 0 on the committed lock file when run locally. It does not check advisories, bans or sources, and the compatibility of the allow list is an inference from license texts, not legal advice.
- `sh scripts/check-offline-tree.sh` passes with nine `ok` lines (`wi-mcp`, `wi-store` and `wi-index`, each on `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin` and `x86_64-pc-windows-msvc`), whatever the host. It fails when the normal dependency tree of `wi-mcp`, `wi-store` or `wi-index`, on any of those targets, contains a listed HTTP, TLS or socket crate, or when `tokio` or `mio` has its `net` feature on. When it passes, those three trees hold no network client. It says nothing about subprocesses such as `git`, about `std::net`, about crates outside those three trees (`wi-notion` has an HTTPS client on purpose) or about the runtime behavior of the whole binary (the `wiki-island` binary still contains the Notion client). By hand, with `WI_OFFLINE_CHECK_CRATES=wi-notion` and with `WI_OFFLINE_CHECK_CRATES=wiki-island` it exits 1, which shows it detects the client trees of those crates; there is no automated test of the script.
- Unmeasured: the `policy` job on a GitHub runner (the workflow file is not validated by `actionlint`, and whether `taiki-e/install-action` accepts the pinned `cargo-deny@0.20.2` form is untested).

## Documentation checks

Relative links in `README.md`, `AGENTS.md`, `CONTRIBUTING.md`, `docs/` and the crate READMEs resolve. The documents describe current behavior only: they contain no phase or review codes, dates of past work, commit hashes or names of other products.

## Remote sync and device work notes

- `wi-store/tests/sync.rs` uses a temporary bare remote and cloned vaults with separate data and index directories: fast-forward and publication-generation changes, disjoint divergence and local-first merge ancestry, same-note copies, both delete/edit directions, provenance round trips, disjoint device work paths, read-only foreign items, unavailable remotes and backoff, rejected push retries, and large-file exclusion with one work request, one health report after a day of failure, and startup of the separate background thread, and operation retries from both merge parents after index deletion, an empty local remote with a different default branch, and a busy git staging index that does not block sync or subsequent saves, and refusal of an unrelated vault identity without changing the remote, and refusal to upload an oversized blob that another git tool committed, and an oversized engine write that stays in the folder and out of local and remote history.
- `wi-cli/tests/sync.rs` uses separate temporary homes and local remotes. It aborts at large-file reporting/exclusion, flush, fetch, intent, file application, ref publication, generation publication, index update and push boundaries; reopening and retry retain both conflicting versions and converge. It also aborts namespaced work-note migration at ordinary move boundaries and checks byte identity, listing repair and idempotence. Status and setup work with an unreachable remote, and a normal CLI read after a large-file exclusion crash does not commit the excluded bytes. A subprocess lock probe checks that transport holds neither engine lock and disables interactive prompts; a short injected timeout checks process-group termination, an editor save after a crash is retained in a recovery copy, and an externally moved branch is retained without leaving a blocking intent.
- `wi-history/tests/history.rs` checks operation and loss trailers from both merge parents, without double-counting shared ancestors.
- `wi-mcp/tests/lifecycle.rs` verifies foreign-device work is listed read-only and excluded from `next`. CLI unit tests use an injected runner to check unavailable authentication, private repository creation/reuse and public repository refusal without contacting a host.
- `wi-store/tests/folder_mode.rs` checks, without a remote, that ingest, capture and `wi write` of bytes above 50 MiB reach no reachable git object, add one exclude rule and one work request each.
- `wi-store/tests/sync.rs` also checks that a tracked note grown past 50 MiB keeps its last committed version at the local and remote tip and on the peer with one work request; that a file above 50 MiB ingested before `wi sync setup` never enters history or blocks the first push; that a remote file name this device cannot create is skipped with one work request, never recorded as a deletion, while later notes still push; and that a peer's `wi doctor` lists a committed sync conflict copy as `ok` and its work item read-only. The v6 regressions add: a valid 225-byte remote name skipped here is never deleted by reconcile or a later sync; a conflict on a 203-byte name places a visible copy of at most 224 bytes; a note that shrinks back under 50 MiB is ingested and reaches the peer; an oversized write retried with its operation id replays; an editor save to the file displaced by an oversized or small attachment write is injected immediately before the final displaced-byte check and kept with a conflict-copy result and work request.
- `wi-cli/tests/sync.rs` checks that an editor save made while a sync intent is pending (remote deletion or remote change, and after a crash between files) stays visible, leaves no hidden copy and is committed over the planned version on both devices.
- `wi-store/tests/folder_mode.rs` and the `wi-history` cloud unit test check that a missing component followed by `..` under a symlinked synced folder still chooses folder mode. `wi-export/tests/views.rs` checks that two open handles of one root under different data directories keep rules, target and backup state apart, with no process-wide registry.
- `wi-notion/tests/publish.rs` checks that `./` and `../` wikilinks resolve from the source note's folder; missing targets and paths above the root stay text.

## Notion view

- `wi-store/tests/view.rs` checks that the committed snapshot reads the branch tip only, skips non-notes, keeps permalinks unique and flags archived notes.
- `wi-cli` unit tests (`publish_cmd`) run against a loopback mock with an injected secret store and token source: page-link parsing, a token that never reaches the vault or data directory, committed-only publication, unchanged notes without requests, update and archive, persisted backoff with deferred background attempts, and a background thread that publishes once after the committed state settles and stops cleanly; parent input with multibyte characters is refused without a panic, the token prompt never reads while echo is on, and setup cannot switch the parent during a publish. `wi-cli/tests/publish.rs` checks that status and an unconfigured publish need no network or keychain and that bad parents and tokens are refused before the keychain is used.
- Real Notion, real keychain access and long-running service cadence are unmeasured.

- These are process-crash and local-transport checks. Real remote authentication, real hosting setup, power loss, and network performance are unmeasured.
