# Repository contract

Instructions for any contributor, human or automated, working in this repository. See [CONTRIBUTING.md](CONTRIBUTING.md) for process and [docs/DESIGN.md](docs/DESIGN.md) for the design.

## Mechanical gate

Run from the repository root before any commit:

```sh
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

Decisions live in [docs/DESIGN.md](docs/DESIGN.md) and the specs it links, proof in [docs/VALIDATION.md](docs/VALIDATION.md), targets in [docs/PERFORMANCE.md](docs/PERFORMANCE.md). Add crates only when there is no alternative.

## Philosophy

These constraints bind every change unless the owner revises them:

- Human-editable Markdown and attachments are canonical.
- The SQLite index is disposable and per device. Deleting it must lose nothing. Never sync a live SQLite file.
- Capture and keyword search need no daemon, network or model. The engine never calls a model. Capture, search, read and every MCP tool never use the network; only the background sync and publish threads and the explicit `wi sync` and `wi publish` commands do, never while an engine lock is held.
- Zero touch: the owner needs no approval step, confirmation prompt or manual repair. The engine maintains itself (repair, index health, work items, skills sync) and acts without being asked.
- Black box for non-developers: one command (`wi setup`) installs everything, and the owner afterwards only talks to their AI. Errors are plain sentences with the next step, never internals.
- Self-maintenance is cheap: maintenance is incremental, time-sliced and model-free; agents spend tokens only when work items exist, and items are compact and byte-budgeted.
- Status, freshness and effective time stay separate. No automatic deletion; age does not imply falsity.
- Folder names are not access control. The project is single-owner; team use is future work.
- No custom UI, dashboard, web app or mobile app. Existing agents, apps and Obsidian use the shared files and protocol.
- Adoption has no human approval step. Rules and preferences follow the origin rule in [docs/AUTHORITY.md](docs/AUTHORITY.md).
- No third-party dependency is added without a written reason ([docs/DEPENDENCIES.md](docs/DEPENDENCIES.md)).
- The project is licensed under AGPL-3.0-only (see LICENSE). New dependencies must use AGPL-3.0-compatible licenses. Do not add per-file license headers.

## Writing rules

- Docs, code, comments, tests and commits are in English. This does not restrict the language of note content.
- Describe current behavior only. Do not record history, phase or review codes, dates of past work, commit hashes or the names of other products.
- State facts with a source, inferences as inferences, unknowns as unknown. A performance number names a corpus, method and machine, or says "unmeasured".
- Do not copy private reports, internal paths, or personal or company examples.
- Prefer short documents. Link instead of repeating.

## Format contracts

- Preserve existing frontmatter, unknown keys, comments and Markdown syntax byte for byte outside what an operation edits ([FORMAT](docs/FORMAT.md#byte-preserving-frontmatter-edits)).
- Report unsupported link or syntax forms instead of rewriting them.
- Tools that edit existing notes must support an expected checksum and fail safely on a stale one ([OPERATIONS](docs/OPERATIONS.md#write-path)).
- A format, protocol or index-schema change updates the matching spec in the same change, says whether existing vaults keep working, and adds a validation entry.

## Review checklist

- Does the change claim something unmeasured as fact?
- Does it make the index the only holder of meaning?
- Does it need a daemon, network, model or external service on the capture or search path?
- Does it add a human step, prompt or manual repair?
- Does it spend tokens or time when nothing needs doing?
- Is the matching spec or validation entry updated?
- Are commits in `{category}: {description}` form, at most 72 characters?
