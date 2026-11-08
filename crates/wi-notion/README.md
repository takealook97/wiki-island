# wi-notion

A synchronous, one-way publisher for a read-only Notion view. The caller supplies
an immutable, complete snapshot of committed notes through `NoteSource::notes`.
The library has no `wi-store` dependency and does not inspect a vault or run a
background task. Capture, search, reads and MCP must not call it.

`PublishNote` carries a relative path, canonical bytes, permalink, title and
archive flag. `PublishOptions::new(parent_page_id)` defaults to the production
HTTPS API, three requests per second, a 30-second request timeout and three
retries. The API version is pinned to `2026-03-11`. Tests may override the address
with an explicit loopback HTTP endpoint. Redirects are disabled.

`publish(source, tokens, map, options)` returns `PublishReport` with created,
updated, unchanged and archived permalinks, per-note failures and mapper warnings.
Source, settings and map failures can return an error. A source must return an
error for an incomplete snapshot: mapped notes missing from a successful snapshot
are archived. Duplicate or empty permalinks reject the whole snapshot.

Open `MapStore` at a caller-chosen **device-local** path and serialize access for
the entire call. The versioned JSON map binds to one parent page and is atomically
replaced through the history durability helpers. It stores only page ids, hashes
and archive flags. It is publication state, never canonical note storage. The
caller must hold no engine lock during publication. `TokenSource` supplies a
credential without including it in errors. `KeychainTokenSource` reads the
existing `wiki-island.notion` generic password for the given account using
`/usr/bin/security` on macOS. Provisioning is outside this library.

Every page starts with “Read-only view; edit in your notes folder”. The mapper
renders frontmatter as a properties paragraph, headings (levels above three use
level three), paragraphs, styled inline text, lists, tasks, code, quotes, dividers,
tables and web links. Known wikilink targets become page mentions; unknown targets
remain text. Files are not uploaded. HTML, images and unsupported local links are
rendered as text with warnings. Unknown code languages use plain text. Canonical
note bytes are never edited. Existing vaults and indexes need no migration.

Rich-text fragments contain at most 2000 UTF-16 units, arrays at most 100 elements
and append batches at most 100 children. Nested children are appended separately;
a table is created with its first row. Large request bodies are split or rejected
before mutation. A dry run reports planned transitions without reading credentials,
saving a map or making requests; mentions for pages yet to be created remain text.

Page identities are saved before content is populated. Content hashes cover the canonical bytes,
title and rendered blocks, including resolved mentions. An unchanged hash makes
no requests and does not read a credential. Changes restore the page, replace all
existing children and append the new blocks. Notion edits are not imported and
are replaced on the next canonical change. Archived or removed notes trash their
mapped pages; a note that returns restores the same page.

Before replacing content the saved hash is cleared. After any partial failure it
stays empty, so the next call clears partial content and rebuilds the same page.
HTTP 429 respects `Retry-After` (delays over 60 seconds end the call). Repeatable
GET, DELETE and page PATCH requests retry 5xx with bounded backoff. Create and
append requests do not retry transport errors or 5xx: the service may already have
accepted them. Response bodies and transport details never enter error messages.
A successful remote page creation followed by a lost response, process crash or
failed local map save can leave an untracked empty page; exactly-once creation
across the service and a local file is not guaranteed. Failed block appends are
repaired on the next call. Publishing is not an atomic multi-page operation.

Validation uses offline tests with a local `TcpListener`: creation, updates,
unchanged skips, archive/restore, throttle and server retries, resumable partial
failure, pagination, nested appends and batch limits. Mapper tests cover block
kinds, styling, mentions, warnings and Unicode chunk boundaries. No real service
or keychain access is used by tests. Run `cargo test -p wi-notion`.
