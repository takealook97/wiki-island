# Knowledge lifecycle

The lifecycle sits on top of plain notes and is implemented as the pure library `crates/wi-lifecycle`, applied by `wi-store`, the MCP provider and the CLI. Field shapes are in [FORMAT](FORMAT.md#lifecycle-frontmatter). No step needs a human approval ([AUTHORITY](AUTHORITY.md)).

**All defaults here are unmeasured policy choices:** `stale_after_days` 30, the URL snapshot `ttl` `P7D`, the per-entity change-cap budget of a third of an entity's claims, the 16384-byte result cap and the 8-hit cap.

## Capture

Capture stores what was said or seen, quickly and without judgment. It needs no model, no network and no metadata chores. A capture note starts `pending` and becomes `resolved` only when it lists its `outputs` or gives an exact `no_memory_reason`. Partial work stays `pending` and lists what remains.

Filters, applied at capture and at proposal:

- Never store secrets, one-off values, or assistant suggestions the user did not adopt.
- Do not widen a task-specific instruction into a general preference.
- An empty capture with a `no_memory_reason` is valid, not a failure.
- A proposal whose body hash equals an existing note's is dropped as a duplicate.

The filter drops on secret patterns (private key headers, AWS key ids, GitHub, Slack, `sk-` and Stripe live keys, Google API keys, JWTs, credential assignments with any value except a placeholder such as `<...>`, `${...}`, `***` or `redacted`, and URLs with credentials) and on one-off values (verification codes, a bare number, hash or UUID). A match drops; no match proves nothing. Assistant suggestions the user did not adopt, and widened instructions, cannot be detected without a model; the host handles them. With `lifecycle: off` nothing is filtered.

## Adoption

Capture never adopts. A candidate becomes `active` automatically when every gate passes. A failure is recorded in `adoption.gates`; a pass leaves no gate record.

1. **Sources**: `fact`, `decision` and `current-state` claims have at least one pin, except user-origin claims, which are self-attested and may have none. Every pin that is present resolves to local bytes and its `source_hash` matches them.
2. **Classification**: `kind`, `claim_kind`, `summary`, `applicability`, `entities` (`[]` allowed) and `scope` are non-null. For user-origin notes an absent `applicability.conditions` counts as `[]`.
3. **Literal**: if `summary` equals the exact span text (its `span_hash` matches), the claim gets `literal: true`. Otherwise `literal: false`, and delivery always attaches the source span, so qualifiers and negations travel with the claim. "X (beta, EU only)" never travels as bare "X".
4. **Lint** is clean ([Lint](#lint-and-the-off-switch)).
5. **Conflict** is model-free and uses the optional `slot` key. Two claims conflict when they share `slot`, an entity in `entities` and `scope`, have overlapping applicability, and differ in `value`. The gate fails when the candidate conflicts with an active claim. Without a slot there is no automatic detection; a host may open a conflict work item through `wi_work`. Valid targets in the candidate's own `applicability.supersedes` are left out.
6. **Origin**: `rule` and `preference` notes pass only with `origin.kind=user` ([AUTHORITY](AUTHORITY.md)).

`rejected` is used only for an origin-rule violation, a duplicate of an active claim (equal body hash) or an unparseable note, with the reason recorded. Every other gate failure leaves the note `candidate` with a work item. An absent `status` on a memory note counts as `candidate`.

**Gate order.** With `lifecycle: off` nothing changes. An unparseable note is rejected; one that is not memory, or not a candidate, is unchanged. `superseded` and `rejected` are terminal. Then the origin rule (rejecting with an `origin-rule` work item), the duplicate check, then sources, classification, literal, lint, supersedes and conflict.

**Claim revisions.** Claim content is `summary`, `sources`, `applicability`, `kind`, `claim_kind`, `observed_at`, `entities`, `scope`, `slot` and `value`, and for `type: memory` notes the Markdown body. `origin` says who authored the current claim content.

- A file-ingress change to claim content (an editor or the CLI) sets `origin.kind=user` and resets the note to `candidate`, and the gates run again. A user editing their own file counts as authorship.
- MCP tools never change the claim content of an existing note in place. `wi_edit` on claim fields creates a new candidate note `<stem>-rev-<8 hex>.md` (origin `agent`, `supersedes` the old note, its own permalink) and runs the gates; the old note is never changed. Mixing claim keys with other keys is refused (`claim-edit-mixed`). For a `rule` or `preference` that candidate can never adopt under the origin rule, so it becomes a work item.
- Engine-owned fields are never writable through `wi_edit` or `wi_write` ([FORMAT](FORMAT.md#lifecycle-frontmatter)). Ordinary tools also cannot create or change a `type: work` note or anything under the work directory.
- **Work closure.** `wi_work close` accepts only an engine-created work note. The recheck condition fingerprint is stored in `work.condition` from creation, so closure records it as `reopen.observed`.

### User-origin auto-completion

The owner should not have to fill classification fields or pin sources for their own notes.

- **Which notes.** `type: memory`, lifecycle on, `origin.kind=user` (file or CLI ingress). Never `agent`, `url` or `file` origin.
- **Which keys.** Top-level keys that are absent or null, never a present value; explicit owner values always win. Defaults: `kind: fact`; `claim_kind: historical`; `summary` = the first non-empty body line with Markdown heading, list and quote markers stripped, trimmed and truncated to 200 characters at a character boundary (empty body: the `title`, else the file stem); `entities: []`; `scope: personal`; `applicability: {from: null, until: null, conditions: [], supersedes: []}`; `sources: []`.
- **Top level only.** If `applicability` exists without `conditions`, nothing is written into the mapping; the classification gate reads the absence as `[]` for user-origin notes.
- **How.** The engine evaluates the completed snapshot, then writes the keys together with `status` and `adoption` in the same checksum-guarded edit as byte-preserving key insertions. The keys it filled are listed in `adoption.completed`; keys left at their defaults are not written. Origin stays `user`.
- **Not a new claim revision.** The completion is an engine write (actor `engine`). It does not reset the note to `candidate` and does not trigger a second gate run.
- **Self-attested sources.** A user-origin claim with `sources: []` passes the sources gate for every `kind` and `claim_kind`: the owner is the evidence. Pins that are present are verified exactly as before. Agent-origin rules are unchanged. A user `current-state` claim without pins ages from its `observed_at`.
- **Per-gate guidance.** A `candidate` outcome's work note `next_action` is built from the failed gates: classification names the missing fields, sources names the failing pins, conflict names the conflicting notes, lint lists its codes, and origin, supersedes and literal each give a one-line fix.
- **Limit.** A shell-capable agent is recorded as `user` ([AUTHORITY](AUTHORITY.md#limits)); this widens what such an agent can assert.

**Origin rule.** Agent-derived content may become `fact`, `decision`, `procedure` or `event`, or a `historical` claim. It never becomes a protected `rule` or `preference`.

### Agent-origin auto-completion

- **Plain notes first.** Plain notes are the default memory and are searchable at once; the MCP instructions say so first. A claim (`type: memory`) is optional.
- **Sources as refs.** An agent gives `sources` as refs only: vault paths or captured URLs. The engine resolves each ref and computes `source_hash` and the span at write time; no hand-made hashes. It completes the classification fields for agent claims the same way as for user claims. The gates still require that every source exists and resolves.
- **Noted rules and preferences.** An agent-written `rule` or `preference` claim is not adopted. The engine stores it as a plain note `preferences/<slug>.md` or `rules/<slug>.md` with `noted_by: agent` and returns outcome `noted`. The brief lists noted items in their own labelled section after the protected rules and preferences. No work item asks the owner to edit files.

### Details that fix behavior

- **Literal gate and attachments.** A deliverable span is a `span` locator over available bytes whose range is valid UTF-8 and whose `span_hash` matches. A claim is literal only when its `summary` equals such a span. A pin without a locator supports the whole source by its hash and needs no attachment; a pin with a locator must deliver its span, which a `page` or `region` locator cannot. Automatically resolved spans are capped at 1 KiB, cut on line boundaries and marked as cut (`cut: true`). Otherwise the literal gate fails with `attachment-unavailable`. At read, an active non-literal claim whose spans cannot be delivered is withheld as `attachment-unavailable`; a span is never dropped silently.
- **Unknown evidence times.** For a non-user claim, every pinned source must carry an engine-recorded evidence time. If one is unknown, `effective_observed_at` is unknown, the sources gate fails, a `supersedes` target is invalid (`unknown-evidence-time`), and reads withhold the claim (`missing-evidence-time`).
- **Null conditions.** `applicability.conditions: null` is unresolved: the classification gate fails and reads withhold an active note (`unresolved-conditions`).
- **Unparseable** means invalid UTF-8, malformed frontmatter (unterminated, invalid YAML, duplicate key, non-mapping, multiple documents) or a lifecycle key of the wrong shape on a lifecycle note. Read-only but valid frontmatter (flow style, anchors) is lint `unsupported-markup`, not a rejection.
- **Body hash** is the SHA-256 of the trimmed body. An empty body has none, so it never counts as a duplicate. The duplicate check skips the note itself and the candidate's `supersedes` targets.
- **Lint gate.** Findings already decided by another gate (`unsupported-claim`, `broken-required-source`, `competing-active-claim`) do not fail the lint gate a second time.

## Conflicts and the automatic-change cap

- A conflict has no winner by modification time or by vote.
- **Propagation.** Both notes get `conflict_with` and a work item only when both are active. A conflicting candidate gets `conflict_with` on itself and a work item; the active claim stays current.
- **Replacement.** A candidate whose `applicability.supersedes` lists active claims is checked for conflict with the valid targets left out. A target is valid only if it has the candidate's `kind` and `scope`, shares at least one entity, has the same `slot` when the target has one, and, for a `current-state` target, has a `last_verified` no later than the candidate's. An invalid target leaves the candidate `candidate` with a work item, and the target stays current. On adoption the engine writes the candidate as `active`, then marks each target `superseded`. Read rule: an active note listed in another active note's `supersedes` is never current, so a crash between the two writes is safe; a recovery pass completes the target status. A pair in which one note validly supersedes the other is a replacement, not a conflict.
- **Rules and preferences.** A `rule` or `preference` loses visibility (supersede, archive, exclude) only through user (file) ingress. Agents and automatic engine actions never retire one.
- **Change cap.** Agent and engine visibility losses are charged per entity over a true trailing 24-hour window of durably recorded loss events (manifest records and commit trailers, so the window survives index loss). An action is allowed if `losses_in_window + 1 ≤ max(1, floor((active_now + losses_in_window) / 3))`. There is no vault-wide cap: only the per-entity cap applies.
  - Counted: every MCP or automatic action that removes a current claim from current reads (supersede, archive including `wi_move` into `archive/`, exclude, conflict). A note with several entities counts against each, using the target's entities before the change. File-ingress (user) actions are not capped.
  - The budget is reserved under the [publisher lock](OPERATIONS.md#publisher-and-manifest), so concurrent processes cannot overspend it. Flagging a conflict between two active claims is one loss charged once to each entity of either note.
  - `retry_at` is the earliest event expiry at which the action fits; it is absent if the action never fits automatically.
  - Overflow goes to a durable FIFO deferred queue, each entry with a work item; replacements backed by newer source evidence go first. The response is `budget-exhausted` with the spent operations and the earliest retry time. The queue resumes automatically, with no agent or human involved, after every lifecycle write, on `wi adopt` and during maintenance.
  - A deferred entry runs only when its holder is the engine-created deferred work item for it. At execution the change is re-authorized: entities come from the live target, a target whose bytes changed since queueing is refused, the rule and preference retirement ban applies, and the cap is checked again under the publisher lock. A refused entry closes its work note as `not_supported`.
  - **Limit.** An actor can still use up the allowance first. The deferral is visible and resumes on its own.

## Freshness

Freshness is computed when a note is read. It is never stored as status.

- `last_verified = max(effective_observed_at, latest observation of the pinned source)`. For non-user notes `effective_observed_at = min(claimed observed_at, engine-recorded evidence time of each pinned source)`, where the evidence time is the manifest or ingest record time of the pinned blob (the author time of the commit holding it once committed), or the snapshot capture time for a URL. A user-origin note keeps its claimed `observed_at`, which must not be in the future. For a vault-internal source, a recheck that re-reads the live file unchanged is an observation (`check.checked_at` with `result: unchanged`). For a URL snapshot, only a new host fetch is an observation. Re-hashing stored bytes sets only `check.integrity_at` and never renews freshness. With several pins, `last_verified` uses the latest observation that every pin shares. A `current-state` claim is citable while `now < last_verified + stale_after_days`.
- **Window.** `check.ttl` (ISO 8601 duration such as `P7D`) wins when present. A `volatile` or URL-pinned claim without a `ttl` uses `P7D`. Every other claim uses `stale_after_days`. Durations are `PnYnMnWnDTnHnMnS` with integer parts, each component at most once and in that order; malformed ones lint as `unsupported-markup`. A claim with a null `claim_kind` ages like `current-state`.
- `historical` and `intent` claims do not age out. Age is never falsity, and nothing is deleted for age or nonuse. Drift, missing sources and future timestamps still withhold them.
- `check` is engine-owned. A future `observed_at`, `checked_at` or observation time is refused on write and makes the note ineligible at read.
- **Drift rule:** if a pinned source's bytes changed since the pin, the claim is withheld from current reads and a recheck work item is opened. The status stays `active`. An unchanged span hash does not clear drift, because the whole source is the dependency.
- Recheck outcomes: an unchanged observation renews `last_verified`; changed bytes create a new candidate revision, which supersedes the old one after the gates; a missing source closes as `evidence_unavailable` with a reopen trigger.
- **External sources.** All evidence is local bytes. A URL is captured as a snapshot file in the vault, through the host's fetch and the capture path. Its pin points to the snapshot, and `locator` keeps the URL. A plain query never fetches. A URL snapshot gets a finite `ttl` at capture, default `P7D`. With no snapshot, or with an expired `ttl`, the claim is withheld.
- **Snapshot provenance.** Every snapshot observation records `observed_by` ([FORMAT](FORMAT.md#lifecycle-frontmatter)). It is self-reported provenance, not authentication, and no gate depends on it; delivered source metadata and lint and gate guidance show it ([AUTHORITY](AUTHORITY.md#limits)).

## Reads and delivery

Two read modes:

- `current` (default): active, citable, conditions met, not excluded, no conflict between active notes, not named in another active note's `supersedes`, and not under the configured archive directory, the default `archive/` folder or any previously configured archive directory. An unknown query condition fails eligibility.
- `all`: any status, each hit labelled with its eligibility. `historic` and `explore` are accepted as aliases.

Every hit carries `status`, `claim_kind`, freshness and source refs. Withheld claims are reported in a separate bounded list of counts and reasons, with no content. A non-current read never fills a current-state answer silently.

An active `rule` or `preference` whose `origin` is not `user` is withheld (`origin-rule`) as defense in depth. A conflict is withheld when `conflict_with` names an active note or when slot detection finds one, even before the flags are written.

**Plain notes.** A note without a lifecycle `type` is a plain note. Search, read and context treat it as an ordinary note, and results label it `lifecycle: none`. Lifecycle is opt-in per note through `type`.

Caps: at most 16384 serialized bytes of structured result and 8 hits per result. Budgets are in bytes only; byte and token units are never mixed, because Korean text inflates byte-to-token estimates. A claim whose qualifiers do not fit is withheld, never truncated. Wire shape: [PROTOCOL](PROTOCOL.md#results).

## Lint and the off-switch

Lint is model-free. It reports unsupported claims (no resolving source), current-state claims without `observed_at`, expired applicability, stale sources, competing active claims, broken required sources, unsupported markup, and the procedure health problems under [Self-maintenance](#self-maintenance). Lint reports; it never rewrites a note. `wi_lint` runs it on demand.

`lifecycle: off` in the vault config turns off every gate, eligibility filter and lint, and reproduces plain-note behavior. It is the validation baseline. Lifecycle keys stay in the files untouched.

## Evidence, not instruction

Memory text is a declarative statement, not a command. Delivered results are labelled as evidence, not instructions. A rule or preference read back next session informs the current request; it does not override it. Usage count never raises truth.

## Procedures

A procedure is a `kind: procedure` note with a stable `skill_id`. `excluded: true` with an `exclusion_reason` means "do not execute", not falsity. A replacement for an excluded version needs new evidence that addresses the recorded fault. Details: [SKILLS](SKILLS.md).

## Work items

Work items are `type: work` notes under `.wiki-island/work/`, named `<kind>-<id>.md`. The id is the first 16 hex digits of the SHA-256 of kind, target and sorted related paths. Each item records the observed state of its trigger condition (freshness state, current hashes of pinned sources and `last_verified` for a recheck; slot values for a conflict; body hash for a pending capture); closure stores it as `reopen.observed`. A refresh updates an open item with the same id; a closed item reopens only when its `reopen.trigger` matches and the condition is now observed in a different state. A closure without a recorded state is never reopened. Lifecycle and states: [OPERATIONS](OPERATIONS.md#work-items).

## Self-maintenance

`Vault::maintain` is one idempotent, deterministic, model-free pass, run by `wi maintain` and by the watcher ([OPERATIONS](OPERATIONS.md#maintenance)). It:

- retries held recovery intents and repairs a pending index;
- checks index integrity cheaply and rebuilds only on detected drift;
- sweeps the lifecycle: creates or refreshes work items, auto-closes items whose cause is gone, turns per-device work requests (conflict copies, recovery problems, merge conflicts, missing files, ingest failures, quarantine) into synced work notes, completes interrupted supersedes and resumes deferred changes;
- opens procedure health items with stable keys: lint failures, stale sources, duplicate `skill_id`, too large for the MCP cap, excluded without a replacement;
- materializes or refreshes the built-in maintenance procedure;
- flushes.

A pass records the vault generation and redoes scan input that becomes stale. It publishes at most one commit for all work-item changes and the built-in procedure refresh. Notes under the default `archive/` folder or any previously configured archive directory remain archived when `archive_dir` changes. It commits nothing and writes nothing when nothing changed. It never changes a status except by completing an interrupted supersede or applying a queued change. Recheck outcomes are not automated: an unchanged recheck is recorded by closing the work item.

**Built-in maintenance procedure.** `skill_id: wiki-island.maintenance` is compiled into the binary, written by maintenance with engine origin and refreshed when the binary's text changes. It tells a host agent: when `wi_work` reports open items (or at the end of a session), take at most 3 items within the returned byte budget, fix them through MCP writes with checksums, turn a workflow repeated 3 or more times into a new procedure, never touch rules or preferences, and stop when the budget is spent. `wi_work next` returns the highest-priority open items, compact ([PROTOCOL](PROTOCOL.md#tools-and-arguments)).

**Efficiency.** Agents spend tokens only when items exist; items are compact and byte-budgeted. Maintenance is incremental and time-sliced, and an idle vault costs one generation check.

## Wiring

How `wi-store` (`src/lifecycle/`), `wi_mcp::provider::VaultLifecycle` and the CLI apply the engine.

- **Inputs.** Lifecycle notes are listed from the device index and parsed once per content hash; without an index the vault is walked. Evidence for a pin: the current bytes of `ref`, and the engine-recorded time of the pinned bytes. `now` is the system clock; `WI_NOW` (RFC 3339) or `Vault::set_lifecycle_clock` fix it for tests.
- **Automatic adoption.** After every acknowledged write of a memory note through `lifecycle_write` or `lifecycle_edit` (CLI and MCP) and after every file ingress (watcher, `wi commit`), the gates run on that note. Outcomes are engine writes (actor `engine`, origin unchanged): the candidate first, then each superseded target. The write result carries `lifecycle.adoption` (`active`, `candidate`, `deferred`, `rejected` or `unchanged`, gate results, work notes, retry time).
- **Change cap.** Loss events are charged under the publisher lock: the write that causes the loss is checked against the durable events (unretired manifest intents and `Wi-Loss` commit trailers inside the window) and records its events in the intent's `losses`; the flush writes one `Wi-Loss: <json>` trailer per event. A spent budget refuses the write with `budget-exhausted`.
- **Reads.** `Vault::search_current` and the MCP provider judge each hit from its re-read bytes against a snapshot of the other lifecycle notes, cached per (generation, index watermark). Plain notes are delivered as `lifecycle: none`; withheld hits become `{reason, count}` only. If a pinned source or superseder changes during the read, the read retries once; a second change adds `generation-changed`.
- **Revert.** A revert of a lifecycle note writes `<stem>-restore-<8 hex>.md` with `restores: {ref, commit}`, `status: candidate` and the reverting ingress as origin, then runs the gates. Plain notes are restored in place.
- **Archive and move.** An agent `wi_archive`, or a `wi_move` into the archive directory, of a current memory claim goes through the loss check: rules and preferences are refused (`rule-retirement`), an exhausted budget is deferred, and an allowed loss is charged with the move. The moved note gets `status: archived`. User actions are not capped.
- **Import.** `wi import` publishes sidecars and run notes with ingress `import`. They are `extracted_text` notes, not `type: memory`, so no gate runs.
- **Capture.** `wi capture` and `wi_capture` refuse text the filter drops (`capture-dropped`, reason codes only) and an empty capture without `no_memory_reason`. A snapshot argument stores host-fetched bytes under the captures directory and records one observation event; the same bytes add only the event.
- **Exports.** `Vault::current_rules()` returns current `rule`/`preference` notes (user origin) with content, and the omitted ones with a reason code and the earliest `applicability.until`. `Vault::procedures(Discovery)` returns current procedures; `Direct` returns every procedure with its eligibility label.
- **Limits.** Evidence time for a committed pin is the commit author time, not the original record time. The read snapshot of other notes follows the index, so an external edit is seen after the watcher or a catch-up indexes it. `exclude` has no tool yet, so only archive, move, supersede and conflict losses are charged.
