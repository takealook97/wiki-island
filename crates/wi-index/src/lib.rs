#![forbid(unsafe_code)]
//! Disposable, per-device lexical index. Vault files remain canonical.
mod filters;
mod operations;
mod query;
pub mod script;
mod sync;

use chrono::{SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, params_from_iter, types::Value as SqlValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use thiserror::Error;

pub use operations::OperationRow;
pub use sync::{
    IndexDiagnostic, SyncReport, ignored, ignored_dir, ignored_with_errors, literal_search,
};
pub type RelPath = PathBuf;
pub type Result<T> = std::result::Result<T, Error>;
const SCHEMA_VERSION: i64 = 7;

#[derive(Debug, Error)]
pub enum Error {
    #[error("filesystem: {0}")]
    Io(#[from] std::io::Error),
    #[error("SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("note: {0}")]
    Note(#[from] wi_note::ParseError),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("walk: {0}")]
    Walk(#[from] ignore::Error),
    #[error("config: {0}")]
    Config(#[from] wi_note::ConfigError),
    #[error("invalid query: {0}")]
    InvalidQuery(String),
    #[error("unsafe or unsupported path: {0}")]
    InvalidPath(PathBuf),
    #[error("note exceeds configured size bound: {0}")]
    Oversize(PathBuf),
    #[error("no platform cache directory")]
    NoCacheDir,
    #[error("invalid vault id")]
    InvalidVaultId,
    #[error("index belongs to another vault")]
    VaultMismatch,
    #[error("changed-during-sync: file changed while it was being indexed: {0}")]
    ChangedDuringRead(PathBuf),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    #[default]
    Text,
    Title,
    Permalink,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Query {
    pub text: String,
    pub mode: SearchMode,
    pub metadata_filters: Map<String, Value>,
    /// Authored frontmatter `type`, not lifecycle eligibility or an FTS row kind.
    pub type_filter: Option<String>,
    pub limit: usize,
    /// Count matching notes up to `count_limit + 1` into
    /// [`SearchResult::matches`]; 0 skips counting.
    pub count_limit: usize,
}
impl Default for Query {
    fn default() -> Self {
        Self {
            text: String::new(),
            mode: SearchMode::Text,
            metadata_filters: Map::new(),
            type_filter: None,
            limit: 8,
            count_limit: 0,
        }
    }
}
impl Query {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }
    pub(crate) fn validate(&self) -> Result<Vec<filters::Predicate>> {
        if self.limit > 10_000 || self.text.len() > 16_384 {
            return Err(Error::InvalidQuery("limit or text exceeds bounds".into()));
        }
        filters::parse(&self.metadata_filters)
    }
}

/// A lifecycle note as indexed (see `Index::lifecycle_rows`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleRow {
    pub path: String,
    pub content_hash: String,
    pub mtime_ns: String,
    pub size: u64,
}

/// Original frontmatter names and the file stamp they describe. This is a
/// partial projection: unsupported or ambiguous rows need a filesystem read.
#[derive(Debug, Clone)]
pub struct NameRow {
    pub path: String,
    pub mtime_ns: String,
    pub size: u64,
    pub title: Option<String>,
    pub permalink: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub rel_path: RelPath,
    pub permalink: String,
    pub title: String,
    pub snippet: String,
    pub content_hash: String,
    pub r#type: String,
    pub status: Option<String>,
    pub kind: Option<String>,
    pub claim_kind: Option<String>,
    pub scope: Option<String>,
    pub entities: Value,
    pub slot: Option<String>,
    /// Complete parsed frontmatter for downstream eligibility checks.
    pub metadata: Value,
    /// SQLite bm25: smaller (more negative) ranks first. Fallback uses 0.
    pub score: f64,
    pub fallback: bool,
    /// The indexed text has git merge conflict markers (`<<<<<<<`,
    /// `=======` and `>>>>>>>` at line starts). The engine refuses to write
    /// such a file; the owner resolves it in an editor.
    #[serde(default)]
    pub conflict_markers: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    /// RFC3339 time captured before the last completed stat-walk; consult `skipped`.
    /// Not a snapshot guarantee.
    /// None means no completed index sync (including literal fallback).
    pub index_as_of: Option<String>,
    pub fallback: bool,
    #[serde(default)]
    pub skipped: Vec<PathBuf>,
    #[serde(default)]
    pub diagnostics: Vec<IndexDiagnostic>,
    /// Matching notes counted up to `Query::count_limit + 1`, so a value
    /// above the limit means "more than the limit". None when not counted.
    #[serde(default)]
    pub matches: Option<usize>,
}

impl SearchResult {
    /// Whether `hit`'s file was skipped (unreadable, invalid UTF-8,
    /// oversized and so on) by the last full walk or a later targeted update
    /// of that path. Such a hit is the last successfully indexed version and
    /// carries no freshness claim: re-read it or call `verify_hit` first.
    pub fn is_skipped(&self, hit: &SearchHit) -> bool {
        self.skipped
            .iter()
            .any(|p| p.to_string_lossy() == hit.rel_path.to_string_lossy())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextEdge {
    pub from_path: RelPath,
    pub to_name: String,
    pub to_path: Option<RelPath>,
    pub relation_type: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextResult {
    pub hits: Vec<SearchHit>,
    pub edges: Vec<ContextEdge>,
    pub index_as_of: Option<String>,
    pub truncated: bool,
    #[serde(default)]
    pub diagnostics: Vec<IndexDiagnostic>,
}

pub struct Index {
    pub(crate) connection: Connection,
    pub(crate) vault_root: PathBuf,
    path: Option<PathBuf>,
}

/// Retry only lock contention; transaction drops roll back each failed attempt.
fn retry_initialization(mut operation: impl FnMut() -> Result<()>) -> Result<()> {
    for attempt in 0..50 {
        match operation() {
            Err(Error::Sqlite(rusqlite::Error::SqliteFailure(error, _)))
                if error.code == rusqlite::ErrorCode::DatabaseBusy && attempt < 49 =>
            {
                std::thread::sleep(Duration::from_millis(10 * (attempt + 1).min(10)));
            }
            result => return result,
        }
    }
    unreachable!("the final attempt returns its result")
}

impl Index {
    pub fn cache_path(vault_id: &str) -> Result<PathBuf> {
        validate_id(vault_id)?;
        Ok(dirs::cache_dir()
            .ok_or(Error::NoCacheDir)?
            .join("wiki-island")
            .join(vault_id)
            .join("index.sqlite3"))
    }
    pub fn open(vault_root: impl AsRef<Path>, vault_id: &str) -> Result<Self> {
        Self::open_at(vault_root, vault_id, Self::cache_path(vault_id)?)
    }
    /// Override the database path, still outside the vault. Useful for tests and tools.
    pub fn open_at(
        vault_root: impl AsRef<Path>,
        vault_id: &str,
        database: impl AsRef<Path>,
    ) -> Result<Self> {
        validate_id(vault_id)?;
        let root = fs::canonicalize(vault_root)?;
        if !root.is_dir() {
            return Err(Error::InvalidPath(root));
        }
        let database = database.as_ref();
        if database.file_name().is_none() {
            return Err(Error::InvalidPath(database.into()));
        }
        let parent = database
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut ancestor = parent;
        while !ancestor.exists() {
            ancestor = ancestor
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
        }
        if fs::canonicalize(ancestor)?.starts_with(&root) {
            return Err(Error::InvalidPath(database.into()));
        }
        fs::create_dir_all(parent)?;
        let parent = fs::canonicalize(parent)?;
        let path = parent.join(
            database
                .file_name()
                .ok_or_else(|| Error::InvalidPath(database.into()))?,
        );
        if path.starts_with(&root)
            || fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink())
        {
            return Err(Error::InvalidPath(path));
        }
        let connection = Connection::open(&path)?;
        Self::initialize(connection, root, Some(path), vault_id)
    }
    pub fn open_in_memory(vault_root: impl AsRef<Path>, vault_id: &str) -> Result<Self> {
        validate_id(vault_id)?;
        Self::initialize(
            Connection::open_in_memory()?,
            fs::canonicalize(vault_root)?,
            None,
            vault_id,
        )
    }
    fn initialize(
        mut connection: Connection,
        vault_root: PathBuf,
        path: Option<PathBuf>,
        vault_id: &str,
    ) -> Result<Self> {
        // WAL conversion can return BUSY without invoking SQLite's busy handler.
        // Keep initialization waits short and retry the entire schema transaction,
        // including its checks, so another opener cannot rebuild a fresh schema.
        connection.busy_timeout(Duration::from_millis(100))?;
        retry_initialization(|| {
            connection.execute_batch("PRAGMA journal_mode=WAL;")?;
            Ok(())
        })?;
        connection.execute_batch("PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;")?;
        retry_initialization(|| {
            let tx =
                connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let has_identity: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='index_meta')",
                [],
                |r| r.get(0),
            )?;
            if has_identity {
                let identity: Option<String> = tx
                    .query_row(
                        "SELECT value FROM index_meta WHERE key='vault_id'",
                        [],
                        |r| r.get(0),
                    )
                    .optional()?;
                if identity.as_deref().is_some_and(|id| id != vault_id) {
                    return Err(Error::VaultMismatch);
                }
            }
            let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            let mut schema_complete = true;
            for name in [
                "index_meta",
                "files",
                "notes",
                "observations",
                "relations",
                "search_fts",
                "operations",
                "lifecycle_snapshots",
            ] {
                schema_complete &= tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                    [name],
                    |r| r.get::<_, bool>(0),
                )?;
            }
            if version != SCHEMA_VERSION || !schema_complete {
                tx.execute_batch("DROP TABLE IF EXISTS lifecycle_snapshots; DROP TABLE IF EXISTS operations; DROP TABLE IF EXISTS search_fts; DROP TABLE IF EXISTS relations; DROP TABLE IF EXISTS observations; DROP TABLE IF EXISTS notes; DROP TABLE IF EXISTS files; DROP TABLE IF EXISTS index_meta;")?;
                tx.execute_batch(include_str!("schema.sql"))?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            let identity: Option<String> = tx
                .query_row(
                    "SELECT value FROM index_meta WHERE key='vault_id'",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            if identity.as_deref().is_some_and(|id| id != vault_id) {
                return Err(Error::VaultMismatch);
            }
            tx.execute(
                "INSERT OR IGNORE INTO index_meta(key,value) VALUES('vault_id',?1)",
                [vault_id],
            )?;
            tx.commit()?;
            Ok(())
        })?;
        connection.busy_timeout(Duration::from_secs(5))?;
        Ok(Self {
            connection,
            vault_root,
            path,
        })
    }
    pub fn database_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
    pub fn index_as_of(&self) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row(
                "SELECT value FROM index_meta WHERE key='index_as_of'",
                [],
                |r| r.get(0),
            )
            .optional()?)
    }
    /// Name fields after a completed walk, using the delivered title rules.
    /// Unindexed files and ambiguous malformed metadata are absent; callers
    /// merge these rows with filesystem discovery and compare stamps.
    pub fn name_rows(&self) -> Result<Option<Vec<NameRow>>> {
        if self.path.as_ref().is_some_and(|p| !p.exists()) {
            return Ok(None);
        }
        let snapshot = self.connection.unchecked_transaction()?;
        if self.index_as_of()?.is_none() {
            return Ok(None);
        }
        let skipped: Option<String> = snapshot
            .query_row(
                "SELECT value FROM index_meta WHERE key='skipped'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let skipped: BTreeSet<PathBuf> = skipped
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or_default();
        let mut statement = snapshot.prepare_cached(
            "SELECT f.path,f.mtime_ns,f.size,n.metadata, \
             CASE WHEN json_type(n.metadata,'$.title') IS NULL \
             OR json_type(n.metadata,'$.summary') IS NOT NULL \
             THEN substr(n.content,1,?1) ELSE '' END \
             FROM files f JOIN notes n ON n.path=f.path ORDER BY f.path",
        )?;
        let scan = i64::try_from(wi_note::TITLE_SCAN_BYTES).unwrap_or(i64::MAX);
        let rows = statement.query_map([scan], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, u64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (path, mtime_ns, size, metadata, mut first) = row?;
            if Path::new(&path).ancestors().any(|p| skipped.contains(p)) {
                continue;
            }
            let metadata: Value = serde_json::from_str(&metadata)?;
            if first.is_empty()
                && metadata
                    .get("title")
                    .is_some_and(|title| title.as_str().is_none_or(|title| title.trim().is_empty()))
            {
                // A present but unusable title still needs the heading fallback.
                first = snapshot.query_row(
                    "SELECT substr(content,1,?1) FROM notes WHERE path=?2",
                    rusqlite::params![scan, path],
                    |r| r.get(0),
                )?;
            }
            // Malformed frontmatter is indexed as empty metadata with its
            // fence retained in content. It can still expose fields through
            // Note::frontmatter(), so leave ambiguous rows to the old reader.
            if metadata.as_object().is_none_or(|m| m.is_empty())
                && first
                    .chars()
                    .next()
                    .is_some_and(|c| c == '-' || c == '\u{feff}' || c.is_whitespace())
            {
                continue;
            }
            let field = |key: &str| metadata.get(key).and_then(Value::as_str).map(str::to_owned);
            let title = Some(wi_note::hit_title(&path, &metadata, &first));
            out.push(NameRow {
                path,
                mtime_ns,
                size,
                title,
                permalink: field("permalink"),
            });
        }
        Ok(Some(out))
    }

    /// Every lifecycle note (`wi_lifecycle::LIFECYCLE_TYPES`) in path
    /// order, with its indexed stamp and hash. `store` writes one
    /// `lifecycle_snapshots` row per indexed lifecycle note in the same
    /// transaction as its `notes` row, so the listing needs no scan of
    /// `notes`. Snapshots are fetched separately, for cache misses only.
    pub fn lifecycle_rows(&self) -> Result<Vec<LifecycleRow>> {
        let mut statement = self.connection.prepare_cached(
            "SELECT s.path, f.content_hash, f.mtime_ns, f.size \
             FROM lifecycle_snapshots s JOIN files f ON f.path = s.path ORDER BY s.path",
        )?;
        let rows = statement.query_map([], |r| {
            Ok(LifecycleRow {
                path: r.get(0)?,
                content_hash: r.get(1)?,
                mtime_ns: r.get(2)?,
                size: r.get::<_, i64>(3)?.try_into().unwrap_or(u64::MAX),
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
    /// Materialized snapshots in `format` of `paths`: path → (content hash
    /// the snapshot was parsed from, snapshot text). Paths without one are
    /// absent. The caller checks the hash against the one it listed.
    pub fn lifecycle_snapshots(
        &self,
        format: &str,
        paths: &[&str],
    ) -> Result<std::collections::HashMap<String, (String, String)>> {
        let mut statement = self.connection.prepare_cached(
            "SELECT path, content_hash, snapshot FROM lifecycle_snapshots \
             WHERE path IN (SELECT value FROM json_each(?1)) AND format = ?2 \
               AND snapshot IS NOT NULL",
        )?;
        let paths = serde_json::to_string(paths)?;
        let rows = statement.query_map([paths.as_str(), format], |r| {
            Ok((r.get(0)?, (r.get(1)?, r.get(2)?)))
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
    pub fn search(&self, query: &Query) -> Result<SearchResult> {
        query.validate()?;
        // A newly created or deleted database has no completeness claim.
        if self.path.as_ref().is_some_and(|p| !p.exists()) {
            return literal_search(&self.vault_root, query);
        }
        let indexed = self.index_as_of().and_then(|as_of| {
            if as_of.is_none() {
                literal_search(&self.vault_root, query)
            } else {
                self.search_index(query)
            }
        });
        match indexed {
            Err(Error::Sqlite(ref e)) if corrupt(e) => literal_search(&self.vault_root, query),
            Err(Error::InvalidQuery(_) | Error::Json(_)) => literal_search(&self.vault_root, query),
            result => result,
        }
    }
    fn search_index(&self, query: &Query) -> Result<SearchResult> {
        // SQLite ends FTS5 strings and GLOB patterns at NUL, which is a hard
        // "unterminated string" error; route such text to literal fallback.
        if query.text.contains('\0') {
            return Err(Error::InvalidQuery("query text contains NUL".into()));
        }
        let snapshot = self.connection.unchecked_transaction()?;
        let index_as_of = self.index_as_of()?;
        if index_as_of.is_none() {
            drop(snapshot);
            return literal_search(&self.vault_root, query);
        }
        let predicates = query.validate()?;
        let expression = query::expression(query)?;
        let mut params: Vec<SqlValue> = Vec::new();
        let mut conditions = Vec::new();
        if let Some(expr) = &expression {
            params.push(SqlValue::Text(expr.clone()));
            conditions.push("search_fts MATCH ?1".into());
        }
        if query.mode == SearchMode::Permalink
            && !query.text.trim().is_empty()
            && query.text.trim() != "*"
        {
            let text = query.text.trim().to_lowercase();
            if text.contains('*') {
                params.push(SqlValue::Text(query::sqlite_glob(&text)));
                conditions.push(format!("lower(n.permalink) GLOB ?{}", params.len()));
            } else if text.contains('/') {
                params.push(SqlValue::Text(text));
                conditions.push(format!("lower(n.permalink) = ?{}", params.len()));
            } else {
                let mut mode_query = query.clone();
                mode_query.mode = SearchMode::Title;
                let expr = query::expression(&mode_query)?
                    .unwrap_or_default()
                    .replacen("title:", "permalink:", 1);
                params.push(SqlValue::Text(expr));
                conditions.push(format!("search_fts MATCH ?{}", params.len()));
            }
        }
        // Engine input (work items, built-in procedures) is indexed for
        // the engine, never delivered as a note; capture notes are.
        params.push(SqlValue::Text(format!("{}/*", wi_note::ENGINE_DIR)));
        params.push(SqlValue::Text(format!("{}/*", wi_note::CAPTURES_DIR)));
        conditions.push(format!(
            "(lower(n.path) NOT GLOB ?{} OR lower(n.path) GLOB ?{})",
            params.len() - 1,
            params.len()
        ));
        if let Some(t) = &query.type_filter {
            params.push(SqlValue::Text(t.clone()));
            conditions.push(format!("n.note_type = ?{}", params.len()));
        }
        for predicate in predicates {
            conditions.push(predicate.sql(&mut params));
        }
        let matching =
            expression.is_some() || conditions.iter().any(|c| c.starts_with("search_fts MATCH"));
        let score = if matching {
            "bm25(search_fts, 5.0, 1.0, 1.0, 1.0)"
        } else {
            "0.0"
        };
        let where_sql = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };
        let count_params = params.clone();
        let exact_title = if query.mode == SearchMode::Title {
            params.push(SqlValue::Text(query.text.trim().into()));
            format!("(lower(n.title)=lower(?{}))", params.len())
        } else {
            "0".into()
        };
        params.push(SqlValue::Integer(query.limit as i64));
        // Rank and limit on row ids first, then load content for the kept
        // rows only: a broad match must not materialize every note body.
        let sql = format!(
            "SELECT n.path,n.permalink,n.title,n.content,n.metadata,f.content_hash,s.score FROM \
             (SELECT n.rowid AS id,{score} AS score,{exact_title} AS exact,n.path AS p FROM notes n \
             JOIN files f ON f.path=n.path JOIN search_fts ON search_fts.rowid=n.rowid {where_sql} \
             ORDER BY exact DESC,score,n.path LIMIT ?{}) s \
             JOIN notes n ON n.rowid=s.id JOIN files f ON f.path=n.path ORDER BY s.exact DESC,s.score,s.p",
            params.len()
        );
        let mut hits = self.fetch_hits(&sql, params.clone(), &query.text)?;
        // Ranked recall: when all-terms matching leaves room, fill the
        // remaining slots from an OR over terms and n-grams (bm25 order).
        // Strict hits usually match the OR too, so fetch enough extra rows
        // that skipping them still fills the limit.
        let ranked = (expression.is_some())
            .then(|| query::ranked_expression(query))
            .flatten();
        if hits.len() < query.limit
            && let Some(ranked) = &ranked
        {
            let mut ranked_params = params;
            ranked_params[0] = SqlValue::Text(ranked.clone());
            if let Some(last) = ranked_params.last_mut() {
                *last = SqlValue::Integer(query.limit.saturating_add(hits.len()) as i64);
            }
            let extra = self.fetch_hits(&sql, ranked_params, &query.text)?;
            let mut seen: HashSet<RelPath> = hits.iter().map(|h| h.rel_path.clone()).collect();
            for hit in extra {
                if hits.len() >= query.limit {
                    break;
                }
                if seen.insert(hit.rel_path.clone()) {
                    hits.push(hit);
                }
            }
        }
        let matches = if query.count_limit == 0 {
            None
        } else {
            Some(self.count_matches(
                &where_sql,
                &conditions,
                count_params,
                ranked,
                query.count_limit,
            )?)
        };
        let skipped = self
            .connection
            .query_row(
                "SELECT value FROM index_meta WHERE key='skipped'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .map(|s| serde_json::from_str(&s))
            .transpose()?
            .unwrap_or_default();
        let diagnostics = self
            .connection
            .query_row(
                "SELECT value FROM index_meta WHERE key='diagnostics'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .map(|s| serde_json::from_str(&s))
            .transpose()?
            .unwrap_or_default();
        snapshot.commit()?;
        Ok(SearchResult {
            hits,
            index_as_of,
            fallback: false,
            skipped,
            diagnostics,
            matches,
        })
    }
    /// Distinct notes matching the strict expression or its ranked OR
    /// expansion, counted up to `limit + 1` without loading content.
    fn count_matches(
        &self,
        where_sql: &str,
        conditions: &[String],
        mut params: Vec<SqlValue>,
        ranked: Option<String>,
        limit: usize,
    ) -> Result<usize> {
        let from = "FROM notes n JOIN files f ON f.path=n.path JOIN search_fts ON search_fts.rowid=n.rowid";
        let mut select = format!("SELECT n.rowid {from} {where_sql}");
        if let Some(ranked) = ranked {
            params.push(SqlValue::Text(ranked));
            let mut ranked_conditions = conditions.to_vec();
            ranked_conditions[0] = format!("search_fts MATCH ?{}", params.len());
            select.push_str(&format!(
                " UNION SELECT n.rowid {from} WHERE {}",
                ranked_conditions.join(" AND ")
            ));
        }
        params.push(SqlValue::Integer(limit.saturating_add(1) as i64));
        let sql = format!("SELECT count(*) FROM ({select} LIMIT ?{})", params.len());
        let count: i64 = self
            .connection
            .prepare_cached(&sql)?
            .query_row(params_from_iter(params), |r| r.get(0))?;
        Ok(usize::try_from(count).unwrap_or(usize::MAX))
    }
    fn fetch_hits(&self, sql: &str, params: Vec<SqlValue>, text: &str) -> Result<Vec<SearchHit>> {
        let mut statement = self.connection.prepare_cached(sql)?;
        let rows = statement.query_map(params_from_iter(params), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, f64>(6)?,
            ))
        })?;
        let mut hits = Vec::new();
        for row in rows {
            let (path, permalink, _identity_title, content, metadata, hash, score) = row?;
            hits.push(make_hit(
                path,
                permalink,
                &content,
                serde_json::from_str(&metadata)?,
                hash,
                score,
                false,
                text,
            ));
        }
        Ok(hits)
    }
    /// Re-hash a hit immediately before using it as current evidence. Missing files return false.
    pub fn verify_hit(&self, hit: &SearchHit) -> Result<bool> {
        verify_hit(&self.vault_root, hit)
    }

    /// Bidirectional breadth-first traversal. Unresolved/ambiguous edges stay visible.
    /// Depth is capped at 16 and output at 256 notes / 2048 edges.
    pub fn context(&self, permalink: &str, depth: usize) -> Result<ContextResult> {
        if depth > 16 {
            return Err(Error::InvalidQuery("context depth exceeds 16".into()));
        }
        let _snapshot = self.connection.unchecked_transaction()?;
        let mut result = ContextResult {
            hits: Vec::new(),
            edges: Vec::new(),
            index_as_of: self.index_as_of()?,
            truncated: false,
            diagnostics: Vec::new(),
        };
        let mut roots = self
            .connection
            .prepare_cached("SELECT path FROM notes WHERE permalink=?1 ORDER BY path")?;
        let paths = roots
            .query_map([permalink], |r| r.get::<_, String>(0))?
            .filter(|p| !p.as_ref().is_ok_and(|p| wi_note::is_hidden_engine_path(p)))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        // Never choose an arbitrary owner of a duplicated explicit identity.
        if paths.len() != 1 {
            result.diagnostics.push(IndexDiagnostic {
                rel_path: PathBuf::new(),
                message: if paths.is_empty() {
                    "context-root-missing"
                } else {
                    "context-root-ambiguous"
                }
                .into(),
            });
            return Ok(result);
        }
        let mut queue: VecDeque<_> = paths.into_iter().map(|p| (p, 0)).collect();
        let mut visited = BTreeSet::new();
        let mut edges = BTreeSet::new();
        let mut file_targets = BTreeSet::new();
        while let Some((path, level)) = queue.pop_front() {
            if visited.contains(&path) {
                continue;
            }
            if visited.len() == 256 {
                result.truncated = true;
                break;
            }
            if wi_note::is_hidden_engine_path(&path) {
                continue;
            }
            visited.insert(path.clone());
            let row = self.connection.query_row("SELECT n.permalink,n.title,n.content,n.metadata,f.content_hash FROM notes n JOIN files f ON f.path=n.path WHERE n.path=?1", [&path], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?)))?;
            result.hits.push(make_hit(
                path.clone(),
                row.0,
                &row.2,
                serde_json::from_str(&row.3)?,
                row.4,
                0.0,
                false,
                "",
            ));
            if level == depth {
                continue;
            }
            let mut stmt = self.connection.prepare_cached("SELECT from_path,to_name,to_path,relation_type,target_file FROM relations WHERE from_path=?1 OR (to_path=?1 AND target_file=0) ORDER BY from_path,to_name,relation_type")?;
            let rows = stmt.query_map([&path], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, bool>(4)?,
                ))
            })?;
            for row in rows {
                let (from, name, to, kind, target_file) = row?;
                let key = (from.clone(), name.clone(), to.clone(), kind.clone());
                if edges.contains(&key) {
                    continue;
                }
                edges.insert(key);
                queue.push_back((from.clone(), level + 1));
                // A file target (a document's original binary) is not a note node.
                match &to {
                    Some(to) if target_file => {
                        file_targets.insert(to.clone());
                    }
                    Some(to) => queue.push_back((to.clone(), level + 1)),
                    None => {}
                }
                result.edges.push(ContextEdge {
                    from_path: from.into(),
                    to_name: name,
                    to_path: to.map(Into::into),
                    relation_type: kind,
                });
            }
        }
        result.edges.retain(|e| {
            visited.contains(&e.from_path.to_string_lossy().into_owned())
                && e.to_path.as_ref().is_none_or(|p| {
                    let p = p.to_string_lossy().into_owned();
                    visited.contains(&p) || file_targets.contains(&p)
                })
        });
        if result.edges.len() > 2048 {
            result.truncated = true;
            // Select in BFS discovery order before sorting the presentation, so
            // deeper queries retain the edges returned by shallower queries.
            result.edges.truncate(2048);
        }
        result.edges.sort_by(|a, b| {
            (&a.from_path, &a.to_name, &a.to_path, &a.relation_type).cmp(&(
                &b.from_path,
                &b.to_name,
                &b.to_path,
                &b.relation_type,
            ))
        });
        Ok(result)
    }
}

/// An index-open failure caused by a missing/corrupt SQLite file can still answer.
/// Invalid query bounds/filters and root/cache access errors remain errors.
/// User syntax errors select labelled literal fallback; per-file failures are diagnostics.
pub fn search_with_fallback(
    vault_root: impl AsRef<Path>,
    vault_id: &str,
    query: &Query,
) -> Result<SearchResult> {
    search_at_with_fallback(vault_root, vault_id, Index::cache_path(vault_id)?, query)
}

/// Explicit cache-path variant of `search_with_fallback`. Missing/corrupt
/// databases are not created, replaced or repaired by a literal scan.
pub fn search_at_with_fallback(
    vault_root: impl AsRef<Path>,
    vault_id: &str,
    database: impl AsRef<Path>,
    query: &Query,
) -> Result<SearchResult> {
    query.validate()?;
    validate_id(vault_id)?;
    if !database.as_ref().exists() {
        return literal_search(vault_root, query);
    }
    match Index::open_at(vault_root.as_ref(), vault_id, database) {
        Ok(index) => index.search(query),
        Err(Error::Sqlite(e)) if corrupt(&e) => literal_search(vault_root, query),
        Err(e) => Err(e),
    }
}

fn corrupt(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(e, _) if matches!(e.code, rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase))
}
fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(Error::InvalidVaultId);
    }
    Ok(())
}
pub fn content_hash(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}
fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

pub fn verify_hit(vault_root: impl AsRef<Path>, hit: &SearchHit) -> Result<bool> {
    sync::validate_relative(&hit.rel_path)?;
    let root = fs::canonicalize(vault_root)?;
    let path = root.join(&hit.rel_path);
    let real = match fs::canonicalize(&path) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if !real.starts_with(&root) {
        return Err(Error::InvalidPath(path));
    }
    Ok(content_hash(&fs::read(real)?) == hit.content_hash)
}

/// Whether `text` has all three git merge conflict markers at line starts.
pub fn has_conflict_markers(text: &str) -> bool {
    let (mut ours, mut mid, mut theirs) = (false, false, false);
    for line in text.lines() {
        if line.starts_with("<<<<<<<") {
            ours = true;
        } else if line.starts_with("=======") {
            mid |= ours;
        } else if line.starts_with(">>>>>>>") {
            theirs |= mid;
        }
    }
    theirs
}

#[allow(clippy::too_many_arguments)]
fn make_hit(
    path: String,
    permalink: String,
    content: &str,
    metadata: Value,
    hash: String,
    score: f64,
    fallback: bool,
    query: &str,
) -> SearchHit {
    let field = |key: &str| {
        metadata
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    // The stored title is the note's identity for links and title search;
    // the delivered title follows the one rule every surface shares.
    let title = wi_note::hit_title(&path, &metadata, content);
    SearchHit {
        rel_path: path.into(),
        permalink,
        title,
        snippet: snippet(content, query),
        content_hash: hash,
        r#type: field("type").unwrap_or_else(|| "note".into()),
        status: field("status"),
        kind: field("kind"),
        claim_kind: field("claim_kind"),
        scope: field("scope"),
        entities: metadata.get("entities").cloned().unwrap_or(Value::Null),
        slot: field("slot"),
        conflict_markers: has_conflict_markers(content),
        metadata,
        score,
        fallback,
    }
}
fn fallback_snippet(content: &str, query: &str) -> String {
    let needle = query
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(['"', '*']);
    let offset = if needle.is_empty() {
        0
    } else {
        content.find(needle).unwrap_or(0)
    };
    let mut start = offset.saturating_sub(80);
    while !content.is_char_boundary(start) {
        start -= 1;
    }
    content[start..].chars().take(320).collect()
}

/// Match the same normalized script bigrams as the index, alongside word tokens.
fn snippet_terms(text: &str) -> BTreeSet<String> {
    use unicode_normalization::UnicodeNormalization;
    let text: String = text.nfkc().flat_map(char::to_lowercase).collect();
    let mut terms: BTreeSet<String> = text
        .split(|c: char| !c.is_alphanumeric() || script::is_script(c))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    terms.extend(
        script::script_runs(&text)
            .iter()
            .flat_map(|r| script::script_run_grams(r)),
    );
    terms
}

fn snippet(content: &str, query: &str) -> String {
    const MAX_BYTES: usize = 320;
    let text = query
        .split_whitespace()
        .filter(|t| !["AND", "OR", "NOT"].contains(t))
        .collect::<Vec<_>>()
        .join(" ");
    let terms = snippet_terms(&text);
    let lines: Vec<&str> = content.lines().collect();
    let mut best = String::new();
    let mut best_score = 0;
    for (i, line) in lines.iter().enumerate() {
        for count in 1..=2 {
            if i + count > lines.len() {
                continue;
            }
            let text = if count == 1 {
                (*line).to_owned()
            } else {
                format!("{}\n{}", line, lines[i + 1])
            };
            let score = snippet_terms(&text).intersection(&terms).count();
            if score > best_score || (score == best_score && score > 0 && text.len() < best.len()) {
                best = text;
                best_score = score;
            }
        }
    }
    if best_score == 0 {
        return fallback_snippet(content, query);
    }
    if best.len() <= MAX_BYTES {
        return best;
    }
    // Score overlapping bounded windows in linear time, including inside long lines.
    // Each window starts on a character boundary; normalization never supplies offsets.
    let mut end = MAX_BYTES;
    while !best.is_char_boundary(end) {
        end -= 1;
    }
    let mut window = &best[..end];
    let mut score = 0;
    let mut next_start = 0;
    for (start, _) in best.char_indices() {
        if start < next_start {
            continue;
        }
        next_start = start + MAX_BYTES / 2;
        let mut end = (start + MAX_BYTES).min(best.len());
        while !best.is_char_boundary(end) {
            end -= 1;
        }
        let candidate = &best[start..end];
        let matched = snippet_terms(candidate).intersection(&terms).count();
        if matched > score {
            window = candidate;
            score = matched;
            if score == best_score {
                break;
            }
        }
    }
    window.to_owned()
}

#[cfg(test)]
mod snippet_tests {
    use super::snippet;

    #[test]
    fn distinct_terms_outweigh_repetition_and_fold_case() {
        assert_eq!(
            snippet(
                "alpha alpha alpha\nunrelated\nALPHA beta gamma",
                "alpha beta gamma"
            ),
            "ALPHA beta gamma"
        );
    }

    #[test]
    fn adjacent_lines_supply_complementary_terms() {
        assert_eq!(
            snippet("alpha\nbeta\nunrelated", "alpha beta"),
            "alpha\nbeta"
        );
    }

    #[test]
    fn korean_uses_the_search_script_grams() {
        assert_eq!(
            snippet("환불 정책\n무관한 줄\n결제와 환불 정책이", "결제 환불 정책"),
            "결제와 환불 정책이"
        );
    }

    #[test]
    fn a_long_multibyte_line_is_bounded_and_contains_the_match() {
        let text = format!("{}결제 환불{}", "가".repeat(400), "나".repeat(400));
        let found = snippet(&text, "결제 환불");
        assert!(found.len() <= 320);
        assert!(found.contains("결제 환불"));
    }

    #[test]
    fn unmatched_queries_keep_the_fallback() {
        let text = "plain content";
        assert_eq!(snippet(text, "missing"), text);
    }
}
