# wi-index

The disposable per-device SQLite index and search layer. `Index::open` places an FTS5 database in the platform cache directory, outside the vault; it syncs from files incrementally by stat and hash, answers text, title, permalink, metadata and bounded relation queries, and falls back to a labelled literal scan when it is missing or corrupt. The vault's ignore rules apply, `.obsidian/` and `.trash/` are never indexed, and a schema-version change rebuilds the index on open. Design and query policy: [DESIGN](../../docs/DESIGN.md#index-and-search).
